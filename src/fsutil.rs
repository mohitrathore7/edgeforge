//! Crash-safe filesystem helpers: atomic writes, atomic symlink swaps and a
//! tar unpacker that refuses anything that could escape its destination.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::error::{EdgeError, Result};

/// Write `data` to `path` so that readers see either the old or the new
/// content, never a partial file — even across power loss.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| EdgeError::Invalid(format!("{} has no parent", path.display())))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    fsync_dir(dir);
    Ok(())
}

/// Point `link` at `target` atomically: build `link.tmp` then rename over `link`.
/// `rename(2)` replaces the old symlink in a single step.
pub fn symlink_atomic(target: &Path, link: &Path) -> Result<()> {
    let tmp = tmp_link_path(link);
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp)?;
    fs::rename(&tmp, link)?;
    if let Some(dir) = link.parent() {
        fsync_dir(dir);
    }
    Ok(())
}

pub fn tmp_link_path(link: &Path) -> PathBuf {
    let mut name = link.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    link.with_file_name(name)
}

pub fn fsync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Remove and recreate a directory.
pub fn reset_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir)?;
    }
    fs::create_dir_all(dir)?;
    Ok(())
}

/// Turn an archive path into a safe relative path, rejecting absolute paths
/// and `..` components.
pub fn sanitize_entry_path(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(EdgeError::Verify(format!(
                    "unsafe path in archive: {}",
                    path.display()
                )))
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(EdgeError::Verify("empty path in archive".into()));
    }
    Ok(out)
}

/// Unpack a tar stream into `dest`. Only regular files and directories are
/// accepted; symlinks, hard links and device nodes are rejected because they
/// can be used to write outside the destination. Returns the number of files.
pub fn safe_unpack<R: Read>(reader: R, dest: &Path) -> Result<u64> {
    let bad = |e: io::Error| EdgeError::Verify(format!("malformed archive: {e}"));
    fs::create_dir_all(dest)?;
    let mut archive = tar::Archive::new(reader);
    let mut files = 0;
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let raw = entry.path().map_err(bad)?.into_owned();
        let rel = sanitize_entry_path(&raw)?;
        let kind = entry.header().entry_type();
        let target = dest.join(&rel);
        if kind.is_dir() {
            fs::create_dir_all(&target)?;
        } else if kind.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut out = File::create(&target)?;
            io::copy(&mut entry, &mut out).map_err(bad)?;
            let mode = entry.header().mode().unwrap_or(0o644) & 0o755;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode | 0o400))?;
            files += 1;
        } else {
            return Err(EdgeError::Verify(format!(
                "unsupported archive entry {:?} at {}",
                kind,
                raw.display()
            )));
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_escapes() {
        assert!(sanitize_entry_path(Path::new("../x")).is_err());
        assert!(sanitize_entry_path(Path::new("/etc/passwd")).is_err());
        assert!(sanitize_entry_path(Path::new("a/../../b")).is_err());
        assert_eq!(
            sanitize_entry_path(Path::new("./bin/run.sh")).unwrap(),
            PathBuf::from("bin/run.sh")
        );
    }

    #[test]
    fn atomic_symlink_replaces_existing_link() {
        let t = tempfile::tempdir().unwrap();
        let link = t.path().join("current");
        symlink_atomic(Path::new("slots/a"), &link).unwrap();
        symlink_atomic(Path::new("slots/b"), &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("slots/b"));
        assert!(!tmp_link_path(&link).exists());
    }

    #[test]
    fn write_atomic_leaves_no_temp_files() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("state.json");
        write_atomic(&p, b"{}").unwrap();
        write_atomic(&p, b"{\"v\":2}").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"v\":2}");
        assert_eq!(fs::read_dir(t.path()).unwrap().count(), 1);
    }
}
