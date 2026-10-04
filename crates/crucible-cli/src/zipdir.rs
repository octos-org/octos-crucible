//! Zip a directory tree written by untrusted code, and unzip untrusted
//! archives, without ever following a link out of the tree.
//!
//! Writing: symlinks and special files are dropped (counted), entries are
//! sorted and get a fixed timestamp, so the same tree gives the same bytes.
//! Reading: absolute paths, `..`, backslashes, symlinks, too many files and
//! too many (actually read) bytes are refused before anything escapes.

use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZipStats {
    pub files: usize,
    pub bytes: u64,
    pub dropped_links: usize,
}

/// A file to add: path inside the zip and source on disk.
pub struct Entry {
    pub name: String,
    pub source: PathBuf,
    pub executable: bool,
}

/// Every regular file under `root/rel` (a file or a directory), named
/// relative to `root`. Symlinks are never followed; `skip` sees each
/// relative path (with `/` separators) and may prune it.
pub fn collect(
    root: &Path,
    rel: &str,
    skip: &dyn Fn(&str) -> bool,
    out: &mut Vec<Entry>,
    stats: &mut ZipStats,
) -> Result<()> {
    let path = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let ft = meta.file_type();
    if ft.is_symlink() {
        stats.dropped_links += 1;
        return Ok(());
    }
    if !rel.is_empty() && skip(rel) {
        return Ok(());
    }
    if ft.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(&path)
            .with_context(|| format!("listing {}", path.display()))?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        for n in names {
            let child = if rel.is_empty() {
                n
            } else {
                format!("{rel}/{n}")
            };
            collect(root, &child, skip, out, stats)?;
        }
    } else if ft.is_file() {
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        out.push(Entry {
            name: rel.to_owned(),
            source: path,
            executable,
        });
        stats.files += 1;
        stats.bytes += meta.len();
    }
    Ok(())
}

pub fn options(executable: bool) -> SimpleFileOptions {
    SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(DateTime::default())
        .unix_permissions(if executable { 0o755 } else { 0o644 })
        .large_file(false)
}

/// Write `entries` (plus in-memory `extra` files) as a zip.
pub fn write_zip<W: Write + Seek>(out: W, entries: &[Entry], extra: &[(&str, &[u8])]) -> Result<W> {
    let mut zip = ZipWriter::new(out);
    type Item<'a> = (&'a str, Option<&'a Entry>, Option<&'a [u8]>);
    let mut all: Vec<Item> = entries
        .iter()
        .map(|e| (e.name.as_str(), Some(e), None))
        .chain(extra.iter().map(|(n, d)| (*n, None, Some(*d))))
        .collect();
    all.sort_by(|a, b| a.0.cmp(b.0));
    for (name, entry, data) in all {
        match (entry, data) {
            (Some(e), _) => {
                let big = std::fs::metadata(&e.source)?.len() >= u32::MAX as u64;
                zip.start_file(name, options(e.executable).large_file(big))?;
                let mut f = File::open(&e.source)
                    .with_context(|| format!("reading {}", e.source.display()))?;
                std::io::copy(&mut f, &mut zip)?;
            }
            (None, Some(d)) => {
                zip.start_file(name, options(false))?;
                zip.write_all(d)?;
            }
            _ => unreachable!(),
        }
    }
    Ok(zip.finish()?)
}

#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
    pub max_files: usize,
    pub max_bytes: u64,
}

fn safe_name(name: &str) -> Option<PathBuf> {
    if name.is_empty()
        || name.contains('\\')
        || name.contains('\0')
        || name.starts_with('/')
        || name.len() > 4096
    {
        return None;
    }
    let p = Path::new(name);
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(p.to_path_buf())
}

/// Check a zip's names, entry types and declared sizes without extracting
/// it; returns the names of its files (not directories).
pub fn check_names<R: Read + Seek>(reader: R, limits: ExtractLimits) -> Result<Vec<String>> {
    let mut archive = ZipArchive::new(reader).context("not a zip file")?;
    check_archive(&mut archive, limits)
}

fn check_archive<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    limits: ExtractLimits,
) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let mut declared = 0u64;
    for i in 0..archive.len() {
        let f = archive.by_index_raw(i)?;
        let name = f.name().to_owned();
        if safe_name(&name).is_none() {
            bail!("unsafe path in zip: {name:?}");
        }
        if f.is_symlink() || f.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000) {
            bail!("symlink not allowed in zip: {name:?}");
        }
        if !f.is_dir() {
            declared = declared.saturating_add(f.size());
            names.push(name);
        }
        if names.len() > limits.max_files {
            bail!("zip has more than {} files", limits.max_files);
        }
        if declared > limits.max_bytes {
            bail!("zip expands to more than {} bytes", limits.max_bytes);
        }
    }
    Ok(names)
}

