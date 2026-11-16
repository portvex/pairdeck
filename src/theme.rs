//! Color themes. Built-in themes plus user overrides from `themes/<name>.toml`.

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub panel: Style,
    pub border: Style,
    pub border_active: Style,
    pub header: Style,
    pub cursor: Style,
    pub cursor_inactive: Style,
    pub marked: Style,
    pub marked_cursor: Style,
    pub dir: Style,
    pub exec: Style,
    pub symlink: Style,
    pub broken: Style,
    pub archive: Style,
    pub image: Style,
    pub media: Style,
    pub doc: Style,
    pub source: Style,
    pub hidden: Style,
    pub special: Style,
    pub menubar: Style,
    pub menubar_key: Style,
    pub menu: Style,
    pub menu_sel: Style,
    pub menu_hotkey: Style,
    pub button_key: Style,
    pub button_label: Style,
    pub dialog: Style,
    pub dialog_border: Style,
    pub dialog_title: Style,
    pub dialog_focus: Style,
    pub input: Style,
    pub input_focus: Style,
    pub error: Style,
    pub error_border: Style,
    pub cmdline: Style,
    pub prompt: Style,
    pub status: Style,
    pub gauge: Style,
    pub viewer: Style,
    pub editor: Style,
    pub editor_line_nr: Style,
    pub editor_selection: Style,
    pub search_match: Style,
    pub diff_add: Style,
    pub diff_del: Style,
    pub diff_change: Style,
    pub git_modified: Style,
    pub git_staged: Style,
    pub git_added: Style,
    pub git_untracked: Style,
    pub git_deleted: Style,
    pub git_renamed: Style,
    pub git_conflict: Style,
    pub git_ignored: Style,
    pub git_branch: Style,
    /// syntect theme name used for highlighting
    pub syntax: String,
}

fn s(fg: Color, bg: Color) -> Style {
    Style::default().fg(fg).bg(bg)
}

fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const BUILTIN: &[&str] = &["dark", "classic", "nord", "gruvbox", "light"];

impl Theme {
    pub fn by_name(name: &str) -> Theme {
        match name {
            "classic" => Self::classic(),
            "nord" => Self::modern("nord", Palette::nord()),
            "gruvbox" => Self::modern("gruvbox", Palette::gruvbox()),
            "light" => Self::modern("light", Palette::light()),
            _ => Self::modern("dark", Palette::mocha()),
        }
    }

    /// Classic Midnight Commander style: blue panels, cyan cursor.
    pub fn classic() -> Theme {
        use Color::*;
        let b = Blue;
        Theme {
            name: "classic".into(),
            panel: s(Gray, b),
            border: s(Gray, b),
            border_active: s(White, b).add_modifier(Modifier::BOLD),
            header: s(Yellow, b).add_modifier(Modifier::BOLD),
            cursor: s(Black, Cyan),
            cursor_inactive: s(Black, DarkGray),
            marked: s(Yellow, b).add_modifier(Modifier::BOLD),
            marked_cursor: s(Yellow, Cyan).add_modifier(Modifier::BOLD),
            dir: s(White, b).add_modifier(Modifier::BOLD),
            exec: s(LightGreen, b).add_modifier(Modifier::BOLD),
            symlink: s(LightCyan, b),
            broken: s(LightRed, b),
            archive: s(LightMagenta, b),
            image: s(LightCyan, b),
            media: s(LightMagenta, b),
            doc: s(Gray, b),
            source: s(Gray, b),
            hidden: s(DarkGray, b),
            special: s(White, b),
            menubar: s(Black, Cyan),
            menubar_key: s(Yellow, Cyan),
            menu: s(White, Cyan),
            menu_sel: s(White, Black),
            menu_hotkey: s(Yellow, Cyan),
            button_key: s(White, Black),
            button_label: s(Black, Cyan),
            dialog: s(Black, Gray),
            dialog_border: s(Black, Gray),
            dialog_title: s(Blue, Gray).add_modifier(Modifier::BOLD),
            dialog_focus: s(Black, Cyan),
            input: s(Black, Cyan),
            input_focus: s(Black, Cyan).add_modifier(Modifier::BOLD),
            error: s(White, Red),
            error_border: s(White, Red).add_modifier(Modifier::BOLD),
            cmdline: s(Gray, Black),
            prompt: s(Gray, Black),
            status: s(Black, Cyan),
            gauge: s(Cyan, Black),
            viewer: s(Gray, b),
            editor: s(Gray, b),
            editor_line_nr: s(Yellow, b),
            editor_selection: s(Black, Cyan),
            search_match: s(Black, Yellow),
            diff_add: s(Black, Green),
            diff_del: s(Black, Red),
            diff_change: s(Black, Magenta),
            git_modified: Style::default().fg(Yellow),
            git_staged: Style::default().fg(LightGreen),
            git_added: Style::default().fg(LightGreen),
            git_untracked: Style::default().fg(LightMagenta),
            git_deleted: Style::default().fg(LightRed),
            git_renamed: Style::default().fg(LightCyan),
            git_conflict: Style::default().fg(LightRed).add_modifier(Modifier::BOLD),
            git_ignored: Style::default().fg(DarkGray),
            git_branch: s(LightCyan, b),
            syntax: "base16-ocean.dark".into(),
        }
    }

