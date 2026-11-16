//! SFTP VFS over libssh2 (ssh2 crate): agent / key / password auth,
//! ~/.ssh/config aliases and known_hosts verification.

use super::{Entry, EntryKind, Location, Vfs, VfsHandle, normalize};
use crate::app::App;
use crate::dialog::Form;
use anyhow::{Context, Result, anyhow, bail};
use ssh2::{CheckResult, FileStat, KnownHostFileKind, OpenFlags, OpenType, Session, Sftp};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Connection parameters after resolving ~/.ssh/config.
#[derive(Clone, Debug, Default)]
pub struct Target {
    /// What the user typed as host (alias or real host); used for display.
    pub alias: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub identity: Vec<PathBuf>,
    pub path: String,
}

/// Returned (inside anyhow) when the server's host key is not in known_hosts.
#[derive(Debug)]
pub struct UnknownHost {
    pub host: String,
    pub port: u16,
    pub fingerprint: String,
}

impl std::fmt::Display for UnknownHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The authenticity of host {}:{} is unknown.\nKey fingerprint (SHA256): {}",
            self.host, self.port, self.fingerprint
        )
    }
}
impl std::error::Error for UnknownHost {}

/// Returned when key based auth failed and no password was given.
#[derive(Debug)]
pub struct NeedPassword(pub String);

impl std::fmt::Display for NeedPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Authentication failed for {}: no usable ssh-agent identity or key.\nUse the SFTP link dialog (Left/Right menu) to enter a password.",
            self.0
        )
    }
}
impl std::error::Error for NeedPassword {}

// ---------------------------------------------------------------- ssh config

#[derive(Default, Debug)]
struct HostConfig {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity: Vec<PathBuf>,
}

fn ssh_config(alias: &str) -> HostConfig {
    let mut cfg = HostConfig::default();
    let Some(text) = dirs::home_dir().and_then(|h| std::fs::read_to_string(h.join(".ssh/config")).ok()) else {
        return cfg;
    };
    let mut active = true; // options before the first Host line apply to all
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, val) = match line.split_once(|c: char| c.is_whitespace() || c == '=') {
            Some((k, v)) => {
                (k.to_ascii_lowercase(), v.trim().trim_start_matches('=').trim().trim_matches('"').to_string())
            }
            None => continue,
        };
        match key.as_str() {
            "host" => {
                active = val.split_whitespace().any(|pat| {
                    let (neg, pat) = match pat.strip_prefix('!') {
                        Some(p) => (true, p),
                        None => (false, pat),
                    };
                    let m = crate::util::glob_matcher(pat, false).is_some_and(|g| g.is_match(alias));
                    m && !neg
                });
            }
            "match" => active = false,
            _ if !active => {}
            // First obtained value wins, like OpenSSH.
            "hostname" if cfg.hostname.is_none() => cfg.hostname = Some(val.replace("%h", alias)),
            "user" if cfg.user.is_none() => cfg.user = Some(val),
            "port" if cfg.port.is_none() => cfg.port = val.parse().ok(),
            "identityfile" => cfg.identity.push(PathBuf::from(crate::util::expand_tilde(&val))),
            _ => {}
        }
    }
    cfg
}

/// Parse `user@host:port/path`, `sftp://user@host:port/path` or an ssh config alias.
pub fn parse_target(input: &str, default_user: Option<&str>) -> Result<Target> {
    let mut s = input.trim();
    for p in ["sftp://", "ssh://"] {
        if let Some(r) = s.strip_prefix(p) {
            s = r;
        }
    }
    let (hostpart, path) = match s.find('/') {
        Some(i) => (&s[..i], s[i..].to_string()),
        None => (s, String::new()),
    };
    if hostpart.is_empty() {
        bail!("No host given");
    }
    let (user, hostport) = match hostpart.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()), h),
        None => (None, hostpart),
    };
    // [ipv6]:port, host:port
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or_else(|| anyhow!("bad IPv6 address"))?;
        (h.to_string(), after.strip_prefix(':').and_then(|p| p.parse().ok()))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h.to_string(), Some(p.parse::<u16>().context("bad port")?)),
            _ => (hostport.to_string(), None),
        }
    };
    let cfg = ssh_config(&host);
    let user = user.or(cfg.user).or_else(|| default_user.map(String::from)).unwrap_or_else(crate::util::current_user);
    let user = percent_decode(&user);
    Ok(Target {
        alias: host.clone(),
        host: cfg.hostname.unwrap_or(host),
        user,
        port: port.or(cfg.port).unwrap_or(22),
        identity: cfg.identity,
        path: percent_decode(&path),
    })
}

fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|d| d as u8)
    }
    // Work on bytes: slicing the &str at byte offsets would panic inside multi-byte
    // characters (found by the sftp_target fuzzer).
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let (Some(&h), Some(&l)) = (b.get(i + 1), b.get(i + 2))
            && let (Some(h), Some(l)) = (hex(h), hex(l))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod percent_tests {
    use super::percent_decode;

    #[test]
    fn decodes_and_never_panics() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("end%41"), "endA");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("%\u{e9}\u{e9}"), "%\u{e9}\u{e9}");
        assert_eq!(percent_decode("%zz"), "%zz");
    }
}

// ---------------------------------------------------------------- connection

pub struct SftpFs {
    sftp: Sftp,
    // Keep the session alive as long as the SFTP channel.
    _sess: Session,
    user: String,
    alias: String,
    port: u16,
}

fn known_hosts_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ssh/known_hosts"))
}

fn sha256_fingerprint(sess: &Session) -> String {
    match sess.host_key_hash(ssh2::HashType::Sha256) {
        Some(h) => crate::commands::base64(h).trim_end_matches('=').to_string(),
        None => "?".into(),
    }
}

fn verify_host(sess: &Session, t: &Target, accept_new: bool) -> Result<()> {
    let (key, ktype) = sess.host_key().ok_or_else(|| anyhow!("server sent no host key"))?;
    let key = key.to_vec();
    let mut kh = sess.known_hosts()?;
    let path = known_hosts_path();
    if let Some(p) = &path
        && p.exists()
    {
        let _ = kh.read_file(p, KnownHostFileKind::OpenSSH);
    }
    match kh.check_port(&t.host, t.port, &key) {
        CheckResult::Match => Ok(()),
        CheckResult::Mismatch => bail!(
            "WARNING: the host key for {}:{} does NOT match ~/.ssh/known_hosts.\n\
             Someone could be intercepting the connection. Refusing to connect.\n\
             Fingerprint (SHA256): {}",
            t.host,
            t.port,
            sha256_fingerprint(sess)
        ),
        CheckResult::NotFound | CheckResult::Failure => {
            if !accept_new {
                return Err(
                    UnknownHost { host: t.host.clone(), port: t.port, fingerprint: sha256_fingerprint(sess) }.into()
                );
            }
            let entry = if t.port == 22 { t.host.clone() } else { format!("[{}]:{}", t.host, t.port) };
            kh.add(&entry, &key, "added by pairdeck", ktype.into())?;
            if let Some(p) = path {
                if let Some(d) = p.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                kh.write_file(&p, KnownHostFileKind::OpenSSH).context("cannot update ~/.ssh/known_hosts")?;
            }
            Ok(())
        }
    }
}

