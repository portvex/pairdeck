//! Single-line text editor with readline-style keys and history.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Frame, layout::Rect, style::Style, text::Span, widgets::Paragraph};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, Default)]
pub struct LineEdit {
    chars: Vec<char>,
    pub cursor: usize,
    scroll: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    saved: String,
    /// Mask input (passwords).
    pub masked: bool,
    /// When true the first typed char replaces the whole text (preselected).
    pub select_all: bool,
}

impl LineEdit {
    pub fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        LineEdit { chars, cursor, ..Default::default() }
    }

    pub fn with_history(mut self, h: Vec<String>) -> Self {
        self.history = h;
        self
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn set(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
        self.select_all = false;
    }

    pub fn clear(&mut self) {
        self.set("");
    }

    pub fn insert_str(&mut self, s: &str) {
        if self.select_all {
            self.chars.clear();
            self.cursor = 0;
            self.select_all = false;
        }
        for c in s.chars().filter(|c| *c != '\n' && *c != '\r') {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    /// Select the file stem (before the extension) like modern rename dialogs.
    pub fn cursor_to(&mut self, pos: usize) {
        self.cursor = pos.min(self.chars.len());
    }

    fn word_left(&self) -> usize {
        let mut i = self.cursor;
        while i > 0 && !self.chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && self.chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let mut i = self.cursor;
        let n = self.chars.len();
        while i < n && !self.chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < n && self.chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    fn history_step(&mut self, up: bool) {
        if self.history.is_empty() {
            return;
        }
        let pos = match (self.hist_pos, up) {
            (None, true) => {
                self.saved = self.text();
                Some(self.history.len() - 1)
            }
            (None, false) => None,
            (Some(0), true) => Some(0),
            (Some(p), true) => Some(p - 1),
            (Some(p), false) if p + 1 < self.history.len() => Some(p + 1),
            (Some(_), false) => None,
        };
        self.hist_pos = pos;
        let t = match pos {
            Some(p) => self.history[p].clone(),
            None => self.saved.clone(),
        };
        self.set(&t);
    }

    pub fn has_history(&self) -> bool {
        !self.history.is_empty()
    }

    /// Handle a key. Returns true if the key was consumed.
    pub fn handle_key(&mut self, k: &KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let keep_select = matches!(k.code, KeyCode::Char(_)) && !ctrl && !alt;
        if !keep_select {
            self.select_all = false;
        }
        match k.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.chars.len(),
            KeyCode::Char('b') if ctrl => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Char('k') if ctrl => self.chars.truncate(self.cursor),
            KeyCode::Char('u') if ctrl => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let s = self.word_left();
                self.chars.drain(s..self.cursor);
                self.cursor = s;
            }
            KeyCode::Char('d') if ctrl => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Char('b') if alt => self.cursor = self.word_left(),
            KeyCode::Char('f') if alt => self.cursor = self.word_right(),
            KeyCode::Char('d') if alt => {
                let e = self.word_right();
                self.chars.drain(self.cursor..e);
            }
            KeyCode::Backspace if alt || ctrl => {
                let s = self.word_left();
                self.chars.drain(s..self.cursor);
                self.cursor = s;
            }
            KeyCode::Left if ctrl || alt => self.cursor = self.word_left(),
            KeyCode::Right if ctrl || alt => self.cursor = self.word_right(),
            KeyCode::Char(c) if !ctrl && !alt => {
                if self.select_all {
                    self.chars.clear();
                    self.cursor = 0;
                    self.select_all = false;
                }
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            KeyCode::Up if !self.history.is_empty() => self.history_step(true),
            KeyCode::Down if !self.history.is_empty() => self.history_step(false),
            KeyCode::Char('p') if alt => self.history_step(true),
            KeyCode::Char('n') if alt => self.history_step(false),
            _ => return false,
        }
        true
    }

    /// Render into `area` (one line) and place the terminal cursor if `focused`.
    pub fn render(&mut self, f: &mut Frame, area: Rect, style: Style, focused: bool) {
        if area.width == 0 {
            return;
        }
        let width = area.width as usize;
        let shown: Vec<char> = if self.masked { vec!['*'; self.chars.len()] } else { self.chars.clone() };
        // Width of chars before cursor, adjust scroll so the cursor is visible.
        let wbefore = |from: usize, to: usize| shown[from..to].iter().map(|c| c.width().unwrap_or(0)).sum::<usize>();
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        while self.scroll < self.cursor && wbefore(self.scroll, self.cursor) >= width {
            self.scroll += 1;
        }
        let mut out = String::new();
        let mut w = 0;
        for c in &shown[self.scroll..] {
            let cw = c.width().unwrap_or(0);
            if w + cw > width {
                break;
            }
            out.push(*c);
            w += cw;
        }
        let pad = width.saturating_sub(w);
        out.push_str(&" ".repeat(pad));
        let st = if self.select_all { style.add_modifier(ratatui::style::Modifier::REVERSED) } else { style };
        f.render_widget(Paragraph::new(Span::styled(out, st)), area);
        if focused {
            let cx = area.x + wbefore(self.scroll, self.cursor) as u16;
            f.set_cursor_position((cx.min(area.right().saturating_sub(1)), area.y));
        }
    }
}
