//! SFTP VFS over russh (pure Rust SSH, no OpenSSL): agent / key / password auth,
//! ~/.ssh/config aliases and known_hosts verification.

use super::{Entry, EntryKind, Location, Vfs, VfsHandle, normalize};
use crate::app::App;
use crate::dialog::Form;
use anyhow::{Context, Result, anyhow, bail};
use russh::client::{self, AuthResult, KeyboardInteractiveAuthResponse};
use russh::keys::PublicKeyOrCertificate;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh::{MethodKind, MethodSet};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::fs::{File, Metadata};
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::runtime::Runtime;
use tokio_util::io::SyncIoBridge;

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

/// An SFTP session. russh is async: each connection owns a small tokio runtime
/// that drives the SSH transport, and the blocking `Vfs` calls wait on it. Readers
/// and writers handed out keep the runtime alive, so they outlive a closed panel.
pub struct SftpFs {
    sftp: SftpSession,
    // Keep the SSH connection open as long as the SFTP channel.
    _ssh: client::Handle<HostCheck>,
    user: String,
    alias: String,
    port: u16,
    // Declared last: dropped after the session and connection that run on it.
    rt: Arc<Runtime>,
}

fn known_hosts_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".ssh/known_hosts"))
}

fn sha256_fingerprint(key: &PublicKey) -> String {
    let fp = key.fingerprint(HashAlg::Sha256).to_string();
    fp.trim_start_matches("SHA256:").to_string()
}

/// What the host key check found, read back after a refused handshake.
#[derive(Default)]
enum HostVerdict {
    #[default]
    Pending,
    Unknown(String),
    Mismatch(String),
    Error(String),
}

/// russh client handler: checks the server's host key against ~/.ssh/known_hosts.
struct HostCheck {
    host: String,
    port: u16,
    accept_new: bool,
    verdict: Arc<Mutex<HostVerdict>>,
}

impl HostCheck {
    fn decide(&self, key: &PublicKey) -> HostVerdict {
        let Some(path) = known_hosts_path() else {
            return HostVerdict::Error("no home directory for ~/.ssh/known_hosts".into());
        };
        let fp = sha256_fingerprint(key);
        let known = if path.exists() {
            russh::keys::check_known_hosts_path(&self.host, self.port, key, &path)
        } else {
            Ok(false)
        };
        match known {
            Ok(true) => HostVerdict::Pending,
            Err(russh::keys::Error::KeyChanged { .. }) => HostVerdict::Mismatch(fp),
            Ok(false) | Err(_) if !self.accept_new => HostVerdict::Unknown(fp),
            Ok(false) | Err(_) => {
                match russh::keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, &path) {
                    Ok(()) => HostVerdict::Pending,
                    Err(e) => HostVerdict::Error(format!("cannot update ~/.ssh/known_hosts: {e}")),
                }
            }
        }
    }
}

impl client::Handler for HostCheck {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let verdict = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => self.decide(key),
            PublicKeyOrCertificate::Certificate(_) => {
                HostVerdict::Error("the server presented a host certificate; only plain host keys are supported".into())
            }
        };
        let ok = matches!(verdict, HostVerdict::Pending);
        *self.verdict.lock().unwrap_or_else(|e| e.into_inner()) = verdict;
        Ok(ok)
    }
}

fn host_error(t: &Target, verdict: HostVerdict, e: russh::Error) -> anyhow::Error {
    match verdict {
        HostVerdict::Unknown(fingerprint) => UnknownHost { host: t.host.clone(), port: t.port, fingerprint }.into(),
        HostVerdict::Mismatch(fp) => anyhow!(
            "WARNING: the host key for {}:{} does NOT match ~/.ssh/known_hosts.\n\
             Someone could be intercepting the connection. Refusing to connect.\n\
             Fingerprint (SHA256): {fp}",
            t.host,
            t.port
        ),
        HostVerdict::Error(msg) => anyhow!(msg),
        HostVerdict::Pending => anyhow!(e).context("SSH handshake failed"),
    }
}

fn has(methods: &MethodSet, m: MethodKind) -> bool {
    methods.contains(&m)
}

