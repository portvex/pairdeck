//! Embedded terminal pane: a real shell on a PTY (portable-pty), emulated with
//! vt100 and drawn in a split below the panels.
//!
//! Keys while the pane is focused go to the shell, except:
//!   Alt-Z            give focus back to the panels (the shell keeps running)
//!   Shift-PgUp/PgDn  scroll back through the output
//! From the panels: Alt-Z shows / focuses the pane, Ctrl-X Z maximizes it,
//! Ctrl-X + / Ctrl-X - resize it, Ctrl-X K hides it.

pub mod keys;

use crate::app::{App, AppEvent};
use crate::panel::PanelView;
use crate::ui::Ctx;
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, BorderType, Borders},
};
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};

/// Lines of scrollback kept by the emulator.
const SCROLLBACK: usize = 5000;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Emulator callbacks: remember the window title programs set (OSC 0/2).
#[derive(Default)]
struct Callbacks {
    title: String,
}

impl vt100::Callbacks for Callbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }
}

type Parser = vt100::Parser<Callbacks>;

/// What to run in the pane.
pub struct Spawn {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub rows: u16,
    pub cols: u16,
}

/// A running program on a PTY plus its emulated screen.
pub struct Session {
    pub id: u64,
    parser: Arc<Mutex<Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
    exited: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap_or(NonZeroU16::MIN)
}

