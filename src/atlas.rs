use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};
use plist::{Dictionary, Value};
use quick_xml::{Reader as XmlReader, Writer as XmlWriter, events::Event as XmlEvent};

use crate::codec;
use crate::profile::Game;
use crate::pvr::{self, RgbaImage};

const CUSTOM_HEADER: &[u8] = &[0xff, 0xdb, 0xff, 0xee, 0x66];
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
const MAX_IMAGE_BYTES: usize = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntSize {
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone)]
pub struct AtlasFrame {
    pub name: String,
    pub frame: IntRect,
    pub source_size: IntSize,
    pub source_color_rect: IntRect,
    pub rotated: bool,
}

#[derive(Debug, Clone)]
pub struct AtlasDefinition {
    pub texture_file_name: Option<String>,
    pub frames: Vec<AtlasFrame>,
}

pub fn read_definition(path: &Path, game: &Game) -> Result<Option<AtlasDefinition>> {
    let source = fs::read(path).with_context(|| format!("读取 plist：{}", path.display()))?;
    parse_definition(&source, game)
}

fn parse_definition(source: &[u8], game: &Game) -> Result<Option<AtlasDefinition>> {
    let decoded;
    let xml = if looks_like_xml(source) || source.starts_with(b"bplist00") {
        source
    } else if source.starts_with(b"czzf") || source.starts_with(CUSTOM_HEADER) {
        let key = game.plist_key().context("当前游戏没有 plist 解密 key")?;
        decoded = codec::decode_custom(source, &key, game.plist_rounds())?;
        decoded.as_slice()
    } else {
        bail!("plist 解密失败，所选游戏与 APK 可能不匹配")
    };
    let repaired = repair_merge_conflicts(xml)?;
    let value = Value::from_reader(Cursor::new(repaired.as_ref())).context("解析 plist 失败")?;
    let root = value.as_dictionary().context("plist 中没有根 dict")?;
    let Some(raw_frames) = root.get("frames").and_then(Value::as_dictionary) else {
        return Ok(None);
    };
    let mut frames = Vec::with_capacity(raw_frames.len());
    for (name, raw) in raw_frames {
        let Some(values) = raw.as_dictionary() else {
            continue;
        };
        let Some(frame) = read_rect(values, &["frame", "textureRect"]) else {
            continue;
        };
        let rotated = get_bool(values, "rotated") || get_bool(values, "textureRotated");
        let packed = read_size(values, &["spriteSize"]).unwrap_or(IntSize {
            width: frame.width,
            height: frame.height,
        });
        let source_size = read_size(values, &["sourceSize", "spriteSourceSize"]).unwrap_or(packed);
        let source_color_rect = match read_rect(values, &["sourceColorRect", "spriteSourceSize"]) {
            Some(rect) => rect,
            None => {
                let (offset_x, offset_y) = read_point(values, &["offset", "spriteOffset"]);
                let x = (i64::from(source_size.width) - i64::from(packed.width)) / 2
                    + i64::from(offset_x);
                let y = (i64::from(source_size.height) - i64::from(packed.height)) / 2
                    - i64::from(offset_y);
                IntRect {
                    x: x.try_into().context("frame 横向偏移溢出")?,
                    y: y.try_into().context("frame 纵向偏移溢出")?,
                    width: packed.width,
                    height: packed.height,
                }
            }
        };
        frames.push(AtlasFrame {
            name: name.clone(),
            frame,
            source_size,
            source_color_rect,
            rotated,
        });
    }
    if frames.is_empty() {
        return Ok(None);
    }
    let metadata = root.get("metadata").and_then(Value::as_dictionary);
    let texture_file_name = metadata
        .and_then(|v| get_str(v, "realTextureFileName").or_else(|| get_str(v, "textureFileName")))
        .or_else(|| get_str(root, "textureFileName"))
        .map(str::to_owned);
    Ok(Some(AtlasDefinition {
        texture_file_name,
        frames,
    }))
}