    fn modern(name: &str, p: Palette) -> Theme {
        Theme {
            name: name.into(),
            panel: s(p.fg, p.bg),
            border: s(p.border, p.bg),
            border_active: s(p.accent, p.bg).add_modifier(Modifier::BOLD),
            header: s(p.yellow, p.bg).add_modifier(Modifier::BOLD),
            cursor: s(p.bg, p.accent).add_modifier(Modifier::BOLD),
            cursor_inactive: s(p.fg, p.surface),
            marked: s(p.yellow, p.bg).add_modifier(Modifier::BOLD),
            marked_cursor: s(p.yellow, p.accent2).add_modifier(Modifier::BOLD),
            dir: s(p.blue, p.bg).add_modifier(Modifier::BOLD),
            exec: s(p.green, p.bg).add_modifier(Modifier::BOLD),
            symlink: s(p.cyan, p.bg).add_modifier(Modifier::ITALIC),
            broken: s(p.red, p.bg).add_modifier(Modifier::CROSSED_OUT),
            archive: s(p.red, p.bg),
            image: s(p.magenta, p.bg),
            media: s(p.orange, p.bg),
            doc: s(p.fg, p.bg),
            source: s(p.fg, p.bg),
            hidden: s(p.muted, p.bg),
            special: s(p.yellow, p.bg),
            menubar: s(p.fg, p.surface),
            menubar_key: s(p.accent, p.surface).add_modifier(Modifier::BOLD),
            menu: s(p.fg, p.surface),
            menu_sel: s(p.bg, p.accent),
            menu_hotkey: s(p.yellow, p.surface),
            button_key: s(p.accent, p.bg).add_modifier(Modifier::BOLD),
            button_label: s(p.fg, p.surface),
            dialog: s(p.fg, p.surface),
            dialog_border: s(p.accent, p.surface),
            dialog_title: s(p.accent, p.surface).add_modifier(Modifier::BOLD),
            dialog_focus: s(p.bg, p.accent).add_modifier(Modifier::BOLD),
            input: s(p.fg, p.bg),
            input_focus: s(p.fg, p.bg).add_modifier(Modifier::BOLD),
            error: s(p.fg, p.err_bg),
            error_border: s(p.red, p.err_bg).add_modifier(Modifier::BOLD),
            cmdline: s(p.fg, p.bg),
            prompt: s(p.green, p.bg).add_modifier(Modifier::BOLD),
            status: s(p.fg, p.surface),
            gauge: s(p.accent, p.bg),
            viewer: s(p.fg, p.bg),
            editor: s(p.fg, p.bg),
            editor_line_nr: s(p.muted, p.bg),
            editor_selection: s(p.fg, p.accent2),
            search_match: s(p.bg, p.yellow),
            diff_add: s(p.green, p.bg),
            diff_del: s(p.red, p.bg),
            diff_change: s(p.yellow, p.bg),
            git_modified: Style::default().fg(p.yellow),
            git_staged: Style::default().fg(p.green),
            git_added: Style::default().fg(p.green),
            git_untracked: Style::default().fg(p.magenta),
            git_deleted: Style::default().fg(p.red),
            git_renamed: Style::default().fg(p.cyan),
            git_conflict: Style::default().fg(p.red).add_modifier(Modifier::BOLD),
            git_ignored: Style::default().fg(p.muted),
            git_branch: s(p.magenta, p.bg).add_modifier(Modifier::BOLD),
            syntax: p.syntax.into(),
        }
    }

