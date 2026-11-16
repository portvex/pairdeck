//! Compare directories (marks differing files) and compare files (side-by-side diff).

use crate::app::App;
use crate::dialog::Form;
use crate::screen::{Screen, ScreenOutcome};
use crate::ui::Ctx;
use crate::util::pad_right;
use crate::vfs::{Entry, Location};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use similar::{Algorithm, DiffTag, capture_diff_slices};
use std::any::Any;
use std::collections::HashMap;
use std::io::Read;

// ------------------------------------------------------------------ directories

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Quick,
    Size,
    Thorough,
}

pub fn compare_dirs(app: &mut App) {
    let d = Form::new("Compare directories")
        .radio("mode", "Mode:", &["Quick (size and date)", "Size only", "Thorough (content)"], 0)
        .buttons(&["OK", "Cancel"], 0)
        .on_submit(|app, r| {
            if r.button != 0 {
                return;
            }
            let mode = match r.radio("mode") {
                1 => Mode::Size,
                2 => Mode::Thorough,
                _ => Mode::Quick,
            };
            run_compare(app, mode);
        });
    app.push_dialog(Box::new(d));
}

fn files_of(entries: &[Entry]) -> HashMap<String, Entry> {
    entries.iter().filter(|e| !e.is_parent() && !e.is_dir()).map(|e| (e.name.clone(), e.clone())).collect()
}

fn same_content(la: &Location, a: &Entry, lb: &Location, b: &Entry) -> bool {
    if a.size != b.size {
        return false;
    }
    let ra = la.vfs.lock().open_read(&la.join(&a.name));
    let rb = lb.vfs.lock().open_read(&lb.join(&b.name));
    let (Ok(mut ra), Ok(mut rb)) = (ra, rb) else { return false };
    let mut ba = vec![0u8; 1 << 16];
    let mut bb = vec![0u8; 1 << 16];
    loop {
        let na = read_full(&mut ra, &mut ba);
        let nb = read_full(&mut rb, &mut bb);
        match (na, nb) {
            (Ok(0), Ok(0)) => return true,
            (Ok(x), Ok(y)) if x == y && ba[..x] == bb[..y] => continue,
            _ => return false,
        }
    }
}

