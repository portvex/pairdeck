//! Bulk rename (edit names in a list) and regex rename with live preview.

use crate::app::App;
use crate::dialog::{Dialog, Form, Outcome, centered, draw_box};
use crate::lineedit::LineEdit;
use crate::screen::{Screen, ScreenOutcome};
use crate::ui::Ctx;
use crate::undo::UndoOp;
use crate::util::{pad_right, str_width, trunc_mid};
use crate::vfs::Location;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use std::any::Any;
use std::collections::HashSet;

// ------------------------------------------------------------------ planning

/// Order renames so no step overwrites an existing name. Cycles and swaps
/// (a -> b, b -> a) go through temporary names. `existing` holds every name in
/// the directory. Returns the steps to perform, or the list of problems.
pub fn plan(pairs: &[(String, String)], existing: &HashSet<String>) -> Result<Vec<(String, String)>, Vec<String>> {
    let changes: Vec<(String, String)> = pairs.iter().filter(|(a, b)| a != b).cloned().collect();
    let mut errors = Vec::new();
    let sources: HashSet<&str> = changes.iter().map(|(a, _)| a.as_str()).collect();
    let mut targets = HashSet::new();
    for (from, to) in &changes {
        if to.trim().is_empty() {
            errors.push(format!("\"{from}\": new name is empty"));
        } else if to.contains('/') || to.contains('\0') || to == "." || to == ".." {
            errors.push(format!("\"{to}\": invalid name"));
        } else if !targets.insert(to.as_str()) {
            errors.push(format!("\"{to}\": more than one file would get this name"));
        } else if existing.contains(to) && !sources.contains(to.as_str()) {
            errors.push(format!("\"{to}\": already exists"));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut present: HashSet<String> = existing.clone();
    present.extend(changes.iter().map(|(a, _)| a.clone()));
    let mut pending = changes;
    let mut steps = Vec::new();
    let mut tmp_n = 0;
    while !pending.is_empty() {
        let mut progressed = false;
        let mut i = 0;
        while i < pending.len() {
            if present.contains(&pending[i].1) {
                i += 1;
                continue;
            }
            let (from, to) = pending.remove(i);
            present.remove(&from);
            present.insert(to.clone());
            steps.push((from, to));
            progressed = true;
        }
        if !progressed {
            // Every remaining target is occupied by another pending source: a cycle.
            // Park the first source under a temporary name to break it.
            let tmp = loop {
                tmp_n += 1;
                let cand = format!(".pairdeck-rename-{tmp_n}");
                if !present.contains(&cand) {
                    break cand;
                }
            };
            let from = std::mem::replace(&mut pending[0].0, tmp.clone());
            present.remove(&from);
            present.insert(tmp.clone());
            steps.push((from, tmp));
        }
    }
    Ok(steps)
}

/// Perform planned steps in `loc`. Returns the undo batch for the steps that
/// succeeded and the first error, if any.
pub fn execute(loc: &Location, steps: &[(String, String)]) -> (UndoOp, Option<anyhow::Error>) {
    let mut done = Vec::new();
    for (from, to) in steps {
        let (fp, tp) = (loc.join(from), loc.join(to));
        let res = loc.vfs.lock().rename(&fp, &tp);
        if let Err(e) = res {
            return (UndoOp::Batch(done), Some(e));
        }
        done.push(UndoOp::Move { src: loc.vfs.clone(), from: fp, dst: loc.vfs.clone(), to: tp });
    }
    (UndoOp::Batch(done), None)
}

/// Which part of a name the regex rename touches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Name,
    Stem,
    Extension,
}

fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

/// Apply `re`/`repl` (with $1 style groups) to the chosen part of `name`.
pub fn regex_rename(name: &str, re: &regex::Regex, repl: &str, scope: Scope) -> String {
    match scope {
        Scope::Name => re.replace_all(name, repl).into_owned(),
        Scope::Stem => {
            let (stem, ext) = split_ext(name);
            let s = re.replace_all(stem, repl);
            if ext.is_empty() && !name.ends_with('.') { s.into_owned() } else { format!("{s}.{ext}") }
        }
        Scope::Extension => {
            let (stem, ext) = split_ext(name);
            if ext.is_empty() {
                return name.to_string();
            }
            let e = re.replace_all(ext, repl);
            if e.is_empty() { stem.to_string() } else { format!("{stem}.{e}") }
        }
    }
}

fn all_names(loc: &Location) -> anyhow::Result<HashSet<String>> {
    Ok(loc.vfs.lock().list(&loc.path)?.into_iter().map(|e| e.name).collect())
}

/// Names the bulk tools work on: marked files or the cursor file.
fn selection(app: &mut App) -> Option<(Location, Vec<String>)> {
    let p = app.panel();
    if p.panelized.is_some() {
        app.flash_error("Bulk rename does not work in a panelized list");
        return None;
    }
    let names: Vec<String> = p.selection().into_iter().map(|e| e.name).collect();
    if names.is_empty() {
        return None;
    }
    let loc = p.loc.clone();
    if !loc.vfs.lock().writable() {
        app.flash_error("This location is read-only");
        return None;
    }
    Some((loc, names))
}

/// Show the planned changes, then apply them.
fn confirm_and_apply(app: &mut App, loc: Location, pairs: Vec<(String, String)>, close_screen: bool) {
    let existing = match all_names(&loc) {
        Ok(n) => n,
        Err(e) => {
            app.error(format!("{e:#}"));
            return;
        }
    };
    let steps = match plan(&pairs, &existing) {
        Ok(s) => s,
        Err(errs) => {
            let mut text = errs.iter().take(12).cloned().collect::<Vec<_>>().join("\n");
            if errs.len() > 12 {
                text.push_str(&format!("\n... and {} more", errs.len() - 12));
            }
            app.error(text);
            return;
        }
    };
    let changes: Vec<&(String, String)> = pairs.iter().filter(|(a, b)| a != b).collect();
    if changes.is_empty() {
        app.info("No names changed");
        return;
    }
    let mut text = changes
        .iter()
        .take(15)
        .map(|(a, b)| format!("{} -> {}", trunc_mid(a, 30), trunc_mid(b, 30)))
        .collect::<Vec<_>>()
        .join("\n");
    if changes.len() > 15 {
        text.push_str(&format!("\n... and {} more", changes.len() - 15));
    }
    let n = changes.len();
    let d = Form::new(&format!("Rename {n} file(s)?")).text(&text).buttons(&["Rename", "Cancel"], 0).on_submit(
        move |app, r| {
            if r.button != 0 {
                return;
            }
            let (undo, err) = execute(&loc, &steps);
            app.undo.push(format!("Bulk rename of {n} file(s)"), undo);
            if close_screen && app.top_screen::<BulkScreen>().is_some() {
                app.screens.pop();
            }
            app.panel_mut().marked.clear();
            app.reload_panels();
            match err {
                Some(e) => app.error(format!("Rename stopped:\n{e:#}\n(Ctrl-X u undoes the renames done so far)")),
                None => app.info(format!("Renamed {n} file(s)")),
            }
        },
    );
    app.push_dialog(Box::new(d));
}

// ------------------------------------------------------------------ bulk screen

pub fn open(app: &mut App) {
    let Some((loc, names)) = selection(app) else { return };
    let lines = names.iter().map(|n| n.chars().collect()).collect();
    app.push_screen(Box::new(BulkScreen { loc, originals: names, lines, row: 0, col: 0, top: 0, height: 10 }));
}

/// Full-screen list editor: one line per file, the line count is fixed.
pub struct BulkScreen {
    loc: Location,
    originals: Vec<String>,
    lines: Vec<Vec<char>>,
    row: usize,
    col: usize,
    top: usize,
    height: usize,
}

impl BulkScreen {
    fn pairs(&self) -> Vec<(String, String)> {
        self.originals.iter().cloned().zip(self.lines.iter().map(|l| l.iter().collect::<String>())).collect()
    }

    fn clamp_col(&mut self) {
        self.col = self.col.min(self.lines[self.row].len());
    }
}

impl Screen for BulkScreen {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(if ctx.classic { BorderType::Double } else { BorderType::Rounded })
            .border_style(ctx.theme.border_active)
            .style(ctx.theme.editor)
            .title(
                Line::styled(format!(" Bulk rename: {} file(s) ", self.lines.len()), ctx.theme.dialog_title).centered(),
            )
            .title_bottom(
                Line::styled(
                    " Enter/F2 apply   Esc cancel   Ctrl-U clear line   F5 lower   F6 upper ",
                    ctx.theme.border,
                )
                .centered(),
            );
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.height = inner.height.saturating_sub(1) as usize;
        let half = (inner.width / 2) as usize;
        let header = format!("{}{}", pad_right(" Current name", half), " New name");
        f.render_widget(Paragraph::new(Span::styled(header, ctx.theme.header)), Rect { height: 1, ..inner });
        if self.row < self.top {
            self.top = self.row;
        } else if self.height > 0 && self.row >= self.top + self.height {
            self.top = self.row + 1 - self.height;
        }
        let mut out = Vec::new();
        for (i, (orig, line)) in self.originals.iter().zip(&self.lines).enumerate().skip(self.top).take(self.height) {
            let new: String = line.iter().collect();
            let changed = &new != orig;
            let left = pad_right(&format!(" {}", trunc_mid(orig, half.saturating_sub(2))), half);
            let st = if i == self.row {
                ctx.theme.input_focus
            } else if changed {
                ctx.theme.marked
            } else {
                ctx.theme.editor
            };
            out.push(Line::from(vec![
                Span::styled(left, ctx.theme.editor_line_nr),
                Span::styled(pad_right(&new, (inner.width as usize).saturating_sub(half)), st),
            ]));
        }
        let list = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
        f.render_widget(Paragraph::new(out), list);
        // Cursor.
        let cx: usize = self.lines[self.row][..self.col].iter().map(|c| str_width(&c.to_string())).sum();
        let x = inner.x + half as u16 + cx as u16;
        let y = list.y + (self.row - self.top) as u16;
        if x < inner.right() && y < list.bottom() {
            f.set_cursor_position((x, y));
        }
    }

    fn key(&mut self, key: KeyEvent) -> ScreenOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.lines.len();
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return ScreenOutcome::Close,
            KeyCode::Enter | KeyCode::F(2) => {
                let loc = self.loc.clone();
                let pairs = self.pairs();
                return ScreenOutcome::Then(Box::new(move |app| confirm_and_apply(app, loc, pairs, true)));
            }
            KeyCode::Char('s') if ctrl => {
                let loc = self.loc.clone();
                let pairs = self.pairs();
                return ScreenOutcome::Then(Box::new(move |app| confirm_and_apply(app, loc, pairs, true)));
            }
            KeyCode::Up => self.row = self.row.saturating_sub(1),
            KeyCode::Down => self.row = (self.row + 1).min(n - 1),
            KeyCode::PageUp => self.row = self.row.saturating_sub(self.height.max(1)),
            KeyCode::PageDown => self.row = (self.row + self.height.max(1)).min(n - 1),
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.lines[self.row].len(),
            KeyCode::Left => self.col = self.col.saturating_sub(1),
            KeyCode::Right => self.col = (self.col + 1).min(self.lines[self.row].len()),
            KeyCode::Backspace if self.col > 0 => {
                self.col -= 1;
                self.lines[self.row].remove(self.col);
            }
            KeyCode::Delete if self.col < self.lines[self.row].len() => {
                self.lines[self.row].remove(self.col);
            }
            KeyCode::Char('u') if ctrl => {
                self.lines[self.row].clear();
                self.col = 0;
            }
            KeyCode::Char('r') if ctrl => {
                // Restore the original name of this line.
                self.lines[self.row] = self.originals[self.row].chars().collect();
            }
            KeyCode::F(5) | KeyCode::F(6) => {
                let upper = key.code == KeyCode::F(6);
                for l in &mut self.lines {
                    let s: String = l.iter().collect();
                    *l = if upper { s.to_uppercase() } else { s.to_lowercase() }.chars().collect();
                }
            }
            KeyCode::Char(c) if !ctrl && c != '/' => {
                self.lines[self.row].insert(self.col, c);
                self.col += 1;
            }
            _ => {}
        }
        self.clamp_col();
        ScreenOutcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> ScreenOutcome {
        match m.kind {
            MouseEventKind::ScrollUp => self.row = self.row.saturating_sub(3),
            MouseEventKind::ScrollDown => self.row = (self.row + 3).min(self.lines.len() - 1),
            _ => {}
        }
        self.clamp_col();
        ScreenOutcome::Keep
    }

    fn paste(&mut self, text: &str) {
        for c in text.chars().filter(|c| *c != '\n' && *c != '\r' && *c != '/') {
            self.lines[self.row].insert(self.col, c);
            self.col += 1;
        }
    }

    fn title(&self) -> String {
        "Bulk rename".into()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ------------------------------------------------------------------ regex dialog

pub fn regex_dialog(app: &mut App) {
    let Some((loc, names)) = selection(app) else { return };
    let mut d = RegexDialog {
        loc,
        names,
        pattern: LineEdit::new(""),
        repl: LineEdit::new(""),
        focus: 0,
        scope: Scope::Stem,
        icase: false,
        preview: Vec::new(),
        error: None,
        scroll: 0,
    };
    d.update();
    app.push_dialog(Box::new(d));
}

pub struct RegexDialog {
    loc: Location,
    names: Vec<String>,
    pattern: LineEdit,
    repl: LineEdit,
    /// 0 pattern, 1 replacement, 2 scope, 3 ignore case.
    focus: usize,
    scope: Scope,
    icase: bool,
    preview: Vec<(String, String)>,
    error: Option<String>,
    scroll: usize,
}

impl RegexDialog {
    fn update(&mut self) {
        self.error = None;
        let pat = self.pattern.text();
        if pat.is_empty() {
            self.preview = self.names.iter().map(|n| (n.clone(), n.clone())).collect();
            return;
        }
        match regex::RegexBuilder::new(&pat).case_insensitive(self.icase).build() {
            Ok(re) => {
                let repl = self.repl.text();
                self.preview =
                    self.names.iter().map(|n| (n.clone(), regex_rename(n, &re, &repl, self.scope))).collect();
            }
            Err(e) => {
                self.error = Some(e.to_string().lines().last().unwrap_or("bad pattern").to_string());
                self.preview = self.names.iter().map(|n| (n.clone(), n.clone())).collect();
            }
        }
    }
}

impl Dialog for RegexDialog {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let r = centered(area, 78, area.height.saturating_sub(4).min(30));
        let inner = draw_box(f, r, "Regex rename", ctx, false);
        let label = |t: &str| Span::styled(t.to_string(), ctx.theme.dialog);
        let mut y = inner.y;
        f.render_widget(Paragraph::new(label("Pattern (regex):")), Rect { y, height: 1, ..inner });
        y += 1;
        let st = |on: bool| if on { ctx.theme.input_focus } else { ctx.theme.input };
        self.pattern.render(f, Rect { y, height: 1, ..inner }, st(self.focus == 0), self.focus == 0);
        y += 1;
        f.render_widget(
            Paragraph::new(label("Replacement ($1, $2 or ${name} for groups):")),
            Rect { y, height: 1, ..inner },
        );
        y += 1;
        self.repl.render(f, Rect { y, height: 1, ..inner }, st(self.focus == 1), self.focus == 1);
        y += 1;
        let mark = |s: Scope| if self.scope == s { "(*)" } else { "( )" };
        let scope_text = format!(
            "Apply to: {} name  {} name without extension  {} extension",
            mark(Scope::Name),
            mark(Scope::Stem),
            mark(Scope::Extension)
        );
        let fs = |on: bool| if on { ctx.theme.dialog_focus } else { ctx.theme.dialog };
        f.render_widget(Paragraph::new(Span::styled(scope_text, fs(self.focus == 2))), Rect { y, height: 1, ..inner });
        y += 1;
        let case = format!("[{}] Ignore case", if self.icase { "x" } else { " " });
        f.render_widget(Paragraph::new(Span::styled(case, fs(self.focus == 3))), Rect { y, height: 1, ..inner });
        y += 1;
        let changed = self.preview.iter().filter(|(a, b)| a != b).count();
        let status = match &self.error {
            Some(e) => Span::styled(format!("Error: {e}"), ctx.theme.error),
            None => Span::styled(
                format!("Preview: {changed} of {} name(s) change", self.preview.len()),
                ctx.theme.dialog_title,
            ),
        };
        f.render_widget(Paragraph::new(status), Rect { y, height: 1, ..inner });
        y += 1;
        let list_h = inner.bottom().saturating_sub(y + 1) as usize;
        let half = (inner.width as usize).saturating_sub(4) / 2;
        self.scroll = self.scroll.min(self.preview.len().saturating_sub(list_h));
        let lines: Vec<Line> = self
            .preview
            .iter()
            .skip(self.scroll)
            .take(list_h)
            .map(|(a, b)| {
                let st = if a != b {
                    ctx.theme.marked.bg(ctx.theme.dialog.bg.unwrap_or_default())
                } else {
                    ctx.theme.dialog
                };
                Line::from(vec![
                    Span::styled(pad_right(&trunc_mid(a, half), half), ctx.theme.dialog),
                    Span::styled(" -> ", ctx.theme.dialog_border),
                    Span::styled(trunc_mid(b, half), st),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), Rect { y, height: list_h as u16, ..inner });
        let foot = "Enter rename   Tab next field   Space/arrows change option   PgUp/PgDn scroll   Esc cancel";
        f.render_widget(
            Paragraph::new(Span::styled(trunc_mid(foot, inner.width as usize), ctx.theme.dialog_border)),
            Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner },
        );
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Enter => {
                if self.error.is_some() {
                    return Outcome::Keep;
                }
                let loc = self.loc.clone();
                let pairs = self.preview.clone();
                return Outcome::CloseThen(Box::new(move |app| confirm_and_apply(app, loc, pairs, false)));
            }
            KeyCode::Tab | KeyCode::Down if key.code == KeyCode::Tab || self.focus >= 2 => {
                self.focus = (self.focus + 1) % 4;
            }
            KeyCode::BackTab | KeyCode::Up if key.code == KeyCode::BackTab || self.focus >= 2 => {
                self.focus = (self.focus + 3) % 4;
            }
            KeyCode::Down => self.focus = (self.focus + 1) % 4,
            KeyCode::Up => self.focus = (self.focus + 3) % 4,
            KeyCode::PageDown => self.scroll += 10,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            _ => match self.focus {
                0 => {
                    self.pattern.handle_key(&key);
                }
                1 => {
                    self.repl.handle_key(&key);
                }
                2 => match key.code {
                    KeyCode::Left => {
                        self.scope = match self.scope {
                            Scope::Name => Scope::Extension,
                            Scope::Stem => Scope::Name,
                            Scope::Extension => Scope::Stem,
                        }
                    }
                    KeyCode::Right | KeyCode::Char(' ') => {
                        self.scope = match self.scope {
                            Scope::Name => Scope::Stem,
                            Scope::Stem => Scope::Extension,
                            Scope::Extension => Scope::Name,
                        }
                    }
                    _ => {}
                },
                _ => {
                    if matches!(key.code, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right) {
                        self.icase = !self.icase;
                    }
                }
            },
        }
        self.update();
        Outcome::Keep
    }

    fn paste(&mut self, text: &str) {
        match self.focus {
            0 => self.pattern.insert_str(text),
            1 => self.repl.insert_str(text),
            _ => {}
        }
        self.update();
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    /// Simulate the steps on a name set and return the final mapping of contents.
    fn simulate(existing: &[&str], steps: &[(String, String)]) -> std::collections::HashMap<String, String> {
        // content is the original name
        let mut fs: std::collections::HashMap<String, String> =
            existing.iter().map(|n| (n.to_string(), n.to_string())).collect();
        for (from, to) in steps {
            assert!(!fs.contains_key(to), "step would overwrite {to}");
            let c = fs.remove(from).expect("source exists");
            fs.insert(to.clone(), c);
        }
        fs
    }

    #[test]
    fn simple_and_chained_renames() {
        let ex = ["a", "b", "c"];
        let steps = plan(&p(&[("a", "x"), ("b", "a")]), &set(&ex)).unwrap();
        let fs = simulate(&ex, &steps);
        assert_eq!(fs["x"], "a");
        assert_eq!(fs["a"], "b");
        assert_eq!(fs["c"], "c");
    }

    #[test]
    fn swaps_and_cycles_use_temp_names() {
        let ex = ["a", "b", "c", "d"];
        // swap a<->b and 3-cycle-ish rotation c->d, d->c
        let steps = plan(&p(&[("a", "b"), ("b", "a"), ("c", "d"), ("d", "c")]), &set(&ex)).unwrap();
        let fs = simulate(&ex, &steps);
        assert_eq!((fs["a"].as_str(), fs["b"].as_str(), fs["c"].as_str(), fs["d"].as_str()), ("b", "a", "d", "c"));
        assert!(steps.iter().any(|(_, t)| t.starts_with(".pairdeck-rename-")), "a temp name was used");
        assert!(!fs.keys().any(|k| k.starts_with(".pairdeck-rename-")), "no temp name left behind");
        // rotation of three
        let ex3 = ["1", "2", "3"];
        let steps = plan(&p(&[("1", "2"), ("2", "3"), ("3", "1")]), &set(&ex3)).unwrap();
        let fs = simulate(&ex3, &steps);
        assert_eq!((fs["2"].as_str(), fs["3"].as_str(), fs["1"].as_str()), ("1", "2", "3"));
    }

    #[test]
    fn conflicts_are_reported() {
        let ex = set(&["a", "b", "taken"]);
        let errs = plan(&p(&[("a", "taken")]), &ex).unwrap_err();
        assert!(errs[0].contains("already exists"));
        let errs = plan(&p(&[("a", "z"), ("b", "z")]), &ex).unwrap_err();
        assert!(errs[0].contains("more than one"));
        assert!(plan(&p(&[("a", "")]), &ex).is_err());
        assert!(plan(&p(&[("a", "x/y")]), &ex).is_err());
        assert!(plan(&p(&[("a", "..")]), &ex).is_err());
        assert_eq!(plan(&p(&[("a", "a")]), &ex).unwrap(), Vec::<(String, String)>::new(), "unchanged is a no-op");
    }

    #[test]
    fn regex_scopes() {
        let re = regex::Regex::new(r"(\d+)").unwrap();
        assert_eq!(regex_rename("img_12.jpg", &re, "n$1", Scope::Name), "img_n12.jpg");
        let re = regex::Regex::new(r"^IMG_").unwrap();
        assert_eq!(regex_rename("IMG_1.JPG", &re, "photo-", Scope::Stem), "photo-1.JPG");
        let re = regex::RegexBuilder::new("jpe?g").case_insensitive(true).build().unwrap();
        assert_eq!(regex_rename("a.JPEG", &re, "jpg", Scope::Extension), "a.jpg");
        assert_eq!(regex_rename("noext", &re, "jpg", Scope::Extension), "noext", "no extension to change");
        let re = regex::Regex::new("x").unwrap();
        assert_eq!(regex_rename(".hidden", &re, "y", Scope::Stem), ".hidden", "leading dot is not an extension");
        let re = regex::Regex::new(r"(?P<y>\d{4})-(?P<m>\d{2})").unwrap();
        assert_eq!(regex_rename("2026-11.txt", &re, "${m}.${y}", Scope::Stem), "11.2026.txt");
    }

    #[test]
    fn execute_applies_and_returns_undo() {
        let base = std::env::temp_dir().join(format!("pairdeck-bulk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a"), "A").unwrap();
        std::fs::write(base.join("b"), "B").unwrap();
        let loc = Location::local(&base);
        let steps = plan(&p(&[("a", "b"), ("b", "a")]), &all_names(&loc).unwrap()).unwrap();
        let (undo, err) = execute(&loc, &steps);
        assert!(err.is_none());
        assert_eq!(std::fs::read_to_string(base.join("a")).unwrap(), "B");
        assert!(matches!(crate::undo::undo_direct(&undo).unwrap(), crate::undo::Direct::Done(_)));
        assert_eq!(std::fs::read_to_string(base.join("a")).unwrap(), "A", "undo restores the swap");
        let _ = std::fs::remove_dir_all(&base);
    }
}
