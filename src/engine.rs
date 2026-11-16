use crate::{
    atlas,
    bundle::{self, ApkBundle},
    codec,
    profile::Game,
};
use anyhow::{Context, Result, bail, ensure};
use rayon::prelude::*;
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

#[derive(Debug, Serialize)]
pub struct Progress {
    pub stage: &'static str,
    pub percent: f64,
    pub message: String,
    pub completed_items: usize,
    pub total_items: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub output_path: String,
    pub assets_extracted: usize,
    pub plists_scanned: usize,
    pub atlases_decoded: usize,
    pub frames_written: usize,
    pub decrypted_standalone_pngs: usize,
    pub skipped_items: usize,
    pub elapsed_seconds: f64,
    pub warnings: Vec<String>,
}

pub fn default_workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(1, 12))
        .unwrap_or(1)
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "已取消");
    Ok(())
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let mut candidate = path.components();
        root.components().all(|expected| {
            candidate.next().is_some_and(|actual| {
                actual
                    .as_os_str()
                    .eq_ignore_ascii_case(expected.as_os_str())
            })
        })
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

fn progress(
    report: &(impl Fn(Progress) + Sync),
    stage: &'static str,
    percent: f64,
    message: String,
    done: usize,
    total: usize,
) {
    report(Progress {
        stage,
        percent,
        message,
        completed_items: done,
        total_items: total,
    });
}

/// Use the same Windows-safe percent mapping for ZIP names and plist references.
/// Structural traversal, ADS, device names and ambiguous aliases remain errors.
pub fn safe_relative(name: &str) -> Result<PathBuf> {
    let normalized = name.replace('\\', "/");
    ensure!(
        !normalized.is_empty() && !normalized.starts_with('/'),
        "APK 条目路径无效：{name}"
    );
    let mut result = PathBuf::new();
    for part in normalized.split('/') {
        ensure!(
            !part.is_empty() && part != "." && part != ".." && !part.contains(':'),
            "APK 条目路径越界或无效：{name}"
        );
        let stem = part
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        ensure!(
            !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9')),
            "APK 条目使用 Windows 保留名称：{name}"
        );
        let trailing = part.trim_end_matches([' ', '.']).len();
        let mut encoded = String::with_capacity(part.len());
        for (index, character) in part.char_indices() {
            if character.is_control()
                || "<>\"|?*".contains(character)
                || (index >= trailing && matches!(character, ' ' | '.'))
            {
                let mut bytes = [0; 4];
                for byte in character.encode_utf8(&mut bytes).bytes() {
                    use std::fmt::Write as _;
                    write!(&mut encoded, "%{byte:02X}").unwrap();
                }
            } else {
                encoded.push(character);
            }
        }
        result.push(encoded);
    }
    Ok(result)
}

struct Extraction {
    assets: usize,
    decrypted_pngs: usize,
    warnings: Vec<String>,
}

fn copy_asset(
    source: &mut impl Read,
    output: &mut impl Write,
    buffer: &mut [u8],
    expected: u64,
    cancel: &AtomicBool,
) -> Result<()> {
    let mut copied = 0_u64;
    loop {
        check_cancel(cancel)?;
        let read = source.read(buffer)?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        ensure!(copied <= expected, "APK 实际资源长度超出声明值");
        output.write_all(&buffer[..read])?;
    }
    ensure!(copied == expected, "APK 资源长度不一致");
    Ok(())
}

fn is_wrapped_png(source: &[u8]) -> bool {
    source.starts_with(b"czzf") || source.starts_with(&[0xff, 0xdb, 0xff, 0xee, 0x66])
}

fn decode_wrapped_png(
    source: &[u8],
    keys: &[(crate::profile::Key, u32)],
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    for (key, rounds) in keys {
        check_cancel(cancel)?;
        if let Ok(decoded) = codec::decode_custom(source, key, *rounds) {
            ensure!(codec::looks_like_png(&decoded), "封装中的资源不是 PNG");
            return Ok(decoded);
        }
    }
    bail!("没有可通过长度与 CRC32 校验的 PNG 解密 key")
}

