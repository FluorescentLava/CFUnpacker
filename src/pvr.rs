use anyhow::{Result, bail, ensure};

#[derive(Clone, Debug)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub fn decode(data: &[u8], unpremultiply_alpha: bool) -> Result<RgbaImage> {
    ensure!(data.len() >= 52, "不支持或已损坏的 PVR 文件。");
    if read_u32(data, 0) == 0x03525650 {
        decode_v3(data, unpremultiply_alpha)
    } else if read_u32(data, 44) == 0x21525650 {
        decode_v2(data, unpremultiply_alpha)
    } else {
        bail!("不支持或已损坏的 PVR 文件。")
    }
}

fn decode_v2(data: &[u8], unpremultiply_alpha: bool) -> Result<RgbaImage> {
    let offset = read_u32(data, 0) as usize;
    let height = read_u32(data, 4);
    let width = read_u32(data, 8);
    let pixel_type = read_u32(data, 16) & 0xFF;
    let bits_per_pixel = read_u32(data, 24);
    validate_dimensions(width, height)?;
    ensure!(
        offset >= 52 && offset <= data.len(),
        "PVR v2 header 长度无效。"
    );
    ensure!(
        pixel_type != 0x18 && pixel_type != 0x19,
        "当前资源使用 PVRTC 压缩，尚不能直接拆分。"
    );
    ensure!(
        matches!(bits_per_pixel, 8 | 16 | 24 | 32),
        "不支持的 PVR v2 位深：{bits_per_pixel}。"
    );
    let masks = [
        read_u32(data, 28),
        read_u32(data, 32),
        read_u32(data, 36),
        read_u32(data, 40),
    ];
    decode_pixels(
        data,
        PixelFormat {
            offset,
            width,
            height,
            bytes_per_pixel: bits_per_pixel as usize / 8,
            masks,
            pixel_type,
        },
        unpremultiply_alpha,
    )
}

fn decode_v3(data: &[u8], unpremultiply_alpha: bool) -> Result<RgbaImage> {
    let height = read_u32(data, 24);
    let width = read_u32(data, 28);
    let metadata_length = read_u32(data, 48) as usize;
    let offset = 52usize
        .checked_add(metadata_length)
        .ok_or_else(|| anyhow::anyhow!("PVR v3 metadata 长度无效。"))?;
    validate_dimensions(width, height)?;
    let names: [u8; 4] = data[8..12].try_into().unwrap();
    let bits: [u8; 4] = data[12..16].try_into().unwrap();
    ensure!(
        offset <= data.len() && bits != [0; 4],
        "不支持的 PVR v3 压缩格式。"
    );
    let total_bits: usize = bits.iter().map(|&value| value as usize).sum();
    ensure!(
        matches!(total_bits, 8 | 16 | 24 | 32),
        "不支持的 PVR v3 位深：{total_bits}。"
    );
    let mut masks = [0; 4];
    if names == *b"rgb\0" && bits == [5, 6, 5, 0] {
        // Android Carrot 3 maps advertise rgb/565 but store the conventional BGR565 word.
        masks = [0xF800, 0x07E0, 0x001F, 0];
    } else {
        let mut shift = 0;
        for (&name, &width_in_bits) in names.iter().zip(&bits) {
            if width_in_bits == 0 {
                break;
            }
            let mask = (u32::MAX >> (32 - width_in_bits as u32)) << shift;
            match name {
                b'r' => masks[0] = mask,
                b'g' => masks[1] = mask,
                b'b' => masks[2] = mask,
                b'a' => masks[3] = mask,
                _ => {}
            }
            shift += width_in_bits as u32;
        }
    }
    decode_pixels(
        data,
        PixelFormat {
            offset,
            width,
            height,
            bytes_per_pixel: total_bits / 8,
            masks,
            pixel_type: u32::MAX,
        },
        unpremultiply_alpha,
    )
}

struct PixelFormat {
    offset: usize,
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
    masks: [u32; 4],
    pixel_type: u32,
}

