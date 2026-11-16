//! A file panel: directory listing state, cursor, selection, sorting, history.

use crate::config::Config;
use crate::tree::TreeState;
use crate::vfs::{self, Entry, EntryKind, Location, VfsHandle};
use anyhow::{Result, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError, bounded};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[cfg(test)]
mod async_tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListMode {
    Full,
    Brief,
    Long,
}

impl ListMode {
    pub fn parse(s: &str) -> ListMode {
        match s {
            "brief" => ListMode::Brief,
            "long" => ListMode::Long,
            _ => ListMode::Full,
        }
    }
    pub fn next(self) -> ListMode {
        match self {
            ListMode::Full => ListMode::Brief,
            ListMode::Brief => ListMode::Long,
            ListMode::Long => ListMode::Full,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelView {
    List,
    Info,
    Tree,
    QuickView,
    /// The embedded terminal docked into this panel (see term.rs).
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Natural,
    Extension,
    Size,
    Modified,
    Accessed,
    Changed,
    Inode,
    Unsorted,
}

impl SortKey {
    pub const ALL: &'static [SortKey] = &[
        SortKey::Name,
        SortKey::Natural,
        SortKey::Extension,
        SortKey::Size,
        SortKey::Modified,
        SortKey::Accessed,
        SortKey::Changed,
        SortKey::Inode,
        SortKey::Unsorted,
    ];
    pub fn parse(s: &str) -> SortKey {
        match s {
            "natural" => SortKey::Natural,
            "ext" | "extension" => SortKey::Extension,
            "size" => SortKey::Size,
            "mtime" | "modified" => SortKey::Modified,
            "atime" | "accessed" => SortKey::Accessed,
            "ctime" | "changed" => SortKey::Changed,
            "inode" => SortKey::Inode,
            "unsorted" | "none" => SortKey::Unsorted,
            _ => SortKey::Name,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SortKey::Name => "Name",
            SortKey::Natural => "Natural name",
            SortKey::Extension => "Extension",
            SortKey::Size => "Size",
            SortKey::Modified => "Modify time",
            SortKey::Accessed => "Access time",
            SortKey::Changed => "Change time",
            SortKey::Inode => "Inode",
            SortKey::Unsorted => "Unsorted",
        }
    }
    pub fn short(self) -> &'static str {
        match self {
            SortKey::Name => "n",
            SortKey::Natural => "N",
            SortKey::Extension => "e",
            SortKey::Size => "s",
            SortKey::Modified => "m",
            SortKey::Accessed => "a",
            SortKey::Changed => "c",
            SortKey::Inode => "i",
            SortKey::Unsorted => "u",
        }
    }
}

pub struct Panel {
    pub loc: Location,
    pub entries: Vec<Entry>,
    pub cursor: usize,
    pub offset: usize,
    pub marked: HashSet<String>,
    pub sort: SortKey,
    pub reverse: bool,
    pub dirs_first: bool,
    pub case_sensitive: bool,
    pub show_hidden: bool,
    /// Glob filter applied to file names (directories always shown).
    pub filter: Option<String>,
    pub mode: ListMode,
    pub view: PanelView,
    pub quick_search: Option<String>,
    back: Vec<Location>,
    forward: Vec<Location>,
    /// Stack of (location, archive name) we came from when entering archives / remote hosts.
    pub vfs_stack: Vec<(Location, String)>,
    /// When set, the panel shows this list of files (relative to `loc.path`) instead of a directory.
    pub panelized: Option<Vec<String>>,
    pub error: Option<String>,
    pub dir_sizes: HashMap<String, u64>,
    pub tree: Option<TreeState>,
    /// Rows available for entries in the last render (for paging).
    pub page_rows: usize,
    /// Columns in the last render (brief mode).
    pub columns: usize,
    /// A directory listing still running on a background thread.
    pub loading: Option<Loading>,
    /// Entry to put the cursor on once the pending listing arrives.
    pending_focus: Option<String>,
    /// Cached `loc.display()` so rendering never waits on the VFS lock.
    display: String,
}

/// State of an asynchronous directory listing.
pub struct Loading {
    rx: Receiver<Result<Vec<Entry>>>,
    pub started: Instant,
    /// Where to go back to if the listing fails or is cancelled (set by `chdir`).
    revert: Option<Revert>,
}

struct Revert {
    loc: Location,
    panelized: Option<Vec<String>>,
    focus: Option<String>,
    pushed_history: bool,
}

thread_local! {
    /// How long `load` waits for a listing before switching to the async path.
    /// Per thread, so a test forcing the async path does not affect others.
    static SYNC_WAIT_MS: std::cell::Cell<u64> = const { std::cell::Cell::new(50) };
}

/// Sender used by listing threads to wake the UI loop when they finish.
static WAKER: OnceLock<Sender<crate::app::AppEvent>> = OnceLock::new();

/// Install the UI wake-up channel (called once by `App::new`).
pub fn set_waker(tx: Sender<crate::app::AppEvent>) {
    let _ = WAKER.set(tx);
}

/// Change the synchronous wait for the current thread (tests use 0 to force the
/// async path).
pub fn set_sync_wait(ms: u64) {
    SYNC_WAIT_MS.with(|c| c.set(ms));
}

impl Panel {
    pub fn new(loc: Location, cfg: &Config) -> Panel {
        let mut p = Panel {
            loc,
            entries: Vec::new(),
            cursor: 0,
            offset: 0,
            marked: HashSet::new(),
            sort: SortKey::parse(&cfg.sort),
            reverse: cfg.sort_reverse,
            dirs_first: cfg.dirs_first,
            case_sensitive: cfg.case_sensitive_sort,
            show_hidden: cfg.show_hidden,
            filter: None,
            mode: ListMode::parse(&cfg.listing),
            view: PanelView::List,
            quick_search: None,
            back: Vec::new(),
            forward: Vec::new(),
            vfs_stack: Vec::new(),
            panelized: None,
            error: None,
            dir_sizes: HashMap::new(),
            tree: None,
            page_rows: 20,
            columns: 1,
            loading: None,
            pending_focus: None,
            display: String::new(),
        };
        if let Err(e) = p.load() {
            p.error = Some(e.to_string());
            // Fall back to home, then root.
            for fallback in [crate::util::home(), PathBuf::from("/")] {
                p.loc = Location::local(fallback);
                if p.load().is_ok() {
                    break;
                }
            }
        }
        p
    }

