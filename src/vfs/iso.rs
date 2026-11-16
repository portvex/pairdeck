//! Read-only ISO 9660 image browsing with Joliet and Rock Ridge extensions.
//!
//! The parser is self-contained and defensive: every offset is bounds checked,
//! directory loops and absurd sizes are rejected, and file data is streamed from
//! the image at its extents (images are never loaded into memory).

use super::{Entry, EntryKind, Vfs, normalize};
use anyhow::{Result, anyhow, bail};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SECTOR: u64 = 2048;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_DEPTH: usize = 64;
const MAX_DIR_BYTES: u64 = 64 << 20;
const MAX_CE_HOPS: usize = 16;

/// Which naming scheme the index was built from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Plain,
    Joliet,
    RockRidge,
}

impl Flavor {
    pub fn label(self) -> &'static str {
        match self {
            Flavor::Plain => "ISO 9660",
            Flavor::Joliet => "ISO 9660 + Joliet",
            Flavor::RockRidge => "ISO 9660 + Rock Ridge",
        }
    }
}

/// Parsed directory tree of an image.
#[derive(Debug)]
pub struct IsoIndex {
    pub entries: HashMap<PathBuf, Entry>,
    pub children: HashMap<PathBuf, Vec<PathBuf>>,
    /// Byte ranges (offset, length) holding each file's data, in order.
    pub extents: HashMap<PathBuf, Vec<(u64, u64)>>,
    pub flavor: Flavor,
    pub volume_id: String,
}

fn read_at<R: Read + Seek>(r: &mut R, size: u64, off: u64, len: u64) -> Result<Vec<u8>> {
    if off >= size {
        bail!("offset {off} beyond end of image");
    }
    let len = len.min(size - off);
    r.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).unwrap_or(0)
}

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).unwrap_or(0)
}

/// 7-byte directory record date (years since 1900, ..., GMT offset in 15 min units).
fn date7(b: &[u8]) -> Option<SystemTime> {
    if b.len() < 7 || b[1] == 0 || b[2] == 0 {
        return None;
    }
    let d = chrono::NaiveDate::from_ymd_opt(1900 + b[0] as i32, b[1] as u32, b[2] as u32)?;
    let t = d.and_hms_opt(b[3] as u32, b[4] as u32, b[5] as u32)?;
    let secs = t.and_utc().timestamp() - (b[6] as i8) as i64 * 15 * 60;
    to_system_time(secs)
}

/// 17-byte "long form" date ("YYYYMMDDHHMMSScc" ASCII + GMT offset).
fn date17(b: &[u8]) -> Option<SystemTime> {
    if b.len() < 17 {
        return None;
    }
    let s = std::str::from_utf8(&b[..14]).ok()?;
    let n = |r: std::ops::Range<usize>| s.get(r).and_then(|x| x.parse::<u32>().ok());
    let d = chrono::NaiveDate::from_ymd_opt(n(0..4)? as i32, n(4..6)?, n(6..8)?)?;
    let t = d.and_hms_opt(n(8..10)?, n(10..12)?, n(12..14)?)?;
    let secs = t.and_utc().timestamp() - (b[16] as i8) as i64 * 15 * 60;
    to_system_time(secs)
}

fn to_system_time(secs: i64) -> Option<SystemTime> {
    if secs >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(secs as u64))
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(std::time::Duration::from_secs(secs.unsigned_abs()))
    }
}

/// One raw directory record.
struct Record<'a> {
    extent: u32,
    data_len: u32,
    date: &'a [u8],
    flags: u8,
    name: &'a [u8],
    system_use: &'a [u8],
}

fn parse_record(rec: &[u8]) -> Option<Record<'_>> {
    if rec.len() < 34 {
        return None;
    }
    let name_len = rec[32] as usize;
    let name_end = 33 + name_len;
    if name_end > rec.len() {
        return None;
    }
    let su_start = (name_end + usize::from(name_len.is_multiple_of(2))).min(rec.len());
    Some(Record {
        extent: le32(rec, 2),
        data_len: le32(rec, 10),
        date: &rec[18..25],
        flags: rec[25],
        name: &rec[33..name_end],
        system_use: &rec[su_start..],
    })
}

