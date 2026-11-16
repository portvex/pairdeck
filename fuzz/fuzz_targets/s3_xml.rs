//! ListObjectsV2 / CreateMultipartUpload bodies come from (possibly hostile) S3 servers.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    let _ = pairdeck::vfs::s3::parse_list_response(s);
    let _ = pairdeck::vfs::s3::parse_upload_id(s);
    let _ = pairdeck::vfs::s3::parse_url(s);
});
