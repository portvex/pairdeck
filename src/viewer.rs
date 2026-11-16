//! F3 internal viewer: text (with syntax highlighting), hex dump and images.

use crate::app::App;
use crate::dialog::{self, Form};
use crate::screen::{Screen, ScreenOutcome};
use crate::ui::Ctx;
use crate::util::{grouped, human_size, pad_right, str_width, trunc_left};
use crate::vfs::Location;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Rect, Size},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use ratatui_image::{Image, Resize, protocol::Protocol};
use std::any::Any;
use std::path::Path;
use unicode_width::UnicodeWidthChar;

/// Maximum number of bytes loaded into the viewer.
const MAX_LOAD: usize = 64 * 1024 * 1024;
/// Highlighting is skipped for very large texts.
const MAX_HIGHLIGHT_BYTES: usize = 4 * 1024 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 60_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Text,
    Hex,
    Image,
}

#[derive(Clone, Debug, Default)]
struct Search {
    pattern: String,
    regex: bool,
    case: bool,
    backwards: bool,
}

struct ImageState {
    img: image::DynamicImage,
    format: String,
    scale: bool,
    cache: Option<(Rect, bool, Protocol)>,
    error: Option<String>,
    /// Zoom / pan (same model as the quick view preview).
    view: crate::preview::View,
    /// Visible region of the last render as fractions of the image.
    region: (f64, f64, f64, f64),
    /// View the cached protocol was made for.
    cached_view: crate::preview::View,
    /// Screen rect of the image drawn last frame.
    rect: Rect,
    drag: Option<(u16, u16)>,
    last_click: Option<(std::time::Instant, u16, u16)>,
}

impl ImageState {
    fn new(img: image::DynamicImage, format: String) -> Self {
        ImageState {
            img,
            format,
            scale: false,
            cache: None,
            error: None,
            view: crate::preview::View::default(),
            region: (0.0, 0.0, 1.0, 1.0),
            cached_view: crate::preview::View::default(),
            rect: Rect::default(),
            drag: None,
            last_click: None,
        }
    }

    fn zoom_at(&mut self, factor: f64, fx: f64, fy: f64) {
        self.view = crate::preview::zoom_view(self.view, self.region, factor, fx, fy);
    }

    fn pan(&mut self, dx: f64, dy: f64) {
        self.view = crate::preview::pan_view(self.view, self.region, dx, dy);
    }

    fn frac(&self, col: u16, row: u16) -> (f64, f64) {
        let r = self.rect;
        (
            (col.saturating_sub(r.x) as f64 + 0.5) / r.width.max(1) as f64,
            (row.saturating_sub(r.y) as f64 + 0.5) / r.height.max(1) as f64,
        )
    }
}

pub struct Viewer {
    name: String,
    display: String,
    data: Vec<u8>,
    total_size: u64,
    truncated: bool,
    mode: Mode,
    /// The mode chosen when the file was opened (Image or Text), used by the F4 toggle.
    natural: Mode,
    /// SVG file: shown rendered, `t` toggles the XML source.
    svg: bool,
    wrap: bool,
    line_numbers: bool,
    tab: usize,
    lines: Vec<String>,
    hl: Option<Vec<Line<'static>>>,
    hl_theme: String,
    hl_tried: bool,
    /// Text mode: first visible logical line. Hex mode: first visible row.
    top: usize,
    /// Horizontal scroll (display columns) when not wrapping.
    left: usize,
    search: Option<Search>,
    re: Option<regex::Regex>,
    bre: Option<regex::bytes::Regex>,
    /// Current match: text (line, byte start, byte end) or hex (offset, len) stored as (0, start, end).
    cur_match: Option<(usize, usize, usize)>,
    message: Option<String>,
    height: usize,
    hex_bpr: usize,
    image: Option<ImageState>,
}

pub fn open(app: &mut App, loc: &Location, path: &Path) {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    let display = loc.vfs.lock().display(path);
    let total_size = loc.vfs.lock().stat(path).map(|e| e.size).unwrap_or(0);
    let data = match loc.vfs.lock().read_head(path, MAX_LOAD) {
        Ok(d) => d,
        Err(e) => {
            app.error(format!("Cannot open {display}:\n{e:#}"));
            return;
        }
    };
    let v = Viewer::new(name, display, data, total_size, app.cfg.viewer_wrap, app.cfg.tab_size.max(1));
    app.push_screen(Box::new(v));
}

