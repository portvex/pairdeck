//! F4 internal editor (mcedit style) with syntax highlighting, undo, search and replace.

use crate::app::App;
use crate::dialog::{self, Form};
use crate::screen::{Screen, ScreenOutcome};
use crate::ui::Ctx;
use crate::util::{pad_right, str_width, trunc_left};
use crate::vfs::Location;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use std::any::Any;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use unicode_width::UnicodeWidthChar;

const MAX_LOAD: u64 = 64 * 1024 * 1024;
/// Lines highlighted above the viewport to establish parser state.
const HL_LOOKBEHIND: usize = 400;

fn clipboard() -> &'static Mutex<String> {
    static C: OnceLock<Mutex<String>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(String::new()))
}

type Pos = (usize, usize); // (line, char index)

#[derive(Clone, Debug)]
enum Op {
    Insert { at: Pos, text: String },
    Delete { at: Pos, text: String },
}

#[derive(Clone, Debug)]
struct Group {
    ops: Vec<Op>,
    before: Pos,
    after: Pos,
    /// Typing groups can be extended by more typing.
    typing: bool,
}

#[derive(Clone, Debug, Default)]
struct SearchOpts {
    pattern: String,
    replace: Option<String>,
    case: bool,
    regex: bool,
    word: bool,
    backwards: bool,
}

pub struct Editor {
    loc: Location,
    path: PathBuf,
    name: String,
    lines: Vec<String>,
    crlf: bool,
    final_newline: bool,
    mode: u32,
    is_new: bool,
    cy: usize,
    cx: usize,
    want_col: Option<usize>,
    top: usize,
    left: usize,
    overwrite: bool,
    anchor: Option<Pos>,
    mark_mode: bool,
    undo: Vec<Group>,
    redo: Vec<Group>,
    version: u64,
    saved_version: u64,
    search: Option<SearchOpts>,
    message: Option<String>,
    tab: usize,
    use_tabs: bool,
    auto_indent: bool,
    line_numbers: bool,
    height: usize,
    text_area: Rect,
    gutter: usize,
    hl_cache: Option<(u64, usize, usize, String, Vec<Vec<Style>>)>,
    dragging: bool,
}

pub fn open(app: &mut App, loc: &Location, path: &Path) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let stat = loc.vfs.lock().stat(path);
    let (data, mode, is_new) = match stat {
        Ok(e) if e.is_dir() => {
            app.error(format!("{} is a directory", path.display()));
            return;
        }
        Ok(e) => {
            if e.size > MAX_LOAD {
                app.error(format!("File too large to edit ({} bytes)", crate::util::grouped(e.size)));
                return;
            }
            let res = loc.vfs.lock().open_read(path).and_then(|mut r| {
                let mut buf = Vec::new();
                r.read_to_end(&mut buf)?;
                Ok(buf)
            });
            match res {
                Ok(d) => (d, e.mode & 0o7777, false),
                Err(err) => {
                    app.error(format!("Cannot open {}:\n{err:#}", path.display()));
                    return;
                }
            }
        }
        Err(_) => (Vec::new(), 0o644, true),
    };
    let cfg = &app.cfg;
    let ed = Editor::from_bytes(
        loc.clone(),
        path.to_path_buf(),
        name,
        &data,
        mode,
        is_new,
        cfg.tab_size.max(1),
        cfg.editor_auto_indent,
        cfg.editor_line_numbers,
    );
    if crate::util::looks_binary(&data) {
        let loc2 = loc.clone();
        let p2 = path.to_path_buf();
        let d = dialog::choice(
            "Binary file",
            &format!("\"{}\" looks like a binary file.\nEditing may corrupt it.", ed.name),
            &["View", "Edit anyway", "Cancel"],
            move |app, b| match b {
                0 => crate::viewer::open(app, &loc2, &p2),
                1 => app.push_screen(Box::new(ed)),
                _ => {}
            },
        );
        app.push_dialog(d);
        return;
    }
    app.push_screen(Box::new(ed));
}

