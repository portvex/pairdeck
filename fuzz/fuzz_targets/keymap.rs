//! Key binding strings from the user's config file.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pairdeck::keymap::{Command, Key, chord_label, parse_chord};

fuzz_target!(|s: &str| {
    if let Some(k) = Key::parse(s) {
        let _ = k.label();
    }
    if let Some(c) = parse_chord(s) {
        let label = chord_label(&c);
        let _ = parse_chord(&label);
    }
    let _ = Command::from_name(s);
    let _ = pairdeck::theme::parse_style(s);
    let _ = pairdeck::theme::parse_color(s);
});