fn authenticate(sess: &Session, t: &Target, password: Option<&str>) -> Result<()> {
    let user = t.user.as_str();
    let methods = sess.auth_methods(user).unwrap_or("publickey,password").to_string();
    if sess.authenticated() {
        return Ok(());
    }
    if methods.contains("publickey") {
        let _ = sess.userauth_agent(user);
        if sess.authenticated() {
            return Ok(());
        }
        let home = crate::util::home();
        let mut keys = t.identity.clone();
        for k in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            keys.push(home.join(".ssh").join(k));
        }
        for k in keys {
            if k.exists() {
                let pubk = PathBuf::from(format!("{}.pub", k.display()));
                let pubk = pubk.exists().then_some(pubk);
                if sess.userauth_pubkey_file(user, pubk.as_deref(), &k, None).is_ok() && sess.authenticated() {
                    return Ok(());
                }
            }
        }
    }
    if let Some(pw) = password.filter(|p| !p.is_empty()) {
        if methods.contains("password") && sess.userauth_password(user, pw).is_ok() && sess.authenticated() {
            return Ok(());
        }
        if methods.contains("keyboard-interactive") {
            struct Pw<'a>(&'a str);
            impl ssh2::KeyboardInteractivePrompt for Pw<'_> {
                fn prompt<'b>(&mut self, _u: &str, _i: &str, prompts: &[ssh2::Prompt<'b>]) -> Vec<String> {
                    prompts.iter().map(|_| self.0.to_string()).collect()
                }
            }
            if sess.userauth_keyboard_interactive(user, &mut Pw(pw)).is_ok() && sess.authenticated() {
                return Ok(());
            }
        }
        bail!("Authentication failed for {}@{} (wrong password?)", t.user, t.host);
    }
    Err(NeedPassword(format!("{}@{}", t.user, t.host)).into())
}