fn repair_merge_conflicts(xml: &[u8]) -> Result<Cow<'_, [u8]>> {
    if xml.starts_with(b"bplist00") || !xml.windows(7).any(|bytes| bytes == b"<<<<<<<") {
        return Ok(Cow::Borrowed(xml));
    }
    let mut stripped = Vec::with_capacity(xml.len());
    let mut removed = false;
    for line in xml.split_inclusive(|byte| *byte == b'\n') {
        let trimmed = line.trim_ascii();
        if trimmed.starts_with(b"<<<<<<<")
            || trimmed.starts_with(b">>>>>>>")
            || trimmed == b"======="
        {
            removed = true;
        } else {
            stripped.extend_from_slice(line);
        }
    }
    if !removed {
        return Ok(Cow::Borrowed(xml));
    }

    // The original permissive dictionary reader retains the first value after a
    // key and skips orphan values left by a conflict (e.g. two smartupdate
    // strings). Preserve both branches' complete key/value pairs, while removing
    // those orphan subtrees before the normal strict plist parser sees them.
    let mut reader = XmlReader::from_reader(stripped.as_slice());
    let mut writer = XmlWriter::new(Vec::with_capacity(stripped.len()));
    let mut dictionary_states: Vec<Option<bool>> = Vec::new();
    let mut skip_depth = 0_usize;
    loop {
        let event = reader.read_event().context("冲突标记后的 XML 无效")?;
        if skip_depth > 0 {
            match event {
                XmlEvent::Start(_) => skip_depth += 1,
                XmlEvent::End(_) => skip_depth -= 1,
                XmlEvent::Eof => bail!("冲突标记后的 XML 不完整"),
                _ => {}
            }
            continue;
        }
        match &event {
            XmlEvent::Start(start) | XmlEvent::Empty(start) => {
                let is_empty = matches!(event, XmlEvent::Empty(_));
                let is_key = start.local_name().as_ref() == "key";
                if let Some(Some(expect_key)) = dictionary_states.last_mut() {
                    if *expect_key && !is_key {
                        if !is_empty {
                            skip_depth = 1;
                        }
                        continue;
                    }
                    *expect_key = !is_key;
                }
                if !is_empty {
                    ensure!(dictionary_states.len() < 512, "plist XML 嵌套过深");
                    dictionary_states.push((start.local_name().as_ref() == "dict").then_some(true));
                }
            }
            XmlEvent::End(_) => {
                dictionary_states.pop();
            }
            XmlEvent::Eof => {
                ensure!(dictionary_states.is_empty(), "冲突标记后的 XML 不完整");
                break;
            }
            _ => {}
        }
        writer.write_event(event)?;
    }
    Ok(Cow::Owned(writer.into_inner()))
}

fn looks_like_xml(data: &[u8]) -> bool {
    let data = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(data);
    data.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'<')
}

fn get_str<'a>(values: &'a Dictionary, key: &str) -> Option<&'a str> {
    values.get(key).and_then(Value::as_string)
}

