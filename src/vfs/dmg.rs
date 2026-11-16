//! Disk images (.dmg).
//!
//! On macOS an image is attached read-only with `hdiutil` under a private temp
//! mount root and browsed like a local directory; it is detached as soon as no
//! panel shows it any more, and on exit. Encrypted images ask for a password,
//! passed to hdiutil on stdin. Elsewhere, if 7-Zip is installed, images are
//! listed and extracted through the `7z` command line (read-only).

use super::secrets::{self, PasswordRequired};
use super::{Entry, EntryKind, Location, Vfs, VfsHandle, local::LocalFs, normalize};
use crate::app::App;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub fn is_dmg_name(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".dmg")
}

/// 7-Zip command line program, if installed.
fn sevenzip() -> Option<PathBuf> {
    ["7zz", "7z", "7za"].iter().find_map(|n| which::which(n).ok())
}

/// Use hdiutil (macOS) unless forced to the 7-Zip path (tests, other platforms).
fn use_hdiutil() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("PAIRDECK_DMG_USE_7Z").is_none() && which::which("hdiutil").is_ok()
}

/// Can .dmg files be browsed on this system?
pub fn supported() -> bool {
    use_hdiutil() || sevenzip().is_some()
}

// ---------------------------------------------------------------- mount registry

struct Mount {
    /// Whole-disk device to detach, e.g. /dev/disk7.
    device: String,
    /// Private directory that holds the mount point(s).
    mount_root: PathBuf,
}

fn mounts() -> &'static Mutex<HashMap<u64, Mount>> {
    static M: OnceLock<Mutex<HashMap<u64, Mount>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn run_with_timeout(mut cmd: Command, stdin: Option<&str>, timeout: Duration) -> Result<(bool, String, String)> {
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("cannot run hdiutil")?;
    if let (Some(text), Some(mut si)) = (stdin, child.stdin.take()) {
        // hdiutil -stdinpass reads up to a NUL or EOF; the password is never logged.
        let _ = si.write_all(text.as_bytes());
    }
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut o = String::new();
        let mut e = String::new();
        if let Some(s) = out.as_mut() {
            let _ = s.read_to_string(&mut o);
        }
        if let Some(s) = err.as_mut() {
            let _ = s.read_to_string(&mut e);
        }
        (o, e)
    });
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let (o, e) = reader.join().unwrap_or_default();
            return Ok((status.success(), o, e));
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("hdiutil timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Values of `<key>name</key><string>value</string>` pairs in a plist.
fn plist_strings(plist: &str, key: &str) -> Vec<String> {
    let pat = format!("<key>{key}</key>");
    let mut out = Vec::new();
    let mut rest = plist;
    while let Some(i) = rest.find(&pat) {
        rest = &rest[i + pat.len()..];
        let Some(a) = rest.find("<string>") else { break };
        let Some(b) = rest.find("</string>") else { break };
        if a < b {
            out.push(rest[a + 8..b].replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">"));
        }
    }
    out
}

pub fn is_encrypted(path: &Path) -> bool {
    let mut cmd = Command::new("hdiutil");
    cmd.arg("isencrypted").arg(path);
    run_with_timeout(cmd, None, Duration::from_secs(15))
        .map(|(_, o, e)| format!("{o}{e}").contains("encrypted: YES"))
        .unwrap_or(false)
}

