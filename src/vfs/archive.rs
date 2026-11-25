//! Archive VFS (zip family, tar family incl. zstd, single-file gz/bz2/xz/zst, 7z)
//! and the "Compress" dialog that builds archives from the selection.
//!
//! Local zip / tar / 7z archives are writable: changes (new files, mkdir, delete,
//! chmod, symlinks) go to an overlay of pending temp files and are committed by
//! rewriting the archive to a temp file next to it and renaming it over the
//! original. Commits happen at the end of each file operation job, on the next
//! listing once no writer is open, and when the archive is closed.

use super::{Entry, EntryKind, Location, Vfs, normalize};
use crate::app::App;
use crate::dialog::Form;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Entries larger than this are extracted to an unlinked temp file instead of memory.
const MEM_LIMIT: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comp {
    None,
    Gz,
    Bz2,
    Xz,
    Zstd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Zip,
    Tar(Comp),
    Single(Comp),
    SevenZ,
}

fn detect(name: &str) -> Option<Kind> {
    let n = name.to_ascii_lowercase();
    let ends = |s: &str| n.ends_with(s);
    Some(if ends(".tar.gz") || ends(".tgz") || ends(".taz") {
        Kind::Tar(Comp::Gz)
    } else if ends(".tar.bz2") || ends(".tbz") || ends(".tbz2") || ends(".tb2") {
        Kind::Tar(Comp::Bz2)
    } else if ends(".tar.xz") || ends(".txz") {
        Kind::Tar(Comp::Xz)
    } else if ends(".tar.zst") || ends(".tzst") {
        Kind::Tar(Comp::Zstd)
    } else if ends(".tar") {
        Kind::Tar(Comp::None)
    } else if ends(".zip")
        || ends(".jar")
        || ends(".war")
        || ends(".ear")
        || ends(".apk")
        || ends(".whl")
        || ends(".epub")
        || ends(".xpi")
        || ends(".ipa")
        || ends(".aar")
        || ends(".nupkg")
    {
        Kind::Zip
    } else if ends(".7z") {
        Kind::SevenZ
    } else if ends(".gz") {
        Kind::Single(Comp::Gz)
    } else if ends(".bz2") {
        Kind::Single(Comp::Bz2)
    } else if ends(".xz") {
        Kind::Single(Comp::Xz)
    } else if ends(".zst") {
        Kind::Single(Comp::Zstd)
    } else {
        return None;
    })
}

pub fn supports(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    detect(name).is_some() || lower.ends_with(".iso") || (super::dmg::is_dmg_name(name) && super::dmg::supported())
}

/// Like [`supports`], but also recognizes `.img` files that are ISO 9660 images
/// (local files only; checks the volume descriptor signature).
pub fn browsable(loc: &Location, name: &str) -> bool {
    if supports(name) {
        return true;
    }
    if !name.to_ascii_lowercase().ends_with(".img") {
        return false;
    }
    let Some(dir) = loc.local_path() else { return false };
    File::open(dir.join(name)).map(|mut f| super::iso::sniff(&mut f)).unwrap_or(false)
}

/// Deletes a temp file when dropped.
struct TempFile(PathBuf);

impl TempFile {
    /// Keep the file (it was renamed into place) without leaking the path, as
    /// `mem::forget` would.
    fn disarm(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

fn temp_path(hint: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let safe: String = hint.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '_' }).collect();
    std::env::temp_dir().join(format!("pairdeck-{}-{n}-{safe}", std::process::id()))
}

/// Read `r` fully into an owned reader: memory for small data, otherwise an
/// already-unlinked temp file (the data disappears when the reader is dropped).
fn owned_reader(mut r: impl Read, size_hint: u64) -> Result<Box<dyn Read + Send>> {
    if size_hint <= MEM_LIMIT {
        let mut buf = Vec::with_capacity(size_hint as usize);
        r.read_to_end(&mut buf)?;
        return Ok(Box::new(Cursor::new(buf)));
    }
    let p = temp_path("extract");
    let mut f = File::options().read(true).write(true).create_new(true).open(&p)?;
    let _ = std::fs::remove_file(&p);
    std::io::copy(&mut r, &mut f)?;
    f.rewind()?;
    Ok(Box::new(f))
}

/// Decoder memory cap for xz. `xz -9` needs about 65 MiB; anything asking for more is
/// treated as hostile (a crafted header can otherwise request a 4 GiB dictionary).
const XZ_MEMLIMIT: u64 = 256 << 20;

/// XZ-format reader (concatenated streams allowed) with a memory limit. The legacy
/// LZMA-alone format is rejected: its header alone can force a multi-GiB allocation.
fn xz_reader<R: Read>(r: R) -> xz2::read::XzDecoder<R> {
    match xz2::stream::Stream::new_stream_decoder(XZ_MEMLIMIT, xz2::stream::CONCATENATED) {
        Ok(stream) => xz2::read::XzDecoder::new_stream(r, stream),
        Err(_) => xz2::read::XzDecoder::new_multi_decoder(r),
    }
}

/// Largest zstd window accepted (2^27 = 128 MiB, the zstd default limit for
/// decoders). Bigger windows in a crafted header would force huge allocations.
const ZSTD_WINDOW_LOG_MAX: u32 = 27;

/// Reader that always fails (used when a decoder cannot even be created).
struct FailReader(Option<std::io::Error>);

impl Read for FailReader {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(self.0.take().unwrap_or_else(|| std::io::Error::other("decoder failed")))
    }
}

fn zstd_reader<'a, R: Read + Send + 'a>(r: R) -> Box<dyn Read + Send + 'a> {
    match zstd::stream::read::Decoder::new(r) {
        Ok(mut d) => match d.window_log_max(ZSTD_WINDOW_LOG_MAX) {
            Ok(()) => Box::new(d),
            Err(e) => Box::new(FailReader(Some(e))),
        },
        Err(e) => Box::new(FailReader(Some(e))),
    }
}

fn decompressor<'a>(r: impl Read + Send + 'a, c: Comp) -> Box<dyn Read + Send + 'a> {
    match c {
        Comp::None => Box::new(r),
        Comp::Gz => Box::new(flate2::read::MultiGzDecoder::new(r)),
        Comp::Bz2 => Box::new(bzip2::read::MultiBzDecoder::new(r)),
        Comp::Xz => Box::new(xz_reader(r)),
        Comp::Zstd => zstd_reader(r),
    }
}

/// Streaming compressor with an explicit, error-checked finish.
enum Enc {
    None(File),
    Gz(flate2::write::GzEncoder<File>),
    Bz2(bzip2::write::BzEncoder<File>),
    Xz(xz2::write::XzEncoder<File>),
    Zstd(zstd::stream::write::Encoder<'static, File>),
}

impl Enc {
    fn new(f: File, c: Comp) -> Result<Enc> {
        Ok(match c {
            Comp::None => Enc::None(f),
            Comp::Gz => Enc::Gz(flate2::write::GzEncoder::new(f, flate2::Compression::default())),
            Comp::Bz2 => Enc::Bz2(bzip2::write::BzEncoder::new(f, bzip2::Compression::default())),
            Comp::Xz => Enc::Xz(xz2::write::XzEncoder::new(f, 6)),
            Comp::Zstd => Enc::Zstd(zstd::stream::write::Encoder::new(f, 3)?),
        })
    }

    fn finish(self) -> Result<File> {
        Ok(match self {
            Enc::None(f) => f,
            Enc::Gz(e) => e.finish()?,
            Enc::Bz2(e) => e.finish()?,
            Enc::Xz(e) => e.finish()?,
            Enc::Zstd(e) => e.finish()?,
        })
    }
}

impl Write for Enc {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Enc::None(w) => w.write(b),
            Enc::Gz(w) => w.write(b),
            Enc::Bz2(w) => w.write(b),
            Enc::Xz(w) => w.write(b),
            Enc::Zstd(w) => w.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Enc::None(w) => w.flush(),
            Enc::Gz(w) => w.flush(),
            Enc::Bz2(w) => w.flush(),
            Enc::Xz(w) => w.flush(),
            Enc::Zstd(w) => w.flush(),
        }
    }
}

/// Writer for a pending file: counts open writers so commits wait for them.
struct PendingWriter {
    f: File,
    open: Arc<AtomicUsize>,
}

impl Write for PendingWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.f.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.f.flush()
    }
}

