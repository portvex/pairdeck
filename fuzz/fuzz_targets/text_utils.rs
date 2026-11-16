//! String helpers used for every file name on screen. Must never panic on any
//! Unicode input, and truncation must respect the requested width.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pairdeck::util;
use std::path::Path;

fuzz_target!(|input: (&str, &str, u8, u64)| {
    let (a, b, w, n) = input;
    let w = w as usize;
    for t in [util::trunc_end(a, w), util::trunc_mid(a, w), util::trunc_left(a, w)] {
        assert!(util::str_width(&t) <= w.max(util::str_width(a).min(w)) + 1, "truncation overflow");
    }
    let _ = util::pad_right(a, w);
    let _ = util::pad_left(a, w);
    let q = util::shell_quote(a);
    assert!(!q.is_empty() || a.is_empty());
    let _ = util::tildify(a);
    let _ = util::expand_tilde(a);
    let _ = util::human_size(n);
    let _ = util::grouped(n);
    let _ = util::looks_binary(a.as_bytes());
    let _ = util::glob_matcher(b, w % 2 == 0).map(|m| m.is_match(a));
    let _ = pairdeck::ops::apply_mask(a, b);
    let _ = pairdeck::panel::natural_cmp(a, b, w % 2 == 0);
    let _ = pairdeck::icons::ext_of(a);
    let _ = pairdeck::vfs::normalize(Path::new(a));
});
