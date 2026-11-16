//! Commands and key bindings. Bindings are strings like "F5", "ctrl-x d", "alt-.".

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;

macro_rules! commands {
    ($($variant:ident => $name:literal, $desc:literal;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Command { $($variant),* }
        impl Command {
            pub const ALL: &'static [Command] = &[$(Command::$variant),*];
            pub fn name(self) -> &'static str { match self { $(Command::$variant => $name),* } }
            pub fn desc(self) -> &'static str { match self { $(Command::$variant => $desc),* } }
            pub fn from_name(s: &str) -> Option<Command> {
                match s { $($name => Some(Command::$variant),)* _ => None }
            }
        }
    };
}

commands! {
    Help => "help", "Help";
    UserMenu => "user_menu", "User menu";
    View => "view", "View file";
    ViewFile => "view_file", "View a file by name";
    Edit => "edit", "Edit file";
    NewFile => "new_file", "Create and edit a new file";
    Copy => "copy", "Copy";
    Move => "move", "Rename / move";
    RenameInPlace => "rename", "Rename in place";
    Mkdir => "mkdir", "Make directory";
    Delete => "delete", "Delete";
    Menu => "menu", "Pull-down menu";
    Quit => "quit", "Quit";
    ToggleMark => "toggle_mark", "Mark / unmark file";
    SelectGroup => "select_group", "Select group";
    UnselectGroup => "unselect_group", "Unselect group";
    InvertSelection => "invert_selection", "Invert selection";
    SwapPanels => "swap_panels", "Swap panels";
    TogglePanels => "toggle_panels", "Hide / show panels";
    SwitchPanel => "switch_panel", "Switch active panel";
    Refresh => "refresh", "Rescan directory";
    ToggleHidden => "toggle_hidden", "Show / hide hidden files";
    QuickSearch => "quick_search", "Quick search";
    QuickCd => "quick_cd", "Quick cd";
    Hotlist => "hotlist", "Directory hotlist";
    AddHotlist => "add_hotlist", "Add current dir to hotlist";
    DirHistory => "dir_history", "Directory history";
    HistoryBack => "history_back", "Previous directory";
    HistoryForward => "history_forward", "Next directory";
    CmdHistory => "cmd_history", "Command history";
    FindFile => "find_file", "Find file";
    CompareDirs => "compare_dirs", "Compare directories";
    CompareFiles => "compare_files", "Compare files (diff)";
    Chmod => "chmod", "Change permissions";
    Chown => "chown", "Change owner";
    Symlink => "symlink", "Create symlink";
    RelSymlink => "rel_symlink", "Create relative symlink";
    Hardlink => "hardlink", "Create hard link";
    EditSymlink => "edit_symlink", "Edit symlink target";
    QuickView => "quick_view", "Quick view (preview) in other panel";
    InfoPanel => "info_panel", "Info panel";
    TreePanel => "tree_panel", "Directory tree panel";
    CycleListing => "cycle_listing", "Cycle listing mode";
    SortMenu => "sort_menu", "Sort order";
    ReverseSort => "reverse_sort", "Reverse sort order";
    Filter => "filter", "Panel filter";
    OtherToCurrent => "other_to_cursor", "Open dir under cursor in other panel";
    OtherSameDir => "other_same_dir", "Other panel to same directory";
    PutFilename => "put_filename", "Copy file name to command line";
    PutPath => "put_path", "Copy full path to command line";
    PutCwd => "put_cwd", "Copy current dir to command line";
    PutOtherCwd => "put_other_cwd", "Copy other panel dir to command line";
    Subshell => "subshell", "Subshell (Ctrl-O)";
    DirSize => "dir_size", "Show directory size";
    Jobs => "jobs", "Background jobs";
    SftpConnect => "sftp_connect", "SFTP link";
    RemoteConnect => "remote_connect", "Remote link: SFTP, S3 or WebDAV";
    Panelize => "panelize", "External panelize";
    OpenExternal => "open_external", "Open with system default app";
    CompressFiles => "compress", "Compress selection";
    Options => "options", "Configuration";
    ThemeCycle => "theme_cycle", "Next theme";
    Redraw => "redraw", "Redraw screen";
    EditConfig => "edit_config", "Edit config file";
    EditUserMenu => "edit_user_menu", "Edit user menu file";
    EditAssoc => "edit_assoc", "Edit file associations";
    CopyPathClipboard => "copy_path", "Copy path to system clipboard";
    ToggleTerminalPanel => "toggle_cmdline", "Show / hide command line";
    PreviewZoomIn => "preview_zoom_in", "Zoom in on the previewed image";
    PreviewZoomOut => "preview_zoom_out", "Zoom out of the previewed image";
    PreviewZoomReset => "preview_zoom_reset", "Fit the previewed image";
    PreviewPanLeft => "preview_pan_left", "Pan the zoomed preview left";
    PreviewPanRight => "preview_pan_right", "Pan the zoomed preview right";
    PreviewPanUp => "preview_pan_up", "Pan the zoomed preview up";
    PreviewPanDown => "preview_pan_down", "Pan the zoomed preview down";
    GitStage => "git_stage", "Git: stage selection";
    GitUnstage => "git_unstage", "Git: unstage selection";
    GitDiscard => "git_discard", "Git: discard changes in selection";
    GitDiff => "git_diff", "Git: diff file against HEAD";
    GitLog => "git_log", "Git: log of file or directory";
    GitBlame => "git_blame", "Git: blame file";
    GitCommit => "git_commit", "Git: commit";
    GitStatus => "git_status", "Git: status summary";
    FuzzyFind => "fuzzy_find", "Fuzzy find file (with preview)";
    JumpDir => "jump_dir", "Jump to a frecent directory";
    BulkRename => "bulk_rename", "Bulk rename selection in an editable list";
    RegexRename => "regex_rename", "Rename selection with a regular expression";
    Undo => "undo", "Undo the last file operation";
    UndoHistory => "undo_history", "Undo history";
    FindDuplicates => "find_duplicates", "Find duplicate files";
    DiskUsage => "disk_usage", "Disk usage analyzer";
    TabNew => "tab_new", "New tab";
    TabClose => "tab_close", "Close tab";
    TabNext => "tab_next", "Next tab";
    TabPrev => "tab_prev", "Previous tab";
    KeyBindings => "key_bindings", "Edit key bindings";
    SyncDirs => "sync_dirs", "Synchronize directories";
    ToggleTerminal => "toggle_terminal", "Embedded terminal: show / focus / back to panels";
    HideTerminal => "hide_terminal", "Embedded terminal: hide (shell keeps running)";
    TerminalMaximize => "terminal_maximize", "Embedded terminal: maximize / restore";
    TerminalGrow => "terminal_grow", "Embedded terminal: make taller";
    TerminalShrink => "terminal_shrink", "Embedded terminal: make shorter";
    TerminalCdPanel => "terminal_cd_panel", "Embedded terminal: cd to the panel directory";
    PanelToTerminalCwd => "panel_to_terminal_cwd", "Panel: go to the terminal's directory";
    TerminalSendSelection => "terminal_send_selection", "Embedded terminal: type selected paths";
    TerminalPanel => "terminal_panel", "Embedded terminal: show in the other panel / back to the file list";
    ImageActions => "image_actions", "Image actions (rotate, resize, convert...)";
    ImageRotateRight => "image_rotate_right", "Rotate image 90 degrees clockwise";
    ImageRotateLeft => "image_rotate_left", "Rotate image 90 degrees counter-clockwise";
    ImageRotate180 => "image_rotate_180", "Rotate image 180 degrees";
    ImageFlipH => "image_flip_h", "Flip image horizontally";
    ImageFlipV => "image_flip_v", "Flip image vertically";
    ImageResize => "image_resize", "Resize images";
    ImageConvert => "image_convert", "Convert image format";
    ImageStrip => "image_strip", "Strip image metadata";
    ImageAutoOrient => "image_auto_orient", "Apply EXIF orientation to image pixels";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Key {
    pub fn from_event(ev: &KeyEvent) -> Key {
        let mut mods = ev.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let mut code = ev.code;
        // Normalize: shifted characters carry the shift in the char itself.
        if let KeyCode::Char(c) = code {
            if mods.contains(KeyModifiers::SHIFT) && !mods.contains(KeyModifiers::CONTROL) && !c.is_ascii_alphabetic() {
                mods.remove(KeyModifiers::SHIFT);
            }
            if mods.contains(KeyModifiers::CONTROL) || mods.contains(KeyModifiers::ALT) {
                if c.is_ascii_uppercase() {
                    code = KeyCode::Char(c.to_ascii_lowercase());
                    mods.insert(KeyModifiers::SHIFT);
                }
            } else {
                mods.remove(KeyModifiers::SHIFT);
            }
        }
        if code == KeyCode::BackTab {
            code = KeyCode::Tab;
            mods.insert(KeyModifiers::SHIFT);
        }
        Key { code, mods }
    }

    pub fn parse(s: &str) -> Option<Key> {
        let mut mods = KeyModifiers::NONE;
        let mut rest = s.trim();
        loop {
            let lower = rest.to_ascii_lowercase();
            if let Some(r) = lower.strip_prefix("ctrl-").or_else(|| lower.strip_prefix("c-")) {
                mods |= KeyModifiers::CONTROL;
                rest = &rest[rest.len() - r.len()..];
            } else if let Some(r) = lower.strip_prefix("alt-").or_else(|| lower.strip_prefix("m-")) {
                mods |= KeyModifiers::ALT;
                rest = &rest[rest.len() - r.len()..];
            } else if let Some(r) = lower.strip_prefix("shift-").or_else(|| lower.strip_prefix("s-")) {
                mods |= KeyModifiers::SHIFT;
                rest = &rest[rest.len() - r.len()..];
            } else {
                break;
            }
        }
        let lower = rest.to_ascii_lowercase();
        let code = match lower.as_str() {
            "enter" | "return" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "esc" | "escape" => KeyCode::Esc,
            "space" => KeyCode::Char(' '),
            "backspace" | "bs" => KeyCode::Backspace,
            "insert" | "ins" => KeyCode::Insert,
            "delete" | "del" => KeyCode::Delete,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            f if f.starts_with('f') && f.len() > 1 && f[1..].chars().all(|c| c.is_ascii_digit()) => {
                KeyCode::F(f[1..].parse().ok()?)
            }
            _ => {
                let mut chars = rest.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                if c.is_ascii_uppercase() && !mods.is_empty() {
                    mods |= KeyModifiers::SHIFT;
                }
                KeyCode::Char(if mods.is_empty() { c } else { c.to_ascii_lowercase() })
            }
        };
        Some(Key { code, mods })
    }

    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            s.push_str("C-");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            s.push_str("M-");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            s.push_str("S-");
        }
        match self.code {
            KeyCode::F(n) => s.push_str(&format!("F{n}")),
            KeyCode::Char(' ') => s.push_str("Space"),
            KeyCode::Char(c) => s.push(c),
            KeyCode::Enter => s.push_str("Enter"),
            KeyCode::Tab => s.push_str("Tab"),
            KeyCode::Insert => s.push_str("Ins"),
            KeyCode::Delete => s.push_str("Del"),
            other => s.push_str(&format!("{other:?}")),
        }
        s
    }
}