/// Attach an image read-only; returns the VFS for its contents.
pub fn attach(path: &Path, base_display: &str, password: Option<&str>) -> Result<DmgFs> {
    let key = path.display().to_string();
    let remembered = secrets::recall(&key);
    let password = password.map(str::to_string).or(remembered);
    if password.is_none() && is_encrypted(path) {
        return Err(PasswordRequired { archive: base_display.to_string(), wrong: false }.into());
    }
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mount_root = std::env::temp_dir().join(format!("pairdeck-dmg-{}-{id}", std::process::id()));
    std::fs::create_dir_all(&mount_root)?;
    let mut cmd = Command::new("hdiutil");
    cmd.args(["attach", "-readonly", "-nobrowse", "-noautoopen", "-noverify", "-plist", "-mountroot"]).arg(&mount_root);
    if password.is_some() {
        cmd.arg("-stdinpass");
    }
    cmd.arg(path);
    let res = run_with_timeout(cmd, password.as_deref(), Duration::from_secs(120));
    let (ok, out, err) = match res {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir(&mount_root);
            return Err(e);
        }
    };
    if !ok {
        let _ = std::fs::remove_dir(&mount_root);
        if err.contains("Authentication error") || err.contains("authentication") {
            secrets::forget(&key);
            return Err(PasswordRequired { archive: base_display.to_string(), wrong: password.is_some() }.into());
        }
        bail!("hdiutil attach failed: {}", err.trim());
    }
    let devices = plist_strings(&out, "dev-entry");
    let points: Vec<PathBuf> = plist_strings(&out, "mount-point").into_iter().map(PathBuf::from).collect();
    let device = devices.iter().min_by_key(|d| d.len()).cloned().ok_or_else(|| anyhow!("hdiutil gave no device"))?;
    mounts().lock().unwrap_or_else(|e| e.into_inner()).insert(id, Mount { device, mount_root: mount_root.clone() });
    if points.is_empty() {
        detach(id);
        bail!("the image has no mountable file system");
    }
    if let Some(pw) = &password {
        secrets::remember(&key, pw);
    }
    // One volume: show it directly; several: show the mount root with one dir per volume.
    let root = if points.len() == 1 { points[0].clone() } else { mount_root };
    Ok(DmgFs { id, root, base_display: base_display.to_string(), backing: path.to_path_buf(), local: LocalFs })
}

/// Detach a mounted image (no-op if already gone).
pub fn detach(id: u64) {
    let m = mounts().lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    let Some(m) = m else { return };
    let try_detach = |force: bool| {
        let mut cmd = Command::new("hdiutil");
        cmd.arg("detach").arg(&m.device).arg("-quiet");
        if force {
            cmd.arg("-force");
        }
        run_with_timeout(cmd, None, Duration::from_secs(30)).map(|r| r.0).unwrap_or(false)
    };
    if !try_detach(false) {
        try_detach(true);
    }
    let _ = std::fs::remove_dir_all(&m.mount_root);
}

/// Detach everything (on exit).
pub fn detach_all() {
    let ids: Vec<u64> = mounts().lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
    for id in ids {
        detach(id);
    }
}

pub fn mounted_count() -> usize {
    mounts().lock().map(|m| m.len()).unwrap_or(0)
}

/// Detach images that no panel (or hidden tab) shows any more.
pub fn gc(app: &App) {
    if mounted_count() == 0 {
        return;
    }
    let mut used: HashSet<u64> = HashSet::new();
    let mut note = |h: &VfsHandle| {
        if let Some(id) = h.lock().mount_id() {
            used.insert(id);
        }
    };
    let panels = app.panels.iter().chain(app.tabs.iter().flat_map(|t| t.others.iter()));
    for p in panels {
        note(&p.loc.vfs);
        for (l, _) in &p.vfs_stack {
            note(&l.vfs);
        }
    }
    let stale: Vec<u64> = {
        let m = mounts().lock().unwrap_or_else(|e| e.into_inner());
        m.keys().copied().filter(|id| !used.contains(id)).collect()
    };
    for id in stale {
        std::thread::spawn(move || detach(id));
    }
}

/// Attach the image `name` in the panel `side` on a background thread and enter it.
pub fn start_attach(app: &mut App, side: usize, dir: Location, name: String, password: Option<String>) {
    let Some(local_dir) = dir.local_path().map(Path::to_path_buf) else {
        app.flash_error("Disk images can only be opened from the local disk");
        return;
    };
    let path = local_dir.join(&name);
    let display = dir.vfs.lock().display(&path);
    if !use_hdiutil() {
        // 7-Zip fallback: listing is quick enough to do directly.
        match SevenZipFs::open(&path, &display) {
            Ok(fs) => {
                let loc = Location { vfs: VfsHandle::new(Box::new(fs)), path: PathBuf::from("/") };
                if let Err(e) = app.panels[side].enter_vfs(loc) {
                    app.error(format!("{e:#}"));
                }
            }
            Err(e) => app.error(format!("{e:#}")),
        }
        return;
    }
    app.info(format!("Mounting {name}..."));
    let tx = app.tx.clone();
    std::thread::spawn(move || {
        let res = attach(&path, &display, password.as_deref());
        crate::app::call(&tx, move |app: &mut App| match res {
            Ok(fs) => {
                let loc = Location { vfs: VfsHandle::new(Box::new(fs)), path: PathBuf::from("/") };
                match app.panels[side].enter_vfs(loc) {
                    Ok(()) => app.info(format!("Mounted {name} (read-only)")),
                    Err(e) => app.error(format!("{e:#}")),
                }
            }
            Err(e) => match secrets::password_required(&e).cloned() {
                Some(pr) => ask_password(app, side, dir, name, pr.wrong),
                None => app.error(format!("Cannot open {name}:\n{e:#}")),
            },
        });
    });
}