impl Session {
    /// Start `spec` on a new PTY. `wake` is called (at most once per drawn frame)
    /// when new output arrives; `on_exit` runs on the reader thread at EOF.
    pub fn spawn(
        spec: Spawn,
        wake: Arc<dyn Fn() + Send + Sync>,
        on_exit: Box<dyn FnOnce(u64) + Send>,
    ) -> Result<Session> {
        let (rows, cols) = (spec.rows.max(2), spec.cols.max(2));
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("cannot open a pseudo terminal")?;
        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("PAIRDECK_SUBSHELL", "1");
        let child = pair.slave.spawn_command(cmd).with_context(|| format!("cannot start {}", spec.program))?;
        // The child owns the slave side now; keeping it open would hide EOF.
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().context("cannot read from the terminal")?;
        let writer = pair.master.take_writer().context("cannot write to the terminal")?;
        let parser =
            Arc::new(Mutex::new(Parser::new_with_callbacks(nz(rows), nz(cols), SCROLLBACK, Callbacks::default())));
        let exited = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(false));
        let id = NEXT_ID.fetch_add(1, Relaxed);
        {
            let (parser, exited, dirty) = (parser.clone(), exited.clone(), dirty.clone());
            std::thread::Builder::new()
                .name("pty-reader".into())
                .spawn(move || {
                    let mut buf = [0u8; 16384];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                parser.lock().unwrap_or_else(|e| e.into_inner()).process(&buf[..n]);
                                // Coalesce: wake the UI only if it has drawn since the last wake.
                                if !dirty.swap(true, Relaxed) {
                                    wake();
                                }
                            }
                        }
                    }
                    exited.store(true, Relaxed);
                    wake();
                    on_exit(id);
                })
                .context("cannot start the terminal reader")?;
        }
        Ok(Session { id, parser, writer, master: pair.master, child, size: (rows, cols), exited, dirty })
    }

    fn parser(&self) -> MutexGuard<'_, Parser> {
        self.parser.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Send a key; returns false if it has no terminal encoding.
    pub fn send_key(&mut self, k: &KeyEvent) -> bool {
        let app_cursor = self.parser().screen().application_cursor();
        match keys::encode(k, app_cursor) {
            Some(b) => {
                self.scroll_to_bottom();
                self.write(&b);
                true
            }
            None => false,
        }
    }

    pub fn paste(&mut self, text: &str) {
        let bracketed = self.parser().screen().bracketed_paste();
        self.scroll_to_bottom();
        self.write(&keys::paste(text, bracketed));
    }

    pub fn has_exited(&self) -> bool {
        self.exited.load(Relaxed)
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Resize the emulator and the PTY (the program gets SIGWINCH).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(2), cols.max(2));
        if (rows, cols) == self.size {
            return;
        }
        self.size = (rows, cols);
        self.parser().screen_mut().set_size(nz(rows), nz(cols));
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    /// Scroll the view through the scrollback (positive = older output).
    pub fn scroll(&mut self, delta: isize) {
        let mut p = self.parser();
        let screen = p.screen_mut();
        let cur = screen.scrollback() as isize;
        screen.set_scrollback((cur + delta).max(0) as usize);
    }

    pub fn scroll_to_bottom(&mut self) {
        self.parser().screen_mut().set_scrollback(0);
    }

    pub fn scrollback(&self) -> usize {
        self.parser().screen().scrollback()
    }

    pub fn alternate_screen(&self) -> bool {
        self.parser().screen().alternate_screen()
    }

    pub fn title(&self) -> String {
        self.parser().callbacks().title.clone()
    }

    /// Plain text of the visible screen (tests, debugging).
    pub fn text(&self) -> String {
        self.parser().screen().contents()
    }

    /// Process id of the program (the shell).
    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// True when the shell itself is in the foreground (no job running in it).
    pub fn shell_idle(&self) -> bool {
        match (self.master.process_group_leader(), self.child.process_id()) {
            (Some(fg), Some(pid)) => fg as i64 == pid as i64,
            _ => false,
        }
    }

    /// Current directory of the shell, best effort.
    pub fn cwd(&self) -> Option<PathBuf> {
        process_cwd(self.child.process_id()?)
    }

    /// Draw the screen into `area`; returns the cursor position to show.
    pub fn render(&self, buf: &mut Buffer, area: Rect, base: Style, focused: bool) -> Option<(u16, u16)> {
        self.dirty.store(false, Relaxed);
        let p = self.parser();
        let screen = p.screen();
        let (rows, cols) = screen.size();
        let (rows, cols) = (rows.get().min(area.height), cols.get().min(area.width));
        for y in 0..area.height {
            for x in 0..area.width {
                let target = &mut buf[(area.x + x, area.y + y)];
                target.reset();
                target.set_style(base);
                if y >= rows || x >= cols {
                    continue;
                }
                let Some(cell) = screen.cell(y, x) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let sym = cell.contents();
                target.set_symbol(if sym.is_empty() { " " } else { sym });
                target.set_style(cell_style(cell, base));
            }
        }
        let (cy, cx) = screen.cursor_position();
        let show = focused && !screen.hide_cursor() && screen.scrollback() == 0 && cy < rows && cx < cols;
        show.then(|| (area.x + cx, area.y + cy))
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.try_wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.has_exited() {
            self.kill();
        }
    }
}

fn convert_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

fn cell_style(cell: &vt100::Cell, base: Style) -> Style {
    let mut st = base;
    if let Some(fg) = convert_color(cell.fgcolor()) {
        st = st.fg(fg);
    }
    if let Some(bg) = convert_color(cell.bgcolor()) {
        st = st.bg(bg);
    }
    let mut m = Modifier::empty();
    if cell.bold() {
        m |= Modifier::BOLD;
    }
    if cell.dim() {
        m |= Modifier::DIM;
    }
    if cell.italic() {
        m |= Modifier::ITALIC;
    }
    if cell.underline() {
        m |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        m |= Modifier::REVERSED;
    }
    st.add_modifier(m)
}

/// Working directory of a process, without unsafe code: /proc on Linux, lsof on macOS.
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    if let Ok(p) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
        return Some(p);
    }
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

// ------------------------------------------------------------------ pane

/// Pane state kept in `App`.
pub struct TermPane {
    pub session: Option<Session>,
    pub visible: bool,
    pub focused: bool,
    pub maximized: bool,
    /// Height as a percentage of the area below the menu bar.
    pub height_pct: u16,
    /// Outer rect of the pane (with border) and the area it was split from, last frame.
    pub rect: Rect,
    pub region: Rect,
    pub dragging: bool,
    /// Last directory sent by "follow panel".
    follow_last: Option<PathBuf>,
    /// When set, the shell is shown inside this panel (0 left, 1 right) instead of
    /// the bottom split. There is one shell session, shared by both placements.
    pub docked: Option<usize>,
}