/// Show command output (git log, blame...) in the viewer. `name` picks the
/// syntax highlighting (e.g. "history.gitlog", "changes.diff").
pub fn open_text(app: &mut App, title: &str, name: &str, data: Vec<u8>) {
    let len = data.len() as u64;
    let v = Viewer::new(name.to_string(), title.to_string(), data, len, false, app.cfg.tab_size.max(1));
    app.push_screen(Box::new(v));
}

fn expand_tabs(s: &str, tab: usize) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0;
    for c in s.chars() {
        if c == '\t' {
            let n = tab - col % tab;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += c.width().unwrap_or(0);
        }
    }
    out
}

/// Replace control characters (other than tab) with visible placeholders.
fn sanitize(s: &str) -> String {
    if !s.chars().any(|c| c.is_control() && c != '\t') {
        return s.to_string();
    }
    s.chars().map(|c| if c.is_control() && c != '\t' { '.' } else { c }).collect()
}

impl Viewer {
    fn new(name: String, display: String, data: Vec<u8>, total_size: u64, wrap: bool, tab: usize) -> Viewer {
        let truncated = (data.len() as u64) < total_size;
        let ext = crate::icons::ext_of(&name);
        let mut v = Viewer {
            name,
            display,
            data,
            total_size,
            truncated,
            mode: Mode::Text,
            natural: Mode::Text,
            wrap,
            line_numbers: false,
            tab,
            lines: Vec::new(),
            hl: None,
            hl_theme: String::new(),
            hl_tried: false,
            top: 0,
            left: 0,
            search: None,
            re: None,
            bre: None,
            cur_match: None,
            message: None,
            height: 20,
            hex_bpr: 16,
            image: None,
            svg: false,
        };
        if crate::svg::is_svg_name(&v.name) {
            match crate::svg::rasterize(&v.data, crate::svg::MAX_SIDE) {
                Ok(r) => {
                    let format = format!("SVG {}x{} (t: source)", r.width.round(), r.height.round());
                    v.image = Some(ImageState::new(r.image, format));
                    v.mode = Mode::Image;
                    v.natural = Mode::Image;
                    v.svg = true;
                }
                Err(e) => v.message = Some(format!("Cannot render SVG: {e:#}")),
            }
        } else if crate::icons::IMAGE_EXTS.contains(&ext.as_str()) {
            match image::load_from_memory(&v.data) {
                Ok(img) => {
                    let format =
                        image::guess_format(&v.data).map(|f| format!("{f:?}")).unwrap_or_else(|_| ext.to_uppercase());
                    v.image = Some(ImageState::new(img, format));
                    v.mode = Mode::Image;
                    v.natural = Mode::Image;
                }
                Err(e) => v.message = Some(format!("Cannot decode image: {e}")),
            }
        }
        if v.mode != Mode::Image && crate::util::looks_binary(&v.data) {
            v.mode = Mode::Hex;
            v.natural = Mode::Hex;
        }
        v.build_lines();
        if v.truncated {
            v.message = Some(format!(
                "File truncated: showing the first {} of {}",
                human_size(v.data.len() as u64),
                human_size(total_size)
            ));
        }
        v
    }

    fn build_lines(&mut self) {
        let text = String::from_utf8_lossy(&self.data);
        let mut lines: Vec<String> =
            text.split('\n').map(|l| sanitize(&expand_tabs(l.strip_suffix('\r').unwrap_or(l), self.tab))).collect();
        if lines.len() > 1 && lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        self.lines = lines;
    }

    fn ensure_highlight(&mut self, theme: &str) {
        if self.hl_tried && self.hl_theme == theme {
            return;
        }
        self.hl_tried = true;
        self.hl_theme = theme.to_string();
        self.hl = None;
        if self.data.len() > MAX_HIGHLIGHT_BYTES || crate::util::looks_binary(&self.data) {
            return;
        }
        let text = self.lines.join("\n");
        self.hl = crate::highlight::highlight(&text, &self.name, theme, MAX_HIGHLIGHT_LINES, self.tab);
    }