impl Drop for PendingWriter {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

fn zip_dt(t: SystemTime) -> Option<zip::DateTime> {
    let dt = crate::util::local(t)?;
    zip::DateTime::from_date_and_time(
        dt.year().clamp(1980, 2107) as u16,
        dt.month() as u8,
        dt.day() as u8,
        dt.hour() as u8,
        dt.minute() as u8,
        dt.second() as u8,
    )
    .ok()
}

fn zip_opts(mode: u32, mtime: Option<SystemTime>, size: u64) -> zip::write::SimpleFileOptions {
    let mut o = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(mode & 0o7777)
        .large_file(size >= 0xFFFF_FFFF);
    if let Some(z) = mtime.and_then(zip_dt) {
        o = o.last_modified_time(z);
    }
    o
}

fn unix_secs(t: Option<SystemTime>) -> u64 {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

/// 7z stores unix modes in the high 16 bits of the attributes (with 0x8000 set).
fn sevenz_attrs(e: &mut sevenz_rust2::ArchiveEntry, mode: u32, mtime: Option<SystemTime>, dir: bool) {
    e.has_windows_attributes = true;
    e.windows_attributes = 0x8000 | ((mode & 0xFFFF) << 16) | if dir { 0x10 } else { 0 };
    if let Some(t) = mtime.and_then(|t| sevenz_rust2::NtTime::try_from(t).ok()) {
        e.has_last_modified_date = true;
        e.last_modified_date = t;
    }
}

/// Archive path for a VFS path ("/a/b" -> "a/b").
fn arc_name(p: &Path) -> String {
    p.to_string_lossy().trim_start_matches('/').to_string()
}

/// How to fetch the data of an entry.
#[derive(Clone, Debug)]
enum Src {
    Zip(usize),
    /// Name as stored in the archive (tar, 7z).
    Named(String),
    /// The whole decompressed stream (single-file compressors).
    Whole,
    /// Written in this session, not committed yet: data in this temp file.
    Pending(PathBuf),
    /// Symlink created in this session (target stored in the entry).
    NewLink,
}

pub struct ArchiveFs {
    kind: Kind,
    file: PathBuf,
    _temp: Option<TempFile>,
    base_display: String,
    backing: Option<PathBuf>,
    entries: HashMap<PathBuf, Entry>,
    children: HashMap<PathBuf, Vec<PathBuf>>,
    sources: HashMap<PathBuf, Src>,
    /// Directories that were only synthesized from file paths.
    synthetic: std::collections::HashSet<PathBuf>,
    default_mtime: Option<SystemTime>,
    /// Uncommitted changes exist.
    dirty: bool,
    /// Temp files of pending writes (deleted when dropped).
    pending: HashMap<PathBuf, TempFile>,
    /// Directories created in this session.
    new_dirs: std::collections::HashSet<PathBuf>,
    /// Existing entries whose mode or mtime changed.
    meta_changed: std::collections::HashSet<PathBuf>,
    /// Pending writers still open (commit waits for them).
    open_writers: Arc<AtomicUsize>,
    /// Encrypted zip entries (ZipCrypto or AES) by VFS path, with their index.
    encrypted: HashMap<PathBuf, usize>,
    /// Verified password for encrypted entries (session memory only).
    password: Option<String>,
}

impl ArchiveFs {
    fn new(kind: Kind, file: PathBuf, base_display: String, backing: Option<PathBuf>, temp: Option<TempFile>) -> Self {
        let default_mtime = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
        let mut fs = ArchiveFs {
            kind,
            file,
            _temp: temp,
            base_display,
            backing,
            entries: HashMap::new(),
            children: HashMap::new(),
            sources: HashMap::new(),
            synthetic: Default::default(),
            default_mtime,
            dirty: false,
            pending: HashMap::new(),
            new_dirs: Default::default(),
            meta_changed: Default::default(),
            open_writers: Arc::new(AtomicUsize::new(0)),
            encrypted: HashMap::new(),
            password: None,
        };
        let mut root = Entry::new("/", EntryKind::Dir);
        root.mode = 0o040755;
        root.mtime = fs.default_mtime;
        fs.entries.insert(PathBuf::from("/"), root);
        fs.children.insert(PathBuf::from("/"), Vec::new());
        fs
    }

    /// Normalize an in-archive name to an absolute VFS path. Returns None for the root
    /// or for names that try to escape the archive.
    fn vpath(raw: &str) -> Option<PathBuf> {
        let mut out = PathBuf::from("/");
        for comp in raw.replace('\\', "/").split('/') {
            match comp {
                "" | "." => {}
                ".." => return None,
                c => out.push(c),
            }
        }
        if out == Path::new("/") { None } else { Some(out) }
    }

    fn ensure_dir(&mut self, p: &Path) {
        if self.entries.contains_key(p) {
            return;
        }
        let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
        self.ensure_dir(&parent);
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut e = Entry::new(name, EntryKind::Dir);
        e.mode = 0o040755;
        e.mtime = self.default_mtime;
        self.entries.insert(p.to_path_buf(), e);
        self.children.entry(parent).or_default().push(p.to_path_buf());
        self.children.entry(p.to_path_buf()).or_default();
        self.synthetic.insert(p.to_path_buf());
    }

    fn add(&mut self, raw_name: &str, mut e: Entry, src: Src) {
        let Some(p) = Self::vpath(raw_name) else { return };
        let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
        self.ensure_dir(&parent);
        e.name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if e.mtime.is_none() {
            e.mtime = self.default_mtime;
        }
        let is_dir = matches!(e.kind, EntryKind::Dir);
        if let Some(old) = self.entries.get(&p) {
            // A real directory record replaces a synthesized one; later duplicates win otherwise.
            if is_dir && !matches!(old.kind, EntryKind::Dir) {
                return;
            }
        } else {
            self.children.entry(parent).or_default().push(p.clone());
        }
        if is_dir {
            self.children.entry(p.clone()).or_default();
        }
        self.synthetic.remove(&p);
        self.entries.insert(p.clone(), e);
        if !is_dir {
            self.sources.insert(p, src);
        }
    }

    /// Resolve symlink targets inside the archive so links to dirs behave as dirs.
    fn resolve_links(&mut self) {
        let links: Vec<(PathBuf, String)> =
            self.entries.iter().filter_map(|(p, e)| e.link_target().map(|t| (p.clone(), t.to_string()))).collect();
        for (p, target) in links {
            let base = p.parent().unwrap_or(Path::new("/"));
            let tp = if target.starts_with('/') { PathBuf::from(&target) } else { normalize(&base.join(&target)) };
            let (to_dir, broken) = match self.entries.get(&tp) {
                Some(t) => (matches!(t.kind, EntryKind::Dir), false),
                None => (false, true),
            };
            if let Some(e) = self.entries.get_mut(&p) {
                e.kind = EntryKind::Symlink { target, to_dir, broken };
            }
        }
    }

    fn file(&self) -> Result<File> {
        File::open(&self.file).with_context(|| format!("open {}", self.file.display()))
    }

    // ------------------------------------------------------------ indexing

    fn index(&mut self) -> Result<()> {
        match self.kind {
            Kind::Zip => self.index_zip(),
            Kind::Tar(c) => self.index_tar(c),
            Kind::Single(c) => self.index_single(c),
            Kind::SevenZ => self.index_7z(),
        }?;
        self.resolve_links();
        Ok(())
    }

    fn index_zip(&mut self) -> Result<()> {
        let mut za = zip::ZipArchive::new(self.file()?).context("not a valid zip archive")?;
        for i in 0..za.len() {
            let (name, is_dir, is_link, size, mode, mtime, enc) = {
                let f = za.by_index_raw(i)?;
                let name = f
                    .name()
                    .map(|n| n.into_owned())
                    .unwrap_or_else(|_| String::from_utf8_lossy(f.name_raw()).into_owned());
                let mtime = f.last_modified().and_then(zip_time);
                (name, f.is_dir(), f.is_symlink(), f.size(), f.unix_mode(), mtime, f.encrypted())
            };
            let kind = if is_dir {
                EntryKind::Dir
            } else if is_link {
                let mut target = String::new();
                if let Ok(mut f) = za.by_index(i) {
                    let _ = f.read_to_string(&mut target);
                }
                EntryKind::Symlink { target, to_dir: false, broken: false }
            } else {
                EntryKind::File
            };
            let mut e = Entry::new("", kind);
            e.size = if is_dir { 0 } else { size };
            e.mode = mode.unwrap_or(if is_dir { 0o040755 } else { 0o100644 });
            e.mtime = mtime;
            e.encrypted = enc && !is_dir;
            if e.encrypted
                && let Some(vp) = Self::vpath(&name)
            {
                self.encrypted.insert(vp, i);
            }
            self.add(&name, e, Src::Zip(i));
        }
        if !self.encrypted.is_empty() && self.password.is_none() {
            self.password = super::secrets::recall(&self.base_display);
        }
        Ok(())
    }

    fn password_error(&self, wrong: bool) -> anyhow::Error {
        anyhow::Error::new(super::secrets::PasswordRequired { archive: self.base_display.clone(), wrong })
    }

    /// Check a password by fully decrypting the smallest encrypted entry (this
    /// verifies the AES MAC / ZipCrypto CRC, not just the 1-2 byte header check).
    fn verify_password(&self, pw: &str) -> Result<bool> {
        let Some((&idx, _)) = self
            .encrypted
            .iter()
            .map(|(p, i)| (i, self.entries.get(p).map(|e| e.size).unwrap_or(u64::MAX)))
            .min_by_key(|(_, s)| *s)
        else {
            return Ok(true);
        };
        let mut za = zip::ZipArchive::new(self.file()?)?;
        let r = za.by_index_decrypt(idx, pw.as_bytes());
        let mut f = match r {
            Ok(f) => f,
            Err(zip::result::ZipError::InvalidPassword) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let mut sink = Vec::new();
        match f.by_ref().take(8 << 20).read_to_end(&mut sink) {
            Ok(_) => Ok(true),
            // CRC / MAC mismatch at the end of the entry: wrong password.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData || e.kind() == std::io::ErrorKind::Other => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn index_tar(&mut self, c: Comp) -> Result<()> {
        let f = self.file()?;
        let mut ar = tar::Archive::new(decompressor(std::io::BufReader::new(f), c));
        for item in ar.entries().context("not a valid tar archive")? {
            let item = item?;
            let h = item.header();
            let name = item.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let et = h.entry_type();
            let link = item.link_name().ok().flatten().map(|l| l.to_string_lossy().into_owned());
            let mode = h.mode().unwrap_or(0o644) & 0o7777;
            let (kind, tmode, src) = if et.is_dir() {
                (EntryKind::Dir, 0o040000, Src::Named(name.clone()))
            } else if et.is_symlink() {
                (
                    EntryKind::Symlink { target: link.unwrap_or_default(), to_dir: false, broken: false },
                    0o120000,
                    Src::Named(name.clone()),
                )
            } else if et.is_hard_link() {
                // Content lives in the link target entry.
                (EntryKind::File, 0o100000, Src::Named(link.unwrap_or_default()))
            } else if et.is_file() || et == tar::EntryType::Continuous || et == tar::EntryType::GNUSparse {
                (EntryKind::File, 0o100000, Src::Named(name.clone()))
            } else if et.is_pax_global_extensions()
                || et.is_pax_local_extensions()
                || et.is_gnu_longname()
                || et.is_gnu_longlink()
            {
                continue;
            } else {
                (EntryKind::Other, 0, Src::Named(name.clone()))
            };
            let mut e = Entry::new("", kind);
            e.size = if matches!(e.kind, EntryKind::File) { item.size() } else { 0 };
            e.mode = tmode | mode;
            e.uid = h.uid().unwrap_or(0) as u32;
            e.gid = h.gid().unwrap_or(0) as u32;
            e.mtime = h.mtime().ok().map(|t| UNIX_EPOCH + Duration::from_secs(t));
            self.add(&name, e, src);
        }
        // Hard links copied their size as 0; fix sizes from their targets.
        let fix: Vec<(PathBuf, u64)> = self
            .sources
            .iter()
            .filter_map(|(p, s)| match s {
                Src::Named(n) => {
                    let tp = Self::vpath(n)?;
                    if &tp != p { self.entries.get(&tp).map(|t| (p.clone(), t.size)) } else { None }
                }
                _ => None,
            })
            .collect();
        for (p, size) in fix {
            if let Some(e) = self.entries.get_mut(&p) {
                e.size = size;
            }
        }
        Ok(())
    }

    fn index_single(&mut self, c: Comp) -> Result<()> {
        let fname = self.file_display_name();
        let inner = {
            let lower = fname.to_ascii_lowercase();
            let cut = match c {
                Comp::Gz => lower.strip_suffix(".gz").map(|s| s.len()),
                Comp::Bz2 => lower.strip_suffix(".bz2").map(|s| s.len()),
                Comp::Xz => lower.strip_suffix(".xz").map(|s| s.len()),
                Comp::Zstd => lower.strip_suffix(".zst").map(|s| s.len()),
                Comp::None => None,
            };
            match cut {
                Some(n) if n > 0 => fname[..n].to_string(),
                _ => format!("{fname}.out"),
            }
        };
        let mut e = Entry::new("", EntryKind::File);
        e.mode = 0o100644;
        e.size = match c {
            // gzip stores the uncompressed size (mod 4 GiB) in the trailer.
            Comp::Gz => {
                let mut f = self.file()?;
                let len = f.metadata()?.len();
                if len >= 18 {
                    f.seek(std::io::SeekFrom::End(-4))?;
                    let mut b = [0u8; 4];
                    f.read_exact(&mut b)?;
                    u32::from_le_bytes(b) as u64
                } else {
                    0
                }
            }
            _ => 0,
        };
        self.add(&inner, e, Src::Whole);
        Ok(())
    }

    fn index_7z(&mut self) -> Result<()> {
        let reader = sevenz_rust2::ArchiveReader::open(&self.file, sevenz_rust2::Password::empty())
            .map_err(|e| anyhow!("not a valid 7z archive: {e}"))?;
        let files: Vec<sevenz_rust2::ArchiveEntry> = reader.archive().files.clone();
        for f in files {
            if f.is_anti_item {
                continue;
            }
            let unix = if f.has_windows_attributes && f.windows_attributes & 0x8000 != 0 {
                Some(f.windows_attributes >> 16)
            } else {
                None
            };
            let is_link = unix.is_some_and(|m| m & 0o170000 == 0o120000);
            let kind = if f.is_directory {
                EntryKind::Dir
            } else if is_link {
                EntryKind::Symlink { target: String::new(), to_dir: false, broken: false }
            } else {
                EntryKind::File
            };
            let mut e = Entry::new("", kind);
            e.size = f.size;
            e.mode = unix.unwrap_or(if f.is_directory { 0o040755 } else { 0o100644 });
            if f.has_last_modified_date {
                let ft: u64 = f.last_modified_date.into();
                let epoch: u64 = sevenz_rust2::NtTime::UNIX_EPOCH.into();
                if ft >= epoch {
                    let d = ft - epoch;
                    e.mtime = Some(UNIX_EPOCH + Duration::new(d / 10_000_000, ((d % 10_000_000) * 100) as u32));
                }
            }
            self.add(&f.name, e, Src::Named(f.name.clone()));
        }
        // Symlink targets are stored as file content; small, so read them now.
        let links: Vec<(PathBuf, String)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.is_symlink())
            .filter_map(|(p, _)| match self.sources.get(p) {
                Some(Src::Named(n)) => Some((p.clone(), n.clone())),
                _ => None,
            })
            .collect();
        if !links.is_empty()
            && let Ok(mut r) = sevenz_rust2::ArchiveReader::open(&self.file, sevenz_rust2::Password::empty())
        {
            for (p, n) in links {
                if let Ok(data) = r.read_file(&n)
                    && let Some(e) = self.entries.get_mut(&p)
                {
                    e.kind = EntryKind::Symlink {
                        target: String::from_utf8_lossy(&data).into_owned(),
                        to_dir: false,
                        broken: false,
                    };
                }
            }
        }
        Ok(())
    }

    fn file_display_name(&self) -> String {
        let src = self.backing.as_deref().unwrap_or(&self.file);
        let from_display = self.base_display.rsplit('/').next().unwrap_or("").to_string();
        if self.backing.is_some() || from_display.is_empty() {
            src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        } else {
            from_display
        }
    }

    fn lookup(&self, path: &Path) -> Result<&Entry> {
        let p = normalize(path);
        self.entries.get(&p).ok_or_else(|| anyhow!("{}: no such file in archive", path.display()))
    }

    /// Follow symlinks inside the archive to the entry that has data.
    fn data_path(&self, path: &Path) -> Result<PathBuf> {
        let mut p = normalize(path);
        for _ in 0..16 {
            let e = self.entries.get(&p).ok_or_else(|| anyhow!("{}: no such file in archive", p.display()))?;
            match &e.kind {
                EntryKind::Symlink { target, .. } => {
                    let base = p.parent().unwrap_or(Path::new("/")).to_path_buf();
                    p = if target.starts_with('/') {
                        normalize(Path::new(target))
                    } else {
                        normalize(&base.join(target))
                    };
                }
                EntryKind::Dir => bail!("{}: is a directory", p.display()),
                _ => return Ok(p),
            }
        }
        bail!("{}: too many levels of symbolic links", path.display())
    }
}

// ---------------------------------------------------------------- writing

impl ArchiveFs {
    fn can_write(&self) -> bool {
        self.backing.is_some() && !matches!(self.kind, Kind::Single(_)) && self.encrypted.is_empty()
    }

    fn check_writable(&self) -> Result<()> {
        if self.can_write() {
            return Ok(());
        }
        if matches!(self.kind, Kind::Single(_)) {
            bail!("{}: single-file compressed archives are read-only", self.scheme());
        }
        if !self.encrypted.is_empty() {
            bail!("encrypted zip archives are read-only in pairdeck (extract, change, and re-create them instead)");
        }
        bail!("archives inside other archives or on remote hosts are read-only")
    }

    /// Parent directory of `p` must exist as a directory in the archive.
    fn check_parent(&self, p: &Path) -> Result<PathBuf> {
        let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
        match self.entries.get(&parent) {
            Some(e) if matches!(e.kind, EntryKind::Dir) => Ok(parent),
            _ => bail!("{}: no such directory in archive", parent.display()),
        }
    }

    fn insert_new(&mut self, p: &Path, e: Entry, src: Option<Src>) {
        let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
        if !self.entries.contains_key(p) {
            self.children.entry(parent).or_default().push(p.to_path_buf());
        }
        if matches!(e.kind, EntryKind::Dir) {
            self.children.entry(p.to_path_buf()).or_default();
        }
        self.entries.insert(p.to_path_buf(), e);
        match src {
            Some(s) => {
                self.sources.insert(p.to_path_buf(), s);
            }
            None => {
                self.sources.remove(p);
            }
        }
        self.synthetic.remove(p);
        self.dirty = true;
    }

    fn remove_entry(&mut self, p: &Path) {
        self.entries.remove(p);
        self.sources.remove(p);
        self.pending.remove(p);
        self.new_dirs.remove(p);
        self.meta_changed.remove(p);
        self.children.remove(p);
        let parent = p.parent().unwrap_or(Path::new("/")).to_path_buf();
        if let Some(k) = self.children.get_mut(&parent) {
            k.retain(|c| c != p);
        }
        self.dirty = true;
    }

    /// Sizes of pending files change while they are written.
    fn refresh_pending_sizes(&mut self) {
        for (p, src) in &self.sources {
            if let Src::Pending(tp) = src
                && let (Ok(m), Some(e)) = (std::fs::metadata(tp), self.entries.get_mut(p))
            {
                e.size = m.len();
            }
        }
    }

    fn commit_if_idle(&mut self) -> Result<()> {
        if self.dirty && self.open_writers.load(Ordering::SeqCst) == 0 { self.commit_now() } else { Ok(()) }
    }

    /// Keep this archive member? Returns its VFS path when it is still present,
    /// not replaced by a pending write, and of the same kind (dir vs non-dir).
    fn keep(&self, raw: &str, is_dir: bool) -> Option<PathBuf> {
        let vp = Self::vpath(raw)?;
        let e = self.entries.get(&vp)?;
        if matches!(self.sources.get(&vp), Some(Src::Pending(_) | Src::NewLink)) || self.new_dirs.contains(&vp) {
            return None;
        }
        (matches!(e.kind, EntryKind::Dir) == is_dir).then_some(vp)
    }

    /// New items to append, in a stable order (parents before children).
    fn new_items(&self) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
        let mut dirs: Vec<PathBuf> = self.new_dirs.iter().filter(|p| self.entries.contains_key(*p)).cloned().collect();
        dirs.sort();
        let mut files = Vec::new();
        let mut links = Vec::new();
        for (p, s) in &self.sources {
            match s {
                Src::Pending(_) => files.push(p.clone()),
                Src::NewLink => links.push(p.clone()),
                _ => {}
            }
        }
        files.sort();
        links.sort();
        (dirs, files, links)
    }

    fn pending_path(&self, p: &Path) -> Result<&Path> {
        match self.sources.get(p) {
            Some(Src::Pending(tp)) => Ok(tp),
            _ => Err(anyhow!("lost pending data for {}", p.display())),
        }
    }

    /// Rewrite the archive with all pending changes and re-index it.
    fn commit_now(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.check_writable()?;
        self.refresh_pending_sizes();
        let tmp = {
            let name = self.file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.file.with_file_name(format!(".{name}.pairdeck-{}.tmp", std::process::id()))
        };
        let guard = TempFile(tmp.clone());
        match self.kind {
            Kind::Zip => self.commit_zip(&tmp),
            Kind::Tar(c) => self.commit_tar(&tmp, c),
            Kind::SevenZ => self.commit_7z(&tmp),
            Kind::Single(_) => unreachable!("checked by check_writable"),
        }
        .with_context(|| format!("cannot update {}", self.file.display()))?;
        if let Ok(m) = std::fs::metadata(&self.file) {
            let _ = std::fs::set_permissions(&tmp, m.permissions());
        }
        std::fs::rename(&tmp, &self.file).with_context(|| format!("cannot replace {}", self.file.display()))?;
        guard.disarm();
        // Start over from the new file.
        self.entries.clear();
        self.children.clear();
        self.sources.clear();
        self.synthetic.clear();
        self.pending.clear();
        self.new_dirs.clear();
        self.meta_changed.clear();
        self.dirty = false;
        self.default_mtime = std::fs::metadata(&self.file).and_then(|m| m.modified()).ok();
        let mut root = Entry::new("/", EntryKind::Dir);
        root.mode = 0o040755;
        root.mtime = self.default_mtime;
        self.entries.insert(PathBuf::from("/"), root);
        self.children.insert(PathBuf::from("/"), Vec::new());
        self.index()
    }

    fn commit_zip(&self, tmp: &Path) -> Result<()> {
        let mut old = zip::ZipArchive::new(self.file()?)?;
        let mut zw = zip::ZipWriter::new(File::create(tmp)?);
        for i in 0..old.len() {
            let (name, is_dir) = {
                let f = old.by_index_raw(i)?;
                (f.name().map(|n| n.into_owned()).unwrap_or_default(), f.is_dir())
            };
            let Some(vp) = self.keep(&name, is_dir) else { continue };
            if !self.meta_changed.contains(&vp) {
                zw.raw_copy_file(old.by_index_raw(i)?)?;
                continue;
            }
            let e = &self.entries[&vp];
            let opts = zip_opts(e.mode, e.mtime, e.size);
            match &e.kind {
                EntryKind::Dir => zw.add_directory(format!("{}/", arc_name(&vp)), opts)?,
                EntryKind::Symlink { target, .. } => zw.add_symlink(arc_name(&vp), target, opts)?,
                _ => {
                    zw.start_file(arc_name(&vp), opts)?;
                    std::io::copy(&mut old.by_index(i)?, &mut zw)?;
                }
            }
        }
        let (dirs, files, links) = self.new_items();
        for p in dirs {
            let e = &self.entries[&p];
            zw.add_directory(format!("{}/", arc_name(&p)), zip_opts(e.mode, e.mtime, 0))?;
        }
        for p in files {
            let e = &self.entries[&p];
            let tp = self.pending_path(&p)?;
            zw.start_file(arc_name(&p), zip_opts(e.mode, e.mtime, e.size))?;
            std::io::copy(&mut File::open(tp)?, &mut zw)?;
        }
        for p in links {
            let e = &self.entries[&p];
            zw.add_symlink(arc_name(&p), e.link_target().unwrap_or_default(), zip_opts(e.mode, e.mtime, 0))?;
        }
        zw.finish()?.sync_all()?;
        Ok(())
    }

    fn commit_tar(&self, tmp: &Path, c: Comp) -> Result<()> {
        let mut b = tar::Builder::new(Enc::new(File::create(tmp)?, c)?);
        b.mode(tar::HeaderMode::Complete);
        {
            let mut ar = tar::Archive::new(decompressor(std::io::BufReader::new(self.file()?), c));
            for item in ar.entries()? {
                let mut item = item?;
                let et = item.header().entry_type();
                if et.is_pax_global_extensions()
                    || et.is_pax_local_extensions()
                    || et.is_gnu_longname()
                    || et.is_gnu_longlink()
                {
                    continue;
                }
                let name = item.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                let Some(vp) = self.keep(&name, et.is_dir()) else { continue };
                let mut h = item.header().clone();
                if self.meta_changed.contains(&vp) {
                    let e = &self.entries[&vp];
                    h.set_mode(e.mode & 0o7777);
                    h.set_mtime(unix_secs(e.mtime));
                }
                let link = item.link_name().ok().flatten().map(|l| l.into_owned());
                match link {
                    Some(l) if et.is_symlink() || et.is_hard_link() => b.append_link(&mut h, &name, l)?,
                    _ => b.append_data(&mut h, &name, &mut item)?,
                }
            }
        }
        let (dirs, files, links) = self.new_items();
        for p in dirs {
            let e = &self.entries[&p];
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Directory);
            h.set_size(0);
            h.set_mode(e.mode & 0o7777);
            h.set_mtime(unix_secs(e.mtime));
            b.append_data(&mut h, format!("{}/", arc_name(&p)), std::io::empty())?;
        }
        for p in files {
            let e = &self.entries[&p];
            let f = File::open(self.pending_path(&p)?)?;
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Regular);
            h.set_size(f.metadata()?.len());
            h.set_mode(e.mode & 0o7777);
            h.set_mtime(unix_secs(e.mtime));
            b.append_data(&mut h, arc_name(&p), f)?;
        }
        for p in links {
            let e = &self.entries[&p];
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            h.set_mtime(unix_secs(e.mtime));
            b.append_link(&mut h, arc_name(&p), e.link_target().unwrap_or_default())?;
        }
        b.into_inner()?.finish()?.sync_all()?;
        Ok(())
    }

