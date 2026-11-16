//! Quick view / preview pane.
//!
//! `render` is called every frame while the pane is visible. It computes a key for the
//! entry under the cursor and, when it changes, hands a job to a single background
//! worker. The worker reads the file through the VFS (so archives and SFTP work),
//! decodes / highlights / runs external tools, and sends the finished body back as
//! `AppEvent::Preview`. The UI thread never does I/O or decoding.

use crate::app::AppEvent;
use crate::icons::{self, Category};
use crate::theme::Theme;
use crate::ui::Ctx;
use crate::util::{self, human_size, pad_left, pad_right, str_width, trunc_mid};
use crate::vfs::{self, Entry, EntryKind, Location};
use crossbeam_channel::{Receiver, Sender, unbounded};
use ratatui::{
    Frame,
    layout::{Rect, Size},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use ratatui_image::{Image, Resize, picker::Picker, protocol::Protocol};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_millis(70);
const MAX_HIGHLIGHT_LINES: usize = 3000;
const MAX_TEXT_LINES: usize = 20000;
const MAX_IMAGE_BYTES: usize = 64 << 20;
const MAX_TEMP_COPY: u64 = 512 << 20;
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

pub enum PreviewMsg {
    /// `id` is the [`Previewer::id`] that requested it (several previewers may exist).
    Ready { id: u64, seq: u64, key: String, body: Body },
}

impl PreviewMsg {
    /// Id of the previewer this result belongs to.
    pub fn target(&self) -> u64 {
        let PreviewMsg::Ready { id, .. } = self;
        *id
    }
}

static NEXT_PREVIEWER_ID: AtomicU64 = AtomicU64::new(1);

pub enum Body {
    Lines(Vec<Line<'static>>),
    Image {
        proto: Protocol,
        caption: String,
        extra: Vec<Line<'static>>,
        /// Visible part of the source image as fractions (x, y, w, h) of its size.
        region: (f64, f64, f64, f64),
    },
    Error(String),
    /// JSON: highlighted pretty text plus a collapsible tree (`t` toggles).
    Json {
        lines: Vec<Line<'static>>,
        tree: crate::preview_ext::structured::JsonTree,
        show_tree: bool,
    },
    /// Animated image: encoded frames with their delays.
    Anim {
        frames: Vec<(Protocol, Duration)>,
        caption: String,
        current: usize,
        next_at: Instant,
        paused: bool,
    },
}

struct Job {
    seq: u64,
    key: String,
    loc: Location,
    entry: Entry,
    path: PathBuf,
    cells: (u16, u16),
    theme: Theme,
    max_bytes: usize,
    tab: usize,
    nerd_fonts: bool,
    picker: Option<Picker>,
    view: View,
    /// Same file as the previous job, only the view changed: skip the debounce.
    quick: bool,
    /// Show `git diff HEAD` instead of the content for modified tracked files.
    git_diff: bool,
}

/// Image zoom and pan. `zoom == 1.0` shows the whole image; `cx`/`cy` is the
/// center of the visible region as a fraction of the image size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub zoom: f64,
    pub cx: f64,
    pub cy: f64,
}

impl Default for View {
    fn default() -> Self {
        View { zoom: 1.0, cx: 0.5, cy: 0.5 }
    }
}

pub const MAX_ZOOM: f64 = 32.0;
const ZOOM_STEP: f64 = 1.25;

pub struct Previewer {
    id: u64,
    tx: Sender<AppEvent>,
    jobs: Sender<Job>,
    latest: Arc<AtomicU64>,
    picker: Option<Picker>,
    seq: u64,
    pending_key: Option<String>,
    current_key: Option<String>,
    /// Path (display string) of the entry the current body belongs to.
    body_path: Option<String>,
    body: Option<Body>,
    scroll: usize,
    last_path: Option<String>,
    view: View,
    /// `d` in the focused preview turns the git diff view off for this session.
    git_diff_off: bool,
    /// Screen rect of the image drawn last frame (for mouse zoom / pan).
    image_rect: Rect,
    /// Last mouse position while dragging, and last click (for double-click).
    drag: Option<(u16, u16)>,
    last_click: Option<(Instant, u16, u16)>,
    /// Horizontal scroll for text bodies (wide tables, long lines).
    hscroll: usize,
    /// Body area of the last frame (JSON tree viewport).
    body_area: Rect,
    /// Whether the preview panel had focus last frame (cursor style).
    focused: bool,
}

impl Previewer {
    pub fn new(tx: Sender<AppEvent>, picker: Option<Picker>) -> Self {
        let (jtx, jrx) = unbounded::<Job>();
        let latest = Arc::new(AtomicU64::new(0));
        let wtx = tx.clone();
        let wl = latest.clone();
        let id = NEXT_PREVIEWER_ID.fetch_add(1, Relaxed);
        std::thread::Builder::new().name("preview".into()).spawn(move || worker(jrx, wtx, wl, id)).ok();
        Previewer {
            id,
            tx,
            jobs: jtx,
            latest,
            picker,
            seq: 0,
            pending_key: None,
            current_key: None,
            body_path: None,
            body: None,
            scroll: 0,
            last_path: None,
            view: View::default(),
            git_diff_off: false,
            image_rect: Rect::default(),
            drag: None,
            last_click: None,
            hscroll: 0,
            body_area: Rect::default(),
            focused: false,
        }
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, loc: &Location, entry: Option<&Entry>, ctx: &Ctx) {
        // Always wipe the area first so terminal graphics from a previous image vanish.
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new("").style(ctx.theme.panel), area);
        if area.width < 4 || area.height < 3 {
            return;
        }
        let Some(entry) = entry else {
            self.draw_message(f, area, "No file", ctx.theme.hidden, ctx);
            return;
        };
        let path = if entry.is_parent() { loc.path.clone() } else { vfs::normalize(&loc.join(&entry.name)) };
        let display = loc.vfs.lock().display(&path);

        // Header: icon, name, size, time, permissions.
        draw_header(f, Rect { height: 1, ..area }, entry, ctx);
        let sep = "\u{2500}".repeat(area.width as usize);
        f.render_widget(Paragraph::new(Span::styled(sep, ctx.theme.border)), Rect { y: area.y + 1, height: 1, ..area });
        let body_area = Rect { y: area.y + 2, height: area.height - 2, ..area };

        if self.last_path.as_deref() != Some(display.as_str()) {
            self.last_path = Some(display.clone());
            self.scroll = 0;
            self.hscroll = 0;
            self.view = View::default();
        }
        let v = self.view;
        let git_diff = ctx.cfg.preview_git_diff && !self.git_diff_off && loc.is_local();
        let key = format!(
            "{display}|{}|{:?}|{}x{}|{}|{}|{:.4}@{:.4},{:.4}|gd{git_diff}",
            entry.size,
            entry.mtime,
            body_area.width,
            body_area.height,
            ctx.theme.syntax,
            ctx.cfg.nerd_fonts,
            v.zoom,
            v.cx,
            v.cy
        );
        let same_file = self.body_path.as_deref() == Some(display.as_str());
        if self.current_key.as_deref() != Some(key.as_str()) && self.pending_key.as_deref() != Some(key.as_str()) {
            self.seq += 1;
            self.latest.store(self.seq, Relaxed);
            self.pending_key = Some(key.clone());
            let job = Job {
                seq: self.seq,
                key: key.clone(),
                loc: loc.clone(),
                entry: entry.clone(),
                path,
                cells: (body_area.width, body_area.height),
                theme: ctx.theme.clone(),
                max_bytes: ctx.cfg.preview_max_kb.max(4) * 1024,
                tab: ctx.cfg.tab_size.max(1),
                nerd_fonts: ctx.cfg.nerd_fonts,
                picker: self.picker.clone(),
                view: v,
                quick: same_file && matches!(self.body, Some(Body::Image { .. })),
                git_diff,
            };
            let _ = self.jobs.send(job);
        }

        let fresh = self.current_key.as_deref() == Some(key.as_str());
        match (&self.body, fresh, same_file) {
            (Some(Body::Image { .. } | Body::Anim { .. }), false, true) => self.draw_body(f, body_area, ctx, false),
            (Some(Body::Image { .. }), false, _) | (None, false, _) | (Some(_), false, false) => {
                self.draw_message(f, body_area, "Loading...", ctx.theme.hidden, ctx);
            }
            (Some(_), _, _) => self.draw_body(f, body_area, ctx, !fresh),
            (None, true, _) => {}
        }
    }

    fn draw_message(&self, f: &mut Frame, area: Rect, msg: &str, style: Style, ctx: &Ctx) {
        let r = Rect { y: area.y + area.height / 2, height: 1, ..area };
        let st = ctx.theme.panel.patch(style);
        f.render_widget(Paragraph::new(Line::styled(msg.to_string(), st).centered()), r);
    }

    fn draw_body(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx, dim: bool) {
        self.body_area = area;
        let hscroll = self.hscroll.min(u16::MAX as usize) as u16;
        let focused = self.focused;
        let Some(body) = &mut self.body else { return };
        match body {
            Body::Json { tree, show_tree: true, .. } => {
                let lines = tree.render(area.height as usize, area.width as usize, ctx.theme, focused);
                f.render_widget(Paragraph::new(lines).style(ctx.theme.panel), area);
                let ind = format!(" tree {}/{} ", tree.cursor + 1, tree.len());
                let w = str_width(&ind) as u16;
                if area.width > w + 2 {
                    let r = Rect { x: area.right() - w, y: area.bottom() - 1, width: w, height: 1 };
                    f.render_widget(Paragraph::new(Span::styled(ind, ctx.theme.status)), r);
                }
            }
            Body::Anim { frames, caption, current, paused, .. } => {
                let img_h = area.height.saturating_sub(1);
                if let Some((proto, _)) = frames.get(*current) {
                    let sz = proto.size();
                    let w = sz.width.min(area.width);
                    let h = sz.height.min(img_h);
                    let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (img_h - h) / 2, width: w, height: h };
                    if w > 0 && h > 0 {
                        f.render_widget(Image::new(proto).allow_clipping(true), r);
                        crate::kitty_cells::complete_placeholders(f.buffer_mut(), r);
                    }
                }
                let cap = format!(
                    "{caption}  frame {}/{}{}",
                    *current + 1,
                    frames.len(),
                    if *paused { "  paused (Space)" } else { "" }
                );
                let r = Rect { y: area.y + img_h, height: 1, ..area };
                f.render_widget(
                    Paragraph::new(Line::styled(cap, ctx.theme.panel.patch(ctx.theme.hidden)).centered()),
                    r,
                );
            }
            Body::Lines(lines) | Body::Json { lines, show_tree: false, .. } => {
                let h = area.height as usize;
                let max = lines.len().saturating_sub(h);
                self.scroll = self.scroll.min(max);
                let mut st = ctx.theme.panel;
                if dim {
                    st = st.add_modifier(Modifier::DIM);
                }
                let shown: Vec<Line> = lines.iter().skip(self.scroll).take(h).cloned().collect();
                f.render_widget(Paragraph::new(shown).style(st).scroll((0, hscroll)), area);
                if lines.len() > h && area.width > 10 {
                    let ind = format!(" {}/{} ", (self.scroll + h).min(lines.len()), lines.len());
                    let w = str_width(&ind) as u16;
                    let r = Rect { x: area.right().saturating_sub(w), y: area.bottom() - 1, width: w, height: 1 };
                    f.render_widget(Paragraph::new(Span::styled(ind, ctx.theme.status)), r);
                }
            }
            Body::Image { proto, caption, extra, .. } => {
                let extra_h = (extra.len() as u16).min(area.height / 3);
                let img_h = area.height.saturating_sub(1 + extra_h);
                let sz = proto.size();
                let w = sz.width.min(area.width);
                let h = sz.height.min(img_h);
                let r = Rect { x: area.x + (area.width - w) / 2, y: area.y + (img_h - h) / 2, width: w, height: h };
                if w > 0 && h > 0 {
                    f.render_widget(Image::new(proto).allow_clipping(true), r);
                    crate::kitty_cells::complete_placeholders(f.buffer_mut(), r);
                }
                self.image_rect = r;
                let cap = Rect { y: area.y + img_h, height: 1, ..area };
                f.render_widget(
                    Paragraph::new(Line::styled(caption.clone(), ctx.theme.panel.patch(ctx.theme.hidden)).centered()),
                    cap,
                );
                if extra_h > 0 {
                    let er = Rect { y: cap.y + 1, height: extra_h, ..area };
                    f.render_widget(Paragraph::new(extra.clone()).style(ctx.theme.panel), er);
                }
            }
            Body::Error(msg) => {
                let lines: Vec<Line> =
                    msg.lines().map(|l| Line::styled(l.to_string(), ctx.theme.error).centered()).collect();
                let n = lines.len() as u16;
                let r = Rect { y: area.y + area.height.saturating_sub(n) / 2, height: n.min(area.height), ..area };
                f.render_widget(Paragraph::new(lines), r);
            }
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn on_msg(&mut self, m: PreviewMsg) {
        let PreviewMsg::Ready { seq, key, body, .. } = m;
        if self.pending_key.as_deref() != Some(key.as_str()) || seq != self.seq {
            return;
        }
        self.pending_key = None;
        self.body_path = key.split('|').next().map(String::from);
        self.current_key = Some(key);
        self.body = Some(body);
    }

    pub fn scroll(&mut self, d: isize) {
        if d <= isize::MIN / 4 {
            self.scroll = 0;
        } else if d < 0 {
            self.scroll = self.scroll.saturating_sub(d.unsigned_abs());
        } else {
            self.scroll = self.scroll.saturating_add(d as usize);
        }
    }

    /// Keys for the focused preview that depend on the content: JSON tree
    /// navigation (`t` toggles), horizontal scroll (Left / Right) for text and
    /// Space to pause an animation. Returns true if the key was used.
    pub fn key(&mut self, k: &crossterm::event::KeyEvent) -> bool {
        use crossterm::event::KeyCode;
        self.focused = true;
        let Some(body) = &mut self.body else { return false };
        match body {
            Body::Json { tree, show_tree, .. } => {
                if k.code == KeyCode::Char('t') {
                    *show_tree = !*show_tree;
                    return true;
                }
                if !*show_tree {
                    return self.hscroll_key(k.code);
                }
                let page = self.body_area.height.max(1) as isize;
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => tree.move_by(-1),
                    KeyCode::Down | KeyCode::Char('j') => tree.move_by(1),
                    KeyCode::PageUp => tree.move_by(-page),
                    KeyCode::PageDown => tree.move_by(page),
                    KeyCode::Home => tree.home(),
                    KeyCode::End => tree.end(),
                    KeyCode::Left | KeyCode::Char('h') => tree.left(),
                    KeyCode::Right | KeyCode::Char('l') => tree.right(),
                    KeyCode::Enter | KeyCode::Char(' ') => tree.toggle(),
                    _ => return false,
                }
                true
            }
            Body::Anim { paused, next_at, .. } => {
                if k.code == KeyCode::Char(' ') {
                    *paused = !*paused;
                    *next_at = Instant::now();
                    return true;
                }
                false
            }
            Body::Lines(_) => self.hscroll_key(k.code),
            _ => false,
        }
    }

    fn hscroll_key(&mut self, code: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Left => self.hscroll = self.hscroll.saturating_sub(8),
            KeyCode::Right => self.hscroll = (self.hscroll + 8).min(4000),
            _ => return false,
        }
        true
    }

    /// Advance an animation when its frame delay has passed. Cheap; call often.
    pub fn tick(&mut self) {
        if let Some(Body::Anim { frames, current, next_at, paused, .. }) = &mut self.body
            && !*paused
            && frames.len() > 1
            && Instant::now() >= *next_at
        {
            *current = (*current + 1) % frames.len();
            *next_at = Instant::now() + frames[*current].1;
        }
    }

    /// True while an animation is playing (the main loop then redraws faster).
    pub fn animating(&self) -> bool {
        matches!(&self.body, Some(Body::Anim { paused: false, frames, .. }) if frames.len() > 1)
    }

    /// Toggle between the git diff and the file content (focused preview, key `d`).
    pub fn toggle_git_diff(&mut self) {
        self.git_diff_off = !self.git_diff_off;
    }

    pub fn is_image(&self) -> bool {
        matches!(self.body, Some(Body::Image { .. }))
    }

    pub fn zoom(&self) -> f64 {
        self.view.zoom
    }

    /// Multiply the zoom, keeping the image point at screen fraction (fx, fy) of the
    /// image rect fixed (the mouse position; the center for keyboard zoom).
    pub fn zoom_at(&mut self, factor: f64, fx: f64, fy: f64) {
        if !self.is_image() {
            return;
        }
        self.view = zoom_view(self.view, self.region(), factor, fx, fy);
    }

    pub fn zoom_in(&mut self) {
        self.zoom_at(ZOOM_STEP, 0.5, 0.5);
    }

    pub fn zoom_out(&mut self) {
        self.zoom_at(1.0 / ZOOM_STEP, 0.5, 0.5);
    }

    pub fn zoom_reset(&mut self) {
        self.view = View::default();
    }

    /// Pan by a fraction of the visible region (e.g. 0.2 = a fifth of the view).
    pub fn pan(&mut self, dx: f64, dy: f64) {
        if !self.is_image() || self.view.zoom <= 1.0 {
            return;
        }
        self.view = pan_view(self.view, self.region(), dx, dy);
    }

    fn region(&self) -> (f64, f64, f64, f64) {
        match &self.body {
            Some(Body::Image { region, .. }) => *region,
            _ => (0.0, 0.0, 1.0, 1.0),
        }
    }

    /// Mouse over the preview pane: wheel zooms toward the pointer, drag pans,
    /// double-click resets. Returns true if the event was used.
    pub fn mouse(&mut self, m: crossterm::event::MouseEvent) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        if !self.is_image() {
            return false;
        }
        let r = self.image_rect;
        let inside = r.width > 0 && r.contains((m.column, m.row).into());
        let frac = |m: &crossterm::event::MouseEvent| {
            (
                (m.column.saturating_sub(r.x) as f64 + 0.5) / r.width.max(1) as f64,
                (m.row.saturating_sub(r.y) as f64 + 0.5) / r.height.max(1) as f64,
            )
        };
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let (fx, fy) = if inside { frac(&m) } else { (0.5, 0.5) };
                let f = if matches!(m.kind, MouseEventKind::ScrollUp) { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
                self.zoom_at(f, fx, fy);
                true
            }
            MouseEventKind::Down(MouseButton::Left) if inside => {
                let double = self
                    .last_click
                    .is_some_and(|(t, x, y)| t.elapsed() < Duration::from_millis(400) && x == m.column && y == m.row);
                if double {
                    self.last_click = None;
                    self.drag = None;
                    if self.view.zoom > 1.0 {
                        self.zoom_reset();
                    } else {
                        let (fx, fy) = frac(&m);
                        self.zoom_at(2.0, fx, fy);
                    }
                } else {
                    self.last_click = Some((Instant::now(), m.column, m.row));
                    self.drag = Some((m.column, m.row));
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((px, py)) = self.drag else { return false };
                let dx = (px as f64 - m.column as f64) / r.width.max(1) as f64;
                let dy = (py as f64 - m.row as f64) / r.height.max(1) as f64;
                self.drag = Some((m.column, m.row));
                self.pan(dx, dy);
                true
            }
            MouseEventKind::Up(_) => {
                let used = self.drag.is_some();
                self.drag = None;
                used
            }
            _ => false,
        }
    }

    pub fn invalidate(&mut self) {
        self.current_key = None;
        self.pending_key = None;
        let _ = &self.tx;
    }
}

