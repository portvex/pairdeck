//! ncdu-like disk usage analyzer.

use crate::app::App;
use crate::dialog;
use crate::ops::JobSpec;
use crate::screen::{Screen, ScreenOutcome};
use crate::ui::Ctx;
use crate::util::{human_size, pad_left, pad_right, trunc_left, trunc_mid};
use crate::vfs::Location;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use std::any::Any;
use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub name: String,
    /// Space used on disk (blocks), including children.
    pub disk: u64,
    /// Apparent size (file lengths), including children.
    pub apparent: u64,
    /// Number of files and directories below (including itself).
    pub items: u64,
    pub is_dir: bool,
    /// Could not be read completely (permission denied...).
    pub error: bool,
    /// Children, largest first.
    pub children: Vec<Node>,
}

impl Node {
    fn sort(&mut self) {
        self.children.sort_by(|a, b| b.disk.cmp(&a.disk).then_with(|| a.name.cmp(&b.name)));
    }

    /// The node at `path` (child indices from this node).
    pub fn at(&self, path: &[usize]) -> Option<&Node> {
        path.iter().try_fold(self, |n, &i| n.children.get(i))
    }

    /// Remove child `child` of the node at `path`, subtracting its totals from
    /// every ancestor. Returns the removed node.
    pub fn remove(&mut self, path: &[usize], child: usize) -> Option<Node> {
        let removed = {
            let parent = path.iter().try_fold(&mut *self, |n, &i| n.children.get_mut(i))?;
            if child >= parent.children.len() {
                return None;
            }
            parent.children.remove(child)
        };
        let mut n = &mut *self;
        n.disk -= removed.disk;
        n.apparent -= removed.apparent;
        n.items -= removed.items;
        for &i in path {
            n = &mut n.children[i];
            n.disk -= removed.disk;
            n.apparent -= removed.apparent;
            n.items -= removed.items;
        }
        Some(removed)
    }
}

#[derive(Default)]
pub struct Progress {
    pub items: AtomicU64,
    pub bytes: AtomicU64,
    pub current: Mutex<String>,
    pub cancel: AtomicBool,
    pub done: AtomicBool,
    pub result: Mutex<Option<Node>>,
}

/// Scan `root` (not crossing into other file systems; hard links counted once).
pub fn scan(root: &Path, p: &Progress) -> Node {
    let dev = std::fs::symlink_metadata(root).map(|m| m.dev()).unwrap_or(0);
    let mut seen = HashSet::new();
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string());
    scan_dir(root, name, dev, &mut seen, p)
}

fn scan_dir(path: &Path, name: String, dev: u64, seen: &mut HashSet<(u64, u64)>, p: &Progress) -> Node {
    let mut node = Node { name, is_dir: true, items: 1, ..Node::default() };
    if let Ok(m) = std::fs::symlink_metadata(path) {
        node.disk = m.blocks() * 512;
    }
    if p.cancel.load(Relaxed) {
        return node;
    }
    *p.current.lock().unwrap() = path.display().to_string();
    let Ok(rd) = std::fs::read_dir(path) else {
        node.error = true;
        return node;
    };
    for de in rd {
        let Ok(de) = de else {
            node.error = true;
            continue;
        };
        let child_path = de.path();
        let child_name = de.file_name().to_string_lossy().into_owned();
        let Ok(m) = std::fs::symlink_metadata(&child_path) else {
            node.error = true;
            continue;
        };
        let child = if m.is_dir() {
            if m.dev() != dev {
                // Another file system (mount point): shown, not descended.
                Node { name: child_name, is_dir: true, items: 1, ..Node::default() }
            } else {
                scan_dir(&child_path, child_name, dev, seen, p)
            }
        } else {
            let first = m.nlink() <= 1 || seen.insert((m.dev(), m.ino()));
            let (disk, apparent) = if first { (m.blocks() * 512, m.len()) } else { (0, 0) };
            p.items.fetch_add(1, Relaxed);
            p.bytes.fetch_add(disk, Relaxed);
            Node { name: child_name, disk, apparent, items: 1, ..Node::default() }
        };
        node.disk += child.disk;
        node.apparent += child.apparent;
        node.items += child.items;
        node.error |= child.error;
        node.children.push(child);
    }
    node.sort();
    node
}

pub fn open(app: &mut App) {
    let Some(dir) = app.panel().loc.local_path().map(Path::to_path_buf) else {
        app.flash_error("Disk usage works on local directories only");
        return;
    };
    let screen = DuScreen::start(dir);
    app.push_screen(Box::new(screen));
}

pub struct DuScreen {
    root: PathBuf,
    progress: Arc<Progress>,
    tree: Option<Node>,
    /// Child indices from the root to the directory being shown.
    path: Vec<usize>,
    cursor: usize,
    top: usize,
    height: usize,
    apparent: bool,
    list_rect: Rect,
}

