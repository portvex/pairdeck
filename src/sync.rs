//! Directory synchronization: recursively compare the two panel directories
//! (through their VFS, so local, archive and remote sides all work), show an
//! editable plan, then execute it as a background job (see `ops::SyncStep`).

use crate::app::App;
use crate::dialog::{self, Form};
use crate::ops::{JobSpec, SyncStep};
use crate::screen::{Screen, ScreenOutcome};
use crate::ui::Ctx;
use crate::util::{human_size, pad_right, short_time, str_width, trunc_left};
use crate::vfs::{EntryKind, Location};
use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

// ------------------------------------------------------------------ options

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    LeftToRight,
    RightToLeft,
    /// Newer file wins in each direction.
    TwoWay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareBy {
    /// Size and modification time (with a tolerance).
    SizeTime,
    Size,
    /// blake3 of the content (files of equal size only).
    Checksum,
}

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub direction: Direction,
    pub compare: CompareBy,
    /// Delete files on the target that do not exist on the source (one-way only).
    pub mirror: bool,
    /// File name globs to include (empty = everything). Directories are always walked.
    pub include: Vec<String>,
    /// Globs excluded everywhere (matched against the name and the relative path).
    pub exclude: Vec<String>,
    pub follow_links: bool,
    pub dry_run: bool,
    /// Modification times closer than this are equal (FAT and many remotes store 2 s steps).
    pub tolerance_secs: u64,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions {
            direction: Direction::LeftToRight,
            compare: CompareBy::SizeTime,
            mirror: false,
            include: Vec::new(),
            exclude: vec![".git".into(), ".DS_Store".into()],
            follow_links: false,
            dry_run: false,
            tolerance_secs: 2,
        }
    }
}

fn split_globs(s: &str) -> Vec<String> {
    s.split([';', ',']).map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

fn last_options() -> &'static Mutex<SyncOptions> {
    static LAST: OnceLock<Mutex<SyncOptions>> = OnceLock::new();
    LAST.get_or_init(|| Mutex::new(SyncOptions::default()))
}

// ------------------------------------------------------------------ scanning

#[derive(Clone, Debug)]
pub struct Node {
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<SystemTime>,
}

/// A scanned tree: relative path ("a/b.txt") to node, plus the directories that
/// contain excluded entries somewhere below them (never removed as a whole).
#[derive(Default, Debug)]
pub struct Tree {
    pub nodes: BTreeMap<String, Node>,
    pub has_excluded: HashSet<String>,
}

impl Tree {
    /// Entries strictly below directory `rel`, in sorted (parents first) order.
    fn below<'a>(&'a self, rel: &'a str) -> impl Iterator<Item = (&'a String, &'a Node)> + 'a {
        let prefix = format!("{rel}/");
        self.nodes.range(prefix.clone()..).take_while(move |(k, _)| k.starts_with(&prefix))
    }

    fn subtree_stats(&self, rel: &str) -> (u64, u64) {
        self.below(rel).filter(|(_, n)| !n.is_dir).fold((0, 0), |(f, b), (_, n)| (f + 1, b + n.size))
    }
}

struct Filter {
    include: Vec<globset::GlobMatcher>,
    exclude: Vec<globset::GlobMatcher>,
}

impl Filter {
    fn new(o: &SyncOptions) -> Filter {
        let m = |v: &[String]| v.iter().filter_map(|g| crate::util::glob_matcher(g, false)).collect();
        Filter { include: m(&o.include), exclude: m(&o.exclude) }
    }
    fn excluded(&self, name: &str, rel: &str) -> bool {
        self.exclude.iter().any(|m| m.is_match(name) || m.is_match(rel))
    }
    fn included_file(&self, name: &str, rel: &str) -> bool {
        self.include.is_empty() || self.include.iter().any(|m| m.is_match(name) || m.is_match(rel))
    }
}

fn mark_excluded(tree: &mut Tree, parent_rel: &str) {
    let mut p = parent_rel.to_string();
    loop {
        if p.is_empty() || !tree.has_excluded.insert(p.clone()) {
            break;
        }
        match p.rfind('/') {
            Some(i) => p.truncate(i),
            None => break,
        }
    }
}