/// Rock Ridge information collected from a record's system use area.
#[derive(Default)]
struct RockRidge {
    name: Option<String>,
    mode: Option<u32>,
    nlink: Option<u64>,
    uid: Option<u32>,
    gid: Option<u32>,
    symlink: Option<String>,
    mtime: Option<SystemTime>,
    atime: Option<SystemTime>,
    child_link: Option<u32>,
    relocated: bool,
    has_sp: Option<u8>,
}

struct Parser<'r, R: Read + Seek> {
    r: &'r mut R,
    size: u64,
    block: u64,
}

impl<R: Read + Seek> Parser<'_, R> {
    /// Walk SUSP entries, following CE continuation areas.
    fn susp(&mut self, area: &[u8], skip: usize) -> RockRidge {
        let mut rr = RockRidge::default();
        let mut name = String::new();
        let mut name_done = false;
        let mut link: Vec<String> = Vec::new();
        let mut link_root = false;
        let mut comp_cont = false;
        let mut queue: VecDeque<Vec<u8>> = VecDeque::new();
        queue.push_back(area.get(skip..).unwrap_or(&[]).to_vec());
        let mut hops = 0;
        while let Some(buf) = queue.pop_front() {
            let mut i = 0;
            while i + 4 <= buf.len() {
                let sig = &buf[i..i + 2];
                let len = buf[i + 2] as usize;
                if len < 4 || i + len > buf.len() {
                    break;
                }
                let e = &buf[i..i + len];
                match sig {
                    b"SP" if len >= 7 && e[4] == 0xBE && e[5] == 0xEF => rr.has_sp = Some(e[6]),
                    b"ST" => break,
                    b"CE" if len >= 28 && hops < MAX_CE_HOPS => {
                        hops += 1;
                        let off = le32(e, 4) as u64 * self.block + le32(e, 12) as u64;
                        let clen = (le32(e, 20) as u64).min(64 << 10);
                        if let Ok(more) = read_at(self.r, self.size, off, clen) {
                            queue.push_back(more);
                        }
                    }
                    b"NM" if len >= 5 && !name_done => {
                        let f = e[4];
                        if f & 0x02 != 0 {
                            name = ".".into();
                        } else if f & 0x04 != 0 {
                            name = "..".into();
                        } else {
                            name.push_str(&String::from_utf8_lossy(&e[5..]));
                        }
                        if f & 0x01 == 0 {
                            name_done = true;
                        }
                        rr.name = Some(name.clone());
                    }
                    b"PX" if len >= 36 => {
                        rr.mode = Some(le32(e, 4));
                        rr.nlink = Some(le32(e, 12) as u64);
                        rr.uid = Some(le32(e, 20));
                        rr.gid = Some(le32(e, 28));
                    }
                    b"SL" if len >= 5 => {
                        let mut j = 5;
                        while j + 2 <= e.len() {
                            let cf = e[j];
                            let cl = e[j + 1] as usize;
                            let content = e.get(j + 2..j + 2 + cl).unwrap_or(&[]);
                            let piece = if cf & 0x02 != 0 {
                                ".".to_string()
                            } else if cf & 0x04 != 0 {
                                "..".to_string()
                            } else if cf & 0x08 != 0 {
                                link_root = true;
                                String::new()
                            } else {
                                String::from_utf8_lossy(content).into_owned()
                            };
                            if cf & 0x08 == 0 {
                                if comp_cont {
                                    if let Some(last) = link.last_mut() {
                                        last.push_str(&piece);
                                    }
                                } else {
                                    link.push(piece);
                                }
                            }
                            comp_cont = cf & 0x01 != 0;
                            j += 2 + cl;
                        }
                        let target = link.join("/");
                        rr.symlink = Some(if link_root { format!("/{target}") } else { target });
                    }
                    b"TF" if len >= 5 => {
                        let f = e[4];
                        let w = if f & 0x80 != 0 { 17 } else { 7 };
                        let mut at = 5;
                        for bit in 0..7 {
                            if f & (1 << bit) == 0 {
                                continue;
                            }
                            let stamp = e.get(at..at + w).unwrap_or(&[]);
                            let t = if w == 17 { date17(stamp) } else { date7(stamp) };
                            match bit {
                                1 => rr.mtime = t,
                                2 => rr.atime = t,
                                _ => {}
                            }
                            at += w;
                        }
                    }
                    b"CL" if len >= 12 => rr.child_link = Some(le32(e, 4)),
                    b"RE" => rr.relocated = true,
                    _ => {}
                }
                i += len;
            }
        }
        rr
    }

    fn dir_data(&mut self, extent: u32, len: u32) -> Result<Vec<u8>> {
        let len = (len as u64).min(MAX_DIR_BYTES);
        read_at(self.r, self.size, extent as u64 * self.block, len)
    }

    /// Iterate the records of a directory's data (records never cross sector boundaries).
    fn records(&self, data: &[u8]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        let bs = self.block as usize;
        while pos < data.len() {
            let len = data[pos] as usize;
            if len == 0 {
                pos = (pos / bs + 1) * bs;
                continue;
            }
            if pos + len > data.len() {
                break;
            }
            out.push((pos, len));
            pos += len;
        }
        out
    }
}

