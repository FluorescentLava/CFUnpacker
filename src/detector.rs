use crate::{
    bundle::{ApkBundle, FileWindow},
    codec,
    profile::Game,
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
use zip::ZipArchive;

const SAMPLE_LIMIT: u64 = 32 * 1024 * 1024;
const CARROT4_HEADER: &[u8] = &[0xff, 0xdb, 0xff, 0xee, 0x66];

#[derive(Debug, Serialize)]
pub struct Detection {
    pub game: Option<&'static str>,
    pub compatible: Vec<&'static str>,
    pub evidence: String,
    pub elapsed_seconds: f64,
}

struct Asset {
    source: usize,
    index: usize,
    name: String,
    size: u64,
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "已取消");
    Ok(())
}

fn read_sample(
    archives: &mut [ZipArchive<FileWindow>],
    asset: &Asset,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    ensure!(asset.size <= SAMPLE_LIMIT, "检测样本超过 32 MiB 限制");
    let mut entry = archives[asset.source].by_index(asset.index)?;
    let mut data = Vec::with_capacity(asset.size as usize);
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        check_cancel(cancel)?;
        let count = entry.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        ensure!(
            data.len() as u64 + count as u64 <= asset.size,
            "检测样本长度超出声明值"
        );
        data.extend_from_slice(&buffer[..count]);
    }
    ensure!(data.len() as u64 == asset.size, "检测样本长度不一致");
    Ok(data)
}

