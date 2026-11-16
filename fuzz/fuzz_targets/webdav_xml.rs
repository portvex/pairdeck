//! PROPFIND multistatus bodies come from (possibly hostile) WebDAV servers.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    if let Ok(items) = pairdeck::vfs::webdav::parse_multistatus(s) {
        for it in items {
            let _ = (it.path.len(), it.is_dir, it.size, it.mtime);
        }
    }
    let _ = pairdeck::vfs::webdav::parse_url(s);
});
