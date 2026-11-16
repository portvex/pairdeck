//! Extra preview formatters: tables, structured data, photo metadata, fonts,
//! animations, executables, office documents and plain-text highlighting.
//! Everything here is a pure function of the file bytes so it can be unit
//! tested and fuzzed; `preview.rs` decides when to call it (on its worker thread).

pub mod anim;
pub mod binary;
pub mod docs;
pub mod exif;
pub mod font;
pub mod plaintext;
pub mod structured;
pub mod table;

use ratatui::style::{Color, Style};

/// Foreground color of a theme style, falling back to `default`.
pub(crate) fn fg(style: Style, default: Color) -> Color {
    style.fg.unwrap_or(default)
}
