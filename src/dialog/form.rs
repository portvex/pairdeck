//! Generic form dialog: text, inputs, checkboxes, radio groups and buttons.

use super::{AppFn, Dialog, Outcome, centered, draw_box};
use crate::app::App;
use crate::lineedit::LineEdit;
use crate::ui::Ctx;
use crate::util::str_width;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use std::any::Any;
use std::collections::HashMap;

pub enum Field {
    Text(String),
    Input { label: String, edit: LineEdit, hist: Option<String> },
    Check { label: String, value: bool },
    Radio { label: String, options: Vec<String>, selected: usize },
    Buttons { labels: Vec<String>, default: usize },
    Separator,
}

#[derive(Debug, Default, Clone)]
pub struct FormResult {
    pub button: usize,
    pub texts: HashMap<String, String>,
    pub checks: HashMap<String, bool>,
    pub radios: HashMap<String, usize>,
}

impl FormResult {
    pub fn text(&self, id: &str) -> String {
        self.texts.get(id).cloned().unwrap_or_default()
    }
    pub fn check(&self, id: &str) -> bool {
        self.checks.get(id).copied().unwrap_or(false)
    }
    pub fn radio(&self, id: &str) -> usize {
        self.radios.get(id).copied().unwrap_or(0)
    }
}

type SubmitFn = Box<dyn FnOnce(&mut App, FormResult)>;

/// A focusable slot: (field index, sub index for radio options / buttons).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Slot(usize, usize);

pub struct Form {
    title: String,
    fields: Vec<(String, Field)>,
    focus: usize,
    width: u16,
    error: bool,
    on_submit: Option<SubmitFn>,
    on_cancel: Option<AppFn>,
    /// Screen rects of slots from the last render, for mouse clicks.
    hit: Vec<(Rect, usize)>,
}

impl Form {
    pub fn new(title: &str) -> Form {
        Form {
            title: title.into(),
            fields: Vec::new(),
            focus: 0,
            width: 0,
            error: false,
            on_submit: None,
            on_cancel: None,
            hit: Vec::new(),
        }
    }
    pub fn width(mut self, w: u16) -> Self {
        self.width = w;
        self
    }
    pub fn error(mut self) -> Self {
        self.error = true;
        self
    }
    pub fn text(mut self, t: &str) -> Self {
        self.fields.push((String::new(), Field::Text(t.into())));
        self
    }
    pub fn separator(mut self) -> Self {
        self.fields.push((String::new(), Field::Separator));
        self
    }
    pub fn input(mut self, id: &str, label: &str, value: &str) -> Self {
        self.fields.push((id.into(), Field::Input { label: label.into(), edit: LineEdit::new(value), hist: None }));
        self
    }
    pub fn password(mut self, id: &str, label: &str) -> Self {
        let mut e = LineEdit::new("");
        e.masked = true;
        self.fields.push((id.into(), Field::Input { label: label.into(), edit: e, hist: None }));
        self
    }
    pub fn input_hist(mut self, id: &str, label: &str, value: &str, key: &str, hist: Vec<String>) -> Self {
        let mut e = LineEdit::new(value).with_history(hist);
        e.select_all = !value.is_empty();
        self.fields.push((id.into(), Field::Input { label: label.into(), edit: e, hist: Some(key.into()) }));
        self
    }
    /// Input whose cursor starts at `cursor` with no preselection (rename dialogs).
    pub fn input_at(mut self, id: &str, label: &str, value: &str, cursor: usize) -> Self {
        let mut e = LineEdit::new(value);
        e.cursor_to(cursor);
        self.fields.push((id.into(), Field::Input { label: label.into(), edit: e, hist: None }));
        self
    }
    pub fn check(mut self, id: &str, label: &str, value: bool) -> Self {
        self.fields.push((id.into(), Field::Check { label: label.into(), value }));
        self
    }
    pub fn radio(mut self, id: &str, label: &str, options: &[&str], selected: usize) -> Self {
        self.fields.push((
            id.into(),
            Field::Radio { label: label.into(), options: options.iter().map(|s| s.to_string()).collect(), selected },
        ));
        self
    }
    pub fn buttons(mut self, labels: &[&str], default: usize) -> Self {
        self.fields
            .push((String::new(), Field::Buttons { labels: labels.iter().map(|s| s.to_string()).collect(), default }));
        self.focus = self.first_focus();
        self
    }
    pub fn on_submit(mut self, f: impl FnOnce(&mut App, FormResult) + 'static) -> Self {
        self.on_submit = Some(Box::new(f));
        self
    }
    pub fn on_cancel(mut self, f: impl FnOnce(&mut App) + 'static) -> Self {
        self.on_cancel = Some(Box::new(f));
        self
    }

