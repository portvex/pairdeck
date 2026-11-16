//! Find file: name globs plus optional content search, in a background thread.

use crate::app::App;
use crate::dialog::{Dialog, Form, Outcome, centered, draw_box};
use crate::ui::Ctx;
use crate::util::{pad_right, str_width, tildify, trunc_left, trunc_mid};
use crate::vfs::Location;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};
use std::any::Any;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

const MAX_RESULTS: usize = 20_000;
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_HITS_PER_FILE: usize = 50;

#[derive(Clone, Debug)]
pub struct Params {
    pub start: String,
    pub names: String,
    pub name_case: bool,
    pub content: String,
    pub regex: bool,
    pub content_case: bool,
    pub whole_words: bool,
    pub recursive: bool,
    pub follow: bool,
    pub skip_hidden: bool,
    pub ignore: String,
}

impl Params {
    fn new(start: String) -> Self {
        Params {
            start,
            names: "*".into(),
            name_case: false,
            content: String::new(),
            regex: false,
            content_case: false,
            whole_words: false,
            recursive: true,
            follow: false,
            skip_hidden: false,
            ignore: ".git;node_modules;target".into(),
        }
    }
}

#[derive(Clone, Debug)]
struct Hit {
    path: PathBuf,
    line: Option<(usize, String)>,
}

#[derive(Default)]
struct Shared {
    hits: Mutex<Vec<Hit>>,
    current: Mutex<String>,
    scanned: AtomicUsize,
    done: AtomicBool,
    stop: AtomicBool,
    error: Mutex<Option<String>>,
}

pub fn open(app: &mut App) {
    let Some(dir) = app.panel().loc.local_path().map(Path::to_path_buf) else {
        app.error("Find file works on local directories only");
        return;
    };
    open_with(app, Params::new(dir.display().to_string()));
}

fn open_with(app: &mut App, p: Params) {
    let hs = app.state.input_hist("find_start").clone();
    let hn = app.state.input_hist("find_name").clone();
    let hc = app.state.input_hist("find_content").clone();
    let form = Form::new("Find File")
        .width(66)
        .input_hist("start", "Start at:", &p.start, "find_start", hs)
        .input_hist("names", "File name (globs separated by ; e.g. *.rs;*.toml):", &p.names, "find_name", hn)
        .check("name_case", "Case sensitive file name", p.name_case)
        .separator()
        .input_hist("content", "Content (empty = do not search inside files):", &p.content, "find_content", hc)
        .check("regex", "Regular expression", p.regex)
        .check("content_case", "Case sensitive content", p.content_case)
        .check("whole", "Whole words", p.whole_words)
        .separator()
        .check("recursive", "Find recursively", p.recursive)
        .check("follow", "Follow symlinks", p.follow)
        .check("hidden", "Skip hidden files and directories", p.skip_hidden)
        .input("ignore", "Ignore directories (separated by ;):", &p.ignore)
        .buttons(&["OK", "Cancel"], 0)
        .on_submit(|app, r| {
            if r.button != 0 {
                return;
            }
            let p = Params {
                start: r.text("start"),
                names: r.text("names"),
                name_case: r.check("name_case"),
                content: r.text("content"),
                regex: r.check("regex"),
                content_case: r.check("content_case"),
                whole_words: r.check("whole"),
                recursive: r.check("recursive"),
                follow: r.check("follow"),
                skip_hidden: r.check("hidden"),
                ignore: r.text("ignore"),
            };
            start_search(app, p);
        });
    app.push_dialog(Box::new(form));
}

fn build_regex(p: &Params) -> anyhow::Result<Option<regex::Regex>> {
    if p.content.is_empty() {
        return Ok(None);
    }
    let mut pat = if p.regex { p.content.clone() } else { regex::escape(&p.content) };
    if p.whole_words {
        pat = format!(r"\b(?:{pat})\b");
    }
    if !p.content_case {
        pat = format!("(?i){pat}");
    }
    Ok(Some(regex::Regex::new(&pat)?))
}

