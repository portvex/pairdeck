//! Application state and event dispatch.

use crate::config::{Config, State};
use crate::dialog::{self, Dialog, Outcome};
use crate::keymap::{Command, Key, KeyMap, KeyResult};
use crate::lineedit::LineEdit;
use crate::ops::{self, ConflictChoice, ErrorChoice, JobMsg, JobProgress};
use crate::panel::{Panel, PanelView};
use crate::preview::{PreviewMsg, Previewer};
use crate::screen::{Screen, ScreenOutcome};
use crate::theme::Theme;
use crate::vfs::Location;
use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui_image::picker::Picker;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

pub enum AppEvent {
    Term(Event),
    Job(JobMsg),
    Preview(PreviewMsg),
    FsChanged(PathBuf),
    /// Generic callback from a worker thread, run on the UI thread.
    Call(Box<dyn FnOnce(&mut App) + Send>),
}

/// Something that needs the real terminal (run outside the TUI).
pub enum External {
    /// Run a shell command line in `cwd`, then wait for a key.
    Command { cmd: String, cwd: PathBuf, pause: bool },
    /// Interactive subshell (Ctrl-O). Its final directory is synced back.
    Shell { cwd: PathBuf },
    /// Run a program directly (external editor / viewer).
    Program { program: String, args: Vec<String>, cwd: PathBuf },
    /// Show the terminal's normal screen (previous command output) until a key is pressed.
    ShowOutput,
}

#[derive(Default, Clone)]
pub struct LayoutCache {
    pub menubar: Rect,
    pub panels: [Rect; 2],
    pub lists: [Rect; 2],
    pub cmdline: Rect,
    pub buttonbar: Rect,
    pub buttons: Vec<(Rect, u8)>,
    pub menu_titles: Vec<(Rect, usize)>,
}

pub struct App {
    pub cfg: Config,
    pub state: State,
    pub theme: Theme,
    pub keymap: KeyMap,
    pub panels: [Panel; 2],
    pub active: usize,
    pub cmdline: LineEdit,
    pub dialogs: Vec<Box<dyn Dialog>>,
    pub screens: Vec<Box<dyn Screen>>,
    pub jobs: Vec<Arc<JobProgress>>,
    pub tx: Sender<AppEvent>,
    pub rx: Receiver<AppEvent>,
    pub quit: bool,
    pub message: Option<(String, Instant, bool)>,
    pub previewer: Previewer,
    pub picker: Option<Picker>,
    pub external: Option<External>,
    pub layout: LayoutCache,
    pub full_redraw: bool,
    pub cmdline_hidden: bool,
    watcher: Option<notify::RecommendedWatcher>,
    watched: [Option<PathBuf>; 2],
    dirty: [Option<Instant>; 2],
    last_click: Option<(Instant, u16, u16)>,
    /// Remember which job ids show a foreground progress dialog.
    pub fg_jobs: Vec<u64>,
    pending_progress: Vec<(Arc<JobProgress>, Instant)>,
    /// Git status cache (see git.rs).
    pub git: crate::git::GitState,
    /// Last directory recorded for frecency, per panel.
    last_visit: [Option<PathBuf>; 2],
    /// Hidden tabs of each panel side (the visible tab is `panels[i]`).
    pub tabs: [crate::tabs::TabSet; 2],
    /// Reversible operations of this session.
    pub undo: crate::undo::Journal,
    /// Watches the config directory for live reload (see reload.rs).
    pub config_watch: Option<crate::reload::ConfigWatch>,
    /// Embedded terminal pane (see term/mod.rs).
    pub term: crate::term::TermPane,
}

impl App {
    pub fn new(cfg: Config, start_dirs: [Option<PathBuf>; 2], picker: Option<Picker>) -> App {
        let state = State::load();
        let theme = Theme::load(&cfg.theme);
        let (keymap, key_errors) = KeyMap::new(&cfg.keys);
        let (tx, rx) = unbounded();
        crate::panel::set_waker(tx.clone());
        let cwd = std::env::current_dir().unwrap_or_else(|_| crate::util::home());
        let left = start_dirs[0].clone().unwrap_or_else(|| cwd.clone());
        let right = start_dirs[1]
            .clone()
            .or_else(|| state.right_dir.clone().map(PathBuf::from).filter(|p| p.is_dir()))
            .unwrap_or_else(|| cwd.clone());
        let panels = [Panel::new(Location::local(left), &cfg), Panel::new(Location::local(right), &cfg)];
        let previewer = Previewer::new(tx.clone(), picker.clone());
        let mut app = App {
            cfg,
            state,
            theme,
            keymap,
            panels,
            active: 0,
            cmdline: LineEdit::new(""),
            dialogs: Vec::new(),
            screens: Vec::new(),
            jobs: Vec::new(),
            tx,
            rx,
            quit: false,
            message: None,
            previewer,
            picker,
            external: None,
            layout: LayoutCache::default(),
            full_redraw: false,
            cmdline_hidden: false,
            watcher: None,
            watched: [None, None],
            dirty: [None, None],
            last_click: None,
            fg_jobs: Vec::new(),
            pending_progress: Vec::new(),
            git: Default::default(),
            last_visit: [None, None],
            tabs: Default::default(),
            undo: Default::default(),
            config_watch: None,
            term: Default::default(),
        };
        crate::tabs::restore(&mut app);
        app.cmdline = LineEdit::new("").with_history(app.state.cmd_history.clone());
        if !key_errors.is_empty() {
            app.push_dialog(dialog::error(&key_errors.join("\n")));
        }
        if let Some(side) = app.state.quick_view_side.filter(|s| *s < 2) {
            app.panels[side].view = PanelView::QuickView;
        }
        app.setup_watcher();
        crate::reload::start(&mut app);
        app
    }

