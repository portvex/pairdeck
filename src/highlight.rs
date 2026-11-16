//! Syntax highlighting with syntect (shared by preview, viewer and editor).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// bat's extended grammar set (about 200 languages, via two-face), loaded once.
pub fn syntax_set() -> &'static SyntaxSet {
    static S: OnceLock<SyntaxSet> = OnceLock::new();
    S.get_or_init(two_face::syntax::extra_newlines)
}

/// bat's themes (Catppuccin, Nord, Gruvbox, GitHub, Dracula, Solarized, ...).
pub fn theme_set() -> &'static ThemeSet {
    static T: OnceLock<ThemeSet> = OnceLock::new();
    T.get_or_init(|| two_face::theme::extra().into())
}

pub fn theme(name: &str) -> &'static Theme {
    let ts = theme_set();
    ts.themes
        .get(name)
        .or_else(|| ts.themes.get("Catppuccin Mocha"))
        .or_else(|| ts.themes.values().next())
        .expect("bundled syntax themes")
}

/// Names of all bundled syntax themes (for theme files and the options dialog).
pub fn theme_names() -> Vec<&'static str> {
    let mut v: Vec<&str> = theme_set().themes.keys().map(String::as_str).collect();
    v.sort_unstable();
    v
}

/// Find a syntax for a file: well-known file names, then the extension, then
/// the first line (shebangs, `<?xml`, modelines). Returns None for plain text.
pub fn find_syntax(file_name: &str, first_line: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    let lower = file_name.to_ascii_lowercase();
    let special = match lower.as_str() {
        "cargo.lock" | "poetry.lock" | "pipfile" | "uv.lock" => Some("TOML"),
        ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | ".zprofile" | ".zshenv" | ".envrc" | ".bash_aliases"
        | ".bash_logout" => Some("Bourne Again Shell (bash)"),
        "justfile" | ".justfile" => Some("Makefile"),
        _ if lower.starts_with("dockerfile") || lower.ends_with(".dockerfile") || lower == "containerfile" => {
            Some("Dockerfile")
        }
        _ if lower.starts_with(".env") => Some("Bourne Again Shell (bash)"),
        _ => None,
    };
    if let Some(s) = special.and_then(|n| ss.find_syntax_by_name(n)) {
        return Some(s);
    }
    // Grammars list whole file names too (Makefile, .gitignore, CMakeLists.txt...).
    if let Some(s) = ss.find_syntax_by_extension(file_name).or_else(|| ss.find_syntax_by_extension(&lower)) {
        return Some(s);
    }
    let ext = std::path::Path::new(&lower).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let mapped = match ext.as_str() {
        "mts" | "cts" => "ts",
        // The TSX grammar handles JSX (bat has no separate JSX grammar).
        "jsx" => "tsx",
        "jsonc" | "json5" => "json",
        "svgz" => "svg",
        "conf" | "cfg" => "ini",
        e => e,
    };
    ss.find_syntax_by_extension(mapped)
        .or_else(|| ss.find_syntax_by_first_line(first_line))
        .filter(|s| s.name != "Plain Text")
}

pub fn convert_style(s: syntect::highlighting::Style, with_bg: bool) -> Style {
    let mut st = Style::default().fg(Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b));
    if with_bg {
        st = st.bg(Color::Rgb(s.background.r, s.background.g, s.background.b));
    }
    if s.font_style.contains(FontStyle::BOLD) {
        st = st.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    st
}

/// Highlight up to `max_lines` lines. Tabs are expanded to `tab` spaces.
/// Returns None if no syntax matches.
pub fn highlight(
    text: &str,
    file_name: &str,
    theme_name: &str,
    max_lines: usize,
    tab: usize,
) -> Option<Vec<Line<'static>>> {
    let first = text.lines().next().unwrap_or("");
    let syntax = find_syntax(file_name, first)?;
    let mut h = HighlightLines::new(syntax, theme(theme_name));
    let ss = syntax_set();
    let mut out = Vec::new();
    for line in syntect::util::LinesWithEndings::from(text).take(max_lines) {
        let ranges = h.highlight_line(line, ss).ok()?;
        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(st, s)| {
                let s = s.trim_end_matches(['\n', '\r']).replace('\t', &" ".repeat(tab));
                Span::styled(s, convert_style(st, false))
            })
            .collect();
        out.push(Line::from(spans));
    }
    Some(out)
}

/// Stateful highlighter for incremental use (editor): keeps parse state per line.
pub struct Incremental {
    pub syntax: &'static SyntaxReference,
    pub theme_name: String,
}

impl Incremental {
    pub fn new(file_name: &str, first_line: &str, theme_name: &str) -> Option<Self> {
        Some(Incremental { syntax: find_syntax(file_name, first_line)?, theme_name: theme_name.to_string() })
    }
    pub fn highlighter(&self) -> HighlightLines<'static> {
        HighlightLines::new(self.syntax, theme(&self.theme_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_files_get_a_grammar() {
        let cases = [
            ("main.rs", ""),
            ("app.py", ""),
            ("Cargo.toml", ""),
            ("Cargo.lock", ""),
            ("index.ts", ""),
            ("view.tsx", ""),
            ("comp.jsx", ""),
            ("Dockerfile", ""),
            ("Dockerfile.dev", ""),
            ("Makefile", ""),
            (".gitignore", ""),
            (".zshrc", ""),
            ("default.nix", ""),
            ("Main.kt", ""),
            ("App.swift", ""),
            ("build.zig", ""),
            ("config.yaml", ""),
            ("data.json", ""),
            ("README.md", ""),
            ("logo.svg", ""),
            ("logo.svgz", ""),
            ("settings.ini", ""),
            ("nginx.conf", ""),
            ("table.csv", ""),
            ("fix.diff", ""),
            ("go.mod", ""),
            ("query.sql", ""),
            ("page.vue", ""),
            ("Comp.svelte", ""),
            ("system.log", ""),
            (".env", ""),
            ("CMakeLists.txt", ""),
            ("run", "#!/usr/bin/env python3"),
            ("tool", "#!/bin/bash"),
        ];
        let missing: Vec<&str> = cases.iter().filter(|(n, l)| find_syntax(n, l).is_none()).map(|(n, _)| *n).collect();
        assert!(missing.is_empty(), "no grammar for: {missing:?}");
        assert!(find_syntax("notes.txt", "hello").is_none(), "plain text stays plain");
    }

    #[test]
    fn app_themes_exist() {
        for t in crate::theme::BUILTIN {
            let name = crate::theme::Theme::by_name(t).syntax;
            assert!(theme_set().themes.contains_key(&name), "missing syntax theme {name}");
        }
    }

    #[test]
    fn highlights_toml_with_colors() {
        let lines = highlight("[package]\nname = \"x\"\n", "Cargo.toml", "Catppuccin Mocha", 10, 4).unwrap();
        let colors: std::collections::HashSet<_> =
            lines.iter().flat_map(|l| l.spans.iter().map(|s| s.style.fg)).collect();
        assert!(colors.len() > 1, "expected several colors, got {colors:?}");
    }
}
