//! SFTP backend against a real, throwaway OpenSSH server.
//!
//! Ignored by default. Run with `scripts/test-sftp.sh` or
//! `cargo test --test sftp_integration -- --ignored --nocapture`.
//!
//! By default the test starts a non-root `sshd` on 127.0.0.1 with freshly
//! generated host and client keys in a temp directory (nothing touches the real
//! ~/.ssh: HOME is pointed at the temp dir and SSH_AUTH_SOCK is unset).
//! Environment:
//!   PAIRDECK_SSHD               path to sshd (default: /opt/homebrew/sbin/sshd, /usr/sbin/sshd, PATH)
//!   PAIRDECK_TEST_SFTP_HOST     use an already running server instead (e.g. Docker, see the script)
//!   PAIRDECK_TEST_SFTP_PORT     its port
//!   PAIRDECK_TEST_SFTP_USER     its user
//!   PAIRDECK_TEST_SFTP_KEY      private key accepted by that server
//!   PAIRDECK_TEST_SFTP_DIR      writable directory on that server (default: its home)

#![allow(unsafe_code)] // std::env::set_var / remove_var for the isolated HOME

use pairdeck::app::AppEvent;
use pairdeck::ops::{self, CopyOpts, JobMsg, JobSpec, Overwrite};
use pairdeck::vfs::sftp::{self, UnknownHost};
use pairdeck::vfs::{EntryKind, Location, VfsHandle};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Option<Child>,
    host: String,
    port: u16,
    user: String,
    dir: PathBuf,
    /// Writable base directory on the server (None = the login home).
    remote_dir: Option<PathBuf>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn find_sshd() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PAIRDECK_SSHD") {
        return Some(PathBuf::from(p));
    }
    ["/opt/homebrew/sbin/sshd", "/usr/local/sbin/sshd", "/usr/sbin/sshd"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .or_else(|| which::which("sshd").ok())
}