fn ask_password(app: &mut App, side: usize, dir: Location, name: String, wrong: bool) {
    let mut form = crate::dialog::Form::new("Encrypted disk image");
    if wrong {
        form = form.text("Wrong password, try again.");
    }
    let form = form
        .text(&format!("{name} is encrypted."))
        .password("pw", "Password:")
        .buttons(&["OK", "Cancel"], 0)
        .on_submit(move |app, r| {
            if r.button == 0 {
                start_attach(app, side, dir, name, Some(r.text("pw")));
            }
        });
    app.push_dialog(Box::new(form));
}

/// Short description of an image without mounting it (preview).
pub fn image_info(path: &Path) -> Result<Vec<String>> {
    if use_hdiutil() {
        if is_encrypted(path) {
            return Ok(vec!["Encrypted disk image (a password is needed to open it)".into()]);
        }
        let mut cmd = Command::new("hdiutil");
        cmd.arg("imageinfo").arg(path);
        let (ok, out, err) = run_with_timeout(cmd, None, Duration::from_secs(15))?;
        if !ok {
            bail!("hdiutil imageinfo failed: {}", err.trim());
        }
        let wanted = [
            "Format:",
            "Format Description:",
            "Class Name:",
            "Partition-Name",
            "Total Bytes",
            "Checksum Type",
            "Compressed Ratio",
            "Software License Agreement",
            "partition-hint",
            "partition-name",
        ];
        let mut lines: Vec<String> = out
            .lines()
            .map(str::trim)
            .filter(|l| wanted.iter().any(|w| l.starts_with(w)))
            .map(str::to_string)
            .collect();
        lines.dedup();
        lines.truncate(60);
        return Ok(lines);
    }
    let fs = SevenZipFs::open(path, &path.display().to_string())?;
    Ok(vec![format!("{} entries (via 7-Zip)", fs.entries.len().saturating_sub(1))])
}

// ---------------------------------------------------------------- hdiutil-backed VFS

/// The contents of an attached image: a read-only view of its mount point.
pub struct DmgFs {
    id: u64,
    root: PathBuf,
    base_display: String,
    backing: PathBuf,
    local: LocalFs,
}

impl DmgFs {
    fn real(&self, path: &Path) -> Result<PathBuf> {
        let rel = normalize(&Path::new("/").join(path));
        let rel = rel.strip_prefix("/").unwrap_or(&rel);
        Ok(self.root.join(rel))
    }
}

impl Drop for DmgFs {
    fn drop(&mut self) {
        let id = self.id;
        std::thread::spawn(move || detach(id));
    }
}

impl Vfs for DmgFs {
    fn scheme(&self) -> &'static str {
        "dmg"
    }
    fn display(&self, path: &Path) -> String {
        format!("{}#{}", self.base_display, normalize(&Path::new("/").join(path)).display())
    }
    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>> {
        let d = self.real(dir)?;
        self.local.list(&d)
    }
    fn stat(&mut self, path: &Path) -> Result<Entry> {
        let p = self.real(path)?;
        let mut e = self.local.stat(&p)?;
        if normalize(&Path::new("/").join(path)) == Path::new("/") {
            e.name = "/".into();
        }
        Ok(e)
    }
    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>> {
        let p = self.real(path)?;
        self.local.open_read(&p)
    }
    fn local_backing(&self) -> Option<PathBuf> {
        Some(self.backing.clone())
    }
    fn mount_id(&self) -> Option<u64> {
        Some(self.id)
    }
}

// ---------------------------------------------------------------- 7-Zip fallback

/// Read-only listing/extraction through the 7-Zip command line.
pub struct SevenZipFs {
    file: PathBuf,
    base_display: String,
    entries: HashMap<PathBuf, Entry>,
    children: HashMap<PathBuf, Vec<PathBuf>>,
    names: HashMap<PathBuf, String>,
    exe: PathBuf,
}