fn byte_idx(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Editor {
    #[allow(clippy::too_many_arguments)]
    fn from_bytes(
        loc: Location,
        path: PathBuf,
        name: String,
        data: &[u8],
        mode: u32,
        is_new: bool,
        tab: usize,
        auto_indent: bool,
        line_numbers: bool,
    ) -> Editor {
        let text = String::from_utf8_lossy(data).into_owned();
        let crlf = text.contains("\r\n");
        let final_newline = is_new || text.ends_with('\n');
        let mut lines: Vec<String> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect();
        if final_newline && lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        let use_tabs = lines.iter().any(|l| l.starts_with('\t'));
        Editor {
            loc,
            path,
            name,
            lines,
            crlf,
            final_newline,
            mode,
            is_new,
            cy: 0,
            cx: 0,
            want_col: None,
            top: 0,
            left: 0,
            overwrite: false,
            anchor: None,
            mark_mode: false,
            undo: Vec::new(),
            redo: Vec::new(),
            version: 0,
            saved_version: 0,
            search: None,
            message: if is_new { Some("New file".into()) } else { None },
            tab,
            use_tabs,
            auto_indent,
            line_numbers,
            height: 20,
            text_area: Rect::default(),
            gutter: 0,
            hl_cache: None,
            dragging: false,
        }
    }

    // ------------------------------------------------------------ primitives

    fn line_len(&self, y: usize) -> usize {
        char_len(&self.lines[y])
    }

    fn clamp_pos(&self, p: Pos) -> Pos {
        let y = p.0.min(self.lines.len() - 1);
        (y, p.1.min(self.line_len(y)))
    }

    /// Insert text (may contain newlines) at `at`; returns the end position.
    fn raw_insert(&mut self, at: Pos, text: &str) -> Pos {
        let (y, x) = at;
        let line = &self.lines[y];
        let b = byte_idx(line, x);
        let tail = line[b..].to_string();
        let head = line[..b].to_string();
        let parts: Vec<&str> = text.split('\n').collect();
        if parts.len() == 1 {
            self.lines[y] = format!("{head}{text}{tail}");
            return (y, x + char_len(text));
        }
        self.lines[y] = format!("{head}{}", parts[0]);
        let mut new_lines: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        let last = new_lines.len() - 1;
        let end_x = char_len(&new_lines[last]);
        new_lines[last].push_str(&tail);
        let n = new_lines.len();
        self.lines.splice(y + 1..y + 1, new_lines);
        (y + n, end_x)
    }

    /// Delete the text between two ordered positions; returns it.
    fn raw_delete(&mut self, a: Pos, b: Pos) -> String {
        if a == b {
            return String::new();
        }
        let (ay, ax) = a;
        let (by, bx) = b;
        if ay == by {
            let line = &self.lines[ay];
            let s = byte_idx(line, ax);
            let e = byte_idx(line, bx);
            let removed = line[s..e].to_string();
            self.lines[ay].replace_range(s..e, "");
            return removed;
        }
        let first = &self.lines[ay];
        let s = byte_idx(first, ax);
        let mut removed = first[s..].to_string();
        for l in &self.lines[ay + 1..by] {
            removed.push('\n');
            removed.push_str(l);
        }
        let last = &self.lines[by];
        let e = byte_idx(last, bx);
        removed.push('\n');
        removed.push_str(&last[..e]);
        let tail = last[e..].to_string();
        self.lines[ay].truncate(s);
        self.lines[ay].push_str(&tail);
        self.lines.drain(ay + 1..=by);
        removed
    }

    fn end_of(at: Pos, text: &str) -> Pos {
        let parts: Vec<&str> = text.split('\n').collect();
        if parts.len() == 1 {
            (at.0, at.1 + char_len(text))
        } else {
            (at.0 + parts.len() - 1, char_len(parts[parts.len() - 1]))
        }
    }

    fn record(&mut self, op: Op, before: Pos, typing: bool) {
        self.redo.clear();
        self.version += 1;
        self.hl_cache = None;
        let after = (self.cy, self.cx);
        if typing
            && let Some(g) = self.undo.last_mut()
            && g.typing
            && g.after == before
        {
            g.ops.push(op);
            g.after = after;
            return;
        }
        self.undo.push(Group { ops: vec![op], before, after, typing });
        if self.undo.len() > 2000 {
            self.undo.remove(0);
        }
    }

    /// Start a compound group that subsequent edits join (until `end_group`).
    fn begin_group(&mut self) {
        self.redo.clear();
        let p = (self.cy, self.cx);
        self.undo.push(Group { ops: Vec::new(), before: p, after: p, typing: false });
    }

    fn push_op(&mut self, op: Op) {
        self.version += 1;
        self.hl_cache = None;
        if let Some(g) = self.undo.last_mut() {
            g.ops.push(op);
        }
    }

    fn end_group(&mut self) {
        let p = (self.cy, self.cx);
        if let Some(g) = self.undo.last_mut() {
            g.after = p;
            if g.ops.is_empty() {
                self.undo.pop();
            }
        }
    }

    fn insert_at_cursor(&mut self, text: &str, typing: bool) {
        let before = (self.cy, self.cx);
        let end = self.raw_insert(before, text);
        self.cy = end.0;
        self.cx = end.1;
        self.want_col = None;
        self.record(Op::Insert { at: before, text: text.to_string() }, before, typing);
    }

    fn delete_range(&mut self, a: Pos, b: Pos, typing: bool) -> String {
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let before = (self.cy, self.cx);
        let text = self.raw_delete(a, b);
        self.cy = a.0;
        self.cx = a.1;
        self.want_col = None;
        if !text.is_empty() {
            self.record(Op::Delete { at: a, text: text.clone() }, before, typing);
        }
        text
    }

    fn apply_undo(&mut self, redo: bool) {
        let g = if redo { self.redo.pop() } else { self.undo.pop() };
        let Some(g) = g else {
            self.message = Some(if redo { "Nothing to redo".into() } else { "Nothing to undo".into() });
            return;
        };
        if redo {
            for op in &g.ops {
                match op {
                    Op::Insert { at, text } => {
                        self.raw_insert(*at, text);
                    }
                    Op::Delete { at, text } => {
                        let end = Self::end_of(*at, text);
                        self.raw_delete(*at, end);
                    }
                }
            }
            (self.cy, self.cx) = self.clamp_pos(g.after);
            self.undo.push(g);
        } else {
            for op in g.ops.iter().rev() {
                match op {
                    Op::Insert { at, text } => {
                        let end = Self::end_of(*at, text);
                        self.raw_delete(*at, end);
                    }
                    Op::Delete { at, text } => {
                        self.raw_insert(*at, text);
                    }
                }
            }
            (self.cy, self.cx) = self.clamp_pos(g.before);
            self.redo.push(g);
        }
        self.anchor = None;
        self.version += 1;
        self.hl_cache = None;
        self.want_col = None;
    }

    // ------------------------------------------------------------ selection

    fn selection(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        let c = (self.cy, self.cx);
        if a == c {
            return None;
        }
        Some(if a < c { (a, c) } else { (c, a) })
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection()?;
        if a.0 == b.0 {
            let l = &self.lines[a.0];
            return Some(l[byte_idx(l, a.1)..byte_idx(l, b.1)].to_string());
        }
        let mut s = self.lines[a.0][byte_idx(&self.lines[a.0], a.1)..].to_string();
        for l in &self.lines[a.0 + 1..b.0] {
            s.push('\n');
            s.push_str(l);
        }
        s.push('\n');
        let last = &self.lines[b.0];
        s.push_str(&last[..byte_idx(last, b.1)]);
        Some(s)
    }

    fn delete_selection(&mut self) -> bool {
        if let Some((a, b)) = self.selection() {
            self.delete_range(a, b, false);
            self.anchor = None;
            self.mark_mode = false;
            true
        } else {
            false
        }
    }

    fn copy(&mut self) -> bool {
        if let Some(t) = self.selected_text() {
            crate::commands::osc52_copy(&t);
            let n = t.lines().count().max(1);
            *clipboard().lock().unwrap() = t;
            self.message = Some(format!("Copied {n} line(s)"));
            true
        } else {
            false
        }
    }

    fn paste_text(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        if text.is_empty() {
            return;
        }
        self.begin_group();
        if let Some((a, b)) = self.selection() {
            let t = self.raw_delete(a, b);
            (self.cy, self.cx) = a;
            self.push_op(Op::Delete { at: a, text: t });
            self.anchor = None;
        }
        let at = (self.cy, self.cx);
        let end = self.raw_insert(at, &text);
        self.push_op(Op::Insert { at, text });
        (self.cy, self.cx) = end;
        self.end_group();
        self.want_col = None;
    }

    // ------------------------------------------------------------ movement

    fn disp_col(&self, y: usize, x: usize) -> usize {
        let mut col = 0;
        for c in self.lines[y].chars().take(x) {
            col += self.cell_width(c, col);
        }
        col
    }

    fn cell_width(&self, c: char, col: usize) -> usize {
        if c == '\t' {
            self.tab - col % self.tab
        } else {
            c.width().unwrap_or(1).max(if c.is_control() { 1 } else { 0 })
        }
    }

    fn x_for_col(&self, y: usize, target: usize) -> usize {
        let mut col = 0;
        for (i, c) in self.lines[y].chars().enumerate() {
            let w = self.cell_width(c, col);
            if col + w > target {
                return i;
            }
            col += w;
        }
        self.line_len(y)
    }

    fn move_to(&mut self, y: usize, x: usize, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some((self.cy, self.cx));
            }
        } else if !self.mark_mode {
            self.anchor = None;
        }
        let (y, x) = self.clamp_pos((y, x));
        self.cy = y;
        self.cx = x;
    }

    fn vertical(&mut self, d: isize, select: bool) {
        let want = self.want_col.unwrap_or_else(|| self.disp_col(self.cy, self.cx));
        let y = (self.cy as isize + d).clamp(0, self.lines.len() as isize - 1) as usize;
        let x = self.x_for_col(y, want);
        self.move_to(y, x, select);
        self.want_col = Some(want);
    }

    fn word_left(&self) -> Pos {
        let (mut y, mut x) = (self.cy, self.cx);
        if x == 0 {
            if y == 0 {
                return (0, 0);
            }
            y -= 1;
            return (y, self.line_len(y));
        }
        let chars: Vec<char> = self.lines[y].chars().collect();
        while x > 0 && !is_word(chars[x - 1]) {
            x -= 1;
        }
        while x > 0 && is_word(chars[x - 1]) {
            x -= 1;
        }
        (y, x)
    }

    fn word_right(&self) -> Pos {
        let (y, mut x) = (self.cy, self.cx);
        let chars: Vec<char> = self.lines[y].chars().collect();
        if x >= chars.len() {
            return if y + 1 < self.lines.len() { (y + 1, 0) } else { (y, x) };
        }
        while x < chars.len() && is_word(chars[x]) {
            x += 1;
        }
        while x < chars.len() && !is_word(chars[x]) {
            x += 1;
        }
        (y, x)
    }

    fn smart_home(&self) -> usize {
        let indent = self.lines[self.cy].chars().take_while(|c| c.is_whitespace()).count();
        if self.cx == indent { 0 } else { indent }
    }

    fn scroll_into_view(&mut self) {
        let h = self.height.max(1);
        if self.cy < self.top {
            self.top = self.cy;
        } else if self.cy >= self.top + h {
            self.top = self.cy + 1 - h;
        }
        let w = (self.text_area.width as usize).max(10);
        let col = self.disp_col(self.cy, self.cx);
        if col < self.left {
            self.left = col.saturating_sub(4);
        } else if col >= self.left + w {
            self.left = (col + 8).saturating_sub(w);
        }
    }

    // ------------------------------------------------------------ editing commands

    fn type_char(&mut self, c: char) {
        if self.selection().is_some() {
            self.delete_selection();
        }
        if self.overwrite && self.cx < self.line_len(self.cy) {
            let before = (self.cy, self.cx);
            self.begin_group();
            let t = self.raw_delete(before, (self.cy, self.cx + 1));
            self.push_op(Op::Delete { at: before, text: t });
            let end = self.raw_insert(before, &c.to_string());
            self.push_op(Op::Insert { at: before, text: c.to_string() });
            (self.cy, self.cx) = end;
            self.end_group();
            return;
        }
        self.insert_at_cursor(&c.to_string(), !c.is_whitespace());
        if let Some(open) = match c {
            '}' => Some('{'),
            ')' => Some('('),
            ']' => Some('['),
            _ => None,
        } {
            // Outdent a lone closing bracket to the matching line's indent.
            let line = self.lines[self.cy].clone();
            if line.trim() == c.to_string()
                && self.auto_indent
                && let Some((my, _)) = self.find_match_bracket((self.cy, self.cx - 1), c, open)
            {
                let indent: String = self.lines[my].chars().take_while(|ch| ch.is_whitespace()).collect();
                let cur_ws = char_len(&line) - 1;
                if indent != line[..line.len() - 1] {
                    self.begin_group();
                    let t = self.raw_delete((self.cy, 0), (self.cy, cur_ws));
                    self.push_op(Op::Delete { at: (self.cy, 0), text: t });
                    self.raw_insert((self.cy, 0), &indent);
                    self.push_op(Op::Insert { at: (self.cy, 0), text: indent.clone() });
                    self.cx = char_len(&indent) + 1;
                    self.end_group();
                }
            }
        }
    }

    fn newline(&mut self) {
        self.delete_selection();
        let mut text = String::from("\n");
        if self.auto_indent {
            let line = &self.lines[self.cy];
            let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            let indent_len = char_len(&indent).min(self.cx);
            text.push_str(&indent.chars().take(indent_len).collect::<String>());
            let before: String = line.chars().take(self.cx).collect();
            if before.trim_end().ends_with(['{', '(', '[', ':']) {
                text.push_str(&self.indent_unit());
            }
        }
        self.insert_at_cursor(&text, false);
    }

    fn indent_unit(&self) -> String {
        if self.use_tabs { "\t".into() } else { " ".repeat(self.tab) }
    }

    fn tab_key(&mut self, shift: bool) {
        if let Some((a, b)) = self.selection() {
            let last = if b.1 == 0 && b.0 > a.0 { b.0 - 1 } else { b.0 };
            let unit = self.indent_unit();
            self.begin_group();
            for y in a.0..=last {
                if shift {
                    let line = &self.lines[y];
                    let n = if line.starts_with('\t') {
                        1
                    } else {
                        line.chars().take(self.tab).take_while(|c| *c == ' ').count()
                    };
                    if n > 0 {
                        let t = self.raw_delete((y, 0), (y, n));
                        self.push_op(Op::Delete { at: (y, 0), text: t });
                    }
                } else if !self.lines[y].is_empty() {
                    self.raw_insert((y, 0), &unit);
                    self.push_op(Op::Insert { at: (y, 0), text: unit.clone() });
                }
            }
            self.anchor = Some((a.0, 0));
            self.cy = last;
            self.cx = self.line_len(last);
            self.end_group();
            return;
        }
        if shift {
            let line = &self.lines[self.cy];
            let n =
                if line.starts_with('\t') { 1 } else { line.chars().take(self.tab).take_while(|c| *c == ' ').count() };
            if n > 0 {
                let keep = self.cx.saturating_sub(n);
                self.delete_range((self.cy, 0), (self.cy, n), false);
                self.cx = keep;
            }
            return;
        }
        if self.use_tabs {
            self.insert_at_cursor("\t", false);
        } else {
            let col = self.disp_col(self.cy, self.cx);
            let n = self.tab - col % self.tab;
            self.insert_at_cursor(&" ".repeat(n), false);
        }
    }

    fn backspace(&mut self, word: bool) {
        if self.delete_selection() {
            return;
        }
        let cur = (self.cy, self.cx);
        if cur == (0, 0) {
            return;
        }
        let from = if word {
            self.word_left()
        } else if self.cx == 0 {
            (self.cy - 1, self.line_len(self.cy - 1))
        } else {
            // Remove a whole indent unit of spaces when in leading whitespace.
            let line = &self.lines[self.cy];
            let before: String = line.chars().take(self.cx).collect();
            if !self.use_tabs && !before.is_empty() && before.chars().all(|c| c == ' ') {
                let n = (self.cx - 1) % self.tab + 1;
                (self.cy, self.cx - n)
            } else {
                (self.cy, self.cx - 1)
            }
        };
        self.delete_range(from, cur, !word);
    }

    fn delete_forward(&mut self, word: bool) {
        if self.delete_selection() {
            return;
        }
        let cur = (self.cy, self.cx);
        let to = if word {
            self.word_right()
        } else if self.cx >= self.line_len(self.cy) {
            if self.cy + 1 >= self.lines.len() {
                return;
            }
            (self.cy + 1, 0)
        } else {
            (self.cy, self.cx + 1)
        };
        self.delete_range(cur, to, false);
        (self.cy, self.cx) = cur;
    }

    fn delete_line(&mut self) {
        self.anchor = None;
        let y = self.cy;
        let (a, b) = if y + 1 < self.lines.len() {
            ((y, 0), (y + 1, 0))
        } else if y > 0 {
            ((y - 1, self.line_len(y - 1)), (y, self.line_len(y)))
        } else {
            ((0, 0), (0, self.line_len(0)))
        };
        let t = self.delete_range(a, b, false);
        *clipboard().lock().unwrap() =
            t.trim_start_matches('\n').to_string() + if t.ends_with('\n') { "" } else { "\n" };
        self.cy = y.min(self.lines.len() - 1);
        self.cx = 0;
    }

    fn duplicate_line(&mut self) {
        let line = self.lines[self.cy].clone();
        let x = self.cx;
        let at = (self.cy, self.line_len(self.cy));
        self.cy = at.0;
        self.cx = at.1;
        self.insert_at_cursor(&format!("\n{line}"), false);
        self.cx = x;
    }

    /// MC F5 / F6: copy or move the selected block to the cursor.
    fn block_to_cursor(&mut self, mv: bool) {
        let Some((a, b)) = self.selection() else {
            self.message = Some("No block marked (F3 or Shift+arrows)".into());
            return;
        };
        let cur = (self.cy, self.cx);
        if cur > a && cur < b {
            self.message = Some("Cannot copy a block into itself".into());
            return;
        }
        let text = self.selected_text().unwrap_or_default();
        self.begin_group();
        let mut at = cur;
        if mv {
            let t = self.raw_delete(a, b);
            self.push_op(Op::Delete { at: a, text: t });
            if cur >= b {
                // Adjust the insertion point for the removed text.
                if cur.0 == b.0 {
                    at = (a.0, a.1 + (cur.1 - b.1));
                } else {
                    at = (cur.0 - (b.0 - a.0), cur.1);
                }
            }
        }
        let end = self.raw_insert(at, &text);
        self.push_op(Op::Insert { at, text });
        self.anchor = Some(at);
        (self.cy, self.cx) = end;
        self.end_group();
        self.mark_mode = false;
    }

    fn find_match_bracket(&self, at: Pos, c: char, other: char) -> Option<Pos> {
        let forward = matches!(c, '(' | '[' | '{');
        let mut depth = 0i32;
        let (mut y, mut x) = at;
        let mut steps = 0;
        loop {
            let chars: Vec<char> = self.lines[y].chars().collect();
            if x < chars.len() {
                let ch = chars[x];
                if ch == c {
                    depth += 1;
                } else if ch == other {
                    depth -= 1;
                    if depth == 0 {
                        return Some((y, x));
                    }
                }
            }
            steps += 1;
            if steps > 200_000 {
                return None;
            }
            if forward {
                x += 1;
                if x >= chars.len().max(1) {
                    y += 1;
                    x = 0;
                    if y >= self.lines.len() {
                        return None;
                    }
                }
            } else if x == 0 {
                if y == 0 {
                    return None;
                }
                y -= 1;
                x = self.line_len(y).saturating_sub(1);
                if self.line_len(y) == 0 {
                    continue;
                }
            } else {
                x -= 1;
            }
        }
    }

    fn bracket_pair(&self) -> Option<(Pos, Pos)> {
        let chars: Vec<char> = self.lines[self.cy].chars().collect();
        let try_at = |x: usize| -> Option<(Pos, Pos)> {
            let c = *chars.get(x)?;
            let other = match c {
                '(' => ')',
                ')' => '(',
                '[' => ']',
                ']' => '[',
                '{' => '}',
                '}' => '{',
                _ => return None,
            };
            let m = self.find_match_bracket((self.cy, x), c, other)?;
            Some(((self.cy, x), m))
        };
        try_at(self.cx).or_else(|| if self.cx > 0 { try_at(self.cx - 1) } else { None })
    }

    // ------------------------------------------------------------ search / replace

    fn build_regex(o: &SearchOpts) -> Result<regex::Regex, String> {
        let mut p = if o.regex { o.pattern.clone() } else { regex::escape(&o.pattern) };
        if o.word {
            p = format!(r"\b(?:{p})\b");
        }
        if !o.case {
            p = format!("(?i){p}");
        }
        regex::Regex::new(&p).map_err(|e| e.to_string())
    }

    /// Find the next match from the cursor; selects it. Returns the match range.
    fn find_next(&mut self, invert: bool) -> Option<(Pos, Pos)> {
        let o = self.search.clone()?;
        let re = match Self::build_regex(&o) {
            Ok(r) => r,
            Err(e) => {
                self.message = Some(format!("Bad regex: {e}"));
                return None;
            }
        };
        let back = o.backwards ^ invert;
        let n = self.lines.len();
        let (sy, sx) =
            if back { self.selection().map(|(a, _)| a).unwrap_or((self.cy, self.cx)) } else { (self.cy, self.cx) };
        for i in 0..=n {
            let y = if back { (sy + n * 2 - i) % n } else { (sy + i) % n };
            let line = &self.lines[y];
            let m = if back {
                let limit = if i == 0 { byte_idx(line, sx) } else { usize::MAX };
                re.find_iter(line).filter(|m| m.start() < limit && m.start() < m.end()).last()
            } else {
                let from = if i == 0 { byte_idx(line, sx) } else { 0 };
                if i == n && from == 0 {
                    None
                } else {
                    re.find_iter(&line[..]).find(|m| m.start() >= from && m.start() < m.end())
                }
            };
            if let Some(m) = m {
                let a = (y, char_len(&line[..m.start()]));
                let b = (y, char_len(&line[..m.end()]));
                self.anchor = Some(a);
                (self.cy, self.cx) = b;
                self.message = None;
                return Some((a, b));
            }
        }
        self.message = Some(format!("Search string not found: {}", o.pattern));
        None
    }

    fn replacement_for(&self, a: Pos, b: Pos) -> String {
        let Some(o) = &self.search else { return String::new() };
        let rep = o.replace.clone().unwrap_or_default();
        if !o.regex {
            return rep;
        }
        let line = &self.lines[a.0];
        let text = &line[byte_idx(line, a.1)..byte_idx(line, b.1)];
        match Self::build_regex(o) {
            Ok(re) => re.replace(text, rep.as_str()).into_owned(),
            Err(_) => rep,
        }
    }

    fn replace_current(&mut self, a: Pos, b: Pos) {
        let rep = self.replacement_for(a, b);
        self.begin_group();
        let t = self.raw_delete(a, b);
        self.push_op(Op::Delete { at: a, text: t });
        let end = self.raw_insert(a, &rep);
        self.push_op(Op::Insert { at: a, text: rep });
        (self.cy, self.cx) = end;
        self.anchor = None;
        self.end_group();
    }

    fn replace_all(&mut self) -> usize {
        let Some(o) = self.search.clone() else { return 0 };
        let Ok(re) = Self::build_regex(&o) else { return 0 };
        let rep = o.replace.clone().unwrap_or_default();
        self.begin_group();
        let mut count = 0;
        for y in 0..self.lines.len() {
            let line = self.lines[y].clone();
            if !re.is_match(&line) {
                continue;
            }
            let n = re.find_iter(&line).count();
            let new = if o.regex {
                re.replace_all(&line, rep.as_str()).into_owned()
            } else {
                re.replace_all(&line, regex::NoExpand(&rep)).into_owned()
            };
            if new != line {
                self.raw_delete((y, 0), (y, char_len(&line)));
                self.push_op(Op::Delete { at: (y, 0), text: line });
                self.raw_insert((y, 0), &new);
                self.push_op(Op::Insert { at: (y, 0), text: new });
                count += n;
            }
        }
        self.anchor = None;
        (self.cy, self.cx) = self.clamp_pos((self.cy, self.cx));
        self.end_group();
        count
    }

    // ------------------------------------------------------------ save

    fn content(&self) -> String {
        let nl = if self.crlf { "\r\n" } else { "\n" };
        let mut s = self.lines.join(nl);
        if self.final_newline {
            s.push_str(nl);
        }
        s
    }

    fn save_to(&mut self, path: &Path) -> anyhow::Result<()> {
        let data = self.content();
        let mut w = self.loc.vfs.lock().create_write(path, self.mode)?;
        w.write_all(data.as_bytes())?;
        w.flush()?;
        drop(w);
        self.path = path.to_path_buf();
        self.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.saved_version = self.version;
        self.is_new = false;
        self.hl_cache = None;
        self.message = Some(format!("Saved {} lines to {}", self.lines.len(), self.name));
        Ok(())
    }

    fn save(&mut self) -> Result<(), String> {
        let p = self.path.clone();
        self.save_to(&p).map_err(|e| format!("Cannot save {}:\n{e:#}", p.display()))
    }

    // ------------------------------------------------------------ highlighting

    fn styles_for_view(&mut self, theme: &str, rows: usize) -> Option<&Vec<Vec<Style>>> {
        let valid = matches!(&self.hl_cache, Some((v, t, r, th, _)) if *v == self.version && *t == self.top && *r == rows && th == theme);
        if !valid {
            let first = self.lines.first().map(|s| s.as_str()).unwrap_or("");
            let inc = crate::highlight::Incremental::new(&self.name, first, theme)?;
            let mut h = inc.highlighter();
            let ss = crate::highlight::syntax_set();
            let start = self.top.saturating_sub(HL_LOOKBEHIND);
            let end = (self.top + rows).min(self.lines.len());
            let mut out = Vec::with_capacity(end.saturating_sub(self.top));
            for y in start..end {
                let line = format!("{}\n", self.lines[y]);
                let ranges = h.highlight_line(&line, ss).ok()?;
                if y >= self.top {
                    let mut st = Vec::with_capacity(line.len());
                    for (s, text) in ranges {
                        let style = crate::highlight::convert_style(s, false);
                        for _ in text.chars() {
                            st.push(style);
                        }
                    }
                    out.push(st);
                }
            }
            self.hl_cache = Some((self.version, self.top, rows, theme.to_string(), out));
        }
        self.hl_cache.as_ref().map(|c| &c.4)
    }

    // ------------------------------------------------------------ dialogs

    fn search_dialog(&self, replace: bool) -> ScreenOutcome {
        let prev = self.search.clone().unwrap_or_default();
        let initial = self.selected_text().filter(|t| !t.contains('\n')).unwrap_or_else(|| prev.pattern.clone());
        ScreenOutcome::Then(Box::new(move |app: &mut App| {
            let hs = app.state.input_hist("search").clone();
            let hr = app.state.input_hist("replace").clone();
            let mut form = Form::new(if replace { "Replace" } else { "Search" }).width(60).input_hist(
                "p",
                "Search for:",
                &initial,
                "search",
                hs,
            );
            if replace {
                form = form.input_hist("r", "Replace with:", prev.replace.as_deref().unwrap_or(""), "replace", hr);
            }
            form = form
                .check("case", "Case sensitive", prev.case)
                .check("regex", "Regular expression", prev.regex)
                .check("word", "Whole words", prev.word)
                .check("back", "Backwards", prev.backwards);
            if replace {
                form = form.buttons(&["Replace", "All", "Cancel"], 0);
            } else {
                form = form.buttons(&["OK", "Cancel"], 0);
            }
            let form = form.on_submit(move |app, r| {
                let cancel = if replace { 2 } else { 1 };
                if r.button == cancel || r.text("p").is_empty() {
                    return;
                }
                let o = SearchOpts {
                    pattern: r.text("p"),
                    replace: if replace { Some(r.text("r")) } else { None },
                    case: r.check("case"),
                    regex: r.check("regex"),
                    word: r.check("word"),
                    backwards: r.check("back"),
                };
                if let Err(e) = Editor::build_regex(&o) {
                    app.error(format!("Bad regular expression:\n{e}"));
                    return;
                }
                let Some(ed) = app.top_screen::<Editor>() else { return };
                ed.search = Some(o);
                if replace && r.button == 1 {
                    let n = ed.replace_all();
                    ed.message = Some(format!("Replaced {n} occurrence(s)"));
                } else if replace {
                    replace_step(app);
                } else {
                    ed.find_next(false);
                }
            });
            app.push_dialog(Box::new(form));
        }))
    }

    fn save_as_dialog(&self) -> ScreenOutcome {
        let cur = self.loc.vfs.lock().display(&self.path);
        let cur = if self.loc.is_local() { self.path.display().to_string() } else { cur };
        ScreenOutcome::Then(Box::new(move |app: &mut App| {
            let d = dialog::input(app, "Save as", "File name:", &cur, "save_as", |app, name| {
                if name.trim().is_empty() {
                    return;
                }
                let Some(ed) = app.top_screen::<Editor>() else { return };
                let p = PathBuf::from(crate::util::expand_tilde(name.trim()));
                let p = if p.is_absolute() { p } else { ed.path.parent().unwrap_or(Path::new("/")).join(p) };
                let exists = ed.loc.vfs.lock().stat(&p).is_ok() && p != ed.path;
                let do_save = move |app: &mut App| {
                    if let Some(ed) = app.top_screen::<Editor>()
                        && let Err(e) = ed.save_to(&p)
                    {
                        let msg = format!("Cannot save:\n{e:#}");
                        app.error(msg);
                    }
                };
                if exists {
                    app.push_dialog(dialog::confirm("Save as", "File exists. Overwrite?", do_save));
                } else {
                    do_save(app);
                }
            });
            app.push_dialog(d);
        }))
    }

    fn goto_dialog(&self) -> ScreenOutcome {
        ScreenOutcome::Then(Box::new(|app: &mut App| {
            let d = dialog::input(app, "Goto line", "Line number (line:col allowed):", "", "goto_line", |app, s| {
                if let Some(ed) = app.top_screen::<Editor>() {
                    let mut it = s.trim().split(':');
                    let l: usize = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                    let c: usize = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(1);
                    if l > 0 {
                        ed.anchor = None;
                        let y = (l - 1).min(ed.lines.len() - 1);
                        ed.move_to(y, c.saturating_sub(1), false);
                        ed.top = y.saturating_sub(ed.height / 3);
                    }
                }
            });
            app.push_dialog(d);
        }))
    }

    fn quit(&mut self) -> ScreenOutcome {
        if !self.is_dirty() {
            return ScreenOutcome::Close;
        }
        let name = self.name.clone();
        ScreenOutcome::Then(Box::new(move |app: &mut App| {
            let d = dialog::choice(
                "Quit editor",
                &format!("File \"{name}\" was modified.\nSave before close?"),
                &["Yes", "No", "Cancel"],
                |app, b| match b {
                    0 => {
                        let res = app.top_screen::<Editor>().map(|e| e.save());
                        match res {
                            Some(Ok(())) => {
                                app.screens.pop();
                                app.reload_panels();
                            }
                            Some(Err(e)) => app.error(e),
                            None => {}
                        }
                    }
                    1 => {
                        app.screens.pop();
                        app.reload_panels();
                    }
                    _ => {}
                },
            );
            app.push_dialog(d);
        }))
    }
}