fn decode_pixels(data: &[u8], format: PixelFormat, unpremultiply_alpha: bool) -> Result<RgbaImage> {
    let PixelFormat {
        offset,
        width,
        height,
        bytes_per_pixel,
        masks,
        pixel_type,
    } = format;
    let count = width as usize * height as usize;
    let source_bytes = count
        .checked_mul(bytes_per_pixel)
        .ok_or_else(|| anyhow::anyhow!("PVR 像素数据长度溢出。"))?;
    let end = offset
        .checked_add(source_bytes)
        .ok_or_else(|| anyhow::anyhow!("PVR 像素数据长度溢出。"))?;
    ensure!(end <= data.len(), "PVR 像素数据不完整。");
    let output_bytes = count
        .checked_mul(4)
        .filter(|&length| length <= i32::MAX as usize)
        .ok_or_else(|| anyhow::anyhow!("PVR 图像过大。"))?;
    let pixels = &data[offset..end];
    let mut output = Vec::new();
    output.try_reserve_exact(output_bytes)?;
    if bytes_per_pixel == 4 && masks == [0x000000FF, 0x0000FF00, 0x00FF0000, 0xFF000000] {
        // Already RGBA8888; keep the single owned buffer instead of a bitmap copy.
        output.extend_from_slice(pixels);
    } else {
        output.resize(output_bytes, 0);
        if bytes_per_pixel == 1 && masks[..3] == [0; 3] {
            let alpha_only = masks[3] != 0 || pixel_type == 0x1B;
            for (&value, rgba) in pixels.iter().zip(output.as_chunks_mut::<4>().0.iter_mut()) {
                let value = if alpha_only {
                    [255, 255, 255, value]
                } else {
                    [value, value, value, 255]
                };
                rgba.copy_from_slice(&value);
            }
        } else {
            // Shift counts and channel scaling tables are computed once per atlas.
            let channels = [
                Channel::new(masks[0], 0),
                Channel::new(masks[1], 0),
                Channel::new(masks[2], 0),
                Channel::new(masks[3], 255),
            ];
            match bytes_per_pixel {
                1 => expand_pixels::<1>(pixels, &mut output, &channels),
                2 => expand_pixels::<2>(pixels, &mut output, &channels),
                3 => expand_pixels::<3>(pixels, &mut output, &channels),
                4 => expand_pixels::<4>(pixels, &mut output, &channels),
                _ => unreachable!(),
            }
        }
    }
    if unpremultiply_alpha && masks[3] != 0 {
        unpremultiply(&mut output);
    }
    Ok(RgbaImage {
        width,
        height,
        pixels: output,
    })
}

struct Channel {
    mask: u32,
    shift: u32,
    maximum: u32,
    table: [u8; 256],
}

impl Channel {
    fn new(mask: u32, fallback: u8) -> Self {
        if mask == 0 {
            return Self {
                mask,
                shift: 0,
                maximum: 0,
                table: [fallback; 256],
            };
        }
        let shift = mask.trailing_zeros();
        let maximum = mask >> shift;
        let mut table = [0; 256];
        if maximum <= 255 {
            for (component, value) in table.iter_mut().enumerate().take(maximum as usize + 1) {
                *value = ((component as u32 * 255 + maximum / 2) / maximum) as u8;
            }
        }
        Self {
            mask,
            shift,
            maximum,
            table,
        }
    }

    #[inline(always)]
    fn expand(&self, value: u32) -> u8 {
        let component = (value & self.mask) >> self.shift;
        if self.maximum <= 255 {
            self.table[component as usize]
        } else {
            ((component as u64 * 255 + self.maximum as u64 / 2) / self.maximum as u64) as u8
        }
    }
}

