//! APK / APKS / XAPK inputs with independent, bounded readers for each worker.

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, ensure};
use tempfile::TempDir;
use zip::{CompressionMethod, ZipArchive};

pub fn trim_numeric_suffix(path: &Path) -> PathBuf {
    match path.extension().and_then(|value| value.to_str()) {
        Some(extension)
            if !extension.is_empty() && extension.bytes().all(|b| b.is_ascii_digit()) =>
        {
            path.with_extension("")
        }
        _ => path.to_owned(),
    }
}

pub fn is_supported_input(path: &Path) -> bool {
    let path = trim_numeric_suffix(path);
    path.extension().is_some_and(|extension| {
        ["apk", "apks", "xapk"]
            .iter()
            .any(|expected| extension.eq_ignore_ascii_case(expected))
    })
}

fn is_bundle(path: &Path) -> bool {
    trim_numeric_suffix(path)
        .extension()
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("apks") || extension.eq_ignore_ascii_case("xapk")
        })
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "已取消");
    Ok(())
}

/// A read-only window whose offsets are relative to its own ZIP, including seeks.
#[derive(Debug)]
pub struct FileWindow {
    file: File,
    offset: u64,
    length: u64,
    position: u64,
}

impl FileWindow {
    fn open(path: &Path, offset: u64, length: u64) -> Result<Self> {
        let mut file = File::open(path)?;
        let file_length = file.metadata()?.len();
        ensure!(
            offset
                .checked_add(length)
                .is_some_and(|end| end <= file_length),
            "内层 APK 窗口越界"
        );
        file.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            file,
            offset,
            length,
            position: 0,
        })
    }
}

impl Read for FileWindow {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.length.saturating_sub(self.position);
        let count = remaining.min(buffer.len() as u64) as usize;
        if count == 0 {
            return Ok(0);
        }
        let read = self.file.read(&mut buffer[..count])?;
        self.position += read as u64;
        Ok(read)
    }
}

impl Seek for FileWindow {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.length) + i128::from(delta),
        };
        let position: u64 = position
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "APK 窗口定位越界"))?;
        let absolute = self
            .offset
            .checked_add(position)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "APK 窗口定位溢出"))?;
        self.file.seek(SeekFrom::Start(absolute))?;
        self.position = position;
        Ok(position)
    }
}

#[derive(Debug)]
struct Source {
    path: PathBuf,
    offset: u64,
    length: u64,
}

/// Owns fallback temporary APKs until all extraction/detection workers have ended.
#[derive(Debug)]
pub struct ApkBundle {
    sources: Vec<Source>,
    _temporary: Option<TempDir>,
}

impl ApkBundle {
    pub fn resolve(input: &Path, cancel: &AtomicBool) -> Result<Self> {
        Self::resolve_in(input, cancel, None)
    }

