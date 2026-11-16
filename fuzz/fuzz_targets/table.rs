//! CSV / TSV tables and plain-text highlighting in the preview.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pairdeck::preview_ext::{plaintext, table};

fuzz_target!(|input: (u8, &str)| {
    let ext = ["csv", "tsv", "psv", "tab"][input.0 as usize % 4];
    let theme = pairdeck::theme::Theme::by_name("dark");
    if let Some(t) = table::parse(input.1, ext) {
        let lines = table::render(&t, &theme);
        assert!(!lines.is_empty());
    }
    let pal = plaintext::Palette::new(&theme);
    for l in input.1.lines().take(200) {
        let line = plaintext::line(l, &pal);
        let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, l, "plain-text highlighting must not change the text");
    }
});