pub type Chord = Vec<Key>;

pub fn parse_chord(s: &str) -> Option<Chord> {
    s.split_whitespace().map(Key::parse).collect()
}

pub fn chord_label(c: &Chord) -> String {
    c.iter().map(|k| k.label()).collect::<Vec<_>>().join(" ")
}

const DEFAULTS: &[(&str, &str)] = &[
    ("F1", "help"),
    ("F2", "user_menu"),
    ("F3", "view"),
    ("shift-F3", "view_file"),
    ("F4", "edit"),
    ("shift-F4", "new_file"),
    ("F5", "copy"),
    ("F6", "move"),
    ("shift-F6", "rename"),
    ("F7", "mkdir"),
    ("F8", "delete"),
    ("delete", "delete"),
    ("F9", "menu"),
    ("F10", "quit"),
    ("insert", "toggle_mark"),
    ("ctrl-t", "toggle_mark"),
    ("+", "select_group"),
    ("-", "unselect_group"),
    ("*", "invert_selection"),
    ("ctrl-u", "swap_panels"),
    ("tab", "switch_panel"),
    ("ctrl-r", "refresh"),
    ("alt-.", "toggle_hidden"),
    ("ctrl-s", "quick_search"),
    ("alt-s", "quick_search"),
    ("alt-c", "quick_cd"),
    ("ctrl-\\", "hotlist"),
    ("ctrl-x h", "add_hotlist"),
    ("alt-shift-h", "dir_history"),
    ("alt-y", "history_back"),
    ("alt-u", "history_forward"),
    ("alt-h", "cmd_history"),
    ("alt-?", "find_file"),
    ("ctrl-x d", "compare_dirs"),
    ("ctrl-x ctrl-d", "compare_files"),
    ("ctrl-x c", "chmod"),
    ("ctrl-x o", "chown"),
    ("ctrl-x s", "symlink"),
    ("ctrl-x v", "rel_symlink"),
    ("ctrl-x l", "hardlink"),
    ("ctrl-x ctrl-s", "edit_symlink"),
    ("ctrl-x q", "quick_view"),
    ("alt-q", "quick_view"),
    ("ctrl-q", "quick_view"),
    ("alt-=", "preview_zoom_in"),
    ("alt-+", "preview_zoom_in"),
    ("alt--", "preview_zoom_out"),
    ("alt-0", "preview_zoom_reset"),
    ("alt-left", "preview_pan_left"),
    ("alt-right", "preview_pan_right"),
    ("alt-up", "preview_pan_up"),
    ("alt-down", "preview_pan_down"),
    ("ctrl-g s", "git_stage"),
    ("ctrl-g u", "git_unstage"),
    ("ctrl-g r", "git_discard"),
    ("ctrl-g d", "git_diff"),
    ("ctrl-g l", "git_log"),
    ("ctrl-g b", "git_blame"),
    ("ctrl-g c", "git_commit"),
    ("ctrl-g g", "git_status"),
    ("ctrl-p", "fuzzy_find"),
    ("alt-j", "jump_dir"),
    ("ctrl-x m", "bulk_rename"),
    ("ctrl-x e", "regex_rename"),
    ("ctrl-x u", "undo"),
    ("ctrl-x U", "undo_history"),
    ("ctrl-x g", "image_actions"),
    ("alt-r", "image_rotate_right"),
    ("alt-l", "image_rotate_left"),
    ("ctrl-x =", "find_duplicates"),
    ("ctrl-x b", "disk_usage"),
    ("ctrl-x n", "tab_new"),
    ("ctrl-x w", "tab_close"),
    ("ctrl-x .", "tab_next"),
    ("ctrl-x ,", "tab_prev"),
    ("ctrl-x i", "info_panel"),
    ("ctrl-x t", "tree_panel"),
    ("alt-t", "cycle_listing"),
    ("ctrl-x r", "reverse_sort"),
    ("ctrl-x f", "filter"),
    ("alt-o", "other_to_cursor"),
    ("alt-i", "other_same_dir"),
    ("alt-enter", "put_filename"),
    ("ctrl-enter", "put_filename"),
    ("ctrl-x ctrl-j", "put_filename"),
    ("ctrl-shift-enter", "put_path"),
    ("ctrl-x ctrl-f", "put_path"),
    ("ctrl-x p", "put_cwd"),
    ("ctrl-x ctrl-p", "put_other_cwd"),
    ("ctrl-o", "subshell"),
    ("ctrl-space", "dir_size"),
    ("ctrl-x j", "jobs"),
    ("ctrl-x ctrl-t", "sftp_connect"),
    ("ctrl-x ctrl-r", "remote_connect"),
    ("ctrl-x Y", "sync_dirs"),
    ("alt-z", "toggle_terminal"),
    ("ctrl-x k", "hide_terminal"),
    ("ctrl-x Z", "terminal_maximize"),
    ("ctrl-x +", "terminal_grow"),
    ("ctrl-x -", "terminal_shrink"),
    ("ctrl-x [", "terminal_cd_panel"),
    ("ctrl-x ]", "panel_to_terminal_cwd"),
    ("ctrl-x ctrl-v", "terminal_send_selection"),
    ("ctrl-x ctrl-z", "terminal_panel"),
    ("ctrl-x !", "panelize"),
    ("ctrl-x x", "open_external"),
    ("ctrl-x a", "compress"),
    ("ctrl-x y", "copy_path"),
    ("ctrl-x ctrl-o", "toggle_panels"),
    ("alt-p", "quick_view"),
    ("ctrl-l", "redraw"),
    ("ctrl-x shift-t", "theme_cycle"),
];