    pub fn current(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    pub fn current_name(&self) -> Option<String> {
        self.current().map(|e| e.name.clone())
    }

    /// Absolute path (inside the VFS) of the entry under the cursor.
    pub fn current_path(&self) -> Option<PathBuf> {
        let e = self.current()?;
        if e.is_parent() {
            return None;
        }
        Some(self.loc.join(&e.name))
    }

    pub fn is_local(&self) -> bool {
        self.loc.is_local()
    }

    /// Marked entries, or the entry under the cursor when nothing is marked.
    pub fn selection(&self) -> Vec<Entry> {
        if !self.marked.is_empty() {
            return self.entries.iter().filter(|e| self.marked.contains(&e.name)).cloned().collect();
        }
        match self.current() {
            Some(e) if !e.is_parent() => vec![e.clone()],
            _ => vec![],
        }
    }

    pub fn marked_stats(&self) -> (usize, u64) {
        let mut n = 0;
        let mut size = 0;
        for e in &self.entries {
            if self.marked.contains(&e.name) {
                n += 1;
                size += if e.is_dir() { self.dir_sizes.get(&e.name).copied().unwrap_or(0) } else { e.size };
            }
        }
        (n, size)
    }

    /// Reload the directory listing.
    ///
    /// The listing runs on a background thread. If it finishes within a short wait
    /// (tiny local directories, tests) the result is applied right away; otherwise the
    /// panel shows a loading state and [`Panel::poll_load`] applies the result later.
    pub fn load(&mut self) -> Result<()> {
        self.display = self.loc.display();
        self.loading = None;
        if let Some(files) = &self.panelized {
            let raw: Vec<Entry> = {
                let mut v = self.loc.vfs.lock();
                files
                    .iter()
                    .filter_map(|rel| {
                        let mut e = v.stat(&self.loc.path.join(rel)).ok()?;
                        e.name = rel.clone();
                        Some(e)
                    })
                    .collect()
            };
            self.apply_listing(raw);
            return Ok(());
        }
        let (tx, rx) = bounded(1);
        let local = self.loc.is_local();
        let vfs = self.loc.vfs.clone();
        let path = self.loc.path.clone();
        std::thread::Builder::new().name("list".into()).spawn(move || {
            // Local listings use the stateless backend directly, so the panel's VFS
            // mutex stays free for the UI while a slow directory is read.
            let res = if local { vfs::Vfs::list(&mut vfs::local::LocalFs, &path) } else { vfs.lock().list(&path) };
            if tx.send(res).is_ok()
                && let Some(w) = WAKER.get()
            {
                let _ = w.send(crate::app::AppEvent::Call(Box::new(|app| app.poll_loads())));
            }
        })?;
        match rx.recv_timeout(Duration::from_millis(SYNC_WAIT_MS.with(|c| c.get()))) {
            Ok(res) => {
                self.apply_listing(res?);
                Ok(())
            }
            Err(RecvTimeoutError::Timeout) => {
                self.loading = Some(Loading { rx, started: Instant::now(), revert: None });
                // Keep ".." available so the user can back out while waiting.
                self.entries = if self.at_root() { Vec::new() } else { vec![Entry::parent()] };
                self.cursor = 0;
                self.offset = 0;
                Ok(())
            }
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("directory listing failed")),
        }
    }