/// Recursively scan `loc` through its VFS. `count` is incremented per entry.
pub fn scan_tree(loc: &Location, opts: &SyncOptions, count: &AtomicU64, cancel: &AtomicBool) -> Result<Tree> {
    let filter = Filter::new(opts);
    let mut tree = Tree::default();
    let mut stack = vec![String::new()];
    let mut seen_dirs = 0usize;
    while let Some(rel_dir) = stack.pop() {
        if cancel.load(Relaxed) {
            bail!("cancelled");
        }
        let dir = if rel_dir.is_empty() { loc.path.clone() } else { loc.join(&rel_dir) };
        let list = match loc.vfs.lock().list(&dir) {
            Ok(l) => l,
            // The root must be readable; unreadable subdirectories are skipped.
            Err(e) if rel_dir.is_empty() => return Err(e),
            Err(_) => continue,
        };
        seen_dirs += 1;
        if seen_dirs > 2_000_000 {
            bail!("too many directories");
        }
        for e in list {
            if e.is_parent() {
                continue;
            }
            count.fetch_add(1, Relaxed);
            let rel = if rel_dir.is_empty() { e.name.clone() } else { format!("{rel_dir}/{}", e.name) };
            if filter.excluded(&e.name, &rel) {
                mark_excluded(&mut tree, &rel_dir);
                continue;
            }
            let walk_dir = matches!(e.kind, EntryKind::Dir) || (opts.follow_links && e.is_dir());
            if walk_dir {
                tree.nodes.insert(rel.clone(), Node { is_dir: true, size: 0, mtime: e.mtime });
                stack.push(rel);
            } else if matches!(e.kind, EntryKind::Other) {
                mark_excluded(&mut tree, &rel_dir);
            } else if filter.included_file(&e.name, &rel) {
                tree.nodes.insert(rel, Node { is_dir: false, size: e.size, mtime: e.mtime });
            } else {
                mark_excluded(&mut tree, &rel_dir);
            }
        }
    }
    Ok(tree)
}

fn checksum(loc: &Location, rel: &str, cancel: &AtomicBool) -> Option<blake3::Hash> {
    let mut r = loc.vfs.lock().open_read(&loc.join(rel)).ok()?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if cancel.load(Relaxed) {
            return None;
        }
        match r.read(&mut buf) {
            Ok(0) => return Some(h.finalize()),
            Ok(n) => {
                h.update(&buf[..n]);
            }
            Err(_) => return None,
        }
    }
}

// ------------------------------------------------------------------ plan

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    CopyLR,
    CopyRL,
    DeleteL,
    DeleteR,
    Skip,
    /// Undecided: the user must pick a direction.
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    OnlyLeft,
    OnlyRight,
    Changed,
    /// File on one side, directory on the other.
    TypeMismatch,
}

#[derive(Clone, Debug)]
pub struct Meta {
    pub is_dir: bool,
    /// File size, or total size of the files below a directory.
    pub size: u64,
    /// 1 for files, number of files below for directories.
    pub files: u64,
    pub mtime: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub rel: String,
    pub left: Option<Meta>,
    pub right: Option<Meta>,
    pub kind: Kind,
    pub action: Action,
    pub default: Action,
    /// Short explanation shown in the plan ("target newer", "same time").
    pub note: &'static str,
}

impl Item {
    pub fn active(&self) -> bool {
        !matches!(self.action, Action::Skip | Action::Conflict)
    }
}

pub struct Plan {
    pub left: Location,
    pub right: Location,
    pub opts: SyncOptions,
    pub items: Vec<Item>,
    pub ltree: Tree,
    pub rtree: Tree,
}

fn secs(t: Option<SystemTime>) -> Option<i64> {
    t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64)
}

fn meta_of(tree: &Tree, rel: &str, n: &Node) -> Meta {
    if n.is_dir {
        let (files, size) = tree.subtree_stats(rel);
        Meta { is_dir: true, size, files, mtime: n.mtime }
    } else {
        Meta { is_dir: false, size: n.size, files: 1, mtime: n.mtime }
    }
}

/// Scan both sides (in parallel) and compute the plan.
pub fn compute_plan(
    left: &Location,
    right: &Location,
    opts: &SyncOptions,
    count: &AtomicU64,
    cancel: &AtomicBool,
) -> Result<Plan> {
    let (lt, rt) = std::thread::scope(|s| {
        let l = s.spawn(|| scan_tree(left, opts, count, cancel));
        let r = scan_tree(right, opts, count, cancel);
        (l.join().unwrap_or_else(|_| Err(anyhow::anyhow!("scan failed"))), r)
    });
    let (ltree, rtree) = (lt?, rt?);
    let items = diff_trees(left, &ltree, right, &rtree, opts, cancel)?;
    Ok(Plan { left: left.clone(), right: right.clone(), opts: opts.clone(), items, ltree, rtree })
}