    fn commit_7z(&self, tmp: &Path) -> Result<()> {
        let mut aw = sevenz_rust2::ArchiveWriter::create(tmp).map_err(|e| anyhow!("{e}"))?;
        {
            let mut r = sevenz_rust2::ArchiveReader::open(&self.file, sevenz_rust2::Password::empty())
                .map_err(|e| anyhow!("{e}"))?;
            r.for_each_entries(|e, rd| {
                if e.is_anti_item {
                    return Ok(true);
                }
                let Some(vp) = self.keep(&e.name, e.is_directory) else {
                    std::io::copy(rd, &mut std::io::sink())?;
                    return Ok(true);
                };
                let mut ne = e.clone();
                if self.meta_changed.contains(&vp) {
                    let x = &self.entries[&vp];
                    sevenz_attrs(&mut ne, x.mode, x.mtime, e.is_directory);
                }
                if e.is_directory || !e.has_stream {
                    aw.push_archive_entry::<&[u8]>(ne, None)?;
                } else {
                    aw.push_archive_entry(ne, Some(rd))?;
                }
                Ok(true)
            })
            .map_err(|e| anyhow!("{e}"))?;
        }
        let (dirs, files, links) = self.new_items();
        for p in dirs {
            let x = &self.entries[&p];
            let mut ne = sevenz_rust2::ArchiveEntry::new_directory(&arc_name(&p));
            sevenz_attrs(&mut ne, 0o040000 | (x.mode & 0o7777), x.mtime, true);
            aw.push_archive_entry::<&[u8]>(ne, None).map_err(|e| anyhow!("{e}"))?;
        }
        for p in files {
            let x = &self.entries[&p];
            let f = File::open(self.pending_path(&p)?)?;
            let len = f.metadata()?.len();
            let mut ne = sevenz_rust2::ArchiveEntry::new_file(&arc_name(&p));
            sevenz_attrs(&mut ne, 0o100000 | (x.mode & 0o7777), x.mtime, false);
            if len == 0 {
                ne.has_stream = false;
                aw.push_archive_entry::<&[u8]>(ne, None).map_err(|e| anyhow!("{e}"))?;
            } else {
                aw.push_archive_entry(ne, Some(f)).map_err(|e| anyhow!("{e}"))?;
            }
        }
        for p in links {
            let x = &self.entries[&p];
            let target = x.link_target().unwrap_or_default().to_string();
            let mut ne = sevenz_rust2::ArchiveEntry::new_file(&arc_name(&p));
            sevenz_attrs(&mut ne, 0o120777, x.mtime, false);
            aw.push_archive_entry(ne, Some(target.as_bytes())).map_err(|e| anyhow!("{e}"))?;
        }
        aw.finish()?.sync_all()?;
        Ok(())
    }
}