pub struct KeyMap {
    pub bindings: HashMap<Chord, Command>,
    /// Pending prefix keys of a chord being typed.
    pub pending: Vec<Key>,
}

pub enum KeyResult {
    Command(Command),
    Pending,
    Unbound(Vec<Key>),
}

impl KeyMap {
    pub fn new(user: &HashMap<String, String>) -> (KeyMap, Vec<String>) {
        let mut bindings = HashMap::new();
        let mut errors = Vec::new();
        for (k, c) in DEFAULTS {
            if let (Some(ch), Some(cmd)) = (parse_chord(k), Command::from_name(c)) {
                bindings.insert(ch, cmd);
            }
        }
        for (k, c) in user {
            match (parse_chord(k), Command::from_name(c)) {
                (Some(ch), Some(cmd)) => {
                    bindings.insert(ch, cmd);
                }
                (Some(ch), None) if c == "none" || c.is_empty() => {
                    bindings.remove(&ch);
                }
                _ => errors.push(format!("bad key binding: {k} = {c}")),
            }
        }
        (KeyMap { bindings, pending: Vec::new() }, errors)
    }

    pub fn feed(&mut self, key: Key) -> KeyResult {
        self.pending.push(key);
        if let Some(cmd) = self.bindings.get(&self.pending) {
            let cmd = *cmd;
            self.pending.clear();
            return KeyResult::Command(cmd);
        }
        let is_prefix = self
            .bindings
            .keys()
            .any(|ch| ch.len() > self.pending.len() && ch[..self.pending.len()] == self.pending[..]);
        if is_prefix { KeyResult::Pending } else { KeyResult::Unbound(std::mem::take(&mut self.pending)) }
    }