    fn slots(&self) -> Vec<Slot> {
        let mut v = Vec::new();
        for (i, (_, f)) in self.fields.iter().enumerate() {
            match f {
                Field::Input { .. } | Field::Check { .. } => v.push(Slot(i, 0)),
                Field::Radio { options, .. } => (0..options.len()).for_each(|k| v.push(Slot(i, k))),
                Field::Buttons { labels, .. } => (0..labels.len()).for_each(|k| v.push(Slot(i, k))),
                _ => {}
            }
        }
        v
    }

    fn first_focus(&self) -> usize {
        let slots = self.slots();
        // Focus the first input if any, else the default button.
        if let Some(p) = slots.iter().position(|s| matches!(self.fields[s.0].1, Field::Input { .. })) {
            return p;
        }
        for (p, s) in slots.iter().enumerate() {
            if let Field::Buttons { default, .. } = &self.fields[s.0].1
                && s.1 == *default
            {
                return p;
            }
        }
        0
    }

    fn result(&self, button: usize) -> FormResult {
        let mut r = FormResult { button, ..Default::default() };
        for (id, f) in &self.fields {
            match f {
                Field::Input { edit, .. } => {
                    r.texts.insert(id.clone(), edit.text());
                }
                Field::Check { value, .. } => {
                    r.checks.insert(id.clone(), *value);
                }
                Field::Radio { selected, .. } => {
                    r.radios.insert(id.clone(), *selected);
                }
                _ => {}
            }
        }
        r
    }

    fn submit(&mut self, button: usize) -> Outcome {
        let r = self.result(button);
        let hist: Vec<(String, String)> = self
            .fields
            .iter()
            .filter_map(|(_, f)| match f {
                Field::Input { edit, hist: Some(k), .. } => Some((k.clone(), edit.text())),
                _ => None,
            })
            .collect();
        let cb = self.on_submit.take();
        Outcome::CloseThen(Box::new(move |app: &mut App| {
            if button == 0 {
                for (k, v) in hist {
                    crate::config::State::push_history(app.state.input_hist(&k), &v);
                }
            }
            if let Some(cb) = cb {
                cb(app, r);
            }
        }))
    }

    fn cancel(&mut self) -> Outcome {
        match self.on_cancel.take() {
            Some(f) => Outcome::CloseThen(f),
            None => Outcome::Close,
        }
    }

    fn default_button(&self) -> usize {
        self.fields
            .iter()
            .find_map(|(_, f)| if let Field::Buttons { default, .. } = f { Some(*default) } else { None })
            .unwrap_or(0)
    }

    fn activate(&mut self, slot: Slot) -> Option<Outcome> {
        match &mut self.fields[slot.0].1 {
            Field::Check { value, .. } => *value = !*value,
            Field::Radio { selected, .. } => *selected = slot.1,
            Field::Buttons { labels, .. } => {
                // A button labelled Cancel just closes (with the cancel hook).
                if labels[slot.1].eq_ignore_ascii_case("cancel") && self.on_cancel.is_some() {
                    return Some(self.cancel());
                }
                return Some(self.submit(slot.1));
            }
            _ => {}
        }
        None
    }

    fn needed_size(&self, max_w: u16) -> (u16, u16) {
        let mut w = str_width(&self.title) as u16 + 6;
        let inner_max = max_w.saturating_sub(4).max(20);
        for (_, f) in &self.fields {
            let fw = match f {
                Field::Text(t) => t.lines().map(str_width).max().unwrap_or(0) as u16,
                Field::Input { label, .. } => (str_width(label) as u16).max(40),
                Field::Check { label, .. } => str_width(label) as u16 + 4,
                Field::Radio { options, label, .. } => {
                    options.iter().map(|o| str_width(o)).max().unwrap_or(0).max(str_width(label)) as u16 + 4
                }
                Field::Buttons { labels, .. } => buttons_width(labels),
                Field::Separator => 0,
            };
            w = w.max(fw + 4);
        }
        if self.width > 0 {
            w = w.max(self.width);
        }
        w = w.min(max_w.saturating_sub(4));
        let inner_w = w.saturating_sub(4).max(1).min(inner_max) as usize;
        let mut h = 2u16;
        for (_, f) in &self.fields {
            h += match f {
                Field::Text(t) => t.lines().map(|l| (str_width(l).max(1)).div_ceil(inner_w) as u16).sum::<u16>().max(1),
                Field::Input { label, .. } => {
                    if label.is_empty() {
                        1
                    } else {
                        2
                    }
                }
                Field::Check { .. } => 1,
                Field::Radio { options, label, .. } => options.len() as u16 + if label.is_empty() { 0 } else { 1 },
                Field::Buttons { .. } => 2,
                Field::Separator => 1,
            };
        }
        (w, h)
    }
}