/// Interactive replace: select the next match and ask what to do.
fn replace_step(app: &mut App) {
    let Some(ed) = app.top_screen::<Editor>() else { return };
    let Some((a, b)) = ed.find_next(false) else { return };
    ed.top = if a.0 < ed.top || a.0 >= ed.top + ed.height { a.0.saturating_sub(ed.height / 3) } else { ed.top };
    let rep = ed.replacement_for(a, b);
    let text = format!("Replace this occurrence with\n\"{}\"?", crate::util::trunc_mid(&rep, 50));
    let d = dialog::choice("Confirm replace", &text, &["Replace", "All", "Skip", "Cancel"], move |app, choice| {
        let Some(ed) = app.top_screen::<Editor>() else { return };
        match choice {
            0 => {
                ed.replace_current(a, b);
                replace_step(app);
            }
            1 => {
                ed.replace_current(a, b);
                let n = ed.replace_all() + 1;
                ed.message = Some(format!("Replaced {n} occurrence(s)"));
            }
            2 => {
                ed.anchor = None;
                replace_step(app);
            }
            _ => ed.anchor = None,
        }
    });
    app.push_dialog(d);
}

const HELP: &str = "\
Movement      arrows, Ctrl-Left/Right words, Home (smart)/End,
              PgUp/PgDn, Ctrl-Home/Ctrl-End