    /// Shortest key label for a command (for menus and the button bar).
    pub fn label_for(&self, cmd: Command) -> Option<String> {
        self.bindings
            .iter()
            .filter(|(_, c)| **c == cmd)
            .map(|(k, _)| chord_label(k))
            .min_by_key(|s| (s.len(), s.clone()))
    }
}

/// The built-in bindings, before any user overrides.
pub fn default_bindings() -> HashMap<Chord, Command> {
    KeyMap::new(&HashMap::new()).0.bindings
}

/// A key in config-file syntax (`ctrl-x`, `alt-shift-h`, `F5`, `space`).
pub fn key_config(k: &Key) -> String {
    let mut s = String::new();
    if k.mods.contains(KeyModifiers::CONTROL) {
        s.push_str("ctrl-");
    }
    if k.mods.contains(KeyModifiers::ALT) {
        s.push_str("alt-");
    }
    if k.mods.contains(KeyModifiers::SHIFT) {
        s.push_str("shift-");
    }
    match k.code {
        KeyCode::F(n) => s.push_str(&format!("F{n}")),
        KeyCode::Char(' ') => s.push_str("space"),
        KeyCode::Char(c) => s.push(c),
        KeyCode::Enter => s.push_str("enter"),
        KeyCode::Tab => s.push_str("tab"),
        KeyCode::Esc => s.push_str("esc"),
        KeyCode::Backspace => s.push_str("backspace"),
        KeyCode::Insert => s.push_str("insert"),
        KeyCode::Delete => s.push_str("delete"),
        KeyCode::Home => s.push_str("home"),
        KeyCode::End => s.push_str("end"),
        KeyCode::PageUp => s.push_str("pageup"),
        KeyCode::PageDown => s.push_str("pagedown"),
        KeyCode::Up => s.push_str("up"),
        KeyCode::Down => s.push_str("down"),
        KeyCode::Left => s.push_str("left"),
        KeyCode::Right => s.push_str("right"),
        other => s.push_str(&format!("{other:?}").to_ascii_lowercase()),
    }
    s
}