fn start_search(app: &mut App, p: Params) {
    let start = PathBuf::from(crate::util::expand_tilde(p.start.trim()));
    let start = if start.is_absolute() { start } else { app.active_local_dir().join(start) };
    if !start.is_dir() {
        app.error(format!("Not a directory: {}", start.display()));
        return;
    }
    let re = match build_regex(&p) {
        Ok(r) => r,
        Err(e) => {
            app.error(format!("Bad pattern:\n{e}"));
            return;
        }
    };
    let globs: Vec<globset::GlobMatcher> = p
        .names
        .split([';', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|g| crate::util::glob_matcher(g, p.name_case))
        .collect();
    let ignore: Vec<String> = p.ignore.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let shared = Arc::new(Shared::default());
    let sh = shared.clone();
    let start2 = start.clone();
    let params = p.clone();
    std::thread::spawn(move || {
        search(&start2, &params, &globs, &ignore, re.as_ref(), &sh);
        sh.done.store(true, Relaxed);
    });
    app.push_dialog(Box::new(FindResults {
        params: p,
        base: start,
        shared,
        cursor: 0,
        offset: 0,
        focus: 0,
        list_rect: Rect::default(),
        button_rects: Vec::new(),
        rows_cache: Vec::new(),
    }));
}

fn search(
    start: &Path,
    p: &Params,
    globs: &[globset::GlobMatcher],
    ignore: &[String],
    re: Option<&regex::Regex>,
    sh: &Shared,
) {
    let walker = walkdir::WalkDir::new(start)
        .follow_links(p.follow)
        .max_depth(if p.recursive { usize::MAX } else { 1 })
        .min_depth(1)
        .sort_by_file_name();
    let it = walker.into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        if p.skip_hidden && name.starts_with('.') {
            return false;
        }
        !(e.file_type().is_dir() && ignore.iter().any(|i| *i == name))
    });
    for entry in it {
        if sh.stop.load(Relaxed) {
            return;
        }
        let Ok(e) = entry else { continue };
        let name = e.file_name().to_string_lossy();
        let is_dir = e.file_type().is_dir();
        if is_dir {
            *sh.current.lock().unwrap() = e.path().display().to_string();
        }
        if !globs.is_empty() && !globs.iter().any(|g| g.is_match(name.as_ref())) {
            continue;
        }
        sh.scanned.fetch_add(1, Relaxed);
        let mut found = Vec::new();
        match re {
            None => found.push(Hit { path: e.path().to_path_buf(), line: None }),
            Some(re) => {
                if is_dir || !e.file_type().is_file() && !e.path().is_file() {
                    continue;
                }
                if e.metadata().map(|m| m.len() > MAX_FILE).unwrap_or(true) {
                    continue;
                }
                let mut data = Vec::new();
                if std::fs::File::open(e.path()).and_then(|f| f.take(MAX_FILE).read_to_end(&mut data)).is_err() {
                    continue;
                }
                if crate::util::looks_binary(&data) {
                    continue;
                }
                let text = String::from_utf8_lossy(&data);
                for (i, l) in text.lines().enumerate() {
                    if re.is_match(l) {
                        found.push(Hit {
                            path: e.path().to_path_buf(),
                            line: Some((i + 1, l.trim().replace('\t', " "))),
                        });
                        if found.len() >= MAX_HITS_PER_FILE {
                            break;
                        }
                    }
                }
            }
        }
        if !found.is_empty() {
            let mut hits = sh.hits.lock().unwrap();
            hits.extend(found);
            if hits.len() >= MAX_RESULTS {
                *sh.error.lock().unwrap() = Some(format!("Stopped after {MAX_RESULTS} results"));
                return;
            }
        }
    }
}

/// A display row: a directory header or a hit index.
#[derive(Clone, Copy)]
enum RowKind {
    Header,
    Hit(usize),
}

const BTN: [&str; 7] = ["Chdir", "Again", "Stop", "Panelize", "View", "Edit", "Close"];

pub struct FindResults {
    params: Params,
    base: PathBuf,
    shared: Arc<Shared>,
    cursor: usize,
    offset: usize,
    focus: usize,
    list_rect: Rect,
    button_rects: Vec<(Rect, usize)>,
    rows_cache: Vec<(RowKind, String)>,
}

impl FindResults {
    fn rebuild_rows(&mut self) {
        let hits = self.shared.hits.lock().unwrap();
        let mut rows = Vec::with_capacity(hits.len() + 16);
        let mut last_dir: Option<&Path> = None;
        for (i, h) in hits.iter().enumerate() {
            let dir = h.path.parent();
            if dir != last_dir {
                let d = dir.map(|d| tildify(&d.display().to_string())).unwrap_or_default();
                rows.push((RowKind::Header, d));
                last_dir = dir;
            }
            let name = h.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let label = match &h.line {
                Some((n, l)) => format!("  {name}:{n}: {l}"),
                None => format!("  {name}"),
            };
            rows.push((RowKind::Hit(i), label));
        }
        drop(hits);
        self.rows_cache = rows;
        if self.cursor >= self.rows_cache.len() {
            self.cursor = self.rows_cache.len().saturating_sub(1);
        }
        self.skip_header(1);
    }