/// Compare two scanned trees. Directories present on only one side become a
/// single item; files are compared according to `opts.compare`.
pub fn diff_trees(
    left: &Location,
    lt: &Tree,
    right: &Location,
    rt: &Tree,
    opts: &SyncOptions,
    cancel: &AtomicBool,
) -> Result<Vec<Item>> {
    let keys: BTreeSet<&String> = lt.nodes.keys().chain(rt.nodes.keys()).collect();
    let mut consumed: Vec<String> = Vec::new();
    let mut items = Vec::new();
    let tol = opts.tolerance_secs as i64;
    for rel in keys {
        if cancel.load(Relaxed) {
            bail!("cancelled");
        }
        if consumed
            .iter()
            .any(|c| rel.len() > c.len() && rel.starts_with(c.as_str()) && rel.as_bytes()[c.len()] == b'/')
        {
            continue;
        }
        let (l, r) = (lt.nodes.get(rel), rt.nodes.get(rel));
        let item = match (l, r) {
            (Some(ln), None) => {
                if ln.is_dir {
                    consumed.push(rel.clone());
                }
                let action = match opts.direction {
                    Direction::LeftToRight | Direction::TwoWay => Action::CopyLR,
                    Direction::RightToLeft if opts.mirror => Action::DeleteL,
                    Direction::RightToLeft => Action::Skip,
                };
                Item {
                    rel: rel.clone(),
                    left: Some(meta_of(lt, rel, ln)),
                    right: None,
                    kind: Kind::OnlyLeft,
                    action,
                    default: action,
                    note: "",
                }
            }
            (None, Some(rn)) => {
                if rn.is_dir {
                    consumed.push(rel.clone());
                }
                let action = match opts.direction {
                    Direction::RightToLeft | Direction::TwoWay => Action::CopyRL,
                    Direction::LeftToRight if opts.mirror => Action::DeleteR,
                    Direction::LeftToRight => Action::Skip,
                };
                Item {
                    rel: rel.clone(),
                    left: None,
                    right: Some(meta_of(rt, rel, rn)),
                    kind: Kind::OnlyRight,
                    action,
                    default: action,
                    note: "",
                }
            }
            (Some(ln), Some(rn)) if ln.is_dir && rn.is_dir => continue,
            (Some(ln), Some(rn)) if ln.is_dir != rn.is_dir => {
                if ln.is_dir || rn.is_dir {
                    consumed.push(rel.clone());
                }
                let action = match opts.direction {
                    Direction::LeftToRight => Action::CopyLR,
                    Direction::RightToLeft => Action::CopyRL,
                    Direction::TwoWay => Action::Conflict,
                };
                Item {
                    rel: rel.clone(),
                    left: Some(meta_of(lt, rel, ln)),
                    right: Some(meta_of(rt, rel, rn)),
                    kind: Kind::TypeMismatch,
                    action,
                    default: action,
                    note: "file vs directory",
                }
            }
            (Some(ln), Some(rn)) => {
                let (lm, rm) = (secs(ln.mtime), secs(rn.mtime));
                let time_differs = match (lm, rm) {
                    (Some(a), Some(b)) => (a - b).abs() > tol,
                    _ => false,
                };
                let differs = match opts.compare {
                    CompareBy::Size => ln.size != rn.size,
                    CompareBy::SizeTime => ln.size != rn.size || time_differs,
                    CompareBy::Checksum => {
                        ln.size != rn.size || checksum(left, rel, cancel) != checksum(right, rel, cancel)
                    }
                };
                if !differs {
                    continue;
                }
                // Which side is newer (None = same time within tolerance or unknown).
                let newer_left = match (lm, rm) {
                    (Some(a), Some(b)) if (a - b).abs() > tol => Some(a > b),
                    _ => None,
                };
                let (action, note) = match opts.direction {
                    Direction::LeftToRight => {
                        (Action::CopyLR, if newer_left == Some(false) { "target newer" } else { "" })
                    }
                    Direction::RightToLeft => {
                        (Action::CopyRL, if newer_left == Some(true) { "target newer" } else { "" })
                    }
                    Direction::TwoWay => match newer_left {
                        Some(true) => (Action::CopyLR, "left newer"),
                        Some(false) => (Action::CopyRL, "right newer"),
                        None => (Action::Conflict, "same time"),
                    },
                };
                Item {
                    rel: rel.clone(),
                    left: Some(meta_of(lt, rel, ln)),
                    right: Some(meta_of(rt, rel, rn)),
                    kind: Kind::Changed,
                    action,
                    default: action,
                    note,
                }
            }
            (None, None) => continue,
        };
        items.push(item);
    }
    Ok(items)
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub lr_files: u64,
    pub lr_bytes: u64,
    pub rl_files: u64,
    pub rl_bytes: u64,
    pub del_left: u64,
    pub del_right: u64,
    pub conflicts: u64,
    pub skipped: u64,
}

