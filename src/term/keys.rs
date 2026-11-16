//! Encode crossterm key events as the byte sequences an xterm-compatible
//! terminal sends, so programs running in the embedded pane see real keys.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn modifier_param(m: KeyModifiers) -> u8 {
    1 + u8::from(m.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(m.contains(KeyModifiers::ALT))
        + 4 * u8::from(m.contains(KeyModifiers::CONTROL))
}

/// CSI sequence for cursor-style keys (arrows, Home/End).
fn cursor_key(final_byte: char, m: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    let p = modifier_param(m);
    if p > 1 {
        format!("\x1b[1;{p}{final_byte}").into_bytes()
    } else if app_cursor {
        format!("\x1bO{final_byte}").into_bytes()
    } else {
        format!("\x1b[{final_byte}").into_bytes()
    }
}

/// `CSI n ~` keys (Insert, Delete, PgUp, PgDn, F5-F12).
fn tilde_key(n: u8, m: KeyModifiers) -> Vec<u8> {
    let p = modifier_param(m);
    if p > 1 { format!("\x1b[{n};{p}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
}

/// Control byte for Ctrl+char, if the combination has one.
fn ctrl_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        ' ' | '@' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// Bytes for a key event, or None for keys with no terminal encoding.
/// `app_cursor` is the DECCKM mode reported by the emulator.
pub fn encode(k: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let m = k.modifiers;
    let alt = m.contains(KeyModifiers::ALT);
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let with_alt = |mut v: Vec<u8>| {
        if alt {
            v.insert(0, 0x1b);
        }
        v
    };
    let bytes = match k.code {
        KeyCode::Char(c) if ctrl => with_alt(vec![ctrl_byte(c)?]),
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            with_alt(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => with_alt(vec![b'\r']),
        KeyCode::Tab if m.contains(KeyModifiers::SHIFT) => b"\x1b[Z".to_vec(),
        KeyCode::Tab => with_alt(vec![b'\t']),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if ctrl => with_alt(vec![0x08]),
        KeyCode::Backspace => with_alt(vec![0x7f]),
        KeyCode::Esc => with_alt(vec![0x1b]),
        KeyCode::Up => cursor_key('A', m, app_cursor),
        KeyCode::Down => cursor_key('B', m, app_cursor),
        KeyCode::Right => cursor_key('C', m, app_cursor),
        KeyCode::Left => cursor_key('D', m, app_cursor),
        KeyCode::Home => cursor_key('H', m, app_cursor),
        KeyCode::End => cursor_key('F', m, app_cursor),
        KeyCode::Insert => tilde_key(2, m),
        KeyCode::Delete => tilde_key(3, m),
        KeyCode::PageUp => tilde_key(5, m),
        KeyCode::PageDown => tilde_key(6, m),
        KeyCode::F(n @ 1..=4) => {
            let fb = b"PQRS"[n as usize - 1] as char;
            let p = modifier_param(m);
            if p > 1 { format!("\x1b[1;{p}{fb}").into_bytes() } else { format!("\x1bO{fb}").into_bytes() }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5];
            tilde_key(code, m)
        }
        _ => return None,
    };
    Some(bytes)
}

/// Wrap pasted text in bracketed-paste markers when the program asked for them.
pub fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    // Normalize newlines to carriage returns, as terminals do for pasted text.
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed { format!("\x1b[200~{body}\x1b[201~").into_bytes() } else { body.into_bytes() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> Vec<u8> {
        encode(&KeyEvent::new(code, m), false).unwrap()
    }

    #[test]
    fn printable_and_control() {
        let n = KeyModifiers::NONE;
        assert_eq!(k(KeyCode::Char('a'), n), b"a");
        assert_eq!(k(KeyCode::Char('\u{e9}'), n), "\u{e9}".as_bytes());
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CONTROL), [3]);
        assert_eq!(k(KeyCode::Char('D'), KeyModifiers::CONTROL | KeyModifiers::SHIFT), [4]);
        assert_eq!(k(KeyCode::Char(' '), KeyModifiers::CONTROL), [0]);
        assert_eq!(k(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(k(KeyCode::Enter, n), b"\r");
        assert_eq!(k(KeyCode::Backspace, n), [0x7f]);
        assert_eq!(k(KeyCode::Tab, n), b"\t");
        assert_eq!(k(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(k(KeyCode::Esc, n), [0x1b]);
    }

    #[test]
    fn cursor_and_function_keys() {
        let n = KeyModifiers::NONE;
        assert_eq!(k(KeyCode::Up, n), b"\x1b[A");
        assert_eq!(encode(&KeyEvent::new(KeyCode::Up, n), true).unwrap(), b"\x1bOA");
        assert_eq!(k(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(k(KeyCode::Right, KeyModifiers::SHIFT | KeyModifiers::ALT), b"\x1b[1;4C");
        assert_eq!(k(KeyCode::Home, n), b"\x1b[H");
        assert_eq!(k(KeyCode::Delete, n), b"\x1b[3~");
        assert_eq!(k(KeyCode::PageDown, KeyModifiers::CONTROL), b"\x1b[6;5~");
        assert_eq!(k(KeyCode::F(1), n), b"\x1bOP");
        assert_eq!(k(KeyCode::F(5), n), b"\x1b[15~");
        assert_eq!(k(KeyCode::F(12), KeyModifiers::SHIFT), b"\x1b[24;2~");
        assert!(encode(&KeyEvent::new(KeyCode::CapsLock, n), false).is_none());
    }

    #[test]
    fn paste_modes() {
        assert_eq!(paste("a\nb", false), b"a\rb");
        assert_eq!(paste("x", true), b"\x1b[200~x\x1b[201~");
    }
}