fn keygen(path: &Path) {
    let st = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(path)
        .stdout(Stdio::null())
        .status()
        .expect("ssh-keygen");
    assert!(st.success(), "ssh-keygen failed");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Isolated HOME with the client key at ~/.ssh/id_ed25519 and no known_hosts.
fn isolated_home(dir: &Path, key: Option<&Path>) -> PathBuf {
    let home = dir.join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    match key {
        Some(k) => {
            std::fs::copy(k, home.join(".ssh/id_ed25519")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home.join(".ssh/id_ed25519"), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        None => keygen(&home.join(".ssh/id_ed25519")),
    }
    // SAFETY: this file has a single test; nothing reads the environment
    // concurrently while these variables are being set.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::remove_var("SSH_AUTH_SOCK");
    }
    home
}

fn start() -> Server {
    let dir = std::env::temp_dir().join(format!("pairdeck-sftp-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    if let Ok(host) = std::env::var("PAIRDECK_TEST_SFTP_HOST") {
        let key = std::env::var("PAIRDECK_TEST_SFTP_KEY").expect("PAIRDECK_TEST_SFTP_KEY");
        isolated_home(&dir, Some(Path::new(&key)));
        return Server {
            child: None,
            host,
            port: std::env::var("PAIRDECK_TEST_SFTP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(22),
            user: std::env::var("PAIRDECK_TEST_SFTP_USER").expect("PAIRDECK_TEST_SFTP_USER"),
            dir,
            remote_dir: std::env::var("PAIRDECK_TEST_SFTP_DIR").ok().map(PathBuf::from),
        };
    }

    let sshd = find_sshd().expect("no sshd found (set PAIRDECK_SSHD or PAIRDECK_TEST_SFTP_HOST)");
    let home = isolated_home(&dir, None);
    keygen(&dir.join("host_ed25519"));
    std::fs::copy(home.join(".ssh/id_ed25519.pub"), dir.join("authorized_keys")).unwrap();
    let port = free_port();
    let cfg = format!(
        "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_ed25519\nPidFile {d}/sshd.pid\n\
         AuthorizedKeysFile {d}/authorized_keys\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n\
         UsePAM no\nStrictModes no\nSubsystem sftp internal-sftp\nLogLevel ERROR\n",
        d = dir.display()
    );
    std::fs::write(dir.join("sshd_config"), cfg).unwrap();
    let child = Command::new(&sshd)
        .args(["-D", "-e", "-f"])
        .arg(dir.join("sshd_config"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start sshd");
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "sshd did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let user = std::env::var("USER").unwrap_or_else(|_| pairdeck::util::current_user());
    let remote_dir = Some(dir.join("remote"));
    Server { child: Some(child), host: "127.0.0.1".into(), port, user, dir, remote_dir }
}

fn wait_job(rx: &crossbeam_channel::Receiver<AppEvent>) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("job timed out") {
            AppEvent::Job(JobMsg::Finished { errors, cancelled, .. }) => {
                assert!(errors.is_empty() && !cancelled, "job failed: {errors:?}");
                return;
            }
            AppEvent::Job(JobMsg::Conflict { reply, .. }) => {
                let _ = reply.send(ops::ConflictChoice::Overwrite);
            }
            AppEvent::Job(JobMsg::Error { msg, .. }) => panic!("job error: {msg}"),
            _ => {}
        }
    }
}

fn read_all(v: &VfsHandle, p: &Path) -> Vec<u8> {
    let mut r = v.lock().open_read(p).unwrap();
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    buf
}

fn put(v: &VfsHandle, p: &Path, data: &[u8], mode: u32) {
    let mut w = v.lock().create_write(p, mode).unwrap();
    w.write_all(data).unwrap();
}

#[test]
#[ignore = "needs sshd; run scripts/test-sftp.sh"]
fn sftp_against_real_server() {
    let srv = start();
    let url = format!("sftp://{}@{}:{}/", srv.user, srv.host, srv.port);
    let t = sftp::parse_target(&url, None).unwrap();

    // 1. An unknown host key is reported, not silently accepted.
    let err = sftp::connect(&t, None, false).err().expect("unknown host must fail");
    assert!(err.downcast_ref::<UnknownHost>().is_some(), "expected UnknownHost, got {err:#}");
    // Accepting writes known_hosts in the isolated HOME; afterwards it verifies.
    sftp::connect(&t, None, true).expect("connect accepting the key");
    let kh = std::fs::read_to_string(std::env::var("HOME").unwrap() + "/.ssh/known_hosts").unwrap();
    let host_entry = if srv.port == 22 { srv.host.clone() } else { format!("[{}]:{}", srv.host, srv.port) };
    assert!(kh.contains(&host_entry), "known_hosts: {kh}");
    let (fs, home) = sftp::connect(&t, None, false).expect("connect with a known host");
    let v = VfsHandle::new(Box::new(fs));

    // 2. Basic operations in a scratch dir on the server.
    let base = match &srv.remote_dir {
        Some(d) if srv.child.is_some() => {
            std::fs::create_dir_all(d).unwrap();
            std::fs::canonicalize(d).unwrap()
        }
        Some(d) => d.clone(),
        None => home,
    }
    .join(format!("pairdeck-it-{}", std::process::id()));
    v.lock().mkdir(&base, 0o755).unwrap();
    let d = base.join("work");
    v.lock().mkdir(&d, 0o755).unwrap();
    put(&v, &d.join("a.txt"), b"hello sftp\n", 0o640);
    assert_eq!(read_all(&v, &d.join("a.txt")), b"hello sftp\n");
    let st = v.lock().stat(&d.join("a.txt")).unwrap();
    assert_eq!((st.size, st.mode & 0o777), (11, 0o640));
    // Overwriting a longer file with a shorter one truncates.
    put(&v, &d.join("a.txt"), b"hi", 0);
    assert_eq!(read_all(&v, &d.join("a.txt")), b"hi");
    v.lock().rename(&d.join("a.txt"), &d.join("b.txt")).unwrap();
    // Rename onto an existing file replaces it.
    put(&v, &d.join("c.txt"), b"other", 0);
    v.lock().rename(&d.join("c.txt"), &d.join("b.txt")).unwrap();
    assert_eq!(read_all(&v, &d.join("b.txt")), b"other");
    assert!(v.lock().stat(&d.join("c.txt")).is_err());
    v.lock().set_mode(&d.join("b.txt"), 0o600).unwrap();
    assert_eq!(v.lock().stat(&d.join("b.txt")).unwrap().mode & 0o777, 0o600);
    let t0 = std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    v.lock().set_mtime(&d.join("b.txt"), t0).unwrap();
    assert_eq!(v.lock().stat(&d.join("b.txt")).unwrap().mtime, Some(t0));
    v.lock().symlink("b.txt", &d.join("link")).unwrap();
    match v.lock().stat(&d.join("link")).unwrap().kind {
        EntryKind::Symlink { target, broken, .. } => assert_eq!((target.as_str(), broken), ("b.txt", false)),
        k => panic!("expected symlink, got {k:?}"),
    }
    v.lock().symlink("nowhere", &d.join("dangling")).unwrap();
    assert!(matches!(v.lock().stat(&d.join("dangling")).unwrap().kind, EntryKind::Symlink { broken: true, .. }));
    v.lock().symlink(".", &d.join("selfdir")).unwrap();
    assert!(v.lock().stat(&d.join("selfdir")).unwrap().is_dir(), "link to a dir behaves as a dir");
    let mut names: Vec<String> = v.lock().list(&d).unwrap().into_iter().map(|e| e.name).collect();
    names.sort();
    assert_eq!(names, ["b.txt", "dangling", "link", "selfdir"]);
    assert!(v.lock().stat(&d.join("missing")).is_err());
    assert!(v.lock().mkdir(&d, 0o755).is_err(), "mkdir on an existing dir fails");
    for n in ["link", "dangling", "selfdir", "b.txt"] {
        v.lock().remove_file(&d.join(n)).unwrap();
    }
    v.lock().remove_dir(&d).unwrap();

    // 3. Copy a tree local -> sftp -> local (move) through the real copy job.
    let src = srv.dir.join("src");
    std::fs::create_dir_all(src.join("tree/sub")).unwrap();
    std::fs::write(src.join("tree/one.txt"), b"one").unwrap();
    std::fs::write(src.join("tree/sub/big.bin"), vec![42u8; 3 * 1024 * 1024 + 7]).unwrap();
    std::os::unix::fs::symlink("one.txt", src.join("tree/ln")).unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let remote = Location { vfs: v.clone(), path: base.clone() };
    let copy = |src: Location, dst: Location, names: &[&str], remove_src: bool| JobSpec::Copy {
        src,
        names: names.iter().map(|s| s.to_string()).collect(),
        dst,
        target: None,
        opts: CopyOpts { overwrite: Overwrite::Ask, ..CopyOpts::default() },
        remove_src,
    };
    ops::spawn(copy(Location::local(&src), remote.clone(), &["tree"], false), tx.clone());
    wait_job(&rx);
    assert_eq!(read_all(&v, &base.join("tree/sub/big.bin")).len(), 3 * 1024 * 1024 + 7);
    assert!(v.lock().stat(&base.join("tree/ln")).unwrap().is_symlink());
    // Copying again triggers conflicts (answered "overwrite" by wait_job).
    ops::spawn(copy(Location::local(&src), remote.clone(), &["tree"], false), tx.clone());
    wait_job(&rx);
    let back = srv.dir.join("back");
    std::fs::create_dir_all(&back).unwrap();
    ops::spawn(copy(remote.clone(), Location::local(&back), &["tree"], true), tx.clone());
    wait_job(&rx);
    assert_eq!(std::fs::read(back.join("tree/one.txt")).unwrap(), b"one");
    assert_eq!(std::fs::read(back.join("tree/sub/big.bin")).unwrap().len(), 3 * 1024 * 1024 + 7);
    assert_eq!(std::fs::read_link(back.join("tree/ln")).unwrap(), Path::new("one.txt"));
    assert!(v.lock().stat(&base.join("tree")).is_err(), "move must remove the remote tree");

    // 4. Rename within the server (fast path) and a delete job.
    std::fs::create_dir_all(src.join("gone/x")).unwrap();
    std::fs::write(src.join("gone/x/f"), b"f").unwrap();
    ops::spawn(copy(Location::local(&src), remote.clone(), &["gone"], false), tx.clone());
    wait_job(&rx);
    ops::spawn(
        JobSpec::Copy {
            src: remote.clone(),
            names: vec!["gone".into()],
            dst: remote.clone(),
            target: Some("renamed".into()),
            opts: CopyOpts::default(),
            remove_src: true,
        },
        tx.clone(),
    );
    wait_job(&rx);
    assert_eq!(read_all(&v, &base.join("renamed/x/f")), b"f");
    ops::spawn(JobSpec::Delete { loc: remote, names: vec!["renamed".into()], trash: false }, tx);
    wait_job(&rx);
    assert!(v.lock().stat(&base.join("renamed")).is_err());
    v.lock().remove_dir(&base).unwrap();
}
