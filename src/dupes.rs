//! Duplicate file finder: size groups, then a partial hash, then a full BLAKE3 hash.

use crate::app::App;
use crate::dialog::{self, Form};
use crate::ops::JobSpec;
use crate::screen::{Screen, ScreenOutcome};
use crate::ui::Ctx;
use crate::util::{human_size, pad_right, trunc_left};
use crate::vfs::Location;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

const PARTIAL: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct DupFile {
    pub path: PathBuf,
    pub mtime: SystemTime,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub size: u64,
    pub files: Vec<DupFile>,
}

impl Group {
    /// Space that deleting all but one copy would free.
    pub fn wasted(&self) -> u64 {
        self.size * (self.files.len().saturating_sub(1)) as u64
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub min_size: u64,
    pub hidden: bool,
    pub skip_dirs: Vec<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options { min_size: 1, hidden: false, skip_dirs: vec![".git".into(), "node_modules".into(), "target".into()] }
    }
}

/// Shared scan state, read by the screen while the worker runs.
#[derive(Default)]
pub struct Progress {
    pub phase: Mutex<String>,
    pub files: AtomicU64,
    pub hashed: AtomicU64,
    pub done: AtomicBool,
    pub cancel: AtomicBool,
    pub groups: Mutex<Vec<Group>>,
}

fn hash_file(path: &Path, limit: Option<usize>) -> Option<blake3::Hash> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = limit.unwrap_or(usize::MAX);
    while left > 0 {
        let want = buf.len().min(left);
        let n = f.read(&mut buf[..want]).ok()?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        left -= n;
    }
    Some(h.finalize())
}

/// Find groups of identical files below `root`, largest waste first.
pub fn find(root: &Path, opts: &Options, p: &Progress) -> Vec<Group> {
    *p.phase.lock().unwrap() = "Scanning".into();
    let mut by_size: HashMap<u64, Vec<DupFile>> = HashMap::new();
    let mut seen_inodes = HashSet::new();
    let walker = walkdir::WalkDir::new(root).follow_links(false).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        if e.depth() > 0 && !opts.hidden && name.starts_with('.') {
            return false;
        }
        !(e.file_type().is_dir() && e.depth() > 0 && opts.skip_dirs.iter().any(|d| d == name.as_ref()))
    });
    for e in walker.flatten() {
        if p.cancel.load(Relaxed) {
            return Vec::new();
        }
        if !e.file_type().is_file() {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        if m.len() < opts.min_size.max(1) {
            continue;
        }
        // Hard links to the same inode are one file, not duplicates.
        if !seen_inodes.insert((m.dev(), m.ino())) {
            continue;
        }
        p.files.fetch_add(1, Relaxed);
        by_size
            .entry(m.len())
            .or_default()
            .push(DupFile { path: e.into_path(), mtime: m.modified().unwrap_or(SystemTime::UNIX_EPOCH) });
    }
    *p.phase.lock().unwrap() = "Comparing".into();
    let mut groups = Vec::new();
    for (size, files) in by_size.into_iter().filter(|(_, v)| v.len() > 1) {
        let mut by_partial: HashMap<blake3::Hash, Vec<DupFile>> = HashMap::new();
        for f in files {
            if p.cancel.load(Relaxed) {
                return Vec::new();
            }
            if let Some(h) = hash_file(&f.path, Some(PARTIAL)) {
                p.hashed.fetch_add(1, Relaxed);
                by_partial.entry(h).or_default().push(f);
            }
        }
        for cands in by_partial.into_values().filter(|v| v.len() > 1) {
            if size as usize <= PARTIAL {
                groups.push(Group { size, files: cands });
                continue;
            }
            let mut by_full: HashMap<blake3::Hash, Vec<DupFile>> = HashMap::new();
            for f in cands {
                if p.cancel.load(Relaxed) {
                    return Vec::new();
                }
                if let Some(h) = hash_file(&f.path, None) {
                    by_full.entry(h).or_default().push(f);
                }
            }
            groups.extend(by_full.into_values().filter(|v| v.len() > 1).map(|files| Group { size, files }));
        }
    }
    for g in &mut groups {
        g.files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    groups.sort_by(|a, b| b.wasted().cmp(&a.wasted()).then_with(|| a.files[0].path.cmp(&b.files[0].path)));
    groups
}

/// Paths to mark: every file of each group except the newest (or oldest) one.
pub fn all_but(groups: &[Group], keep_newest: bool) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    for g in groups {
        let keep =
            if keep_newest { g.files.iter().max_by_key(|f| f.mtime) } else { g.files.iter().min_by_key(|f| f.mtime) };
        let keep = keep.map(|f| f.path.clone());
        out.extend(g.files.iter().filter(|f| Some(&f.path) != keep.as_ref()).map(|f| f.path.clone()));
    }
    out
}