impl Plan {
    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for it in &self.items {
            match it.action {
                Action::CopyLR => {
                    let m = it.left.as_ref().expect("copy source");
                    s.lr_files += m.files;
                    s.lr_bytes += m.size;
                }
                Action::CopyRL => {
                    let m = it.right.as_ref().expect("copy source");
                    s.rl_files += m.files;
                    s.rl_bytes += m.size;
                }
                Action::DeleteL => s.del_left += it.left.as_ref().map(|m| m.files.max(1)).unwrap_or(1),
                Action::DeleteR => s.del_right += it.right.as_ref().map(|m| m.files.max(1)).unwrap_or(1),
                Action::Conflict => s.conflicts += 1,
                Action::Skip => s.skipped += 1,
            }
        }
        s
    }

    /// Turn the active items into executable steps.
    pub fn steps(&self) -> Vec<SyncStep> {
        let mut steps = Vec::new();
        for it in self.items.iter().filter(|i| i.active()) {
            match it.action {
                Action::CopyLR => self.copy_steps(&mut steps, it, true),
                Action::CopyRL => self.copy_steps(&mut steps, it, false),
                Action::DeleteL => self.delete_steps(&mut steps, &it.rel, true),
                Action::DeleteR => self.delete_steps(&mut steps, &it.rel, false),
                Action::Skip | Action::Conflict => {}
            }
        }
        steps
    }

    fn tree(&self, left: bool) -> &Tree {
        if left { &self.ltree } else { &self.rtree }
    }

    fn copy_steps(&self, steps: &mut Vec<SyncStep>, it: &Item, from_left: bool) {
        let (src, dst) = if from_left { (&it.left, &it.right) } else { (&it.right, &it.left) };
        let Some(src) = src else { return };
        // Replacing a different type: remove the target first.
        if let Some(d) = dst
            && d.is_dir != src.is_dir
        {
            self.delete_steps(steps, &it.rel, !from_left);
        }
        let overwrite = dst.as_ref().is_some_and(|d| !d.is_dir && !src.is_dir);
        if !src.is_dir {
            steps.push(SyncStep::Copy { from_left, rel: it.rel.clone(), size: src.size, existed: overwrite });
            return;
        }
        steps.push(SyncStep::Mkdir { to_left: !from_left, rel: it.rel.clone() });
        for (rel, n) in self.tree(from_left).below(&it.rel) {
            if n.is_dir {
                steps.push(SyncStep::Mkdir { to_left: !from_left, rel: rel.clone() });
            } else {
                steps.push(SyncStep::Copy { from_left, rel: rel.clone(), size: n.size, existed: false });
            }
        }
    }

    fn delete_steps(&self, steps: &mut Vec<SyncStep>, rel: &str, on_left: bool) {
        let tree = self.tree(on_left);
        let Some(node) = tree.nodes.get(rel) else { return };
        if !node.is_dir {
            steps.push(SyncStep::Delete { on_left, rel: rel.to_string(), size: node.size, tree: false });
            return;
        }
        if !tree.has_excluded.contains(rel) {
            let (_, size) = tree.subtree_stats(rel);
            steps.push(SyncStep::Delete { on_left, rel: rel.to_string(), size, tree: true });
            return;
        }
        // Keep excluded entries: delete files one by one, then empty directories, deepest first.
        let mut dirs = vec![rel.to_string()];
        for (r, n) in tree.below(rel) {
            if n.is_dir {
                dirs.push(r.clone());
            } else {
                steps.push(SyncStep::Delete { on_left, rel: r.clone(), size: n.size, tree: false });
            }
        }
        dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
        steps.extend(dirs.into_iter().map(|rel| SyncStep::Rmdir { on_left, rel }));
    }
}

// ------------------------------------------------------------------ entry point

