//! Untrusted ISO 9660 images (Joliet, Rock Ridge): parse the tree and read every file.
//! The input is placed after 16 empty sectors (the system area), so the fuzzer
//! starts right at the volume descriptors; seeds are images minus their first 32 KiB.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pairdeck::vfs::iso;
use std::io::{Cursor, Read};

fuzz_target!(|data: &[u8]| {
    let mut img = vec![0u8; 16 * 2048];
    img.extend_from_slice(data);
    let size = img.len() as u64;
    let mut c = Cursor::new(&img);
    let _ = iso::sniff(&mut c);
    if let Ok(idx) = iso::parse(&mut c, size) {
        let _ = idx.flavor.label();
        for (i, ext) in idx.extents.values().enumerate().take(2000) {
            let mut r = iso::ExtentReader::new(Cursor::new(&img), ext);
            let mut buf = Vec::new();
            let _ = r.by_ref().take(64 << 10).read_to_end(&mut buf);
            if i > 2000 {
                break;
            }
        }
    }
});