    /// Load built-in theme `base`, then apply overrides from a user TOML file if present.
    pub fn load(name: &str) -> Theme {
        let mut theme = Theme::by_name(name);
        let Some(dir) = crate::config::config_dir() else { return theme };
        let path = dir.join("themes").join(format!("{name}.toml"));
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(user) = toml::from_str::<UserTheme>(&text)
        {
            if let Some(base) = &user.base {
                theme = Theme::by_name(base);
            }
            theme.name = name.to_string();
            theme.apply(&user);
        }
        theme
    }

    fn apply(&mut self, u: &UserTheme) {
        if let Some(sx) = &u.syntax {
            self.syntax = sx.clone();
        }
        for (key, spec) in &u.styles {
            let Some(style) = parse_style(spec) else { continue };
            let slot = match key.as_str() {
                "panel" => &mut self.panel,
                "border" => &mut self.border,
                "border_active" => &mut self.border_active,
                "header" => &mut self.header,
                "cursor" => &mut self.cursor,
                "cursor_inactive" => &mut self.cursor_inactive,
                "marked" => &mut self.marked,
                "marked_cursor" => &mut self.marked_cursor,
                "dir" => &mut self.dir,
                "exec" => &mut self.exec,
                "symlink" => &mut self.symlink,
                "broken" => &mut self.broken,
                "archive" => &mut self.archive,
                "image" => &mut self.image,
                "media" => &mut self.media,
                "doc" => &mut self.doc,
                "source" => &mut self.source,
                "hidden" => &mut self.hidden,
                "menubar" => &mut self.menubar,
                "menu" => &mut self.menu,
                "menu_sel" => &mut self.menu_sel,
                "button_key" => &mut self.button_key,
                "button_label" => &mut self.button_label,
                "dialog" => &mut self.dialog,
                "dialog_border" => &mut self.dialog_border,
                "dialog_focus" => &mut self.dialog_focus,
                "input" => &mut self.input,
                "error" => &mut self.error,
                "cmdline" => &mut self.cmdline,
                "prompt" => &mut self.prompt,
                "status" => &mut self.status,
                "viewer" => &mut self.viewer,
                "editor" => &mut self.editor,
                "git_modified" => &mut self.git_modified,
                "git_staged" => &mut self.git_staged,
                "git_added" => &mut self.git_added,
                "git_untracked" => &mut self.git_untracked,
                "git_deleted" => &mut self.git_deleted,
                "git_renamed" => &mut self.git_renamed,
                "git_conflict" => &mut self.git_conflict,
                "git_ignored" => &mut self.git_ignored,
                "git_branch" => &mut self.git_branch,
                _ => continue,
            };
            *slot = style;
        }
    }
}

/// User theme file:
/// ```toml
/// base = "dark"
/// syntax = "Solarized (dark)"
/// [styles]
/// cursor = "black on #88c0d0 bold"
/// ```
#[derive(Deserialize, Default)]
struct UserTheme {
    base: Option<String>,
    syntax: Option<String>,
    #[serde(default)]
    styles: HashMap<String, String>,
}

pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        return u32::from_str_radix(hex, 16).ok().map(rgb);
    }
    s.parse::<Color>().ok()
}