    fn rows_total(&self) -> usize {
        match self.mode {
            Mode::Text => self.lines.len(),
            Mode::Hex => self.data.len().div_ceil(self.hex_bpr).max(1),
            Mode::Image => 1,
        }
    }

    fn max_top(&self) -> usize {
        self.rows_total().saturating_sub(self.height.max(1))
    }

    fn scroll(&mut self, d: isize) {
        let t = self.top as isize + d;
        self.top = t.clamp(0, self.max_top() as isize) as usize;
    }

    fn percent(&self) -> usize {
        let total = self.rows_total();
        if total <= self.height {
            return 100;
        }
        ((self.top + self.height).min(total) * 100) / total
    }

    // ------------------------------------------------------------ search

    fn set_search(&mut self, s: Search) -> Result<(), String> {
        let mut pat = if s.regex { s.pattern.clone() } else { regex::escape(&s.pattern) };
        if !s.case {
            pat = format!("(?i){pat}");
        }
        self.re = Some(regex::Regex::new(&pat).map_err(|e| e.to_string())?);
        self.bre = Some(regex::bytes::Regex::new(&pat).map_err(|e| e.to_string())?);
        self.search = Some(s);
        self.cur_match = None;
        Ok(())
    }

    fn find(&mut self, backwards: bool) {
        let Some(s) = self.search.clone() else {
            self.message = Some("No search pattern (F7)".into());
            return;
        };
        let back = s.backwards ^ backwards;
        let found = match self.mode {
            Mode::Hex => self.find_hex(back),
            _ => self.find_text(back),
        };
        if !found {
            self.message = Some(format!("Search string not found: {}", s.pattern));
        } else {
            self.message = None;
        }
    }

    fn find_text(&mut self, back: bool) -> bool {
        let Some(re) = self.re.clone() else { return false };
        let n = self.lines.len();
        if n == 0 {
            return false;
        }
        let (start_line, start_byte) = match self.cur_match {
            Some((l, s, e)) => {
                if back {
                    (l, s)
                } else {
                    (l, e.max(s + 1))
                }
            }
            None => (self.top, 0),
        };
        if !back {
            for i in 0..=n {
                let li = (start_line + i) % n;
                let line = &self.lines[li];
                let from = if i == 0 { start_byte.min(line.len()) } else { 0 };
                if i == n && from == 0 {
                    break;
                }
                if !line.is_char_boundary(from) {
                    continue;
                }
                if let Some(m) = re.find_at(line, from) {
                    if i == 0 && m.start() < from {
                        continue;
                    }
                    self.cur_match = Some((li, m.start(), m.end()));
                    self.reveal(li);
                    return true;
                }
            }
        } else {
            for i in 0..=n {
                let li = (start_line + n - i) % n;
                let line = &self.lines[li];
                let limit = if i == 0 { start_byte } else { usize::MAX };
                let last = re.find_iter(line).filter(|m| m.start() < limit).last();
                if let Some(m) = last {
                    self.cur_match = Some((li, m.start(), m.end()));
                    self.reveal(li);
                    return true;
                }
            }
        }
        false
    }

    fn find_hex(&mut self, back: bool) -> bool {
        let Some(re) = self.bre.clone() else { return false };
        let from = match self.cur_match {
            Some((_, s, e)) => {
                if back {
                    s
                } else {
                    e.max(s + 1)
                }
            }
            None => self.top * self.hex_bpr,
        };
        let m = if back {
            re.find_iter(&self.data).filter(|m| m.start() < from).last().or_else(|| re.find_iter(&self.data).last())
        } else {
            re.find_at(&self.data, from.min(self.data.len())).or_else(|| re.find(&self.data))
        };
        if let Some(m) = m {
            self.cur_match = Some((0, m.start(), m.end()));
            let row = m.start() / self.hex_bpr;
            self.reveal(row);
            return true;
        }
        false
    }

    fn reveal(&mut self, row: usize) {
        let h = self.height.max(1);
        if row < self.top || row >= self.top + h {
            self.top = row.saturating_sub(h / 3);
        }
        self.top = self.top.min(self.max_top());
        if !self.wrap
            && self.mode == Mode::Text
            && let Some((_, s, _)) = self.cur_match
        {
            let col = str_width(&self.lines[row][..s.min(self.lines[row].len())]);
            if col < self.left || col > self.left + 60 {
                self.left = col.saturating_sub(10);
            }
        }
    }