impl DuScreen {
    fn start(root: PathBuf) -> DuScreen {
        let progress = Arc::new(Progress::default());
        let p = progress.clone();
        let r = root.clone();
        std::thread::Builder::new()
            .name("du".into())
            .spawn(move || {
                let node = scan(&r, &p);
                *p.result.lock().unwrap() = Some(node);
                p.done.store(true, Relaxed);
            })
            .ok();
        DuScreen {
            root,
            progress,
            tree: None,
            path: Vec::new(),
            cursor: 0,
            top: 0,
            height: 10,
            apparent: false,
            list_rect: Rect::default(),
        }
    }

    fn current_dir_path(&self) -> PathBuf {
        let mut p = self.root.clone();
        if let Some(t) = &self.tree {
            let mut n = t;
            for &i in &self.path {
                n = &n.children[i];
                p.push(&n.name);
            }
        }
        p
    }

    fn dir(&self) -> Option<&Node> {
        self.tree.as_ref()?.at(&self.path)
    }

    fn size_of(&self, n: &Node) -> u64 {
        if self.apparent { n.apparent } else { n.disk }
    }

    fn enter(&mut self) -> Option<PathBuf> {
        let dir = self.dir()?;
        // Row 0 is ".." when not at the root.
        let up = usize::from(!self.path.is_empty());
        if self.cursor < up {
            self.leave();
            return None;
        }
        let idx = self.cursor - up;
        let child = dir.children.get(idx)?;
        if child.is_dir && !child.children.is_empty() {
            self.path.push(idx);
            self.cursor = 0;
            self.top = 0;
            None
        } else {
            Some(self.current_dir_path().join(&child.name))
        }
    }

    fn leave(&mut self) {
        if let Some(i) = self.path.pop() {
            self.cursor = i + usize::from(!self.path.is_empty());
        }
    }

    fn rows(&self) -> usize {
        self.dir().map(|d| d.children.len()).unwrap_or(0) + usize::from(!self.path.is_empty())
    }
}