fn draw_header(f: &mut Frame, r: Rect, e: &Entry, ctx: &Ctx) {
    let w = r.width as usize;
    let mut spans = Vec::new();
    let mut used = 0;
    if ctx.cfg.nerd_fonts {
        let (g, c) = icons::icon_for(e);
        let mut st = ctx.theme.header;
        if let Some(c) = c {
            st = st.fg(c);
        }
        spans.push(Span::styled(format!("{g} "), st));
        used += 2;
    }
    let size = if e.is_dir() { "DIR".to_string() } else { human_size(e.size) };
    let right = format!(" {}  {}  {}", size, util::short_time(e.mtime), e.perm_string());
    let rw = str_width(&right);
    let name_w = w.saturating_sub(used + rw);
    let (right, name_w) =
        if name_w < 8 { (format!(" {size}"), w.saturating_sub(used + size.len() + 1)) } else { (right, name_w) };
    spans.push(Span::styled(pad_right(&trunc_mid(&e.name, name_w), name_w), ctx.theme.header));
    spans.push(Span::styled(right, ctx.theme.panel.patch(ctx.theme.hidden)));
    f.render_widget(Paragraph::new(Line::from(spans)).style(ctx.theme.panel), r);
}

// ------------------------------------------------------------------ worker

fn worker(rx: Receiver<Job>, tx: Sender<AppEvent>, latest: Arc<AtomicU64>, id: u64) {
    while let Ok(mut job) = rx.recv() {
        // Debounce: wait a little and keep only the newest request. Zoom/pan of the
        // image already shown only needs a short wait to coalesce wheel bursts.
        let deadline = Instant::now() + if job.quick { Duration::from_millis(15) } else { DEBOUNCE };
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(j) => job = j,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
                Err(_) => return,
            }
        }
        if job.seq != latest.load(Relaxed) {
            continue;
        }
        let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| build(&job)))
            .unwrap_or_else(|_| Body::Error("Preview failed".into()));
        if job.seq != latest.load(Relaxed) {
            continue;
        }
        if tx.send(AppEvent::Preview(PreviewMsg::Ready { id, seq: job.seq, key: job.key, body })).is_err() {
            return;
        }
    }
}

