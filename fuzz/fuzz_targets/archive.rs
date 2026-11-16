//! Open arbitrary bytes as every supported archive format, list the whole tree
//! and read the head of every file. Archives are untrusted input.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pairdeck::vfs::{Location, archive};
use std::path::{Path, PathBuf};

// New formats are appended so the selector bytes of existing seeds keep their meaning.
const EXTS: &[&str] =
    &["zip", "tar", "tar.gz", "tar.bz2", "tar.xz", "7z", "gz", "bz2", "xz", "jar", "zst", "tar.zst", "tzst", "iso"];

fn walk(vfs: &mut Box<dyn pairdeck::vfs::Vfs>, dir: &Path, depth: usize, budget: &mut usize) {
    if depth > 16 || *budget == 0 {
        return;
    }
    let Ok(list) = vfs.list(dir) else { return };
    for e in list {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let p = dir.join(&e.name);
        let _ = vfs.stat(&p);
        if e.is_dir() {
            walk(vfs, &p, depth + 1, budget);
        } else {
            let _ = vfs.read_head(&p, 4096);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&sel, body)) = data.split_first() else { return };
    let ext = EXTS[sel as usize % EXTS.len()];
    let dir: PathBuf = std::env::temp_dir().join(format!("pairdeck-fuzz-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("input.{ext}"));
    if std::fs::write(&path, body).is_err() {
        return;
    }
    let parent = Location::local(&dir);
    if let Ok(mut vfs) = archive::open(&parent, &path) {
        let _ = vfs.display(Path::new("/"));
        let mut budget = 2000;
        walk(&mut vfs, Path::new("/"), 0, &mut budget);
        // Selector >= 128: also write into the archive and commit (rewrite path).
        if sel >= 128 && vfs.writable() {
            if let Ok(mut w) = vfs.create_write(Path::new("/fuzz-new.txt"), 0o644) {
                let _ = std::io::Write::write_all(&mut w, b"fuzz");
            }
            let _ = vfs.mkdir(Path::new("/fuzz-dir"), 0o755);
            let _ = vfs.remove_file(Path::new("/fuzz-new.txt"));
            if vfs.commit().is_ok() {
                drop(vfs);
                if let Ok(mut again) = archive::open(&parent, &path) {
                    let mut budget = 2000;
                    walk(&mut again, Path::new("/"), 0, &mut budget);
                }
            }
        }
    }
});