impl Drop for ArchiveFs {
    fn drop(&mut self) {
        // Last chance to persist changes (e.g. the panel left the archive).
        let _ = self.commit_now();
    }
}

fn zip_time(d: zip::DateTime) -> Option<SystemTime> {
    let dt = jiff::civil::DateTime::new(
        i16::try_from(d.year()).ok()?,
        d.month() as i8,
        d.day() as i8,
        d.hour() as i8,
        d.minute() as i8,
        d.second().min(59) as i8,
        0,
    )
    .ok()?;
    // A time repeated by a DST change maps to its earlier instance. A time skipped by
    // one (a gap) is read with the offset from before the change, e.g. 02:30 on a
    // spring-forward night in New York becomes 01:30 standard time, where chrono
    // gave no time at all.
    let zoned = crate::util::local_tz().to_ambiguous_zoned_datetime(dt).earlier().ok()?;
    SystemTime::try_from(zoned.timestamp()).ok()
}

impl Vfs for ArchiveFs {
    fn scheme(&self) -> &'static str {
        match self.kind {
            Kind::Zip => "zip",
            Kind::Tar(_) => "tar",
            Kind::Single(Comp::Gz) => "gz",
            Kind::Single(Comp::Bz2) => "bz2",
            Kind::Single(Comp::Zstd) => "zst",
            Kind::Single(_) => "xz",
            Kind::SevenZ => "7z",
        }
    }

    fn display(&self, path: &Path) -> String {
        format!("{}#{}", self.base_display, normalize(path).display())
    }

    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>> {
        self.commit_if_idle()?;
        self.refresh_pending_sizes();
        let mut d = normalize(dir);
        // Allow listing through a symlinked directory.
        if let Some(Entry { kind: EntryKind::Symlink { target, to_dir: true, .. }, .. }) = self.entries.get(&d) {
            let base = d.parent().unwrap_or(Path::new("/")).to_path_buf();
            d = if target.starts_with('/') { normalize(Path::new(target)) } else { normalize(&base.join(target)) };
        }
        let kids = self.children.get(&d).ok_or_else(|| anyhow!("{}: not a directory in archive", dir.display()))?;
        Ok(kids.iter().filter_map(|p| self.entries.get(p).cloned()).collect())
    }

    fn stat(&mut self, path: &Path) -> Result<Entry> {
        self.refresh_pending_sizes();
        let mut e = self.lookup(path)?.clone();
        if normalize(path) == Path::new("/") {
            e.name = "/".into();
        }
        Ok(e)
    }

    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>> {
        let p = self.data_path(path)?;
        let size = self.entries.get(&p).map(|e| e.size).unwrap_or(0);
        let src = self.sources.get(&p).cloned().ok_or_else(|| anyhow!("{}: no data", path.display()))?;
        match (self.kind, src) {
            (_, Src::Pending(tp)) => Ok(Box::new(File::open(&tp).with_context(|| format!("open {}", tp.display()))?)),
            (Kind::Zip, Src::Zip(i)) if self.encrypted.contains_key(&p) => {
                let pw = self.password.clone().ok_or_else(|| self.password_error(false))?;
                let mut za = zip::ZipArchive::new(self.file()?)?;
                let f = match za.by_index_decrypt(i, pw.as_bytes()) {
                    Ok(f) => f,
                    Err(zip::result::ZipError::InvalidPassword) => return Err(self.password_error(true)),
                    Err(e) => bail!("{}: {e}", path.display()),
                };
                owned_reader(f, size)
            }
            (Kind::Zip, Src::Zip(i)) => {
                let mut za = zip::ZipArchive::new(self.file()?)?;
                let f = za.by_index(i).map_err(|e| anyhow!("{}: {e}", path.display()))?;
                owned_reader(f, size)
            }
            (Kind::Tar(c), Src::Named(name)) => {
                let want = Self::vpath(&name);
                let f = self.file()?;
                let mut ar = tar::Archive::new(decompressor(std::io::BufReader::new(f), c));
                for item in ar.entries()? {
                    let item = item?;
                    let n = item.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                    if Self::vpath(&n) == want && !item.header().entry_type().is_hard_link() {
                        return owned_reader(item, size);
                    }
                }
                bail!("{}: entry not found in archive", path.display())
            }
            (Kind::Single(c), Src::Whole) => {
                let f = self.file()?;
                let dec: Box<dyn Read + Send> = match c {
                    Comp::Gz => Box::new(flate2::read::MultiGzDecoder::new(std::io::BufReader::new(f))),
                    Comp::Bz2 => Box::new(bzip2::read::MultiBzDecoder::new(std::io::BufReader::new(f))),
                    Comp::Xz => Box::new(xz_reader(std::io::BufReader::new(f))),
                    Comp::Zstd => zstd_reader(std::io::BufReader::new(f)),
                    Comp::None => Box::new(f),
                };
                // The archive file is owned by us (or a temp kept alive by self); stream directly.
                if self._temp.is_some() {
                    return owned_reader(dec, u64::MAX);
                }
                Ok(dec)
            }
            (Kind::SevenZ, Src::Named(name)) => {
                let mut r = sevenz_rust2::ArchiveReader::open(&self.file, sevenz_rust2::Password::empty())
                    .map_err(|e| anyhow!("{e}"))?;
                if size > MEM_LIMIT {
                    let tp = temp_path("7z");
                    let mut out = File::options().read(true).write(true).create_new(true).open(&tp)?;
                    let _ = std::fs::remove_file(&tp);
                    let mut found = false;
                    let mut err: Option<std::io::Error> = None;
                    r.for_each_entries(|e, rd| {
                        if e.name == name {
                            found = true;
                            if let Err(x) = std::io::copy(rd, &mut out) {
                                err = Some(x);
                            }
                            return Ok(false);
                        }
                        Ok(true)
                    })
                    .map_err(|e| anyhow!("{e}"))?;
                    if let Some(e) = err {
                        return Err(e.into());
                    }
                    if !found {
                        bail!("{}: entry not found in archive", path.display());
                    }
                    out.rewind()?;
                    return Ok(Box::new(out));
                }
                let data = r.read_file(&name).map_err(|e| anyhow!("{}: {e}", path.display()))?;
                Ok(Box::new(Cursor::new(data)))
            }
            _ => bail!("{}: unsupported entry", path.display()),
        }
    }

    fn local_backing(&self) -> Option<PathBuf> {
        self.backing.clone()
    }

    fn writable(&self) -> bool {
        self.can_write()
    }

    fn needs_password(&self, paths: &[PathBuf]) -> bool {
        if self.encrypted.is_empty() || self.password.is_some() {
            return false;
        }
        paths.iter().any(|p| {
            let p = normalize(p);
            self.encrypted.keys().any(|e| e == &p || e.starts_with(&p))
        })
    }

    fn set_password(&mut self, password: &str) -> Result<bool> {
        if self.encrypted.is_empty() {
            return Ok(true);
        }
        if self.verify_password(password)? {
            self.password = Some(password.to_string());
            super::secrets::remember(&self.base_display, password);
            Ok(true)
        } else {
            super::secrets::forget(&self.base_display);
            Ok(false)
        }
    }

    fn create_write(&mut self, path: &Path, mode: u32) -> Result<Box<dyn Write + Send>> {
        self.check_writable()?;
        let p = normalize(path);
        self.check_parent(&p)?;
        if self.entries.get(&p).is_some_and(|e| matches!(e.kind, EntryKind::Dir)) {
            bail!("{}: is a directory", p.display());
        }
        let tp = temp_path("pending");
        let f = File::options()
            .write(true)
            .create_new(true)
            .open(&tp)
            .with_context(|| format!("create {}", tp.display()))?;
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut e = Entry::new(name, EntryKind::File);
        e.mode = 0o100000 | if mode == 0 { 0o644 } else { mode & 0o7777 };
        e.mtime = Some(SystemTime::now());
        self.insert_new(&p, e, Some(Src::Pending(tp.clone())));
        self.pending.insert(p, TempFile(tp));
        self.open_writers.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(PendingWriter { f, open: self.open_writers.clone() }))
    }

    fn mkdir(&mut self, path: &Path, mode: u32) -> Result<()> {
        self.check_writable()?;
        let p = normalize(path);
        if self.entries.contains_key(&p) {
            bail!("{}: already exists", p.display());
        }
        self.check_parent(&p)?;
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut e = Entry::new(name, EntryKind::Dir);
        e.mode = 0o040000 | if mode == 0 { 0o755 } else { mode & 0o7777 };
        e.mtime = Some(SystemTime::now());
        self.insert_new(&p, e, None);
        self.new_dirs.insert(p);
        Ok(())
    }

    fn remove_file(&mut self, path: &Path) -> Result<()> {
        self.check_writable()?;
        let p = normalize(path);
        match self.entries.get(&p) {
            Some(e) if matches!(e.kind, EntryKind::Dir) => bail!("{}: is a directory", p.display()),
            Some(_) => {
                self.remove_entry(&p);
                Ok(())
            }
            None => bail!("{}: no such file in archive", p.display()),
        }
    }

    fn remove_dir(&mut self, path: &Path) -> Result<()> {
        self.check_writable()?;
        let p = normalize(path);
        if p == Path::new("/") {
            bail!("cannot remove the archive root");
        }
        match self.entries.get(&p) {
            Some(e) if matches!(e.kind, EntryKind::Dir) => {
                if self.children.get(&p).is_some_and(|k| !k.is_empty()) {
                    bail!("{}: directory not empty", p.display());
                }
                self.remove_entry(&p);
                Ok(())
            }
            Some(_) => bail!("{}: not a directory", p.display()),
            None => bail!("{}: no such directory in archive", p.display()),
        }
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        self.check_writable()?;
        let (f, t) = (normalize(from), normalize(to));
        let e = self.lookup(&f)?.clone();
        if matches!(e.kind, EntryKind::Dir) {
            bail!("renaming directories inside archives is not supported (copy and delete instead)");
        }
        if self.entries.get(&t).is_some_and(|x| matches!(x.kind, EntryKind::Dir)) {
            bail!("{}: is a directory", t.display());
        }
        self.check_parent(&t)?;
        if let EntryKind::Symlink { target, .. } = &e.kind {
            let target = target.clone();
            self.remove_entry(&f);
            if self.entries.contains_key(&t) {
                self.remove_entry(&t);
            }
            return self.symlink(&target, &t);
        }
        let mut r = self.open_read(&f)?;
        {
            let mut w = self.create_write(&t, e.mode & 0o7777)?;
            std::io::copy(&mut r, &mut w)?;
        }
        if let Some(x) = self.entries.get_mut(&t) {
            x.mtime = e.mtime;
        }
        self.remove_entry(&f);
        Ok(())
    }

    fn set_mode(&mut self, path: &Path, mode: u32) -> Result<()> {
        self.check_writable()?;
        let p = normalize(path);
        let e = self.entries.get_mut(&p).ok_or_else(|| anyhow!("{}: no such file in archive", p.display()))?;
        e.mode = (e.mode & 0o170000) | (mode & 0o7777);
        self.meta_changed.insert(p);
        self.dirty = true;
        Ok(())
    }

    fn set_mtime(&mut self, path: &Path, mtime: SystemTime) -> Result<()> {
        if !self.can_write() {
            return Ok(());
        }
        let p = normalize(path);
        if let Some(e) = self.entries.get_mut(&p) {
            e.mtime = Some(mtime);
            self.meta_changed.insert(p);
            self.dirty = true;
        }
        Ok(())
    }

    fn symlink(&mut self, target: &str, link: &Path) -> Result<()> {
        self.check_writable()?;
        let p = normalize(link);
        if self.entries.contains_key(&p) {
            bail!("{}: already exists", p.display());
        }
        self.check_parent(&p)?;
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut e = Entry::new(name, EntryKind::Symlink { target: target.to_string(), to_dir: false, broken: false });
        e.mode = 0o120777;
        e.mtime = Some(SystemTime::now());
        self.insert_new(&p, e, Some(Src::NewLink));
        self.resolve_links();
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        self.commit_now()
    }
}

