//! Options > Key bindings: browse all commands, bind new keys or chords, remove
//! bindings, reset to defaults. Changes apply immediately and are saved to the
//! `[keys]` table of config.toml, keeping the rest of the file intact.

use crate::app::App;
use crate::dialog::{Dialog, Outcome, centered, draw_box};
use crate::keymap::{Chord, Command, Key, chord_label, default_bindings, overrides};
use crate::ui::Ctx;
use crate::util::{pad_right, trunc_end};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::Path;

enum Mode {
    Browse,
    /// Recording a new binding for the selected command.
    Capture(Vec<Key>),
    /// The captured chord belongs to another command: replace it?
    ConfirmReplace(Chord, Command),
    ConfirmReset,
}

pub struct KeyEditor {
    bindings: HashMap<Chord, Command>,
    filter: String,
    visible: Vec<Command>,
    cursor: usize,
    offset: usize,
    mode: Mode,
    message: String,
    list_rect: Rect,
}

pub fn open(app: &mut App) {
    let mut ed = KeyEditor {
        bindings: app.keymap.bindings.clone(),
        filter: String::new(),
        visible: Vec::new(),
        cursor: 0,
        offset: 0,
        mode: Mode::Browse,
        message: String::new(),
        list_rect: Rect::default(),
    };
    ed.refilter();
    app.push_dialog(Box::new(ed));
}

impl KeyEditor {
    fn keys_of(&self, cmd: Command) -> Vec<String> {
        let mut v: Vec<String> =
            self.bindings.iter().filter(|(_, c)| **c == cmd).map(|(k, _)| chord_label(k)).collect();
        v.sort_by_key(|s| (s.len(), s.clone()));
        v
    }

    fn refilter(&mut self) {
        let f = self.filter.to_lowercase();
        self.visible = Command::ALL
            .iter()
            .copied()
            .filter(|c| {
                f.is_empty()
                    || c.desc().to_lowercase().contains(&f)
                    || c.name().contains(&f)
                    || self.keys_of(*c).iter().any(|k| k.to_lowercase().contains(&f))
            })
            .collect();
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }

    fn selected(&self) -> Option<Command> {
        self.visible.get(self.cursor).copied()
    }

    /// First keys of multi-key chords: after one of these, wait for a second key.
    fn prefixes(&self) -> HashSet<Key> {
        self.bindings.keys().filter(|c| c.len() > 1).map(|c| c[0]).collect()
    }

    /// Apply the working bindings to the app and save them.
    fn commit(&self, msg: String) -> Outcome {
        let bindings = self.bindings.clone();
        Outcome::KeepThen(Box::new(move |app: &mut App| {
            apply_and_save(app, bindings);
            if let Some(ed) = app.dialogs.last_mut().and_then(|d| d.as_any_mut().downcast_mut::<KeyEditor>()) {
                ed.message = msg;
            }
        }))
    }

    fn bind(&mut self, chord: Chord, cmd: Command) -> Outcome {
        self.bindings.insert(chord.clone(), cmd);
        self.mode = Mode::Browse;
        self.refilter();
        self.commit(format!("{} bound to {}", chord_label(&chord), cmd.desc()))
    }
}

/// Install `bindings` as the live keymap and write the `[keys]` overrides.
pub fn apply_and_save(app: &mut App, bindings: HashMap<Chord, Command>) {
    let table = overrides(&bindings);
    app.keymap.bindings = bindings;
    app.keymap.pending.clear();
    app.cfg.keys = table.clone().into_iter().collect();
    let Some(path) = crate::config::Config::path() else { return };
    match save_keys(&path, &table) {
        Ok(()) => crate::reload::start(app),
        Err(e) => app.error(format!("Cannot save key bindings to {}:\n{e}", path.display())),
    }
}

/// Replace the `[keys]` table in a TOML file, preserving everything else
/// (comments, ordering, formatting). Creates the file if needed.
pub fn save_keys(path: &Path, table: &std::collections::BTreeMap<String, String>) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| crate::config::SAMPLE_CONFIG.to_string());
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let mut keys = toml_edit::Table::new();
    for (k, v) in table {
        keys.insert(k, toml_edit::value(v.as_str()));
    }
    // Keep the header comment of an existing [keys] table.
    if let Some(old) = doc.get("keys").and_then(|i| i.as_table()) {
        keys.decor_mut().clone_from(old.decor());
    }
    doc.insert("keys", toml_edit::Item::Table(keys));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, doc.to_string())?;
    Ok(())
}