/// Parse "fg on bg [bold] [italic] [underline]".
pub fn parse_style(spec: &str) -> Option<Style> {
    let mut style = Style::default();
    let words: Vec<&str> = spec.split_whitespace().collect();
    let mut i = 0;
    let mut expect_bg = false;
    while i < words.len() {
        match words[i] {
            "on" => expect_bg = true,
            "bold" => style = style.add_modifier(Modifier::BOLD),
            "italic" => style = style.add_modifier(Modifier::ITALIC),
            "underline" => style = style.add_modifier(Modifier::UNDERLINED),
            "reverse" => style = style.add_modifier(Modifier::REVERSED),
            w => {
                let c = parse_color(w)?;
                style = if expect_bg { style.bg(c) } else { style.fg(c) };
                expect_bg = false;
            }
        }
        i += 1;
    }
    Some(style)
}

struct Palette {
    bg: Color,
    surface: Color,
    fg: Color,
    muted: Color,
    border: Color,
    accent: Color,
    accent2: Color,
    blue: Color,
    cyan: Color,
    green: Color,
    yellow: Color,
    orange: Color,
    red: Color,
    magenta: Color,
    err_bg: Color,
    syntax: &'static str,
}

impl Palette {
    fn mocha() -> Self {
        Palette {
            bg: rgb(0x1e1e2e),
            surface: rgb(0x313244),
            fg: rgb(0xcdd6f4),
            muted: rgb(0x6c7086),
            border: rgb(0x45475a),
            accent: rgb(0x89b4fa),
            accent2: rgb(0x585b70),
            blue: rgb(0x89b4fa),
            cyan: rgb(0x94e2d5),
            green: rgb(0xa6e3a1),
            yellow: rgb(0xf9e2af),
            orange: rgb(0xfab387),
            red: rgb(0xf38ba8),
            magenta: rgb(0xcba6f7),
            err_bg: rgb(0x45263a),
            syntax: "Catppuccin Mocha",
        }
    }
    fn nord() -> Self {
        Palette {
            bg: rgb(0x2e3440),
            surface: rgb(0x3b4252),
            fg: rgb(0xd8dee9),
            muted: rgb(0x616e88),
            border: rgb(0x4c566a),
            accent: rgb(0x88c0d0),
            accent2: rgb(0x4c566a),
            blue: rgb(0x81a1c1),
            cyan: rgb(0x8fbcbb),
            green: rgb(0xa3be8c),
            yellow: rgb(0xebcb8b),
            orange: rgb(0xd08770),
            red: rgb(0xbf616a),
            magenta: rgb(0xb48ead),
            err_bg: rgb(0x4a2f36),
            syntax: "Nord",
        }
    }
    fn gruvbox() -> Self {
        Palette {
            bg: rgb(0x282828),
            surface: rgb(0x3c3836),
            fg: rgb(0xebdbb2),
            muted: rgb(0x928374),
            border: rgb(0x504945),
            accent: rgb(0xfabd2f),
            accent2: rgb(0x665c54),
            blue: rgb(0x83a598),
            cyan: rgb(0x8ec07c),
            green: rgb(0xb8bb26),
            yellow: rgb(0xfabd2f),
            orange: rgb(0xfe8019),
            red: rgb(0xfb4934),
            magenta: rgb(0xd3869b),
            err_bg: rgb(0x4a2a24),
            syntax: "gruvbox-dark",
        }
    }
    fn light() -> Self {
        Palette {
            bg: rgb(0xfafafa),
            surface: rgb(0xe8e8ec),
            fg: rgb(0x383a42),
            muted: rgb(0xa0a1a7),
            border: rgb(0xc8c8d0),
            accent: rgb(0x4078f2),
            accent2: rgb(0xc8d6f8),
            blue: rgb(0x4078f2),
            cyan: rgb(0x0184bc),
            green: rgb(0x50a14f),
            yellow: rgb(0x986801),
            orange: rgb(0xc18401),
            red: rgb(0xe45649),
            magenta: rgb(0xa626a4),
            err_bg: rgb(0xfde2e0),
            syntax: "Catppuccin Latte",
        }
    }
}
