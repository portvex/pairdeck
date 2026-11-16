//! Ctrl-P fuzzy finder over the files below the active directory, and Alt-J
//! jump list of frecent directories. Both share one full-screen view with a
//! live preview of the selected entry.

use crate::app::App;
use crate::dialog;
use crate::lineedit::LineEdit;
use crate::preview::{PreviewMsg, Previewer};
use crate::screen::{Screen, ScreenOutcome};
use crate::ui::Ctx;
use crate::util::{pad_right, str_width, tildify};
use crate::vfs::{Entry, EntryKind, Location};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use ratatui::{
    Frame,
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use std::any::Any;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Stop walking after this many entries.
pub const MAX_ENTRIES: usize = 500_000;
/// Items scored per frame, so typing stays responsive on huge trees.
const SCORE_BUDGET: usize = 150_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Files,
    Dirs,
}

#[derive(Clone, Debug)]
pub struct Item {
    /// Text that is shown and matched (relative path, or `~/...` for dirs).
    pub text: String,
    pub abs: PathBuf,
    pub is_dir: bool,
    /// Frecency score (dirs mode) used as tie-breaker.
    pub frec: f64,
}

/// Walk options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalkOpts {
    pub hidden: bool,
    pub ignore_files: bool,
}

impl Default for WalkOpts {
    fn default() -> Self {
        WalkOpts { hidden: false, ignore_files: true }
    }
}

/// Collects entries in batches and pushes them into the shared list.
struct Batch {
    buf: Vec<Item>,
    out: Arc<Mutex<Vec<Item>>>,
}