    fn at_root(&self) -> bool {
        self.loc.path == Path::new("/") && self.vfs_stack.is_empty() && self.panelized.is_none()
    }

    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Cached human readable location (never locks the VFS).
    pub fn display_name(&self) -> &str {
        &self.display
    }

    /// Apply a finished background listing. Returns None when nothing is pending or
    /// the listing is still running, Some(Err) when it failed (the panel has already
    /// gone back to where it was).
    pub fn poll_load(&mut self) -> Option<Result<()>> {
        let res = match self.loading.as_ref()?.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(anyhow!("directory listing failed")),
        };
        let loading = self.loading.take()?;
        match res {
            Ok(raw) => {
                self.apply_listing(raw);
                Some(Ok(()))
            }
            Err(e) => {
                self.undo_chdir(loading.revert);
                self.error = Some(e.to_string());
                Some(Err(e))
            }
        }
    }

    /// Abandon a pending listing (Esc). Goes back to the previous directory when the
    /// listing came from a directory change. Returns true if something was cancelled.
    pub fn cancel_load(&mut self) -> bool {
        let Some(loading) = self.loading.take() else { return false };
        if loading.revert.is_some() {
            self.undo_chdir(loading.revert);
        } else {
            self.error = Some("Listing cancelled".into());
        }
        true
    }

    fn undo_chdir(&mut self, revert: Option<Revert>) {
        let Some(r) = revert else { return };
        if r.pushed_history {
            self.back.pop();
        }
        self.loc = r.loc;
        self.panelized = r.panelized;
        self.pending_focus = r.focus;
        if self.load().is_err() {
            self.entries.clear();
        }
    }

    fn apply_listing(&mut self, raw: Vec<Entry>) {
        let mut entries: Vec<Entry> = raw
            .into_iter()
            .filter(|e| self.show_hidden || !e.name.starts_with('.'))
            .filter(|e| match &self.filter {
                Some(f) if !e.is_dir() => crate::util::glob_matcher(f, false).is_none_or(|m| m.is_match(&e.name)),
                _ => true,
            })
            .collect();
        self.sort_entries(&mut entries);
        let at_root = self.loc.path == Path::new("/") && self.vfs_stack.is_empty() && self.panelized.is_none();
        if !at_root {
            let mut up = Entry::parent();
            if let Some(local) = self.loc.local_path()
                && let Ok(e) = vfs::local::stat_local(&local.join(".."))
            {
                up.mtime = e.mtime;
            }
            entries.insert(0, up);
        }
        let names: HashSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        self.marked.retain(|m| names.contains(m.as_str()));
        self.entries = entries;
        self.error = None;
        self.clamp();
        if let Some(f) = self.pending_focus.take() {
            self.focus(&f);
        }
    }

