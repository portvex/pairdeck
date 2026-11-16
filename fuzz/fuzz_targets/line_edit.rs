//! Drive the line editor with arbitrary key sequences; the cursor must stay in bounds.
#![no_main]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use libfuzzer_sys::fuzz_target;
use pairdeck::lineedit::LineEdit;

fn key(b: &[u8]) -> KeyEvent {
    let code = match b[0] % 16 {
        0 => KeyCode::Backspace,
        1 => KeyCode::Delete,
        2 => KeyCode::Left,
        3 => KeyCode::Right,
        4 => KeyCode::Home,
        5 => KeyCode::End,
        6 => KeyCode::Up,
        7 => KeyCode::Down,
        _ => KeyCode::Char(char::from_u32(u32::from(b[1]) | (u32::from(b[0] & 0x70) << 4)).unwrap_or('x')),
    };
    let mods = KeyModifiers::from_bits_truncate(b[0] >> 4 & 0x6);
    KeyEvent::new(code, mods)
}

fuzz_target!(|data: &[u8]| {
    let mut ed = LineEdit::new("start").with_history(vec!["one".into(), "two".into()]);
    for chunk in data.chunks_exact(2) {
        ed.handle_key(&key(chunk));
        assert!(ed.cursor <= ed.text().chars().count(), "cursor out of bounds");
    }
    ed.insert_str(&String::from_utf8_lossy(data));
    assert!(ed.cursor <= ed.text().chars().count());
});