/// Connect, verify, authenticate and open the SFTP channel.
pub fn connect(t: &Target, password: Option<&str>, accept_new: bool) -> Result<(SftpFs, PathBuf)> {
    let addr = (t.host.as_str(), t.port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {}", t.host))?
        .next()
        .ok_or_else(|| anyhow!("cannot resolve {}", t.host))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
        .with_context(|| format!("cannot connect to {}:{}", t.host, t.port))?;
    let _ = tcp.set_nodelay(true);
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.set_timeout(30_000);
    sess.handshake().context("SSH handshake failed")?;
    verify_host(&sess, t, accept_new)?;
    authenticate(&sess, t, password)?;
    sess.set_keepalive(true, 30);
    let sftp = sess.sftp().context("cannot start the SFTP subsystem")?;
    let start = if t.path.is_empty() || t.path == "/~" || t.path == "/~/" {
        sftp.realpath(Path::new(".")).unwrap_or_else(|_| PathBuf::from("/"))
    } else if let Some(rest) = t.path.strip_prefix("/~/") {
        sftp.realpath(Path::new(".")).unwrap_or_else(|_| PathBuf::from("/")).join(rest)
    } else {
        PathBuf::from(&t.path)
    };
    let fs = SftpFs { sftp, _sess: sess, user: t.user.clone(), alias: t.alias.clone(), port: t.port };
    Ok((fs, normalize(&start)))
}

fn to_time(t: Option<u64>) -> Option<SystemTime> {
    t.map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

fn entry_from(name: String, st: &FileStat) -> Entry {
    let mode = st.perm.unwrap_or(0);
    let kind = match mode & 0o170000 {
        0o040000 => EntryKind::Dir,
        0o120000 => EntryKind::Symlink { target: String::new(), to_dir: false, broken: false },
        0o100000 | 0 => EntryKind::File,
        _ => EntryKind::Other,
    };
    let mut e = Entry::new(name, kind);
    e.size = st.size.unwrap_or(0);
    e.mode = mode;
    e.uid = st.uid.unwrap_or(0);
    e.gid = st.gid.unwrap_or(0);
    e.mtime = to_time(st.mtime);
    e.atime = to_time(st.atime);
    e
}

impl SftpFs {
    fn finish_link(&self, path: &Path, e: &mut Entry) {
        if !e.is_symlink() {
            return;
        }
        let target = self.sftp.readlink(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        match self.sftp.stat(path) {
            Ok(st) => {
                let to_dir = st.perm.unwrap_or(0) & 0o170000 == 0o040000;
                e.size = st.size.unwrap_or(e.size);
                e.kind = EntryKind::Symlink { target, to_dir, broken: false };
            }
            Err(_) => e.kind = EntryKind::Symlink { target, to_dir: false, broken: true },
        }
    }
}

fn err(op: &str, p: &Path, e: ssh2::Error) -> anyhow::Error {
    anyhow!("{op} {}: {}", p.display(), e.message())
}

impl Vfs for SftpFs {
    fn scheme(&self) -> &'static str {
        "sftp"
    }

    fn display(&self, path: &Path) -> String {
        let port = if self.port == 22 { String::new() } else { format!(":{}", self.port) };
        format!("sftp://{}@{}{}{}", self.user, self.alias, port, path.display())
    }

    fn writable(&self) -> bool {
        true
    }

    fn list(&mut self, dir: &Path) -> Result<Vec<Entry>> {
        let items = self.sftp.readdir(dir).map_err(|e| err("cannot read", dir, e))?;
        let mut out = Vec::with_capacity(items.len());
        for (p, st) in items {
            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if name == "." || name == ".." || name.is_empty() {
                continue;
            }
            let mut e = entry_from(name, &st);
            self.finish_link(&p, &mut e);
            out.push(e);
        }
        Ok(out)
    }

    fn stat(&mut self, path: &Path) -> Result<Entry> {
        let st = self.sftp.lstat(path).map_err(|e| err("stat", path, e))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
        let mut e = entry_from(name, &st);
        self.finish_link(path, &mut e);
        Ok(e)
    }

    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>> {
        let f = self.sftp.open(path).map_err(|e| err("open", path, e))?;
        Ok(Box::new(f))
    }

    fn create_write(&mut self, path: &Path, mode: u32) -> Result<Box<dyn Write + Send>> {
        let mode = if mode == 0 { 0o644 } else { mode & 0o7777 };
        let f = self
            .sftp
            .open_mode(path, OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE, mode as i32, OpenType::File)
            .map_err(|e| err("create", path, e))?;
        Ok(Box::new(f))
    }

    fn mkdir(&mut self, path: &Path, mode: u32) -> Result<()> {
        let mode = if mode == 0 { 0o755 } else { mode & 0o7777 };
        self.sftp.mkdir(path, mode as i32).map_err(|e| err("mkdir", path, e))
    }

    fn remove_file(&mut self, path: &Path) -> Result<()> {
        self.sftp.unlink(path).map_err(|e| err("delete", path, e))
    }

    fn remove_dir(&mut self, path: &Path) -> Result<()> {
        self.sftp.rmdir(path).map_err(|e| err("rmdir", path, e))
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        // Plain rename first (fails on existing targets with many servers), then with overwrite flags.
        if self.sftp.rename(from, to, Some(ssh2::RenameFlags::NATIVE)).is_ok() {
            return Ok(());
        }
        let first = match self.sftp.rename(
            from,
            to,
            Some(ssh2::RenameFlags::OVERWRITE | ssh2::RenameFlags::ATOMIC | ssh2::RenameFlags::NATIVE),
        ) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        // SFTP v3 servers (OpenSSH) refuse to rename onto an existing file and libssh2
        // has no posix-rename extension: replace a non-directory target explicitly.
        match self.sftp.lstat(to) {
            Ok(st) if st.perm.unwrap_or(0) & 0o170000 != 0o040000 && self.sftp.lstat(from).is_ok() => {
                self.sftp.unlink(to).map_err(|e| err("rename (replace)", to, e))?;
                self.sftp.rename(from, to, Some(ssh2::RenameFlags::NATIVE)).map_err(|e| err("rename", from, e))
            }
            _ => Err(err("rename", from, first)),
        }
    }

    fn set_mode(&mut self, path: &Path, mode: u32) -> Result<()> {
        let st = FileStat { size: None, uid: None, gid: None, perm: Some(mode & 0o7777), atime: None, mtime: None };
        self.sftp.setstat(path, st).map_err(|e| err("chmod", path, e))
    }

    fn symlink(&mut self, target: &str, link: &Path) -> Result<()> {
        // libssh2 argument order matches OpenSSH's sftp-server: (points-to, link path).
        self.sftp.symlink(Path::new(target), link).map_err(|e| err("symlink", link, e))
    }

    fn set_mtime(&mut self, path: &Path, mtime: SystemTime) -> Result<()> {
        let secs = mtime.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let st = FileStat { size: None, uid: None, gid: None, perm: None, atime: Some(secs), mtime: Some(secs) };
        self.sftp.setstat(path, st).map_err(|e| err("set time", path, e))
    }
}

fn location(fs: SftpFs, path: PathBuf) -> Location {
    Location { vfs: VfsHandle::new(Box::new(fs)), path }
}

/// `cd sftp://user@host:port/path`: agent / key auth only.
pub fn connect_url(url: &str) -> Result<Location> {
    let t = parse_target(url, None)?;
    match connect(&t, None, false) {
        Ok((fs, path)) => Ok(location(fs, path)),
        Err(e) if e.downcast_ref::<UnknownHost>().is_some() => {
            Err(anyhow!("{e}\nUse the SFTP link dialog (Left/Right menu) to review and accept the host key."))
        }
        Err(e) => Err(e),
    }
}

pub fn connect_dialog(app: &mut App) {
    let hist = app.state.input_hist("sftp_host").clone();
    let last = hist.last().cloned().unwrap_or_default();
    let d = Form::new("SFTP link")
        .width(60)
        .input_hist("host", "Host (user@host:port or ssh config alias):", &last, "sftp_host", hist)
        .input("path", "Remote directory (empty = home):", "")
        .password("pw", "Password (only used if key authentication fails):")
        .buttons(&["Connect", "Cancel"], 0)
        .on_submit(|app, r| {
            if r.button != 0 || r.text("host").trim().is_empty() {
                return;
            }
            let mut input = r.text("host").trim().to_string();
            let path = r.text("path").trim().to_string();
            if !path.is_empty() {
                let p = if path.starts_with('/') { path } else { format!("/~/{path}") };
                input = format!("{}{p}", input.trim_end_matches('/'));
            }
            let pw = r.text("pw");
            start_connect(app, input, pw, false);
        });
    app.push_dialog(Box::new(d));
}

fn start_connect(app: &mut App, input: String, password: String, accept_new: bool) {
    let default_user = app.cfg.ssh_default_user.clone();
    let t = match parse_target(&input, default_user.as_deref()) {
        Ok(t) => t,
        Err(e) => {
            app.error(format!("{e:#}"));
            return;
        }
    };
    app.info(format!("Connecting to {}@{}:{}...", t.user, t.host, t.port));
    let tx = app.tx.clone();
    std::thread::spawn(move || {
        let pw = if password.is_empty() { None } else { Some(password.as_str()) };
        let res = connect(&t, pw, accept_new);
        let res = res.map(|(fs, path)| location(fs, path));
        let res: std::result::Result<Location, (String, Option<String>)> = res.map_err(|e| {
            let fp = e.downcast_ref::<UnknownHost>().map(|u| u.to_string());
            (format!("{e:#}"), fp)
        });
        crate::app::call(&tx, move |app| match res {
            Ok(loc) => {
                let disp = loc.display();
                if let Err(e) = app.panel_mut().enter_vfs(loc) {
                    app.error(format!("{e:#}"));
                    return;
                }
                crate::config::State::push_history(&mut app.state.dir_history, &disp);
                app.setup_watcher();
                app.info(format!("Connected: {disp}"));
            }
            Err((_, Some(question))) => {
                let text = format!("{question}\n\nAdd it to ~/.ssh/known_hosts and connect?");
                app.push_dialog(crate::dialog::confirm("Unknown host", &text, move |app| {
                    start_connect(app, input, password, true);
                }));
            }
            Err((msg, None)) => app.error(format!("SFTP connection failed:\n{msg}")),
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_targets() {
        let t = parse_target("sftp://bob@example.com:2222/var/log", None).unwrap();
        assert_eq!(
            (t.user.as_str(), t.host.as_str(), t.port, t.path.as_str()),
            ("bob", "example.com", 2222, "/var/log")
        );
        let t = parse_target("alice@[::1]:2200", None).unwrap();
        assert_eq!((t.host.as_str(), t.port, t.path.as_str()), ("::1", 2200, ""));
        let t = parse_target("ssh://x%40y@h/", None).unwrap();
        assert_eq!(t.user, "x@y");
        assert_eq!(t.port, 22);
    }
}