Select        Shift+movement, F3 start/end mark, Ctrl-A all
Clipboard     Ctrl-C copy, Ctrl-X cut, Ctrl-V paste (also to system
              clipboard via OSC 52)
Blocks        F5 copy block to cursor, F6 move block, F8 delete block
Edit          Ctrl-Z undo, Ctrl-Y / Ctrl-Shift-Z redo, Insert overwrite
              Ctrl-K delete line, Ctrl-D duplicate line,
              Tab / Shift-Tab indent, Ctrl-Backspace/Del delete word
Search        F7 / Ctrl-F search, Shift-F7 / Ctrl-G find next,
              F4 / Ctrl-R replace
Goto          Ctrl-L or Alt-L go to line
File          F2 / Ctrl-S save, Shift-F2 / F12 / Ctrl-Alt-S save as
              F10 / Esc quit, F9 toggle line numbers";

impl Screen for Editor {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        if area.height < 3 {
            return;
        }
        let status = Rect { height: 1, ..area };
        let bar = Rect { y: area.bottom() - 1, height: 1, ..area };
        let body = Rect { y: area.y + 1, height: area.height - 2, ..area };
        self.height = body.height as usize;
        self.gutter = if self.line_numbers { self.lines.len().to_string().len().max(3) + 1 } else { 0 };
        self.text_area =
            Rect { x: body.x + self.gutter as u16, width: body.width.saturating_sub(self.gutter as u16), ..body };
        self.scroll_into_view();