impl Default for TermPane {
    fn default() -> Self {
        TermPane {
            session: None,
            visible: false,
            focused: false,
            maximized: false,
            height_pct: 35,
            rect: Rect::default(),
            region: Rect::default(),
            dragging: false,
            docked: None,
            follow_last: None,
        }
    }
}

impl TermPane {
    /// True when the pane is on screen.
    pub fn shown(&self) -> bool {
        (self.visible || self.docked.is_some()) && self.session.is_some()
    }
}

/// Program for the pane: `terminal_shell` from the config, else $SHELL.
fn shell_program(app: &App) -> (String, Vec<String>) {
    let shell = app
        .cfg
        .terminal_shell
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/sh".into());
    let mut parts = shell.split_whitespace().map(String::from);
    let program = parts.next().unwrap_or_else(|| "/bin/sh".into());
    (program, parts.collect())
}

/// Start the shell in the active panel's directory.
pub fn open(app: &mut App) -> Result<()> {
    let (program, args) = shell_program(app);
    let cwd = app.active_local_dir();
    // Size until the first draw corrects it.
    let (rows, cols) = (app.term.rect.height.saturating_sub(2).max(10), app.term.rect.width.saturating_sub(2).max(40));
    let tx = app.tx.clone();
    let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let _ = tx.send(AppEvent::Call(Box::new(|_| {})));
    });
    let tx = app.tx.clone();
    let on_exit = Box::new(move |id: u64| crate::app::call(&tx, move |app| on_exit(app, id)));
    let s = Session::spawn(Spawn { program, args, cwd: cwd.clone(), rows, cols }, wake, on_exit)?;
    app.term.session = Some(s);
    app.term.follow_last = Some(cwd);
    Ok(())
}

fn on_exit(app: &mut App, id: u64) {
    if app.term.session.as_ref().is_some_and(|s| s.id == id) {
        app.term.session = None;
        app.term.visible = false;
        app.term.focused = false;
        app.term.maximized = false;
        undock(app);
        app.info("Terminal closed (shell exited)");
    }
}

/// Alt-Z from the panels: show and focus the pane (starting the shell if needed);
/// if it is already focused, give focus back to the panels.
pub fn toggle(app: &mut App) {
    if app.term.session.is_none()
        && let Err(e) = open(app)
    {
        app.error(format!("Cannot start the terminal:\n{e:#}"));
        return;
    }
    if let Some(side) = app.term.docked {
        if app.term.focused {
            app.term.focused = false;
            app.active = 1 - side;
        } else {
            app.term.focused = true;
            app.active = side;
        }
        return;
    }
    let t = &mut app.term;
    if t.visible && t.focused {
        t.focused = false;
    } else {
        t.visible = true;
        t.focused = true;
    }
}

/// Show the shell inside panel `side` (starting it if needed) and focus it. The
/// bottom split is hidden while docked; the session (history, running programs)
/// is the same one.
pub fn dock(app: &mut App, side: usize) {
    if app.term.session.is_none()
        && let Err(e) = open(app)
    {
        app.error(format!("Cannot start the terminal:\n{e:#}"));
        return;
    }
    // Only one panel can hold the shell.
    for p in &mut app.panels {
        if p.view == PanelView::Terminal {
            p.view = PanelView::List;
        }
    }
    app.panels[side].view = PanelView::Terminal;
    let t = &mut app.term;
    t.docked = Some(side);
    t.visible = false;
    t.maximized = false;
    t.focused = true;
    app.active = side;
    app.previewer.invalidate();
}

/// Return the docked panel to its file list; the shell keeps running (Alt-Z
/// shows it in the bottom split again).
pub fn undock(app: &mut App) {
    if let Some(side) = app.term.docked.take() {
        if app.panels[side].view == PanelView::Terminal {
            app.panels[side].view = PanelView::List;
        }
        if app.term.focused {
            app.term.focused = false;
        }
    }
    for p in &mut app.panels {
        if p.view == PanelView::Terminal {
            p.view = PanelView::List;
        }
    }
}

