//! Shared helpers for the remote backend integration tests.

#![allow(dead_code)]

use pairdeck::app::AppEvent;
use pairdeck::ops::{self, CopyOpts, JobMsg, JobSpec, Overwrite};
use pairdeck::vfs::{Location, VfsHandle};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A server process killed (and its data removed) on drop.
pub struct Proc {
    pub child: Option<Child>,
    pub dir: PathBuf,
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pairdeck-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

pub fn wait_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "server on port {port} did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Start `rclone serve <proto> <root> --addr 127.0.0.1:<port> <extra...>`.
pub fn rclone(proto: &str, root: &Path, dir: PathBuf, extra: &[&str]) -> (Proc, u16) {
    let port = free_port();
    let bin = std::env::var("PAIRDECK_RCLONE").unwrap_or_else(|_| "rclone".into());
    let child = Command::new(bin)
        .args(["serve", proto])
        .arg(root)
        .args(["--addr", &format!("127.0.0.1:{port}"), "--config", "/dev/null", "-q"])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start rclone (install it or set the PAIRDECK_TEST_* variables)");
    wait_port(port);
    (Proc { child: Some(child), dir }, port)
}

pub fn wait_job(rx: &crossbeam_channel::Receiver<AppEvent>) {
    let deadline = Instant::now() + Duration::from_secs(120);
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

pub fn read_all(v: &VfsHandle, p: &Path) -> Vec<u8> {
    let mut r = v.lock().open_read(p).unwrap_or_else(|e| panic!("open {}: {e:#}", p.display()));
    let mut buf = Vec::new();
    r.read_to_end(&mut buf).unwrap();
    buf
}

pub fn put(v: &VfsHandle, p: &Path, data: &[u8]) {
    let mut w = v.lock().create_write(p, 0o644).unwrap();
    w.write_all(data).unwrap();
    w.flush().unwrap_or_else(|e| panic!("upload {}: {e:#}", p.display()));
}

fn names(v: &VfsHandle, p: &Path) -> Vec<String> {
    let mut n: Vec<String> = v.lock().list(p).unwrap().into_iter().map(|e| e.name).collect();
    n.sort();
    n
}

fn copy(src: Location, dst: Location, names: &[&str], remove_src: bool) -> JobSpec {
    JobSpec::Copy {
        src,
        names: names.iter().map(|s| s.to_string()).collect(),
        dst,
        target: None,
        opts: CopyOpts { overwrite: Overwrite::Ask, ..CopyOpts::default() },
        remove_src,
    }
}

/// The same checks for every writable remote backend. `base` must not exist yet.
/// `big` is the size of the large file used to exercise streaming / multipart.
pub fn conformance(v: &VfsHandle, base: &Path, local_scratch: &Path, big: usize) {
    v.lock().mkdir(base, 0o755).unwrap_or_else(|e| panic!("mkdir {}: {e:#}", base.display()));
    assert!(v.lock().stat(base).unwrap().is_dir());
    let d = base.join("work dir");
    v.lock().mkdir(&d, 0o755).unwrap();
    assert!(names(v, &d).is_empty(), "new dir is empty");
    assert!(names(v, base).contains(&"work dir".to_string()), "mkdir shows up in the parent");
    put(v, &d.join("a.txt"), b"hello remote\n");
    assert_eq!(read_all(v, &d.join("a.txt")), b"hello remote\n");
    let st = v.lock().stat(&d.join("a.txt")).unwrap();
    assert_eq!((st.size, st.is_dir()), (13, false));
    assert!(st.mtime.is_some(), "mtime");
    put(v, &d.join("a.txt"), b"short");
    assert_eq!(read_all(v, &d.join("a.txt")), b"short", "overwrite truncates");
    put(v, &d.join("empty"), b"");
    assert_eq!(v.lock().stat(&d.join("empty")).unwrap().size, 0);
    put(v, &d.join("ü n+i%code.txt"), b"unicode");
    assert_eq!(read_all(v, &d.join("ü n+i%code.txt")), b"unicode");
    v.lock().rename(&d.join("a.txt"), &d.join("b.txt")).unwrap();
    assert!(v.lock().stat(&d.join("a.txt")).is_err());
    assert_eq!(read_all(v, &d.join("b.txt")), b"short");
    assert_eq!(names(v, &d), ["b.txt", "empty", "ü n+i%code.txt"]);
    assert!(v.lock().stat(&d.join("missing")).is_err());
    assert!(v.lock().remove_dir(&d).is_err(), "rmdir of a non-empty dir must fail");
    for n in ["b.txt", "empty", "ü n+i%code.txt"] {
        v.lock().remove_file(&d.join(n)).unwrap();
    }
    v.lock().remove_dir(&d).unwrap();
    assert!(v.lock().stat(&d).is_err(), "rmdir");

    // Tree copy local -> remote -> local (move), with conflicts on the second copy.
    let src = local_scratch.join("src");
    std::fs::create_dir_all(src.join("tree/sub")).unwrap();
    std::fs::write(src.join("tree/one.txt"), b"one").unwrap();
    let pattern: Vec<u8> = (0..big).map(|i| (i % 251) as u8).collect();
    std::fs::write(src.join("tree/sub/big.bin"), &pattern).unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let remote = Location { vfs: v.clone(), path: base.to_path_buf() };
    ops::spawn(copy(Location::local(&src), remote.clone(), &["tree"], false), tx.clone());
    wait_job(&rx);
    assert_eq!(v.lock().stat(&base.join("tree/sub/big.bin")).unwrap().size, big as u64);
    ops::spawn(copy(Location::local(&src), remote.clone(), &["tree"], false), tx.clone());
    wait_job(&rx);
    let back = local_scratch.join("back");
    std::fs::create_dir_all(&back).unwrap();
    ops::spawn(copy(remote.clone(), Location::local(&back), &["tree"], true), tx.clone());
    wait_job(&rx);
    assert_eq!(std::fs::read(back.join("tree/one.txt")).unwrap(), b"one");
    assert!(std::fs::read(back.join("tree/sub/big.bin")).unwrap() == pattern, "big file content");
    assert!(v.lock().stat(&base.join("tree")).is_err(), "move removed the remote tree");

    // Delete job.
    ops::spawn(copy(Location::local(&src), remote.clone(), &["tree"], false), tx.clone());
    wait_job(&rx);
    ops::spawn(JobSpec::Delete { loc: remote, names: vec!["tree".into()], trash: false }, tx);
    wait_job(&rx);
    assert!(v.lock().stat(&base.join("tree")).is_err());
    v.lock().remove_dir(base).unwrap();
}