thread_local! {
    /// Last decoded image in the worker: (file key, image, format label, extra lines).
    static DECODED: std::cell::RefCell<Option<(String, image::DynamicImage, String, Vec<Line<'static>>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Identity of the file a job previews, independent of pane size and view.
fn file_key(job: &Job) -> String {
    format!("{}|{}|{:?}", job.path.display(), job.entry.size, job.entry.mtime)
}

/// Largest side kept for the cached source image (bounds memory; plenty for zooming).
const MAX_SOURCE_SIDE: u32 = 4096;

fn build(job: &Job) -> Body {
    let fk = file_key(job);
    let cached = DECODED
        .with(|c| c.borrow().as_ref().filter(|(k, ..)| *k == fk).map(|(_, i, f, x)| (i.clone(), f.clone(), x.clone())));
    if let Some((img, fmt, extra)) = cached {
        return render_image(job, &img, &fmt, extra);
    }
    let e = &job.entry;
    if e.is_dir() {
        return dir_listing(job).unwrap_or_else(|err| Body::Error(format!("Cannot read directory\n{err:#}")));
    }
    if matches!(e.kind, EntryKind::Other) {
        return Body::Lines(info_card(job, "Special file"));
    }
    if matches!(e.kind, EntryKind::Symlink { broken: true, .. }) {
        let mut l = info_card(job, "Broken symbolic link");
        l.insert(0, Line::styled(format!("-> {}", e.link_target().unwrap_or("?")), job.theme.broken));
        return Body::Lines(l);
    }
    let ext = icons::ext_of(&e.name);
    if crate::preview_ext::docs::is_doc_ext(&ext) {
        return doc_preview(job, &ext);
    }
    if crate::preview_ext::font::is_font_ext(&ext) {
        return font_preview(job);
    }
    if vfs::dmg::is_dmg_name(&e.name) {
        return dmg_preview(job);
    }
    if vfs::archive::supports(&e.name) {
        return archive_listing(job).unwrap_or_else(|err| {
            let mut l = vec![Line::styled(format!("Cannot open archive: {err:#}"), job.theme.error), Line::raw("")];
            l.extend(info_card(job, "Archive"));
            Body::Lines(l)
        });
    }
    if ext == "pdf" {
        return pdf_preview(job);
    }
    match icons::category(e) {
        Category::Image => image_preview(job),
        Category::Video => video_preview(job),
        Category::Audio => audio_preview(job),
        _ => {
            if job.git_diff
                && let Some(lines) = crate::git::preview_diff(&job.path, &job.theme)
            {
                return Body::Lines(lines);
            }
            text_or_binary(job, &ext)
        }
    }
}

/// Disk image: describe it with `hdiutil imageinfo` (or 7-Zip) without mounting.
fn dmg_preview(job: &Job) -> Body {
    let mut l = vec![Line::styled("Disk image (Enter mounts it read-only)", job.theme.header), Line::raw("")];
    let Some(path) = job.loc.local_path().map(|d| d.join(&job.entry.name)) else {
        l.extend(info_card(job, "Disk image"));
        return Body::Lines(l);
    };
    match vfs::dmg::image_info(&path) {
        Ok(lines) => l.extend(lines.into_iter().map(|t| Line::styled(t, job.theme.panel))),
        Err(e) => l.push(Line::styled(format!("{e:#}"), job.theme.error)),
    }
    l.push(Line::raw(""));
    l.extend(info_card(job, "Disk image"));
    Body::Lines(l)
}

fn read_head(job: &Job, max: usize) -> anyhow::Result<Vec<u8>> {
    job.loc.vfs.lock().read_head(&job.path, max)
}

fn info_card(job: &Job, kind: &str) -> Vec<Line<'static>> {
    let e = &job.entry;
    let t = &job.theme;
    let row =
        |k: &str, v: String| Line::from(vec![Span::styled(format!("{k:>10}: "), t.header), Span::styled(v, t.panel)]);
    vec![
        row("Type", kind.to_string()),
        row("Size", format!("{} ({})", util::grouped(e.size), human_size(e.size))),
        row("Modified", util::full_time(e.mtime)),
        row("Mode", format!("{} ({:04o})", e.perm_string(), e.mode & 0o7777)),
        row("Owner", format!("{}/{}", util::user_name(e.uid), util::group_name(e.gid))),
    ]
}

// ------------------------------------------------------------------ directories / archives

fn dir_listing(job: &Job) -> anyhow::Result<Body> {
    let mut list = job.loc.vfs.lock().list(&job.path)?;
    list.sort_by(|a, b| b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    let dirs = list.iter().filter(|e| e.is_dir()).count();
    let files = list.len() - dirs;
    let total: u64 = list.iter().filter(|e| !e.is_dir()).map(|e| e.size).sum();
    let t = &job.theme;
    let mut lines = vec![
        Line::styled(format!("{dirs} directories, {files} files, {} in files", human_size(total)), t.header),
        Line::raw(""),
    ];
    let w = job.cells.0 as usize;
    for e in list.iter().take(1000) {
        lines.push(entry_line(e, w, 0, t, job.nerd_fonts));
    }
    if list.len() > 1000 {
        lines.push(Line::styled(format!("... {} more", list.len() - 1000), t.hidden));
    }
    if list.is_empty() {
        lines.push(Line::styled("(empty)", t.hidden));
    }
    Ok(Body::Lines(lines))
}

fn entry_line(e: &Entry, width: usize, depth: usize, t: &Theme, nerd: bool) -> Line<'static> {
    let indent = "  ".repeat(depth);
    let mut spans = vec![Span::styled(indent.clone(), t.panel)];
    let mut used = indent.len();
    if nerd {
        let (g, c) = icons::icon_for(e);
        let mut st = t.panel;
        if let Some(c) = c {
            st = st.fg(c);
        }
        spans.push(Span::styled(format!("{g} "), st));
        used += 2;
    }
    let size = if e.is_dir() { String::new() } else { human_size(e.size) };
    let name = if e.is_dir() && !nerd { format!("{}/", e.name) } else { e.name.clone() };
    let name_w = width.saturating_sub(used + 8);
    spans.push(Span::styled(pad_right(&trunc_mid(&name, name_w), name_w), icons::style_for(e, t)));
    spans.push(Span::styled(pad_left(&size, 8), t.panel.patch(t.hidden)));
    Line::from(spans)
}

fn archive_listing(job: &Job) -> anyhow::Result<Body> {
    let mut fs = vfs::archive::open(&job.loc, &job.path)?;
    let t = &job.theme;
    let w = job.cells.0 as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut count = 0usize;
    let mut total = 0u64;
    const LIMIT: usize = 2000;
    #[allow(clippy::too_many_arguments)]
    fn walk(
        fs: &mut Box<dyn vfs::Vfs>,
        dir: &Path,
        depth: usize,
        out: &mut Vec<Line<'static>>,
        count: &mut usize,
        total: &mut u64,
        job: &Job,
        w: usize,
    ) {
        if *count > LIMIT * 4 || depth > 32 {
            return;
        }
        let mut list = fs.list(dir).unwrap_or_default();
        list.sort_by(|a, b| {
            b.is_dir().cmp(&a.is_dir()).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        for e in list {
            *count += 1;
            *total += e.size;
            if out.len() < LIMIT {
                out.push(entry_line(&e, w, depth, &job.theme, job.nerd_fonts));
            }
            if e.is_dir() {
                walk(fs, &dir.join(&e.name), depth + 1, out, count, total, job, w);
            }
        }
    }
    walk(&mut fs, Path::new("/"), 0, &mut out, &mut count, &mut total, job, w);
    let mut lines = vec![
        Line::styled(format!("Archive: {count} entries, {} uncompressed", human_size(total)), t.header),
        Line::raw(""),
    ];
    lines.extend(out);
    if count > LIMIT {
        lines.push(Line::styled(format!("... listing truncated at {LIMIT} entries"), t.hidden));
    }
    Ok(Body::Lines(lines))
}

// ------------------------------------------------------------------ text / binary

fn text_or_binary(job: &Job, ext: &str) -> Body {
    let data = match read_head(job, job.max_bytes) {
        Ok(d) => d,
        Err(e) => return Body::Error(format!("Cannot read file\n{e:#}")),
    };
    if data.is_empty() {
        return Body::Lines(vec![Line::styled("(empty file)", job.theme.hidden)]);
    }
    if util::looks_binary(&data) {
        if crate::preview_ext::binary::looks_executable(&data) {
            return executable_preview(job, &data);
        }
        return Body::Lines(hex_dump(&data[..data.len().min(16 * 1024)], job));
    }
    let text = String::from_utf8_lossy(&data).replace("\r\n", "\n");
    let truncated = data.len() >= job.max_bytes && (job.entry.size as usize) > data.len();
    if let Some(body) = structured_preview(job, &text, ext, truncated) {
        return body;
    }
    let mut lines = if matches!(ext, "md" | "markdown" | "mdx") { markdown(&text, job) } else { source(&text, job) };
    if truncated {
        lines.push(Line::styled(
            format!("... truncated, showing first {}", human_size(data.len() as u64)),
            job.theme.hidden,
        ));
    }
    Body::Lines(lines)
}

fn source(text: &str, job: &Job) -> Vec<Line<'static>> {
    let t = &job.theme;
    let total = text.lines().count().min(MAX_TEXT_LINES);
    let hl = if total <= MAX_HIGHLIGHT_LINES {
        crate::highlight::highlight(text, &job.entry.name, &t.syntax, MAX_HIGHLIGHT_LINES, job.tab)
    } else {
        None
    };
    let body: Vec<Line<'static>> = match hl {
        Some(l) => l,
        None if total <= MAX_TEXT_LINES
            && crate::highlight::find_syntax(&job.entry.name, text.lines().next().unwrap_or("")).is_none() =>
        {
            // No grammar: light highlighting of URLs, dates, numbers, paths, log levels.
            let pal = crate::preview_ext::plaintext::Palette::new(t);
            text.lines()
                .take(MAX_TEXT_LINES)
                .map(|l| crate::preview_ext::plaintext::line(&l.replace('\t', &" ".repeat(job.tab)), &pal))
                .collect()
        }
        None => text
            .lines()
            .take(MAX_TEXT_LINES)
            .map(|l| Line::styled(l.replace('\t', &" ".repeat(job.tab)), t.panel))
            .collect(),
    };
    let nw = total.max(1).to_string().len();
    let show_nr = job.cells.0 as usize > nw + 20;
    body.into_iter()
        .enumerate()
        .map(|(i, mut line)| {
            if show_nr {
                line.spans.insert(0, Span::styled(format!("{:>nw$} ", i + 1), t.editor_line_nr));
            }
            line
        })
        .collect()
}

fn hex_dump(data: &[u8], job: &Job) -> Vec<Line<'static>> {
    let t = &job.theme;
    let per = if job.cells.0 >= 78 { 16 } else { 8 };
    let mut lines = vec![Line::styled(format!("Binary file, first {} bytes:", data.len()), t.header), Line::raw("")];
    for (row, chunk) in data.chunks(per).enumerate() {
        let mut hex = String::new();
        for (i, b) in chunk.iter().enumerate() {
            if i == 8 {
                hex.push(' ');
            }
            hex.push_str(&format!("{b:02x} "));
        }
        let hex_w = per * 3 + if per > 8 { 1 } else { 0 };
        let ascii: String = chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
        lines.push(Line::from(vec![
            Span::styled(format!("{:08x}  ", row * per), t.editor_line_nr),
            Span::styled(pad_right(&hex, hex_w), t.panel),
            Span::styled(format!(" {ascii}"), t.panel.patch(t.hidden)),
        ]));
    }
    lines
}

