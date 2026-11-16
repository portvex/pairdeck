//! Tests for password-protected zip archives (included from archive.rs).

use super::*;
use crate::vfs::secrets;
use zip::unstable::write::FileOptionsExt;

fn read_all(fs: &mut Box<dyn Vfs>, p: &str) -> Result<String> {
    let mut s = String::new();
    fs.open_read(Path::new(p))?.read_to_string(&mut s)?;
    Ok(s)
}

fn make_encrypted_zip(path: &Path, pw: &str) {
    let mut zw = zip::ZipWriter::new(File::create(path).unwrap());
    let plain = zip::write::SimpleFileOptions::default();
    zw.start_file("plain.txt", plain).unwrap();
    zw.write_all(b"not secret").unwrap();
    for (name, mode) in [
        ("secret/aes128.txt", zip::AesMode::Aes128),
        ("secret/aes192.txt", zip::AesMode::Aes192),
        ("secret/aes256.txt", zip::AesMode::Aes256),
    ] {
        zw.start_file(name, plain.with_aes_encryption(mode, pw)).unwrap();
        zw.write_all(format!("data of {name}").as_bytes()).unwrap();
    }
    let zc = plain.with_deprecated_encryption(pw.as_bytes()).unwrap();
    zw.start_file("zipcrypto.txt", zc).unwrap();
    zw.write_all(b"legacy encryption").unwrap();
    zw.finish().unwrap();
}

#[test]
fn password_flow_for_aes_and_zipcrypto() {
    let dir = std::env::temp_dir().join(format!("pairdeck-zippw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let zip_path = dir.join("locked.zip");
    make_encrypted_zip(&zip_path, "correct horse");
    let parent = Location::local(&dir);

    let mut fs = open(&parent, &zip_path).unwrap();
    let list = fs.list(Path::new("/")).unwrap();
    let enc = |n: &str| list.iter().find(|e| e.name == n).map(|e| e.encrypted);
    assert_eq!(enc("plain.txt"), Some(false));
    assert_eq!(enc("zipcrypto.txt"), Some(true));
    assert!(fs.stat(Path::new("/secret/aes256.txt")).unwrap().encrypted);

    // Unencrypted entries read fine; encrypted ones ask for a password.
    assert_eq!(read_all(&mut fs, "/plain.txt").unwrap(), "not secret");
    let err = read_all(&mut fs, "/secret/aes128.txt").unwrap_err();
    assert!(secrets::password_required(&err).is_some_and(|p| !p.wrong), "{err:#}");
    assert!(fs.needs_password(&[PathBuf::from("/secret")]));
    assert!(fs.needs_password(&[PathBuf::from("/")]));
    assert!(!fs.needs_password(&[PathBuf::from("/plain.txt")]));

    // Wrong password is rejected and nothing is remembered.
    assert!(!fs.set_password("wrong").unwrap());
    assert!(fs.needs_password(&[PathBuf::from("/zipcrypto.txt")]));
    assert!(secrets::recall(&zip_path.display().to_string()).is_none());

    // Right password unlocks every encryption method.
    assert!(fs.set_password("correct horse").unwrap());
    assert!(!fs.needs_password(&[PathBuf::from("/")]));
    for n in ["aes128", "aes192", "aes256"] {
        assert_eq!(read_all(&mut fs, &format!("/secret/{n}.txt")).unwrap(), format!("data of secret/{n}.txt"));
    }
    assert_eq!(read_all(&mut fs, "/zipcrypto.txt").unwrap(), "legacy encryption");

    // Encrypted archives are read-only with a clear message.
    assert!(!fs.writable());
    let err = fs.create_write(Path::new("/new.txt"), 0o644).err().unwrap();
    assert!(format!("{err:#}").contains("encrypted"), "{err:#}");

    // The password is remembered for this session: a fresh instance is unlocked.
    let mut again = open(&parent, &zip_path).unwrap();
    assert!(!again.needs_password(&[PathBuf::from("/")]));
    assert_eq!(read_all(&mut again, "/secret/aes256.txt").unwrap(), "data of secret/aes256.txt");
    secrets::forget(&zip_path.display().to_string());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn copy_job_out_of_unlocked_zip() {
    let dir = std::env::temp_dir().join(format!("pairdeck-zippw-job-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("out")).unwrap();
    let zip_path = dir.join("j.zip");
    make_encrypted_zip(&zip_path, "pw");
    let mut fs = open(&Location::local(&dir), &zip_path).unwrap();
    assert!(fs.set_password("pw").unwrap());
    let src = Location { vfs: crate::vfs::VfsHandle::new(fs), path: PathBuf::from("/") };
    let (tx, rx) = crossbeam_channel::unbounded();
    crate::ops::spawn(
        crate::ops::JobSpec::Copy {
            src,
            names: vec!["secret".into(), "zipcrypto.txt".into()],
            dst: Location::local(dir.join("out")),
            target: None,
            opts: crate::ops::CopyOpts::default(),
            remove_src: false,
        },
        tx,
    );
    loop {
        if let crate::app::AppEvent::Job(crate::ops::JobMsg::Finished { errors, .. }) =
            rx.recv_timeout(Duration::from_secs(20)).expect("job finished")
        {
            assert!(errors.is_empty(), "{errors:?}");
            break;
        }
    }
    assert_eq!(std::fs::read_to_string(dir.join("out/secret/aes192.txt")).unwrap(), "data of secret/aes192.txt");
    assert_eq!(std::fs::read_to_string(dir.join("out/zipcrypto.txt")).unwrap(), "legacy encryption");
    secrets::forget(&zip_path.display().to_string());
    let _ = std::fs::remove_dir_all(&dir);
}
