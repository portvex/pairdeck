//! Scrollable list chooser with type-to-filter (hotlist, histories, user menu...).

use super::{Dialog, Outcome, centered, draw_box};
use crate::app::App;
use crate::ui::Ctx;
use crate::util::{pad_right, str_width, trunc_mid};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};
use std::any::Any;

#[derive(Clone, Debug)]
pub struct ListItem {
    pub label: String,
    pub hint: String,
    /// Non-selectable header line.
    pub header: bool,
}

impl ListItem {
    pub fn new(label: impl Into<String>) -> Self {
        ListItem { label: label.into(), hint: String::new(), header: false }
    }
    pub fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = h.into();
        self
    }
    #[allow(dead_code)]
    pub fn header(label: impl Into<String>) -> Self {
        ListItem { label: label.into(), hint: String::new(), header: true }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListAction {
    Select(usize),
    Delete(usize),
    Add,
    Edit(usize),
}

type ActionFn = Box<dyn FnOnce(&mut App, ListAction)>;

pub struct ListDialog {
    title: String,
    items: Vec<ListItem>,
    visible: Vec<usize>,
    cursor: usize,
    offset: usize,
    filter: String,
    pub filterable: bool,
    pub allow_delete: bool,
    pub allow_add: bool,
    pub allow_edit: bool,
    footer: String,
    on_action: Option<ActionFn>,
    list_rect: Rect,
}

impl ListDialog {
    pub fn new(title: &str, items: Vec<ListItem>, on_action: impl FnOnce(&mut App, ListAction) + 'static) -> Self {
        let mut d = ListDialog {
            title: title.into(),
            items,
            visible: Vec::new(),
            cursor: 0,
            offset: 0,
            filter: String::new(),
            filterable: true,
            allow_delete: false,
            allow_add: false,
            allow_edit: false,
            footer: String::new(),
            on_action: Some(Box::new(on_action)),
            list_rect: Rect::default(),
        };
        d.refilter();
        d
    }
    #[allow(dead_code)]
    pub fn select(mut self, idx: usize) -> Self {
        if let Some(p) = self.visible.iter().position(|&i| i == idx) {
            self.cursor = p;
        }
        self
    }
    pub fn select_last(mut self) -> Self {
        self.cursor = self.visible.len().saturating_sub(1);
        self
    }
    pub fn footer(mut self, f: &str) -> Self {
        self.footer = f.into();
        self
    }
    pub fn deletable(mut self) -> Self {
        self.allow_delete = true;
        self
    }
    pub fn addable(mut self) -> Self {
        self.allow_add = true;
        self
    }
    pub fn editable(mut self) -> Self {
        self.allow_edit = true;
        self
    }

    fn refilter(&mut self) {
        let f = self.filter.to_lowercase();
        self.visible = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, it)| f.is_empty() || (!it.header && fuzzy(&it.label.to_lowercase(), &f)))
            .map(|(i, _)| i)
            .collect();
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
        self.skip_headers(1);
    }

    fn skip_headers(&mut self, dir: isize) {
        let n = self.visible.len();
        let mut guard = 0;
        while n > 0 && self.items[self.visible[self.cursor]].header && guard < n {
            let c = self.cursor as isize + dir;
            if c < 0 || c >= n as isize {
                break;
            }
            self.cursor = c as usize;
            guard += 1;
        }
    }

    fn act(&mut self, a: impl FnOnce(usize) -> ListAction) -> Outcome {
        let Some(&idx) = self.visible.get(self.cursor) else { return Outcome::Keep };
        if self.items[idx].header {
            return Outcome::Keep;
        }
        let action = a(idx);
        match self.on_action.take() {
            Some(cb) => Outcome::CloseThen(Box::new(move |app| cb(app, action))),
            None => Outcome::Close,
        }
    }
}