/// Minimal Markdown renderer: headings, lists, quotes, rules, fenced code, inline emphasis.
fn markdown(text: &str, job: &Job) -> Vec<Line<'static>> {
    let t = &job.theme;
    let mut out = Vec::new();
    let mut in_code: Option<(String, String)> = None; // (lang, buffer)
    let rule_w = (job.cells.0 as usize).min(80);
    for raw in text.lines().take(MAX_TEXT_LINES) {
        let line = raw.replace('\t', &" ".repeat(job.tab));
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if let Some((lang, buf)) = in_code.take() {
                let name = format!("x.{}", if lang.is_empty() { "txt" } else { &lang });
                let hl = crate::highlight::highlight(&buf, &name, &t.syntax, MAX_HIGHLIGHT_LINES, job.tab)
                    .unwrap_or_else(|| buf.lines().map(|l| Line::styled(l.to_string(), t.panel)).collect());
                for mut l in hl {
                    l.spans.insert(0, Span::styled("  ", t.panel));
                    out.push(l);
                }
            } else {
                in_code = Some((trimmed.trim_start_matches(['`', '~']).trim().to_string(), String::new()));
            }
            continue;
        }
        if let Some((_, buf)) = in_code.as_mut() {
            buf.push_str(&line);
            buf.push('\n');
            continue;
        }
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|c| *c == '#').count();
            let title = trimmed[level..].trim().to_string();
            let st = match level {
                1 => t.header.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                2 => t.dir.add_modifier(Modifier::BOLD),
                _ => t.exec.add_modifier(Modifier::BOLD),
            };
            if !out.is_empty() {
                out.push(Line::raw(""));
            }
            out.push(Line::styled(title, st));
            continue;
        }
        if matches!(trimmed, "---" | "***" | "___") {
            out.push(Line::styled("-".repeat(rule_w), t.border));
            continue;
        }
        let indent = line.len() - trimmed.len();
        if let Some(rest) = trimmed.strip_prefix("> ").or_else(|| (trimmed == ">").then_some("")) {
            let mut spans = vec![Span::styled("  | ", t.border)];
            spans.extend(inline(rest, t, t.panel.patch(t.hidden).add_modifier(Modifier::ITALIC)));
            out.push(Line::from(spans));
            continue;
        }
        let bullet = ["- ", "* ", "+ "].iter().find_map(|p| trimmed.strip_prefix(p));
        if let Some(rest) = bullet {
            let (mark, rest) = if let Some(r) = rest.strip_prefix("[ ] ") {
                ("[ ] ", r)
            } else if let Some(r) = rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
                ("[x] ", r)
            } else {
                ("- ", rest)
            };
            let mut spans = vec![Span::styled(format!("{}{mark}", " ".repeat(indent + 2)), t.header)];
            spans.extend(inline(rest, t, t.panel));
            out.push(Line::from(spans));
            continue;
        }
        let num_end =
            trimmed.find(". ").filter(|&i| i > 0 && i < 4 && trimmed[..i].chars().all(|c| c.is_ascii_digit()));
        if let Some(i) = num_end {
            let mut spans = vec![Span::styled(format!("{}{} ", " ".repeat(indent + 2), &trimmed[..=i]), t.header)];
            spans.extend(inline(&trimmed[i + 2..], t, t.panel));
            out.push(Line::from(spans));
            continue;
        }
        out.push(Line::from(inline(&line, t, t.panel)));
    }
    if let Some((_, buf)) = in_code {
        for l in buf.lines() {
            out.push(Line::styled(format!("  {l}"), t.panel));
        }
    }
    out
}