pub fn open(parent: &Location, path: &Path) -> Result<Box<dyn Vfs>> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    let base_display = parent.vfs.lock().display(path);
    if super::dmg::is_dmg_name(&name) {
        let local = parent.local_path().map(|_| path.to_path_buf());
        let p = local.ok_or_else(|| anyhow!("{name}: disk images can only be opened from the local disk"))?;
        return super::dmg::open_sync(&p, &base_display);
    }
    if lower.ends_with(".iso") || lower.ends_with(".img") {
        let (file, temp, backing) = materialize(parent, path, &name)?;
        let temp = temp.map(|t| Box::new(t) as Box<dyn std::any::Any + Send>);
        let fs = super::iso::IsoFs::open(file, base_display, backing, temp)
            .with_context(|| format!("cannot open {name}"))?;
        return Ok(Box::new(fs));
    }
    let kind = detect(&name).ok_or_else(|| anyhow!("{name}: unsupported archive format"))?;
    let (file, temp, backing) = materialize(parent, path, &name)?;
    let mut fs = ArchiveFs::new(kind, file, base_display, backing, temp);
    fs.index().with_context(|| format!("cannot open {name}"))?;
    Ok(Box::new(fs))
}

/// Local file for an archive: itself when local, else a temp copy (removed on drop).
fn materialize(parent: &Location, path: &Path, name: &str) -> Result<(PathBuf, Option<TempFile>, Option<PathBuf>)> {
    let local = parent.is_local();
    Ok(if local {
        (path.to_path_buf(), None, Some(path.to_path_buf()))
    } else {
        // Remote or nested archive: copy it to a temp file first.
        let tp = temp_path(name);
        let guard = TempFile(tp.clone());
        let mut r = parent.vfs.lock().open_read(path)?;
        let mut out = File::create(&tp).with_context(|| format!("create {}", tp.display()))?;
        std::io::copy(&mut r, &mut out)?;
        out.flush()?;
        (tp, Some(guard), None)
    })
}