// ------------------------------------------------------------------ UI

pub fn open(app: &mut App) {
    let Some(dir) = app.panel().loc.local_path().map(Path::to_path_buf) else {
        app.flash_error("Duplicate search works on local directories only");
        return;
    };
    let d = Form::new("Find duplicate files")
        .width(64)
        .input("dir", "Search in:", &dir.display().to_string())
        .input("min", "Minimum file size in bytes (K, M, G suffixes):", "1")
        .check("hidden", "Include hidden files", false)
        .input("skip", "Skip directories (separated by ;):", ".git;node_modules;target")
        .buttons(&["Search", "Cancel"], 0)
        .on_submit(|app, r| {
            if r.button != 0 {
                return;
            }
            let root = PathBuf::from(crate::util::expand_tilde(r.text("dir").trim()));
            if !root.is_dir() {
                app.error(format!("Not a directory: {}", root.display()));
                return;
            }
            let opts = Options {
                min_size: parse_size(&r.text("min")).unwrap_or(1),
                hidden: r.check("hidden"),
                skip_dirs: r.text("skip").split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            };
            start(app, root, opts);
        });
    app.push_dialog(Box::new(d));
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_uppercase();
    let (num, mul) = match s.chars().last()? {
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        'M' => (&s[..s.len() - 1], 1 << 20),
        'G' => (&s[..s.len() - 1], 1 << 30),
        _ => (s.as_str(), 1),
    };
    num.trim().parse::<u64>().ok().map(|n| n * mul)
}

fn start(app: &mut App, root: PathBuf, opts: Options) {
    let progress = Arc::new(Progress::default());
    let p = progress.clone();
    let r = root.clone();
    std::thread::Builder::new()
        .name("dupes".into())
        .spawn(move || {
            let groups = find(&r, &opts, &p);
            *p.groups.lock().unwrap() = groups;
            p.done.store(true, Relaxed);
        })
        .ok();
    app.push_screen(Box::new(DupesScreen {
        root,
        progress,
        groups: Vec::new(),
        loaded: false,
        rows: Vec::new(),
        cursor: 0,
        top: 0,
        height: 10,
        marked: HashSet::new(),
    }));
}

/// A visible row: a group header or a file of a group.
#[derive(Clone, Copy)]
enum Row {
    Header(usize),
    File(usize, usize),
}

pub struct DupesScreen {
    root: PathBuf,
    progress: Arc<Progress>,
    groups: Vec<Group>,
    loaded: bool,
    rows: Vec<Row>,
    cursor: usize,
    top: usize,
    height: usize,
    marked: HashSet<PathBuf>,
}

impl DupesScreen {
    fn rebuild(&mut self) {
        self.groups.retain(|g| g.files.len() > 1);
        self.rows.clear();
        for (gi, g) in self.groups.iter().enumerate() {
            self.rows.push(Row::Header(gi));
            for fi in 0..g.files.len() {
                self.rows.push(Row::File(gi, fi));
            }
        }
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
        self.skip_header(1);
    }

    fn skip_header(&mut self, dir: isize) {
        while let Some(Row::Header(_)) = self.rows.get(self.cursor) {
            let next = self.cursor as isize + dir;
            if next < 0 || next >= self.rows.len() as isize {
                if dir < 0 {
                    // At the very top: step down to the first file instead.
                    self.cursor += 1;
                }
                break;
            }
            self.cursor = next as usize;
        }
    }

    fn current_file(&self) -> Option<&DupFile> {
        match self.rows.get(self.cursor)? {
            Row::File(g, f) => self.groups.get(*g)?.files.get(*f),
            Row::Header(_) => None,
        }
    }

    fn move_by(&mut self, d: isize) {
        let n = self.rows.len() as isize;
        if n == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + d).clamp(0, n - 1) as usize;
        self.skip_header(if d < 0 { -1 } else { 1 });
    }

    fn marked_size(&self) -> u64 {
        self.groups
            .iter()
            .flat_map(|g| g.files.iter().map(move |f| (g.size, f)))
            .filter(|(_, f)| self.marked.contains(&f.path))
            .map(|(s, _)| s)
            .sum()
    }
}