    fn skip_header(&mut self, dir: isize) {
        let n = self.rows_cache.len();
        if n == 0 {
            return;
        }
        let mut c = self.cursor as isize;
        while c >= 0 && (c as usize) < n && matches!(self.rows_cache[c as usize].0, RowKind::Header) {
            c += dir;
        }
        if c < 0 || c as usize >= n {
            // Search the other way.
            c = self.cursor as isize;
            while c >= 0 && (c as usize) < n && matches!(self.rows_cache[c as usize].0, RowKind::Header) {
                c -= dir;
            }
        }
        self.cursor = c.clamp(0, n as isize - 1) as usize;
    }

    fn current_hit(&self) -> Option<Hit> {
        let (RowKind::Hit(i), _) = self.rows_cache.get(self.cursor)? else { return None };
        self.shared.hits.lock().unwrap().get(*i).cloned()
    }

    fn stop(&self) {
        self.shared.stop.store(true, Relaxed);
    }

    fn press(&mut self, b: usize) -> Outcome {
        match BTN[b] {
            "Chdir" => {
                let Some(h) = self.current_hit() else { return Outcome::Keep };
                self.stop();
                Outcome::CloseThen(Box::new(move |app: &mut App| {
                    let dir = h.path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
                    let name = h.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    app.panel_mut().view = crate::panel::PanelView::List;
                    app.chdir_active(Location::local(dir));
                    app.panel_mut().focus(&name);
                }))
            }
            "Again" => {
                self.stop();
                let p = self.params.clone();
                Outcome::CloseThen(Box::new(move |app: &mut App| open_with(app, p)))
            }
            "Stop" => {
                self.stop();
                Outcome::Keep
            }
            "Panelize" => {
                self.stop();
                let base = self.base.clone();
                let mut rels: Vec<String> = Vec::new();
                for h in self.shared.hits.lock().unwrap().iter() {
                    let rel = h
                        .path
                        .strip_prefix(&base)
                        .map(|r| r.display().to_string())
                        .unwrap_or_else(|_| h.path.display().to_string());
                    if rels.last() != Some(&rel) {
                        rels.push(rel);
                    }
                }
                rels.dedup();
                if rels.is_empty() {
                    return Outcome::Keep;
                }
                Outcome::CloseThen(Box::new(move |app: &mut App| app.panelize_list(base, rels)))
            }
            "View" | "Edit" => {
                let Some(h) = self.current_hit() else { return Outcome::Keep };
                let edit = BTN[b] == "Edit";
                Outcome::KeepThen(Box::new(move |app: &mut App| {
                    let dir = h.path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
                    let loc = Location::local(dir);
                    // The results dialog stays on the stack; hide it while the screen is open.
                    let me = app.dialogs.pop();
                    if edit {
                        app.open_editor(&loc, &h.path);
                    } else {
                        crate::viewer::open(app, &loc, &h.path);
                    }
                    if let Some(d) = me
                        && app.screens.is_empty()
                        && app.external.is_none()
                    {
                        app.dialogs.push(d);
                    }
                }))
            }
            _ => {
                self.stop();
                Outcome::Close
            }
        }
    }
}