fn get_bool(values: &Dictionary, key: &str) -> bool {
    match values.get(key) {
        Some(Value::Boolean(value)) => *value,
        Some(Value::String(value)) => value.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn get_i32(values: &Dictionary, key: &str) -> Option<i32> {
    match values.get(key)? {
        Value::Integer(value) => value.as_signed()?.try_into().ok(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn parse_numbers(text: &str) -> Vec<i32> {
    // Matches the original -?\d+ grammar without building a regex per frame.
    let bytes = text.as_bytes();
    let mut result = Vec::with_capacity(4);
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        if bytes[index] == b'-' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit) {
            index += 1;
        } else if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if let Ok(number) = text[start..index].parse() {
            result.push(number);
        } else {
            return Vec::new();
        }
    }
    result
}

fn read_rect(values: &Dictionary, keys: &[&str]) -> Option<IntRect> {
    for key in keys {
        match values.get(key) {
            Some(Value::String(text)) => {
                let n = parse_numbers(text);
                if n.len() >= 4 {
                    return Some(IntRect {
                        x: n[0],
                        y: n[1],
                        width: n[2],
                        height: n[3],
                    });
                }
            }
            Some(Value::Dictionary(d)) => {
                if let (Some(x), Some(y), Some(width), Some(height)) = (
                    get_i32(d, "x"),
                    get_i32(d, "y"),
                    get_i32(d, "width"),
                    get_i32(d, "height"),
                ) {
                    return Some(IntRect {
                        x,
                        y,
                        width,
                        height,
                    });
                }
            }
            _ => {}
        }
    }
    None
}

fn read_size(values: &Dictionary, keys: &[&str]) -> Option<IntSize> {
    for key in keys {
        match values.get(key) {
            Some(Value::String(text)) => {
                let n = parse_numbers(text);
                if n.len() >= 2 {
                    let start = if n.len() >= 4 { n.len() - 2 } else { 0 };
                    return Some(IntSize {
                        width: n[start],
                        height: n[start + 1],
                    });
                }
            }
            Some(Value::Dictionary(d)) => {
                if let (Some(width), Some(height)) = (get_i32(d, "width"), get_i32(d, "height")) {
                    return Some(IntSize { width, height });
                }
            }
            _ => {}
        }
    }
    None
}

fn read_point(values: &Dictionary, keys: &[&str]) -> (i32, i32) {
    for key in keys {
        if let Some(text) = get_str(values, key) {
            let n = parse_numbers(text);
            if n.len() >= 2 {
                return (n[0], n[1]);
            }
        }
    }
    (0, 0)
}

/// Writes only the final frame PNGs. The texture and reusable frame buffer stay in memory.
pub fn split(
    plist_path: &Path,
    assets_root: &Path,
    output_root: &Path,
    game: Game,
    cancel: &AtomicBool,
) -> Result<usize> {
    ensure!(!cancel.load(Ordering::Relaxed), "操作已取消");
    let Some(definition) = read_definition(plist_path, &game)? else {
        return Ok(0);
    };
    let texture = find_texture(
        plist_path,
        definition.texture_file_name.as_deref(),
        assets_root,
    )?;
    let atlas = decode_texture(&texture, game)?;
    let relative = plist_path
        .strip_prefix(assets_root)
        .context("plist 不在 assets 根目录内")?;
    ensure!(
        relative
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "plist 路径越界"
    );
    let folder = output_root.join(relative.with_extension(""));
    fs::create_dir_all(&folder)?;
    let mut directories = HashSet::new();
    directories.insert(folder.clone());
    let mut buffer = Vec::new();
    let mut encoder = PngEncoder::new();
    let mut paths = HashSet::new();
    for frame in &definition.frames {
        ensure!(!cancel.load(Ordering::Relaxed), "操作已取消");
        let (width, height) = render_frame(&atlas, frame, &mut buffer)?;
        let path = safe_output_path(&folder, &frame.name)?;
        // Windows paths are case insensitive: avoid accidental overwrites within one atlas.
        ensure!(
            paths.insert(path.to_string_lossy().to_lowercase()),
            "frame 输出路径重复：{}",
            frame.name
        );
        let parent = path.parent().context("frame 没有父目录")?;
        if directories.insert(parent.to_path_buf()) {
            fs::create_dir_all(parent)?;
        }
        encoder.write(&path, width, height, &buffer)?;
    }
    Ok(definition.frames.len())
}

pub fn find_texture(plist_path: &Path, name: Option<&str>, assets_root: &Path) -> Result<PathBuf> {
    let directory = plist_path.parent().context("plist 没有父目录")?;
    let mut candidates = Vec::with_capacity(6);
    if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
        let name = name.replace('\\', "/");
        let mapped = crate::engine::safe_relative(&name)?;
        candidates.push(directory.join(&mapped));
        if name.to_ascii_lowercase().ends_with(".pvr") {
            candidates.push(directory.join(crate::engine::safe_relative(&format!("{name}.ccz"))?));
        }
        // Already-extracted directories on a filesystem which accepts these
        // characters may retain the original name. Containment is still checked.
        if mapped != Path::new(&name) {
            candidates.push(directory.join(&name));
        }
    }
    for extension in ["pvr.ccz", "png", "pvr", "ccz"] {
        candidates.push(plist_path.with_extension(extension));
    }
    let safe_root = assets_root.canonicalize()?;
    for candidate in candidates {
        if candidate.is_file() {
            let canonical = candidate.canonicalize()?;
            ensure!(
                canonical.starts_with(&safe_root),
                "纹理路径越界：{}",
                candidate.display()
            );
            return Ok(candidate);
        }
    }
    bail!("找不到 plist 对应的纹理文件：{}", plist_path.display())
}

pub fn decode_texture(path: &Path, game: Game) -> Result<RgbaImage> {
    let source = fs::read(path)?;
    if codec::is_ccz(&source) {
        let (decoded, key) = codec::decode_ccz_with_key(&source, game.pvr_keys())?;
        let compatibility = game == Game::Carrot1
            && key.is_some_and(|key| key.parts == Game::Carrot2.pvr_keys()[0].parts);
        return pvr::decode(&decoded, game.unpremultiply() || compatibility);
    }
    if source.starts_with(CUSTOM_HEADER) || source.starts_with(b"czzf") {
        let decoded = game
            .wrapper_keys()
            .into_iter()
            .find_map(|(key, rounds)| codec::decode_custom(&source, &key, rounds).ok())
            .context("纹理封装解密失败，未通过长度、CRC32 或格式校验")?;
        return if decoded.starts_with(PNG_SIGNATURE) {
            decode_png(&decoded)
        } else {
            pvr::decode(&decoded, game.unpremultiply())
        };
    }
    if source.starts_with(PNG_SIGNATURE) {
        return decode_png(&source);
    }
    // Plain PVR in the original program is already straight-alpha.
    pvr::decode(&source, false)
}

pub fn decode_png(data: &[u8]) -> Result<RgbaImage> {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.set_limits(png::Limits {
        bytes: MAX_IMAGE_BYTES,
    });
    let mut reader = decoder.read_info().context("PNG 无法读取")?;
    // Validate dimensions before allocating the decoder's full pixel buffer.
    image_byte_count(reader.info().width, reader.info().height)?;
    let mut raw = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut raw).context("PNG 解码失败")?;
    ensure!(info.bit_depth == png::BitDepth::Eight, "不支持的 PNG 位深");
    let count = image_byte_count(info.width, info.height)?;
    let mut pixels = Vec::with_capacity(count);
    match info.color_type {
        png::ColorType::Rgba => {
            raw.truncate(info.buffer_size());
            pixels = raw;
        }
        png::ColorType::Rgb => {
            for rgb in raw[..info.buffer_size()].as_chunks::<3>().0 {
                pixels.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for ga in raw[..info.buffer_size()].as_chunks::<2>().0 {
                pixels.extend_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for g in &raw[..info.buffer_size()] {
                pixels.extend_from_slice(&[*g, *g, *g, 255]);
            }
        }
        png::ColorType::Indexed => bail!("PNG 索引色未展开"),
    }
    ensure!(pixels.len() == count, "PNG 像素长度不一致");
    Ok(RgbaImage {
        width: info.width,
        height: info.height,
        pixels,
    })
}

fn image_byte_count(width: u32, height: u32) -> Result<usize> {
    ensure!(
        width > 0 && height > 0 && width <= 32768 && height <= 32768,
        "图像尺寸无效：{width}x{height}"
    );
    let count = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .context("图像尺寸溢出")?;
    ensure!(count <= MAX_IMAGE_BYTES, "图像内存超过 1 GiB 限制");
    Ok(count)
}

pub fn render_frame(
    atlas: &RgbaImage,
    frame: &AtlasFrame,
    output: &mut Vec<u8>,
) -> Result<(u32, u32)> {
    let mut rect = frame.frame;
    if frame.rotated {
        std::mem::swap(&mut rect.width, &mut rect.height);
    }
    ensure!(
        rect.x >= 0
            && rect.y >= 0
            && rect.width > 0
            && rect.height > 0
            && i64::from(rect.x) + i64::from(rect.width) <= i64::from(atlas.width)
            && i64::from(rect.y) + i64::from(rect.height) <= i64::from(atlas.height),
        "frame 越界：{} ({},{},{},{})",
        frame.name,
        rect.x,
        rect.y,
        rect.width,
        rect.height
    );
    let (sprite_width, sprite_height) = if frame.rotated {
        (rect.height, rect.width)
    } else {
        (rect.width, rect.height)
    };
    let width = if frame.source_size.width > 0 {
        frame.source_size.width
    } else {
        sprite_width
    };
    let height = if frame.source_size.height > 0 {
        frame.source_size.height
    } else {
        sprite_height
    };
    let byte_count = image_byte_count(width as u32, height as u32)?;
    ensure!(
        atlas.pixels.len() == image_byte_count(atlas.width, atlas.height)?,
        "图集像素长度错误"
    );
    output.resize(byte_count, 0);
    let dx = i64::from(frame.source_color_rect.x);
    let dy = i64::from(frame.source_color_rect.y);
    if !(dx == 0 && dy == 0 && sprite_width == width && sprite_height == height) {
        output.fill(0);
    }
    let sx = 0_i64.max(-dx);
    let sy = 0_i64.max(-dy);
    let tx = 0_i64.max(dx);
    let ty = 0_i64.max(dy);
    let copy_width = (i64::from(sprite_width) - sx).min(i64::from(width) - tx);
    let copy_height = (i64::from(sprite_height) - sy).min(i64::from(height) - ty);
    if copy_width <= 0 || copy_height <= 0 {
        return Ok((width as u32, height as u32));
    }
    if !frame.rotated {
        let row_bytes = copy_width as usize * 4;
        for row in 0..copy_height {
            let source =
                (((i64::from(rect.y) + sy + row) * i64::from(atlas.width) + i64::from(rect.x) + sx)
                    * 4) as usize;
            let target = (((ty + row) * i64::from(width) + tx) * 4) as usize;
            output[target..target + row_bytes]
                .copy_from_slice(&atlas.pixels[source..source + row_bytes]);
        }
    } else {
        for sprite_y in sy..sy + copy_height {
            for sprite_x in sx..sx + copy_width {
                let ax = i64::from(rect.x) + i64::from(rect.width) - 1 - sprite_y;
                let ay = i64::from(rect.y) + sprite_x;
                let source = ((ay * i64::from(atlas.width) + ax) * 4) as usize;
                let target =
                    (((ty + sprite_y - sy) * i64::from(width) + tx + sprite_x - sx) * 4) as usize;
                output[target..target + 4].copy_from_slice(&atlas.pixels[source..source + 4]);
            }
        }
    }
    Ok((width as u32, height as u32))
}

pub fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    PngEncoder::new().write(path, width, height, rgba)
}

// Keep the compressor's dictionary and buffers between frames in an atlas.
// Resetting its stream preserves allocations without carrying data across PNGs.
struct PngEncoder {
    compressor: flate2::Compress,
    scanlines: Vec<u8>,
    compressed: Vec<u8>,
    idat: Vec<u8>,
}

impl PngEncoder {
    fn new() -> Self {
        Self {
            compressor: flate2::Compress::new(flate2::Compression::fast(), true),
            scanlines: Vec::new(),
            compressed: vec![0; 64 * 1024],
            idat: Vec::with_capacity(64 * 1024),
        }
    }

    fn write(&mut self, path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        ensure!(
            rgba.len() == image_byte_count(width, height)?,
            "RGBA 像素长度与图像尺寸不匹配"
        );
        let mut output = BufWriter::with_capacity(16 * 1024, File::create(path)?);
        output.write_all(PNG_SIGNATURE)?;
        let mut header = [0_u8; 13];
        header[..4].copy_from_slice(&width.to_be_bytes());
        header[4..8].copy_from_slice(&height.to_be_bytes());
        header[8] = 8;
        header[9] = 6; // Eight-bit straight RGBA, preserving transparent RGB.
        write_png_chunk(&mut output, b"IHDR", &header)?;

        let row_bytes = width as usize * 4;
        let scanline_bytes = row_bytes + 1;
        let rows_per_batch = (256 * 1024 / scanline_bytes).max(1).min(height as usize);
        self.scanlines.resize(scanline_bytes * rows_per_batch, 0);
        self.compressor.reset();
        let mut idat = PngIdatWriter {
            output: &mut output,
            pending: std::mem::take(&mut self.idat),
        };
        for source in rgba.chunks(row_bytes * rows_per_batch) {
            for (row, pixels) in source.chunks_exact(row_bytes).enumerate() {
                let start = row * scanline_bytes;
                self.scanlines[start] = 0;
                self.scanlines[start + 1..start + scanline_bytes].copy_from_slice(pixels);
            }
            compress_png(
                &mut self.compressor,
                &self.scanlines[..source.len() + source.len() / row_bytes],
                &mut self.compressed,
                &mut idat,
                flate2::FlushCompress::None,
            )?;
        }
        compress_png(
            &mut self.compressor,
            &[],
            &mut self.compressed,
            &mut idat,
            flate2::FlushCompress::Finish,
        )?;
        self.idat = idat.complete()?;
        write_png_chunk(&mut output, b"IEND", &[])?;
        output.flush()?;
        Ok(())
    }
}

fn compress_png(
    compressor: &mut flate2::Compress,
    mut source: &[u8],
    buffer: &mut [u8],
    output: &mut impl Write,
    flush: flate2::FlushCompress,
) -> Result<()> {
    loop {
        let before_in = compressor.total_in();
        let before_out = compressor.total_out();
        let status = compressor.compress(source, buffer, flush)?;
        let consumed = (compressor.total_in() - before_in) as usize;
        let written = (compressor.total_out() - before_out) as usize;
        output.write_all(&buffer[..written])?;
        source = &source[consumed..];
        if status == flate2::Status::StreamEnd
            || (source.is_empty() && written < buffer.len() && flush == flate2::FlushCompress::None)
        {
            return Ok(());
        }
        ensure!(consumed > 0 || written > 0, "PNG 压缩未能继续");
    }
}

fn write_png_chunk(output: &mut impl Write, kind: &[u8; 4], payload: &[u8]) -> io::Result<()> {
    output.write_all(&(payload.len() as u32).to_be_bytes())?;
    output.write_all(kind)?;
    output.write_all(payload)?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(payload);
    output.write_all(&crc.finalize().to_be_bytes())
}

struct PngIdatWriter<'a, W: Write> {
    output: &'a mut W,
    pending: Vec<u8>,
}

impl<W: Write> PngIdatWriter<'_, W> {
    fn complete(mut self) -> io::Result<Vec<u8>> {
        self.flush()?;
        Ok(self.pending)
    }
}