    fn goto(&mut self, s: &str) {
        let s = s.trim();
        match self.mode {
            Mode::Hex => {
                let off = if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                    usize::from_str_radix(h, 16).ok()
                } else if let Some(p) = s.strip_suffix('%') {
                    p.parse::<usize>().ok().map(|p| self.data.len() * p.min(100) / 100)
                } else {
                    s.parse().ok()
                };
                match off {
                    Some(o) => {
                        self.top = (o.min(self.data.len()) / self.hex_bpr).min(self.max_top());
                    }
                    None => self.message = Some(format!("Bad offset: {s}")),
                }
            }
            _ => {
                let line = if let Some(p) = s.strip_suffix('%') {
                    p.parse::<usize>().ok().map(|p| self.lines.len() * p.min(100) / 100 + 1)
                } else {
                    s.parse::<usize>().ok()
                };
                match line {
                    Some(l) => self.top = l.saturating_sub(1).min(self.max_top()),
                    None => self.message = Some(format!("Bad line number: {s}")),
                }
            }
        }
    }

    fn toggle_hex(&mut self) {
        self.mode = if self.mode == Mode::Hex {
            if self.natural == Mode::Image { Mode::Image } else { Mode::Text }
        } else {
            Mode::Hex
        };
        self.top = 0;
        self.cur_match = None;
    }

    // ------------------------------------------------------------ rendering

    fn render_text(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        self.ensure_highlight(&ctx.theme.syntax);
        let base = ctx.theme.viewer;
        let gutter = if self.line_numbers { self.lines.len().to_string().len() + 1 } else { 0 };
        let width = (area.width as usize).saturating_sub(gutter).max(1);
        let mut out: Vec<Line> = Vec::with_capacity(area.height as usize);
        let rows = area.height as usize;
        let mut li = self.top;
        while out.len() < rows && li < self.lines.len() {
            let cells = self.line_cells(li, base, ctx);
            let segments: Vec<Vec<(char, Style)>> =
                if self.wrap { wrap_cells(&cells, width) } else { vec![skip_cols(&cells, self.left, width)] };
            for (k, seg) in segments.into_iter().enumerate() {
                if out.len() >= rows {
                    break;
                }
                let mut spans = Vec::new();
                if gutter > 0 {
                    let label = if k == 0 { format!("{:>w$} ", li + 1, w = gutter - 1) } else { " ".repeat(gutter) };
                    spans.push(Span::styled(label, ctx.theme.editor_line_nr));
                }
                spans.extend(cells_to_spans(&seg));
                out.push(Line::from(spans));
            }
            li += 1;
        }
        f.render_widget(Paragraph::new(out).style(base), area);
    }

    /// Characters of a line with their styles (syntax + search highlights).
    fn line_cells(&self, li: usize, base: Style, ctx: &Ctx) -> Vec<(char, Style)> {
        let line = &self.lines[li];
        let mut cells: Vec<(char, Style)> = Vec::with_capacity(line.len());
        match self.hl.as_ref().and_then(|h| h.get(li)) {
            Some(hl) => {
                for sp in &hl.spans {
                    let st = base.patch(sp.style);
                    for c in sp.content.chars() {
                        cells.push((if c.is_control() { '.' } else { c }, st));
                    }
                }
            }
            None => cells.extend(line.chars().map(|c| (c, base))),
        }
        if let Some(re) = &self.re {
            // Map byte offsets of matches to char indices.
            let mut char_starts = Vec::with_capacity(line.len());
            for (b, _) in line.char_indices() {
                char_starts.push(b);
            }
            for m in re.find_iter(line) {
                if m.start() == m.end() {
                    continue;
                }
                let current = self.cur_match == Some((li, m.start(), m.end()));
                let st = if current {
                    ctx.theme.search_match.add_modifier(ratatui::style::Modifier::REVERSED)
                } else {
                    ctx.theme.search_match
                };
                for (ci, b) in char_starts.iter().enumerate() {
                    if *b >= m.start() && *b < m.end() && ci < cells.len() {
                        cells[ci].1 = st;
                    }
                }
            }
        }
        cells
    }

    fn render_hex(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let w = area.width as usize;
        // offset(8) + 2 + bpr*3 + group gaps + 1 + bpr ascii
        self.hex_bpr = if w >= 8 + 2 + 32 * 3 + 4 + 2 + 32 {
            32
        } else if w >= 8 + 2 + 16 * 3 + 2 + 2 + 16 {
            16
        } else {
            8
        };
        let bpr = self.hex_bpr;
        let base = ctx.theme.viewer;
        let dim = ctx.theme.editor_line_nr;
        let matched = |off: usize| matches!(self.cur_match, Some((_, s, e)) if off >= s && off < e);
        let mut out = Vec::new();
        for row in self.top..(self.top + area.height as usize) {
            let start = row * bpr;
            if start >= self.data.len() && !(row == 0 && self.data.is_empty()) {
                break;
            }
            let chunk = &self.data[start..(start + bpr).min(self.data.len())];
            let mut spans = vec![Span::styled(format!("{start:08X}  "), dim)];
            for i in 0..bpr {
                if i > 0 && i % 8 == 0 {
                    spans.push(Span::styled(" ", base));
                }
                match chunk.get(i) {
                    Some(b) => {
                        let st = if matched(start + i) {
                            ctx.theme.search_match
                        } else if *b == 0 {
                            dim
                        } else {
                            base
                        };
                        spans.push(Span::styled(format!("{b:02X}"), st));
                        spans.push(Span::styled(" ", base));
                    }
                    None => spans.push(Span::styled("   ", base)),
                }
            }
            spans.push(Span::styled(" ", base));
            for (i, b) in chunk.iter().enumerate() {
                let c = if b.is_ascii_graphic() || *b == b' ' { *b as char } else { '.' };
                let st = if matched(start + i) {
                    ctx.theme.search_match
                } else if c == '.' && !b.is_ascii_punctuation() {
                    dim
                } else {
                    base
                };
                spans.push(Span::styled(c.to_string(), st));
            }
            out.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(out).style(base), area);
    }

    fn render_image(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        f.render_widget(Paragraph::new("").style(ctx.theme.viewer), area);
        let fallback;
        let picker = match ctx.picker {
            Some(p) => p,
            None => {
                fallback = ratatui_image::picker::Picker::halfblocks();
                &fallback
            }
        };
        let Some(st) = self.image.as_mut() else { return };
        let need = match &st.cache {
            Some((r, scale, _)) => *r != area || *scale != st.scale || st.cached_view != st.view,
            None => true,
        };
        if need && st.error.is_none() && st.view.zoom > 1.0 {
            // Zoomed: crop the visible region to the screen shape (shared with the preview).
            let fs = picker.font_size();
            let (pw, ph) = (area.width as u32 * fs.width.max(1) as u32, area.height as u32 * fs.height.max(1) as u32);
            let (img, region) = crate::preview::zoom_crop(&st.img, pw, ph, st.view);
            match picker.new_protocol(img, Size::new(area.width, area.height), Resize::Fit(None)) {
                Ok(p) => {
                    st.cache = Some((area, st.scale, p));
                    st.region = region;
                    st.cached_view = st.view;
                }
                Err(e) => st.error = Some(e.to_string()),
            }
        } else if need && st.error.is_none() {
            st.region = (0.0, 0.0, 1.0, 1.0);
            st.cached_view = st.view;
            let resize = if st.scale {
                Resize::Scale(Some(image::imageops::FilterType::Triangle))
            } else {
                Resize::Fit(Some(image::imageops::FilterType::Triangle))
            };
            match picker.new_protocol(st.img.clone(), Size::new(area.width, area.height), resize) {
                Ok(p) => st.cache = Some((area, st.scale, p)),
                Err(e) => st.error = Some(e.to_string()),
            }
        }
        if let Some(err) = &st.error {
            f.render_widget(Paragraph::new(format!("Cannot render image: {err}")).style(ctx.theme.error), area);
            return;
        }
        if let Some((_, _, proto)) = &st.cache {
            let sz = proto.size();
            let x = area.x + area.width.saturating_sub(sz.width) / 2;
            let y = area.y + area.height.saturating_sub(sz.height) / 2;
            let r = Rect { x, y, width: sz.width.min(area.width), height: sz.height.min(area.height) };
            f.render_widget(Image::new(proto).allow_clipping(true), r);
            crate::kitty_cells::complete_placeholders(f.buffer_mut(), r);
            st.rect = r;
        }
    }

    /// Zoom / pan keys in image mode. Returns true if used.
    fn image_key(&mut self, code: KeyCode) -> bool {
        if self.mode != Mode::Image {
            return false;
        }
        let Some(st) = self.image.as_mut() else { return false };
        let zoomed = st.view.zoom > 1.0;
        let step = crate::preview::ZOOM_STEP_PUB;
        match code {
            KeyCode::Char('+') | KeyCode::Char('=') => st.zoom_at(step, 0.5, 0.5),
            KeyCode::Char('-') | KeyCode::Char('_') => st.zoom_at(1.0 / step, 0.5, 0.5),
            KeyCode::Char('0') => st.view = crate::preview::View::default(),
            KeyCode::Left | KeyCode::Char('h') if zoomed => st.pan(-0.2, 0.0),
            KeyCode::Right | KeyCode::Char('l') if zoomed => st.pan(0.2, 0.0),
            KeyCode::Up | KeyCode::Char('k') if zoomed => st.pan(0.0, -0.2),
            KeyCode::Down | KeyCode::Char('j') if zoomed => st.pan(0.0, 0.2),
            _ => return false,
        }
        true
    }

    /// Mouse zoom / pan in image mode: wheel toward the pointer, drag, double-click.
    fn image_mouse(&mut self, m: MouseEvent) -> bool {
        if self.mode != Mode::Image {
            return false;
        }
        let Some(st) = self.image.as_mut() else { return false };
        let inside = st.rect.width > 0 && st.rect.contains((m.column, m.row).into());
        let step = crate::preview::ZOOM_STEP_PUB;
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let (fx, fy) = if inside { st.frac(m.column, m.row) } else { (0.5, 0.5) };
                let f = if matches!(m.kind, MouseEventKind::ScrollUp) { step } else { 1.0 / step };
                st.zoom_at(f, fx, fy);
                true
            }
            MouseEventKind::Down(MouseButton::Left) if inside => {
                let double = st.last_click.is_some_and(|(t, x, y)| {
                    t.elapsed() < std::time::Duration::from_millis(400) && x == m.column && y == m.row
                });
                if double {
                    st.last_click = None;
                    st.drag = None;
                    if st.view.zoom > 1.0 {
                        st.view = crate::preview::View::default();
                    } else {
                        let (fx, fy) = st.frac(m.column, m.row);
                        st.zoom_at(2.0, fx, fy);
                    }
                } else {
                    st.last_click = Some((std::time::Instant::now(), m.column, m.row));
                    st.drag = Some((m.column, m.row));
                }
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some((px, py)) = st.drag else { return false };
                let dx = (px as f64 - m.column as f64) / st.rect.width.max(1) as f64;
                let dy = (py as f64 - m.row as f64) / st.rect.height.max(1) as f64;
                st.drag = Some((m.column, m.row));
                st.pan(dx, dy);
                true
            }
            MouseEventKind::Up(_) => st.drag.take().is_some(),
            _ => false,
        }
    }

    fn status_line(&self) -> String {
        let mode = match self.mode {
            Mode::Text => {
                let mut m = String::from("text");
                if self.wrap {
                    m.push_str(" wrap");
                }
                if self.hl.is_some() {
                    m.push_str(" syntax");
                }
                m
            }
            Mode::Hex => "hex".into(),
            Mode::Image => "image".into(),
        };
        let pos = match self.mode {
            Mode::Text => format!("Line {}/{}", (self.top + 1).min(self.lines.len().max(1)), self.lines.len()),
            Mode::Hex => format!("Offset 0x{:08X}/0x{:X}", self.top * self.hex_bpr, self.data.len()),
            Mode::Image => match &self.image {
                Some(st) => format!(
                    "{}x{} {}{}",
                    st.img.width(),
                    st.img.height(),
                    st.format,
                    if st.scale { " (scaled)" } else { "" }
                ),
                None => String::new(),
            },
        };
        format!("{}  {}  {} bytes  {}%  [{}]", self.display, pos, grouped(self.total_size), self.percent(), mode)
    }

    fn search_dialog(&self, backwards: bool) -> ScreenOutcome {
        let prev = self.search.clone().unwrap_or_default();
        ScreenOutcome::Then(Box::new(move |app: &mut App| {
            let hist = app.state.input_hist("search").clone();
            let d = Form::new("Search")
                .input_hist("p", "Search for:", &prev.pattern, "search", hist)
                .check("regex", "Regular expression", prev.regex)
                .check("case", "Case sensitive", prev.case)
                .check("back", "Backwards", backwards || prev.backwards)
                .buttons(&["OK", "Cancel"], 0)
                .on_submit(|app, r| {
                    if r.button != 0 || r.text("p").is_empty() {
                        return;
                    }
                    let s = Search {
                        pattern: r.text("p"),
                        regex: r.check("regex"),
                        case: r.check("case"),
                        backwards: r.check("back"),
                    };
                    if let Some(v) = app.top_screen::<Viewer>() {
                        match v.set_search(s) {
                            Ok(()) => v.find(false),
                            Err(e) => v.message = Some(format!("Bad regex: {e}")),
                        }
                    }
                });
            app.push_dialog(Box::new(d));
        }))
    }
}

