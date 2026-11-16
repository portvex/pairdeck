//! Full-screen views stacked over the panels (viewer, editor, diff, help).

use crate::dialog::AppFn;
use crate::ui::Ctx;
use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{Frame, layout::Rect};
use std::any::Any;

#[allow(dead_code)]
pub enum ScreenOutcome {
    Keep,
    Close,
    /// Keep the screen and run the closure (e.g. to push a dialog).
    Then(AppFn),
    CloseThen(AppFn),
}

#[allow(dead_code)]
pub trait Screen: Any {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx);
    fn key(&mut self, key: KeyEvent) -> ScreenOutcome;
    fn mouse(&mut self, _m: MouseEvent) -> ScreenOutcome {
        ScreenOutcome::Keep
    }
    fn paste(&mut self, _text: &str) {}
    fn tick(&mut self) -> ScreenOutcome {
        ScreenOutcome::Keep
    }
    /// Title for the terminal window / screen list.
    fn title(&self) -> String;
    /// Return true if closing would lose data (editor with unsaved changes).
    fn is_dirty(&self) -> bool {
        false
    }
    fn as_any_mut(&mut self) -> &mut dyn Any;
}