impl Dialog for FindResults {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        self.rebuild_rows();
        let w = area.width.saturating_sub(6).max(40);
        let h = area.height.saturating_sub(4).max(10);
        let r = centered(area, w, h);
        let done = self.shared.done.load(Relaxed);
        let nhits = self.shared.hits.lock().unwrap().len();
        let title = format!(
            "Find File: {}",
            if self.params.content.is_empty() {
                self.params.names.clone()
            } else {
                format!("\"{}\"", self.params.content)
            }
        );
        let inner = draw_box(f, r, &title, ctx, false);
        let list = Rect { height: inner.height.saturating_sub(4), ..inner };
        self.list_rect = list;
        let rows = list.height as usize;
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if rows > 0 && self.cursor >= self.offset + rows {
            self.offset = self.cursor + 1 - rows;
        }
        let width = list.width as usize;
        let lines: Vec<Line> = self
            .rows_cache
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(rows)
            .map(|(i, (kind, label))| match kind {
                RowKind::Header => Line::styled(pad_right(&trunc_left(label, width), width), ctx.theme.dialog_title),
                RowKind::Hit(_) => {
                    let st = if i == self.cursor { ctx.theme.dialog_focus } else { ctx.theme.dialog };
                    Line::styled(pad_right(&trunc_mid(label, width), width), st)
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list);
        let status = if let Some(e) = self.shared.error.lock().unwrap().clone() {
            e
        } else if done {
            format!("Finished: {nhits} result(s), {} matched name(s)", self.shared.scanned.load(Relaxed))
        } else if self.shared.stop.load(Relaxed) {
            format!("Stopped: {nhits} result(s)")
        } else {
            let cur = self.shared.current.lock().unwrap().clone();
            format!("Searching... {nhits} found  {}", trunc_left(&tildify(&cur), width.saturating_sub(30)))
        };
        let sy = list.bottom() + 1;
        f.render_widget(
            Paragraph::new(Span::styled(pad_right(&status, width), ctx.theme.dialog_border)),
            Rect { y: sy, height: 1, ..inner },
        );
        let total: u16 = BTN.iter().map(|b| str_width(b) as u16 + 5).sum();
        let mut x = inner.x + inner.width.saturating_sub(total) / 2;
        let by = inner.bottom().saturating_sub(1);
        self.button_rects.clear();
        for (k, b) in BTN.iter().enumerate() {
            let text = if k == 0 { format!("[< {b} >]") } else { format!("[ {b} ]") };
            let tw = str_width(&text) as u16;
            let st = if k == self.focus { ctx.theme.dialog_focus } else { ctx.theme.dialog };
            let rr = Rect { x, y: by, width: tw.min(inner.right().saturating_sub(x)), height: 1 };
            f.render_widget(Paragraph::new(Span::styled(text, st)), rr);
            self.button_rects.push((rr, k));
            x += tw + 1;
        }
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        let n = self.rows_cache.len();
        let page = self.list_rect.height.max(1) as usize;
        match key.code {
            KeyCode::Esc => {
                self.stop();
                return Outcome::Close;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.stop();
                return Outcome::Keep;
            }
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                self.skip_header(-1);
            }
            KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(n.saturating_sub(1));
                self.skip_header(1);
            }
            KeyCode::PageUp => {
                self.cursor = self.cursor.saturating_sub(page);
                self.skip_header(1);
            }
            KeyCode::PageDown => {
                self.cursor = (self.cursor + page).min(n.saturating_sub(1));
                self.skip_header(-1);
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.skip_header(1);
            }
            KeyCode::End => {
                self.cursor = n.saturating_sub(1);
                self.skip_header(-1);
            }
            KeyCode::Left | KeyCode::BackTab => self.focus = (self.focus + BTN.len() - 1) % BTN.len(),
            KeyCode::Right | KeyCode::Tab => self.focus = (self.focus + 1) % BTN.len(),
            KeyCode::Enter => return self.press(self.focus),
            KeyCode::F(3) => return self.press(4),
            KeyCode::F(4) => return self.press(5),
            KeyCode::Char('a') => return self.press(1),
            KeyCode::Char('s') => return self.press(2),
            KeyCode::Char('p') => return self.press(3),
            KeyCode::Char('v') => return self.press(4),
            KeyCode::Char('e') => return self.press(5),
            _ => {}
        }
        Outcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> Outcome {
        let pos = (m.column, m.row).into();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, k)) = self.button_rects.iter().find(|(r, _)| r.contains(pos)).copied() {
                    self.focus = k;
                    return self.press(k);
                }
                if self.list_rect.contains(pos) {
                    let idx = self.offset + (m.row - self.list_rect.y) as usize;
                    if idx < self.rows_cache.len() && matches!(self.rows_cache[idx].0, RowKind::Hit(_)) {
                        if idx == self.cursor {
                            return self.press(0);
                        }
                        self.cursor = idx;
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                self.cursor = self.cursor.saturating_sub(3);
                self.skip_header(-1);
            }
            MouseEventKind::ScrollDown => {
                self.cursor = (self.cursor + 3).min(self.rows_cache.len().saturating_sub(1));
                self.skip_header(1);
            }
            _ => {}
        }
        Outcome::Keep
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