fn iso_name(raw: &[u8], joliet: bool) -> String {
    let s = if joliet {
        let units: Vec<u16> = raw.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c)).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    let s = match s.rfind(';') {
        Some(i) => s[..i].to_string(),
        None => s,
    };
    let s = s.strip_suffix('.').map(str::to_string).unwrap_or(s);
    // Never let a name escape its directory.
    s.replace(['/', '\0'], "_")
}

/// True if the data looks like an ISO 9660 image (has a volume descriptor at sector 16).
pub fn sniff<R: Read + Seek>(r: &mut R) -> bool {
    let mut b = [0u8; 6];
    r.seek(SeekFrom::Start(16 * SECTOR + 1)).is_ok() && r.read_exact(&mut b[..5]).is_ok() && &b[..5] == b"CD001"
}

/// Parse the directory tree of an image of `size` bytes.
pub fn parse<R: Read + Seek>(r: &mut R, size: u64) -> Result<IsoIndex> {
    // Volume descriptors.
    let mut pvd: Option<Vec<u8>> = None;
    let mut svd: Option<Vec<u8>> = None;
    for s in 16..16 + 64u64 {
        let Ok(vd) = read_at(r, size, s * SECTOR, SECTOR) else { break };
        if vd.len() < SECTOR as usize || &vd[1..6] != b"CD001" {
            if s == 16 {
                bail!("not an ISO 9660 image");
            }
            break;
        }
        match vd[0] {
            1 if pvd.is_none() => pvd = Some(vd),
            2 if svd.is_none() => {
                let esc = &vd[88..120];
                let joliet = esc.windows(3).any(|w| w == b"%/@" || w == b"%/C" || w == b"%/E");
                if joliet {
                    svd = Some(vd);
                }
            }
            255 => break,
            _ => {}
        }
    }
    let pvd = pvd.ok_or_else(|| anyhow!("ISO image has no primary volume descriptor"))?;
    let bs = le16(&pvd, 128) as u64;
    let block = if bs.is_power_of_two() && (512..=8192).contains(&bs) { bs } else { SECTOR };
    let volume_id = String::from_utf8_lossy(&pvd[40..72]).trim_matches(|c: char| c == ' ' || c == '\0').to_string();
    let mut p = Parser { r, size, block };

    // Rock Ridge: the root's "." record starts with an SP entry.
    let root = parse_record(&pvd[156..190]).ok_or_else(|| anyhow!("bad root directory record"))?;
    let (root_ext, root_len) = (root.extent, root.data_len);
    let mut rr_skip: Option<usize> = None;
    if let Ok(data) = p.dir_data(root_ext, root_len.min(SECTOR as u32))
        && let Some(&(pos, len)) = p.records(&data).first()
        && let Some(rec) = parse_record(&data[pos..pos + len])
    {
        let su = rec.system_use.to_vec();
        rr_skip = p.susp(&su, 0).has_sp.map(usize::from);
    }
    // Some writers (hdiutil) emit Rock Ridge attributes but no NM names; then the
    // Joliet tree has the better (long, mixed case) names.
    if let (Some(skip), Some(_)) = (rr_skip, &svd) {
        let has_names = p.dir_data(root_ext, root_len).is_ok_and(|data| {
            p.records(&data).iter().any(|&(pos, len)| {
                parse_record(&data[pos..pos + len]).is_some_and(|rec| {
                    rec.name != [0] && rec.name != [1] && {
                        let su = rec.system_use.to_vec();
                        p.susp(&su, skip).name.is_some()
                    }
                })
            })
        });
        if !has_names {
            rr_skip = None;
        }
    }
    let (flavor, joliet, start) = match (rr_skip, &svd) {
        (Some(_), _) => (Flavor::RockRidge, false, (root_ext, root_len)),
        (None, Some(s)) => {
            let r = parse_record(&s[156..190]).ok_or_else(|| anyhow!("bad Joliet root record"))?;
            (Flavor::Joliet, true, (r.extent, r.data_len))
        }
        (None, None) => (Flavor::Plain, false, (root_ext, root_len)),
    };
    let skip = rr_skip.unwrap_or(0);

    let mut idx =
        IsoIndex { entries: HashMap::new(), children: HashMap::new(), extents: HashMap::new(), flavor, volume_id };
    let mut root_e = Entry::new("/", EntryKind::Dir);
    root_e.mode = 0o040555;
    root_e.mtime = date7(root.date);
    idx.entries.insert(PathBuf::from("/"), root_e);
    idx.children.insert(PathBuf::from("/"), Vec::new());

    let mut visited: HashSet<u32> = HashSet::new();
    let mut stack: Vec<(PathBuf, u32, u32, usize)> = vec![(PathBuf::from("/"), start.0, start.1, 0)];
    let mut count = 0usize;
    while let Some((dir, ext, len, depth)) = stack.pop() {
        if depth > MAX_DEPTH || !visited.insert(ext) {
            continue;
        }
        let Ok(data) = p.dir_data(ext, len) else { continue };
        // Pending multi-extent file: (name, extents, record info).
        let mut multi: Option<(String, Vec<(u64, u64)>)> = None;
        for (pos, rlen) in p.records(&data) {
            let Some(rec) = parse_record(&data[pos..pos + rlen]) else { continue };
            if rec.name == [0] || rec.name == [1] {
                continue;
            }
            let rr = if flavor == Flavor::RockRidge { p.susp(rec.system_use, skip) } else { RockRidge::default() };
            if rr.relocated {
                continue;
            }
            let name = rr.name.clone().unwrap_or_else(|| iso_name(rec.name, joliet));
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            if flavor == Flavor::RockRidge && depth == 0 && name == "rr_moved" {
                continue;
            }
            let ext_range = (rec.extent as u64 * block, rec.data_len as u64);
            let is_multi = rec.flags & 0x80 != 0;
            if let Some((mname, exts)) = multi.as_mut() {
                if *mname == name {
                    exts.push(ext_range);
                    if is_multi {
                        continue;
                    }
                } else {
                    multi = None;
                }
            } else if is_multi && rec.flags & 0x02 == 0 {
                multi = Some((name.clone(), vec![ext_range]));
                continue;
            }
            let extents = match multi.take() {
                Some((_, exts)) => exts,
                None => vec![ext_range],
            };
            count += 1;
            if count > MAX_ENTRIES {
                bail!("ISO image has too many entries");
            }
            let path = dir.join(&name);
            let is_dir = rec.flags & 0x02 != 0 || rr.child_link.is_some();
            let kind = if let Some(t) = rr.symlink.clone() {
                EntryKind::Symlink { target: t, to_dir: false, broken: false }
            } else if is_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            let mut e = Entry::new(name, kind);
            let size: u64 = extents.iter().map(|(o, l)| (*l).min(size.saturating_sub(*o))).sum();
            e.size = if is_dir { 0 } else { size };
            e.mode = rr.mode.unwrap_or(if is_dir { 0o040555 } else { 0o100444 });
            e.nlink = rr.nlink.unwrap_or(1);
            e.uid = rr.uid.unwrap_or(0);
            e.gid = rr.gid.unwrap_or(0);
            e.mtime = rr.mtime.or_else(|| date7(rec.date));
            e.atime = rr.atime;
            idx.children.entry(dir.clone()).or_default().push(path.clone());
            if is_dir && matches!(e.kind, EntryKind::Dir) {
                idx.children.entry(path.clone()).or_default();
                match rr.child_link {
                    Some(cl) => {
                        // Relocated directory: its length comes from its own "." record.
                        let real_len = p
                            .dir_data(cl, SECTOR as u32)
                            .ok()
                            .and_then(|d| d.first().and_then(|_| parse_record(&d)).map(|r| r.data_len))
                            .unwrap_or(SECTOR as u32);
                        stack.push((path.clone(), cl, real_len, depth + 1));
                    }
                    None => stack.push((path.clone(), rec.extent, rec.data_len, depth + 1)),
                }
            } else {
                idx.extents.insert(path.clone(), extents);
            }
            idx.entries.insert(path, e);
        }
    }
    for list in idx.children.values_mut() {
        list.sort();
        list.dedup();
    }
    Ok(idx)
}