/// Inline markdown: **bold**, *italic* / _italic_, `code`, [text](url).
fn inline(s: &str, t: &Theme, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut buf = String::new();
    let mut i = 0;
    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), base));
        }
    };
    let find = |from: usize, pat: &[char]| -> Option<usize> {
        (from..chars.len().saturating_sub(pat.len() - 1)).find(|&j| chars[j..j + pat.len()] == *pat)
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '`'
            && let Some(end) = find(i + 1, &['`'])
        {
            flush(&mut buf, &mut spans);
            let code: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(code, t.source.patch(t.cursor_inactive).remove_modifier(Modifier::BOLD)));
            i = end + 1;
            continue;
        }
        if c == '*'
            && chars.get(i + 1) == Some(&'*')
            && let Some(end) = find(i + 2, &['*', '*'])
        {
            flush(&mut buf, &mut spans);
            let txt: String = chars[i + 2..end].iter().collect();
            spans.push(Span::styled(txt, base.add_modifier(Modifier::BOLD)));
            i = end + 2;
            continue;
        }
        if (c == '*' || c == '_')
            && chars.get(i + 1).is_some_and(|n| !n.is_whitespace())
            && (i == 0 || !chars[i - 1].is_alphanumeric())
            && let Some(end) = find(i + 1, &[c])
            && end > i + 1
        {
            flush(&mut buf, &mut spans);
            let txt: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(txt, base.add_modifier(Modifier::ITALIC)));
            i = end + 1;
            continue;
        }
        if c == '['
            && let Some(mid) = find(i + 1, &[']', '('])
            && let Some(end) = find(mid + 2, &[')'])
        {
            flush(&mut buf, &mut spans);
            let txt: String = chars[i + 1..mid].iter().collect();
            spans.push(Span::styled(txt, t.symlink.add_modifier(Modifier::UNDERLINED)));
            i = end + 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut spans);
    spans
}

// ------------------------------------------------------------------ images

fn image_preview(job: &Job) -> Body {
    let data = match read_head(job, MAX_IMAGE_BYTES) {
        Ok(d) => d,
        Err(e) => return Body::Error(format!("Cannot read image\n{e:#}")),
    };
    if crate::svg::is_svg_name(&job.entry.name) {
        return match crate::svg::rasterize(&data, crate::svg::MAX_SIDE) {
            Ok(r) => {
                image_body(job, r.image, format!("SVG {}x{} vector", r.width.round(), r.height.round()), Vec::new())
            }
            Err(e) => {
                let mut l = vec![Line::styled(format!("Cannot render SVG: {e:#}"), job.theme.error), Line::raw("")];
                l.extend(info_card(job, "SVG image"));
                Body::Lines(l)
            }
        };
    }
    let fmt = image::guess_format(&data).ok().map(|f| format!("{f:?}").to_uppercase());
    if job.view.zoom <= 1.0
        && let Some(body) = anim_preview(job, &data, fmt.as_deref().unwrap_or("IMAGE"))
    {
        return body;
    }
    let exif = crate::preview_ext::exif::read(&data);
    match image::load_from_memory(&data) {
        Ok(img) => {
            let (img, extra) = match &exif {
                Some(info) => (
                    crate::preview_ext::exif::apply_orientation(img, info.orientation),
                    kv_rows(&info.fields, &job.theme),
                ),
                None => (img, Vec::new()),
            };
            image_body(job, img, fmt.unwrap_or_else(|| icons::ext_of(&job.entry.name).to_uppercase()), extra)
        }
        Err(e) => {
            let mut l = vec![Line::styled(format!("Cannot decode image: {e}"), job.theme.error), Line::raw("")];
            l.extend(info_card(job, "Image"));
            Body::Lines(l)
        }
    }
}

fn image_body(job: &Job, img: image::DynamicImage, fmt: String, extra: Vec<Line<'static>>) -> Body {
    let img = if img.width().max(img.height()) > MAX_SOURCE_SIDE {
        img.thumbnail(MAX_SOURCE_SIDE, MAX_SOURCE_SIDE)
    } else {
        img
    };
    let body = render_image(job, &img, &fmt, extra.clone());
    DECODED.with(|c| *c.borrow_mut() = Some((file_key(job), img, fmt, extra)));
    body
}

