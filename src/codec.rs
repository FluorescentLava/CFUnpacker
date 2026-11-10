use std::{
    collections::HashMap,
    io::Read,
    sync::{Arc, OnceLock, RwLock},
};

use anyhow::{Context, Result, bail, ensure};
use flate2::read::ZlibDecoder;

use crate::profile::Key;

const STREAM_LENGTH: usize = 1024;
const MAX_DECODED_LENGTH: usize = 1024 * 1024 * 1024;
type Stream = Arc<[u32; STREAM_LENGTH]>;
type StreamCache = RwLock<HashMap<([u32; 4], u32), Stream>>;
static STREAM_CACHE: OnceLock<StreamCache> = OnceLock::new();

pub fn is_ccz(data: &[u8]) -> bool {
    data.len() >= 16 && (data.starts_with(b"CCZ!") || data.starts_with(b"CCZp"))
}

pub fn decode_ccz(data: &[u8], keys: &[Key]) -> Result<Vec<u8>> {
    decode_ccz_with_key(data, keys).map(|(decoded, _)| decoded)
}

pub fn decode_ccz_with_key(data: &[u8], keys: &[Key]) -> Result<(Vec<u8>, Option<Key>)> {
    ensure!(is_ccz(data), "不是 CCZ 数据。");
    if data[3] == b'!' {
        return inflate_ccz(data).map(|decoded| (decoded, None));
    }

    // Reuse the candidate allocation when trying compatibility keys.
    let mut candidate = data.to_vec();
    for key in keys {
        candidate.copy_from_slice(data);
        transform_ccz(&mut candidate[12..], &key_stream(key, 6));
        if let Ok(decoded) = inflate_ccz(&candidate)
            && looks_like_pvr(&decoded)
        {
            return Ok((decoded, Some(*key)));
        }
    }
    bail!("CCZ 解密失败，所选游戏与 APK 可能不匹配。")
}

pub fn decode_custom(data: &[u8], key: &Key, rounds: u32) -> Result<Vec<u8>> {
    ensure!(data.len() >= 24, "自定义封装头不完整。");
    let expected_length = u32::from_le_bytes(data[16..20].try_into().unwrap()) as usize;
    ensure!(
        expected_length > 0 && expected_length <= MAX_DECODED_LENGTH,
        "自定义封装解压长度无效。"
    );
    let expected_crc = u32::from_le_bytes(data[8..12].try_into().unwrap());
    let mut payload = data[20..].to_vec();
    transform_wrapped(&mut payload, &key_stream(key, rounds));
    let decoded = inflate(&payload, expected_length)?;
    ensure!(
        expected_crc == 0 || crc32fast::hash(&decoded) == expected_crc,
        "自定义封装 CRC32 校验失败。"
    );
    ensure!(
        looks_like_xml(&decoded) || looks_like_png(&decoded) || looks_like_pvr(&decoded),
        "自定义封装解密后格式无效。"
    );
    Ok(decoded)
}

fn inflate_ccz(data: &[u8]) -> Result<Vec<u8>> {
    let expected_length = u32::from_be_bytes(data[12..16].try_into().unwrap()) as usize;
    ensure!(
        expected_length > 0 && expected_length <= MAX_DECODED_LENGTH,
        "CCZ 解压长度无效。"
    );
    inflate(&data[16..], expected_length)
}

fn inflate(compressed: &[u8], expected_length: usize) -> Result<Vec<u8>> {
    // Bound actual expansion as well as the header, including incorrect-key attempts.
    let mut decoder = ZlibDecoder::new(compressed).take((expected_length + 1) as u64);
    let mut output = Vec::with_capacity(expected_length.min(16 * 1024 * 1024));
    decoder
        .read_to_end(&mut output)
        .context("zlib 数据无效。")?;
    ensure!(output.len() == expected_length, "解压长度不一致。");
    Ok(output)
}

fn key_stream(key: &Key, rounds: u32) -> Stream {
    let cache = STREAM_CACHE.get_or_init(|| RwLock::new(HashMap::new()));
    let cache_key = (key.parts, rounds);
    if let Some(stream) = cache
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&cache_key)
        .cloned()
    {
        return stream;
    }
    let stream = Arc::new(generate_key_stream(key.parts, rounds));
    cache
        .write()
        .unwrap_or_else(|poison| poison.into_inner())
        .entry(cache_key)
        .or_insert(stream)
        .clone()
}

fn generate_key_stream(parts: [u32; 4], rounds: u32) -> [u32; STREAM_LENGTH] {
    let mut stream = [0u32; STREAM_LENGTH];
    let mut z = 0u32;
    let mut sum = 0u32;
    for _ in 0..rounds {
        sum = sum.wrapping_add(0x9E3779B9);
        let e = ((sum >> 2) & 3) as usize;
        for p in 0..STREAM_LENGTH - 1 {
            let y = stream[p + 1];
            stream[p] = stream[p].wrapping_add(mix(z, y, sum, parts[(p & 3) ^ e]));
            z = stream[p];
        }
        let y = stream[0];
        stream[STREAM_LENGTH - 1] = stream[STREAM_LENGTH - 1].wrapping_add(mix(
            z,
            y,
            sum,
            parts[((STREAM_LENGTH - 1) & 3) ^ e],
        ));
        z = stream[STREAM_LENGTH - 1];
    }
    stream
}

#[inline]
fn mix(z: u32, y: u32, sum: u32, key_part: u32) -> u32 {
    ((z >> 5) ^ (y << 2)).wrapping_add((y >> 3) ^ (z << 4)) ^ (sum ^ y).wrapping_add(key_part ^ z)
}