impl Dialog for KeyEditor {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let w = area.width.saturating_sub(8).clamp(40, 110);
        let h = area.height.saturating_sub(4).max(10);
        let r = centered(area, w, h);
        let inner = draw_box(f, r, "Key bindings", ctx, false);
        let width = inner.width as usize;
        let help = "type: filter  Enter: add key  Del: remove keys  Ctrl-R: reset all  Esc: close";
        f.render_widget(
            Paragraph::new(Span::styled(trunc_end(help, width), ctx.theme.dialog_border)),
            Rect { height: 1, ..inner },
        );
        let filter = if self.filter.is_empty() {
            "Filter: (type to search)".to_string()
        } else {
            format!("Filter: {}", self.filter)
        };
        f.render_widget(
            Paragraph::new(Span::styled(filter, ctx.theme.dialog)),
            Rect { y: inner.y + 1, height: 1, ..inner },
        );
        let list = Rect { y: inner.y + 3, height: inner.height.saturating_sub(5), ..inner };
        self.list_rect = list;
        let rows = list.height as usize;
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if rows > 0 && self.cursor >= self.offset + rows {
            self.offset = self.cursor + 1 - rows;
        }
        let keys_w = (width / 3).max(14);
        let lines: Vec<Line> = self
            .visible
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(rows)
            .map(|(i, c)| {
                let st = if i == self.cursor { ctx.theme.dialog_focus } else { ctx.theme.dialog };
                let keys = self.keys_of(*c).join(", ");
                let left_w = width.saturating_sub(keys_w + 1);
                let left = format!("{}  ({})", c.desc(), c.name());
                Line::from(vec![
                    Span::styled(pad_right(&trunc_end(&left, left_w), left_w + 1), st),
                    Span::styled(
                        pad_right(&trunc_end(&keys, keys_w), keys_w),
                        st.patch(ctx.theme.menu_hotkey.bg(st.bg.unwrap_or_default())),
                    ),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list);
        let status = match &self.mode {
            Mode::Browse => self.message.clone(),
            Mode::Capture(keys) => format!(
                "Press the new key for \"{}\"{}  (Esc cancels)",
                self.selected().map(|c| c.desc()).unwrap_or(""),
                if keys.is_empty() { String::new() } else { format!(": {} ...", chord_label(keys)) }
            ),
            Mode::ConfirmReplace(ch, other) => {
                format!("{} is bound to \"{}\". Replace it? (y/n)", chord_label(ch), other.desc())
            }
            Mode::ConfirmReset => "Reset ALL key bindings to the defaults? (y/n)".to_string(),
        };
        let st = if matches!(self.mode, Mode::Browse) { ctx.theme.dialog_title } else { ctx.theme.dialog_focus };
        let sr = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
        f.render_widget(Paragraph::new(Span::styled(pad_right(&trunc_end(&status, width), width), st)), sr);
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Capture(mut keys) => {
                if key.code == KeyCode::Esc {
                    self.message = "Cancelled".into();
                    return Outcome::Keep;
                }
                keys.push(Key::from_event(&key));
                if keys.len() == 1 && self.prefixes().contains(&keys[0]) {
                    self.mode = Mode::Capture(keys);
                    return Outcome::Keep;
                }
                let Some(cmd) = self.selected() else { return Outcome::Keep };
                return match self.bindings.get(&keys).copied() {
                    Some(other) if other == cmd => {
                        self.message = format!("{} is already bound to it", chord_label(&keys));
                        Outcome::Keep
                    }
                    Some(other) => {
                        self.mode = Mode::ConfirmReplace(keys, other);
                        Outcome::Keep
                    }
                    None => self.bind(keys, cmd),
                };
            }
            Mode::ConfirmReplace(chord, _) => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter)
                    && let Some(cmd) = self.selected()
                {
                    return self.bind(chord, cmd);
                }
                self.message = "Not changed".into();
                return Outcome::Keep;
            }
            Mode::ConfirmReset => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                    self.bindings = default_bindings();
                    self.refilter();
                    return self.commit("All key bindings reset to the defaults".into());
                }
                self.message = "Not changed".into();
                return Outcome::Keep;
            }
            Mode::Browse => {}
        }
        let page = self.list_rect.height.max(1) as usize;
        let n = self.visible.len();
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(page),
            KeyCode::PageDown => self.cursor = (self.cursor + page).min(n.saturating_sub(1)),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = n.saturating_sub(1),
            KeyCode::Enter | KeyCode::F(4) | KeyCode::Insert => {
                if self.selected().is_some() {
                    self.mode = Mode::Capture(Vec::new());
                }
            }
            KeyCode::Delete | KeyCode::F(8) => {
                if let Some(cmd) = self.selected() {
                    let before = self.bindings.len();
                    self.bindings.retain(|_, c| *c != cmd);
                    if self.bindings.len() != before {
                        return self.commit(format!("Removed the keys of \"{}\"", cmd.desc()));
                    }
                }
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => self.mode = Mode::ConfirmReset,
            KeyCode::Backspace => {
                self.filter.pop();
                self.refilter();
            }
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                self.filter.push(c);
                self.cursor = 0;
                self.refilter();
            }
            _ => {}
        }
        Outcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> Outcome {
        let r = self.list_rect;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if r.contains((m.column, m.row).into()) => {
                let idx = self.offset + (m.row - r.y) as usize;
                if idx < self.visible.len() {
                    self.cursor = idx;
                }
            }
            MouseEventKind::ScrollUp => self.cursor = self.cursor.saturating_sub(3),
            MouseEventKind::ScrollDown => self.cursor = (self.cursor + 3).min(self.visible.len().saturating_sub(1)),
            _ => {}
        }
        Outcome::Keep
    }

    fn paste(&mut self, text: &str) {
        if matches!(self.mode, Mode::Browse) {
            self.filter.push_str(text);
            self.refilter();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_keys_preserves_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("pairdeck-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# my settings\ntheme = \"nord\" # favourite\n\n[keys]\n\"F5\" = \"copy\"\n").unwrap();
        let mut table = std::collections::BTreeMap::new();
        table.insert("ctrl-p".to_string(), "find_file".to_string());
        table.insert("F5".to_string(), "none".to_string());
        save_keys(&path, &table).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my settings") && text.contains("theme = \"nord\" # favourite"), "{text}");
        let parsed: crate::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.theme, "nord");
        assert_eq!(parsed.keys.get("ctrl-p").map(String::as_str), Some("find_file"));
        assert_eq!(parsed.keys.get("F5").map(String::as_str), Some("none"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