/// Streams a file's extents from the image.
pub struct ExtentReader<R: Read + Seek> {
    inner: R,
    extents: VecDeque<(u64, u64)>,
    left: u64,
}

impl<R: Read + Seek> ExtentReader<R> {
    pub fn new(inner: R, extents: &[(u64, u64)]) -> Self {
        ExtentReader { inner, extents: extents.iter().copied().collect(), left: 0 }
    }
}

impl<R: Read + Seek> Read for ExtentReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.left == 0 {
                let Some((off, len)) = self.extents.pop_front() else { return Ok(0) };
                self.inner.seek(SeekFrom::Start(off))?;
                self.left = len;
                continue;
            }
            let want = buf.len().min(self.left as usize);
            let n = self.inner.read(&mut buf[..want])?;
            if n == 0 {
                // Image shorter than the extent claims: stop cleanly.
                self.left = 0;
                self.extents.clear();
                return Ok(0);
            }
            self.left -= n as u64;
            return Ok(n);
        }
    }
}

/// A mounted (indexed) ISO image.
pub struct IsoFs {
    file: PathBuf,
    base_display: String,
    backing: Option<PathBuf>,
    idx: IsoIndex,
    _temp: Option<Box<dyn std::any::Any + Send>>,
}

impl IsoFs {
    pub fn open(
        file: PathBuf,
        base_display: String,
        backing: Option<PathBuf>,
        temp: Option<Box<dyn std::any::Any + Send>>,
    ) -> Result<IsoFs> {
        let mut f = File::open(&file)?;
        let size = f.metadata()?.len();
        let idx = parse(&mut f, size)?;
        Ok(IsoFs { file, base_display, backing, idx, _temp: temp })
    }