fn extract(
    bundle: &ApkBundle,
    root: &Path,
    game: Game,
    workers: usize,
    cancel: &AtomicBool,
    report: &(impl Fn(Progress) + Sync),
) -> Result<Extraction> {
    let mut entries = Vec::new();
    let mut seen = HashMap::new();
    let mut directories = HashSet::new();
    let mut total_size = 0_u64;
    for (source_index, mut zip) in bundle.open_archives()?.into_iter().enumerate() {
        for index in 0..zip.len() {
            check_cancel(cancel)?;
            let entry = zip.by_index(index)?;
            let name = entry.name().replace('\\', "/");
            if entry.is_dir()
                || !name
                    .get(..7)
                    .is_some_and(|s| s.eq_ignore_ascii_case("assets/"))
            {
                continue;
            }
            ensure!(
                entry
                    .unix_mode()
                    .is_none_or(|mode| mode & 0o170000 != 0o120000),
                "拒绝 ZIP 符号链接：{name}"
            );
            ensure!(
                entry.size() <= 1024 * 1024 * 1024,
                "APK 资源超过 1 GiB：{name}"
            );
            total_size = total_size
                .checked_add(entry.size())
                .context("资源大小溢出")?;
            ensure!(
                total_size <= 64 * 1024 * 1024 * 1024,
                "APK 资源总大小超过 64 GiB"
            );
            let relative = safe_relative(&name[7..])?;
            let first = relative
                .components()
                .next()
                .unwrap()
                .as_os_str()
                .to_string_lossy()
                .to_ascii_lowercase();
            ensure!(
                !["unpacked_png", "readme.md", "_unpack_log.txt"].contains(&first.as_str()),
                "APK 条目与输出文件冲突：{name}"
            );
            let identity = relative.to_string_lossy().to_lowercase();
            if let Some((previous_name, previous_source)) = seen.get(&identity) {
                // base.apk wins only for an identical resource name in another split.
                ensure!(
                    previous_name == &name[7..] && *previous_source != source_index,
                    "APK 条目在 Windows 上重名或编码冲突：{name}"
                );
                continue;
            }
            seen.insert(identity, (name[7..].to_owned(), source_index));
            let path = root.join(relative);
            let directory = path.parent().context("资源父目录无效")?;
            if directories.insert(directory.to_owned()) {
                fs::create_dir_all(directory)?;
            }
            entries.push((source_index, index, path));
        }
    }
    ensure!(!entries.is_empty(), "APK 中没有 assets 资源目录。");
    let count = AtomicUsize::new(0);
    let decrypted_pngs = AtomicUsize::new(0);
    let warnings = Mutex::new(Vec::new());
    let keys = game.wrapper_keys();
    (0..workers.min(entries.len()))
        .into_par_iter()
        .try_for_each(|worker| -> Result<()> {
            let mut archives = bundle.open_archives()?;
            let mut buffer = vec![0_u8; 256 * 1024];
            let mut wrapped = Vec::new();
            for (source_index, index, output) in entries
                .iter()
                .skip(worker)
                .step_by(workers.min(entries.len()))
            {
                check_cancel(cancel)?;
                let mut entry = archives[*source_index].by_index(*index)?;
                let png = output
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("png"));
                let mut first = entry.read(&mut buffer)?;
                // Read implementations may return fewer bytes than requested.
                while png && first < 5 && (first as u64) < entry.size() {
                    let read = entry.read(&mut buffer[first..5])?;
                    ensure!(read != 0, "APK 资源长度不一致");
                    first += read;
                }
                ensure!(first as u64 <= entry.size(), "APK 资源长度不一致");
                let remaining = entry.size() - first as u64;
                if png && is_wrapped_png(&buffer[..first]) {
                    wrapped.clear();
                    wrapped.extend_from_slice(&buffer[..first]);
                    copy_asset(&mut entry, &mut wrapped, &mut buffer, remaining, cancel)?;
                    // The entire output is already private staging. Decode before its
                    // first write; unpack's directory commit provides atomicity.
                    match decode_wrapped_png(&wrapped, &keys, cancel) {
                        Ok(decoded) => {
                            check_cancel(cancel)?;
                            fs::write(output, decoded)?;
                            decrypted_pngs.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => {
                            check_cancel(cancel)?;
                            fs::write(output, &wrapped)?;
                            warnings.lock().unwrap().push(format!(
                                "独立图片解密失败 {}: {error:#}",
                                output.strip_prefix(root).unwrap_or(output).display()
                            ));
                        }
                    }
                } else {
                    let mut file = File::create(output)?;
                    file.write_all(&buffer[..first])?;
                    copy_asset(&mut entry, &mut file, &mut buffer, remaining, cancel)?;
                }
                let done = count.fetch_add(1, Ordering::Relaxed) + 1;
                if done.is_multiple_of(32) || done == entries.len() {
                    progress(
                        report,
                        "Extracting",
                        2.0 + 34.0 * done as f64 / entries.len() as f64,
                        format!("正在提取资源 {done} / {}", entries.len()),
                        done,
                        entries.len(),
                    );
                }
            }
            Ok(())
        })?;
    Ok(Extraction {
        assets: count.into_inner(),
        decrypted_pngs: decrypted_pngs.into_inner(),
        warnings: warnings.into_inner().unwrap(),
    })
}