impl Screen for DupesScreen {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        if !self.loaded && self.progress.done.load(Relaxed) {
            self.groups = std::mem::take(&mut *self.progress.groups.lock().unwrap());
            self.loaded = true;
            self.rebuild();
        }
        let wasted: u64 = self.groups.iter().map(Group::wasted).sum();
        let title = if self.loaded {
            format!(" Duplicates: {} group(s), {} wasted ", self.groups.len(), human_size(wasted))
        } else {
            " Duplicates: searching... ".to_string()
        };
        let help = " Space mark  n/o keep newest/oldest  c clear  F8 trash marked  p panelize  Enter go to  Esc close ";
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(if ctx.classic { BorderType::Double } else { BorderType::Rounded })
            .border_style(ctx.theme.border_active)
            .style(ctx.theme.panel)
            .title(Line::styled(title, ctx.theme.dialog_title).centered())
            .title_bottom(Line::styled(help, ctx.theme.border).centered());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let w = inner.width as usize;
        if !self.loaded {
            let phase = self.progress.phase.lock().unwrap().clone();
            let text = vec![
                Line::raw(""),
                Line::styled(
                    format!("  {phase} {}", trunc_left(&self.root.display().to_string(), w.saturating_sub(16))),
                    ctx.theme.header,
                ),
                Line::raw(format!(
                    "  {} file(s) seen, {} hashed",
                    self.progress.files.load(Relaxed),
                    self.progress.hashed.load(Relaxed)
                )),
                Line::raw(""),
                Line::styled("  Esc stops the search", ctx.theme.hidden),
            ];
            f.render_widget(Paragraph::new(text), inner);
            return;
        }
        if self.groups.is_empty() {
            f.render_widget(Paragraph::new(Line::styled("  No duplicate files found.", ctx.theme.header)), inner);
            return;
        }
        let list_h = inner.height.saturating_sub(1) as usize;
        self.height = list_h.max(1);
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + list_h {
            self.top = self.cursor + 1 - list_h;
        }
        let mut lines = Vec::new();
        for (i, row) in self.rows.iter().enumerate().skip(self.top).take(list_h) {
            match *row {
                Row::Header(g) => {
                    let gr = &self.groups[g];
                    let t = format!(
                        " {} copies of {} ({} wasted)",
                        gr.files.len(),
                        human_size(gr.size),
                        human_size(gr.wasted())
                    );
                    lines.push(Line::styled(pad_right(&t, w), ctx.theme.header));
                }
                Row::File(g, fi) => {
                    let file = &self.groups[g].files[fi];
                    let mark = if self.marked.contains(&file.path) { "*" } else { " " };
                    let rel = file.path.strip_prefix(&self.root).unwrap_or(&file.path).display().to_string();
                    let date = crate::util::short_time(Some(file.mtime));
                    let name_w = w.saturating_sub(date.len() + 6);
                    let text = format!("  {mark} {} {date}", pad_right(&trunc_left(&rel, name_w), name_w));
                    let st = if i == self.cursor {
                        ctx.theme.cursor
                    } else if self.marked.contains(&file.path) {
                        ctx.theme.marked
                    } else {
                        ctx.theme.panel
                    };
                    lines.push(Line::styled(pad_right(&text, w), st));
                }
            }
        }
        f.render_widget(Paragraph::new(lines), Rect { height: list_h as u16, ..inner });
        let status = format!(" {} marked ({})", self.marked.len(), human_size(self.marked_size()));
        f.render_widget(
            Paragraph::new(Span::styled(status, ctx.theme.status)),
            Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner },
        );
    }

    fn key(&mut self, key: KeyEvent) -> ScreenOutcome {
        if !self.loaded {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q')) {
                self.progress.cancel.store(true, Relaxed);
                return ScreenOutcome::Close;
            }
            return ScreenOutcome::Keep;
        }
        let page = self.height as isize;
        match key.code {
            KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q') => return ScreenOutcome::Close,
            KeyCode::Up => self.move_by(-1),
            KeyCode::Down => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home => {
                self.cursor = 0;
                self.skip_header(1);
            }
            KeyCode::End => {
                self.cursor = self.rows.len().saturating_sub(1);
                self.skip_header(-1);
            }
            KeyCode::Char(' ') | KeyCode::Insert => {
                if let Some(p) = self.current_file().map(|f| f.path.clone())
                    && !self.marked.remove(&p)
                {
                    self.marked.insert(p);
                }
                self.move_by(1);
            }
            KeyCode::Char('n') => self.marked = all_but(&self.groups, true),
            KeyCode::Char('o') => self.marked = all_but(&self.groups, false),
            KeyCode::Char('c') => self.marked.clear(),
            KeyCode::Char('p') | KeyCode::F(2) => {
                let root = self.root.clone();
                let mut files: Vec<String> = if self.marked.is_empty() {
                    self.groups
                        .iter()
                        .flat_map(|g| g.files.iter().map(|f| f.path.clone()))
                        .map(|p| rel(&root, &p))
                        .collect()
                } else {
                    self.marked.iter().map(|p| rel(&root, p)).collect()
                };
                files.sort();
                return ScreenOutcome::CloseThen(Box::new(move |app| app.panelize_list(root, files)));
            }
            KeyCode::F(8) | KeyCode::Delete => {
                if self.marked.is_empty() {
                    return ScreenOutcome::Keep;
                }
                let paths: Vec<PathBuf> = self.marked.iter().cloned().collect();
                // Never delete every copy of a group by accident.
                let whole = self.groups.iter().any(|g| g.files.iter().all(|f| self.marked.contains(&f.path)));
                let mut text =
                    format!("Move {} marked file(s) ({}) to the trash?", paths.len(), human_size(self.marked_size()));
                if whole {
                    text.push_str("\n\nWARNING: every copy of at least one group is marked.");
                }
                let marked: HashSet<PathBuf> = self.marked.clone();
                return ScreenOutcome::Then(Box::new(move |app| {
                    app.push_dialog(dialog::confirm("Delete duplicates", &text, move |app| {
                        let names: Vec<String> = paths
                            .iter()
                            .map(|p| p.strip_prefix("/").unwrap_or(p).to_string_lossy().into_owned())
                            .collect();
                        let loc = Location::local("/");
                        let trash = app.cfg.use_trash;
                        app.start_job(JobSpec::Delete { loc, names, trash }, true);
                        if let Some(s) = app.top_screen::<DupesScreen>() {
                            for g in &mut s.groups {
                                g.files.retain(|f| !marked.contains(&f.path));
                            }
                            s.marked.clear();
                            s.rebuild();
                        }
                    }));
                }));
            }
            KeyCode::Enter => {
                if let Some(path) = self.current_file().map(|f| f.path.clone()) {
                    return ScreenOutcome::CloseThen(Box::new(move |app| {
                        if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
                            app.chdir_active(Location::local(dir));
                            app.panel_mut().focus(&name.to_string_lossy());
                        }
                    }));
                }
            }
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> ScreenOutcome {
        match m.kind {
            MouseEventKind::ScrollUp => self.move_by(-3),
            MouseEventKind::ScrollDown => self.move_by(3),
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn title(&self) -> String {
        "Duplicates".into()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_identical_content_only() {
        let base = std::env::temp_dir().join(format!("pairdeck-dupes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        let big: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut big2 = big.clone();
        *big2.last_mut().unwrap() ^= 1; // same size, same first 16 KB, different end
        std::fs::write(base.join("a.bin"), &big).unwrap();
        std::fs::write(base.join("sub/b.bin"), &big).unwrap();
        std::fs::write(base.join("c.bin"), &big2).unwrap();
        std::fs::write(base.join("x.txt"), "hello").unwrap();
        std::fs::write(base.join("sub/y.txt"), "hello").unwrap();
        std::fs::write(base.join("z.txt"), "world").unwrap();
        std::fs::write(base.join(".git/x.txt"), "hello").unwrap(); // skipped dir
        std::fs::hard_link(base.join("z.txt"), base.join("z-link.txt")).unwrap(); // not a duplicate
        let p = Progress::default();
        let groups = find(&base, &Options::default(), &p);
        assert_eq!(groups.len(), 2, "{groups:?}");
        assert_eq!(groups[0].size, 100_000, "largest waste first");
        let names = |g: &Group| g.files.iter().map(|f| rel(&base, &f.path)).collect::<Vec<_>>();
        assert_eq!(names(&groups[0]), ["a.bin", "sub/b.bin"]);
        assert_eq!(names(&groups[1]), ["sub/y.txt", "x.txt"]);
        assert_eq!(groups[0].wasted(), 100_000);
        // min size filters the small group out
        let big_only = find(&base, &Options { min_size: 1000, ..Options::default() }, &p);
        assert_eq!(big_only.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn mark_all_but_newest_or_oldest() {
        let t = |s: u64| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s);
        let g = Group {
            size: 10,
            files: vec![
                DupFile { path: "/a".into(), mtime: t(1) },
                DupFile { path: "/b".into(), mtime: t(3) },
                DupFile { path: "/c".into(), mtime: t(2) },
            ],
        };
        let keep_new = all_but(std::slice::from_ref(&g), true);
        assert_eq!(keep_new, HashSet::from([PathBuf::from("/a"), PathBuf::from("/c")]));
        let keep_old = all_but(&[g], false);
        assert_eq!(keep_old, HashSet::from([PathBuf::from("/b"), PathBuf::from("/c")]));
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("10"), Some(10));
        assert_eq!(parse_size("4k"), Some(4096));
        assert_eq!(parse_size(" 2M "), Some(2 << 20));
        assert_eq!(parse_size("x"), None);
    }
}