fn read_full(r: &mut Box<dyn Read + Send>, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = r.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// Returns names to mark on the left and on the right.
fn compute(la: &Location, lefts: &[Entry], lb: &Location, rights: &[Entry], mode: Mode) -> (Vec<String>, Vec<String>) {
    let a = files_of(lefts);
    let b = files_of(rights);
    let mut ma = Vec::new();
    let mut mb = Vec::new();
    for (name, ea) in &a {
        match b.get(name) {
            None => ma.push(name.clone()),
            Some(eb) => {
                let differs = match mode {
                    Mode::Size => ea.size != eb.size,
                    Mode::Quick => ea.size != eb.size || ea.mtime.map(secs) != eb.mtime.map(secs),
                    Mode::Thorough => !same_content(la, ea, lb, eb),
                };
                if differs {
                    match (ea.mtime.map(secs), eb.mtime.map(secs)) {
                        (Some(x), Some(y)) if x > y && mode != Mode::Size => ma.push(name.clone()),
                        (Some(x), Some(y)) if y > x && mode != Mode::Size => mb.push(name.clone()),
                        _ => {
                            ma.push(name.clone());
                            mb.push(name.clone());
                        }
                    }
                }
            }
        }
    }
    for name in b.keys() {
        if !a.contains_key(name) {
            mb.push(name.clone());
        }
    }
    (ma, mb)
}

fn secs(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn apply_marks(app: &mut App, a: usize, ma: Vec<String>, mb: Vec<String>) {
    let b = 1 - a;
    app.panels[a].marked = ma.iter().cloned().collect();
    app.panels[b].marked = mb.iter().cloned().collect();
    if ma.is_empty() && mb.is_empty() {
        app.info("The directories are identical (files only)");
    } else {
        app.info(format!("Marked {} file(s) on the active side, {} on the other side", ma.len(), mb.len()));
    }
}

fn run_compare(app: &mut App, mode: Mode) {
    let a = app.active;
    let la = app.panels[a].loc.clone();
    let lb = app.panels[1 - a].loc.clone();
    let ea = app.panels[a].entries.clone();
    let eb = app.panels[1 - a].entries.clone();
    if mode != Mode::Thorough {
        let (ma, mb) = compute(&la, &ea, &lb, &eb, mode);
        apply_marks(app, a, ma, mb);
        return;
    }
    app.info("Comparing file contents...");
    let tx = app.tx.clone();
    std::thread::spawn(move || {
        let (ma, mb) = compute(&la, &ea, &lb, &eb, mode);
        crate::app::call(&tx, move |app| apply_marks(app, a, ma, mb));
    });
}

// ------------------------------------------------------------------ files

const MAX_DIFF: usize = 16 * 1024 * 1024;

fn read_text(loc: &Location, name: &str) -> anyhow::Result<String> {
    let data = loc.vfs.lock().read_head(&loc.join(name), MAX_DIFF)?;
    if crate::util::looks_binary(&data) {
        anyhow::bail!("\"{name}\" is a binary file");
    }
    Ok(String::from_utf8_lossy(&data).replace('\t', "    "))
}

pub fn compare_files(app: &mut App) {
    let Some(ea) = app.panel().current().cloned().filter(|e| !e.is_dir()) else {
        app.flash_error("Put the cursor on a file to compare");
        return;
    };
    let other = app.other();
    let eb = match other.current().filter(|e| !e.is_dir()) {
        Some(e) => e.clone(),
        None => match other.entries.iter().find(|e| e.name == ea.name && !e.is_dir()) {
            Some(e) => e.clone(),
            None => {
                app.flash_error("No file under the cursor in the other panel");
                return;
            }
        },
    };
    let la = app.panel().loc.clone();
    let lb = other.loc.clone();
    let (ta, tb) = match (read_text(&la, &ea.name), read_text(&lb, &eb.name)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            app.error(format!("{e:#}"));
            return;
        }
    };
    let left_title = la.vfs.lock().display(&la.join(&ea.name));
    let right_title = lb.vfs.lock().display(&lb.join(&eb.name));
    app.push_screen(Box::new(DiffScreen::new(left_title, &ta, right_title, &tb)));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tag {
    Equal,
    Delete,
    Insert,
    Change,
}

struct Row {
    left: Option<(usize, String)>,
    right: Option<(usize, String)>,
    tag: Tag,
}

pub struct DiffScreen {
    left_title: String,
    right_title: String,
    rows: Vec<Row>,
    hunks: Vec<usize>,
    scroll: usize,
    hscroll: usize,
    unified: bool,
    height: usize,
    adds: usize,
    dels: usize,
}

impl DiffScreen {
    pub fn new(left_title: String, a: &str, right_title: String, b: &str) -> Self {
        let la: Vec<&str> = a.lines().collect();
        let lb: Vec<&str> = b.lines().collect();
        let ops = capture_diff_slices(Algorithm::Myers, &la, &lb);
        let mut rows = Vec::new();
        let (mut adds, mut dels) = (0, 0);
        for op in ops {
            let (tag, or, nr) = op.as_tag_tuple();
            match tag {
                DiffTag::Equal => {
                    for (i, j) in or.zip(nr) {
                        rows.push(Row {
                            left: Some((i + 1, la[i].to_string())),
                            right: Some((j + 1, lb[j].to_string())),
                            tag: Tag::Equal,
                        });
                    }
                }
                DiffTag::Delete => {
                    for i in or {
                        dels += 1;
                        rows.push(Row { left: Some((i + 1, la[i].to_string())), right: None, tag: Tag::Delete });
                    }
                }
                DiffTag::Insert => {
                    for j in nr {
                        adds += 1;
                        rows.push(Row { left: None, right: Some((j + 1, lb[j].to_string())), tag: Tag::Insert });
                    }
                }
                DiffTag::Replace => {
                    let ol: Vec<usize> = or.collect();
                    let nl: Vec<usize> = nr.collect();
                    dels += ol.len();
                    adds += nl.len();
                    for k in 0..ol.len().max(nl.len()) {
                        let l = ol.get(k).map(|&i| (i + 1, la[i].to_string()));
                        let r = nl.get(k).map(|&j| (j + 1, lb[j].to_string()));
                        let tag = match (&l, &r) {
                            (Some(_), Some(_)) => Tag::Change,
                            (Some(_), None) => Tag::Delete,
                            _ => Tag::Insert,
                        };
                        rows.push(Row { left: l, right: r, tag });
                    }
                }
            }
        }
        let mut hunks = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            if r.tag != Tag::Equal && (i == 0 || rows[i - 1].tag == Tag::Equal) {
                hunks.push(i);
            }
        }
        let mut s = DiffScreen {
            left_title,
            right_title,
            rows,
            hunks,
            scroll: 0,
            hscroll: 0,
            unified: false,
            height: 20,
            adds,
            dels,
        };
        if let Some(&h) = s.hunks.first() {
            s.scroll = h.saturating_sub(3);
        }
        s
    }

    fn unified_len(&self) -> usize {
        self.rows.iter().map(|r| if r.tag == Tag::Change { 2 } else { 1 }).sum()
    }

    fn total(&self) -> usize {
        if self.unified { self.unified_len() } else { self.rows.len() }
    }

    /// Map a row index to its line position in the current view.
    fn view_pos(&self, row: usize) -> usize {
        if self.unified { self.rows[..row].iter().map(|r| if r.tag == Tag::Change { 2 } else { 1 }).sum() } else { row }
    }

    fn view_row(&self, pos: usize) -> usize {
        if !self.unified {
            return pos;
        }
        let mut p = 0;
        for (i, r) in self.rows.iter().enumerate() {
            p += if r.tag == Tag::Change { 2 } else { 1 };
            if p > pos {
                return i;
            }
        }
        self.rows.len()
    }

    fn jump(&mut self, next: bool) {
        let cur = self.view_row(self.scroll + 3);
        let target = if next {
            self.hunks.iter().find(|&&h| h > cur).copied()
        } else {
            self.hunks.iter().rev().find(|&&h| h < cur).copied()
        };
        if let Some(h) = target {
            self.scroll = self.view_pos(h).saturating_sub(3);
        }
    }

    fn clamp(&mut self) {
        self.scroll = self.scroll.min(self.total().saturating_sub(self.height.max(1)));
    }
}

fn hslice(s: &str, start: usize, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for (i, c) in s.chars().enumerate() {
        if i < start {
            continue;
        }
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// Spans for one side of a changed line with differing characters emphasized.
fn inline_spans(this: &str, other: &str, is_left: bool, base: Style, start: usize, width: usize) -> Vec<Span<'static>> {
    let a: Vec<char> = this.chars().collect();
    let b: Vec<char> = other.chars().collect();
    let (old, new) = if is_left { (&a, &b) } else { (&b, &a) };
    let ops = capture_diff_slices(Algorithm::Myers, old, new);
    let mut diff = vec![false; a.len()];
    for op in ops {
        let (tag, or, nr) = op.as_tag_tuple();
        if tag != DiffTag::Equal {
            let range = if is_left { or } else { nr };
            for i in range {
                if i < diff.len() {
                    diff[i] = true;
                }
            }
        }
    }
    let strong = base.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut w = 0;
    let mut cur = String::new();
    let mut cur_d = false;
    for (i, c) in a.iter().enumerate().skip(start) {
        let cw = unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
        if w + cw > width {
            break;
        }
        if diff[i] != cur_d && !cur.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut cur), if cur_d { strong } else { base }));
        }
        cur_d = diff[i];
        cur.push(*c);
        w += cw;
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, if cur_d { strong } else { base }));
    }
    if w < width {
        spans.push(Span::styled(" ".repeat(width - w), base));
    }
    spans
}