/// Command: dock the shell into the other panel, or undock it if docked.
pub fn toggle_panel(app: &mut App) {
    if app.term.docked.is_some() {
        undock(app);
    } else {
        let side = 1 - app.active;
        dock(app, side);
    }
}

/// Keep focus consistent when the active panel changes (Tab, click): the shell
/// is focused exactly when its panel is the active one.
pub fn sync_focus(app: &mut App) {
    if let Some(side) = app.term.docked {
        app.term.focused = app.active == side && app.term.session.is_some();
    }
}

/// Hide the pane; the shell keeps running and comes back with Alt-Z.
pub fn hide(app: &mut App) {
    undock(app);
    let t = &mut app.term;
    t.visible = false;
    t.focused = false;
    t.maximized = false;
}

pub fn toggle_maximize(app: &mut App) {
    // Maximizing uses the bottom placement.
    if app.term.docked.is_some() {
        undock(app);
        app.term.visible = true;
    }
    if !app.term.shown() {
        toggle(app);
    }
    if app.term.shown() {
        app.term.maximized = !app.term.maximized;
        app.term.focused = true;
    }
}

pub fn resize_by(app: &mut App, delta: i16) {
    let t = &mut app.term;
    t.maximized = false;
    t.height_pct = (t.height_pct as i16 + delta).clamp(15, 85) as u16;
}

/// Type `cd <active panel dir>` into the shell.
pub fn cd_to_panel(app: &mut App) {
    let Some(dir) = app.panel().loc.local_path().map(Path::to_path_buf) else {
        app.flash_error("The active panel is not a local directory");
        return;
    };
    if let Some(s) = app.term.session.as_mut() {
        s.write(format!(" cd -- {}\r", crate::util::shell_quote(&dir.display().to_string())).as_bytes());
        app.term.follow_last = Some(dir);
    } else {
        app.flash_error("No terminal running (Alt-Z)");
    }
}

/// Change the active panel to the shell's current directory.
pub fn panel_to_cwd(app: &mut App) {
    let Some(dir) = app.term.session.as_ref().and_then(Session::cwd) else {
        app.flash_error("Cannot read the terminal's directory");
        return;
    };
    app.term.follow_last = Some(dir.clone());
    app.chdir_active(crate::vfs::Location::local(dir));
}

/// Type the quoted paths of the selection into the shell (no Enter).
pub fn send_selection(app: &mut App) {
    let p = app.panel();
    let paths: Vec<String> =
        p.selection().iter().map(|e| crate::util::shell_quote(&p.loc.join(&e.name).display().to_string())).collect();
    if paths.is_empty() {
        return;
    }
    match app.term.session.as_mut() {
        Some(s) => {
            s.write(format!("{} ", paths.join(" ")).as_bytes());
            app.term.visible = true;
            app.term.focused = true;
        }
        None => app.flash_error("No terminal running (Alt-Z)"),
    }
}

/// Keys while the pane is focused. Returns true if consumed.
pub fn key(app: &mut App, k: KeyEvent) -> bool {
    if !app.term.shown() || !app.term.focused {
        return false;
    }
    let alt_z = matches!(k.code, KeyCode::Char('z' | 'Z')) && k.modifiers.contains(KeyModifiers::ALT)
        // macOS Option-Z without "option as alt" in the terminal.
        || (cfg!(target_os = "macos") && app.cfg.macos_option_as_alt && k.code == KeyCode::Char('\u{03a9}'));
    if alt_z {
        app.term.focused = false;
        // Docked: focus moves to the file panel next to the shell.
        if let Some(side) = app.term.docked {
            app.active = 1 - side;
        }
        return true;
    }
    let half = (app.term.rect.height / 2).max(1) as isize;
    let Some(s) = app.term.session.as_mut() else { return false };
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    match k.code {
        KeyCode::PageUp if shift => s.scroll(half),
        KeyCode::PageDown if shift => s.scroll(-half),
        _ => {
            s.send_key(&k);
        }
    }
    true
}