/// Check a zip and extract it into `dest` (created if missing).
pub fn safe_extract<R: Read + Seek>(
    reader: R,
    dest: &Path,
    limits: ExtractLimits,
) -> Result<usize> {
    let mut archive = ZipArchive::new(reader).context("not a zip file")?;
    // Pass 1: names, types and declared sizes, before writing anything.
    let files = check_archive(&mut archive, limits)?.len();
    std::fs::create_dir_all(dest)?;
    let mut written = 0u64;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i)?;
        let rel = safe_name(f.name()).expect("checked above");
        let target = dest.join(&rel);
        if f.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A directory component created above cannot be a link (we never
        // create links), but refuse to write through anything already there.
        if std::fs::symlink_metadata(&target).is_ok() {
            bail!("duplicate entry in zip: {:?}", f.name());
        }
        let exec = f.unix_mode().is_some_and(|m| m & 0o111 != 0);
        let mut out = File::create(&target)?;
        // Never trust the declared size: cap what is actually inflated.
        let budget = limits.max_bytes - written;
        let n = std::io::copy(&mut (&mut f).take(budget + 1), &mut out)?;
        written += n;
        if n > budget {
            bail!("zip expands to more than {} bytes", limits.max_bytes);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if exec { 0o755 } else { 0o644 };
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = exec;
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const LIMITS: ExtractLimits = ExtractLimits {
        max_files: 100,
        max_bytes: 1 << 20,
    };

    fn raw_zip(entries: &[(&str, &[u8], Option<u32>)]) -> Vec<u8> {
        let mut z = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data, mode) in entries {
            let mut o = SimpleFileOptions::default();
            if let Some(m) = mode {
                o = o.unix_permissions(*m);
            }
            if mode.is_some_and(|m| m & 0o170000 == 0o120000) {
                z.add_symlink(*name, std::str::from_utf8(data).unwrap(), o)
                    .unwrap();
            } else {
                z.start_file(*name, o).unwrap();
                z.write_all(data).unwrap();
            }
        }
        z.finish().unwrap().into_inner()
    }

    #[test]
    fn round_trip_drops_links_and_is_deterministic() {
        let src = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("a/b")).unwrap();
        std::fs::write(src.path().join("a/b/x.txt"), "x").unwrap();
        std::fs::write(src.path().join("top.txt"), "t").unwrap();
        std::fs::create_dir_all(src.path().join("skipme")).unwrap();
        std::fs::write(src.path().join("skipme/y"), "y").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", src.path().join("a/link")).unwrap();
        let zipit = || {
            let mut entries = Vec::new();
            let mut stats = ZipStats::default();
            collect(src.path(), "", &|r| r == "skipme", &mut entries, &mut stats).unwrap();
            (
                write_zip(
                    Cursor::new(Vec::new()),
                    &entries,
                    &[("Dockerfile", b"FROM x")],
                )
                .unwrap()
                .into_inner(),
                stats,
            )
        };
        let (z1, stats) = zipit();
        let (z2, _) = zipit();
        assert_eq!(z1, z2);
        assert_eq!(stats.files, 2);
        #[cfg(unix)]
        assert_eq!(stats.dropped_links, 1);
        let dest = tempfile::tempdir().unwrap();
        assert_eq!(
            safe_extract(Cursor::new(&z1), dest.path(), LIMITS).unwrap(),
            3
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("a/b/x.txt")).unwrap(),
            "x"
        );
        assert!(dest.path().join("Dockerfile").is_file());
        assert!(!dest.path().join("a/link").exists());
        assert!(!dest.path().join("skipme").exists());
    }

    #[test]
    fn refuses_unsafe_archives() {
        for (name, z) in [
            ("traversal", raw_zip(&[("../evil", b"x", None)])),
            ("absolute", raw_zip(&[("/etc/evil", b"x", None)])),
            ("backslash", raw_zip(&[("a\\..\\evil", b"x", None)])),
            (
                "symlink",
                raw_zip(&[("link", b"/etc/passwd", Some(0o120777))]),
            ),
        ] {
            let dest = tempfile::tempdir().unwrap();
            assert!(
                safe_extract(Cursor::new(&z), dest.path(), LIMITS).is_err(),
                "{name}"
            );
            assert_eq!(std::fs::read_dir(dest.path()).unwrap().count(), 0, "{name}");
        }
        let many: Vec<(String, &[u8], Option<u32>)> =
            (0..5).map(|i| (format!("f{i}"), &b"x"[..], None)).collect();
        let many: Vec<(&str, &[u8], Option<u32>)> =
            many.iter().map(|(n, d, m)| (n.as_str(), *d, *m)).collect();
        let z = raw_zip(&many);
        let dest = tempfile::tempdir().unwrap();
        let small = ExtractLimits {
            max_files: 3,
            max_bytes: 1 << 20,
        };
        assert!(safe_extract(Cursor::new(&z), dest.path(), small).is_err());
        let big = vec![0u8; 10_000];
        let z = raw_zip(&[("big", &big, None)]);
        let tiny = ExtractLimits {
            max_files: 10,
            max_bytes: 1000,
        };
        assert!(safe_extract(Cursor::new(&z), dest.path(), tiny).is_err());
    }
}