pub fn split_directory(
    root: &Path,
    output: &Path,
    game: Game,
    workers: usize,
    cancel: &AtomicBool,
    report: &(impl Fn(Progress) + Sync),
) -> Result<Stats> {
    ensure!((1..=256).contains(&workers), "并行度应在 1 到 256 之间");
    let start = Instant::now();
    let plists: Vec<_> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            e.path() != output
                && !e
                    .file_name()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("Unpacked_PNG")
        })
        .map(|e| {
            check_cancel(cancel)?;
            e.context("扫描资源目录失败")
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|e| {
            e.file_type().is_file()
                && e.path()
                    .extension()
                    .is_some_and(|s| s.eq_ignore_ascii_case("plist"))
        })
        .map(|e| e.into_path())
        .collect();
    fs::create_dir_all(output)?;
    let (frames, atlases, skipped, completed) = (
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    );
    let warnings = Mutex::new(Vec::new());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()?;
    pool.install(|| {
        plists.par_iter().try_for_each(|path| -> Result<()> {
            check_cancel(cancel)?;
            match atlas::split(path, root, output, game, cancel) {
                Ok(0) => {
                    skipped.fetch_add(1, Ordering::Relaxed);
                }
                Ok(n) => {
                    frames.fetch_add(n, Ordering::Relaxed);
                    atlases.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    check_cancel(cancel)?;
                    skipped.fetch_add(1, Ordering::Relaxed);
                    warnings.lock().unwrap().push(format!(
                        "{}: {error:#}",
                        path.strip_prefix(root).unwrap_or(path).display()
                    ));
                }
            }
            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            if done % 8 == 0 || done == plists.len() {
                progress(
                    report,
                    "Splitting",
                    40.0 + 54.0 * done as f64 / plists.len() as f64,
                    format!(
                        "正在拆分图集 {done} / {}，已输出 {} 张 PNG",
                        plists.len(),
                        frames.load(Ordering::Relaxed)
                    ),
                    done,
                    plists.len(),
                );
            }
            Ok(())
        })
    })?;
    check_cancel(cancel)?;
    let mut warnings = warnings.into_inner().unwrap();
    warnings.sort();
    Ok(Stats {
        output_path: output.to_string_lossy().into_owned(),
        plists_scanned: plists.len(),
        atlases_decoded: atlases.into_inner(),
        frames_written: frames.into_inner(),
        skipped_items: skipped.into_inner(),
        elapsed_seconds: start.elapsed().as_secs_f64(),
        warnings,
        ..Stats::default()
    })
}