impl Dialog for Form {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let (w, h) = self.needed_size(area.width);
        let r = centered(area, w, h);
        let inner = draw_box(f, r, &self.title, ctx, self.error);
        let slots = self.slots();
        let focus = slots.get(self.focus).copied();
        let base = if self.error { ctx.theme.error } else { ctx.theme.dialog };
        let mut y = inner.y;
        self.hit.clear();
        let bottom = inner.bottom();
        for (fi, (_, field)) in self.fields.iter_mut().enumerate() {
            if y >= bottom {
                break;
            }
            match field {
                Field::Text(t) => {
                    let lines: Vec<Line> = t.lines().map(|l| Line::styled(l.to_string(), base)).collect();
                    let n = t
                        .lines()
                        .map(|l| (str_width(l).max(1)).div_ceil(inner.width.max(1) as usize) as u16)
                        .sum::<u16>()
                        .max(1);
                    let rr = Rect { y, height: n.min(bottom - y), ..inner };
                    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).style(base), rr);
                    y += n;
                }
                Field::Separator => {
                    let line = "─".repeat(inner.width as usize);
                    f.render_widget(
                        Paragraph::new(Span::styled(line, ctx.theme.dialog_border)),
                        Rect { y, height: 1, ..inner },
                    );
                    y += 1;
                }
                Field::Input { label, edit, .. } => {
                    if !label.is_empty() {
                        f.render_widget(
                            Paragraph::new(Span::styled(label.clone(), base)),
                            Rect { y, height: 1, ..inner },
                        );
                        y += 1;
                    }
                    if y >= bottom {
                        break;
                    }
                    let focused = focus == Some(Slot(fi, 0));
                    let st = if focused { ctx.theme.input_focus } else { ctx.theme.input };
                    let rr = Rect { y, height: 1, ..inner };
                    edit.render(f, rr, st, focused);
                    if let Some(p) = slots.iter().position(|s| *s == Slot(fi, 0)) {
                        self.hit.push((rr, p));
                    }
                    y += 1;
                }
                Field::Check { label, value } => {
                    let focused = focus == Some(Slot(fi, 0));
                    let mark = if *value { "[x] " } else { "[ ] " };
                    let st = if focused { ctx.theme.dialog_focus } else { base };
                    let rr = Rect { y, height: 1, width: (str_width(label) as u16 + 4).min(inner.width), ..inner };
                    f.render_widget(Paragraph::new(Span::styled(format!("{mark}{label}"), st)), rr);
                    if let Some(p) = slots.iter().position(|s| *s == Slot(fi, 0)) {
                        self.hit.push((rr, p));
                    }
                    y += 1;
                }
                Field::Radio { label, options, selected } => {
                    if !label.is_empty() {
                        f.render_widget(
                            Paragraph::new(Span::styled(label.clone(), base)),
                            Rect { y, height: 1, ..inner },
                        );
                        y += 1;
                    }
                    for (k, o) in options.iter().enumerate() {
                        if y >= bottom {
                            break;
                        }
                        let focused = focus == Some(Slot(fi, k));
                        let mark = if *selected == k { "(*) " } else { "( ) " };
                        let st = if focused { ctx.theme.dialog_focus } else { base };
                        let rr = Rect { y, height: 1, width: (str_width(o) as u16 + 4).min(inner.width), ..inner };
                        f.render_widget(Paragraph::new(Span::styled(format!("{mark}{o}"), st)), rr);
                        if let Some(p) = slots.iter().position(|s| *s == Slot(fi, k)) {
                            self.hit.push((rr, p));
                        }
                        y += 1;
                    }
                }
                Field::Buttons { labels, default } => {
                    y += 1;
                    if y >= bottom {
                        y = bottom - 1;
                    }
                    let total: u16 = buttons_width(labels);
                    let mut x = inner.x + inner.width.saturating_sub(total) / 2;
                    for (k, l) in labels.iter().enumerate() {
                        let focused = focus == Some(Slot(fi, k));
                        let text = if k == *default { format!("[< {l} >]") } else { format!("[ {l} ]") };
                        let tw = str_width(&text) as u16;
                        let st = if focused { ctx.theme.dialog_focus } else { base };
                        let rr = Rect { x, y, width: tw.min(inner.right().saturating_sub(x)), height: 1 };
                        f.render_widget(Paragraph::new(Span::styled(text, st)), rr);
                        if let Some(p) = slots.iter().position(|s| *s == Slot(fi, k)) {
                            self.hit.push((rr, p));
                        }
                        x += tw + 1;
                    }
                    y += 1;
                }
            }
        }
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        let slots = self.slots();
        if slots.is_empty() {
            return match key.code {
                KeyCode::Esc => self.cancel(),
                KeyCode::Enter => self.submit(0),
                _ => Outcome::Keep,
            };
        }
        let cur = slots[self.focus.min(slots.len() - 1)];
        let in_buttons = matches!(self.fields[cur.0].1, Field::Buttons { .. });
        let in_input = matches!(self.fields[cur.0].1, Field::Input { .. });
        // Up/Down on an input with history browse history only with Alt; plain arrows move focus.
        match key.code {
            KeyCode::Esc => return self.cancel(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return self.cancel(),
            KeyCode::Tab | KeyCode::Down => {
                if key.code == KeyCode::Down
                    && in_input
                    && let Field::Input { edit, .. } = &mut self.fields[cur.0].1
                    && edit.has_history()
                    && key.modifiers.contains(KeyModifiers::ALT)
                {
                    edit.handle_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                    return Outcome::Keep;
                }
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.focus = (self.focus + slots.len() - 1) % slots.len();
                } else {
                    self.focus = (self.focus + 1) % slots.len();
                }
                return Outcome::Keep;
            }
            KeyCode::BackTab | KeyCode::Up => {
                if key.code == KeyCode::Up
                    && in_input
                    && let Field::Input { edit, .. } = &mut self.fields[cur.0].1
                    && edit.has_history()
                    && key.modifiers.contains(KeyModifiers::ALT)
                {
                    edit.handle_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
                    return Outcome::Keep;
                }
                self.focus = (self.focus + slots.len() - 1) % slots.len();
                return Outcome::Keep;
            }
            KeyCode::Left | KeyCode::Right if in_buttons || !in_input => {
                let d = if key.code == KeyCode::Left { slots.len() - 1 } else { 1 };
                self.focus = (self.focus + d) % slots.len();
                return Outcome::Keep;
            }
            KeyCode::Enter => {
                if in_buttons && let Some(o) = self.activate(cur) {
                    return o;
                }
                let b = self.default_button();
                return self.submit(b);
            }
            KeyCode::Char(' ') if !in_input => {
                if let Some(o) = self.activate(cur) {
                    return o;
                }
                return Outcome::Keep;
            }
            _ => {}
        }
        if let Field::Input { edit, .. } = &mut self.fields[cur.0].1 {
            // Alt-H / Ctrl-Up: history (Up/Down are used for focus movement)
            if key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::ALT) {
                edit.handle_key(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
                return Outcome::Keep;
            }
            edit.handle_key(&key);
            return Outcome::Keep;
        }
        // Hotkeys: first letter of buttons / checkboxes.
        if let KeyCode::Char(c) = key.code {
            let c = c.to_ascii_lowercase();
            for (p, s) in slots.iter().enumerate() {
                let label = match &self.fields[s.0].1 {
                    Field::Buttons { labels, .. } => labels[s.1].clone(),
                    Field::Check { label, .. } => label.clone(),
                    Field::Radio { options, .. } => options[s.1].clone(),
                    _ => continue,
                };
                if label.chars().next().map(|x| x.to_ascii_lowercase()) == Some(c) {
                    self.focus = p;
                    if let Some(o) = self.activate(*s) {
                        return o;
                    }
                    return Outcome::Keep;
                }
            }
        }
        Outcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> Outcome {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            let hit = self.hit.iter().find(|(r, _)| r.contains((m.column, m.row).into())).map(|(_, p)| *p);
            if let Some(p) = hit {
                self.focus = p;
                let slots = self.slots();
                if let Some(o) = self.activate(slots[p]) {
                    return o;
                }
            }
        }
        Outcome::Keep
    }

    fn paste(&mut self, text: &str) {
        let slots = self.slots();
        if let Some(s) = slots.get(self.focus)
            && let Field::Input { edit, .. } = &mut self.fields[s.0].1
        {
            edit.insert_str(text);
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Exact width of a button row: "[ a ]" per button, "[< d >]" for the default, 1 space gaps.
fn buttons_width(labels: &[String]) -> u16 {
    labels.iter().map(|l| str_width(l) as u16 + 4).sum::<u16>() + 2 + labels.len().saturating_sub(1) as u16
}