// ---------------------------------------------------------------- compression

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutFmt {
    Zip,
    TarGz,
    TarXz,
    TarBz2,
    TarZst,
    SevenZ,
}

const FORMATS: [(&str, OutFmt, &str); 6] = [
    ("zip", OutFmt::Zip, ".zip"),
    ("tar.gz", OutFmt::TarGz, ".tar.gz"),
    ("tar.xz", OutFmt::TarXz, ".tar.xz"),
    ("tar.bz2", OutFmt::TarBz2, ".tar.bz2"),
    ("tar.zst", OutFmt::TarZst, ".tar.zst"),
    ("7z", OutFmt::SevenZ, ".7z"),
];

pub fn compress_dialog(app: &mut App) {
    let p = app.panel();
    let Some(src_dir) = p.loc.local_path().map(Path::to_path_buf) else {
        app.error("Compress works on local files only.\nCopy the files to a local directory first.");
        return;
    };
    let sel = p.selection();
    if sel.is_empty() {
        return;
    }
    let names: Vec<String> = sel.iter().map(|e| e.name.clone()).collect();
    let stem = if names.len() == 1 {
        let n = &names[0];
        match n.rfind('.') {
            Some(i) if i > 0 && !sel[0].is_dir() => n[..i].to_string(),
            _ => n.clone(),
        }
    } else {
        src_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "archive".into())
    };
    let out_dir = app.other().loc.local_path().map(Path::to_path_buf).unwrap_or_else(|| src_dir.clone());
    let default = out_dir.join(format!("{stem}.zip")).display().to_string();
    let what = if names.len() == 1 {
        format!("\"{}\"", crate::util::trunc_mid(&names[0], 40))
    } else {
        format!("{} items", names.len())
    };
    let labels: Vec<&str> = FORMATS.iter().map(|f| f.0).collect();
    let form = Form::new("Compress")
        .width(64)
        .input("name", &format!("Compress {what} to:"), &default)
        .radio("fmt", "Format:", &labels, 0)
        .buttons(&["OK", "Cancel"], 0)
        .on_submit(move |app, r| {
            if r.button != 0 {
                return;
            }
            let (_, fmt, ext) = FORMATS[r.radio("fmt")];
            let mut target = crate::util::expand_tilde(r.text("name").trim());
            if target.is_empty() {
                return;
            }
            // Swap a known archive extension for the chosen one.
            for (_, _, e) in FORMATS {
                if target.to_ascii_lowercase().ends_with(e) {
                    target.truncate(target.len() - e.len());
                    break;
                }
            }
            target.push_str(ext);
            let mut out = PathBuf::from(&target);
            if !out.is_absolute() {
                out = src_dir.join(out);
            }
            if out.exists() {
                let o2 = out.clone();
                let (sd, nm) = (src_dir.clone(), names.clone());
                app.push_dialog(crate::dialog::confirm(
                    "Compress",
                    &format!("{} already exists. Overwrite?", out.display()),
                    move |app| start_compress(app, sd, nm, o2, fmt),
                ));
                return;
            }
            start_compress(app, src_dir.clone(), names.clone(), out, fmt);
        });
    app.push_dialog(Box::new(form));
}