pub fn unpack(
    apk: &Path,
    parent: &Path,
    game: Game,
    overwrite: bool,
    workers: usize,
    cancel: &AtomicBool,
    report: &(impl Fn(Progress) + Sync),
) -> Result<Stats> {
    ensure!((1..=256).contains(&workers), "并行度应在 1 到 256 之间");
    ensure!(
        bundle::is_supported_input(apk) && apk.is_file(),
        "请选择存在的 .apk / .apks / .xapk 文件（支持 .apk.1 等数字后缀）"
    );
    let start = Instant::now();
    let apk = fs::canonicalize(apk)?;
    fs::create_dir_all(parent)?;
    let parent = fs::canonicalize(parent)?;
    let input_name = bundle::trim_numeric_suffix(&apk);
    let stem = input_name
        .file_stem()
        .context("APK 文件名无效")?
        .to_string_lossy();
    let stem = safe_relative(&stem)?.to_string_lossy().into_owned();
    let final_path = parent.join(&stem);
    ensure!(
        !final_path.exists() || (overwrite && final_path.is_dir()),
        "输出目录已存在：{}",
        final_path.display()
    );
    if final_path.exists() {
        // Canonicalize reparse points and compare with Windows path casing rules.
        let existing_output = fs::canonicalize(&final_path)?;
        ensure!(
            !path_is_within(&apk, &existing_output),
            "输入 APK 不能位于待替换的输出目录内"
        );
    }
    let staging = tempfile::Builder::new()
        .prefix(&format!(".{stem}.unpacking-"))
        .tempdir_in(&parent)?;
    progress(report, "Preparing", 0.0, "正在检查 APK…".into(), 0, 0);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()?;
    let bundle = ApkBundle::resolve(&apk, cancel)?;
    let extracted =
        pool.install(|| extract(&bundle, staging.path(), game, workers, cancel, report))?;
    drop(bundle);
    progress(
        report,
        "Scanning",
        38.0,
        "正在扫描 plist 和纹理…".into(),
        0,
        0,
    );
    let mut stats = split_directory(
        staging.path(),
        &staging.path().join("Unpacked_PNG"),
        game,
        workers,
        cancel,
        report,
    )?;
    stats.assets_extracted = extracted.assets;
    stats.decrypted_standalone_pngs = extracted.decrypted_pngs;
    stats.warnings.extend(extracted.warnings);
    stats.warnings.sort();
    stats.output_path = final_path.to_string_lossy().into_owned();
    check_cancel(cancel)?;
    progress(report, "Scanning", 94.0, "正在完成资源校验…".into(), 0, 0);
    stats.elapsed_seconds = start.elapsed().as_secs_f64();
    check_cancel(cancel)?;
    progress(
        report,
        "Finalizing",
        96.0,
        "正在写入说明并清理中间步骤…".into(),
        0,
        0,
    );
    let keys: Vec<_> = game
        .pvr_keys()
        .iter()
        .map(|key| format!("- {}: {:?}", key.name, key.parts))
        .collect();
    fs::write(
        staging.path().join("README.md"),
        format!(
            "# {}资源解包说明\n\n解包工具作者：熔萤FluorescentLava\n\n本工具的格式分析、算法整理、Rust 程序实现与验证借助了 AI 工具。\n\n输入 APK：{}\n\n从 APK 并行提取 assets，在内存中解密并拆分图集；最终 PNG 位于 Unpacked_PNG，不输出整张 atlas PNG。CCZ/PVR 和自定义封装的长度及 CRC32 均会验证。\n\n## key\n\n{}\n\n## 本次统计\n\n- 提取资源：{}\n- 扫描 plist：{}\n- 解出图集：{}\n- 拆分 PNG：{}\n- 就地解密独立 PNG：{}\n- 跳过项目：{}\n- 用时：{:.3} 秒\n\n详细统计和警告见 _unpack_log.txt。\n",
            game.display_name(),
            apk.display(),
            keys.join("\n"),
            stats.assets_extracted,
            stats.plists_scanned,
            stats.atlases_decoded,
            stats.frames_written,
            stats.decrypted_standalone_pngs,
            stats.skipped_items,
            stats.elapsed_seconds
        ),
    )?;
    fs::write(
        staging.path().join("_unpack_log.txt"),
        serde_json::to_string_pretty(&stats)?,
    )?;
    check_cancel(cancel)?;
    if final_path.exists() {
        ensure!(overwrite, "输出目录已存在");
        let backup = tempfile::Builder::new()
            .prefix(&format!(".{stem}.backup-"))
            .tempdir_in(&parent)?;
        let previous = backup.path().join("previous");
        fs::rename(&final_path, &previous)?;
        if let Err(error) = fs::rename(staging.path(), &final_path) {
            if let Err(restore) = fs::rename(&previous, &final_path) {
                let recovery = backup.keep();
                bail!(
                    "提交失败：{error}；还原失败：{restore}；原输出保留在 {}",
                    recovery.display()
                );
            }
            return Err(error.into());
        }
        if let Err(error) = backup.close() {
            stats.warnings.push(format!("旧输出备份未能清理：{error}"));
        }
    } else {
        fs::rename(staging.path(), &final_path)?;
    }
    stats.elapsed_seconds = start.elapsed().as_secs_f64();
    progress(report, "Completed", 100.0, "解包完成。".into(), 0, 0);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in files {
            zip.start_file(
                *name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn merges_base_and_asset_split_without_copying_over_base_resources() {
        let root = tempfile::tempdir().unwrap();
        let base = make_zip(&[
            ("assets/common.dat", b"base wins"),
            ("assets/a<b>.dat", b"encoded"),
        ]);
        let split = make_zip(&[
            ("assets/common.dat", b"split loses"),
            ("assets/extra.dat", b"extra"),
        ]);
        let input = root.path().join("game.apks.2");
        fs::write(
            &input,
            make_zip(&[
                ("assets.apk", &split),
                ("base.apk", &base),
                ("split_config.arm64.apk", b"ignored invalid ZIP"),
            ]),
        )
        .unwrap();
        let result = unpack(
            &input,
            &root.path().join("out"),
            Game::Carrot1,
            false,
            2,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        assert_eq!(result.assets_extracted, 3);
        let output = Path::new(&result.output_path);
        assert_eq!(output.file_name().unwrap(), "game");
        assert_eq!(fs::read(output.join("common.dat")).unwrap(), b"base wins");
        assert_eq!(fs::read(output.join("extra.dat")).unwrap(), b"extra");
        assert_eq!(fs::read(output.join("a%3Cb%3E.dat")).unwrap(), b"encoded");
    }

    #[test]
    fn rejects_percent_mapping_collisions_and_keeps_previous_output() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("game.apk.1");
        fs::write(
            &input,
            make_zip(&[("assets/a<.dat", b"one"), ("assets/a%3C.dat", b"two")]),
        )
        .unwrap();
        let output = root.path().join("out/game");
        fs::create_dir_all(&output).unwrap();
        fs::write(output.join("previous"), b"preserved").unwrap();
        let error = unpack(
            &input,
            &root.path().join("out"),
            Game::Carrot1,
            true,
            1,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("编码冲突"), "{error:#}");
        assert_eq!(fs::read(output.join("previous")).unwrap(), b"preserved");
        assert_eq!(fs::read_dir(root.path().join("out")).unwrap().count(), 1);
    }

    #[test]
    fn cancelled_unpack_keeps_previous_output_and_removes_staging() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("game.apk");
        fs::write(&input, make_zip(&[("assets/data", b"new data")])).unwrap();
        let parent = root.path().join("out");
        fs::create_dir_all(parent.join("game")).unwrap();
        fs::write(parent.join("game/previous"), b"preserved").unwrap();
        let cancel = AtomicBool::new(false);
        let error = unpack(
            &input,
            &parent,
            Game::Carrot1,
            true,
            1,
            &cancel,
            &|progress| {
                if progress.stage == "Scanning" && progress.percent == 94.0 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("已取消"));
        assert_eq!(
            fs::read(parent.join("game/previous")).unwrap(),
            b"preserved"
        );
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
    }

    #[test]
    fn decrypts_standalone_png_wrappers_in_place_and_preserves_failed_sources() {
        let root = tempfile::tempdir().unwrap();
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[123, 45, 67, 89]).unwrap();
        }
        let cf4 = codec::make_test_custom(&png, &crate::profile::CARROT4_PLIST, 8);
        let czzf = codec::make_test_custom(&png, &crate::profile::CARROT3_PLIST, 6);
        let mut bad_crc = cf4.clone();
        bad_crc[8] ^= 1;
        let input = root.path().join("standalone.apk");
        fs::write(
            &input,
            make_zip(&[
                ("assets/res/four.png", &cf4),
                ("assets/res/three.PNG", &czzf),
                ("assets/plain.png", &png),
                ("assets/broken.png", &bad_crc),
                ("assets/empty.PNG", &[]),
                ("assets/short.png", &[1, 2, 3]),
            ]),
        )
        .unwrap();
        // Both wrapper key families remain available even with another selected game.
        let result = unpack(
            &input,
            &root.path().join("out"),
            Game::Carrot1,
            false,
            2,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        assert_eq!(result.decrypted_standalone_pngs, 2);
        assert_eq!(result.frames_written, 0);
        assert_eq!(result.warnings.len(), 1);
        let output = Path::new(&result.output_path);
        for path in ["res/four.png", "res/three.PNG", "plain.png"] {
            assert_eq!(fs::read(output.join(path)).unwrap(), png);
        }
        assert_eq!(fs::read(output.join("broken.png")).unwrap(), bad_crc);
        assert!(fs::read(output.join("empty.PNG")).unwrap().is_empty());
        assert_eq!(fs::read(output.join("short.png")).unwrap(), [1, 2, 3]);
        assert_eq!(
            fs::read_dir(output.join("Unpacked_PNG")).unwrap().count(),
            0
        );
    }

    #[test]
    fn splits_wrapped_png_texture_after_decrypting_during_extraction() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("texture.apk");
        let texture = root.path().join("texture.png");
        let rgba = [123, 45, 67, 89];
        atlas::write_png(&texture, 1, 1, &rgba).unwrap();
        let png = fs::read(texture).unwrap();
        let wrapped = codec::make_test_custom(&png, &crate::profile::CARROT4_PLIST, 8);
        let plist = br#"<?xml version="1.0"?><plist version="1.0"><dict>
            <key>frames</key><dict><key>sprite.png</key><dict>
            <key>frame</key><string>{{0,0},{1,1}}</string>
            <key>sourceSize</key><string>{1,1}</string></dict></dict>
            <key>metadata</key><dict><key>textureFileName</key><string>atlas.png</string>
            </dict></dict></plist>"#;
        fs::write(
            &input,
            make_zip(&[
                ("assets/res/atlas.png", &wrapped),
                ("assets/res/atlas.plist", plist),
            ]),
        )
        .unwrap();
        let stats = unpack(
            &input,
            &root.path().join("out"),
            Game::Carrot4,
            false,
            2,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap();
        assert_eq!(stats.decrypted_standalone_pngs, 1);
        assert_eq!(stats.frames_written, 1);
        assert!(stats.warnings.is_empty());
        let output = Path::new(&stats.output_path);
        assert_eq!(fs::read(output.join("res/atlas.png")).unwrap(), png);
        let sprite = fs::read(output.join("Unpacked_PNG/res/atlas/sprite.png")).unwrap();
        assert_eq!(atlas::decode_png(&sprite).unwrap().pixels, rgba);
    }

    #[test]
    fn corrupt_zip_png_rolls_back_instead_of_becoming_a_decode_warning() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("corrupt.apk");
        let mut bytes = make_zip(&[("assets/bad.png", b"czzf damaged wrapper")]);
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
        let data_start = archive.by_index(0).unwrap().data_start() as usize;
        drop(archive);
        bytes[data_start + 8] ^= 1;
        fs::write(&input, bytes).unwrap();
        let parent = root.path().join("out");
        fs::create_dir_all(parent.join("corrupt")).unwrap();
        fs::write(parent.join("corrupt/previous"), b"keep").unwrap();
        assert!(
            unpack(
                &input,
                &parent,
                Game::Carrot4,
                true,
                1,
                &AtomicBool::new(false),
                &|_| {},
            )
            .is_err()
        );
        assert_eq!(fs::read(parent.join("corrupt/previous")).unwrap(), b"keep");
        assert_eq!(fs::read_dir(parent).unwrap().count(), 1);
    }
    #[test]
    fn reject_windows_path_aliases_and_traversal() {
        for path in [
            "../evil",
            "a/../../evil",
            "C:/evil",
            "//server/x",
            "a\\..\\evil",
            "x:stream",
            "NUL.png",
            "a//b",
        ] {
            assert!(safe_relative(path).is_err(), "{path}");
        }
        assert_eq!(
            safe_relative("Themes/关卡/test.png").unwrap(),
            PathBuf::from("Themes/关卡/test.png")
        );
        assert_eq!(
            safe_relative("res/a<>\"|?*/tail. ").unwrap(),
            PathBuf::from("res/a%3C%3E%22%7C%3F%2A/tail%2E%20")
        );
        assert_eq!(safe_relative("a./x").unwrap(), PathBuf::from("a%2E/x"));
    }

    #[cfg(windows)]
    #[test]
    fn protects_input_inside_output_with_different_casing() {
        let root = tempfile::tempdir().unwrap();
        let original_output = root.path().join("Game");
        fs::create_dir(&original_output).unwrap();
        let apk = original_output.join("game.apk");
        fs::write(&apk, b"input must survive").unwrap();
        let error = unpack(
            &apk,
            root.path(),
            Game::Carrot1,
            true,
            1,
            &AtomicBool::new(false),
            &|_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("输入 APK 不能位于"), "{error:#}");
        assert_eq!(fs::read(&apk).unwrap(), b"input must survive");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