impl Batch {
    fn flush(&mut self) {
        if !self.buf.is_empty() {
            self.out.lock().unwrap_or_else(|e| e.into_inner()).append(&mut self.buf);
        }
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Walk `root` on background threads (gitignore aware, `.git` skipped),
/// streaming entries into `out`. `scanning` is cleared when done.
pub fn spawn_walk(
    root: PathBuf,
    opts: WalkOpts,
    out: Arc<Mutex<Vec<Item>>>,
    scanning: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) {
    scanning.store(true, Relaxed);
    std::thread::Builder::new()
        .name("fuzzy-walk".into())
        .spawn(move || {
            walk(&root, opts, &out, &cancel);
            scanning.store(false, Relaxed);
        })
        .ok();
}

/// Synchronous walk (used by the background thread and by tests).
pub fn walk(root: &Path, opts: WalkOpts, out: &Arc<Mutex<Vec<Item>>>, cancel: &Arc<AtomicBool>) {
    let ig = opts.ignore_files;
    let threads = std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(4);
    let walker = ignore::WalkBuilder::new(root)
        .hidden(!opts.hidden)
        .ignore(ig)
        .git_ignore(ig)
        .git_global(ig)
        .git_exclude(ig)
        .parents(ig)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|e| e.file_name() != ".git")
        .threads(threads)
        .build_parallel();
    let count = Arc::new(AtomicUsize::new(0));
    walker.run(|| {
        let mut batch = Batch { buf: Vec::with_capacity(256), out: out.clone() };
        let count = count.clone();
        let cancel = cancel.clone();
        let root = root.to_path_buf();
        Box::new(move |res| {
            if cancel.load(Relaxed) || count.load(Relaxed) >= MAX_ENTRIES {
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = res else { return ignore::WalkState::Continue };
            if entry.depth() == 0 {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(&root) else { return ignore::WalkState::Continue };
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            batch.buf.push(Item {
                text: rel.to_string_lossy().into_owned(),
                abs: path.to_path_buf(),
                is_dir,
                frec: 0.0,
            });
            count.fetch_add(1, Relaxed);
            if batch.buf.len() >= 256 {
                batch.flush();
            }
            ignore::WalkState::Continue
        })
    });
}

/// Incremental fuzzy matcher over a growing item list.
pub struct MatchState {
    matcher: Matcher,
    pattern: Pattern,
    text: String,
    dirs_only: bool,
    /// (score, item index) of matches, sorted when `dirty` is false.
    pub matched: Vec<(u32, u32)>,
    /// Items checked against the current pattern so far.
    scored_upto: usize,
    /// Pending narrowing pass: previous matches to re-check, and progress.
    rescore: Option<(Vec<u32>, usize)>,
    dirty: bool,
}

impl Default for MatchState {
    fn default() -> Self {
        Self::new()
    }
}

impl MatchState {
    pub fn new() -> Self {
        MatchState {
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            pattern: Pattern::parse("", CaseMatching::Smart, Normalization::Smart),
            text: String::new(),
            dirs_only: false,
            matched: Vec::new(),
            scored_upto: 0,
            rescore: None,
            dirty: false,
        }
    }

    /// Change the query / filter. Narrowing an existing query rescans only the
    /// current matches.
    pub fn set_query(&mut self, text: &str, dirs_only: bool) {
        if text == self.text && dirs_only == self.dirs_only {
            return;
        }
        let narrowing = dirs_only == self.dirs_only
            && !self.text.is_empty()
            && text.starts_with(&self.text)
            && !text.contains(' ')
            && !text.contains('|');
        self.pattern = Pattern::parse(text, CaseMatching::Smart, Normalization::Smart);
        self.text = text.to_string();
        self.dirs_only = dirs_only;
        if narrowing {
            // Re-check only the previous matches (and any unfinished narrowing pass);
            // items past scored_upto are still scored normally afterwards.
            let mut prev: Vec<u32> = self.matched.drain(..).map(|(_, i)| i).collect();
            if let Some((list, pos)) = self.rescore.take() {
                prev.extend_from_slice(&list[pos..]);
            }
            self.rescore = Some((prev, 0));
        } else {
            self.matched.clear();
            self.scored_upto = 0;
            self.rescore = None;
        }
        self.dirty = true;
    }

    /// Score up to `budget` more items. Returns true when everything is up to date.
    pub fn update(&mut self, items: &[Item], budget: usize) -> bool {
        let mut buf = Vec::new();
        let mut left = budget;
        // Narrowing pass over the previous matches.
        if let Some((list, pos)) = self.rescore.as_mut() {
            while *pos < list.len() && left > 0 {
                let idx = list[*pos];
                *pos += 1;
                left -= 1;
                let it = &items[idx as usize];
                if let Some(s) = score_item(&self.pattern, &mut self.matcher, it, self.dirs_only, &mut buf) {
                    self.matched.push((s, idx));
                }
            }
            if *pos >= list.len() {
                self.rescore = None;
            }
            self.dirty = true;
        }
        let end = items.len().min(self.scored_upto + left);
        for (idx, it) in items.iter().enumerate().take(end).skip(self.scored_upto) {
            if let Some(s) = score_item(&self.pattern, &mut self.matcher, it, self.dirs_only, &mut buf) {
                self.matched.push((s, idx as u32));
            }
        }
        if end > self.scored_upto {
            self.dirty = true;
        }
        self.scored_upto = end;
        let done = self.rescore.is_none() && self.scored_upto >= items.len();
        if self.dirty {
            let empty = self.text.is_empty();
            // Best score first; ties: frecency, then shorter path, then discovery order.
            self.matched.sort_by(|a, b| {
                let (ia, ib) = (&items[a.1 as usize], &items[b.1 as usize]);
                b.0.cmp(&a.0)
                    .then_with(|| ib.frec.total_cmp(&ia.frec))
                    .then_with(|| if empty { std::cmp::Ordering::Equal } else { ia.text.len().cmp(&ib.text.len()) })
                    .then_with(|| a.1.cmp(&b.1))
            });
            self.dirty = false;
        }
        done
    }

    /// Character indices of `text` that match the current pattern (for highlighting).
    pub fn indices(&mut self, text: &str) -> Vec<u32> {
        let mut buf = Vec::new();
        let mut idx = Vec::new();
        self.pattern.indices(Utf32Str::new(text, &mut buf), &mut self.matcher, &mut idx);
        idx.sort_unstable();
        idx.dedup();
        idx
    }
}

fn score_item(p: &Pattern, m: &mut Matcher, it: &Item, dirs_only: bool, buf: &mut Vec<char>) -> Option<u32> {
    if dirs_only && !it.is_dir {
        return None;
    }
    p.score(Utf32Str::new(&it.text, buf), m)
}

pub struct FuzzyScreen {
    mode: Mode,
    root: PathBuf,
    items: Arc<Mutex<Vec<Item>>>,
    scanning: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    opts: WalkOpts,
    dirs_only: bool,
    query: LineEdit,
    ms: MatchState,
    total: usize,
    cursor: usize,
    offset: usize,
    marked: HashSet<u32>,
    previewer: Previewer,
    preview_entry: Option<(PathBuf, Entry)>,
    show_preview: bool,
    list_rect: Rect,
    preview_rect: Rect,
    last_click: Option<(Instant, u16, u16)>,
}

impl Drop for FuzzyScreen {
    fn drop(&mut self) {
        self.cancel.store(true, Relaxed);
    }
}

/// Ctrl-P: fuzzy find files below the active panel's directory.
pub fn open_files(app: &mut App) {
    let Some(root) = app.panel().loc.local_path().map(Path::to_path_buf) else {
        app.flash_error("Fuzzy finder works on local directories only");
        return;
    };
    let mut s = FuzzyScreen::new(app, Mode::Files, root);
    s.start_walk();
    app.push_screen(Box::new(s));
}

/// Alt-J / `zi`: fuzzy jump to a frecent directory.
pub fn open_dirs(app: &mut App) {
    let now = crate::frecency::now();
    app.state.frecent.retain(|e| Path::new(&e.path).is_dir());
    let items: Vec<Item> = crate::frecency::ranked(&app.state.frecent, now)
        .into_iter()
        .map(|(p, f)| Item { text: tildify(&p), abs: PathBuf::from(p), is_dir: true, frec: f })
        .collect();
    if items.is_empty() {
        app.info("No directory history yet: visit some directories first");
        return;
    }
    let mut s = FuzzyScreen::new(app, Mode::Dirs, PathBuf::from("/"));
    s.total = items.len();
    *s.items.lock().unwrap_or_else(|e| e.into_inner()) = items;
    app.push_screen(Box::new(s));
}

impl FuzzyScreen {
    fn new(app: &App, mode: Mode, root: PathBuf) -> Self {
        FuzzyScreen {
            mode,
            root,
            items: Arc::new(Mutex::new(Vec::new())),
            scanning: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            opts: WalkOpts { hidden: app.cfg.show_hidden, ignore_files: true },
            dirs_only: false,
            query: LineEdit::new(""),
            ms: MatchState::new(),
            total: 0,
            cursor: 0,
            offset: 0,
            marked: HashSet::new(),
            previewer: Previewer::new(app.tx.clone(), app.picker.clone()),
            preview_entry: None,
            show_preview: true,
            list_rect: Rect::default(),
            preview_rect: Rect::default(),
            last_click: None,
        }
    }

    fn start_walk(&mut self) {
        self.cancel.store(true, Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.items = Arc::new(Mutex::new(Vec::new()));
        self.ms = MatchState::new();
        self.ms.set_query(&self.query.text(), self.dirs_only);
        self.marked.clear();
        self.cursor = 0;
        self.offset = 0;
        spawn_walk(self.root.clone(), self.opts, self.items.clone(), self.scanning.clone(), self.cancel.clone());
    }

    /// Preview results produced by this screen's own previewer.
    pub fn on_preview(&mut self, m: PreviewMsg) {
        if m.target() == self.previewer.id() {
            self.previewer.on_msg(m);
        }
    }

    pub fn previewer_id(&self) -> u64 {
        self.previewer.id()
    }

    fn refresh(&mut self) {
        self.ms.set_query(&self.query.text(), self.dirs_only);
        let items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        self.total = items.len();
        self.ms.update(&items, SCORE_BUDGET);
        drop(items);
        let n = self.ms.matched.len();
        if self.cursor >= n {
            self.cursor = n.saturating_sub(1);
        }
    }

    fn selected(&self) -> Option<(u32, Item)> {
        let &(_, idx) = self.ms.matched.get(self.cursor)?;
        let items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        items.get(idx as usize).cloned().map(|it| (idx, it))
    }

    fn move_by(&mut self, d: isize) {
        let n = self.ms.matched.len() as isize;
        if n == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + d).clamp(0, n - 1) as usize;
    }

    fn activate(&mut self) -> ScreenOutcome {
        let Some((_, it)) = self.selected() else { return ScreenOutcome::Keep };
        ScreenOutcome::CloseThen(Box::new(move |app: &mut App| goto(app, &it, false)))
    }

    fn marked_or_selected(&self) -> Vec<Item> {
        let items = self.items.lock().unwrap_or_else(|e| e.into_inner());
        if self.marked.is_empty() {
            return self
                .ms
                .matched
                .get(self.cursor)
                .and_then(|&(_, i)| items.get(i as usize).cloned())
                .into_iter()
                .collect();
        }
        // Keep the result order.
        self.ms
            .matched
            .iter()
            .filter(|(_, i)| self.marked.contains(i))
            .filter_map(|&(_, i)| items.get(i as usize).cloned())
            .collect()
    }

    fn panelize(&mut self) -> ScreenOutcome {
        let chosen = self.marked_or_selected();
        if chosen.is_empty() {
            return ScreenOutcome::Keep;
        }
        let (base, names): (PathBuf, Vec<String>) = match self.mode {
            Mode::Files => (self.root.clone(), chosen.iter().map(|i| i.text.clone()).collect()),
            Mode::Dirs => (
                PathBuf::from("/"),
                chosen.iter().map(|i| i.abs.to_string_lossy().trim_start_matches('/').to_string()).collect(),
            ),
        };
        ScreenOutcome::CloseThen(Box::new(move |app: &mut App| app.panelize_list(base, names)))
    }

    fn update_preview_entry(&mut self) {
        let Some((_, it)) = self.selected() else {
            self.preview_entry = None;
            return;
        };
        if self.preview_entry.as_ref().is_some_and(|(p, _)| *p == it.abs) {
            return;
        }
        self.preview_entry = crate::vfs::local::stat_local(&it.abs).ok().map(|e| (it.abs.clone(), e));
    }
}

/// Go to an item in the active panel (or the other panel).
fn goto(app: &mut App, it: &Item, other: bool) {
    let (dir, focus) = if it.is_dir {
        (it.abs.clone(), None)
    } else {
        (
            it.abs.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/")),
            it.abs.file_name().map(|n| n.to_string_lossy().into_owned()),
        )
    };
    if other {
        let o = app.other_mut();
        o.view = crate::panel::PanelView::List;
        if let Err(e) = o.chdir(Location::local(&dir), focus.as_deref()) {
            app.error(e);
        }
        app.setup_watcher();
        return;
    }
    app.panel_mut().view = crate::panel::PanelView::List;
    app.chdir_active(Location::local(&dir));
    if let Some(f) = focus {
        app.panel_mut().focus(&f);
    }
}

const HELP: &str = "\
Type to filter (fzf syntax: words AND, !not, ^prefix, suffix$, 'exact)
Up/Down PgUp/PgDn   move           Enter       go to file / enter dir
F3 / F4             view / edit    Alt-Enter   path to command line
Ctrl-O              open in other panel
Tab                 mark result    Ctrl-Space / F2  panelize marked
Alt-H  hidden files   Alt-I  .gitignore on/off   Alt-D  directories only
Alt-V  toggle preview  Esc  close";

impl Screen for FuzzyScreen {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        if area.height < 5 || area.width < 20 {
            return;
        }
        self.refresh();
        let theme = ctx.theme;
        f.render_widget(Block::default().style(theme.panel), area);

        // Prompt line.
        let prompt = match self.mode {
            Mode::Files => " Find: ",
            Mode::Dirs => " Jump: ",
        };
        let pw = str_width(prompt) as u16;
        f.render_widget(Paragraph::new(Span::styled(prompt, theme.prompt)), Rect { width: pw, height: 1, ..area });
        let input = Rect { x: area.x + pw, y: area.y, width: area.width.saturating_sub(pw + 4), height: 1 };
        self.query.render(f, input, theme.input_focus, true);

        // Status line: counts, scanning state, toggles, root.
        let scanning = self.scanning.load(Relaxed);
        let mut status = format!(" {}/{}", self.ms.matched.len(), self.total);
        if scanning {
            status.push_str("  scanning...");
        }
        if !self.marked.is_empty() {
            status.push_str(&format!("  {} marked", self.marked.len()));
        }
        let flag = |on: bool, s: &str| if on { format!("[{s}]") } else { format!(" {s} ") };
        if self.mode == Mode::Files {
            status.push_str(&format!(
                "  {}{}{}  {}",
                flag(self.opts.hidden, "hidden"),
                flag(self.opts.ignore_files, "gitignore"),
                flag(self.dirs_only, "dirs"),
                tildify(&self.root.display().to_string())
            ));
        }
        f.render_widget(
            Paragraph::new(Span::styled(pad_right(&status, area.width as usize), theme.status)),
            Rect { y: area.y + 1, height: 1, ..area },
        );

        // Body: list | preview.
        let body = Rect { y: area.y + 2, height: area.height.saturating_sub(3), ..area };
        let with_preview = self.show_preview && body.width >= 70;
        let list_w = if with_preview { body.width / 2 } else { body.width };
        let list = Rect { width: list_w, ..body };
        self.list_rect = list;
        let rows = list.height as usize;
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if rows > 0 && self.cursor >= self.offset + rows {
            self.offset = self.cursor + 1 - rows;
        }
        let visible: Vec<(u32, Item)> = {
            let items = self.items.lock().unwrap_or_else(|e| e.into_inner());
            self.ms
                .matched
                .iter()
                .skip(self.offset)
                .take(rows)
                .filter_map(|&(_, i)| items.get(i as usize).cloned().map(|it| (i, it)))
                .collect()
        };
        let width = list.width as usize;
        let mut lines = Vec::with_capacity(rows);
        for (row, (idx, it)) in visible.iter().enumerate() {
            let selected = self.offset + row == self.cursor;
            let base = if selected {
                theme.cursor
            } else if it.is_dir {
                theme.dir
            } else {
                theme.panel
            };
            let hl = base.patch(theme.search_match).add_modifier(Modifier::BOLD);
            let mut spans = vec![Span::styled(if self.marked.contains(idx) { "*" } else { " " }, base)];
            let mut used = 1;
            if ctx.cfg.nerd_fonts {
                let name = it.abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let e = Entry::new(name, if it.is_dir { EntryKind::Dir } else { EntryKind::File });
                let (g, c) = crate::icons::icon_for(&e);
                let mut st = base;
                if !selected && let Some(c) = c {
                    st = st.fg(c);
                }
                spans.push(Span::styled(format!("{g} "), st));
                used += 2;
            }
            let shown = if it.is_dir && self.mode == Mode::Files { format!("{}/", it.text) } else { it.text.clone() };
            let idxs = self.ms.indices(&it.text);
            let avail = width.saturating_sub(used);
            // Long paths: keep the end (file name) visible.
            let chars: Vec<char> = shown.chars().collect();
            let skip = chars.len().saturating_sub(avail);
            let mut cur = String::new();
            let mut cur_hl = false;
            for (ci, ch) in chars.iter().enumerate().skip(skip) {
                let is_hl = idxs.binary_search(&(ci as u32)).is_ok();
                if is_hl != cur_hl && !cur.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut cur), if cur_hl { hl } else { base }));
                }
                cur_hl = is_hl;
                cur.push(*ch);
            }
            if !cur.is_empty() {
                spans.push(Span::styled(cur, if cur_hl { hl } else { base }));
            }
            let w: usize = spans.iter().map(|s| str_width(&s.content)).sum();
            if w < width {
                spans.push(Span::styled(" ".repeat(width - w), base));
            }
            lines.push(Line::from(spans));
        }
        if visible.is_empty() {
            let msg = if scanning { "Scanning..." } else { "No matches" };
            lines.push(Line::styled(format!(" {msg}"), theme.hidden));
        }
        f.render_widget(Paragraph::new(lines).style(theme.panel), list);

        if with_preview {
            let pr = Rect { x: body.x + list_w, width: body.width - list_w, ..body };
            let block = Block::default().borders(Borders::LEFT).border_style(theme.border).style(theme.panel);
            let inner = block.inner(pr);
            f.render_widget(block, pr);
            self.preview_rect = inner;
            self.update_preview_entry();
            if let Some((abs, e)) = self.preview_entry.clone() {
                let parent = abs.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
                self.previewer.render(f, inner, &Location::local(parent), Some(&e), ctx);
            }
        } else {
            self.preview_rect = Rect::default();
        }

        let bar = Rect { y: area.bottom() - 1, height: 1, ..area };
        crate::viewer::fkey_bar(f, bar, ctx, &["Help", "Panel", "View", "Edit", "", "", "", "", "", "Quit"]);
    }

    fn key(&mut self, key: KeyEvent) -> ScreenOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let page = self.list_rect.height.max(1) as isize;
        match key.code {
            KeyCode::Esc | KeyCode::F(10) => return ScreenOutcome::Close,
            KeyCode::Char('c') | KeyCode::Char('g') if ctrl => return ScreenOutcome::Close,
            KeyCode::Enter if alt => {
                let Some((_, it)) = self.selected() else { return ScreenOutcome::Keep };
                return ScreenOutcome::CloseThen(Box::new(move |app: &mut App| {
                    app.cmdline.insert_str(&format!("{} ", crate::util::shell_quote(&it.abs.display().to_string())));
                }));
            }
            KeyCode::Enter => return self.activate(),
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::Char('p') | KeyCode::Char('k') if ctrl => self.move_by(-1),
            KeyCode::Char('n') | KeyCode::Char('j') if ctrl => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Tab => {
                if let Some(&(_, idx)) = self.ms.matched.get(self.cursor)
                    && !self.marked.remove(&idx)
                {
                    self.marked.insert(idx);
                }
                self.move_by(1);
            }
            KeyCode::Char(' ') if ctrl => return self.panelize(),
            KeyCode::F(2) => return self.panelize(),
            KeyCode::F(1) => {
                return ScreenOutcome::Then(Box::new(|app: &mut App| {
                    app.push_dialog(dialog::message("Fuzzy finder", HELP));
                }));
            }
            KeyCode::F(3) | KeyCode::F(4) => {
                let Some((_, it)) = self.selected() else { return ScreenOutcome::Keep };
                if it.is_dir {
                    return ScreenOutcome::Keep;
                }
                let edit = key.code == KeyCode::F(4);
                return ScreenOutcome::Then(Box::new(move |app: &mut App| {
                    let parent = it.abs.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
                    let loc = Location::local(parent);
                    if edit { app.open_editor(&loc, &it.abs) } else { crate::viewer::open(app, &loc, &it.abs) }
                }));
            }
            KeyCode::Char('o') if ctrl => {
                let Some((_, it)) = self.selected() else { return ScreenOutcome::Keep };
                return ScreenOutcome::CloseThen(Box::new(move |app: &mut App| goto(app, &it, true)));
            }
            KeyCode::Char('h') if alt && self.mode == Mode::Files => {
                self.opts.hidden = !self.opts.hidden;
                self.start_walk();
            }
            KeyCode::Char('i') if alt && self.mode == Mode::Files => {
                self.opts.ignore_files = !self.opts.ignore_files;
                self.start_walk();
            }
            KeyCode::Char('d') if alt && self.mode == Mode::Files => {
                self.dirs_only = !self.dirs_only;
                self.cursor = 0;
            }
            KeyCode::Char('v') if alt => self.show_preview = !self.show_preview,
            _ => {
                let before = self.query.text();
                self.query.handle_key(&key);
                if self.query.text() != before {
                    self.cursor = 0;
                    self.offset = 0;
                }
            }
        }
        ScreenOutcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> ScreenOutcome {
        let pos = (m.column, m.row).into();
        if self.preview_rect.width > 0 && self.preview_rect.contains(pos) && self.previewer.mouse(m) {
            return ScreenOutcome::Keep;
        }
        if matches!(m.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)) && self.previewer.mouse(m) {
            return ScreenOutcome::Keep;
        }
        let r = self.list_rect;
        match m.kind {
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::ScrollDown => self.move_by(3),
            MouseEventKind::Down(MouseButton::Left) if r.contains(pos) => {
                let idx = self.offset + (m.row - r.y) as usize;
                if idx < self.ms.matched.len() {
                    let double = self.last_click.is_some_and(|(t, x, y)| {
                        t.elapsed() < Duration::from_millis(400) && x == m.column && y == m.row
                    });
                    self.cursor = idx;
                    if double {
                        self.last_click = None;
                        return self.activate();
                    }
                    self.last_click = Some((Instant::now(), m.column, m.row));
                }
            }
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn paste(&mut self, text: &str) {
        self.query.insert_str(text);
        self.cursor = 0;
    }

    fn tick(&mut self) -> ScreenOutcome {
        // Keep scoring between frames while the walk streams in.
        self.refresh();
        ScreenOutcome::Keep
    }

    fn title(&self) -> String {
        match self.mode {
            Mode::Files => "Find file".into(),
            Mode::Dirs => "Jump to directory".into(),
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(paths: &[&str]) -> Vec<Item> {
        paths.iter().map(|p| Item { text: p.to_string(), abs: PathBuf::from(p), is_dir: false, frec: 0.0 }).collect()
    }

    fn run(ms: &mut MatchState, it: &[Item]) -> Vec<String> {
        while !ms.update(it, 3) {}
        ms.matched.iter().map(|&(_, i)| it[i as usize].text.clone()).collect()
    }

    #[test]
    fn ranks_filename_matches_first() {
        let it = items(&["src/app.rs", "docs/apple/notes.md", "src/ui/panel.rs", "target/debug/app"]);
        let mut ms = MatchState::new();
        ms.set_query("app", false);
        let r = run(&mut ms, &it);
        assert!(r.len() >= 3, "{r:?}");
        assert!(!r.contains(&"src/ui/panel.rs".to_string()));
        assert!(r[0].ends_with("app.rs") || r[0].ends_with("app"), "{r:?}");
    }

    #[test]
    fn narrowing_matches_full_rescan() {
        let paths: Vec<String> = (0..2000).map(|i| format!("dir{}/file_{i}_{}.rs", i % 37, i * 7 % 101)).collect();
        let it = items(&paths.iter().map(String::as_str).collect::<Vec<_>>());
        let mut inc = MatchState::new();
        inc.set_query("f", false);
        run(&mut inc, &it);
        inc.set_query("fi1", false);
        let a = run(&mut inc, &it);
        let mut full = MatchState::new();
        full.set_query("fi1", false);
        let b = run(&mut full, &it);
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn empty_query_keeps_order_and_dirs_filter() {
        let mut it = items(&["b", "a", "c"]);
        it[1].is_dir = true;
        let mut ms = MatchState::new();
        assert_eq!(run(&mut ms, &it), vec!["b", "a", "c"]);
        ms.set_query("", true);
        assert_eq!(run(&mut ms, &it), vec!["a"]);
    }

    #[test]
    fn highlight_indices_point_at_matches() {
        let mut ms = MatchState::new();
        ms.set_query("pnl", false);
        let idx = ms.indices("src/ui/panel.rs");
        let chars: Vec<char> = "src/ui/panel.rs".chars().collect();
        let picked: String = idx.iter().map(|&i| chars[i as usize]).collect();
        assert_eq!(picked.to_lowercase(), "pnl");
    }

    #[test]
    fn walk_respects_gitignore_and_skips_git() {
        let root = std::env::temp_dir().join(format!("pairdeck-fuzzy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["src", "target/debug", ".git/objects", ".hidden"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        for f in ["src/main.rs", "target/debug/app", ".git/objects/x", ".hidden/secret", "README.md"] {
            std::fs::write(root.join(f), "x").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        let collect = |opts: WalkOpts| {
            let out = Arc::new(Mutex::new(Vec::new()));
            walk(&root, opts, &out, &Arc::new(AtomicBool::new(false)));
            let mut v: Vec<String> = out.lock().unwrap().iter().map(|i| i.text.clone()).collect();
            v.sort();
            v
        };
        let def = collect(WalkOpts::default());
        assert!(def.contains(&"src/main.rs".to_string()), "{def:?}");
        assert!(def.contains(&"README.md".to_string()));
        assert!(!def.iter().any(|p| p.starts_with("target")), "gitignored: {def:?}");
        assert!(!def.iter().any(|p| p.starts_with(".git")), ".git skipped: {def:?}");
        assert!(!def.iter().any(|p| p.starts_with(".hidden")), "hidden skipped: {def:?}");
        let all = collect(WalkOpts { hidden: true, ignore_files: false });
        assert!(all.iter().any(|p| p == "target/debug/app"), "{all:?}");
        assert!(all.iter().any(|p| p == ".hidden/secret"), "{all:?}");
        assert!(!all.iter().any(|p| p.starts_with(".git/") || p == ".git"), ".git always skipped: {all:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod perf {
    use super::*;

    /// cargo test --release fuzzy_perf -- --ignored --nocapture
    #[test]
    #[ignore]
    fn fuzzy_perf() {
        let it: Vec<Item> = (0..200_000)
            .map(|i| {
                let t = format!("project{}/src/module_{}/component_{i}.rs", i % 97, i % 1013);
                Item { abs: PathBuf::from(&t), text: t, is_dir: false, frec: 0.0 }
            })
            .collect();
        let mut ms = MatchState::new();
        let t = Instant::now();
        ms.set_query("cmp12", false);
        while !ms.update(&it, usize::MAX) {}
        eprintln!("full score of 200k: {:?}, {} matches", t.elapsed(), ms.matched.len());
        let t = Instant::now();
        ms.set_query("cmp123", false);
        while !ms.update(&it, usize::MAX) {}
        eprintln!("narrowing: {:?}, {} matches", t.elapsed(), ms.matched.len());
    }
}