pub fn paste(app: &mut App, text: &str) -> bool {
    if !app.term.shown() || !app.term.focused {
        return false;
    }
    if let Some(s) = app.term.session.as_mut() {
        s.paste(text);
    }
    true
}

/// Mouse over the pane: focus, divider drag to resize, wheel scrollback.
/// Returns true if consumed; clicks elsewhere move focus back to the panels.
pub fn mouse(app: &mut App, m: MouseEvent) -> bool {
    // A click on the docked shell also makes its panel the active one.
    if let (Some(side), MouseEventKind::Down(_)) = (app.term.docked, m.kind)
        && app.term.session.is_some()
        && app.term.rect.contains((m.column, m.row).into())
    {
        app.active = side;
    }
    let t = &mut app.term;
    if !t.shown() {
        return false;
    }
    let pos = (m.column, m.row).into();
    let inside = t.rect.contains(pos);
    match m.kind {
        MouseEventKind::Down(MouseButton::Left)
            if inside && m.row == t.rect.y && !t.maximized && t.docked.is_none() =>
        {
            t.dragging = true;
            t.focused = true;
            true
        }
        MouseEventKind::Drag(MouseButton::Left) if t.dragging => {
            let r = t.region;
            if r.height > 0 {
                let h = r.bottom().saturating_sub(m.row) as u32 * 100 / r.height as u32;
                t.height_pct = (h as u16).clamp(15, 85);
            }
            true
        }
        MouseEventKind::Up(_) if t.dragging => {
            t.dragging = false;
            true
        }
        MouseEventKind::Down(_) if inside => {
            t.focused = true;
            true
        }
        MouseEventKind::Down(_) => {
            t.focused = false;
            false
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if inside => {
            let up = matches!(m.kind, MouseEventKind::ScrollUp);
            if let Some(s) = t.session.as_mut() {
                if s.alternate_screen() {
                    // Full-screen programs (less, vim) scroll with arrow keys.
                    let k = KeyEvent::new(if up { KeyCode::Up } else { KeyCode::Down }, KeyModifiers::NONE);
                    for _ in 0..3 {
                        s.send_key(&k);
                    }
                } else {
                    s.scroll(if up { 3 } else { -3 });
                }
            }
            true
        }
        _ => false,
    }
}

/// Periodic work: "follow panel" sends `cd` to an idle shell when the active
/// panel's directory changes.
pub fn tick(app: &mut App) {
    if !app.cfg.terminal_follow_panel || !app.term.shown() {
        return;
    }
    let Some(dir) = app.panel().loc.local_path().map(Path::to_path_buf) else { return };
    if app.term.follow_last.as_ref() == Some(&dir) {
        return;
    }
    let idle = app.term.session.as_ref().is_some_and(Session::shell_idle);
    if idle {
        cd_to_panel(app);
    }
}

/// Split the area below the menu bar into (panels, pane). Returns None for the
/// pane when it is hidden.
pub fn split(app: &mut App, area: Rect) -> (Rect, Option<Rect>) {
    app.term.region = area;
    if app.term.docked.is_some() {
        // Drawn by the panel it is docked in; that draw sets `rect`.
        return (area, None);
    }
    if !app.term.shown() || area.height < 6 {
        app.term.rect = Rect::default();
        return (area, None);
    }
    if app.term.maximized {
        app.term.rect = area;
        return (Rect { height: 0, ..area }, Some(area));
    }
    let h = (area.height as u32 * app.term.height_pct as u32 / 100) as u16;
    let h = h.clamp(4, area.height.saturating_sub(5).max(4));
    let pane = Rect { y: area.bottom() - h, height: h, ..area };
    app.term.rect = pane;
    (Rect { height: area.height - h, ..area }, Some(pane))
}

/// Draw the pane (border + screen) and place the cursor when focused.
pub fn draw(f: &mut Frame, app: &mut App, r: Rect, ctx: &Ctx) {
    let focused = app.term.focused && app.dialogs.is_empty();
    let Some(s) = app.term.session.as_mut() else { return };
    let title = s.title();
    let cwd_hint = if title.trim().is_empty() { "Terminal".to_string() } else { title };
    let mut hints = String::from(" Alt-Z panels  Shift-PgUp/Dn scroll ");
    if s.scrollback() > 0 {
        hints = format!(" scrollback -{} ", s.scrollback());
    }
    let border = if focused { ctx.theme.border_active } else { ctx.theme.border };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(if ctx.classic { BorderType::Double } else { BorderType::Rounded })
        .border_style(border)
        .style(ctx.theme.panel)
        .title(Line::styled(
            format!(" {} ", crate::util::trunc_left(&cwd_hint, r.width.saturating_sub(10) as usize)),
            if focused { ctx.theme.cursor } else { ctx.theme.border },
        ))
        .title(Line::styled(hints, ctx.theme.border).right_aligned());
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    s.resize(inner.height, inner.width);
    let base =
        Style::default().fg(ctx.theme.panel.fg.unwrap_or(Color::Reset)).bg(ctx.theme.panel.bg.unwrap_or(Color::Reset));
    if let Some(pos) = s.render(f.buffer_mut(), inner, base, focused) {
        f.set_cursor_position(pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn run(program: &str, args: &[&str], rows: u16, cols: u16) -> Session {
        Session::spawn(
            Spawn {
                program: program.into(),
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd: std::env::temp_dir(),
                rows,
                cols,
            },
            Arc::new(|| {}),
            Box::new(|_| {}),
        )
        .expect("spawn")
    }

    fn wait(s: &Session, what: &str, cond: impl Fn(&Session) -> bool) {
        let start = Instant::now();
        while !cond(s) {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out: {what}\n{}", s.text());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn runs_a_command_and_parses_output() {
        let s = run("/bin/sh", &["-c", "printf 'hello \\033[1;31mred\\033[0m world'"], 5, 40);
        wait(&s, "output", |s| s.text().contains("hello red world"));
        wait(&s, "exit", Session::has_exited);
        // Bold red attributes reach the emulator.
        let p = s.parser();
        let cell = p.screen().cell(0, 6).unwrap();
        assert_eq!(cell.contents(), "r");
        assert!(cell.bold());
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1));
    }

    #[test]
    fn resize_reaches_the_program() {
        let mut s = run("/bin/sh", &["-c", "sleep 0.5; stty size; sleep 0.2"], 5, 40);
        s.resize(12, 50);
        assert_eq!(s.size(), (12, 50));
        wait(&s, "stty size", |s| s.text().contains("12 50"));
    }

    #[test]
    fn input_and_exit() {
        let mut s = run("/bin/sh", &[], 6, 60);
        s.write(b"echo pairdeck-$((40+2))\r");
        wait(&s, "echo", |s| s.text().contains("pairdeck-42"));
        s.write(b"exit\r");
        wait(&s, "exit", Session::has_exited);
    }

    #[test]
    fn exit_callback_fires() {
        let fired = Arc::new(AtomicBool::new(false));
        let f2 = fired.clone();
        let s = Session::spawn(
            Spawn {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), "true".into()],
                cwd: std::env::temp_dir(),
                rows: 4,
                cols: 20,
            },
            Arc::new(|| {}),
            Box::new(move |_| f2.store(true, Relaxed)),
        )
        .unwrap();
        wait(&s, "callback", |_| fired.load(Relaxed));
    }

    #[test]
    fn renders_into_a_buffer() {
        let s = run("/bin/sh", &["-c", "printf 'abc'; sleep 0.3"], 3, 10);
        wait(&s, "output", |s| s.text().contains("abc"));
        let area = Rect::new(0, 0, 10, 3);
        let mut buf = Buffer::empty(area);
        s.render(&mut buf, area, Style::default(), true);
        let line: String = (0..3).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(line, "abc");
    }
}