pub fn detect(path: &Path, cancel: &AtomicBool) -> Result<Detection> {
    check_cancel(cancel)?;
    let start = Instant::now();
    let bundle = ApkBundle::resolve(path, cancel)?;
    let mut archives = bundle.open_archives()?;
    let mut assets = Vec::new();
    for (source, archive) in archives.iter_mut().enumerate() {
        for index in 0..archive.len() {
            check_cancel(cancel)?;
            let entry = archive.by_index(index)?;
            let name = entry.name().replace('\\', "/").to_ascii_lowercase();
            if !entry.is_dir() && name.starts_with("assets/") {
                assets.push(Asset {
                    source,
                    index,
                    name,
                    size: entry.size(),
                });
            }
        }
    }
    let finish = |game: Option<Game>, compatible: Vec<Game>, evidence: String| Detection {
        game: game.map(Game::id),
        compatible: compatible.into_iter().map(Game::id).collect(),
        evidence,
        elapsed_seconds: start.elapsed().as_secs_f64(),
    };
    if assets.is_empty() {
        return Ok(finish(None, vec![], "APK 中没有 assets 资源目录。".into()));
    }
    let has_prefix = |prefix: &str| assets.iter().any(|asset| asset.name.starts_with(prefix));
    let has_entry = |wanted: &str| assets.iter().any(|asset| asset.name == wanted);
    let has_ext = |prefix: &str, ext: &str| {
        assets
            .iter()
            .any(|asset| asset.name.starts_with(prefix) && asset.name.ends_with(ext))
    };

    // Separate sampling keeps early CF4 plaintext plists from hiding wrapped PNGs.
    let mut czzf = false;
    let mut wrapped: Option<&Asset> = None;
    for extension in [".plist", ".png"] {
        let mut candidates: Vec<_> = assets
            .iter()
            .filter(|asset| asset.name.ends_with(extension))
            .collect();
        candidates.sort_by_key(|asset| asset.size);
        for asset in candidates.into_iter().take(48) {
            check_cancel(cancel)?;
            let mut header = Vec::with_capacity(5);
            archives[asset.source]
                .by_index(asset.index)?
                .take(5)
                .read_to_end(&mut header)?;
            if header.starts_with(CARROT4_HEADER) {
                if wrapped.is_none_or(|previous| asset.size < previous.size) {
                    wrapped = Some(asset);
                }
            } else {
                czzf |= header.starts_with(b"czzf");
            }
        }
    }
    if let Some(sample) = wrapped {
        check_cancel(cancel)?;
        let verified = sample.size <= SAMPLE_LIMIT
            && codec::decode_custom(
                &read_sample(&mut archives, sample, cancel)?,
                &Game::Carrot4.plist_key().unwrap(),
                8,
            )
            .is_ok();
        check_cancel(cancel)?;
        let evidence = if verified {
            "检测到四代的 ff db ff ee 66 加密资源头，样本通过四代 key 解密（国际版共用该 key）。"
        } else if sample.size > SAMPLE_LIMIT {
            "检测到四代的 ff db ff ee 66 加密资源头（样本超过检测大小限制，按四代处理）。"
        } else {
            "检测到四代的 ff db ff ee 66 加密资源头（样本 key 校验未通过，按四代处理）。"
        };
        return Ok(finish(
            Some(Game::Carrot4),
            vec![Game::Carrot4],
            evidence.into(),
        ));
    }
    if czzf {
        let abo = (has_prefix("assets/pandora/") || has_prefix("assets/tusdk.bundle/"))
            && !has_prefix("assets/carrot3/");
        return Ok(finish(
            Some(if abo {
                Game::AboAdventure
            } else {
                Game::Carrot3
            }),
            vec![Game::Carrot3, Game::AboAdventure],
            "检测到 czzf 加密 plist，结合特征目录选择三代或阿波之旅。".into(),
        ));
    }
    let mut candidates: Vec<_> = assets
        .iter()
        .filter(|asset| {
            asset.name.ends_with(".pvr.ccz") && (16..=SAMPLE_LIMIT).contains(&asset.size)
        })
        .collect();
    candidates.sort_by_key(|asset| asset.size);
    for sample in candidates.into_iter().take(4) {
        let data = read_sample(&mut archives, sample, cancel)?;
        if !codec::is_ccz(&data) || data[3] != b'p' {
            continue;
        }
        for game in [Game::Carrot1, Game::Carrot2, Game::Carrot4, Game::Carrot3] {
            check_cancel(cancel)?;
            if codec::decode_ccz(&data, &game.pvr_keys()[..1]).is_err() {
                continue;
            }
            let (compatible, evidence) = match game {
                Game::Carrot1 => (
                    vec![Game::Carrot1],
                    "CCZ 样本通过一代 key 解密并验证为 PVR。",
                ),
                Game::Carrot2 => (
                    vec![Game::Carrot1, Game::Carrot2],
                    "CCZ 样本使用二代 key；一代部分版本也使用该 key，两个旧版流程均视为兼容。",
                ),
                Game::Carrot4 => (
                    vec![Game::Carrot4, Game::Carrot3],
                    "CCZ 样本通过四代 key 解密并验证为 PVR。",
                ),
                Game::Carrot3 => (
                    vec![Game::Carrot3, Game::AboAdventure],
                    "CCZ 样本通过三代 key 解密并验证为 PVR。",
                ),
                Game::AboAdventure => unreachable!(),
            };
            check_cancel(cancel)?;
            return Ok(finish(Some(game), compatible, evidence.into()));
        }
    }
    let score1 = 3 * has_ext("assets/music/", ".ogg") as u32
        + 2 * has_prefix("assets/themes/scene/") as u32
        + 2 * has_entry("assets/themes/items/commontip.plist") as u32;
    let score2 = 3 * has_prefix("assets/themes_cn/") as u32
        + 2 * has_prefix("assets/themes/daymaps/") as u32
        + 2 * has_ext("assets/music/", ".mp3") as u32
        + has_entry("assets/info.plist") as u32;
    let score4 =
        3 * has_prefix("assets/res/") as u32 + 2 * has_ext("assets/res/", ".exportjson") as u32;
    check_cancel(cancel)?;
    if score1 >= 3 || score2 >= 3 || score4 >= 3 {
        let (game, evidence) = if score4 > score1 && score4 > score2 {
            (Game::Carrot4, "四代资源目录特征得分更高。")
        } else if score2 > score1 {
            (Game::Carrot2, "二代资源目录特征得分更高。")
        } else {
            (Game::Carrot1, "一代资源目录特征得分更高。")
        };
        return Ok(finish(Some(game), vec![game], evidence.into()));
    }
    Ok(finish(
        None,
        vec![],
        "没有检测到受支持的加密头、可验证 CCZ key 或足够的系列资源特征。".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::{Cursor, Write},
    };
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    fn apk(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            zip.start_file(
                *name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
    fn detect_entries(entries: &[(&str, Vec<u8>)]) -> Detection {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sample.apk");
        fs::write(&path, apk(entries)).unwrap();
        detect(&path, &AtomicBool::new(false)).unwrap()
    }
    fn encrypted_pvr(game: Game) -> Vec<u8> {
        let mut pvr = vec![0; 52];
        pvr[..4].copy_from_slice(&0x03525650_u32.to_le_bytes());
        codec::make_test_ccz(&pvr, &game.pvr_keys()[0])
    }
    #[test]
    fn recognizes_all_ccz_keys_before_conflicting_directory_scores() {
        for (game, compatible) in [
            (Game::Carrot1, vec!["cf1"]),
            (Game::Carrot2, vec!["cf1", "cf2"]),
            (Game::Carrot3, vec!["cf3", "abo"]),
            (Game::Carrot4, vec!["cf4", "cf3"]),
        ] {
            let result = detect_entries(&[
                (
                    "assets/plain.plist",
                    b"<?xml version=\"1.0\"?><plist><dict/></plist>".to_vec(),
                ),
                ("assets/texture.pvr.ccz", encrypted_pvr(game)),
                ("assets/Music/theme.ogg", Vec::new()),
                ("assets/Themes/scene/test", Vec::new()),
                ("assets/res/animation.ExportJson", Vec::new()),
            ]);
            assert_eq!(result.game, Some(game.id()));
            assert_eq!(result.compatible, compatible);
            assert!(result.evidence.contains("key"));
        }
    }
    #[test]
    fn independently_samples_pngs_and_validates_cf4_wrapper() {
        let mut entries: Vec<_> = (0..80)
            .map(|index| (format!("assets/{index}.plist"), b"<plist/>".to_vec()))
            .collect();
        entries.push((
            "assets/wrapped.png".into(),
            codec::make_test_custom(b"\x89PNG\r\n\x1a\n", &Game::Carrot4.plist_key().unwrap(), 8),
        ));
        let borrowed: Vec<_> = entries
            .iter()
            .map(|(name, data)| (name.as_str(), data.clone()))
            .collect();
        let result = detect_entries(&borrowed);
        assert_eq!(result.game, Some("cf4"));
        assert!(result.evidence.contains("样本通过"));
        let result = detect_entries(&[("assets/invalid.png", CARROT4_HEADER.to_vec())]);
        assert_eq!(result.game, Some("cf4"));
        assert!(result.evidence.contains("未通过"));
    }
    #[test]
    fn recognizes_early_cf4_layout_and_czzf_identity() {
        let result = detect_entries(&[("assets/res/animation.ExportJson", Vec::new())]);
        assert_eq!(result.game, Some("cf4"));
        for (marker, expected) in [
            ("assets/pandora/resource", "abo"),
            ("assets/Carrot3/resource", "cf3"),
        ] {
            let result = detect_entries(&[
                ("assets/data.plist", b"czzf".to_vec()),
                (marker, Vec::new()),
            ]);
            assert_eq!(result.game, Some(expected));
            assert_eq!(result.compatible, ["cf3", "abo"]);
        }
    }
    #[test]
    fn detects_assets_in_another_apks_archive_and_honors_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("sample.apks.1");
        fs::write(
            &path,
            apk(&[
                (
                    "base.apk",
                    apk(&[("AndroidManifest.xml", b"manifest".to_vec())]),
                ),
                (
                    "assets.apk",
                    apk(&[("assets/texture.pvr.ccz", encrypted_pvr(Game::Carrot4))]),
                ),
            ]),
        )
        .unwrap();
        let result = detect(&path, &AtomicBool::new(false)).unwrap();
        assert_eq!(result.game, Some("cf4"));
        assert!(detect(&path, &AtomicBool::new(true)).is_err());
    }
}