fn transform_ccz(data: &mut [u8], stream: &[u32; STREAM_LENGTH]) {
    let words = data.len() / 4;
    let edge = words.min(512);
    for (index, &key) in stream.iter().take(edge).enumerate() {
        xor_word(data, index, key);
    }
    for (key_index, word_index) in (edge..words).step_by(64).enumerate() {
        xor_word(
            data,
            word_index,
            stream[(edge + key_index) & (STREAM_LENGTH - 1)],
        );
    }
}

fn transform_wrapped(data: &mut [u8], stream: &[u32; STREAM_LENGTH]) {
    let words = data.len() / 4;
    let mut index = 0;
    let mut key_index = 0;
    while index < words.min(512) {
        xor_word(data, index, stream[key_index & (STREAM_LENGTH - 1)]);
        index += 1;
        key_index += 1;
    }
    while index < words.saturating_sub(512) {
        xor_word(data, index, stream[key_index & (STREAM_LENGTH - 1)]);
        index += 4;
        key_index += 1;
    }
    while index < words {
        xor_word(data, index, stream[key_index & (STREAM_LENGTH - 1)]);
        index += 1;
        key_index += 1;
    }
}

#[inline]
fn xor_word(data: &mut [u8], index: usize, key: u32) {
    let word = &mut data[index * 4..index * 4 + 4];
    let value = u32::from_le_bytes(word.try_into().unwrap()) ^ key;
    word.copy_from_slice(&value.to_le_bytes());
}

pub fn looks_like_pvr(data: &[u8]) -> bool {
    data.len() >= 52
        && (data[44..48] == 0x21525650u32.to_le_bytes() || data[..4] == 0x03525650u32.to_le_bytes())
}

pub fn looks_like_xml(data: &[u8]) -> bool {
    let data = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);
    let data = data.trim_ascii_start();
    data.len() >= 5 && (data.starts_with(b"<?") || data.starts_with(b"<plist"))
}

pub fn looks_like_png(data: &[u8]) -> bool {
    data.starts_with(b"\x89PNG\r\n\x1a\n")
}

#[cfg(test)]
fn test_compress(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

#[cfg(test)]
pub(crate) fn make_test_ccz(pvr: &[u8], key: &Key) -> Vec<u8> {
    let mut ccz = b"CCZp\0\0\0\x02\0\0\0\0".to_vec();
    ccz.extend_from_slice(&(pvr.len() as u32).to_be_bytes());
    ccz.extend_from_slice(&test_compress(pvr));
    transform_ccz(&mut ccz[12..], &key_stream(key, 6));
    ccz
}

#[cfg(test)]
pub(crate) fn make_test_custom(bytes: &[u8], key: &Key, rounds: u32) -> Vec<u8> {
    let mut wrapped = vec![0; 20];
    if key.parts == crate::profile::CARROT4_PLIST.parts {
        wrapped[..5].copy_from_slice(&[0xff, 0xdb, 0xff, 0xee, 0x66]);
    } else {
        wrapped[..4].copy_from_slice(b"czzf");
    }
    wrapped[8..12].copy_from_slice(&crc32fast::hash(bytes).to_le_bytes());
    wrapped[16..20].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    wrapped.extend_from_slice(&test_compress(bytes));
    transform_wrapped(&mut wrapped[20..], &key_stream(key, rounds));
    wrapped
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::ZlibEncoder};

    use super::*;
    use crate::profile::{CARROT1_PVR, CARROT2_PVR, CARROT3_PLIST, CARROT4_PLIST};

    fn compress(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn pvr_sample() -> Vec<u8> {
        let mut pvr = vec![0; 2049];
        pvr[..4].copy_from_slice(&0x03525650u32.to_le_bytes());
        pvr
    }

    #[test]
    fn ccz_plain_encrypted_and_compatibility_key() {
        let pvr = pvr_sample();
        let mut ccz = b"CCZ!\0\0\0\x02\0\0\0\0".to_vec();
        ccz.extend_from_slice(&(pvr.len() as u32).to_be_bytes());
        ccz.extend_from_slice(&compress(&pvr));
        assert_eq!(decode_ccz(&ccz, &[]).unwrap(), pvr);
        ccz[3] = b'p';
        transform_ccz(&mut ccz[12..], &key_stream(&CARROT2_PVR, 6));
        assert_eq!(decode_ccz(&ccz, &[CARROT1_PVR, CARROT2_PVR]).unwrap(), pvr);
        assert!(decode_ccz(&ccz, &[CARROT1_PVR]).is_err());
    }

    #[test]
    fn custom_wrapping_crc_and_rounds() {
        // A long, incompressible suffix exercises the sparse middle and final edge.
        let mut xml = b"<?xml version=\"1.0\"?><plist>".to_vec();
        let mut state = 0x12345678u32;
        for _ in 0..16000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            xml.push(state as u8);
        }
        for (key, rounds) in [(CARROT3_PLIST, 6), (CARROT4_PLIST, 8)] {
            let mut wrapped = vec![0; 20];
            wrapped[8..12].copy_from_slice(&crc32fast::hash(&xml).to_le_bytes());
            wrapped[16..20].copy_from_slice(&(xml.len() as u32).to_le_bytes());
            wrapped.extend_from_slice(&compress(&xml));
            transform_wrapped(&mut wrapped[20..], &key_stream(&key, rounds));
            assert_eq!(decode_custom(&wrapped, &key, rounds).unwrap(), xml);
            wrapped[8] ^= 1;
            assert!(decode_custom(&wrapped, &key, rounds).is_err());
        }
    }

    #[test]
    fn rejects_expansion_beyond_declared_length() {
        assert!(inflate(&compress(&vec![0; 10000]), 8).is_err());
        assert!(decode_ccz(b"CCZp", &[CARROT1_PVR]).is_err());
        assert!(decode_custom(&[0; 24], &CARROT3_PLIST, 6).is_err());
    }
}