async fn authenticate(ssh: &mut client::Handle<HostCheck>, t: &Target, password: Option<&str>) -> Result<()> {
    let user = t.user.as_str();
    // "none" either logs in (rare) or tells us which methods the server accepts.
    let methods = match ssh.authenticate_none(user).await? {
        AuthResult::Success => return Ok(()),
        AuthResult::Failure { remaining_methods, .. } => remaining_methods,
    };
    if has(&methods, MethodKind::PublicKey) {
        let rsa_hash = ssh.best_supported_rsa_hash().await.ok().flatten().flatten();
        if let Ok(mut agent) = AgentClient::connect_env().await
            && let Ok(ids) = agent.request_identities().await
        {
            for id in ids {
                let key = id.public_key().into_owned();
                if let Ok(AuthResult::Success) = ssh.authenticate_publickey_with(user, key, rsa_hash, &mut agent).await
                {
                    return Ok(());
                }
            }
        }
        let home = crate::util::home();
        let mut keys = t.identity.clone();
        for k in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            keys.push(home.join(".ssh").join(k));
        }
        for k in keys {
            // Keys with a passphrase are skipped: use ssh-agent for those.
            let Ok(key) = russh::keys::load_secret_key(&k, None) else { continue };
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash);
            if let Ok(AuthResult::Success) = ssh.authenticate_publickey(user, key).await {
                return Ok(());
            }
        }
    }
    if let Some(pw) = password.filter(|p| !p.is_empty()) {
        if has(&methods, MethodKind::Password)
            && let Ok(AuthResult::Success) = ssh.authenticate_password(user, pw).await
        {
            return Ok(());
        }
        if has(&methods, MethodKind::KeyboardInteractive) {
            let mut reply = ssh.authenticate_keyboard_interactive_start(user, None).await;
            // Answer every prompt with the password; servers may send empty rounds too.
            for _ in 0..5 {
                match reply {
                    Ok(KeyboardInteractiveAuthResponse::Success) => return Ok(()),
                    Ok(KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. }) => {
                        let answers = prompts.iter().map(|_| pw.to_string()).collect();
                        reply = ssh.authenticate_keyboard_interactive_respond(answers).await;
                    }
                    _ => break,
                }
            }
        }
        bail!("Authentication failed for {}@{} (wrong password?)", t.user, t.host);
    }
    Err(NeedPassword(format!("{}@{}", t.user, t.host)).into())
}

/// Connect, verify, authenticate and open the SFTP channel.
pub fn connect(t: &Target, password: Option<&str>, accept_new: bool) -> Result<(SftpFs, PathBuf)> {
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("pairdeck-ssh")
            .enable_all()
            .build()
            .context("cannot start the SSH runtime")?,
    );
    let verdict = Arc::new(Mutex::new(HostVerdict::Pending));
    let handler = HostCheck { host: t.host.clone(), port: t.port, accept_new, verdict: verdict.clone() };
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        nodelay: true,
        ..Default::default()
    });
    let (ssh, sftp, start) = rt.block_on(async {
        let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((t.host.as_str(), t.port)))
            .await
            .map_err(|_| anyhow!("connection timed out"))
            .and_then(|r| r.map_err(anyhow::Error::from))
            .with_context(|| format!("cannot connect to {}:{}", t.host, t.port))?;
        let handshake = tokio::time::timeout(Duration::from_secs(30), client::connect_stream(config, tcp, handler));
        let mut ssh = match handshake.await {
            Err(_) => bail!("SSH handshake with {}:{} timed out", t.host, t.port),
            Ok(Ok(ssh)) => ssh,
            Ok(Err(e)) => {
                let v = std::mem::take(&mut *verdict.lock().unwrap_or_else(|e| e.into_inner()));
                return Err(host_error(t, v, e));
            }
        };
        authenticate(&mut ssh, t, password).await?;
        let channel = ssh.channel_open_session().await.context("cannot open an SSH channel")?;
        channel.request_subsystem(true, "sftp").await.context("cannot start the SFTP subsystem")?;
        let sftp = SftpSession::new(channel.into_stream()).await.context("cannot start the SFTP subsystem")?;
        sftp.set_timeout(30);
        let home = || async { sftp.canonicalize(".").await.unwrap_or_else(|_| "/".into()) };
        let start = if t.path.is_empty() || t.path == "/~" || t.path == "/~/" {
            PathBuf::from(home().await)
        } else if let Some(rest) = t.path.strip_prefix("/~/") {
            PathBuf::from(home().await).join(rest)
        } else {
            PathBuf::from(&t.path)
        };
        Ok((ssh, sftp, start))
    })?;
    let fs = SftpFs { sftp, _ssh: ssh, user: t.user.clone(), alias: t.alias.clone(), port: t.port, rt };
    Ok((fs, normalize(&start)))
}

fn to_time(t: Option<u32>) -> Option<SystemTime> {
    t.map(|s| UNIX_EPOCH + Duration::from_secs(u64::from(s)))
}

fn entry_from(name: String, st: &Metadata) -> Entry {
    let mode = st.permissions.unwrap_or(0);
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

fn sp(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn err(op: &str, p: &Path, e: SftpError) -> anyhow::Error {
    anyhow!("{op} {}: {e}", p.display())
}

/// Blocking reader over an SFTP file.
struct SftpReader {
    inner: SyncIoBridge<File>,
    _rt: Arc<Runtime>,
}

impl Read for SftpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

/// Blocking writer over an SFTP file. Writes are pipelined; `flush` waits for the
/// server to confirm them and dropping closes the remote handle.
struct SftpWriter {
    inner: Option<SyncIoBridge<File>>,
    rt: Arc<Runtime>,
}

impl Write for SftpWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.as_mut().map_or(Ok(0), |w| w.write(buf))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.as_mut().map_or(Ok(()), |w| w.flush())
    }
}

impl Drop for SftpWriter {
    fn drop(&mut self) {
        if let Some(w) = self.inner.take() {
            let mut f = w.into_inner();
            let _ = self.rt.block_on(f.shutdown());
        }
    }
}

impl SftpFs {
    fn run<T>(&self, f: impl Future<Output = T>) -> T {
        self.rt.block_on(f)
    }