    /// Reload keeping the cursor on the same file name if it still exists.
    pub fn reload(&mut self) {
        // While a listing is pending, the cursor name to keep is the one already queued.
        let name = if self.loading.is_some() { self.pending_focus.clone() } else { self.current_name() };
        let cur = self.cursor;
        if let Err(e) = self.load() {
            self.error = Some(e.to_string());
            // Directory vanished: walk up until something loads.
            let mut p = self.loc.path.clone();
            while p.pop() {
                self.loc = self.loc.with_path(p.clone());
                if self.load().is_ok() {
                    break;
                }
            }
            return;
        }
        if let Some(n) = name
            && !self.focus(&n)
        {
            self.cursor = cur.min(self.entries.len().saturating_sub(1));
        }
    }

    fn sort_entries(&self, v: &mut [Entry]) {
        use std::cmp::Ordering;
        let cs = self.case_sensitive;
        let name_cmp = |a: &Entry, b: &Entry| -> Ordering {
            if cs { a.name.cmp(&b.name) } else { a.name.to_lowercase().cmp(&b.name.to_lowercase()) }
        };
        let key = self.sort;
        let rev = self.reverse;
        v.sort_by(|a, b| {
            if self.dirs_first {
                match (a.is_dir(), b.is_dir()) {
                    (true, false) => return Ordering::Less,
                    (false, true) => return Ordering::Greater,
                    _ => {}
                }
            }
            let o = match key {
                SortKey::Name => name_cmp(a, b),
                SortKey::Natural => natural_cmp(&a.name, &b.name, cs),
                SortKey::Extension => {
                    crate::icons::ext_of(&a.name).cmp(&crate::icons::ext_of(&b.name)).then_with(|| name_cmp(a, b))
                }
                SortKey::Size => b.size.cmp(&a.size).then_with(|| name_cmp(a, b)),
                SortKey::Modified => b.mtime.cmp(&a.mtime).then_with(|| name_cmp(a, b)),
                SortKey::Accessed => b.atime.cmp(&a.atime).then_with(|| name_cmp(a, b)),
                SortKey::Changed => b.ctime.cmp(&a.ctime).then_with(|| name_cmp(a, b)),
                SortKey::Inode => a.inode.cmp(&b.inode),
                SortKey::Unsorted => Ordering::Equal,
            };
            if rev { o.reverse() } else { o }
        });
    }

    pub fn resort(&mut self) {
        let name = self.current_name();
        let up = self.entries.first().filter(|e| e.is_parent()).cloned();
        let mut rest: Vec<Entry> = self.entries.drain(..).filter(|e| !e.is_parent()).collect();
        self.sort_entries(&mut rest);
        if let Some(u) = up {
            self.entries.push(u);
        }
        self.entries.extend(rest);
        if let Some(n) = name {
            self.focus(&n);
        }
    }

    pub fn focus(&mut self, name: &str) -> bool {
        if self.loading.is_some() {
            // Applied when the listing arrives.
            self.pending_focus = Some(name.to_string());
            return true;
        }
        if let Some(i) = self.entries.iter().position(|e| e.name == name) {
            self.cursor = i;
            self.clamp();
            true
        } else {
            false
        }
    }

    pub fn clamp(&mut self) {
        if self.entries.is_empty() {
            self.cursor = 0;
            self.offset = 0;
            return;
        }
        self.cursor = self.cursor.min(self.entries.len() - 1);
    }