        let base = ctx.theme.editor;
        let rows = body.height as usize;
        let width = self.text_area.width as usize;
        let sel = self.selection();
        let brackets = self.bracket_pair();
        let search_re = self.search.as_ref().and_then(|o| Editor::build_regex(o).ok());
        let theme_name = ctx.theme.syntax.clone();
        let styles = self.styles_for_view(&theme_name, rows).cloned();
        let mut out = Vec::with_capacity(rows);
        for r in 0..rows {
            let y = self.top + r;
            let mut spans: Vec<Span> = Vec::new();
            if y >= self.lines.len() {
                if self.gutter > 0 {
                    spans.push(Span::styled(" ".repeat(self.gutter), ctx.theme.editor_line_nr));
                }
                spans.push(Span::styled("~", ctx.theme.editor_line_nr));
                out.push(Line::from(spans));
                continue;
            }
            if self.gutter > 0 {
                let st = if y == self.cy {
                    ctx.theme.editor_line_nr.add_modifier(Modifier::BOLD)
                } else {
                    ctx.theme.editor_line_nr
                };
                spans.push(Span::styled(format!("{:>w$} ", y + 1, w = self.gutter - 1), st));
            }
            let line = &self.lines[y];
            // Search match ranges as char indices.
            let mut matches: Vec<(usize, usize)> = Vec::new();
            if let Some(re) = &search_re {
                for m in re.find_iter(line) {
                    if m.start() < m.end() {
                        matches.push((char_len(&line[..m.start()]), char_len(&line[..m.end()])));
                    }
                }
            }
            let line_styles = styles.as_ref().and_then(|s| s.get(r));
            let mut cells: Vec<(String, Style)> = Vec::new();
            let mut col = 0usize;
            for (ci, c) in line.chars().enumerate() {
                let w = self.cell_width(c, col);
                let mut st = match line_styles.and_then(|ls| ls.get(ci)) {
                    Some(s) => base.patch(*s),
                    None => base,
                };
                if matches.iter().any(|(a, b)| ci >= *a && ci < *b) {
                    st = ctx.theme.search_match;
                }
                if let Some((a, b)) = sel
                    && (y, ci) >= a
                    && (y, ci) < b
                {
                    st = ctx.theme.editor_selection;
                }
                if let Some((p, q)) = brackets
                    && (p == (y, ci) || q == (y, ci))
                {
                    st = st.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                }
                let text = if c == '\t' {
                    " ".repeat(w)
                } else if c.is_control() {
                    "?".to_string()
                } else {
                    c.to_string()
                };
                cells.push((text, st));
                col += w;
            }
            // Selection continues past the end of line when it spans lines.
            if let Some((a, b)) = sel {
                let eol = (y, char_len(line));
                if eol >= a && eol < b {
                    cells.push((" ".into(), ctx.theme.editor_selection));
                }
            }
            // Horizontal scroll.
            let mut c = 0usize;
            let mut cur = String::new();
            let mut cur_st: Option<Style> = None;
            let mut used = 0usize;
            for (text, st) in cells {
                let w = str_width(&text).max(if text.is_empty() { 0 } else { 1 });
                if c + w <= self.left {
                    c += w;
                    continue;
                }
                if used + w > width {
                    break;
                }
                if cur_st != Some(st) {
                    if let Some(s) = cur_st {
                        spans.push(Span::styled(std::mem::take(&mut cur), s));
                    }
                    cur_st = Some(st);
                }
                cur.push_str(&text);
                c += w;
                used += w;
            }
            if let Some(s) = cur_st {
                spans.push(Span::styled(cur, s));
            }
            out.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(out).style(base), body);