    fn resolve_in(
        input: &Path,
        cancel: &AtomicBool,
        temporary_parent: Option<&Path>,
    ) -> Result<Self> {
        check_cancel(cancel)?;
        ensure!(
            is_supported_input(input) && input.is_file(),
            "请选择存在的 .apk / .apks / .xapk 文件（支持 .apk.1 等数字后缀）"
        );
        if !is_bundle(input) {
            return Ok(Self {
                sources: vec![Source {
                    path: input.to_owned(),
                    offset: 0,
                    length: input.metadata()?.len(),
                }],
                _temporary: None,
            });
        }
        let mut outer = ZipArchive::new(File::open(input)?).context("解析 APK 分包失败")?;
        let mut indexes = Vec::new();
        for index in 0..outer.len() {
            check_cancel(cancel)?;
            let entry = outer.by_index(index)?;
            let name = entry.name().replace('\\', "/");
            let file_name = name.rsplit('/').next().unwrap_or("");
            if !entry.is_dir()
                && file_name.to_ascii_lowercase().ends_with(".apk")
                && !file_name.to_ascii_lowercase().starts_with("split_config.")
            {
                indexes.push((index, !file_name.eq_ignore_ascii_case("base.apk"), name));
            }
        }
        indexes.sort_by(|left, right| (left.1, &left.2).cmp(&(right.1, &right.2)));
        ensure!(!indexes.is_empty(), "分包中没有找到任何 APK。");
        let mut temporary: Option<TempDir> = None;
        let mut sources = Vec::with_capacity(indexes.len());
        let mut buffer = vec![0u8; 256 * 1024];
        for (index, _, name) in indexes {
            check_cancel(cancel)?;
            let mut entry = outer.by_index(index)?;
            ensure!(!entry.encrypted(), "分包中的 APK 使用 ZIP 密码加密：{name}");
            ensure!(
                entry.size() <= 64 * 1024 * 1024 * 1024,
                "分包中的 APK 超过 64 GiB：{name}"
            );
            let source = if entry.compression() == CompressionMethod::Stored
                && entry.compressed_size() == entry.size()
            {
                // data_start handles ZIP64/local extra fields; no copy of the inner APK.
                Source {
                    path: input.to_owned(),
                    offset: entry.data_start(),
                    length: entry.size(),
                }
            } else {
                let directory = match temporary.as_ref() {
                    Some(directory) => directory,
                    None => {
                        let mut builder = tempfile::Builder::new();
                        builder.prefix(".cfunpacker-bundle-");
                        temporary.insert(match temporary_parent {
                            Some(parent) => builder.tempdir_in(parent)?,
                            None => builder.tempdir()?,
                        })
                    }
                };
                // Never use a ZIP-controlled path for the fallback file name.
                let path = directory.path().join(format!("{index}.apk"));
                let mut output = File::create(&path)?;
                let mut copied = 0u64;
                loop {
                    check_cancel(cancel)?;
                    let read = entry.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    copied = copied
                        .checked_add(read as u64)
                        .context("内层 APK 长度溢出")?;
                    ensure!(
                        copied <= entry.size(),
                        "内层 APK 解压长度超出声明值：{name}"
                    );
                    output.write_all(&buffer[..read])?;
                }
                ensure!(copied == entry.size(), "内层 APK 解压长度不一致：{name}");
                Source {
                    path,
                    offset: 0,
                    length: copied,
                }
            };
            // Verify each window before allowing worker extraction to begin.
            ZipArchive::new(FileWindow::open(
                &source.path,
                source.offset,
                source.length,
            )?)
            .with_context(|| format!("内层 APK 无效：{name}"))?;
            sources.push(source);
        }
        check_cancel(cancel)?;
        Ok(Self {
            sources,
            _temporary: temporary,
        })
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    pub fn open_archive(&self, index: usize) -> Result<ZipArchive<FileWindow>> {
        let source = self.sources.get(index).context("APK 分包索引无效")?;
        Ok(ZipArchive::new(FileWindow::open(
            &source.path,
            source.offset,
            source.length,
        )?)?)
    }

    pub fn open_archives(&self) -> Result<Vec<ZipArchive<FileWindow>>> {
        (0..self.len())
            .map(|index| self.open_archive(index))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::{ZipWriter, write::SimpleFileOptions};

    fn apk(contents: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::new(io::Cursor::new(Vec::new()));
        for (name, data) in contents {
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
    fn stored_windows_and_compressed_fallback_skip_configuration_splits() {
        for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
            let root = tempfile::tempdir().unwrap();
            let input = root.path().join("game.xapk.2");
            let mut outer = ZipWriter::new(File::create(&input).unwrap());
            for (name, payload) in [
                ("assets.apk", apk(&[("assets/extra.dat", b"split")])),
                ("base.apk", apk(&[("assets/base.dat", b"base")])),
                ("split_config.arm64_v8a.apk", vec![0; 7]),
            ] {
                outer
                    .start_file(
                        name,
                        SimpleFileOptions::default().compression_method(method),
                    )
                    .unwrap();
                outer.write_all(&payload).unwrap();
            }
            outer.finish().unwrap();
            let bundle = ApkBundle::resolve(&input, &AtomicBool::new(false)).unwrap();
            assert_eq!(bundle.len(), 2);
            assert_eq!(
                bundle.open_archive(0).unwrap().by_index(0).unwrap().name(),
                "assets/base.dat"
            );
            let temporary = bundle
                ._temporary
                .as_ref()
                .map(|temp| temp.path().to_owned());
            assert_eq!(temporary.is_some(), method != CompressionMethod::Stored);
            if method == CompressionMethod::Stored {
                assert!(
                    bundle
                        .sources
                        .iter()
                        .all(|source| source.path == input && source.offset > 0)
                );
            }
            drop(bundle);
            assert!(temporary.is_none_or(|path| !path.exists()));
        }
    }

    #[test]
    fn supported_extensions_and_numeric_suffixes() {
        for name in ["x.apk", "x.APK.1", "x.apks.12", "x.xapk"] {
            assert!(is_supported_input(Path::new(name)), "{name}");
        }
        for name in ["x.apk.txt", "x.zip", "x.apk.1.2"] {
            assert!(!is_supported_input(Path::new(name)), "{name}");
        }
        assert_eq!(
            trim_numeric_suffix(Path::new("game.apk.2")),
            PathBuf::from("game.apk")
        );
    }

    #[test]
    fn fallback_files_are_cleaned_if_a_later_inner_apk_is_invalid() {
        let root = tempfile::tempdir().unwrap();
        let temporary_parent = root.path().join("temporary");
        std::fs::create_dir(&temporary_parent).unwrap();
        let input = root.path().join("broken.apks");
        let mut outer = ZipWriter::new(File::create(&input).unwrap());
        for (name, data) in [
            ("base.apk", apk(&[("assets/base", b"valid")])),
            ("later.apk", b"broken ZIP".to_vec()),
        ] {
            outer
                .start_file(
                    name,
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
                )
                .unwrap();
            outer.write_all(&data).unwrap();
        }
        outer.finish().unwrap();
        assert!(
            ApkBundle::resolve_in(&input, &AtomicBool::new(false), Some(&temporary_parent))
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&temporary_parent).unwrap().count(), 0);
        assert!(
            ApkBundle::resolve_in(&input, &AtomicBool::new(true), Some(&temporary_parent)).is_err()
        );
        assert_eq!(std::fs::read_dir(&temporary_parent).unwrap().count(), 0);
    }

    #[test]
    fn file_window_limits_reads_and_relative_seeks() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("data");
        std::fs::write(&path, b"beforePAYLOADafter").unwrap();
        let mut window = FileWindow::open(&path, 6, 7).unwrap();
        let mut bytes = Vec::new();
        window.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"PAYLOAD");
        assert_eq!(window.seek(SeekFrom::End(-2)).unwrap(), 5);
        let mut tail = [0; 4];
        assert_eq!(window.read(&mut tail).unwrap(), 2);
        assert_eq!(&tail[..2], b"AD");
        assert!(window.seek(SeekFrom::Current(-100)).is_err());
        assert!(FileWindow::open(&path, 6, u64::MAX).is_err());
    }
}