    /// Keep the cursor visible given `visible` slots.
    pub fn scroll_into_view(&mut self, visible: usize) {
        if visible == 0 {
            return;
        }
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + visible {
            self.offset = self.cursor + 1 - visible;
        }
        let max_off = self.entries.len().saturating_sub(visible);
        self.offset = self.offset.min(max_off);
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, n - 1) as usize;
    }

    pub fn page(&self) -> usize {
        (self.page_rows * self.columns.max(1)).max(1)
    }

    /// Change directory, recording history.
    pub fn chdir(&mut self, loc: Location, focus: Option<&str>) -> Result<()> {
        let old = std::mem::replace(&mut self.loc, loc);
        let old_panelized = self.panelized.take();
        let old_filter_name = self.current_name();
        self.pending_focus = None;
        match self.load() {
            Ok(()) => {
                let mut pushed_history = false;
                if old.vfs.same(&self.loc.vfs) || old.is_local() {
                    self.back.push(old.clone());
                    pushed_history = true;
                    if self.back.len() > 100 {
                        self.back.remove(0);
                    }
                    self.forward.clear();
                }
                if let Some(l) = self.loading.as_mut() {
                    l.revert =
                        Some(Revert { loc: old, panelized: old_panelized, focus: old_filter_name, pushed_history });
                }
                self.marked.clear();
                self.dir_sizes.clear();
                self.cursor = 0;
                self.offset = 0;
                self.quick_search = None;
                if let Some(f) = focus {
                    self.focus(f);
                }
                Ok(())
            }
            Err(e) => {
                self.loc = old;
                self.panelized = old_panelized;
                let _ = self.load();
                if let Some(n) = old_filter_name {
                    self.focus(&n);
                }
                Err(e)
            }
        }
    }

    pub fn chdir_path(&mut self, path: impl Into<PathBuf>) -> Result<()> {
        let loc = self.loc.with_path(path);
        self.chdir(loc, None)
    }

    /// Go to the parent directory (or leave an archive / remote host / panelized list).
    pub fn go_up(&mut self) -> Result<()> {
        if self.panelized.is_some() {
            self.panelized = None;
            self.load()?;
            self.cursor = 0;
            return Ok(());
        }
        let here = self.loc.path.clone();
        if here == Path::new("/") || here.as_os_str().is_empty() {
            if let Some((loc, name)) = self.vfs_stack.pop() {
                return self.chdir(loc, Some(&name));
            }
            return Ok(());
        }
        let name = here.file_name().map(|s| s.to_string_lossy().into_owned());
        let parent = here.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
        self.chdir_path(parent)?;
        if let Some(n) = name {
            self.focus(&n);
        }
        Ok(())
    }

    /// Enter the entry under the cursor if it is a directory or a browsable archive.
    /// Returns Ok(false) if it is a regular file (caller decides what to do).
    pub fn enter(&mut self) -> Result<bool> {
        let Some(e) = self.current().cloned() else { return Ok(false) };
        if e.is_parent() {
            self.go_up()?;
            return Ok(true);
        }
        if e.is_dir() {
            let target = self.loc.join(&e.name);
            if self.panelized.is_some() {
                self.panelized = None;
            }
            self.chdir_path(vfs::normalize(&target))?;
            return Ok(true);
        }
        if matches!(e.kind, EntryKind::File | EntryKind::Symlink { .. }) && vfs::archive::browsable(&self.loc, &e.name)
        {
            self.enter_archive(&e.name)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn enter_archive(&mut self, name: &str) -> Result<()> {
        let path = self.loc.join(name);
        let fs = vfs::archive::open(&self.loc, &path)?;
        let prev = self.loc.clone();
        let loc = Location { vfs: VfsHandle::new(fs), path: PathBuf::from("/") };
        // Push first so the archive root lists a ".." entry.
        self.vfs_stack.push((prev, name.to_string()));
        if let Err(e) = self.chdir(loc, None) {
            self.vfs_stack.pop();
            return Err(e);
        }
        Ok(())
    }

    /// Switch to a new VFS (e.g. an SFTP connection), remembering where we were.
    pub fn enter_vfs(&mut self, loc: Location) -> Result<()> {
        let prev = self.loc.clone();
        let name = self.current_name().unwrap_or_default();
        self.vfs_stack.push((prev, name));
        if let Err(e) = self.chdir(loc, None) {
            self.vfs_stack.pop();
            return Err(e);
        }
        Ok(())
    }

    pub fn history_back(&mut self) -> Result<()> {
        if let Some(loc) = self.back.pop() {
            let cur = self.loc.clone();
            let name = cur.path.file_name().map(|n| n.to_string_lossy().into_owned());
            self.loc = loc;
            self.panelized = None;
            self.load()?;
            self.forward.push(cur);
            self.cursor = 0;
            if let Some(n) = name {
                self.focus(&n);
            }
        }
        Ok(())
    }

    pub fn history_forward(&mut self) -> Result<()> {
        if let Some(loc) = self.forward.pop() {
            let cur = self.loc.clone();
            self.loc = loc;
            self.panelized = None;
            self.load()?;
            self.back.push(cur);
            self.cursor = 0;
        }
        Ok(())
    }

    pub fn toggle_mark_current(&mut self) {
        if let Some(e) = self.current() {
            if e.is_parent() {
                return;
            }
            let n = e.name.clone();
            if !self.marked.remove(&n) {
                self.marked.insert(n);
            }
        }
    }

    pub fn mark_matching(&mut self, pattern: &str, select: bool, dirs_too: bool, case_sensitive: bool) -> usize {
        let Some(m) = crate::util::glob_matcher(pattern, case_sensitive) else { return 0 };
        let mut n = 0;
        for e in &self.entries {
            if e.is_parent() || (e.is_dir() && !dirs_too) {
                continue;
            }
            if m.is_match(&e.name) {
                n += 1;
                if select {
                    self.marked.insert(e.name.clone());
                } else {
                    self.marked.remove(&e.name);
                }
            }
        }
        n
    }

    pub fn invert_marks(&mut self) {
        let all: Vec<String> =
            self.entries.iter().filter(|e| !e.is_parent() && !e.is_dir()).map(|e| e.name.clone()).collect();
        for n in all {
            if !self.marked.remove(&n) {
                self.marked.insert(n);
            }
        }
    }

    /// Incremental search: jump to the first entry starting with (or containing) the text.
    pub fn quick_search_jump(&mut self, text: &str, from_next: bool) -> bool {
        let needle = text.to_lowercase();
        let n = self.entries.len();
        if n == 0 {
            return false;
        }
        let start = if from_next { self.cursor + 1 } else { self.cursor };
        let glob = if text.contains('*') || text.contains('?') {
            crate::util::glob_matcher(&format!("{text}*"), false)
        } else {
            None
        };
        for i in 0..n {
            let idx = (start + i) % n;
            let name = &self.entries[idx].name;
            let hit = match &glob {
                Some(g) => g.is_match(name),
                None => name.to_lowercase().starts_with(&needle),
            };
            if hit {
                self.cursor = idx;
                return true;
            }
        }
        false
    }

    pub fn title(&self) -> String {
        let shown = if self.display.is_empty() { self.loc.display() } else { self.display.clone() };
        let mut t = crate::util::tildify(&shown);
        if self.panelized.is_some() {
            t = format!("Panelize: {t}");
        }
        if let Some(f) = &self.filter {
            t = format!("{t} [{f}]");
        }
        t
    }
}

/// Natural ordering: "file2" < "file10".
pub fn natural_cmp(a: &str, b: &str, case_sensitive: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (a, b) = if case_sensitive { (a.to_string(), b.to_string()) } else { (a.to_lowercase(), b.to_lowercase()) };
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let na2 = na.trim_start_matches('0');
                let nb2 = nb.trim_start_matches('0');
                let o = na2.len().cmp(&nb2.len()).then_with(|| na2.cmp(nb2));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}
