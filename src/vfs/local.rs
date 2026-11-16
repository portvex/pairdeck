//! Local disk backend.

use super::{Entry, EntryKind, Vfs};
use anyhow::{Context, Result};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub struct LocalFs;

pub fn entry_from_meta(name: String, path: &Path, lmeta: &fs::Metadata) -> Entry {
    let ft = lmeta.file_type();
    let mut size = lmeta.len();
    let mut mode = lmeta.mode();
    let kind = if ft.is_symlink() {
        let target = fs::read_link(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        match fs::metadata(path) {
            Ok(m) => {
                size = m.len();
                mode = (lmeta.mode() & 0o170000) | (m.mode() & 0o7777);
                EntryKind::Symlink { target, to_dir: m.is_dir(), broken: false }
            }
            Err(_) => EntryKind::Symlink { target, to_dir: false, broken: true },
        }
    } else if ft.is_dir() {
        EntryKind::Dir
    } else if ft.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    Entry {
        name,
        kind,
        size,
        mtime: lmeta.modified().ok(),
        atime: lmeta.accessed().ok(),
        ctime: SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(lmeta.ctime().max(0) as u64)),
        mode,
        uid: lmeta.uid(),
        gid: lmeta.gid(),
        nlink: lmeta.nlink(),
        inode: lmeta.ino(),
        dev: lmeta.dev(),
        encrypted: false,
    }
}

pub fn stat_local(path: &Path) -> Result<Entry> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
    Ok(entry_from_meta(name, path, &meta))
}

impl Vfs for LocalFs {
    fn scheme(&self) -> &'static str {
        "local"
    }
    fn display(&self, path: &Path) -> String {
        path.display().to_string()
    }
    fn is_local(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
    }

    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>> {
        let rd = fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?;
        let mut out = Vec::new();
        for de in rd.flatten() {
            let path = de.path();
            let name = de.file_name().to_string_lossy().into_owned();
            if let Ok(meta) = fs::symlink_metadata(&path) {
                out.push(entry_from_meta(name, &path, &meta));
            }
        }
        Ok(out)
    }

    fn stat(&mut self, path: &Path) -> Result<Entry> {
        stat_local(path)
    }

    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(fs::File::open(path).with_context(|| format!("open {}", path.display()))?))
    }

    fn create_write(&mut self, path: &Path, mode: u32) -> Result<Box<dyn Write + Send>> {
        use std::os::unix::fs::OpenOptionsExt;
        let f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(if mode == 0 { 0o644 } else { mode & 0o7777 })
            .open(path)
            .with_context(|| format!("create {}", path.display()))?;
        Ok(Box::new(f))
    }

    fn mkdir(&mut self, path: &Path, mode: u32) -> Result<()> {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(if mode == 0 { 0o755 } else { mode & 0o7777 })
            .create(path)
            .with_context(|| format!("mkdir {}", path.display()))
    }
    fn remove_file(&mut self, path: &Path) -> Result<()> {
        fs::remove_file(path).with_context(|| format!("delete {}", path.display()))
    }
    fn remove_dir(&mut self, path: &Path) -> Result<()> {
        fs::remove_dir(path).with_context(|| format!("rmdir {}", path.display()))
    }
    fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        fs::rename(from, to).with_context(|| format!("rename {} -> {}", from.display(), to.display()))
    }
    fn set_mode(&mut self, path: &Path, mode: u32) -> Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
            .with_context(|| format!("chmod {}", path.display()))
    }
    fn symlink(&mut self, target: &str, link: &Path) -> Result<()> {
        std::os::unix::fs::symlink(target, link).with_context(|| format!("symlink {}", link.display()))
    }
    fn set_mtime(&mut self, path: &Path, mtime: SystemTime) -> Result<()> {
        let f = fs::File::options().write(true).open(path)?;
        f.set_modified(mtime)?;
        Ok(())
    }
    fn local_backing(&self) -> Option<PathBuf> {
        None
    }
}
