//! Office documents / e-books (zip + XML) and the XML tag stripper.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pairdeck::preview_ext::docs;

fuzz_target!(|input: (u8, &[u8])| {
    let (sel, data) = input;
    let ext = ["docx", "odt", "pptx", "xlsx", "epub", "ods"][sel as usize % 6];
    if let Ok(text) = docs::extract_text(data, ext) {
        assert!(text.len() <= docs::MAX_TEXT + 4);
    }
    let s = String::from_utf8_lossy(data);
    let _ = docs::strip_xml(&s, &["p", "w:p", "br"], &["w:tab"]);
});