fn start_compress(app: &mut App, src_dir: PathBuf, names: Vec<String>, out: PathBuf, fmt: OutFmt) {
    let tx = app.tx.clone();
    app.info(format!("Compressing to {}...", out.display()));
    std::thread::spawn(move || {
        let res = build_archive(&src_dir, &names, &out, fmt);
        crate::app::call(&tx, move |app| {
            match res {
                Ok(()) => {
                    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                    app.info(format!("Created {} ({})", out.display(), crate::util::human_size(size)));
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&out);
                    app.error(format!("Compression failed:\n{e:#}"));
                }
            }
            app.reload_panels();
        });
    });
}

/// Walk `names` under `src_dir`, yielding (absolute path, archive-relative name, lstat).
fn walk(src_dir: &Path, names: &[String]) -> Vec<(PathBuf, String, std::fs::Metadata)> {
    let mut out = Vec::new();
    for n in names {
        for de in walkdir::WalkDir::new(src_dir.join(n)).follow_links(false).sort_by_file_name().into_iter().flatten() {
            let p = de.path().to_path_buf();
            let Ok(rel) = p.strip_prefix(src_dir) else { continue };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if let Ok(m) = std::fs::symlink_metadata(&p) {
                out.push((p, rel, m));
            }
        }
    }
    out
}