    fn finish_link(&self, path: &Path, e: &mut Entry) {
        if !e.is_symlink() {
            return;
        }
        let target = self.run(self.sftp.read_link(sp(path))).unwrap_or_default();
        match self.run(self.sftp.metadata(sp(path))) {
            Ok(st) => {
                let to_dir = st.permissions.unwrap_or(0) & 0o170000 == 0o040000;
                e.size = st.size.unwrap_or(e.size);
                e.kind = EntryKind::Symlink { target, to_dir, broken: false };
            }
            Err(_) => e.kind = EntryKind::Symlink { target, to_dir: false, broken: true },
        }
    }

    fn lstat_mode(&self, p: &Path) -> Option<u32> {
        self.run(self.sftp.symlink_metadata(sp(p))).ok().map(|m| m.permissions.unwrap_or(0))
    }
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
        let items = self.run(self.sftp.read_dir(sp(dir))).map_err(|e| err("cannot read", dir, e))?;
        let mut out = Vec::new();
        for item in items {
            let name = item.file_name();
            if name == "." || name == ".." || name.is_empty() {
                continue;
            }
            let mut e = entry_from(name, &item.metadata());
            self.finish_link(&dir.join(&e.name), &mut e);
            out.push(e);
        }
        Ok(out)
    }

    fn stat(&mut self, path: &Path) -> Result<Entry> {
        let st = self.run(self.sftp.symlink_metadata(sp(path))).map_err(|e| err("stat", path, e))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
        let mut e = entry_from(name, &st);
        self.finish_link(path, &mut e);
        Ok(e)
    }

    fn open_read(&mut self, path: &Path) -> Result<Box<dyn Read + Send>> {
        let f = self.run(self.sftp.open(sp(path))).map_err(|e| err("open", path, e))?;
        let inner = SyncIoBridge::new_with_handle(f, self.rt.handle().clone());
        Ok(Box::new(SftpReader { inner, _rt: self.rt.clone() }))
    }

    fn create_write(&mut self, path: &Path, mode: u32) -> Result<Box<dyn Write + Send>> {
        let mut attrs = FileAttributes::empty();
        attrs.permissions = Some(if mode == 0 { 0o644 } else { mode & 0o7777 });
        let flags = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE;
        let f = self
            .run(self.sftp.open_with_flags_and_attributes(sp(path), flags, attrs))
            .map_err(|e| err("create", path, e))?;
        let inner = SyncIoBridge::new_with_handle(f, self.rt.handle().clone());
        Ok(Box::new(SftpWriter { inner: Some(inner), rt: self.rt.clone() }))
    }

    fn mkdir(&mut self, path: &Path, mode: u32) -> Result<()> {
        self.run(self.sftp.create_dir(sp(path))).map_err(|e| err("mkdir", path, e))?;
        if mode != 0 && mode & 0o7777 != 0o755 {
            let _ = self.set_mode(path, mode);
        }
        Ok(())
    }

    fn remove_file(&mut self, path: &Path) -> Result<()> {
        self.run(self.sftp.remove_file(sp(path))).map_err(|e| err("delete", path, e))
    }

    fn remove_dir(&mut self, path: &Path) -> Result<()> {
        self.run(self.sftp.remove_dir(sp(path))).map_err(|e| err("rmdir", path, e))
    }

    fn rename(&mut self, from: &Path, to: &Path) -> Result<()> {
        let first = match self.run(self.sftp.rename(sp(from), sp(to))) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        // SFTP v3 servers (OpenSSH) refuse to rename onto an existing file: replace a
        // non-directory target explicitly.
        match self.lstat_mode(to) {
            Some(m) if m & 0o170000 != 0o040000 && self.lstat_mode(from).is_some() => {
                self.run(self.sftp.remove_file(sp(to))).map_err(|e| err("rename (replace)", to, e))?;
                self.run(self.sftp.rename(sp(from), sp(to))).map_err(|e| err("rename", from, e))
            }
            _ => Err(err("rename", from, first)),
        }
    }

    fn set_mode(&mut self, path: &Path, mode: u32) -> Result<()> {
        let mut attrs = FileAttributes::empty();
        attrs.permissions = Some(mode & 0o7777);
        self.run(self.sftp.set_metadata(sp(path), attrs)).map_err(|e| err("chmod", path, e))
    }

    fn symlink(&mut self, target: &str, link: &Path) -> Result<()> {
        // OpenSSH's sftp-server reads the two SSH_FXP_SYMLINK fields swapped against
        // the draft: send (points-to, link path), which is what it expects.
        self.run(self.sftp.symlink(target, sp(link))).map_err(|e| err("symlink", link, e))
    }

    fn set_mtime(&mut self, path: &Path, mtime: SystemTime) -> Result<()> {
        let secs = mtime.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let secs = u32::try_from(secs).unwrap_or(u32::MAX);
        let mut attrs = FileAttributes::empty();
        attrs.atime = Some(secs);
        attrs.mtime = Some(secs);
        self.run(self.sftp.set_metadata(sp(path), attrs)).map_err(|e| err("set time", path, e))
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
