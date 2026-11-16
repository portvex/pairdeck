//! Modal dialogs. A dialog handles keys and returns an [`Outcome`]; actions that
//! need the application are closures run after the dialog is popped.

pub mod form;
pub mod list;
pub mod menu;
pub mod progress;

#[allow(unused_imports)]
pub use form::{Field, Form, FormResult};
pub use list::{ListAction, ListDialog, ListItem};

use crate::app::App;
use crate::ui::Ctx;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Line,
    widgets::{Block, BorderType, Borders, Clear},
};
use std::any::Any;

pub type AppFn = Box<dyn FnOnce(&mut App)>;

pub enum Outcome {
    /// Stay open.
    Keep,
    /// Close the dialog.
    Close,
    /// Close the dialog, then run the closure.
    CloseThen(AppFn),
    /// Stay open and run the closure.
    KeepThen(AppFn),
}

pub trait Dialog: Any {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx);
    fn key(&mut self, key: KeyEvent) -> Outcome;
    fn mouse(&mut self, _m: MouseEvent) -> Outcome {
        Outcome::Keep
    }
    fn paste(&mut self, _text: &str) {}
    /// Called on every UI tick (~10 Hz); used by progress dialogs.
    fn tick(&mut self) -> Outcome {
        Outcome::Keep
    }
    #[allow(dead_code)]
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Rect of size w×h centered in `area` (clamped).
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2)).max(1);
    let h = h.min(area.height.saturating_sub(2)).max(1);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 3, width: w, height: h }
}

/// Clear `r`, draw a shadow and a bordered box with a title. Returns the inner area.
pub fn draw_box(f: &mut Frame, r: Rect, title: &str, ctx: &Ctx, error: bool) -> Rect {
    shadow(f, r);
    f.render_widget(Clear, r);
    let (bg, border) =
        if error { (ctx.theme.error, ctx.theme.error_border) } else { (ctx.theme.dialog, ctx.theme.dialog_border) };
    let title_style = if error { ctx.theme.error_border } else { ctx.theme.dialog_title };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(if ctx.classic { BorderType::Double } else { BorderType::Rounded })
        .border_style(border)
        .style(bg)
        .title(Line::styled(format!(" {title} "), title_style).centered());
    let inner = block.inner(r);
    f.render_widget(block, r);
    Rect { x: inner.x + 1, width: inner.width.saturating_sub(2), ..inner }
}

pub fn shadow(f: &mut Frame, r: Rect) {
    let area = f.area();
    let buf = f.buffer_mut();
    let st = Style::default().bg(Color::Black).fg(Color::DarkGray);
    for y in r.y + 1..=r.bottom() {
        for x in [r.right(), r.right() + 1] {
            if x < area.right() && y < area.bottom() {
                buf[(x, y)].set_style(st);
            }
        }
    }
    let y = r.bottom();
    if y < area.bottom() {
        for x in r.x + 2..r.right().min(area.right()) {
            buf[(x, y)].set_style(st);
        }
    }
}

// ---------- convenience constructors ----------

pub fn message(title: &str, text: &str) -> Box<dyn Dialog> {
    Box::new(Form::new(title).text(text).buttons(&["OK"], 0))
}

pub fn error(text: &str) -> Box<dyn Dialog> {
    Box::new(Form::new("Error").text(text).buttons(&["OK"], 0).error())
}

pub fn confirm(title: &str, text: &str, on_yes: impl FnOnce(&mut App) + 'static) -> Box<dyn Dialog> {
    Box::new(Form::new(title).text(text).buttons(&["Yes", "No"], 0).on_submit(move |app, r| {
        if r.button == 0 {
            on_yes(app)
        }
    }))
}

/// Buttons dialog; the closure receives the chosen button index.
pub fn choice(
    title: &str,
    text: &str,
    buttons: &[&str],
    on_choice: impl FnOnce(&mut App, usize) + 'static,
) -> Box<dyn Dialog> {
    Box::new(Form::new(title).text(text).buttons(buttons, 0).on_submit(move |app, r| on_choice(app, r.button)))
}

/// Single input dialog. `hist` is the history key (also used to persist history).
pub fn input(
    app: &mut App,
    title: &str,
    prompt: &str,
    initial: &str,
    hist: &str,
    on_ok: impl FnOnce(&mut App, String) + 'static,
) -> Box<dyn Dialog> {
    let h = app.state.input_hist(hist).clone();
    Box::new(Form::new(title).input_hist("v", prompt, initial, hist, h).buttons(&["OK", "Cancel"], 0).on_submit(
        move |app, r| {
            if r.button == 0 {
                on_ok(app, r.text("v"))
            }
        },
    ))
}
