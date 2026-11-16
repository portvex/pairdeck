//! SFTP connection strings typed on the command line (`cd sftp://...`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: (&str, Option<&str>)| {
    let _ = pairdeck::vfs::sftp::parse_target(input.0, input.1);
});