fn wrap_cells(cells: &[(char, Style)], width: usize) -> Vec<Vec<(char, Style)>> {
    let mut out = vec![Vec::new()];
    let mut w = 0;
    for &(c, st) in cells {
        let cw = c.width().unwrap_or(0);
        if w + cw > width && w > 0 {
            out.push(Vec::new());
            w = 0;
        }
        out.last_mut().unwrap().push((c, st));
        w += cw;
    }
    out
}

fn skip_cols(cells: &[(char, Style)], left: usize, width: usize) -> Vec<(char, Style)> {
    let mut out = Vec::new();
    let mut col = 0;
    for &(c, st) in cells {
        let cw = c.width().unwrap_or(0);
        if col >= left && col + cw <= left + width {
            out.push((c, st));
        }
        col += cw;
        if col >= left + width {
            break;
        }
    }
    out
}

fn cells_to_spans(cells: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut cur = String::new();
    let mut cur_st: Option<Style> = None;
    for &(c, st) in cells {
        if cur_st != Some(st) {
            if let Some(s) = cur_st {
                spans.push(Span::styled(std::mem::take(&mut cur), s));
            }
            cur_st = Some(st);
        }
        cur.push(c);
    }
    if let Some(s) = cur_st {
        spans.push(Span::styled(cur, s));
    }
    spans
}