impl Screen for DuScreen {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        if self.tree.is_none() && self.progress.done.load(Relaxed) {
            self.tree = self.progress.result.lock().unwrap().take();
        }
        let help = " Enter open  Left up  d/F8 trash  a apparent/disk  r rescan  o show in panel  Esc close ";
        let title = match &self.tree {
            Some(t) => format!(
                " Disk usage: {} in {} items{} ",
                human_size(self.size_of(t)),
                t.items,
                if t.error { " (some unreadable)" } else { "" }
            ),
            None => " Disk usage: scanning... ".to_string(),
        };
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
        let Some(dir) = self.dir().cloned() else {
            let cur = self.progress.current.lock().unwrap().clone();
            let lines = vec![
                Line::raw(""),
                Line::styled(
                    format!(
                        "  Scanning: {} items, {}",
                        self.progress.items.load(Relaxed),
                        human_size(self.progress.bytes.load(Relaxed))
                    ),
                    ctx.theme.header,
                ),
                Line::raw(format!("  {}", trunc_left(&cur, w.saturating_sub(4)))),
                Line::raw(""),
                Line::styled("  Esc stops", ctx.theme.hidden),
            ];
            f.render_widget(Paragraph::new(lines), inner);
            return;
        };
        let header =
            format!(" {}", trunc_left(&crate::util::tildify(&self.current_dir_path().display().to_string()), w - 1));
        f.render_widget(
            Paragraph::new(Span::styled(pad_right(&header, w), ctx.theme.header)),
            Rect { height: 1, ..inner },
        );
        let list = Rect { y: inner.y + 1, height: inner.height.saturating_sub(1), ..inner };
        self.list_rect = list;
        self.height = list.height.max(1) as usize;
        let rows = self.rows();
        self.cursor = self.cursor.min(rows.saturating_sub(1));
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + self.height {
            self.top = self.cursor + 1 - self.height;
        }
        let total = self.size_of(&dir).max(1);
        let bar_w = 20usize.min(w / 4);
        let up = usize::from(!self.path.is_empty());
        let mut lines = Vec::new();
        for row in self.top..(self.top + self.height).min(rows) {
            let st = if row == self.cursor { ctx.theme.cursor } else { ctx.theme.panel };
            if row < up {
                lines.push(Line::styled(pad_right("  ..", w), st));
                continue;
            }
            let n = &dir.children[row - up];
            let size = self.size_of(n);
            let frac = size as f64 / total as f64;
            let filled = (frac * bar_w as f64).round() as usize;
            let bar = format!("[{}{}]", "#".repeat(filled), " ".repeat(bar_w - filled.min(bar_w)));
            let name = if n.is_dir { format!("{}/", n.name) } else { n.name.clone() };
            let flag = if n.error { "!" } else { " " };
            let left = format!("{} {} {:>5.1}% {flag}", pad_left(&human_size(size), 7), bar, frac * 100.0);
            let name_w = w.saturating_sub(left.chars().count() + 1);
            let text = format!("{left} {}", trunc_mid(&name, name_w));
            let name_style = if row == self.cursor {
                st
            } else if n.is_dir {
                ctx.theme.dir
            } else {
                ctx.theme.panel
            };
            lines.push(Line::styled(pad_right(&text, w), name_style));
        }
        f.render_widget(Paragraph::new(lines), list);
    }

    fn key(&mut self, key: KeyEvent) -> ScreenOutcome {
        if self.tree.is_none() {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q')) {
                self.progress.cancel.store(true, Relaxed);
                return ScreenOutcome::Close;
            }
            return ScreenOutcome::Keep;
        }
        let rows = self.rows();
        match key.code {
            KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q') => return ScreenOutcome::Close,
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(rows.saturating_sub(1)),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(self.height),
            KeyCode::PageDown => self.cursor = (self.cursor + self.height).min(rows.saturating_sub(1)),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = rows.saturating_sub(1),
            KeyCode::Enter | KeyCode::Right => {
                self.enter();
            }
            KeyCode::Left | KeyCode::Backspace => self.leave(),
            KeyCode::Char('a') => self.apparent = !self.apparent,
            KeyCode::Char('r') => {
                let root = self.root.clone();
                *self = DuScreen::start(root);
            }
            KeyCode::Char('o') => {
                let up = usize::from(!self.path.is_empty());
                let dir = self.current_dir_path();
                let name =
                    self.dir().and_then(|d| d.children.get(self.cursor.wrapping_sub(up))).map(|n| n.name.clone());
                return ScreenOutcome::CloseThen(Box::new(move |app| {
                    app.chdir_active(Location::local(dir));
                    if let Some(n) = name {
                        app.panel_mut().focus(&n);
                    }
                }));
            }
            KeyCode::Char('d') | KeyCode::F(8) | KeyCode::Delete => {
                let up = usize::from(!self.path.is_empty());
                if self.cursor < up {
                    return ScreenOutcome::Keep;
                }
                let idx = self.cursor - up;
                let Some(n) = self.dir().and_then(|d| d.children.get(idx)).cloned() else {
                    return ScreenOutcome::Keep;
                };
                let target = self.current_dir_path().join(&n.name);
                let path = self.path.clone();
                let text = format!(
                    "Move \"{}\" ({}, {} items) to the trash?",
                    trunc_mid(&n.name, 40),
                    human_size(self.size_of(&n)),
                    n.items
                );
                return ScreenOutcome::Then(Box::new(move |app| {
                    app.push_dialog(dialog::confirm("Disk usage", &text, move |app| {
                        let name = target.strip_prefix("/").unwrap_or(&target).to_string_lossy().into_owned();
                        let trash = app.cfg.use_trash;
                        app.start_job(JobSpec::Delete { loc: Location::local("/"), names: vec![name], trash }, true);
                        if let Some(s) = app.top_screen::<DuScreen>()
                            && let Some(t) = s.tree.as_mut()
                        {
                            t.remove(&path, idx);
                        }
                    }));
                }));
            }
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> ScreenOutcome {
        let rows = self.rows();
        match m.kind {
            MouseEventKind::ScrollUp => self.cursor = self.cursor.saturating_sub(3),
            MouseEventKind::ScrollDown => self.cursor = (self.cursor + 3).min(rows.saturating_sub(1)),
            MouseEventKind::Down(MouseButton::Left) if self.list_rect.contains((m.column, m.row).into()) => {
                let row = self.top + (m.row - self.list_rect.y) as usize;
                if row < rows {
                    if row == self.cursor {
                        self.enter();
                    } else {
                        self.cursor = row;
                    }
                }
            }
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn title(&self) -> String {
        "Disk usage".into()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_sizes_and_removes() {
        let base = std::env::temp_dir().join(format!("pairdeck-du-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("big/inner")).unwrap();
        std::fs::create_dir_all(base.join("small")).unwrap();
        std::fs::write(base.join("big/inner/a"), vec![1u8; 200_000]).unwrap();
        std::fs::write(base.join("big/b"), vec![1u8; 100_000]).unwrap();
        std::fs::write(base.join("small/c"), vec![1u8; 10]).unwrap();
        std::fs::hard_link(base.join("big/b"), base.join("small/b-link")).unwrap();
        let p = Progress::default();
        let t = scan(&base, &p);
        assert_eq!(t.apparent, 300_010, "hard link counted once");
        assert_eq!(t.children[0].name, "big", "largest first");
        // The hard-linked file counts in whichever directory the walk reaches first,
        // and directory order differs between file systems (APFS vs ext4).
        assert!(matches!(t.children[0].apparent, 200_000 | 300_000), "big: {}", t.children[0].apparent);
        assert_eq!(t.children[0].apparent + t.children[1].apparent, 300_010);
        let inner = t.at(&[0, 0]).unwrap();
        assert_eq!(inner.name, "inner");
        assert!(t.disk >= t.apparent, "disk usage includes block overhead");
        assert_eq!(t.items, 8, "root, 3 dirs, 4 files");

        let mut t2 = t.clone();
        let removed = t2.remove(&[0], 0).unwrap(); // remove big/inner
        assert_eq!(removed.name, "inner");
        assert_eq!(t2.apparent, 100_010);
        let big_after = t2.children.iter().find(|c| c.name == "big").unwrap().apparent;
        assert_eq!(big_after, t.children[0].apparent - 200_000);
        assert_eq!(t2.items, t.items - removed.items);
        assert!(t2.remove(&[9], 0).is_none(), "bad path");
        let _ = std::fs::remove_dir_all(&base);
    }
}
