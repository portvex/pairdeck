//! JSON parsing, pretty printing and tree navigation; TOML validation.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pairdeck::preview_ext::structured::{self, JsonTree};

fuzz_target!(|input: (&str, Vec<u8>)| {
    let (text, moves) = input;
    let _ = structured::toml_error(text);
    let Ok(v) = structured::parse_json(text) else { return };
    let _ = structured::pretty_json(&v);
    let mut t = JsonTree::new(&v);
    let theme = pairdeck::theme::Theme::by_name("dark");
    for m in moves.iter().take(64) {
        match m % 7 {
            0 => t.move_by(1),
            1 => t.move_by(-3),
            2 => t.left(),
            3 => t.right(),
            4 => t.toggle(),
            5 => t.end(),
            _ => t.home(),
        }
        assert!(t.is_empty() || t.cursor < t.len());
    }
    let _ = t.render(20, 60, &theme, true);
});
