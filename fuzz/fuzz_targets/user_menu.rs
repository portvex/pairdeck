//! User menu files (MC format), including a local `.pairdeck.menu` in any directory.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    for e in pairdeck::usermenu::parse(s) {
        let _ = (e.hotkey.len(), e.title.len(), e.script.len(), e.show.as_deref());
    }
});
