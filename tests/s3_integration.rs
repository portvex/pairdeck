//! S3 backend against a real S3-compatible server.
//!
//! Ignored by default; run `scripts/test-remote.sh s3` or
//! `cargo test --test s3_integration -- --ignored --nocapture`.
//! Starts `rclone serve s3` on a temp dir unless these are set (e.g. MinIO in Docker):
//!   PAIRDECK_TEST_S3_ENDPOINT  http://127.0.0.1:9000
//!   PAIRDECK_TEST_S3_BUCKET    an existing, empty-ish bucket
//!   PAIRDECK_TEST_S3_KEY / PAIRDECK_TEST_S3_SECRET

#[path = "remote_common/mod.rs"]
mod common;

use pairdeck::vfs::s3::{self, S3Config};
use std::path::Path;

#[test]
#[ignore = "needs rclone or an S3 server; run scripts/test-remote.sh s3"]
fn s3_against_real_server() {
    let dir = common::scratch("s3");
    let (_proc, cfg) = match std::env::var("PAIRDECK_TEST_S3_ENDPOINT") {
        Ok(endpoint) => (
            None,
            S3Config {
                bucket: std::env::var("PAIRDECK_TEST_S3_BUCKET").expect("PAIRDECK_TEST_S3_BUCKET"),
                endpoint: Some(endpoint),
                region: Some("us-east-1".into()),
                access_key: std::env::var("PAIRDECK_TEST_S3_KEY").ok(),
                secret_key: std::env::var("PAIRDECK_TEST_S3_SECRET").ok(),
                ..S3Config::default()
            },
        ),
        Err(_) => {
            let root = dir.join("root");
            std::fs::create_dir_all(root.join("testbucket")).unwrap();
            let (p, port) = common::rclone("s3", &root, dir.join("rclone"), &["--auth-key", "AKTEST,SKTEST"]);
            (
                Some(p),
                S3Config {
                    bucket: "testbucket".into(),
                    endpoint: Some(format!("http://127.0.0.1:{port}")),
                    region: Some("us-east-1".into()),
                    access_key: Some("AKTEST".into()),
                    secret_key: Some("SKTEST".into()),
                    ..S3Config::default()
                },
            )
        }
    };
    // Wrong credentials are rejected with a readable error.
    let bad = S3Config { secret_key: Some("wrong".into()), ..cfg.clone() };
    if cfg.access_key.is_some() {
        let err = s3::connect(&bad).err().expect("bad secret must fail");
        assert!(format!("{err:#}").contains("HTTP 403"), "{err:#}");
    }
    let loc = s3::connect(&cfg).unwrap_or_else(|e| panic!("connect: {e:#}"));
    assert_eq!(loc.display(), format!("s3://{}/", cfg.bucket));
    let base = Path::new("/").join(format!("pairdeck-it-{}", std::process::id()));
    // 70 MB: above the 64 MB threshold, so the copy job uses multipart upload.
    common::conformance(&loc.vfs, &base, &dir, 70 << 20);
    let _ = std::fs::remove_dir_all(&dir);
}