/// Scale (and for zoom > 1, crop) the source image to the pane and encode it.
fn render_image(job: &Job, img: &image::DynamicImage, fmt: &str, extra: Vec<Line<'static>>) -> Body {
    let (iw, ih) = (img.width(), img.height());
    let mut caption = if fmt.starts_with("SVG ") {
        format!("{fmt}  {}", human_size(job.entry.size))
    } else {
        format!("{iw}x{ih}  {fmt}  {}", human_size(job.entry.size))
    };
    if job.view.zoom > 1.0 {
        caption.push_str(&format!("  zoom {:.1}x", job.view.zoom));
    }
    let Some(picker) = &job.picker else {
        let mut l = vec![Line::styled(caption, job.theme.header), Line::raw("")];
        l.push(Line::styled("Image display is disabled (no terminal graphics).", job.theme.hidden));
        l.extend(extra);
        return Body::Lines(l);
    };
    let extra_h = (extra.len() as u16).min(job.cells.1 / 3);
    let cw = job.cells.0.max(1);
    let ch = job.cells.1.saturating_sub(1 + extra_h).max(1);
    let fs = picker.font_size();
    let (pw, ph) = (cw as u32 * fs.width.max(1) as u32, ch as u32 * fs.height.max(1) as u32);
    if job.view.zoom > 1.0 {
        let (zoomed, region) = zoom_crop(img, pw, ph, job.view);
        return match picker.new_protocol(zoomed, Size::new(cw, ch), Resize::Fit(None)) {
            Ok(proto) => Body::Image { proto, caption, extra, region },
            Err(e) => Body::Error(format!("Cannot render image\n{e}")),
        };
    }
    let img = img.clone();
    let img = if iw > pw || ih > ph {
        img.thumbnail(pw, ph)
    } else {
        // Upscale small images (at most 4x) so icons and thumbnails stay readable.
        let factor = (pw as f64 / iw.max(1) as f64).min(ph as f64 / ih.max(1) as f64).min(4.0);
        if factor >= 1.5 {
            let filter = if iw.max(ih) <= 64 {
                image::imageops::FilterType::Nearest
            } else {
                image::imageops::FilterType::Triangle
            };
            img.resize((iw as f64 * factor) as u32, (ih as f64 * factor) as u32, filter)
        } else {
            img
        }
    };
    match picker.new_protocol(img, Size::new(cw, ch), Resize::Fit(None)) {
        Ok(proto) => Body::Image { proto, caption, extra, region: (0.0, 0.0, 1.0, 1.0) },
        Err(e) => Body::Error(format!("Cannot render image\n{e}")),
    }
}

/// Zoom step used by keys and the mouse wheel.
pub const ZOOM_STEP_PUB: f64 = ZOOM_STEP;

/// New view after multiplying the zoom by `factor`, keeping the image point at
/// screen fraction (fx, fy) of the shown `region` fixed. Shared by the preview
/// and the F3 viewer.
pub fn zoom_view(view: View, region: (f64, f64, f64, f64), factor: f64, fx: f64, fy: f64) -> View {
    let (rx, ry, rw, rh) = region;
    let old = view.zoom;
    let new = (old * factor).clamp(1.0, MAX_ZOOM);
    if (new - old).abs() < 1e-9 {
        return view;
    }
    if new <= 1.0 {
        return View::default();
    }
    // Image point under the cursor, as a fraction of the whole image.
    let ux = rx + fx.clamp(0.0, 1.0) * rw;
    let uy = ry + fy.clamp(0.0, 1.0) * rh;
    // The visible size shrinks by old/new (before clamping at the image edges).
    let k = old / new;
    let (nw, nh) = ((rw * k).min(1.0), (rh * k).min(1.0));
    View { zoom: new, cx: (ux - fx * nw + nw / 2.0).clamp(0.0, 1.0), cy: (uy - fy * nh + nh / 2.0).clamp(0.0, 1.0) }
}

/// New view after panning by a fraction of the visible region.
pub fn pan_view(view: View, region: (f64, f64, f64, f64), dx: f64, dy: f64) -> View {
    if view.zoom <= 1.0 {
        return view;
    }
    let (rx, ry, rw, rh) = region;
    // Start from the actual visible center (the requested one may lie past an edge),
    // and keep the region inside the image.
    let (cx, cy) = (rx + rw / 2.0, ry + rh / 2.0);
    View {
        zoom: view.zoom,
        cx: (cx + dx * rw).clamp(rw / 2.0, 1.0 - rw / 2.0),
        cy: (cy + dy * rh).clamp(rh / 2.0, 1.0 - rh / 2.0),
    }
}

/// Scale used at zoom 1: fit inside the pane, small images enlarged up to 4x.
fn fit_scale(iw: u32, ih: u32, pw: u32, ph: u32) -> f64 {
    let fit = (pw as f64 / iw.max(1) as f64).min(ph as f64 / ih.max(1) as f64);
    if fit < 1.0 {
        fit
    } else if fit.min(4.0) >= 1.5 {
        fit.min(4.0)
    } else {
        1.0
    }
}

/// Crop the region visible at `view` (shaped like the pane) and scale it to pane pixels.
/// Returns the image and the region as fractions of the source.
pub fn zoom_crop(
    img: &image::DynamicImage,
    pw: u32,
    ph: u32,
    view: View,
) -> (image::DynamicImage, (f64, f64, f64, f64)) {
    let (iw, ih) = (img.width().max(1) as f64, img.height().max(1) as f64);
    let s = fit_scale(img.width(), img.height(), pw, ph) * view.zoom.clamp(1.0, MAX_ZOOM);
    let vw = (pw as f64 / s).clamp(1.0, iw);
    let vh = (ph as f64 / s).clamp(1.0, ih);
    let x0 = (view.cx * iw - vw / 2.0).clamp(0.0, iw - vw);
    let y0 = (view.cy * ih - vh / 2.0).clamp(0.0, ih - vh);
    let crop = img.crop_imm(x0 as u32, y0 as u32, (vw.round() as u32).max(1), (vh.round() as u32).max(1));
    let tw = ((vw * s).round() as u32).clamp(1, pw.max(1));
    let th = ((vh * s).round() as u32).clamp(1, ph.max(1));
    // Nearest keeps pixels crisp when magnifying a lot (pixel art, inspecting detail).
    let filter = if s >= 3.0 { image::imageops::FilterType::Nearest } else { image::imageops::FilterType::Triangle };
    let out = crop.resize_exact(tw, th, filter);
    (out, (x0 / iw, y0 / ih, vw / iw, vh / ih))
}

// ------------------------------------------------------------------ external tools

fn tool(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

/// Run a command with a timeout; return stdout on success.
fn run_tool(cmd: &mut Command) -> Option<Vec<u8>> {
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let out = reader.join().ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if start.elapsed() > TOOL_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(_) => return None,
        }
    }
}

fn temp_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("pairdeck-preview-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// A local path for external tools: the file itself, or a temp copy for non-local VFS.
fn local_file(job: &Job) -> Option<PathBuf> {
    if job.loc.is_local() {
        return Some(job.path.clone());
    }
    if job.entry.size > MAX_TEMP_COPY {
        return None;
    }
    let ext = icons::ext_of(&job.entry.name);
    let dst = temp_dir().join(format!("src.{ext}"));
    let mut r = job.loc.vfs.lock().open_read(&job.path).ok()?;
    let mut w = std::fs::File::create(&dst).ok()?;
    std::io::copy(&mut r, &mut w).ok()?;
    Some(dst)
}

fn load_png(path: &Path) -> Option<image::DynamicImage> {
    let img = image::open(path).ok();
    let _ = std::fs::remove_file(path);
    img
}

fn pdf_preview(job: &Job) -> Body {
    let Some(file) = local_file(job) else { return Body::Lines(info_card(job, "PDF document")) };
    let info = tool("pdfinfo").and_then(|p| run_tool(Command::new(p).arg(&file))).map(|o| {
        kv_lines(
            &String::from_utf8_lossy(&o),
            job,
            &["Title", "Author", "Creator", "Producer", "Pages", "Page size", "PDF version"],
        )
    });
    if job.picker.is_some()
        && let Some(p) = tool("pdftoppm")
    {
        let base = temp_dir().join("page");
        let ok = run_tool(
            Command::new(p)
                .args(["-png", "-f", "1", "-l", "1", "-singlefile", "-scale-to", "1200"])
                .arg(&file)
                .arg(&base),
        )
        .is_some();
        if ok && let Some(img) = load_png(&base.with_extension("png")) {
            let extra = info.clone().unwrap_or_default().into_iter().take(3).collect();
            return image_body(job, img, "PDF page 1".into(), extra);
        }
    }
    let mut l = info.unwrap_or_default();
    if !l.is_empty() {
        l.push(Line::raw(""));
    }
    l.extend(info_card(job, "PDF document"));
    Body::Lines(l)
}

/// Turn "Key: value" text into styled lines, keeping only wanted keys (in that order).
fn kv_lines(text: &str, job: &Job, keys: &[&str]) -> Vec<Line<'static>> {
    let t = &job.theme;
    let mut out = Vec::new();
    for k in keys {
        if let Some(v) = text.lines().find_map(|l| l.strip_prefix(&format!("{k}:")).map(|v| v.trim().to_string()))
            && !v.is_empty()
        {
            out.push(Line::from(vec![Span::styled(format!("{k:>11}: "), t.header), Span::styled(v, t.panel)]));
        }
    }
    out
}