        // Cursor.
        let col = self.disp_col(self.cy, self.cx);
        if self.cy >= self.top && self.cy < self.top + rows && col >= self.left && col < self.left + width {
            let x = self.text_area.x + (col - self.left) as u16;
            let y = body.y + (self.cy - self.top) as u16;
            f.set_cursor_position((x, y));
        }

        // Status line.
        let w = area.width as usize;
        let flags =
            format!("{}{}", if self.is_dirty() { "[*]" } else { "[-]" }, if self.mark_mode { " [MARK]" } else { "" });
        let right = format!(
            " Ln {}/{} Col {}  {}  {}  {} ",
            self.cy + 1,
            self.lines.len(),
            col + 1,
            if self.overwrite { "OVR" } else { "INS" },
            if self.crlf { "CRLF" } else { "LF" },
            if self.use_tabs { "TAB" } else { "SPC" }
        );
        let left = match &self.message {
            Some(m) => format!(" {} {flags}  {m}", self.name),
            None => format!(
                " {} {flags}",
                trunc_left(&self.loc.vfs.lock().display(&self.path), w.saturating_sub(right.len() + 8))
            ),
        };
        let gap = w.saturating_sub(str_width(&left) + str_width(&right));
        let text = if gap == 0 {
            pad_right(&trunc_left(&left, w.saturating_sub(right.len())), w.saturating_sub(right.len())) + &right
        } else {
            format!("{left}{}{right}", " ".repeat(gap))
        };
        f.render_widget(Paragraph::new(Span::styled(text, ctx.theme.status)), status);
        crate::viewer::fkey_bar(
            f,
            bar,
            ctx,
            &["Help", "Save", "Mark", "Replac", "Copy", "Move", "Search", "Delete", "LineNr", "Quit"],
        );
    }

    fn key(&mut self, key: KeyEvent) -> ScreenOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let sel = shift || self.mark_mode;
        self.message = None;
        let page = self.height.max(1) as isize;
        match key.code {
            // ---- file / screen
            KeyCode::F(10) => return self.quit(),
            KeyCode::Esc => {
                if self.anchor.is_some() || self.mark_mode {
                    self.anchor = None;
                    self.mark_mode = false;
                } else {
                    return self.quit();
                }
            }
            KeyCode::F(1) => {
                return ScreenOutcome::Then(Box::new(|app| app.push_dialog(dialog::message("Editor keys", HELP))));
            }
            KeyCode::F(2) if shift => return self.save_as_dialog(),
            KeyCode::F(12) => return self.save_as_dialog(),
            KeyCode::Char('s') if ctrl && alt => return self.save_as_dialog(),
            KeyCode::F(2) | KeyCode::Char('s') if key.code == KeyCode::F(2) || ctrl => {
                if let Err(e) = self.save() {
                    return ScreenOutcome::Then(Box::new(move |app| app.error(e)));
                }
            }
            KeyCode::F(9) => self.line_numbers = !self.line_numbers,
            // ---- search
            KeyCode::F(7) if shift => {
                self.find_next(false);
            }
            KeyCode::F(17) => {
                self.find_next(false);
            }
            KeyCode::Char('g') if ctrl => {
                self.find_next(shift);
            }
            KeyCode::F(7) | KeyCode::Char('f') if key.code == KeyCode::F(7) || ctrl => {
                return self.search_dialog(false);
            }
            KeyCode::F(4) => return self.search_dialog(true),
            KeyCode::Char('r') if ctrl => return self.search_dialog(true),
            KeyCode::Char('l') if ctrl || alt => return self.goto_dialog(),
            // ---- undo
            KeyCode::Char('z') if ctrl && shift => self.apply_undo(true),
            KeyCode::Char('z') if ctrl => self.apply_undo(false),
            KeyCode::Char('y') if ctrl => self.apply_undo(true),
            // ---- clipboard / blocks
            KeyCode::Char('a') if ctrl => {
                self.anchor = Some((0, 0));
                self.cy = self.lines.len() - 1;
                self.cx = self.line_len(self.cy);
            }
            KeyCode::Char('c') if ctrl => {
                if !self.copy() {
                    self.message = Some("Nothing selected".into());
                }
            }
            KeyCode::Insert if ctrl => {
                self.copy();
            }
            KeyCode::Char('x') if ctrl => {
                if self.copy() {
                    self.delete_selection();
                }
            }
            KeyCode::Delete if shift => {
                if self.copy() {
                    self.delete_selection();
                }
            }
            KeyCode::Char('v') if ctrl => {
                let t = clipboard().lock().unwrap().clone();
                self.paste_text(&t);
            }
            KeyCode::Insert if shift => {
                let t = clipboard().lock().unwrap().clone();
                self.paste_text(&t);
            }
            KeyCode::F(3) => {
                if self.mark_mode {
                    self.mark_mode = false;
                } else {
                    self.mark_mode = true;
                    self.anchor = Some((self.cy, self.cx));
                }
            }
            KeyCode::F(5) => self.block_to_cursor(false),
            KeyCode::F(6) => self.block_to_cursor(true),
            KeyCode::F(8) => {
                if !self.delete_selection() {
                    self.delete_line();
                }
            }
            KeyCode::Char('k') if ctrl => self.delete_line(),
            KeyCode::Char('d') if ctrl => self.duplicate_line(),
            KeyCode::Insert => self.overwrite = !self.overwrite,
            // ---- movement
            KeyCode::Left if ctrl || alt => {
                let p = self.word_left();
                self.move_to(p.0, p.1, sel);
            }
            KeyCode::Right if ctrl || alt => {
                let p = self.word_right();
                self.move_to(p.0, p.1, sel);
            }
            KeyCode::Left => {
                if !sel && self.anchor.is_some() && !self.mark_mode {
                    let (a, _) = self.selection().unwrap_or(((self.cy, self.cx), (0, 0)));
                    self.anchor = None;
                    (self.cy, self.cx) = a;
                } else if self.cx > 0 {
                    self.move_to(self.cy, self.cx - 1, sel);
                } else if self.cy > 0 {
                    let y = self.cy - 1;
                    self.move_to(y, self.line_len(y), sel);
                }
                self.want_col = None;
            }
            KeyCode::Right => {
                if !sel && self.anchor.is_some() && !self.mark_mode {
                    let (_, b) = self.selection().unwrap_or(((0, 0), (self.cy, self.cx)));
                    self.anchor = None;
                    (self.cy, self.cx) = b;
                } else if self.cx < self.line_len(self.cy) {
                    self.move_to(self.cy, self.cx + 1, sel);
                } else if self.cy + 1 < self.lines.len() {
                    self.move_to(self.cy + 1, 0, sel);
                }
                self.want_col = None;
            }
            KeyCode::Up if ctrl => self.top = self.top.saturating_sub(1),
            KeyCode::Down if ctrl => self.top = (self.top + 1).min(self.lines.len().saturating_sub(1)),
            KeyCode::Up => self.vertical(-1, sel),
            KeyCode::Down => self.vertical(1, sel),
            KeyCode::PageUp if ctrl => self.move_to(0, 0, sel),
            KeyCode::PageDown if ctrl => {
                let y = self.lines.len() - 1;
                self.move_to(y, self.line_len(y), sel);
            }
            KeyCode::PageUp => {
                self.top = self.top.saturating_sub(page as usize);
                self.vertical(-page, sel);
            }
            KeyCode::PageDown => {
                self.top = (self.top + page as usize).min(self.lines.len().saturating_sub(1));
                self.vertical(page, sel);
            }
            KeyCode::Home if ctrl => self.move_to(0, 0, sel),
            KeyCode::End if ctrl => {
                let y = self.lines.len() - 1;
                self.move_to(y, self.line_len(y), sel);
            }
            KeyCode::Home => {
                let x = self.smart_home();
                self.move_to(self.cy, x, sel);
                self.want_col = None;
            }
            KeyCode::End => {
                self.move_to(self.cy, self.line_len(self.cy), sel);
                self.want_col = None;
            }
            // ---- editing
            KeyCode::Enter => self.newline(),
            KeyCode::Tab => self.tab_key(shift),
            KeyCode::BackTab => self.tab_key(true),
            KeyCode::Backspace => self.backspace(ctrl || alt),
            KeyCode::Delete => self.delete_forward(ctrl || alt),
            KeyCode::Char('h') if ctrl => self.backspace(false),
            KeyCode::Char('w') if ctrl => self.backspace(true),
            KeyCode::Char(c) if !ctrl && !alt => self.type_char(c),
            _ => {}
        }
        if !matches!(key.code, KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown) {
            self.want_col = None;
        }
        if ctrl && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            // Scroll the view; drag the cursor along if it leaves the viewport.
            let h = self.height.max(1);
            if self.cy < self.top {
                self.move_to(self.top, self.cx, false);
            } else if self.cy >= self.top + h {
                self.move_to(self.top + h - 1, self.cx, false);
            }
        }
        ScreenOutcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> ScreenOutcome {
        let ta = self.text_area;
        let pos_at = |ed: &Editor, col: u16, row: u16| -> Pos {
            let y = (ed.top + row.saturating_sub(ta.y) as usize).min(ed.lines.len() - 1);
            let c = ed.left + col.saturating_sub(ta.x) as usize;
            (y, ed.x_for_col(y, c))
        };
        match m.kind {
            MouseEventKind::ScrollUp => self.top = self.top.saturating_sub(3),
            MouseEventKind::ScrollDown => self.top = (self.top + 3).min(self.lines.len().saturating_sub(1)),
            MouseEventKind::Down(MouseButton::Left) if m.row >= ta.y && m.row < ta.bottom() => {
                let (y, x) = pos_at(self, m.column, m.row);
                self.mark_mode = false;
                self.anchor = None;
                self.move_to(y, x, false);
                self.anchor = Some((y, x));
                self.dragging = true;
                self.want_col = None;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                let (y, x) = pos_at(self, m.column, m.row.clamp(ta.y, ta.bottom().saturating_sub(1)));
                self.cy = y;
                self.cx = x;
                if m.row < ta.y {
                    self.top = self.top.saturating_sub(1);
                } else if m.row >= ta.bottom() {
                    self.top = (self.top + 1).min(self.lines.len().saturating_sub(1));
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.dragging = false;
                if self.anchor == Some((self.cy, self.cx)) {
                    self.anchor = None;
                }
            }
            _ => {}
        }
        // Keep the cursor visible after wheel scrolling by moving it into the viewport.
        if matches!(m.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
            let h = self.height.max(1);
            if self.cy < self.top {
                self.move_to(self.top, self.cx, false);
            } else if self.cy >= self.top + h {
                self.move_to(self.top + h - 1, self.cx, false);
            }
        }
        ScreenOutcome::Keep
    }

    fn paste(&mut self, text: &str) {
        self.paste_text(text);
    }

    fn title(&self) -> String {
        format!("Edit: {}", self.name)
    }

    fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