    pub fn flavor(&self) -> Flavor {
        self.idx.flavor
    }

    fn key(path: &Path) -> PathBuf {
        normalize(&Path::new("/").join(path))
    }
}

impl Vfs for IsoFs {
    fn scheme(&self) -> &'static str {
        "iso"
    }

    fn display(&self, path: &Path) -> String {
        format!("{}#{}", self.base_display, Self::key(path).display())
    }

    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>> {
        let d = Self::key(dir);
        let kids = self.idx.children.get(&d).ok_or_else(|| anyhow!("{}: no such directory", d.display()))?;
        Ok(kids.iter().filter_map(|k| self.idx.entries.get(k).cloned()).collect())
    }

    fn stat(&mut self, path: &Path) -> Result<Entry> {
        let p = Self::key(path);
        self.idx.entries.get(&p).cloned().ok_or_else(|| anyhow!("{}: not found", p.display()))
    }

    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>> {
        let p = Self::key(path);
        let ext = self.idx.extents.get(&p).ok_or_else(|| anyhow!("{}: not a file", p.display()))?;
        let f = File::open(&self.file)?;
        Ok(Box::new(ExtentReader::new(std::io::BufReader::new(f), ext)))
    }

    fn local_backing(&self) -> Option<PathBuf> {
        self.backing.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Builds a tiny image by hand: PVD + Joliet SVD, nested dirs, a multi-extent file.
    fn build_minimal() -> Vec<u8> {
        let mut img = vec![0u8; 40 * SECTOR as usize];
        let both32 = |v: u32| {
            let mut b = v.to_le_bytes().to_vec();
            b.extend(v.to_be_bytes());
            b
        };
        let record = |name: &[u8], ext: u32, len: u32, flags: u8| {
            let nl = name.len();
            let mut r = vec![0u8; 33 + nl + usize::from(nl.is_multiple_of(2))];
            r[0] = r.len() as u8;
            r[2..10].copy_from_slice(&both32(ext));
            r[10..18].copy_from_slice(&both32(len));
            r[18..25].copy_from_slice(&[124, 11, 13, 12, 0, 0, 0]);
            r[25] = flags;
            r[32] = nl as u8;
            r[33..33 + nl].copy_from_slice(name);
            r
        };
        let put = |img: &mut Vec<u8>, sector: usize, data: &[u8]| {
            let o = sector * SECTOR as usize;
            img[o..o + data.len()].copy_from_slice(data);
        };
        // PVD at 16, terminator at 17.
        let mut pvd = vec![0u8; SECTOR as usize];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[40..44].copy_from_slice(b"TEST");
        pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
        let root = record(&[0], 20, SECTOR as u32, 2);
        pvd[156..156 + root.len()].copy_from_slice(&root);
        put(&mut img, 16, &pvd);
        let mut term = vec![0u8; 7];
        term[0] = 255;
        term[1..6].copy_from_slice(b"CD001");
        put(&mut img, 17, &term);
        // Root dir at 20: ".", "..", SUB dir (21), HELLO.TXT (30), BIG.BIN multi-extent (31, 32).
        let mut d = Vec::new();
        d.extend(record(&[0], 20, SECTOR as u32, 2));
        d.extend(record(&[1], 20, SECTOR as u32, 2));
        d.extend(record(b"SUB", 21, SECTOR as u32, 2));
        d.extend(record(b"HELLO.TXT;1", 30, 5, 0));
        d.extend(record(b"BIG.BIN;1", 31, 3, 0x80));
        d.extend(record(b"BIG.BIN;1", 32, 3, 0));
        put(&mut img, 20, &d);
        let mut s = Vec::new();
        s.extend(record(&[0], 21, SECTOR as u32, 2));
        s.extend(record(&[1], 20, SECTOR as u32, 2));
        s.extend(record(b"DEEP.;1", 33, 4, 0));
        put(&mut img, 21, &s);
        put(&mut img, 30, b"hello");
        put(&mut img, 31, b"abc");
        put(&mut img, 32, b"def");
        put(&mut img, 33, b"deep");
        img
    }

    #[test]
    fn parses_hand_built_image() {
        let img = build_minimal();
        let mut c = Cursor::new(&img);
        assert!(sniff(&mut c));
        let idx = parse(&mut c, img.len() as u64).unwrap();
        assert_eq!(idx.flavor, Flavor::Plain);
        assert_eq!(idx.volume_id, "TEST");
        let names: Vec<_> = idx.children[Path::new("/")].iter().map(|p| p.display().to_string()).collect();
        assert_eq!(names, ["/BIG.BIN", "/HELLO.TXT", "/SUB"]);
        assert_eq!(idx.entries[Path::new("/BIG.BIN")].size, 6);
        let mut out = String::new();
        ExtentReader::new(Cursor::new(&img), &idx.extents[Path::new("/BIG.BIN")]).read_to_string(&mut out).unwrap();
        assert_eq!(out, "abcdef");
        assert!(idx.entries.contains_key(Path::new("/SUB/DEEP")));
    }

    /// Joliet supplementary descriptor with UCS-2 long names (no Rock Ridge).
    #[test]
    fn joliet_long_names() {
        let mut img = build_minimal();
        let s = SECTOR as usize;
        // Move the terminator to 18 and put a Joliet SVD at 17 whose root is sector 22.
        img.copy_within(17 * s..17 * s + 7, 18 * s);
        let mut svd = img[16 * s..17 * s].to_vec();
        svd[0] = 2;
        svd[88..91].copy_from_slice(b"%/E");
        svd[156 + 2..156 + 6].copy_from_slice(&22u32.to_le_bytes());
        img[17 * s..18 * s].copy_from_slice(&svd);
        let utf16 = |t: &str| t.encode_utf16().flat_map(|u| u.to_be_bytes()).collect::<Vec<u8>>();
        let mut d = Vec::new();
        for (name, ext, len, flags) in
            [(vec![0u8], 22u32, 2048u32, 2u8), (vec![1u8], 22, 2048, 2), (utf16("A long Joliet name.txt;1"), 30, 5, 0)]
        {
            let nl = name.len();
            let mut r = vec![0u8; 33 + nl + usize::from(nl.is_multiple_of(2))];
            r[0] = r.len() as u8;
            r[2..6].copy_from_slice(&ext.to_le_bytes());
            r[10..14].copy_from_slice(&len.to_le_bytes());
            r[25] = flags;
            r[32] = nl as u8;
            r[33..33 + nl].copy_from_slice(&name);
            d.extend(r);
        }
        img[22 * s..22 * s + d.len()].copy_from_slice(&d);
        let idx = parse(&mut Cursor::new(&img), img.len() as u64).unwrap();
        assert_eq!(idx.flavor, Flavor::Joliet);
        assert_eq!(idx.entries[Path::new("/A long Joliet name.txt")].size, 5);
    }

    /// Rock Ridge: SP in the root, NM long names, PX modes, SL symlinks, CE continuation.
    #[test]
    fn rock_ridge_names_modes_symlinks() {
        let s = SECTOR as usize;
        let mut img = build_minimal();
        let su_entry = |sig: &[u8], body: &[u8]| {
            let mut v = sig.to_vec();
            v.push((4 + body.len()) as u8);
            v.push(1);
            v.extend_from_slice(body);
            v
        };
        let px = |mode: u32| {
            let mut b = Vec::new();
            for v in [mode, 1, 501, 20] {
                b.extend(v.to_le_bytes());
                b.extend(v.to_be_bytes());
            }
            su_entry(b"PX", &b)
        };
        let nm = |n: &str| {
            let mut b = vec![0u8];
            b.extend_from_slice(n.as_bytes());
            su_entry(b"NM", &b)
        };
        let rec = |name: &[u8], ext: u32, len: u32, flags: u8, su: &[u8]| {
            let nl = name.len();
            let pad = usize::from(nl.is_multiple_of(2));
            let mut r = vec![0u8; 33 + nl + pad];
            r[2..6].copy_from_slice(&ext.to_le_bytes());
            r[10..14].copy_from_slice(&len.to_le_bytes());
            r[25] = flags;
            r[32] = nl as u8;
            r[33..33 + nl].copy_from_slice(name);
            r.extend_from_slice(su);
            if r.len() % 2 == 1 {
                r.push(0);
            }
            r[0] = r.len() as u8;
            r
        };
        // SL: one component "hello world.txt" relative.
        let mut sl_body = vec![0u8, 0u8, 15u8];
        sl_body.extend_from_slice(b"hello world.txt");
        let sl = su_entry(b"SL", &sl_body);
        // CE: the dir's NM lives in a continuation area at sector 34.
        let cont = [nm("Sub Directory"), px(0o040750)].concat();
        let mut ce_body = Vec::new();
        for v in [34u32, 0, cont.len() as u32] {
            ce_body.extend(v.to_le_bytes());
            ce_body.extend(v.to_be_bytes());
        }
        let ce = su_entry(b"CE", &ce_body);
        let sp = su_entry(b"SP", &[0xBE, 0xEF, 0]);
        let mut d = Vec::new();
        d.extend(rec(&[0], 20, 2048, 2, &sp));
        d.extend(rec(&[1], 20, 2048, 2, &[]));
        d.extend(rec(b"HELLO.TXT;1", 30, 5, 0, &[nm("hello world.txt"), px(0o100640)].concat()));
        d.extend(rec(b"LINK;1", 0, 0, 0, &[nm("link"), px(0o120777), sl].concat()));
        d.extend(rec(b"SUB", 21, 2048, 2, &ce));
        img[20 * s..21 * s].fill(0);
        img[20 * s..20 * s + d.len()].copy_from_slice(&d);
        img[34 * s..34 * s + cont.len()].copy_from_slice(&cont);

        let idx = parse(&mut Cursor::new(&img), img.len() as u64).unwrap();
        assert_eq!(idx.flavor, Flavor::RockRidge);
        let f = &idx.entries[Path::new("/hello world.txt")];
        assert_eq!(f.mode, 0o100640);
        assert_eq!((f.uid, f.gid), (501, 20));
        let l = &idx.entries[Path::new("/link")];
        assert_eq!(l.link_target(), Some("hello world.txt"));
        let sub = &idx.entries[Path::new("/Sub Directory")];
        assert!(sub.is_dir() && sub.mode == 0o040750);
        assert!(idx.entries.contains_key(Path::new("/Sub Directory/DEEP")));
    }

    #[test]
    fn rejects_garbage_and_loops() {
        assert!(parse(&mut Cursor::new(vec![0u8; 100]), 100).is_err());
        // A directory pointing at itself must not loop forever.
        let mut img = build_minimal();
        let o = 21 * SECTOR as usize;
        let rec_start = o + 34 + 34; // third record in SUB
        img[rec_start + 25] = 2; // make DEEP a directory...
        img[rec_start + 2..rec_start + 6].copy_from_slice(&21u32.to_le_bytes()); // ...pointing at SUB
        let idx = parse(&mut Cursor::new(&img), img.len() as u64).unwrap();
        assert!(idx.entries.len() < 20);
    }

    /// Real images from the system tools, with Joliet and Rock Ridge.
    #[test]
    fn parses_tool_built_images() {
        let tmp = std::env::temp_dir().join(format!("pairdeck-iso-{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join("A Long Directory Name/nested")).unwrap();
        std::fs::write(src.join("A Long Directory Name/nested/a file with spaces.txt"), "content").unwrap();
        std::fs::write(src.join("readme.md"), "# hi").unwrap();
        let mut built = Vec::new();
        if cfg!(target_os = "macos") {
            let out = tmp.join("joliet.iso");
            let ok = std::process::Command::new("hdiutil")
                .args(["makehybrid", "-quiet", "-iso", "-joliet", "-o"])
                .arg(&out)
                .arg(&src)
                .status()
                .is_ok_and(|s| s.success());
            // hdiutil writes Rock Ridge attributes without NM names, so Joliet names win.
            if ok {
                built.push((out, Flavor::Joliet));
            }
        }
        for tool in ["xorriso", "mkisofs", "genisoimage"] {
            if which::which(tool).is_err() {
                continue;
            }
            let out = tmp.join(format!("{tool}.iso"));
            let mut cmd = std::process::Command::new(tool);
            if tool == "xorriso" {
                cmd.args(["-as", "mkisofs"]);
            }
            let ok = cmd.args(["-R", "-J", "-quiet", "-o"]).arg(&out).arg(&src).status().is_ok_and(|s| s.success());
            if ok {
                built.push((out, Flavor::RockRidge));
            }
            break;
        }
        for (iso, flavor) in built {
            let mut fs = IsoFs::open(iso.clone(), iso.display().to_string(), None, None).unwrap();
            assert_eq!(fs.flavor(), flavor, "{}", iso.display());
            let p = Path::new("/A Long Directory Name/nested/a file with spaces.txt");
            let mut s = String::new();
            fs.open_read(p).unwrap().read_to_string(&mut s).unwrap();
            assert_eq!(s, "content");
            assert!(fs.list(Path::new("/")).unwrap().iter().any(|e| e.name == "readme.md"));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