fn build_archive(src_dir: &Path, names: &[String], out: &Path, fmt: OutFmt) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let items = walk(src_dir, names);
    match fmt {
        OutFmt::Zip => {
            let mut zw = zip::ZipWriter::new(File::create(out)?);
            for (p, rel, m) in &items {
                let mode = m.permissions().mode() & 0o7777;
                let mut opts = zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(mode)
                    .large_file(m.len() >= 0xFFFF_FFFF);
                if let Some(z) = m.modified().ok().and_then(zip_dt) {
                    opts = opts.last_modified_time(z);
                }
                let ft = m.file_type();
                if ft.is_symlink() {
                    let target = std::fs::read_link(p)?.to_string_lossy().into_owned();
                    zw.add_symlink(rel.clone(), target, opts)?;
                } else if ft.is_dir() {
                    zw.add_directory(format!("{rel}/"), opts)?;
                } else if ft.is_file() {
                    zw.start_file(rel.clone(), opts)?;
                    std::io::copy(&mut File::open(p)?, &mut zw)?;
                }
            }
            zw.finish()?;
        }
        OutFmt::TarGz | OutFmt::TarXz | OutFmt::TarBz2 | OutFmt::TarZst => {
            let comp = match fmt {
                OutFmt::TarGz => Comp::Gz,
                OutFmt::TarXz => Comp::Xz,
                OutFmt::TarZst => Comp::Zstd,
                _ => Comp::Bz2,
            };
            let mut b = tar::Builder::new(Enc::new(File::create(out)?, comp)?);
            b.follow_symlinks(false);
            for (p, rel, m) in &items {
                let ft = m.file_type();
                if ft.is_dir() || ft.is_file() || ft.is_symlink() {
                    b.append_path_with_name(p, rel).with_context(|| format!("add {}", p.display()))?;
                }
            }
            b.into_inner()?.finish()?;
        }
        OutFmt::SevenZ => {
            let mut aw = sevenz_rust2::ArchiveWriter::create(out).map_err(|e| anyhow!("{e}"))?;
            for (p, rel, m) in &items {
                let ft = m.file_type();
                let entry = sevenz_rust2::ArchiveEntry::from_path(p, rel.clone());
                if ft.is_dir() {
                    aw.push_archive_entry::<&[u8]>(entry, None).map_err(|e| anyhow!("{e}"))?;
                } else if ft.is_file() {
                    aw.push_archive_entry(entry, Some(File::open(p)?)).map_err(|e| anyhow!("{e}"))?;
                } else if ft.is_symlink() {
                    // Stored like p7zip does: unix mode in the attributes, target as content.
                    let target = std::fs::read_link(p)?.to_string_lossy().into_owned();
                    let mut e = sevenz_rust2::ArchiveEntry::new_file(rel);
                    sevenz_attrs(&mut e, 0o120777, m.modified().ok(), false);
                    aw.push_archive_entry(e, Some(target.as_bytes())).map_err(|e| anyhow!("{e}"))?;
                }
            }
            aw.finish()?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "archive_pw_tests.rs"]
mod password_tests;

#[cfg(test)]
mod tests {
    #[test]
    fn temp_file_guard_removes_unless_disarmed() {
        let dir = std::env::temp_dir().join(format!("pairdeck-tempfile-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let (gone, kept) = (dir.join("gone"), dir.join("kept"));
        for p in [&gone, &kept] {
            assert!(std::fs::write(p, b"x").is_ok());
        }
        drop(TempFile(gone.clone()));
        TempFile(kept.clone()).disarm();
        assert!(!gone.exists());
        assert!(kept.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_times_round_trip_in_local_time() {
        let z = zip::DateTime::from_date_and_time(2021, 3, 14, 12, 30, 44).ok();
        let t = z.and_then(zip_time);
        assert!(t.is_some());
        let back = t.and_then(zip_dt);
        assert_eq!(
            back.map(|b| (b.year(), b.month(), b.day(), b.hour(), b.minute(), b.second())),
            Some((2021, 3, 14, 12, 30, 44))
        );
        // The last representable second (zip seconds have 2 s resolution).
        assert!(zip::DateTime::from_date_and_time(2021, 3, 14, 12, 30, 58).ok().and_then(zip_time).is_some());
    }

    use super::*;

    fn sample_tree(dir: &Path) -> PathBuf {
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("sub/deep")).unwrap();
        std::fs::write(src.join("hello.txt"), b"hello world\n").unwrap();
        std::fs::write(src.join("sub/deep/data.bin"), vec![7u8; 100_000]).unwrap();
        let _ = std::os::unix::fs::symlink("hello.txt", src.join("link.txt"));
        src
    }

    fn check(fs: &mut Box<dyn Vfs>) {
        let root = fs.list(Path::new("/")).unwrap();
        let src = root.iter().find(|e| e.name == "src").expect("src dir");
        assert!(src.is_dir());
        let names: Vec<String> = fs.list(Path::new("/src")).unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"hello.txt".to_string()), "{names:?}");
        assert!(names.contains(&"sub".to_string()));
        let mut s = String::new();
        fs.open_read(Path::new("/src/hello.txt")).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello world\n");
        let mut v = Vec::new();
        fs.open_read(Path::new("/src/sub/deep/data.bin")).unwrap().read_to_end(&mut v).unwrap();
        assert_eq!(v.len(), 100_000);
        assert_eq!(fs.stat(Path::new("/src/sub/deep/data.bin")).unwrap().size, 100_000);
    }

    #[test]
    fn roundtrip_all_formats() {
        let dir = temp_path("test");
        std::fs::create_dir_all(&dir).unwrap();
        let _src = sample_tree(&dir);
        let loc = Location::local(&dir);
        for (_, fmt, ext) in FORMATS {
            let out = dir.join(format!("out{ext}"));
            build_archive(&dir, &["src".to_string()], &out, fmt).unwrap();
            let mut fs = open(&loc, &out).unwrap_or_else(|e| panic!("{ext}: {e:#}"));
            check(&mut fs);
            {
                let l = fs.stat(Path::new("/src/link.txt")).unwrap();
                assert!(l.is_symlink(), "{ext}: link");
                let mut s = String::new();
                fs.open_read(Path::new("/src/link.txt")).unwrap().read_to_string(&mut s).unwrap();
                assert_eq!(s, "hello world\n");
            }
            assert!(fs.display(Path::new("/src")).ends_with(&format!("out{ext}#/src")));
        }
        // Single-file gzip.
        let gz = dir.join("note.txt.gz");
        let mut enc = flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::default());
        enc.write_all(b"single file").unwrap();
        enc.finish().unwrap();
        let mut fs = open(&loc, &gz).unwrap();
        let l = fs.list(Path::new("/")).unwrap();
        assert_eq!(l[0].name, "note.txt");
        assert_eq!(l[0].size, 11);
        let mut s = String::new();
        fs.open_read(Path::new("/note.txt")).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "single file");
        // Nested archive: zip inside the tar.gz, opened through the outer archive.
        let inner = dir.join("src/inner.zip");
        build_archive(&dir, &["src/hello.txt".to_string()], &inner, OutFmt::Zip).unwrap();
        let outer = dir.join("outer.tar.gz");
        build_archive(&dir, &["src".to_string()], &outer, OutFmt::TarGz).unwrap();
        let ofs = open(&loc, &outer).unwrap();
        let oloc = Location { vfs: crate::vfs::VfsHandle::new(ofs), path: PathBuf::from("/src") };
        let mut nfs = open(&oloc, Path::new("/src/inner.zip")).unwrap();
        let mut s = String::new();
        nfs.open_read(Path::new("/src/hello.txt")).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello world\n");
        assert!(nfs.display(Path::new("/")).contains("outer.tar.gz#/src/inner.zip#/"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(supports("x.zst") && supports("x.tar.zst") && !supports("x.rar") && supports("a.TGZ"));
    }

    fn read_str(fs: &mut Box<dyn Vfs>, p: &str) -> String {
        let mut s = String::new();
        fs.open_read(Path::new(p)).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    fn put(fs: &mut Box<dyn Vfs>, p: &str, data: &[u8], mode: u32) {
        let mut w = fs.create_write(Path::new(p), mode).unwrap();
        w.write_all(data).unwrap();
    }

    #[test]
    fn write_into_every_writable_format() {
        let dir = temp_path("wtest");
        std::fs::create_dir_all(&dir).unwrap();
        sample_tree(&dir);
        let loc = Location::local(&dir);
        for (_, fmt, ext) in FORMATS {
            let out = dir.join(format!("w{ext}"));
            build_archive(&dir, &["src".to_string()], &out, fmt).unwrap();
            {
                let mut fs = open(&loc, &out).unwrap();
                assert!(fs.writable(), "{ext} should be writable");
                put(&mut fs, "/src/new.txt", b"brand new", 0o600);
                put(&mut fs, "/src/hello.txt", b"replaced", 0o644);
                fs.mkdir(Path::new("/src/newdir"), 0o750).unwrap();
                put(&mut fs, "/src/newdir/inner.txt", b"inner", 0);
                fs.remove_file(Path::new("/src/sub/deep/data.bin")).unwrap();
                fs.remove_dir(Path::new("/src/sub/deep")).unwrap();
                fs.set_mode(Path::new("/src/sub"), 0o700).unwrap();
                fs.rename(Path::new("/src/new.txt"), Path::new("/src/renamed.txt")).unwrap();
                fs.symlink("renamed.txt", Path::new("/src/nl")).unwrap();
                assert!(fs.mkdir(Path::new("/src/newdir"), 0).is_err(), "{ext}: mkdir twice");
                assert!(fs.remove_dir(Path::new("/src")).is_err(), "{ext}: rmdir non-empty");
                // Uncommitted changes are visible right away.
                assert_eq!(read_str(&mut fs, "/src/hello.txt"), "replaced");
                assert_eq!(fs.stat(Path::new("/src/renamed.txt")).unwrap().size, 9);
                fs.commit().unwrap_or_else(|e| panic!("{ext}: commit: {e:#}"));
                // Still usable after the commit (re-indexed).
                assert_eq!(read_str(&mut fs, "/src/newdir/inner.txt"), "inner");
            }
            let mut fs = open(&loc, &out).unwrap_or_else(|e| panic!("{ext}: reopen: {e:#}"));
            let mut names: Vec<String> = fs.list(Path::new("/src")).unwrap().into_iter().map(|e| e.name).collect();
            names.sort();
            assert_eq!(names, ["hello.txt", "link.txt", "newdir", "nl", "renamed.txt", "sub"], "{ext}");
            assert_eq!(read_str(&mut fs, "/src/hello.txt"), "replaced", "{ext}");
            assert_eq!(read_str(&mut fs, "/src/renamed.txt"), "brand new", "{ext}");
            assert_eq!(read_str(&mut fs, "/src/newdir/inner.txt"), "inner", "{ext}");
            assert_eq!(fs.stat(Path::new("/src/renamed.txt")).unwrap().mode & 0o777, 0o600, "{ext}: mode");
            assert_eq!(fs.stat(Path::new("/src/sub")).unwrap().mode & 0o777, 0o700, "{ext}: chmod dir");
            assert!(fs.list(Path::new("/src/sub")).unwrap().is_empty(), "{ext}: deep removed");
            assert!(fs.stat(Path::new("/src/new.txt")).is_err(), "{ext}: renamed away");
            assert!(fs.stat(Path::new("/src/nl")).unwrap().is_symlink(), "{ext}: new symlink");
            assert_eq!(read_str(&mut fs, "/src/nl"), "brand new", "{ext}: through symlink");
            assert!(!dir.read_dir().unwrap().flatten().any(|d| d.file_name().to_string_lossy().contains(".pairdeck-")));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_job_into_archive_and_drop_commit() {
        let dir = temp_path("jtest");
        std::fs::create_dir_all(dir.join("in/a")).unwrap();
        std::fs::write(dir.join("in/a/x.txt"), b"xx").unwrap();
        std::fs::write(dir.join("in/big.bin"), vec![3u8; 2_000_000]).unwrap();
        sample_tree(&dir);
        let out = dir.join("t.tar.zst");
        build_archive(&dir, &["src".to_string()], &out, OutFmt::TarZst).unwrap();
        let loc = Location::local(&dir);
        let handle = crate::vfs::VfsHandle::new(open(&loc, &out).unwrap());
        let (tx, rx) = crossbeam_channel::unbounded();
        crate::ops::spawn(
            crate::ops::JobSpec::Copy {
                src: Location::local(dir.join("in")),
                names: vec!["a".into(), "big.bin".into()],
                dst: Location { vfs: handle.clone(), path: PathBuf::from("/src") },
                target: None,
                opts: crate::ops::CopyOpts::default(),
                remove_src: false,
            },
            tx,
        );
        loop {
            if let crate::app::AppEvent::Job(crate::ops::JobMsg::Finished { errors, .. }) =
                rx.recv_timeout(Duration::from_secs(30)).unwrap()
            {
                assert!(errors.is_empty(), "{errors:?}");
                break;
            }
        }
        let mut fs = open(&loc, &out).unwrap();
        assert_eq!(read_str(&mut fs, "/src/a/x.txt"), "xx");
        assert_eq!(fs.stat(Path::new("/src/big.bin")).unwrap().size, 2_000_000);
        // Dropping the VFS commits pending changes too.
        {
            let mut fs = open(&loc, &out).unwrap();
            fs.remove_file(Path::new("/src/big.bin")).unwrap();
        }
        let mut fs = open(&loc, &out).unwrap();
        assert!(fs.stat(Path::new("/src/big.bin")).is_err());
        // Single-file zstd reads; single-file archives are read-only.
        let zst = dir.join("note.txt.zst");
        std::fs::write(&zst, zstd::encode_all(&b"zstd single"[..], 3).unwrap()).unwrap();
        let mut fs = open(&loc, &zst).unwrap();
        assert_eq!(read_str(&mut fs, "/note.txt"), "zstd single");
        assert!(!fs.writable() && fs.create_write(Path::new("/x"), 0).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