impl Screen for DiffScreen {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = ctx.theme;
        let status = format!(
            " +{} -{}  hunks {}  n/p: next/prev hunk  u: {}  arrows/PgUp/PgDn  q: quit ",
            self.adds,
            self.dels,
            self.hunks.len(),
            if self.unified { "side-by-side" } else { "unified" }
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(if ctx.classic { BorderType::Double } else { BorderType::Rounded })
            .border_style(t.border_active)
            .style(t.viewer)
            .title(Line::styled(" Compare files ", t.dialog_title).centered())
            .title_bottom(Line::styled(status, t.border).centered());
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height < 2 {
            return;
        }
        let header = Rect { height: 1, ..inner };
        let body = Rect { y: inner.y + 1, height: inner.height - 1, ..inner };
        self.height = body.height as usize;
        self.clamp();
        let width = inner.width as usize;
        let num_w = self
            .rows
            .iter()
            .filter_map(|r| r.left.as_ref().or(r.right.as_ref()).map(|x| x.0))
            .max()
            .unwrap_or(1)
            .to_string()
            .len()
            .max(3);
        let style_for = |tag: Tag| match tag {
            Tag::Equal => t.viewer,
            Tag::Delete => t.diff_del,
            Tag::Insert => t.diff_add,
            Tag::Change => t.diff_change,
        };
        if self.unified {
            let title = format!("--- {}   +++ {}", self.left_title, self.right_title);
            f.render_widget(
                Paragraph::new(Span::styled(pad_right(&crate::util::trunc_mid(&title, width), width), t.header)),
                header,
            );
            let mut lines = Vec::new();
            for r in &self.rows {
                let mut push = |sign: char, n: usize, s: &str, st: Style| {
                    let prefix = format!("{sign}{:>num_w$} ", n);
                    let avail = width.saturating_sub(prefix.len());
                    lines.push(Line::from(vec![
                        Span::styled(prefix, t.editor_line_nr.patch(Style::default().fg(st.fg.unwrap_or_default()))),
                        Span::styled(pad_right(&hslice(s, self.hscroll, avail), avail), st),
                    ]));
                };
                match r.tag {
                    Tag::Equal => {
                        let (n, s) = r.left.as_ref().unwrap();
                        push(' ', *n, s, t.viewer);
                    }
                    Tag::Delete => {
                        let (n, s) = r.left.as_ref().unwrap();
                        push('-', *n, s, t.diff_del);
                    }
                    Tag::Insert => {
                        let (n, s) = r.right.as_ref().unwrap();
                        push('+', *n, s, t.diff_add);
                    }
                    Tag::Change => {
                        let (n, s) = r.left.as_ref().unwrap();
                        push('-', *n, s, t.diff_del);
                        let (n, s) = r.right.as_ref().unwrap();
                        push('+', *n, s, t.diff_add);
                    }
                }
            }
            let shown: Vec<Line> = lines.into_iter().skip(self.scroll).take(self.height).collect();
            f.render_widget(Paragraph::new(shown), body);
            return;
        }
        let half = width.saturating_sub(1) / 2;
        let right_w = width.saturating_sub(half + 1);
        let lt = crate::util::trunc_left(&self.left_title, half);
        let rt = crate::util::trunc_left(&self.right_title, right_w);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(pad_right(&lt, half), t.header),
                Span::styled("│", t.border),
                Span::styled(pad_right(&rt, right_w), t.header),
            ])),
            header,
        );
        let mut lines = Vec::new();
        for r in self.rows.iter().skip(self.scroll).take(self.height) {
            let st = style_for(r.tag);
            let mut spans = Vec::new();
            for (side, w) in [(0, half), (1, right_w)] {
                if side == 1 {
                    spans.push(Span::styled("│", t.border));
                }
                let (cell, other) = if side == 0 { (&r.left, &r.right) } else { (&r.right, &r.left) };
                match cell {
                    Some((n, s)) => {
                        let num = format!("{:>num_w$} ", n);
                        let avail = w.saturating_sub(num.len());
                        spans.push(Span::styled(num, t.editor_line_nr));
                        if r.tag == Tag::Change {
                            let o = other.as_ref().map(|x| x.1.as_str()).unwrap_or("");
                            spans.extend(inline_spans(s, o, side == 0, st, self.hscroll, avail));
                        } else {
                            spans.push(Span::styled(pad_right(&hslice(s, self.hscroll, avail), avail), st));
                        }
                    }
                    None => spans.push(Span::styled(" ".repeat(w), t.viewer.add_modifier(Modifier::DIM))),
                }
            }
            lines.push(Line::from(spans));
        }
        while lines.len() < self.height {
            lines.push(Line::from(vec![
                Span::styled(" ".repeat(half), t.viewer),
                Span::styled("│", t.border),
                Span::styled(" ".repeat(right_w), t.viewer),
            ]));
        }
        f.render_widget(Paragraph::new(lines), body);
    }

    fn key(&mut self, key: KeyEvent) -> ScreenOutcome {
        let page = self.height.max(1);
        match key.code {
            KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q') => return ScreenOutcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Enter => self.scroll += 1,
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll += page,
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = usize::MAX / 2,
            KeyCode::Left => self.hscroll = self.hscroll.saturating_sub(8),
            KeyCode::Right => self.hscroll += 8,
            KeyCode::Char('n') => self.jump(true),
            KeyCode::Char('p') | KeyCode::Char('N') => self.jump(false),
            KeyCode::F(7) if key.modifiers.contains(KeyModifiers::SHIFT) => self.jump(false),
            KeyCode::F(7) => self.jump(true),
            KeyCode::F(17) => self.jump(false),
            KeyCode::Char('u') => {
                let row = self.view_row(self.scroll);
                self.unified = !self.unified;
                self.scroll = self.view_pos(row.min(self.rows.len()));
            }
            _ => {}
        }
        self.clamp();
        ScreenOutcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> ScreenOutcome {
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => self.scroll += 3,
            _ => {}
        }
        self.clamp();
        ScreenOutcome::Keep
    }

    fn title(&self) -> String {
        format!("Diff {} {}", self.left_title, self.right_title)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