fn expand_pixels<const N: usize>(pixels: &[u8], output: &mut [u8], channels: &[Channel; 4]) {
    for (source, target) in pixels
        .as_chunks::<N>()
        .0
        .iter()
        .zip(output.as_chunks_mut::<4>().0.iter_mut())
    {
        let value = match N {
            1 => source[0] as u32,
            2 => u16::from_le_bytes([source[0], source[1]]) as u32,
            3 => source[0] as u32 | ((source[1] as u32) << 8) | ((source[2] as u32) << 16),
            4 => u32::from_le_bytes([source[0], source[1], source[2], source[3]]),
            _ => unreachable!(),
        };
        target[0] = channels[0].expand(value);
        target[1] = channels[1].expand(value);
        target[2] = channels[2].expand(value);
        target[3] = channels[3].expand(value);
    }
}

fn unpremultiply(pixels: &mut [u8]) {
    for rgba in pixels.as_chunks_mut::<4>().0 {
        let alpha = rgba[3] as u32;
        if alpha == 0 || alpha == 255 {
            continue;
        }
        for component in &mut rgba[..3] {
            *component = ((*component as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
        }
    }
}

#[inline]
fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width <= 32768 && height <= 32768,
        "PVR 尺寸无效：{width} x {height}。"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(bits: u32, masks: [u32; 4], pixel_type: u32, pixels: &[u8]) -> Vec<u8> {
        let mut data = vec![0; 52];
        for (offset, value) in [
            (0, 52),
            (4, 1),
            (8, pixels.len() as u32 / (bits / 8)),
            (16, pixel_type),
            (24, bits),
            (44, 0x21525650),
        ] {
            data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        for (index, mask) in masks.into_iter().enumerate() {
            data[28 + index * 4..32 + index * 4].copy_from_slice(&mask.to_le_bytes());
        }
        data.extend_from_slice(pixels);
        data
    }

    #[test]
    fn rgba4444_and_unpremultiply_rounding() {
        let data = v2(
            16,
            [0xF000, 0x0F00, 0x00F0, 0x000F],
            0x10,
            &[0x48, 0x12, 0x0F, 0xF0],
        );
        assert_eq!(
            decode(&data, false).unwrap().pixels,
            [17, 34, 68, 136, 255, 0, 0, 255]
        );
        assert_eq!(
            decode(&data, true).unwrap().pixels,
            [32, 64, 128, 136, 255, 0, 0, 255]
        );
    }

    #[test]
    fn rgba8888_transparent_rgb_is_preserved() {
        let pixels = [17, 31, 94, 0, 50, 60, 70, 255];
        let data = v2(32, [0xFF, 0xFF00, 0xFF0000, 0xFF000000], 0x12, &pixels);
        assert_eq!(decode(&data, true).unwrap().pixels, pixels);
    }

    #[test]
    fn alpha_and_luminance_8bit() {
        assert_eq!(
            decode(&v2(8, [0; 4], 0x1B, &[123]), false).unwrap().pixels,
            [255, 255, 255, 123]
        );
        assert_eq!(
            decode(&v2(8, [0; 4], 0x16, &[123]), false).unwrap().pixels,
            [123, 123, 123, 255]
        );
    }

    #[test]
    fn android_rgb565_channel_order_and_metadata() {
        let mut data = vec![0; 56];
        data[..4].copy_from_slice(&0x03525650u32.to_le_bytes());
        data[8..12].copy_from_slice(b"rgb\0");
        data[12..16].copy_from_slice(&[5, 6, 5, 0]);
        data[24..28].copy_from_slice(&1u32.to_le_bytes());
        data[28..32].copy_from_slice(&1u32.to_le_bytes());
        data[48..52].copy_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(&0xF800u16.to_le_bytes());
        assert_eq!(decode(&data, false).unwrap().pixels, [255, 0, 0, 255]);
    }

    #[test]
    fn rejects_truncated_and_unsupported_formats() {
        assert!(decode(&[0; 51], false).is_err());
        let mut data = v2(16, [0xF800, 0x7E0, 0x1F, 0], 0x18, &[0, 0]);
        assert!(decode(&data, false).is_err());
        data[16] = 0;
        data.pop();
        assert!(decode(&data, false).is_err());
    }
}