/// Command `sync_dirs`: options dialog, then the plan screen.
pub fn open(app: &mut App) {
    let left = app.panels[0].loc.clone();
    let right = app.panels[1].loc.clone();
    if left.vfs.same(&right.vfs) && crate::vfs::normalize(&left.path) == crate::vfs::normalize(&right.path) {
        app.error("Both panels show the same directory.");
        return;
    }
    let o = last_options().lock().map(|o| o.clone()).unwrap_or_default();
    let dir_idx = match o.direction {
        Direction::LeftToRight => 0,
        Direction::RightToLeft => 1,
        Direction::TwoWay => 2,
    };
    let cmp_idx = match o.compare {
        CompareBy::SizeTime => 0,
        CompareBy::Size => 1,
        CompareBy::Checksum => 2,
    };
    let title = format!(
        "{}  <->  {}",
        trunc_left(&crate::util::tildify(&left.display()), 28),
        trunc_left(&crate::util::tildify(&right.display()), 28)
    );
    let d = Form::new("Synchronize directories")
        .width(66)
        .text(&title)
        .separator()
        .radio("dir", "Direction:", &["Left -> right", "Right -> left", "Two-way (newer wins)"], dir_idx)
        .radio(
            "cmp",
            "Compare by:",
            &["Size and time (2 s tolerance)", "Size only", "Content (blake3 checksum)"],
            cmp_idx,
        )
        .check("mirror", "Mirror: delete extra files on the target (one-way only)", o.mirror)
        .input("inc", "Include files (globs, ; separated, empty = all):", &o.include.join(";"))
        .input("exc", "Exclude (globs, ; separated):", &o.exclude.join(";"))
        .check("follow", "Follow symbolic links", o.follow_links)
        .check("dry", "Dry run (show the plan only)", o.dry_run)
        .buttons(&["Compare", "Cancel"], 0)
        .on_submit(move |app, r| {
            if r.button != 0 {
                return;
            }
            let opts = SyncOptions {
                direction: match r.radio("dir") {
                    1 => Direction::RightToLeft,
                    2 => Direction::TwoWay,
                    _ => Direction::LeftToRight,
                },
                compare: match r.radio("cmp") {
                    1 => CompareBy::Size,
                    2 => CompareBy::Checksum,
                    _ => CompareBy::SizeTime,
                },
                mirror: r.check("mirror"),
                include: split_globs(&r.text("inc")),
                exclude: split_globs(&r.text("exc")),
                follow_links: r.check("follow"),
                dry_run: r.check("dry"),
                tolerance_secs: 2,
            };
            if let Ok(mut l) = last_options().lock() {
                *l = opts.clone();
            }
            app.push_screen(Box::new(SyncScreen::start(left.clone(), right.clone(), opts)));
        });
    app.push_dialog(Box::new(d));
}

// ------------------------------------------------------------------ plan screen

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    All,
    ToRight,
    ToLeft,
    Delete,
    Conflict,
    Skip,
}

const VIEWS: [(View, &str); 6] = [
    (View::All, "All"),
    (View::ToRight, "-> right"),
    (View::ToLeft, "<- left"),
    (View::Delete, "Delete"),
    (View::Conflict, "Conflicts"),
    (View::Skip, "Skipped"),
];

type Slot = Arc<Mutex<Option<std::result::Result<Plan, String>>>>;

enum State {
    Scanning { count: Arc<AtomicU64>, cancel: Arc<AtomicBool>, slot: Slot, started: Instant },
    Ready(Box<Plan>),
    Failed(String),
}

pub struct SyncScreen {
    state: State,
    view: View,
    cursor: usize,
    offset: usize,
    rows: usize,
    list_rect: Rect,
    message: Option<String>,
    last_click: Option<(Instant, u16)>,
}

impl SyncScreen {
    pub fn start(left: Location, right: Location, opts: SyncOptions) -> SyncScreen {
        let count = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let slot: Slot = Arc::new(Mutex::new(None));
        let (c, k, s) = (count.clone(), cancel.clone(), slot.clone());
        std::thread::Builder::new()
            .name("sync-scan".into())
            .spawn(move || {
                let res = compute_plan(&left, &right, &opts, &c, &k).map_err(|e| format!("{e:#}"));
                if let Ok(mut g) = s.lock() {
                    *g = Some(res);
                }
            })
            .ok();
        SyncScreen {
            state: State::Scanning { count, cancel, slot, started: Instant::now() },
            view: View::All,
            cursor: 0,
            offset: 0,
            rows: 10,
            list_rect: Rect::default(),
            message: None,
            last_click: None,
        }
    }

    /// Screen with an already computed plan (tests).
    pub fn with_plan(plan: Plan) -> SyncScreen {
        SyncScreen {
            state: State::Ready(Box::new(plan)),
            view: View::All,
            cursor: 0,
            offset: 0,
            rows: 10,
            list_rect: Rect::default(),
            message: None,
            last_click: None,
        }
    }