/// Media metadata via ffprobe, as styled lines.
fn ffprobe(file: &Path, job: &Job) -> Vec<Line<'static>> {
    let Some(p) = tool("ffprobe") else { return Vec::new() };
    let Some(out) = run_tool(Command::new(p).args([
        "-v", "error",
        "-show_entries",
        "format=duration,bit_rate,format_long_name:format_tags=title,artist,album,date,genre:stream=codec_type,codec_name,width,height,sample_rate,channels,r_frame_rate",
        "-of", "default=noprint_wrappers=1",
    ]).arg(file)) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out);
    let t = &job.theme;
    let mut rows: Vec<(String, String)> = Vec::new();
    let mut stream_type = String::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        if v.is_empty() || v == "N/A" {
            continue;
        }
        let k = k.trim_start_matches("TAG:").to_ascii_lowercase();
        let label = match k.as_str() {
            "codec_type" => {
                stream_type = v.to_string();
                continue;
            }
            "codec_name" => format!("{} codec", if stream_type.is_empty() { "stream" } else { &stream_type }),
            "duration" => {
                let secs: f64 = v.parse().unwrap_or(0.0);
                let s = secs as u64;
                rows.push(("duration".into(), format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)));
                continue;
            }
            "bit_rate" => {
                let b: u64 = v.parse().unwrap_or(0);
                rows.push(("bitrate".into(), format!("{} kb/s", b / 1000)));
                continue;
            }
            "width" => {
                rows.push(("resolution".into(), v.to_string()));
                continue;
            }
            "height" => {
                if let Some(r) = rows.iter_mut().rev().find(|r| r.0 == "resolution") {
                    r.1 = format!("{}x{v}", r.1);
                }
                continue;
            }
            "r_frame_rate" => {
                if let Some((a, b)) = v.split_once('/') {
                    let (a, b): (f64, f64) = (a.parse().unwrap_or(0.0), b.parse().unwrap_or(1.0));
                    if a > 0.0 && b > 0.0 {
                        rows.push(("frame rate".into(), format!("{:.2} fps", a / b)));
                    }
                }
                continue;
            }
            "sample_rate" => "sample rate".into(),
            "format_long_name" => "format".into(),
            other => other.to_string(),
        };
        rows.push((label, v.to_string()));
    }
    rows.dedup();
    rows.into_iter()
        .map(|(k, v)| Line::from(vec![Span::styled(format!("{k:>13}: "), t.header), Span::styled(v, t.panel)]))
        .collect()
}

fn video_preview(job: &Job) -> Body {
    let Some(file) = local_file(job) else { return Body::Lines(info_card(job, "Video")) };
    let meta = ffprobe(&file, job);
    if job.picker.is_some() {
        let out = temp_dir().join("frame.png");
        let mut got = false;
        if let Some(p) = tool("ffmpegthumbnailer") {
            got = run_tool(Command::new(p).arg("-i").arg(&file).arg("-o").arg(&out).args(["-s", "800", "-t", "10%"]))
                .is_some();
        }
        if !got && let Some(p) = tool("ffmpeg") {
            for seek in ["00:00:05", "00:00:00"] {
                let ok = run_tool(
                    Command::new(&p)
                        .args(["-v", "error", "-ss", seek, "-i"])
                        .arg(&file)
                        .args(["-frames:v", "1", "-vf", "scale=800:-2", "-y"])
                        .arg(&out),
                )
                .is_some();
                if ok && out.exists() {
                    got = true;
                    break;
                }
            }
        }
        if got && let Some(img) = load_png(&out) {
            return image_body(job, img, "video frame".into(), meta);
        }
    }
    let mut l = meta;
    if !l.is_empty() {
        l.push(Line::raw(""));
    }
    l.extend(info_card(job, "Video"));
    Body::Lines(l)
}

fn audio_preview(job: &Job) -> Body {
    let mut l = match local_file(job) {
        Some(file) => ffprobe(&file, job),
        None => Vec::new(),
    };
    if !l.is_empty() {
        l.push(Line::raw(""));
    }
    l.extend(info_card(job, "Audio"));
    Body::Lines(l)
}

// ------------------------------------------------------------------ extended previews

/// "Label: value" rows (label column aligned) in theme colors.
fn kv_rows(rows: &[(String, String)], t: &Theme) -> Vec<Line<'static>> {
    let w = rows.iter().map(|(k, _)| str_width(k)).max().unwrap_or(0);
    rows.iter()
        .map(|(k, v)| {
            if k.is_empty() {
                Line::styled(format!("{}  {v}", " ".repeat(w)), t.panel)
            } else {
                Line::from(vec![
                    Span::styled(format!("{}: ", pad_right(k, w)), t.header),
                    Span::styled(v.clone(), t.panel),
                ])
            }
        })
        .collect()
}

/// CSV tables, JSON (pretty + tree) and TOML validation. None = not handled here.
fn structured_preview(job: &Job, text: &str, ext: &str, truncated: bool) -> Option<Body> {
    use crate::preview_ext::{structured, table};
    let t = &job.theme;
    if table::is_table_ext(ext) {
        let tb = table::parse(text, ext)?;
        let mut lines = table::render(&tb, t);
        if truncated {
            lines.push(Line::styled("... file truncated for preview", t.hidden));
        }
        return Some(Body::Lines(lines));
    }
    if structured::is_json_ext(ext) && !truncated {
        return match structured::parse_json(text) {
            Ok(v) => {
                let pretty = structured::pretty_json(&v);
                let mut lines = source(&pretty, job);
                lines.insert(0, Line::styled("JSON (t: tree view)", t.panel.patch(t.hidden)));
                let tree = structured::JsonTree::new(&v);
                Some(Body::Json { lines, tree, show_tree: false })
            }
            Err(e) => {
                let mut lines = vec![Line::styled(format!("Invalid JSON: {e}"), t.diff_del), Line::raw("")];
                lines.extend(source(text, job));
                Some(Body::Lines(lines))
            }
        };
    }
    if ext == "toml"
        && !truncated
        && let Some(err) = structured::toml_error(text)
    {
        let mut lines = vec![Line::styled(format!("Invalid TOML: {err}"), t.diff_del), Line::raw("")];
        lines.extend(source(text, job));
        return Some(Body::Lines(lines));
    }
    None
}

/// ELF / Mach-O / PE summary followed by a hex dump of the start.
fn executable_preview(job: &Job, head: &[u8]) -> Body {
    use crate::preview_ext::binary;
    let full;
    let data = if (job.entry.size as usize) > head.len() {
        full = read_head(job, binary::MAX_BINARY_BYTES).unwrap_or_else(|_| head.to_vec());
        &full[..]
    } else {
        head
    };
    let mut lines = kv_rows(&binary::summary(data), &job.theme);
    lines.push(Line::raw(""));
    lines.extend(hex_dump(&head[..head.len().min(4096)], job));
    Body::Lines(lines)
}

/// Office documents and e-books as plain text.
fn doc_preview(job: &Job, ext: &str) -> Body {
    let data = match read_head(job, MAX_IMAGE_BYTES) {
        Ok(d) => d,
        Err(e) => return Body::Error(format!("Cannot read document\n{e:#}")),
    };
    match crate::preview_ext::docs::extract_text(&data, ext) {
        Ok(text) if !text.trim().is_empty() => {
            let t = &job.theme;
            let mut lines = vec![Line::styled(format!("{} text", ext.to_uppercase()), t.panel.patch(t.hidden))];
            lines.extend(text.lines().take(MAX_TEXT_LINES).map(|l| Line::styled(l.to_string(), t.panel)));
            Body::Lines(lines)
        }
        Ok(_) => Body::Lines(info_card(job, "Document (no text found)")),
        Err(e) => {
            if ext == "epub" {
                return archive_listing(job).unwrap_or_else(|_| Body::Error(format!("Cannot read e-book\n{e:#}")));
            }
            let mut l = vec![Line::styled(format!("Cannot read document: {e:#}"), job.theme.error), Line::raw("")];
            l.extend(info_card(job, "Document"));
            Body::Lines(l)
        }
    }
}