/// Draw an MC style function key bar.
pub fn fkey_bar(f: &mut Frame, r: Rect, ctx: &Ctx, labels: &[&str; 10]) {
    let n = 10u16;
    let slot = (r.width / n).max(1);
    let mut spans = Vec::new();
    for i in 0..n {
        let num = format!("{}", i + 1);
        let w = if i == n - 1 { r.width.saturating_sub(slot * (n - 1)) } else { slot };
        let lw = (w as usize).saturating_sub(num.len());
        let label: String = labels[i as usize].chars().take(lw).collect();
        spans.push(Span::styled(num, ctx.theme.button_key));
        spans.push(Span::styled(pad_right(&label, lw), ctx.theme.button_label));
        crate::ui::add_hit(
            Rect { x: r.x + i * slot, y: r.y, width: w, height: 1 },
            crate::ui::Hit::Key(crossterm::event::KeyCode::F((i + 1) as u8)),
        );
    }
    f.render_widget(Paragraph::new(Line::from(spans)), r);
}

const HELP: &str = "\
Arrows, PgUp/PgDn, Home/End    scroll (g/G top/bottom)
F2 / w        toggle wrap
F4 / h        toggle hex mode
F5 / :        go to line (hex: offset, 0x.. or N%)
F6 / l        toggle line numbers
F7 / /        search forward      ?  search backward
n / N         next / previous match
z             image: toggle fit / scale to screen
F3 F10 q Esc  quit";

