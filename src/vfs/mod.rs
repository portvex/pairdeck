//! Virtual file system layer. Every panel shows a directory inside some `Vfs`:
//! the local disk, an archive, or a remote SFTP host.

pub mod archive;
pub mod connect;
pub mod dmg;
pub mod http;
pub mod iso;
pub mod local;
pub mod s3;
pub mod secrets;
pub mod sftp;
pub mod webdav;

use anyhow::{Result, bail};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink {
        target: String,
        to_dir: bool,
        broken: bool,
    },
    /// Device, fifo, socket...
    Other,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub atime: Option<SystemTime>,
    pub ctime: Option<SystemTime>,
    /// Full st_mode (type + permission bits) when known, else permission bits only.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u64,
    pub inode: u64,
    pub dev: u64,
    /// Content is encrypted (password-protected archive entry).
    pub encrypted: bool,
}

impl Entry {
    pub fn new(name: impl Into<String>, kind: EntryKind) -> Entry {
        Entry {
            name: name.into(),
            kind,
            size: 0,
            mtime: None,
            atime: None,
            ctime: None,
            mode: 0,
            uid: 0,
            gid: 0,
            nlink: 1,
            inode: 0,
            dev: 0,
            encrypted: false,
        }
    }

    pub fn parent() -> Entry {
        let mut e = Entry::new("..", EntryKind::Dir);
        e.mode = 0o755;
        e
    }

    pub fn is_parent(&self) -> bool {
        self.name == ".."
    }

    /// True for directories and symlinks pointing to directories.
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::Symlink { to_dir: true, .. })
    }

    pub fn is_symlink(&self) -> bool {
        matches!(self.kind, EntryKind::Symlink { .. })
    }

    pub fn is_exec(&self) -> bool {
        !self.is_dir() && self.mode & 0o111 != 0
    }

    pub fn link_target(&self) -> Option<&str> {
        match &self.kind {
            EntryKind::Symlink { target, .. } => Some(target),
            _ => None,
        }
    }

    /// `drwxr-xr-x` style string.
    pub fn perm_string(&self) -> String {
        let t = match &self.kind {
            EntryKind::Dir => 'd',
            EntryKind::Symlink { .. } => 'l',
            EntryKind::Other => match self.mode & 0o170000 {
                0o020000 => 'c',
                0o060000 => 'b',
                0o010000 => 'p',
                0o140000 => 's',
                _ => '?',
            },
            EntryKind::File => '-',
        };
        let m = self.mode;
        let mut s = String::with_capacity(10);
        s.push(t);
        let bits = [
            (0o400, 'r'),
            (0o200, 'w'),
            (0o100, 'x'),
            (0o040, 'r'),
            (0o020, 'w'),
            (0o010, 'x'),
            (0o004, 'r'),
            (0o002, 'w'),
            (0o001, 'x'),
        ];
        for (i, (b, c)) in bits.iter().enumerate() {
            let mut ch = if m & b != 0 { *c } else { '-' };
            if i == 2 && m & 0o4000 != 0 {
                ch = if m & 0o100 != 0 { 's' } else { 'S' };
            }
            if i == 5 && m & 0o2000 != 0 {
                ch = if m & 0o010 != 0 { 's' } else { 'S' };
            }
            if i == 8 && m & 0o1000 != 0 {
                ch = if m & 0o001 != 0 { 't' } else { 'T' };
            }
            s.push(ch);
        }
        s
    }
}

/// A file system backend. Paths passed in are absolute *within* the VFS (rooted at "/").
/// All methods take `&mut self`; callers hold the handle's mutex only for a single call.
#[allow(dead_code)]
pub trait Vfs: Send {
    /// Short scheme name: "local", "zip", "tar", "7z", "sftp".
    fn scheme(&self) -> &'static str;

    /// Human readable location used in panel titles and history, e.g.
    /// `/home/u`, `/tmp/a.zip#/docs`, `sftp://user@host/var/log`.
    fn display(&self, path: &Path) -> String;

    fn is_local(&self) -> bool {
        false
    }

    fn writable(&self) -> bool {
        false
    }

    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>>;

    fn stat(&mut self, path: &Path) -> Result<Entry>;

    /// Open a file for streaming reads. The returned reader must not borrow `self`.
    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>>;

    /// Read up to `max` bytes from the start of a file (previews, viewer).
    fn read_head(&mut self, path: &Path, max: usize) -> Result<Vec<u8>> {
        let mut r = self.open_read(path)?;
        let mut buf = Vec::new();
        r.by_ref().take(max as u64).read_to_end(&mut buf)?;
        Ok(buf)
    }