/// Font sample sheet rendered as an image, with family / style rows below.
fn font_preview(job: &Job) -> Body {
    let data = match read_head(job, crate::preview_ext::font::MAX_FONT_BYTES) {
        Ok(d) => d,
        Err(e) => return Body::Error(format!("Cannot read font\n{e:#}")),
    };
    match crate::preview_ext::font::sample_sheet(&data) {
        Ok(sheet) => {
            let extra = kv_rows(&sheet.info, &job.theme);
            image_body(job, sheet.image, "font sample".into(), extra)
        }
        Err(e) => {
            let mut l = vec![Line::styled(format!("{e:#}"), job.theme.error), Line::raw("")];
            if let Ok(info) = crate::preview_ext::font::info(&data) {
                l.extend(kv_rows(&info, &job.theme));
                l.push(Line::raw(""));
            }
            l.extend(info_card(job, "Font"));
            Body::Lines(l)
        }
    }
}

/// Encode every frame of an animated GIF / APNG / WebP for the pane.
/// Returns None for still images (the normal image path handles them).
fn anim_preview(job: &Job, data: &[u8], fmt: &str) -> Option<Body> {
    use crate::preview_ext::anim;
    let picker = job.picker.as_ref()?;
    if !anim::is_animated(data) {
        return None;
    }
    let cw = job.cells.0.max(1);
    let ch = job.cells.1.saturating_sub(1).max(1);
    let fs = picker.font_size();
    let (pw, ph) = (cw as u32 * fs.width.max(1) as u32, ch as u32 * fs.height.max(1) as u32);
    let mut frames = Vec::new();
    let mut dims = (0, 0);
    let mut total = Duration::ZERO;
    anim::for_each_frame(data, |img, delay| {
        dims = img.dimensions();
        let img = image::DynamicImage::ImageRgba8(img);
        let s = fit_scale(dims.0, dims.1, pw, ph);
        let img = if (s - 1.0).abs() > 0.01 {
            let filter =
                if s >= 3.0 { image::imageops::FilterType::Nearest } else { image::imageops::FilterType::Triangle };
            img.resize(((dims.0 as f64 * s) as u32).max(1), ((dims.1 as f64 * s) as u32).max(1), filter)
        } else {
            img
        };
        match picker.new_protocol(img, Size::new(cw, ch), Resize::Fit(None)) {
            Ok(p) => {
                total += delay;
                frames.push((p, delay));
                true
            }
            Err(_) => false,
        }
    })?;
    if frames.len() < 2 {
        return None;
    }
    let caption = format!(
        "{}x{}  {fmt} animation  {} frames  {:.1}s  {}",
        dims.0,
        dims.1,
        frames.len(),
        total.as_secs_f64(),
        human_size(job.entry.size)
    );
    let first = frames[0].1;
    Some(Body::Anim { frames, caption, current: 0, next_at: Instant::now() + first, paused: false })
}

#[cfg(test)]
mod kitty_tests {
    use super::*;
    use crate::app::AppEvent;
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::{FontSize, picker::ProtocolType};

    /// The quick view must emit Kitty graphics data for an image in a half-width pane.
    #[test]
    #[allow(deprecated)]
    fn quick_view_emits_kitty_image() {
        let dir = std::env::temp_dir().join(format!("pairdeck-kitty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("pic.png");
        image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([(x * 4) as u8, (y * 5) as u8, 128])).save(&png).unwrap();

        let mut picker = Picker::from_fontsize(FontSize::new(8, 16));
        picker.set_protocol_type(ProtocolType::Kitty);
        let (tx, rx) = crossbeam_channel::unbounded::<AppEvent>();
        let mut pv = Previewer::new(tx, Some(picker.clone()));
        let loc = Location::local(&dir);
        let entry = crate::vfs::local::stat_local(&png).unwrap();
        let theme = crate::theme::Theme::by_name("dark");
        let cfg = crate::config::Config::default();
        let ctx = crate::ui::Ctx { theme: &theme, cfg: &cfg, classic: false, picker: Some(&picker) };
        let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
        let area = Rect::new(0, 0, 60, 30);
        term.draw(|f| pv.render(f, area, &loc, Some(&entry), &ctx)).unwrap();
        match rx.recv_timeout(std::time::Duration::from_secs(5)).expect("preview result") {
            AppEvent::Preview(m) => pv.on_msg(m),
            _ => panic!("unexpected event"),
        }
        let frame = term.draw(|f| pv.render(f, area, &loc, Some(&entry), &ctx)).unwrap();
        let all: String = frame.buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(all.contains("\x1b_G"), "no kitty transmit sequence in quick view buffer");
        assert!(all.contains('\u{10EEEE}'), "no kitty placeholder cells in quick view buffer");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::*;
    use crate::app::AppEvent;
    use ratatui::{Terminal, backend::TestBackend};
    use ratatui_image::FontSize;

    fn gradient(w: u32, h: u32) -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| image::Rgb([x as u8, y as u8, 7])))
    }

    #[test]
    fn crop_matches_zoom_and_stays_in_bounds() {
        let img = gradient(800, 400);
        // Pane exactly the image size: zoom 2 shows half of each side, centered.
        let (out, (x, y, w, h)) = zoom_crop(&img, 800, 400, View { zoom: 2.0, cx: 0.5, cy: 0.5 });
        assert_eq!((out.width(), out.height()), (800, 400));
        assert!((w - 0.5).abs() < 1e-6 && (h - 0.5).abs() < 1e-6);
        assert!((x - 0.25).abs() < 1e-6 && (y - 0.25).abs() < 1e-6);
        // A center past the corner is clamped so the region stays inside the image.
        let (_, (x, y, w, _)) = zoom_crop(&img, 800, 400, View { zoom: 4.0, cx: 1.5, cy: -1.0 });
        assert!(x + w <= 1.0 + 1e-9 && y >= 0.0 && (x + w - 1.0).abs() < 1e-6 && y.abs() < 1e-6);
        // Extreme zoom on a tiny image never produces an empty crop.
        let (out, _) = zoom_crop(&gradient(3, 2), 640, 480, View { zoom: MAX_ZOOM, cx: 0.0, cy: 1.0 });
        assert!(out.width() > 0 && out.height() > 0);
    }

    /// Zoom through the real worker: the second render shows a cropped region.
    #[test]
    #[allow(deprecated)]
    fn zoom_in_rerenders_cropped_region() {
        let dir = std::env::temp_dir().join(format!("pairdeck-zoom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("big.png");
        gradient(1200, 900).save(&png).unwrap();
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let (tx, rx) = crossbeam_channel::unbounded::<AppEvent>();
        let mut pv = Previewer::new(tx, Some(picker.clone()));
        let loc = Location::local(&dir);
        let entry = crate::vfs::local::stat_local(&png).unwrap();
        let theme = crate::theme::Theme::by_name("dark");
        let cfg = crate::config::Config::default();
        let ctx = crate::ui::Ctx { theme: &theme, cfg: &cfg, classic: false, picker: Some(&picker) };
        let mut term = Terminal::new(TestBackend::new(60, 30)).unwrap();
        let area = Rect::new(0, 0, 60, 30);
        let mut step = |pv: &mut Previewer| {
            term.draw(|f| pv.render(f, area, &loc, Some(&entry), &ctx)).unwrap();
            if let AppEvent::Preview(m) = rx.recv_timeout(std::time::Duration::from_secs(10)).expect("preview") {
                pv.on_msg(m);
            }
            term.draw(|f| pv.render(f, area, &loc, Some(&entry), &ctx)).unwrap();
        };
        step(&mut pv);
        assert!(pv.is_image());
        assert_eq!(pv.region(), (0.0, 0.0, 1.0, 1.0));
        pv.zoom_in();
        pv.zoom_in();
        step(&mut pv);
        let (_, _, w, h) = pv.region();
        assert!(pv.zoom() > 1.5 && w < 0.9 && h < 0.9, "region not cropped: {w} {h}");
        if let Some(Body::Image { caption, .. }) = &pv.body {
            assert!(caption.contains("zoom"), "caption: {caption}");
        }
        // Panning right moves the region right; reset returns to the full image.
        let before = pv.region().0;
        pv.pan(0.5, 0.0);
        step(&mut pv);
        assert!(pv.region().0 > before);
        pv.zoom_reset();
        step(&mut pv);
        assert_eq!(pv.region(), (0.0, 0.0, 1.0, 1.0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod ext_tests;