impl<W: Write> Write for PngIdatWriter<'_, W> {
    fn write(&mut self, mut source: &[u8]) -> io::Result<usize> {
        let count = source.len();
        while !source.is_empty() {
            let copy = (64 * 1024 - self.pending.len()).min(source.len());
            self.pending.extend_from_slice(&source[..copy]);
            source = &source[copy..];
            if self.pending.len() == 64 * 1024 {
                self.flush()?;
            }
        }
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            write_png_chunk(self.output, b"IDAT", &self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
}

fn safe_output_path(root: &Path, name: &str) -> Result<PathBuf> {
    let mut relative = crate::engine::safe_relative(name).context("frame 路径无效")?;
    if relative.extension().is_none() {
        relative.set_extension("png");
    }
    Ok(root.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_encoder_resets_between_different_frame_sizes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("frame.png");
        let mut encoder = PngEncoder::new();
        let mut state = 0x91A2_3456_u32;
        for (width, height) in [(513, 700), (1, 1), (3, 90), (1024, 80)] {
            let pixels: Vec<u8> = (0..width * height * 4)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 24) as u8
                })
                .collect();
            encoder.write(&path, width, height, &pixels).unwrap();
            let decoded = decode_png(&fs::read(&path).unwrap()).unwrap();
            assert_eq!((decoded.width, decoded.height), (width, height));
            assert_eq!(decoded.pixels, pixels);
        }
    }

    #[test]
    fn png_round_trip_crosses_scanline_batches_and_idat_chunks() {
        let (width, height) = (257, 300);
        let mut state = 0xCF12_3456_u32;
        let rgba: Vec<u8> = (0..width * height * 4)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("roundtrip.png");
        write_png(&path, width, height, &rgba).unwrap();
        let encoded = fs::read(path).unwrap();
        let decoded = decode_png(&encoded).unwrap();
        assert_eq!((decoded.width, decoded.height), (width, height));
        assert_eq!(decoded.pixels, rgba);
        let mut offset = 8;
        let mut idat_count = 0;
        while offset < encoded.len() {
            let size = u32::from_be_bytes(encoded[offset..offset + 4].try_into().unwrap()) as usize;
            if &encoded[offset + 4..offset + 8] == b"IDAT" {
                idat_count += 1;
            }
            offset += size + 12;
        }
        assert!(idat_count > 1);
    }

    fn image(width: u32, values: &[u8]) -> RgbaImage {
        RgbaImage {
            width,
            height: values.len() as u32 / width,
            pixels: values.iter().flat_map(|v| [*v, 0, 0, 255]).collect(),
        }
    }

    fn frame(rotated: bool) -> AtlasFrame {
        AtlasFrame {
            name: "sprite.png".into(),
            frame: IntRect {
                x: 0,
                y: 0,
                width: 2,
                height: 3,
            },
            source_size: IntSize {
                width: 2,
                height: 3,
            },
            source_color_rect: IntRect {
                x: 0,
                y: 0,
                width: 2,
                height: 3,
            },
            rotated,
        }
    }

    #[test]
    fn reverses_clockwise_atlas_rotation() {
        let atlas = image(3, &[5, 3, 1, 6, 4, 2]);
        let mut buffer = Vec::new();
        assert_eq!(
            render_frame(&atlas, &frame(true), &mut buffer).unwrap(),
            (2, 3)
        );
        assert_eq!(
            buffer
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p[0])
                .collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 6]
        );
    }

    #[test]
    fn clips_negative_offsets_and_clears_reused_canvas() {
        let atlas = image(2, &[1, 2, 3, 4, 5, 6]);
        let mut frame = frame(false);
        frame.source_color_rect.x = -1;
        frame.source_color_rect.y = 1;
        let mut buffer = vec![99; 24];
        render_frame(&atlas, &frame, &mut buffer).unwrap();
        assert_eq!(
            buffer
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p[0])
                .collect::<Vec<_>>(),
            [0, 0, 2, 0, 4, 0]
        );
        assert_eq!(buffer[3], 0);
    }

    #[test]
    fn rejects_frame_outside_atlas() {
        let atlas = image(2, &[1, 2, 3, 4]);
        assert!(render_frame(&atlas, &frame(false), &mut Vec::new()).is_err());
    }

    #[test]
    fn parses_plist_formats_and_derived_offsets() {
        let xml = br#"<?xml version="1.0"?><plist version="1.0"><dict>
        <key>frames</key><dict><key>sub/sprite</key><dict>
        <key>textureRect</key><string>{{1,2},{3,4}}</string>
        <key>spriteSize</key><string>{3,4}</string>
        <key>spriteSourceSize</key><string>{9,10}</string>
        <key>spriteOffset</key><string>{-1,2}</string>
        <key>textureRotated</key><true/></dict></dict>
        <key>metadata</key><dict><key>realTextureFileName</key><string>atlas.pvr</string></dict>
        </dict></plist>"#;
        let definition = parse_definition(xml, &Game::Carrot1).unwrap().unwrap();
        assert_eq!(definition.texture_file_name.as_deref(), Some("atlas.pvr"));
        assert_eq!(
            definition.frames[0].source_color_rect,
            IntRect {
                x: 2,
                y: 1,
                width: 3,
                height: 4
            }
        );
        assert!(definition.frames[0].rotated);
    }

    #[test]
    fn encodes_windows_special_names_and_rejects_unsafe_paths() {
        let root = Path::new("safe");
        assert_eq!(
            safe_output_path(root, "sub\\sprite<a>?*.png").unwrap(),
            root.join("sub/sprite%3Ca%3E%3F%2A.png")
        );
        assert_eq!(
            safe_output_path(root, "100%/sprite").unwrap(),
            root.join("100%/sprite.png")
        );
        for name in [
            "../..",
            "sub/../sprite",
            "/sprite",
            "CON.png",
            "sub/x:y",
            "sub//sprite",
        ] {
            assert!(safe_output_path(root, name).is_err(), "{name}");
        }
    }

    fn conflicted_plist() -> &'static [u8] {
        br#"<?xml version="1.0"?><plist version="1.0"><dict>
        <key>frames</key><dict>
        <<<<<<< HEAD
        <key>one.png</key><dict><key>frame</key><string>{{0,0},{1,1}}</string></dict>
        =======
        <key>two.png</key><dict><key>frame</key><string>{{1,0},{1,1}}</string></dict>
        >>>>>>> branch
        </dict>
        <key>keptArray</key><array><dict><key>value</key><string>inside-array</string></dict><array><true/><false/></array></array>
        <key>keptDict</key><dict><key>nested</key><array><string>value</string></array></dict>
        <key>metadata</key><dict><key>textureFileName</key><string>atlas.png</string>
        <key>smartupdate</key>
        <<<<<<< HEAD
        <string>first</string>
        =======
        <string>second</string>
        >>>>>>> branch
        <dict><key>orphan</key><string>skipped</string></dict>
        <array><dict><key>orphan</key><string>skipped</string></dict></array>
        <key>after</key><dict><key>retained</key><array><string>yes</string></array></dict>
        </dict></dict></plist>"#
    }

    #[test]
    fn repairs_conflict_markers_without_losing_nested_values_or_frames() {
        let xml = conflicted_plist();
        let repaired = repair_merge_conflicts(xml).unwrap();
        let value = Value::from_reader(Cursor::new(repaired)).unwrap();
        let root = value.as_dictionary().unwrap();
        let array = root["keptArray"].as_array().unwrap();
        assert_eq!(
            array[0].as_dictionary().unwrap()["value"].as_string(),
            Some("inside-array")
        );
        assert_eq!(array[1].as_array().unwrap().len(), 2);
        assert_eq!(
            root["keptDict"].as_dictionary().unwrap()["nested"]
                .as_array()
                .unwrap()[0]
                .as_string(),
            Some("value")
        );
        let metadata = root["metadata"].as_dictionary().unwrap();
        assert_eq!(metadata["smartupdate"].as_string(), Some("first"));
        assert_eq!(metadata.len(), 3);
        assert_eq!(
            metadata["after"].as_dictionary().unwrap()["retained"]
                .as_array()
                .unwrap()[0]
                .as_string(),
            Some("yes")
        );
        let definition = parse_definition(xml, &Game::Carrot4).unwrap().unwrap();
        assert_eq!(definition.frames.len(), 2);
        assert_eq!(definition.texture_file_name.as_deref(), Some("atlas.png"));
        let normal = b"<plist><dict><key>array</key><array><dict/></array></dict></plist>";
        assert!(matches!(
            repair_merge_conflicts(normal).unwrap(),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn validates_wrapped_conflicted_plist_before_repairing_xml() {
        let mut wrapped =
            codec::make_test_custom(conflicted_plist(), &Game::Carrot4.plist_key().unwrap(), 8);
        let definition = parse_definition(&wrapped, &Game::Carrot4).unwrap().unwrap();
        assert_eq!(definition.frames.len(), 2);
        wrapped[8] ^= 1;
        assert!(parse_definition(&wrapped, &Game::Carrot4).is_err());
    }

    #[test]
    fn finds_texture_using_extraction_path_encoding() {
        let root = tempfile::tempdir().unwrap();
        let plist_path = root.path().join("definition.plist");
        let texture = root.path().join("atlas%3Ca%3E.pvr.ccz");
        fs::write(&texture, []).unwrap();
        assert_eq!(
            find_texture(&plist_path, Some("atlas<a>.pvr"), root.path()).unwrap(),
            texture
        );
        assert!(find_texture(&plist_path, Some("../atlas.pvr"), root.path()).is_err());
    }

    #[test]
    fn atlas_texture_decodes_either_wrapped_png_key_with_fallback_rounds() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("texture.png");
        let pixels = [10, 20, 30, 255, 100, 150, 200, 128];
        write_png(&path, 2, 1, &pixels).unwrap();
        let png = fs::read(&path).unwrap();
        for (wrapper_game, selected_game) in [
            (Game::Carrot4, Game::Carrot3),
            (Game::Carrot3, Game::Carrot4),
        ] {
            let wrapped = codec::make_test_custom(
                &png,
                &wrapper_game.plist_key().unwrap(),
                wrapper_game.plist_rounds(),
            );
            fs::write(&path, wrapped).unwrap();
            let decoded = decode_texture(&path, selected_game).unwrap();
            assert_eq!((decoded.width, decoded.height), (2, 1));
            assert_eq!(decoded.pixels, pixels);
        }
    }
}