    fn create_write(&mut self, _path: &Path, _mode: u32) -> Result<Box<dyn Write + Send>> {
        bail!("{}: file system is read-only", self.scheme())
    }
    fn mkdir(&mut self, _path: &Path, _mode: u32) -> Result<()> {
        bail!("{}: file system is read-only", self.scheme())
    }
    fn remove_file(&mut self, _path: &Path) -> Result<()> {
        bail!("{}: file system is read-only", self.scheme())
    }
    fn remove_dir(&mut self, _path: &Path) -> Result<()> {
        bail!("{}: file system is read-only", self.scheme())
    }
    fn rename(&mut self, _from: &Path, _to: &Path) -> Result<()> {
        bail!("{}: rename not supported", self.scheme())
    }
    fn set_mode(&mut self, _path: &Path, _mode: u32) -> Result<()> {
        bail!("{}: chmod not supported", self.scheme())
    }
    fn symlink(&mut self, _target: &str, _link: &Path) -> Result<()> {
        bail!("{}: symlinks not supported", self.scheme())
    }
    fn set_mtime(&mut self, _path: &Path, _mtime: SystemTime) -> Result<()> {
        Ok(())
    }

    /// Persist buffered changes (archives rewrite themselves here). Called at the
    /// end of every file operation job; a no-op for live file systems.
    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    /// For archives opened from the local disk: the archive file path. Used to
    /// return to the containing directory when leaving the archive root.
    fn local_backing(&self) -> Option<PathBuf> {
        None
    }

    /// True when reading any of `paths` (or anything below them) needs a password
    /// that has not been supplied yet (encrypted archives).
    fn needs_password(&self, _paths: &[PathBuf]) -> bool {
        false
    }

    /// Supply a password. Returns Ok(true) if it was verified, Ok(false) if wrong.
    fn set_password(&mut self, _password: &str) -> Result<bool> {
        bail!("{}: no password needed", self.scheme())
    }

    /// Identifier of a mounted disk image backing this VFS (see dmg.rs).
    fn mount_id(&self) -> Option<u64> {
        None
    }
}

#[derive(Clone)]
pub struct VfsHandle(pub Arc<Mutex<Box<dyn Vfs>>>, bool);

impl std::fmt::Debug for VfsHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VfsHandle(local={})", self.1)
    }
}

impl VfsHandle {
    pub fn new(v: Box<dyn Vfs>) -> Self {
        // Locality never changes for a backend; cache it so rendering and background
        // listings never wait on the mutex just to ask.
        let local = v.is_local();
        VfsHandle(Arc::new(Mutex::new(v)), local)
    }
    pub fn local() -> Self {
        Self::new(Box::new(local::LocalFs))
    }
    pub fn lock(&self) -> MutexGuard<'_, Box<dyn Vfs>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn same(&self, other: &VfsHandle) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub fn is_local(&self) -> bool {
        self.1
    }
}

/// A directory inside a VFS.
#[derive(Clone)]
pub struct Location {
    pub vfs: VfsHandle,
    pub path: PathBuf,
}

impl Location {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Location { vfs: VfsHandle::local(), path: path.into() }
    }
    pub fn is_local(&self) -> bool {
        self.vfs.is_local()
    }
    pub fn display(&self) -> String {
        self.vfs.lock().display(&self.path)
    }
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
    pub fn with_path(&self, path: impl Into<PathBuf>) -> Self {
        Location { vfs: self.vfs.clone(), path: path.into() }
    }
    /// Local path if this location is on the local disk.
    pub fn local_path(&self) -> Option<&Path> {
        if self.is_local() { Some(&self.path) } else { None }
    }
}

/// Returns true if a file looks like an archive we can browse.
pub fn is_browsable_archive(name: &str) -> bool {
    archive::supports(name)
}

/// Normalize `a/b/../c` style paths without touching the file system.
pub fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push("/");
    }
    out
}

/// Parse a user-typed location such as `sftp://user@host:22/path`, `~/x` or `/abs`.
pub fn parse_location(input: &str, base: &Location) -> Result<Location> {
    let input = input.trim();
    if input.starts_with("sftp://") || input.starts_with("ssh://") {
        return sftp::connect_url(input);
    }
    if input.starts_with("s3://") {
        return s3::connect_url(input);
    }
    if webdav::is_dav_url(input) {
        return webdav::connect_url(input);
    }
    let expanded = crate::util::expand_tilde(input);
    let p = Path::new(&expanded);
    let path = if p.is_absolute() { p.to_path_buf() } else { base.path.join(p) };
    Ok(base.with_path(normalize(&path)))
}