/// Subsequence match.
fn fuzzy(hay: &str, needle: &str) -> bool {
    if hay.contains(needle) {
        return true;
    }
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

impl Dialog for ListDialog {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let content_w = self
            .items
            .iter()
            .map(|i| str_width(&i.label) + if i.hint.is_empty() { 0 } else { str_width(&i.hint) + 2 })
            .max()
            .unwrap_or(10)
            .max(str_width(&self.title) + 4)
            .max(str_width(&self.footer)) as u16;
        let w = (content_w + 6).clamp(30, area.width.saturating_sub(4));
        let list_h = (self.items.len() as u16).clamp(1, area.height.saturating_sub(8));
        let extra = if self.filterable { 2 } else { 0 } + if self.footer.is_empty() { 0 } else { 1 };
        let r = centered(area, w, list_h + 2 + extra);
        let inner = draw_box(f, r, &self.title, ctx, false);
        let mut lh = inner.height;
        if self.filterable {
            lh = lh.saturating_sub(2);
        }
        if !self.footer.is_empty() {
            lh = lh.saturating_sub(1);
        }
        let rows = lh as usize;
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if rows > 0 && self.cursor >= self.offset + rows {
            self.offset = self.cursor + 1 - rows;
        }
        self.list_rect = Rect { height: lh, ..inner };
        let width = inner.width as usize;
        let mut lines = Vec::new();
        for (row, &idx) in self.visible.iter().enumerate().skip(self.offset).take(rows) {
            let it = &self.items[idx];
            let selected = row == self.cursor;
            if it.header {
                lines.push(Line::styled(pad_right(&format!("── {} ", it.label), width), ctx.theme.dialog_title));
                continue;
            }
            let hint_w = if it.hint.is_empty() { 0 } else { str_width(&it.hint) + 1 };
            let label = trunc_mid(&it.label, width.saturating_sub(hint_w));
            let gap = width.saturating_sub(str_width(&label) + hint_w);
            let st = if selected { ctx.theme.dialog_focus } else { ctx.theme.dialog };
            lines.push(Line::from(vec![
                Span::styled(format!("{label}{}", " ".repeat(gap)), st),
                Span::styled(
                    if hint_w > 0 { format!(" {}", it.hint) } else { String::new() },
                    st.patch(ctx.theme.menu_hotkey.bg(st.bg.unwrap_or_default())),
                ),
            ]));
        }
        f.render_widget(Paragraph::new(lines), self.list_rect);
        let mut y = inner.y + lh;
        if self.filterable {
            y += 1;
            let txt =
                if self.filter.is_empty() { "type to filter".to_string() } else { format!("filter: {}", self.filter) };
            f.render_widget(Paragraph::new(Span::styled(txt, ctx.theme.dialog_border)), Rect { y, height: 1, ..inner });
            y += 1;
        }
        if !self.footer.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled(self.footer.clone(), ctx.theme.dialog_border)),
                Rect { y, height: 1, ..inner },
            );
        }
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        let n = self.visible.len();
        let page = self.list_rect.height.max(1) as usize;
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Outcome::Close,
            KeyCode::Enter => return self.act(ListAction::Select),
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                self.skip_headers(-1);
            }
            KeyCode::Down => {
                if self.cursor + 1 < n {
                    self.cursor += 1;
                }
                self.skip_headers(1);
            }
            KeyCode::PageUp => {
                self.cursor = self.cursor.saturating_sub(page);
                self.skip_headers(1);
            }
            KeyCode::PageDown => {
                self.cursor = (self.cursor + page).min(n.saturating_sub(1));
                self.skip_headers(-1);
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.skip_headers(1);
            }
            KeyCode::End => {
                self.cursor = n.saturating_sub(1);
                self.skip_headers(-1);
            }
            KeyCode::Delete | KeyCode::F(8) if self.allow_delete => return self.act(ListAction::Delete),
            KeyCode::Insert | KeyCode::F(7) if self.allow_add => {
                return match self.on_action.take() {
                    Some(cb) => Outcome::CloseThen(Box::new(move |app| cb(app, ListAction::Add))),
                    None => Outcome::Close,
                };
            }
            KeyCode::F(4) if self.allow_edit => return self.act(ListAction::Edit),
            KeyCode::Backspace if self.filterable => {
                self.filter.pop();
                self.refilter();
            }
            KeyCode::Char(c)
                if self.filterable && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.filter.push(c);
                self.refilter();
            }
            KeyCode::Char(c) if !self.filterable => {
                // Hotkey: first item whose hint or label starts with c.
                if let Some(p) = self.visible.iter().position(|&i| {
                    let it = &self.items[i];
                    !it.header && (it.hint.eq_ignore_ascii_case(&c.to_string()) || it.label.trim_start().starts_with(c))
                }) {
                    self.cursor = p;
                    return self.act(ListAction::Select);
                }
            }
            _ => {}
        }
        Outcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> Outcome {
        let r = self.list_rect;
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) if r.contains((m.column, m.row).into()) => {
                let idx = self.offset + (m.row - r.y) as usize;
                if idx < self.visible.len() {
                    if idx == self.cursor {
                        return self.act(ListAction::Select);
                    }
                    self.cursor = idx;
                }
            }
            MouseEventKind::ScrollUp => self.cursor = self.cursor.saturating_sub(3),
            MouseEventKind::ScrollDown => self.cursor = (self.cursor + 3).min(self.visible.len().saturating_sub(1)),
            _ => {}
        }
        Outcome::Keep
    }

    fn paste(&mut self, text: &str) {
        if self.filterable {
            self.filter.push_str(text);
            self.refilter();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