/// A chord in config-file syntax; round-trips through [`parse_chord`].
pub fn chord_config(c: &Chord) -> String {
    c.iter().map(key_config).collect::<Vec<_>>().join(" ")
}

/// The `[keys]` table that turns the defaults into `bindings`: changed or added
/// chords map to their command, removed defaults map to "none".
pub fn overrides(bindings: &HashMap<Chord, Command>) -> std::collections::BTreeMap<String, String> {
    let defaults = default_bindings();
    let mut out = std::collections::BTreeMap::new();
    for (ch, cmd) in bindings {
        if defaults.get(ch) != Some(cmd) {
            out.insert(chord_config(ch), cmd.name().to_string());
        }
    }
    for ch in defaults.keys() {
        if !bindings.contains_key(ch) {
            out.insert(chord_config(ch), "none".to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn chord_config_round_trips_and_overrides_rebuild_the_map() {
        let defaults = default_bindings();
        for ch in defaults.keys() {
            assert_eq!(parse_chord(&chord_config(ch)).as_ref(), Some(ch), "{}", chord_config(ch));
        }
        assert!(overrides(&defaults).is_empty());
        let mut changed = defaults.clone();
        changed.remove(&parse_chord("F5").unwrap());
        changed.insert(parse_chord("ctrl-alt-c").unwrap(), Command::Copy);
        let table: HashMap<String, String> = overrides(&changed).into_iter().collect();
        assert_eq!(table.get("F5").map(String::as_str), Some("none"));
        let (rebuilt, errors) = KeyMap::new(&table);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(rebuilt.bindings, changed);
    }

    fn feed(map: &mut KeyMap, code: KeyCode, mods: KeyModifiers) -> Option<Command> {
        match map.feed(Key::from_event(&KeyEvent::new(code, mods))) {
            KeyResult::Command(c) => Some(c),
            _ => None,
        }
    }

    #[test]
    fn default_bindings_resolve() {
        let (mut m, errors) = KeyMap::new(&HashMap::new());
        assert!(errors.is_empty());
        assert_eq!(feed(&mut m, KeyCode::Char('q'), KeyModifiers::CONTROL), Some(Command::QuickView));
        assert_eq!(feed(&mut m, KeyCode::F(5), KeyModifiers::NONE), Some(Command::Copy));
        assert_eq!(feed(&mut m, KeyCode::Char('='), KeyModifiers::ALT), Some(Command::PreviewZoomIn));
        assert_eq!(feed(&mut m, KeyCode::Char('-'), KeyModifiers::ALT), Some(Command::PreviewZoomOut));
        assert_eq!(feed(&mut m, KeyCode::Char('0'), KeyModifiers::ALT), Some(Command::PreviewZoomReset));
        assert_eq!(feed(&mut m, KeyCode::Char('x'), KeyModifiers::CONTROL), None);
        assert_eq!(feed(&mut m, KeyCode::Char('d'), KeyModifiers::NONE), Some(Command::CompareDirs));
    }
}
