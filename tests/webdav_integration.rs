//! WebDAV backend against a real server.
//!
//! Ignored by default; run `scripts/test-remote.sh webdav` or
//! `cargo test --test webdav_integration -- --ignored --nocapture`.
//! Starts `rclone serve webdav` on a temp dir unless these are set:
//!   PAIRDECK_TEST_DAV_URL   dav://127.0.0.1:8080/some/writable/dir
//!   PAIRDECK_TEST_DAV_USER / PAIRDECK_TEST_DAV_PASS

#[path = "remote_common/mod.rs"]
mod common;

use pairdeck::vfs::webdav;
use std::path::PathBuf;

#[test]
#[ignore = "needs rclone or a WebDAV server; run scripts/test-remote.sh webdav"]
fn webdav_against_real_server() {
    let dir = common::scratch("dav");
    let (_proc, mut target) = match std::env::var("PAIRDECK_TEST_DAV_URL") {
        Ok(url) => {
            let mut t = webdav::parse_url(&url).unwrap();
            t.user = std::env::var("PAIRDECK_TEST_DAV_USER").ok().or(t.user);
            t.password = std::env::var("PAIRDECK_TEST_DAV_PASS").ok().or(t.password);
            (None, t)
        }
        Err(_) => {
            let root = dir.join("root");
            std::fs::create_dir_all(&root).unwrap();
            let (p, port) =
                common::rclone("webdav", &root, dir.join("rclone"), &["--user", "davuser", "--pass", "davpass"]);
            (Some(p), webdav::parse_url(&format!("dav://davuser:davpass@127.0.0.1:{port}/")).unwrap())
        }
    };
    // Wrong password is rejected.
    let good_pw = target.password.clone();
    target.password = Some("wrong".into());
    let err = webdav::connect(&target).err().expect("wrong password must fail");
    assert!(format!("{err:#}").contains("HTTP 401"), "{err:#}");
    target.password = good_pw;
    let loc = webdav::connect(&target).unwrap_or_else(|e| panic!("connect: {e:#}"));
    assert!(loc.display().starts_with("dav://"), "{}", loc.display());
    let base = PathBuf::from(&target.path).join(format!("pairdeck-it-{}", std::process::id()));
    common::conformance(&loc.vfs, &base, &dir, 5 << 20);
    let _ = std::fs::remove_dir_all(&dir);
}