    // ------------------------------------------------------------ helpers

    pub fn panel(&self) -> &Panel {
        &self.panels[self.active]
    }
    pub fn panel_mut(&mut self) -> &mut Panel {
        &mut self.panels[self.active]
    }
    pub fn other(&self) -> &Panel {
        &self.panels[1 - self.active]
    }
    pub fn other_mut(&mut self) -> &mut Panel {
        &mut self.panels[1 - self.active]
    }

    pub fn push_dialog(&mut self, d: Box<dyn Dialog>) {
        self.dialogs.push(d);
    }

    pub fn push_screen(&mut self, s: Box<dyn Screen>) {
        self.screens.push(s);
    }

    /// Access the top screen if it is of type `T`.
    pub fn top_screen<T: Screen>(&mut self) -> Option<&mut T> {
        self.screens.last_mut().and_then(|s| s.as_any_mut().downcast_mut::<T>())
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now(), false));
    }

    pub fn error(&mut self, msg: impl std::fmt::Display) {
        self.push_dialog(dialog::error(&msg.to_string()));
    }

    pub fn flash_error(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now(), true));
    }

    pub fn reload_panels(&mut self) {
        self.refresh_git();
        for p in &mut self.panels {
            p.reload();
        }
        self.previewer.invalidate();
        self.setup_watcher();
    }

    pub fn active_local_dir(&self) -> PathBuf {
        let p = self.panel();
        match p.loc.local_path() {
            Some(d) => d.to_path_buf(),
            None => p
                .vfs_stack
                .first()
                .and_then(|(l, _)| l.local_path().map(Path::to_path_buf))
                .unwrap_or_else(crate::util::home),
        }
    }

    pub fn chdir_active(&mut self, loc: Location) {
        let disp = loc.display();
        if let Err(e) = self.panel_mut().chdir(loc, None) {
            self.error(format!("Cannot change directory:\n{e:#}"));
        } else {
            State::push_history(&mut self.state.dir_history, &disp);
            self.setup_watcher();
        }
    }

    // ------------------------------------------------------------ file watching

    pub fn setup_watcher(&mut self) {
        self.refresh_git();
        if !self.cfg.auto_refresh {
            return;
        }
        use notify::{RecursiveMode, Watcher};
        if self.watcher.is_none() {
            let tx = self.tx.clone();
            let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res {
                    if matches!(ev.kind, notify::EventKind::Access(_)) {
                        return;
                    }
                    for p in ev.paths {
                        let dir = p.parent().map(Path::to_path_buf).unwrap_or(p);
                        let _ = tx.send(AppEvent::FsChanged(dir));
                    }
                }
            });
            self.watcher = w.ok();
        }
        let Some(w) = self.watcher.as_mut() else { return };
        for i in 0..2 {
            let want = self.panels[i].loc.local_path().map(Path::to_path_buf);
            if want != self.watched[i] {
                if let Some(old) = self.watched[i].take()
                    && self.watched[1 - i].as_ref() != Some(&old)
                {
                    let _ = w.unwatch(&old);
                }
                if let Some(new) = &want {
                    let _ = w.watch(new, RecursiveMode::NonRecursive);
                }
                self.watched[i] = want;
            }
        }
    }

    // ------------------------------------------------------------ events

    pub fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Term(Event::Key(k)) if k.kind != KeyEventKind::Release => self.on_key(k),
            AppEvent::Term(Event::Mouse(m)) => self.on_mouse(m),
            AppEvent::Term(Event::Paste(s)) => self.on_paste(&s),
            AppEvent::Term(Event::Resize(..)) => self.previewer.invalidate(),
            AppEvent::Term(_) => {}
            AppEvent::Job(m) => self.on_job(m),
            AppEvent::Preview(m) => {
                if m.target() == self.previewer.id() {
                    self.previewer.on_msg(m);
                } else {
                    // Results for a screen's own previewer (fuzzy finder).
                    for s in self.screens.iter_mut().rev() {
                        if let Some(fz) = s.as_any_mut().downcast_mut::<crate::fuzzy::FuzzyScreen>() {
                            fz.on_preview(m);
                            break;
                        }
                    }
                }
            }
            AppEvent::FsChanged(dir) => {
                for i in 0..2 {
                    if self.panels[i].loc.local_path() == Some(dir.as_path()) {
                        self.dirty[i] = Some(Instant::now());
                    }
                }
            }
            AppEvent::Call(f) => f(self),
        }
    }

    /// Turn what a finished job changed into undo journal entries.
    fn record_job_undo(&mut self, summary: &str, ops: Vec<crate::undo::UndoOp>) {
        use crate::undo::UndoOp;
        if ops.is_empty() {
            return;
        }
        let label = summary.split(':').next().unwrap_or(summary).to_string();
        // Group copies per file system (a sync job copies in both directions).
        let mut copies: Vec<(crate::vfs::VfsHandle, Vec<std::path::PathBuf>)> = Vec::new();
        let mut rest = Vec::new();
        for op in ops {
            match op {
                UndoOp::Copy { vfs, paths } => match copies.iter_mut().find(|(v, _)| v.same(&vfs)) {
                    Some((_, list)) => list.extend(paths),
                    None => copies.push((vfs, paths)),
                },
                other => rest.push(other),
            }
        }
        for (vfs, paths) in copies {
            self.undo.push(label.clone(), UndoOp::Copy { vfs, paths });
        }
        match rest.len() {
            0 => {}
            1 => self.undo.push(label, rest.pop().expect("one op")),
            _ => self.undo.push(label, UndoOp::Batch(rest)),
        }
    }

    /// Apply finished background directory listings.
    pub fn poll_loads(&mut self) {
        let mut changed = false;
        for i in 0..2 {
            match self.panels[i].poll_load() {
                Some(Ok(())) => changed = true,
                Some(Err(e)) => {
                    changed = true;
                    self.flash_error(format!("Cannot read directory: {e:#}"));
                }
                None => {}
            }
        }
        if changed {
            self.previewer.invalidate();
            self.setup_watcher();
        }
    }

    pub fn tick(&mut self) {
        crate::term::tick(self);
        crate::vfs::dmg::gc(self);
        self.poll_loads();
        self.record_visits();
        self.previewer.tick();
        let due: Vec<Arc<JobProgress>> = self
            .pending_progress
            .iter()
            .filter(|(j, t)| t.elapsed() >= Duration::from_millis(300) && !j.finished.load(Relaxed))
            .map(|(j, _)| j.clone())
            .collect();
        self.pending_progress.retain(|(j, t)| t.elapsed() < Duration::from_millis(300) && !j.finished.load(Relaxed));
        for j in due {
            // Below any dialog already open (e.g. a conflict question raised by this job).
            self.dialogs.insert(0, Box::new(crate::dialog::progress::ProgressDialog::new(j)));
        }
        // Debounced auto refresh.
        for i in 0..2 {
            if let Some(t) = self.dirty[i]
                && t.elapsed() > Duration::from_millis(250)
            {
                self.dirty[i] = None;
                self.panels[i].reload();
                self.previewer.invalidate();
                self.refresh_git();
            }
        }
        if let Some((_, t, err)) = &self.message
            && t.elapsed() > Duration::from_secs(if *err { 6 } else { 3 })
        {
            self.message = None;
        }
        if let Some(mut d) = self.dialogs.pop() {
            let o = d.tick();
            self.apply_outcome(d, o);
        } else if let Some(mut s) = self.screens.pop() {
            let o = s.tick();
            self.apply_screen_outcome(s, o);
        }
        self.jobs.retain(|j| !j.finished.load(Relaxed));
    }

    fn apply_outcome(&mut self, d: Box<dyn Dialog>, o: Outcome) {
        match o {
            Outcome::Keep => self.dialogs.push(d),
            Outcome::Close => {}
            Outcome::CloseThen(f) => f(self),
            Outcome::KeepThen(f) => {
                self.dialogs.push(d);
                f(self);
            }
        }
    }

    fn apply_screen_outcome(&mut self, s: Box<dyn Screen>, o: ScreenOutcome) {
        match o {
            ScreenOutcome::Keep => self.screens.push(s),
            ScreenOutcome::Close => {
                self.reload_panels();
            }
            ScreenOutcome::CloseThen(f) => {
                f(self);
                self.reload_panels();
            }
            ScreenOutcome::Then(f) => {
                self.screens.push(s);
                f(self);
            }
        }
    }

    fn on_paste(&mut self, s: &str) {
        if let Some(d) = self.dialogs.last_mut() {
            d.paste(s);
        } else if let Some(sc) = self.screens.last_mut() {
            sc.paste(s);
        } else if !crate::term::paste(self, s) {
            self.cmdline.insert_str(s);
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        if let Some(mut d) = self.dialogs.pop() {
            let o = d.key(k);
            self.apply_outcome(d, o);
            return;
        }
        if let Some(mut s) = self.screens.pop() {
            let o = s.key(k);
            self.apply_screen_outcome(s, o);
            return;
        }
        // The focused terminal pane gets every key (except its escape key).
        if crate::term::key(self, k) {
            return;
        }
        self.on_panel_key(k);
    }

    fn on_panel_key(&mut self, k: KeyEvent) {
        let k = if self.cfg.macos_option_as_alt { option_as_alt(k) } else { k };
        let key = Key::from_event(&k);
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);

        // Esc while a directory is still loading cancels it.
        if k.code == KeyCode::Esc && self.keymap.pending.is_empty() && self.panel_mut().cancel_load() {
            self.info("Directory listing cancelled");
            return;
        }
        // Alt-1..9 jumps to a tab of the active panel.
        if alt
            && !ctrl
            && self.keymap.pending.is_empty()
            && let KeyCode::Char(c @ '1'..='9') = k.code
        {
            let side = self.active;
            crate::tabs::switch_to(self, side, c as usize - '1' as usize);
            return;
        }

        // Quick search mode captures typing.
        if self.panel().quick_search.is_some() && self.keymap.pending.is_empty() {
            match k.code {
                KeyCode::Char(c) if !ctrl && !alt => {
                    let mut q = self.panel().quick_search.clone().unwrap_or_default();
                    q.push(c);
                    if self.panel_mut().quick_search_jump(&q, false) {
                        self.panel_mut().quick_search = Some(q);
                    }
                    return;
                }
                KeyCode::Backspace => {
                    let p = self.panel_mut();
                    if let Some(q) = p.quick_search.as_mut() {
                        q.pop();
                        let q = q.clone();
                        p.quick_search_jump(&q, false);
                    }
                    return;
                }
                KeyCode::Char('s') if ctrl || alt => {
                    let q = self.panel().quick_search.clone().unwrap_or_default();
                    self.panel_mut().quick_search_jump(&q, true);
                    return;
                }
                KeyCode::Esc => {
                    self.panel_mut().quick_search = None;
                    return;
                }
                KeyCode::Enter => {
                    self.panel_mut().quick_search = None;
                    // fall through to normal Enter handling
                }
                _ => self.panel_mut().quick_search = None,
            }
        }

        // Tree view navigation.
        if self.panel().view == PanelView::Tree && self.keymap.pending.is_empty() && self.tree_key(&k) {
            return;
        }
        // Zoom / pan an image in the quick view when it is the active panel.
        if self.panel().view == PanelView::QuickView && self.keymap.pending.is_empty() && self.previewer.is_image() {
            let zoomed = self.previewer.zoom() > 1.0;
            let handled = match k.code {
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    self.previewer.zoom_in();
                    true
                }
                KeyCode::Char('-') | KeyCode::Char('_') => {
                    self.previewer.zoom_out();
                    true
                }
                KeyCode::Char('0') | KeyCode::Home => {
                    self.previewer.zoom_reset();
                    true
                }
                KeyCode::Left | KeyCode::Char('h') if zoomed => {
                    self.previewer.pan(-0.2, 0.0);
                    true
                }
                KeyCode::Right | KeyCode::Char('l') if zoomed => {
                    self.previewer.pan(0.2, 0.0);
                    true
                }
                KeyCode::Up | KeyCode::Char('k') if zoomed => {
                    self.previewer.pan(0.0, -0.2);
                    true
                }
                KeyCode::Down | KeyCode::Char('j') if zoomed => {
                    self.previewer.pan(0.0, 0.2);
                    true
                }
                _ => false,
            };
            if handled {
                return;
            }
        }
        // `d` toggles the git diff view of the quick view when it is the active panel.
        if self.panel().view == PanelView::QuickView
            && self.keymap.pending.is_empty()
            && k.code == KeyCode::Char('d')
            && k.modifiers.is_empty()
        {
            self.previewer.toggle_git_diff();
            return;
        }
        // JSON tree, horizontal scroll, animation pause in the focused quick view.
        if self.panel().view == PanelView::QuickView && self.keymap.pending.is_empty() && self.previewer.key(&k) {
            return;
        }
        // Scroll the quick view when it is the active panel.
        if self.panel().view == PanelView::QuickView && self.keymap.pending.is_empty() {
            let d = match k.code {
                KeyCode::Up => Some(-1),
                KeyCode::Down => Some(1),
                KeyCode::PageUp => Some(-20),
                KeyCode::PageDown => Some(20),
                KeyCode::Home => Some(isize::MIN / 2),
                _ => None,
            };
            if let Some(d) = d {
                self.previewer.scroll(d);
                return;
            }
        }

        // Command line editing when it has content.
        let has_cmd = !self.cmdline.is_empty();
        if self.keymap.pending.is_empty() {
            match k.code {
                KeyCode::Enter if has_cmd && !alt && !ctrl => {
                    let cmd = self.cmdline.text();
                    self.cmdline.clear();
                    self.run_cmdline(&cmd);
                    return;
                }
                KeyCode::Esc if has_cmd => {
                    self.cmdline.clear();
                    return;
                }
                KeyCode::Backspace
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Delete
                    if has_cmd && !alt =>
                {
                    self.cmdline.handle_key(&k);
                    return;
                }
                KeyCode::Char('a' | 'e' | 'k' | 'w' | 'b' | 'f') if has_cmd && ctrl => {
                    self.cmdline.handle_key(&k);
                    return;
                }
                KeyCode::Char('p') if alt => {
                    self.cmdline.handle_key(&k);
                    return;
                }
                KeyCode::Char('n') if alt => {
                    self.cmdline.handle_key(&k);
                    return;
                }
                KeyCode::Tab if has_cmd => {
                    self.complete_cmdline();
                    return;
                }
                // While a command is being typed, plain characters belong to it, even
                // ones bound to panel commands such as `+ - *` (select / unselect /
                // invert). Like Midnight Commander, those act on files only when the
                // command line is empty, so `ls -la` and `cp a+b` can be typed.
                KeyCode::Char(c) if has_cmd && !alt && !ctrl => {
                    self.cmdline.insert_str(&c.to_string());
                    return;
                }
                _ => {}
            }
        }

        match self.keymap.feed(key) {
            KeyResult::Command(c) => {
                self.exec(c);
                return;
            }
            KeyResult::Pending => {
                self.info(format!("{} -", crate::keymap::chord_label(&self.keymap.pending)));
                return;
            }
            KeyResult::Unbound(keys) => {
                if keys.len() > 1 {
                    self.flash_error(format!("Unbound key: {}", crate::keymap::chord_label(&keys)));
                    return;
                }
            }
        }
        if self.message.as_ref().is_some_and(|(m, _, _)| m.ends_with(" -")) {
            self.message = None;
        }

        // Built-in navigation.
        let lynx = self.cfg.lynx_motion;
        let brief = self.panel().mode == crate::panel::ListMode::Brief;
        let page = self.panel().page();
        let rows = self.panel().page_rows.max(1) as isize;
        match k.code {
            KeyCode::Up => self.panel_mut().move_by(-1),
            KeyCode::Down => self.panel_mut().move_by(1),
            KeyCode::Char('p') if ctrl => self.panel_mut().move_by(-1),
            KeyCode::Char('n') if ctrl => self.panel_mut().move_by(1),
            KeyCode::PageUp => self.panel_mut().move_by(-(page as isize)),
            KeyCode::PageDown => self.panel_mut().move_by(page as isize),
            KeyCode::Home => self.panel_mut().cursor = 0,
            KeyCode::End => {
                let n = self.panel().entries.len();
                self.panel_mut().cursor = n.saturating_sub(1);
            }
            KeyCode::Left if brief && !lynx => self.panel_mut().move_by(-rows),
            KeyCode::Right if brief && !lynx => self.panel_mut().move_by(rows),
            KeyCode::Left if lynx => self.go_up(),
            KeyCode::Right if lynx => {
                if self.panel().current().is_some_and(|e| e.is_dir()) {
                    self.enter();
                }
            }
            KeyCode::Enter => self.enter(),
            KeyCode::Backspace if !has_cmd => self.go_up(),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.cmdline.insert_str(&c.to_string());
            }
            KeyCode::Char(c) if alt && !ctrl && c.is_alphanumeric() => {
                // Alt-letter: start quick search with that letter.
                let q = c.to_string();
                if self.panel_mut().quick_search_jump(&q, false) {
                    self.panel_mut().quick_search = Some(q);
                }
            }
            _ => {}
        }
    }

    fn tree_key(&mut self, k: &KeyEvent) -> bool {
        let i = self.active;
        let show_hidden = self.panels[i].show_hidden;
        let start = self.panels[i].loc.path.clone();
        let tree = self.panels[i].tree.get_or_insert_with(|| crate::tree::TreeState::new(&start, show_hidden));
        match k.code {
            KeyCode::Up => tree.move_by(-1),
            KeyCode::Down => tree.move_by(1),
            KeyCode::PageUp => tree.move_by(-20),
            KeyCode::PageDown => tree.move_by(20),
            KeyCode::Home => tree.cursor = 0,
            KeyCode::End => tree.cursor = tree.nodes.len().saturating_sub(1),
            KeyCode::Left => tree.left(),
            KeyCode::Right => tree.right(),
            KeyCode::Char(' ') => tree.toggle(),
            KeyCode::Enter => {
                if let Some(p) = tree.selected().map(Path::to_path_buf) {
                    self.panels[i].view = PanelView::List;
                    let loc = Location::local(p);
                    self.chdir_active(loc);
                }
                return true;
            }
            _ => return false,
        }
        // Other panel follows the tree cursor.
        if let Some(p) = self.panels[i].tree.as_ref().and_then(|t| t.selected()).map(Path::to_path_buf) {
            let o = 1 - i;
            if self.panels[o].view == PanelView::List && self.panels[o].is_local() && self.panels[o].loc.path != p {
                let _ = self.panels[o].chdir(Location::local(p), None);
            }
        }
        true
    }

    // ------------------------------------------------------------ mouse

    fn on_mouse(&mut self, m: MouseEvent) {
        if !self.cfg.mouse {
            return;
        }
        if let Some(mut d) = self.dialogs.pop() {
            let o = d.mouse(m);
            self.apply_outcome(d, o);
            return;
        }
        if let MouseEventKind::Down(MouseButton::Left) = m.kind
            && let Some(h) = crate::ui::hit_at(m.column, m.row)
        {
            match h {
                crate::ui::Hit::Key(code) => self.on_key(KeyEvent::new(code, KeyModifiers::NONE)),
                crate::ui::Hit::Cmd(c) => self.exec(c),
                crate::ui::Hit::Tab(side, k) => crate::tabs::switch_to(self, side, k),
                crate::ui::Hit::PanelList(i) => {
                    if self.panels[i].view == PanelView::Terminal {
                        crate::term::undock(self);
                    }
                    self.panels[i].view = PanelView::List;
                    self.previewer.invalidate();
                }
            }
            return;
        }
        if let Some(mut s) = self.screens.pop() {
            let o = s.mouse(m);
            self.apply_screen_outcome(s, o);
            return;
        }
        if crate::term::mouse(self, m) {
            return;
        }
        let pos = (m.column, m.row);
        // Image zoom / pan in the quick view pane.
        for i in 0..2 {
            let over = self.layout.lists[i].contains(pos.into());
            let dragging = matches!(m.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_));
            if self.panels[i].view == PanelView::QuickView && (over || dragging) && self.previewer.mouse(m) {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    self.active = i;
                }
                return;
            }
        }
        match m.kind {
            MouseEventKind::Down(btn) => {
                if self.layout.menubar.contains(pos.into()) {
                    let idx = self
                        .layout
                        .menu_titles
                        .iter()
                        .find(|(r, _)| r.contains(pos.into()))
                        .map(|(_, i)| *i)
                        .unwrap_or(0);
                    crate::dialog::menu::open(self, idx);
                    return;
                }
                if let Some((_, n)) = self.layout.buttons.iter().find(|(r, _)| r.contains(pos.into())).cloned() {
                    self.on_key(KeyEvent::new(KeyCode::F(n), KeyModifiers::NONE));
                    return;
                }
                for i in 0..2 {
                    if !self.layout.panels[i].contains(pos.into()) {
                        continue;
                    }
                    self.active = i;
                    let list = self.layout.lists[i];
                    if !list.contains(pos.into()) || self.panels[i].view != PanelView::List {
                        return;
                    }
                    let p = &self.panels[i];
                    let cols = p.columns.max(1);
                    let col_w = (list.width / cols as u16).max(1);
                    let col = ((m.column - list.x) / col_w) as usize;
                    let row = (m.row - list.y) as usize;
                    let idx = p.offset + col * p.page_rows + row;
                    if idx >= p.entries.len() {
                        return;
                    }
                    let double = self.last_click.is_some_and(|(t, x, y)| {
                        t.elapsed() < Duration::from_millis(400) && x == m.column && y == m.row
                    });
                    self.panels[i].cursor = idx;
                    if btn == MouseButton::Right {
                        self.panels[i].toggle_mark_current();
                    } else if btn == MouseButton::Left && double {
                        self.last_click = None;
                        self.enter();
                        return;
                    }
                    self.last_click = Some((Instant::now(), m.column, m.row));
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let d = if matches!(m.kind, MouseEventKind::ScrollUp) { -3 } else { 3 };
                for i in 0..2 {
                    if self.layout.panels[i].contains(pos.into()) {
                        match self.panels[i].view {
                            PanelView::QuickView => self.previewer.scroll(d),
                            PanelView::Tree => {
                                if let Some(t) = self.panels[i].tree.as_mut() {
                                    t.move_by(d)
                                }
                            }
                            _ => self.panels[i].move_by(d),
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ jobs

    pub fn start_job(&mut self, spec: ops::JobSpec, foreground: bool) {
        let p = ops::spawn(spec, self.tx.clone());
        self.track_job(p, foreground);
    }

    /// Track a job started elsewhere (its worker sends `JobMsg::Finished`); a
    /// foreground job gets a progress dialog if it runs longer than a moment.
    pub fn track_job(&mut self, p: Arc<JobProgress>, foreground: bool) {
        if foreground {
            self.fg_jobs.push(p.id);
            // Show the progress dialog only if the job is still running shortly after.
            self.pending_progress.push((p.clone(), Instant::now()));
        }
        self.jobs.push(p);
    }

    fn on_job(&mut self, m: JobMsg) {
        match m {
            JobMsg::Conflict { src, dst, dst_path, reply, .. } => {
                let text = format!(
                    "Target file already exists:\n{}\n\nNew:      {:>12}  {}\nExisting: {:>12}  {}",
                    crate::util::trunc_left(&dst_path, 60),
                    crate::util::grouped(src.size),
                    crate::util::full_time(src.mtime),
                    crate::util::grouped(dst.size),
                    crate::util::full_time(dst.mtime),
                );
                let r2 = reply.clone();
                let d = crate::dialog::Form::new("File exists")
                    .text(&text)
                    .buttons(&["Yes", "No", "All", "None", "Older", "Rename", "Abort"], 0)
                    .on_submit(move |_, r| {
                        let c = match r.button {
                            0 => ConflictChoice::Overwrite,
                            1 => ConflictChoice::Skip,
                            2 => ConflictChoice::OverwriteAll,
                            3 => ConflictChoice::SkipAll,
                            4 => ConflictChoice::OlderAll,
                            5 => ConflictChoice::Rename,
                            _ => ConflictChoice::Abort,
                        };
                        let _ = reply.send(c);
                    })
                    .on_cancel(move |_| {
                        let _ = r2.send(ConflictChoice::Abort);
                    });
                self.push_dialog(Box::new(d));
            }
            JobMsg::Error { msg, reply, .. } => {
                let r2 = reply.clone();
                let d = crate::dialog::Form::new("Error")
                    .text(&msg)
                    .buttons(&["Skip", "Skip all", "Retry", "Abort"], 0)
                    .error()
                    .on_submit(move |_, r| {
                        let c = match r.button {
                            0 => ErrorChoice::Skip,
                            1 => ErrorChoice::SkipAll,
                            2 => ErrorChoice::Retry,
                            _ => ErrorChoice::Abort,
                        };
                        let _ = reply.send(c);
                    })
                    .on_cancel(move |_| {
                        let _ = r2.send(ErrorChoice::Abort);
                    });
                self.push_dialog(Box::new(d));
            }
            JobMsg::Finished { id, summary, cancelled, errors, undo } => {
                self.fg_jobs.retain(|j| *j != id);
                self.record_job_undo(&summary, undo);
                self.reload_panels();
                if !errors.is_empty() {
                    let mut text = errors.iter().take(15).cloned().collect::<Vec<_>>().join("\n");
                    if errors.len() > 15 {
                        text.push_str(&format!("\n... and {} more", errors.len() - 15));
                    }
                    self.push_dialog(dialog::error(&text));
                } else if cancelled {
                    self.flash_error(format!("Aborted. {summary}"));
                } else {
                    self.info(summary);
                }
            }
            JobMsg::DirSize { dir, name, size, .. } => {
                for p in &mut self.panels {
                    if p.loc.path == dir {
                        p.dir_sizes.insert(name.clone(), size);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ actions

    pub fn go_up(&mut self) {
        if let Err(e) = self.panel_mut().go_up() {
            self.error(format!("{e:#}"));
        }
        self.setup_watcher();
    }

    /// Enter: directories, archives, executables, associations.
    pub fn enter(&mut self) {
        let Some(e) = self.panel().current().cloned() else { return };
        // Disk images attach on a background thread.
        if !e.is_dir() && crate::vfs::dmg::is_dmg_name(&e.name) && crate::vfs::dmg::supported() {
            let side = self.active;
            let dir = self.panel().loc.clone();
            crate::vfs::dmg::start_attach(self, side, dir, e.name.clone(), None);
            return;
        }
        match self.panel_mut().enter() {
            Ok(true) => {
                let d = self.panel().loc.display();
                State::push_history(&mut self.state.dir_history, &d);
                self.setup_watcher();
                return;
            }
            Ok(false) => {}
            Err(err) => {
                self.error(format!("{err:#}"));
                return;
            }
        }
        // Encrypted archive entries: unlock first, then open.
        let loc = self.panel().loc.clone();
        let names = [e.name.clone()];
        crate::vfs::secrets::ensure_password(self, &loc, &names, move |app| app.open_file_entry(&e));
    }

    /// Open a regular file: associations, executables, else the viewer.
    fn open_file_entry(&mut self, e: &crate::vfs::Entry) {
        if crate::assoc::open_entry(self, e) {
            return;
        }
        if e.is_exec() && self.panel().is_local() {
            let cmd = format!("./{}", crate::util::shell_quote(&e.name));
            self.run_cmdline(&cmd);
            return;
        }
        self.exec(Command::View);
    }

    /// Execute a command typed on the command line.
    pub fn run_cmdline(&mut self, cmd: &str) {
        let trimmed = cmd.trim();
        if trimmed.is_empty() {
            return;
        }
        State::push_history(&mut self.state.cmd_history, trimmed);
        self.cmdline = LineEdit::new("").with_history(self.state.cmd_history.clone());
        // zoxide-style jumps: `z terms`, `zi`.
        if trimmed == "zi" {
            crate::fuzzy::open_dirs(self);
            return;
        }
        if trimmed == "z" || trimmed.starts_with("z ") {
            let terms: Vec<&str> = trimmed[1..].split_whitespace().collect();
            if terms.is_empty() {
                self.chdir_active(Location::local(crate::util::home()));
                return;
            }
            let here = self.panel().loc.local_path().map(Path::to_path_buf);
            let now = crate::frecency::now();
            match crate::frecency::best_match(&mut self.state.frecent, &terms, here.as_deref(), now) {
                Some(dir) => self.chdir_active(Location::local(dir)),
                None => self.flash_error(format!("z: no match for '{}'", terms.join(" "))),
            }
            return;
        }
        // Built-in cd.
        if trimmed == "cd" || trimmed.starts_with("cd ") {
            let arg = trimmed[2..].trim();
            let arg = if arg.is_empty() { "~" } else { arg };
            let arg = arg.trim_matches(|c| c == '"' || c == '\'');
            if arg == "-" {
                let _ = self.panel_mut().history_back();
                return;
            }
            match crate::vfs::parse_location(arg, &self.panel().loc.clone()) {
                Ok(loc) => {
                    if loc.vfs.same(&self.panel().loc.vfs) {
                        self.chdir_active(loc);
                    } else if let Err(e) = self.panel_mut().enter_vfs(loc) {
                        self.error(format!("{e:#}"));
                    }
                }
                Err(e) => self.error(format!("{e:#}")),
            }
            return;
        }
        if trimmed == "exit" {
            self.exec(Command::Quit);
            return;
        }
        let cwd = self.active_local_dir();
        self.external = Some(External::Command { cmd: trimmed.to_string(), cwd, pause: true });
    }

    /// Tab completion for the command line: files in the active dir.
    fn complete_cmdline(&mut self) {
        let text = self.cmdline.text();
        let word_start = text.rfind(' ').map(|i| i + 1).unwrap_or(0);
        let word = &text[word_start..];
        let (dir_part, prefix) = match word.rfind('/') {
            Some(i) => (&word[..=i], &word[i + 1..]),
            None => ("", word),
        };
        let base = self.active_local_dir();
        let dir = if dir_part.is_empty() {
            base
        } else {
            let exp = crate::util::expand_tilde(dir_part);
            let p = Path::new(&exp);
            if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
        };
        let mut matches: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .map(|d| {
                        let n = d.file_name().to_string_lossy().into_owned();
                        if d.path().is_dir() { format!("{n}/") } else { n }
                    })
                    .filter(|n| n.starts_with(prefix) && (!n.starts_with('.') || prefix.starts_with('.')))
                    .collect()
            })
            .unwrap_or_default();
        // Also complete commands from PATH for the first word.
        if word_start == 0
            && dir_part.is_empty()
            && !prefix.is_empty()
            && let Ok(path) = std::env::var("PATH")
        {
            for d in path.split(':') {
                if let Ok(rd) = std::fs::read_dir(d) {
                    for e in rd.flatten() {
                        let n = e.file_name().to_string_lossy().into_owned();
                        if n.starts_with(prefix) {
                            matches.push(n);
                        }
                    }
                }
            }
        }
        matches.sort();
        matches.dedup();
        if matches.is_empty() {
            return;
        }
        // Longest common prefix.
        let mut common = matches[0].clone();
        for m in &matches[1..] {
            while !m.starts_with(&common) {
                common.pop();
            }
        }
        if common.len() > prefix.len() {
            let mut new = text[..word_start].to_string();
            new.push_str(dir_part);
            new.push_str(&crate::util::shell_quote(&common).replace("'/", "/'"));
            if matches.len() == 1 && !common.ends_with('/') {
                new.push(' ');
            }
            self.cmdline.set(&new);
        } else if matches.len() > 1 {
            let shown: Vec<String> = matches.iter().take(12).cloned().collect();
            self.info(shown.join("  "));
        }
    }

    /// Record directory visits for frecency (any way a panel changed directory).
    fn record_visits(&mut self) {
        let now = crate::frecency::now();
        for i in 0..2 {
            let p = &self.panels[i];
            let Some(dir) = p.loc.local_path().filter(|_| p.panelized.is_none()).map(Path::to_path_buf) else {
                continue;
            };
            if self.last_visit[i].as_ref() != Some(&dir) {
                crate::frecency::record(&mut self.state.frecent, &dir.display().to_string(), now);
                self.last_visit[i] = Some(dir);
            }
        }
    }

    pub fn save_state(&mut self) {
        self.state.left_dir = self.panels[0].loc.local_path().map(|p| p.display().to_string());
        self.state.right_dir = self.panels[1].loc.local_path().map(|p| p.display().to_string());
        self.state.quick_view_side = self.panels.iter().position(|p| p.view == PanelView::QuickView);
        self.state.tabs = crate::tabs::save(self);
        self.state.save();
    }
}

/// macOS terminals send Option+key as a special character unless configured otherwise
/// (Ghostty: `macos-option-as-alt`). Map the US-layout characters back to Alt+key.
fn option_as_alt(k: KeyEvent) -> KeyEvent {
    if !cfg!(target_os = "macos") || !k.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return k;
    }
    let KeyCode::Char(c) = k.code else { return k };
    let mapped = match c {
        '\u{0153}' => 'q',
        '\u{03c0}' => 'p',
        '\u{00e7}' => 'c',
        '\u{02d9}' => 'h',
        '\u{00d3}' => 'H',
        '\u{00a5}' => 'y',
        '\u{00f8}' => 'o',
        '\u{2020}' => 't',
        '\u{00df}' => 's',
        '\u{2265}' => '.',
        '\u{00bf}' => '?',
        '\u{222b}' => 'b',
        '\u{0192}' => 'f',
        '\u{2202}' => 'd',
        '\u{2264}' => ',',
        '\u{00ae}' => 'r',
        '\u{2248}' => 'x',
        '\u{00e5}' => 'a',
        '\u{00a9}' => 'g',
        '\u{2206}' => 'j',
        '\u{02da}' => 'k',
        '\u{00ac}' => 'l',
        '\u{00b5}' => 'm',
        '\u{221a}' => 'v',
        '\u{2211}' => 'w',
        '\u{03a9}' => 'z',
        '\u{00a8}' => 'u',
        '\u{02c6}' => 'i',
        // Option-=, Option-Shift-=, Option--, Option-0 (preview zoom)
        '\u{2260}' => '=',
        '\u{00b1}' => '+',
        '\u{2013}' => '-',
        '\u{00ba}' => '0',
        // Option-1..9 (tab switching)
        '\u{00a1}' => '1',
        '\u{2122}' => '2',
        '\u{00a3}' => '3',
        '\u{00a2}' => '4',
        '\u{221e}' => '5',
        '\u{00a7}' => '6',
        '\u{00b6}' => '7',
        '\u{2022}' => '8',
        '\u{00aa}' => '9',
        _ => return k,
    };
    let mut mods = KeyModifiers::ALT;
    if mapped.is_ascii_uppercase() {
        mods |= KeyModifiers::SHIFT;
    }
    KeyEvent::new(KeyCode::Char(mapped.to_ascii_lowercase()), mods)
}

/// Apply a dialog closure later from any thread-safe context.
pub fn call(tx: &Sender<AppEvent>, f: impl FnOnce(&mut App) + Send + 'static) {
    let _ = tx.send(AppEvent::Call(Box::new(f)));
}