impl SevenZipFs {
    pub fn open(file: &Path, base_display: &str) -> Result<SevenZipFs> {
        let exe = sevenzip().ok_or_else(|| anyhow!("7-Zip (7z or 7zz) is needed to open disk images here"))?;
        let out = Command::new(&exe)
            .args(["l", "-slt", "-ba", "-p-"])
            .arg(file)
            .stdin(Stdio::null())
            .output()
            .context("cannot run 7-Zip")?;
        if !out.status.success() {
            bail!("7-Zip cannot read {}: {}", file.display(), String::from_utf8_lossy(&out.stderr).trim());
        }
        let mut fs = SevenZipFs {
            file: file.to_path_buf(),
            base_display: base_display.to_string(),
            entries: HashMap::new(),
            children: HashMap::new(),
            names: HashMap::new(),
            exe,
        };
        let mut root = Entry::new("/", EntryKind::Dir);
        root.mode = 0o040555;
        fs.entries.insert(PathBuf::from("/"), root);
        fs.children.insert(PathBuf::from("/"), Vec::new());
        for block in String::from_utf8_lossy(&out.stdout).split("\n\n") {
            let mut path = None;
            let mut size = 0u64;
            let mut dir = false;
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("Path = ") {
                    path = Some(v.to_string());
                } else if let Some(v) = line.strip_prefix("Size = ") {
                    size = v.trim().parse().unwrap_or(0);
                } else if let Some(v) = line.strip_prefix("Folder = ") {
                    dir |= v.trim() == "+";
                } else if let Some(v) = line.strip_prefix("Attributes = ") {
                    dir |= v.starts_with('D');
                }
            }
            if let Some(p) = path {
                fs.add(&p, size, dir);
            }
        }
        Ok(fs)
    }

    fn add(&mut self, raw: &str, size: u64, dir: bool) {
        let vp = normalize(&Path::new("/").join(raw.trim_start_matches('/')));
        if vp == Path::new("/") || !vp.starts_with("/") {
            return;
        }
        // Synthesize parents.
        let mut cur = PathBuf::from("/");
        let comps: Vec<_> = vp.strip_prefix("/").unwrap_or(&vp).components().collect();
        for (i, c) in comps.iter().enumerate() {
            let next = cur.join(c);
            let last = i + 1 == comps.len();
            if !self.entries.contains_key(&next) {
                let is_dir = !last || dir;
                let mut e =
                    Entry::new(c.as_os_str().to_string_lossy(), if is_dir { EntryKind::Dir } else { EntryKind::File });
                e.mode = if is_dir { 0o040555 } else { 0o100444 };
                e.size = if is_dir { 0 } else { size };
                self.entries.insert(next.clone(), e);
                self.children.entry(cur.clone()).or_default().push(next.clone());
                if is_dir {
                    self.children.entry(next.clone()).or_default();
                }
            }
            cur = next;
        }
        self.names.insert(vp, raw.to_string());
    }
}

/// Child process stdout that reaps the process when dropped.
struct ChildReader(std::process::Child);

impl Read for ChildReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.0.stdout.as_mut() {
            Some(s) => s.read(buf),
            None => Ok(0),
        }
    }
}

impl Drop for ChildReader {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Vfs for SevenZipFs {
    fn scheme(&self) -> &'static str {
        "dmg"
    }
    fn display(&self, path: &Path) -> String {
        format!("{}#{}", self.base_display, normalize(&Path::new("/").join(path)).display())
    }
    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>> {
        let d = normalize(&Path::new("/").join(dir));
        let kids = self.children.get(&d).ok_or_else(|| anyhow!("{}: no such directory", d.display()))?;
        Ok(kids.iter().filter_map(|k| self.entries.get(k).cloned()).collect())
    }
    fn stat(&mut self, path: &Path) -> Result<Entry> {
        let p = normalize(&Path::new("/").join(path));
        self.entries.get(&p).cloned().ok_or_else(|| anyhow!("{}: not found", p.display()))
    }
    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>> {
        let p = normalize(&Path::new("/").join(path));
        let name = self.names.get(&p).ok_or_else(|| anyhow!("{}: not a file", p.display()))?;
        let child = Command::new(&self.exe)
            .args(["e", "-so", "-p-", "-sns-"])
            .arg(&self.file)
            .arg(name)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot run 7-Zip")?;
        Ok(Box::new(ChildReader(child)))
    }
    fn local_backing(&self) -> Option<PathBuf> {
        Some(self.file.clone())
    }
}