impl Screen for Viewer {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        if area.height < 3 {
            return;
        }
        let status = Rect { height: 1, ..area };
        let bar = Rect { y: area.bottom() - 1, height: 1, ..area };
        let body = Rect { y: area.y + 1, height: area.height - 2, ..area };
        self.height = body.height as usize;
        if self.mode != Mode::Image {
            self.top = self.top.min(self.max_top());
        }
        match self.mode {
            Mode::Text => self.render_text(f, body, ctx),
            Mode::Hex => self.render_hex(f, body, ctx),
            Mode::Image => self.render_image(f, body, ctx),
        }
        let w = area.width as usize;
        let text = match &self.message {
            Some(m) => {
                let s = self.status_line();
                let room = w.saturating_sub(str_width(m) + 3);
                format!("{}  | {m}", trunc_left(&s, room))
            }
            None => trunc_left(&self.status_line(), w),
        };
        f.render_widget(Paragraph::new(Span::styled(pad_right(&text, w), ctx.theme.status)), status);
        let wrap_label = if self.wrap { "UnWrap" } else { "Wrap" };
        let hex_label = if self.mode == Mode::Hex { "Ascii" } else { "Hex" };
        let ln_label = if self.line_numbers { "NoNum" } else { "LineNr" };
        fkey_bar(f, bar, ctx, &["Help", wrap_label, "Quit", hex_label, "Goto", ln_label, "Search", "", "", "Quit"]);
    }

    fn key(&mut self, key: KeyEvent) -> ScreenOutcome {
        let page = self.height.max(1) as isize;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if !matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N')) {
            self.message = None;
        }
        if !ctrl && self.image_key(key.code) {
            return ScreenOutcome::Keep;
        }
        match key.code {
            KeyCode::Esc | KeyCode::F(10) | KeyCode::F(3) | KeyCode::Char('q') => return ScreenOutcome::Close,
            KeyCode::F(1) => {
                return ScreenOutcome::Then(Box::new(|app| app.push_dialog(dialog::message("Viewer keys", HELP))));
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Enter => self.scroll(1),
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll(page),
            KeyCode::Char('u') if ctrl => self.scroll(-page / 2),
            KeyCode::Char('d') if ctrl => self.scroll(page / 2),
            KeyCode::Home | KeyCode::Char('g') => {
                self.top = 0;
                self.left = 0;
            }
            KeyCode::End | KeyCode::Char('G') => self.top = self.max_top(),
            KeyCode::Left if !self.wrap => self.left = self.left.saturating_sub(8),
            KeyCode::Right if !self.wrap => self.left += 8,
            KeyCode::F(2) | KeyCode::Char('w') => {
                self.wrap = !self.wrap;
                self.left = 0;
            }
            KeyCode::F(4) | KeyCode::Char('h') => self.toggle_hex(),
            KeyCode::Char('t') if self.svg => {
                self.mode = if self.mode == Mode::Image { Mode::Text } else { Mode::Image };
                self.top = 0;
                self.cur_match = None;
            }
            KeyCode::F(6) | KeyCode::Char('l') => self.line_numbers = !self.line_numbers,
            KeyCode::Char('z') => {
                if let Some(st) = self.image.as_mut() {
                    st.scale = !st.scale;
                }
            }
            KeyCode::F(7) | KeyCode::Char('/') => return self.search_dialog(false),
            KeyCode::Char('?') => return self.search_dialog(true),
            KeyCode::Char('n') | KeyCode::F(17) => self.find(false),
            KeyCode::Char('N') => self.find(true),
            KeyCode::F(5) | KeyCode::Char(':') => {
                let label =
                    if self.mode == Mode::Hex { "Offset (decimal, 0x hex or N%):" } else { "Line number (or N%):" };
                let label = label.to_string();
                return ScreenOutcome::Then(Box::new(move |app: &mut App| {
                    let d = dialog::input(app, "Goto", &label, "", "viewer_goto", |app, s| {
                        if let Some(v) = app.top_screen::<Viewer>() {
                            v.goto(&s);
                        }
                    });
                    app.push_dialog(d);
                }));
            }
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> ScreenOutcome {
        if self.image_mouse(m) {
            return ScreenOutcome::Keep;
        }
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll(-3),
            MouseEventKind::ScrollDown => self.scroll(3),
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn title(&self) -> String {
        format!("View: {}", self.name)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