    fn plan(&self) -> Option<&Plan> {
        match &self.state {
            State::Ready(p) => Some(p),
            _ => None,
        }
    }

    fn visible(&self) -> Vec<usize> {
        let Some(p) = self.plan() else { return Vec::new() };
        p.items
            .iter()
            .enumerate()
            .filter(|(_, it)| match self.view {
                View::All => true,
                View::ToRight => it.action == Action::CopyLR,
                View::ToLeft => it.action == Action::CopyRL,
                View::Delete => matches!(it.action, Action::DeleteL | Action::DeleteR),
                View::Conflict => it.action == Action::Conflict,
                View::Skip => it.action == Action::Skip,
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn current_item(&mut self) -> Option<&mut Item> {
        let vis = self.visible();
        let idx = *vis.get(self.cursor)?;
        match &mut self.state {
            State::Ready(p) => p.items.get_mut(idx),
            _ => None,
        }
    }

    fn execute(&mut self) -> ScreenOutcome {
        let Some(plan) = self.plan() else { return ScreenOutcome::Keep };
        if plan.opts.dry_run {
            self.message = Some("Dry run: nothing is changed. Press Esc to close.".into());
            return ScreenOutcome::Keep;
        }
        let steps = plan.steps();
        if steps.is_empty() {
            self.message = Some("Nothing to do.".into());
            return ScreenOutcome::Keep;
        }
        let s = plan.summary();
        let (left, right, follow) = (plan.left.clone(), plan.right.clone(), plan.opts.follow_links);
        ScreenOutcome::CloseThen(Box::new(move |app: &mut App| {
            let trash = app.cfg.use_trash;
            let run = move |app: &mut App| {
                app.start_job(JobSpec::Sync { left, right, steps, trash, follow_links: follow }, true);
            };
            let deletes = s.del_left + s.del_right;
            if deletes > 0 {
                let how = if trash { "move to the trash (local) or delete" } else { "permanently delete" };
                let msg = format!(
                    "The plan will {how} {deletes} file(s):\n{} on the left, {} on the right.\nContinue?",
                    s.del_left, s.del_right
                );
                app.push_dialog(dialog::confirm("Synchronize", &msg, run));
            } else {
                run(app);
            }
        }))
    }
}

fn action_label(a: Action) -> (&'static str, &'static str) {
    match a {
        Action::CopyLR => ("->", "copy"),
        Action::CopyRL => ("<-", "copy"),
        Action::DeleteL => ("x<", "delete L"),
        Action::DeleteR => (">x", "delete R"),
        Action::Skip => ("--", "skip"),
        Action::Conflict => ("??", "conflict"),
    }
}

fn meta_text(m: &Option<Meta>, w: usize) -> String {
    let s = match m {
        None => "-".to_string(),
        Some(m) if m.is_dir => format!("DIR {} files {}", m.files, human_size(m.size)),
        Some(m) => format!("{} {}", human_size(m.size), short_time(m.mtime)),
    };
    pad_right(&crate::util::trunc_end(&s, w), w)
}

impl Screen for SyncScreen {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let t = ctx.theme;
        let title = match &self.state {
            State::Ready(p) if p.opts.dry_run => " Synchronize (dry run) ",
            _ => " Synchronize ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(if ctx.classic { BorderType::Double } else { BorderType::Rounded })
            .border_style(t.border_active)
            .style(t.panel)
            .title(Line::styled(title, t.dialog_title).centered());
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height < 6 || inner.width < 30 {
            return;
        }
        let w = inner.width as usize;
        let line = |f: &mut Frame, y: u16, l: Line| f.render_widget(Paragraph::new(l), Rect { y, height: 1, ..inner });
        match &self.state {
            State::Scanning { count, started, .. } => {
                let msg = format!(
                    "Scanning... {} entries  {:.1} s   (Esc cancels)",
                    count.load(Relaxed),
                    started.elapsed().as_secs_f32()
                );
                line(f, inner.y + inner.height / 2, Line::styled(msg, t.header).centered());
                return;
            }
            State::Failed(e) => {
                line(f, inner.y + 1, Line::styled(format!("Cannot compare: {e}"), t.error));
                line(f, inner.y + 3, Line::styled("Press Esc to close.", t.hidden));
                return;
            }
            State::Ready(_) => {}
        }
        let vis = self.visible();
        // Layout bookkeeping first (needs &mut self), then borrow the plan.
        let list = Rect { y: inner.y + 2, height: inner.height.saturating_sub(5), ..inner };
        self.list_rect = list;
        self.rows = list.height as usize;
        if self.cursor >= vis.len() {
            self.cursor = vis.len().saturating_sub(1);
        }
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.rows > 0 && self.cursor >= self.offset + self.rows {
            self.offset = self.cursor + 1 - self.rows;
        }
        let plan = self.plan().expect("ready");
        let s = plan.summary();
        let dir = match plan.opts.direction {
            Direction::LeftToRight => "left -> right",
            Direction::RightToLeft => "right -> left",
            Direction::TwoWay => "two-way",
        };
        let cmp = match plan.opts.compare {
            CompareBy::SizeTime => "size+time",
            CompareBy::Size => "size",
            CompareBy::Checksum => "checksum",
        };
        let head = format!(
            "L: {}   R: {}   [{dir}, {cmp}{}]",
            crate::util::tildify(&plan.left.display()),
            crate::util::tildify(&plan.right.display()),
            if plan.opts.mirror { ", mirror" } else { "" }
        );
        line(f, inner.y, Line::styled(trunc_left(&head, w), t.header));
        // View tabs.
        let mut spans = Vec::new();
        for (i, (v, label)) in VIEWS.iter().enumerate() {
            let st = if *v == self.view { t.cursor } else { t.panel };
            spans.push(Span::styled(format!(" {} {label} ", i + 1), st));
            spans.push(Span::raw(" "));
        }
        line(f, inner.y + 1, Line::from(spans));
        // List.
        let side_w = 22.min(w / 4);
        let mut lines = Vec::new();
        for (row, &idx) in vis.iter().enumerate().skip(self.offset).take(self.rows) {
            let it = &plan.items[idx];
            let (arrow, verb) = action_label(it.action);
            let st = match it.action {
                Action::CopyLR | Action::CopyRL => t.diff_add,
                Action::DeleteL | Action::DeleteR => t.diff_del,
                Action::Conflict => t.diff_change,
                Action::Skip => t.hidden,
            };
            let st = if row == self.cursor { t.cursor } else { t.panel.patch(st) };
            let check = if it.active() { "[x]" } else { "[ ]" };
            let note = if it.note.is_empty() { String::new() } else { format!("  ({})", it.note) };
            let fixed = format!(
                "{check} {arrow} {:<9}{} | {} ",
                verb,
                meta_text(&it.left, side_w),
                meta_text(&it.right, side_w)
            );
            let rest = w.saturating_sub(str_width(&fixed));
            let path = format!(
                "{}{}{note}",
                it.rel,
                if it.left.as_ref().or(it.right.as_ref()).is_some_and(|m| m.is_dir) { "/" } else { "" }
            );
            lines.push(Line::styled(format!("{fixed}{}", pad_right(&trunc_left(&path, rest), rest)), st));
        }
        if vis.is_empty() {
            let msg = if plan.items.is_empty() { "The directories are in sync." } else { "No items in this view." };
            lines.push(Line::styled(msg, t.hidden));
        }
        f.render_widget(Paragraph::new(lines), list);
        // Summary and help.
        let y = inner.bottom() - 3;
        let sum = format!(
            "-> {} file(s) {}   <- {} file(s) {}   delete L {}  R {}   conflicts {}   skipped {}",
            s.lr_files,
            human_size(s.lr_bytes),
            s.rl_files,
            human_size(s.rl_bytes),
            s.del_left,
            s.del_right,
            s.conflicts,
            s.skipped
        );
        line(f, y, Line::styled(pad_right(&sum, w), t.status));
        let msg = self.message.clone().unwrap_or_default();
        line(f, y + 1, Line::styled(msg, t.header.patch(Style::default())));
        line(
            f,
            y + 2,
            Line::styled(
                "Space on/off  Left/Right direction  s skip  r reset  a all  Tab/1-6 view  Enter/F2 execute  Esc close",
                t.hidden,
            ),
        );
    }

    fn key(&mut self, key: KeyEvent) -> ScreenOutcome {
        if let State::Scanning { cancel, .. } = &self.state {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q')) {
                cancel.store(true, Relaxed);
                return ScreenOutcome::Close;
            }
            return ScreenOutcome::Keep;
        }
        if matches!(self.state, State::Failed(_)) {
            return if matches!(key.code, KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q') | KeyCode::Enter) {
                ScreenOutcome::Close
            } else {
                ScreenOutcome::Keep
            };
        }
        self.message = None;
        let n = self.visible().len();
        let page = self.rows.max(1);
        match key.code {
            KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q') => return ScreenOutcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(page),
            KeyCode::PageDown => self.cursor = (self.cursor + page).min(n.saturating_sub(1)),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = n.saturating_sub(1),
            KeyCode::Tab => {
                let i = VIEWS.iter().position(|(v, _)| *v == self.view).unwrap_or(0);
                self.view = VIEWS[(i + 1) % VIEWS.len()].0;
                self.cursor = 0;
            }
            KeyCode::BackTab => {
                let i = VIEWS.iter().position(|(v, _)| *v == self.view).unwrap_or(0);
                self.view = VIEWS[(i + VIEWS.len() - 1) % VIEWS.len()].0;
                self.cursor = 0;
            }
            KeyCode::Char(c @ '1'..='6') => {
                self.view = VIEWS[(c as u8 - b'1') as usize].0;
                self.cursor = 0;
            }
            KeyCode::Char(' ') | KeyCode::Insert => {
                if let Some(it) = self.current_item() {
                    it.action = if it.active() {
                        Action::Skip
                    } else if it.default != Action::Skip && it.default != Action::Conflict {
                        it.default
                    } else {
                        // No default direction: pick "left wins" when possible.
                        if it.left.is_some() { Action::CopyLR } else { Action::CopyRL }
                    };
                }
                self.cursor = (self.cursor + 1).min(n.saturating_sub(1));
            }
            KeyCode::Right | KeyCode::Char('l') => {
                // Left side wins: copy it to the right, or delete the right-only item.
                if let Some(it) = self.current_item() {
                    it.action = if it.left.is_some() { Action::CopyLR } else { Action::DeleteR };
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(it) = self.current_item() {
                    it.action = if it.right.is_some() { Action::CopyRL } else { Action::DeleteL };
                }
            }
            KeyCode::Char('s') => {
                if let Some(it) = self.current_item() {
                    it.action = Action::Skip;
                }
            }
            KeyCode::Char('r') => {
                if let Some(it) = self.current_item() {
                    it.action = it.default;
                }
            }
            KeyCode::Char('a') => {
                let vis = self.visible();
                if let State::Ready(p) = &mut self.state {
                    let any_active = vis.iter().any(|&i| p.items[i].active());
                    for i in vis {
                        let it = &mut p.items[i];
                        it.action = if any_active {
                            Action::Skip
                        } else if it.default != Action::Skip && it.default != Action::Conflict {
                            it.default
                        } else {
                            it.action
                        };
                    }
                }
            }
            KeyCode::Enter | KeyCode::F(2) => return self.execute(),
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> ScreenOutcome {
        let n = self.visible().len();
        match m.kind {
            MouseEventKind::ScrollUp => self.cursor = self.cursor.saturating_sub(3),
            MouseEventKind::ScrollDown => self.cursor = (self.cursor + 3).min(n.saturating_sub(1)),
            MouseEventKind::Down(MouseButton::Left) if self.list_rect.contains((m.column, m.row).into()) => {
                let idx = self.offset + (m.row - self.list_rect.y) as usize;
                if idx < n {
                    let double = self.last_click.is_some_and(|(t, r)| r == m.row && t.elapsed().as_millis() < 400);
                    self.cursor = idx;
                    self.last_click = Some((Instant::now(), m.row));
                    if double {
                        return self.key(KeyEvent::from(KeyCode::Char(' ')));
                    }
                }
            }
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn tick(&mut self) -> ScreenOutcome {
        if let State::Scanning { slot, .. } = &self.state {
            let res = slot.lock().ok().and_then(|mut g| g.take());
            match res {
                Some(Ok(plan)) => self.state = State::Ready(Box::new(plan)),
                Some(Err(e)) => self.state = State::Failed(e),
                None => {}
            }
        }
        ScreenOutcome::Keep
    }

    fn title(&self) -> String {
        "Synchronize".into()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// True when both trees hold the same files with the same content (tests, verification).
pub fn trees_identical(a: &Path, b: &Path) -> bool {
    let list = |root: &Path| -> BTreeMap<String, Vec<u8>> {
        walkdir::WalkDir::new(root)
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                let rel = e.path().strip_prefix(root).unwrap_or(e.path()).to_string_lossy().into_owned();
                (rel, std::fs::read(e.path()).unwrap_or_default())
            })
            .collect()
    };
    list(a) == list(b)
}

#[cfg(test)]
mod tests;