/// Synchronous open used by generic callers (archive::open); the panel uses
/// `start_attach` instead so the UI never waits for hdiutil.
pub fn open_sync(path: &Path, base_display: &str) -> Result<Box<dyn Vfs>> {
    if use_hdiutil() {
        Ok(Box::new(attach(path, base_display, None)?))
    } else {
        Ok(Box::new(SevenZipFs::open(path, base_display)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_parsing() {
        let p = "<dict><key>dev-entry</key>\n<string>/dev/disk9</string><key>mount-point</key><string>/tmp/x &amp; y</string></dict><dict><key>dev-entry</key><string>/dev/disk9s1</string></dict>";
        assert_eq!(plist_strings(p, "dev-entry"), ["/dev/disk9", "/dev/disk9s1"]);
        assert_eq!(plist_strings(p, "mount-point"), ["/tmp/x & y"]);
    }

    fn make_dmg(dir: &Path, encrypted: Option<&str>) -> Option<PathBuf> {
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("inner")).ok()?;
        std::fs::write(src.join("hello.txt"), "hi from dmg").ok()?;
        std::fs::write(src.join("inner/deep.txt"), "deep").ok()?;
        let out = dir.join(if encrypted.is_some() { "enc.dmg" } else { "plain.dmg" });
        let mut cmd = Command::new("hdiutil");
        cmd.args(["create", "-quiet", "-fs", "HFS+", "-volname", "PairdeckTest", "-srcfolder"]).arg(&src).arg(&out);
        if encrypted.is_some() {
            cmd.args(["-encryption", "AES-128", "-stdinpass"]);
        }
        let ok = run_with_timeout(cmd, encrypted, Duration::from_secs(120)).is_ok_and(|r| r.0);
        ok.then_some(out)
    }

    fn hdiutil_mentions(needle: &str) -> bool {
        Command::new("hdiutil")
            .arg("info")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(needle))
            .unwrap_or(false)
    }

    /// Creates, attaches, lists, reads and detaches real images (macOS only).
    #[test]
    #[ignore]
    fn attach_list_detach_real_dmg() {
        if !use_hdiutil() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("pairdeck-dmgtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plain = make_dmg(&dir, None).expect("hdiutil create");
        // 7-Zip fallback (used on other platforms) reads the same image without mounting.
        if sevenzip().is_some() {
            let mut z = SevenZipFs::open(&plain, "plain.dmg").unwrap();
            let all: Vec<String> = z.entries.keys().map(|p| p.display().to_string()).collect();
            let hello = all.iter().find(|p| p.ends_with("/hello.txt")).cloned().expect("hello.txt via 7-Zip");
            let mut s = String::new();
            z.open_read(Path::new(&hello)).unwrap().read_to_string(&mut s).unwrap();
            assert_eq!(s, "hi from dmg");
        }
        {
            let mut fs = attach(&plain, "plain.dmg", None).unwrap();
            let names: Vec<String> = fs.list(Path::new("/")).unwrap().into_iter().map(|e| e.name).collect();
            assert!(names.contains(&"hello.txt".to_string()), "{names:?}");
            let mut s = String::new();
            fs.open_read(Path::new("/inner/deep.txt")).unwrap().read_to_string(&mut s).unwrap();
            assert_eq!(s, "deep");
            assert!(hdiutil_mentions(&fs.root.display().to_string()));
            let id = fs.id;
            detach(id);
            assert!(!hdiutil_mentions(&fs.root.display().to_string()), "mount left behind");
        }
        // Encrypted: no password -> PasswordRequired, wrong -> wrong, right -> works.
        let enc = make_dmg(&dir, Some("s3cret")).expect("hdiutil create encrypted");
        let e = attach(&enc, "enc.dmg", None).err().expect("needs password");
        assert!(secrets::password_required(&e).is_some_and(|p| !p.wrong));
        let e = attach(&enc, "enc.dmg", Some("nope")).err().expect("wrong password");
        assert!(secrets::password_required(&e).is_some_and(|p| p.wrong));
        let fs = attach(&enc, "enc.dmg", Some("s3cret")).unwrap();
        let root = fs.root.display().to_string();
        drop(fs);
        // Drop detaches on a background thread.
        let start = Instant::now();
        while hdiutil_mentions(&root) && start.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(!hdiutil_mentions(&root), "encrypted mount left behind");
        assert_eq!(mounted_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
