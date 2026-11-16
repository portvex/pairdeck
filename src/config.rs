//! Configuration (`~/.config/pairdeck/config.toml`) and persistent state
//! (`~/.local/share/pairdeck/state.toml`: hotlist, histories).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Process-wide override of the config and data directories. Tests use it so the
/// user's real ~/.config/pairdeck and ~/.local/share/pairdeck are never read or written.
static DIR_OVERRIDE: std::sync::OnceLock<(PathBuf, PathBuf)> = std::sync::OnceLock::new();

/// Redirect `config_dir()` and `data_dir()` for this process. Only the first call
/// takes effect.
pub fn override_dirs(config: PathBuf, data: PathBuf) {
    let _ = DIR_OVERRIDE.set((config, data));
}

pub fn config_dir() -> Option<PathBuf> {
    if let Some((c, _)) = DIR_OVERRIDE.get() {
        return Some(c.clone());
    }
    if let Ok(x) = std::env::var("PAIRDECK_CONFIG_DIR") {
        return Some(PathBuf::from(x));
    }
    // Prefer XDG layout on macOS too, which is what terminal users expect.
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(x).join("pairdeck"));
    }
    dirs::home_dir().map(|h| h.join(".config").join("pairdeck"))
}

pub fn data_dir() -> Option<PathBuf> {
    if let Some((_, d)) = DIR_OVERRIDE.get() {
        return Some(d.clone());
    }
    if let Ok(x) = std::env::var("PAIRDECK_DATA_DIR") {
        return Some(PathBuf::from(x));
    }
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        return Some(PathBuf::from(x).join("pairdeck"));
    }
    dirs::home_dir().map(|h| h.join(".local").join("share").join("pairdeck"))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub nerd_fonts: bool,
    pub show_hidden: bool,
    pub mouse: bool,
    pub show_menubar: bool,
    pub show_buttonbar: bool,
    pub show_cmdline: bool,
    pub show_mini_status: bool,
    pub confirm_delete: bool,
    pub confirm_overwrite: bool,
    pub confirm_exit: bool,
    pub use_trash: bool,
    pub use_internal_viewer: bool,
    pub use_internal_editor: bool,
    pub lynx_motion: bool,
    pub preview_images: bool,
    /// auto | kitty | sixel | iterm2 | halfblocks
    pub image_protocol: String,
    pub dirs_first: bool,
    pub sort: String,
    pub sort_reverse: bool,
    pub case_sensitive_sort: bool,
    /// full | brief | long
    pub listing: String,
    pub auto_refresh: bool,
    pub tab_size: usize,
    pub editor_line_numbers: bool,
    pub editor_auto_indent: bool,
    pub editor_word_wrap: bool,
    pub viewer_wrap: bool,
    pub preview_max_kb: usize,
    pub ssh_default_user: Option<String>,
    /// Treat macOS Option+letter characters as Alt+letter in the panels.
    pub macos_option_as_alt: bool,
    /// Rotate / flip JPEGs by rewriting the EXIF orientation tag (lossless).
    pub image_lossless_jpeg_rotate: bool,
    /// Quality used when image actions re-encode JPEGs.
    pub image_jpeg_quality: u8,
    /// Show git status markers and branch info in panels.
    pub git_status: bool,
    /// Quick view shows `git diff HEAD` for modified tracked files.
    pub preview_git_diff: bool,
    /// Re-apply config.toml, themes, associations and the user menu when they change.
    pub live_reload: bool,
    /// Program for the embedded terminal pane (default: $SHELL).
    pub terminal_shell: Option<String>,
    /// The embedded terminal follows the active panel's directory (cd when the shell is idle).
    pub terminal_follow_panel: bool,
    pub keys: HashMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "dark".into(),
            nerd_fonts: true,
            show_hidden: false,
            mouse: true,
            show_menubar: true,
            show_buttonbar: true,
            show_cmdline: true,
            show_mini_status: true,
            confirm_delete: true,
            confirm_overwrite: true,
            confirm_exit: false,
            use_trash: true,
            use_internal_viewer: true,
            use_internal_editor: true,
            lynx_motion: false,
            preview_images: true,
            image_protocol: "auto".into(),
            dirs_first: true,
            sort: "name".into(),
            sort_reverse: false,
            case_sensitive_sort: false,
            listing: "full".into(),
            auto_refresh: true,
            tab_size: 4,
            editor_line_numbers: true,
            editor_auto_indent: true,
            editor_word_wrap: false,
            viewer_wrap: true,
            preview_max_kb: 512,
            ssh_default_user: None,
            macos_option_as_alt: true,
            image_lossless_jpeg_rotate: true,
            image_jpeg_quality: 90,
            git_status: true,
            preview_git_diff: true,
            live_reload: true,
            terminal_shell: None,
            terminal_follow_panel: false,
            keys: HashMap::new(),
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("config.toml"))
    }

    pub fn load() -> (Config, Option<String>) {
        let Some(path) = Self::path() else { return (Config::default(), None) };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(c) => (c, None),
                Err(e) => (Config::default(), Some(format!("{}: {e}", path.display()))),
            },
            Err(_) => (Config::default(), None),
        }
    }

    pub fn save(&self) -> anyhow::Result<PathBuf> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(path)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub hotlist: Vec<HotlistEntry>,
    pub cmd_history: Vec<String>,
    pub dir_history: Vec<String>,
    pub input_history: HashMap<String, Vec<String>>,
    pub left_dir: Option<String>,
    pub right_dir: Option<String>,
    /// Panel index showing the quick view at exit.
    pub quick_view_side: Option<usize>,
    /// Frecent directories (zoxide-like), for Alt-J and the `z` builtin.
    #[serde(default)]
    pub frecent: Vec<crate::frecency::DirVisit>,
    /// Open tabs per panel side (local directories).
    #[serde(default)]
    pub tabs: Vec<crate::tabs::SavedTabs>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HotlistEntry {
    pub name: String,
    pub path: String,
}

const HISTORY_MAX: usize = 200;

impl State {
    fn path() -> Option<PathBuf> {
        data_dir().map(|d| d.join("state.toml"))
    }
    pub fn load() -> State {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) {
        if let Some(p) = Self::path() {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            if let Ok(t) = toml::to_string(self) {
                let _ = std::fs::write(p, t);
            }
        }
    }
    pub fn push_history(list: &mut Vec<String>, item: &str) {
        if item.trim().is_empty() {
            return;
        }
        list.retain(|x| x != item);
        list.push(item.to_string());
        if list.len() > HISTORY_MAX {
            let n = list.len() - HISTORY_MAX;
            list.drain(..n);
        }
    }
    pub fn input_hist(&mut self, key: &str) -> &mut Vec<String> {
        self.input_history.entry(key.to_string()).or_default()
    }
}

pub const SAMPLE_CONFIG: &str = r#"# pairdeck configuration. Delete a line to get the default back.
theme = "dark"            # dark | classic | nord | gruvbox | light | <name of themes/<name>.toml>
nerd_fonts = true         # file type icons (needs a Nerd Font in your terminal)
image_protocol = "auto"   # auto | kitty | sixel | iterm2 | halfblocks
use_trash = true          # F8 offers "move to trash" first
show_menubar = true
mouse = true

[keys]
# Rebind any command. Chords are written with a space: "ctrl-x d".
# "F5" = "copy"
# "ctrl-p" = "find_file"
"#;
