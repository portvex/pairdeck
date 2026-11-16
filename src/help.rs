//! F1 help screen: overview plus the live key binding table.

use crate::app::App;
use crate::keymap::Command;
use crate::screen::{Screen, ScreenOutcome};
use crate::ui::Ctx;
use crate::util::pad_right;
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};
use std::any::Any;

const INTRO: &str = "\
pairdeck is a twin-panel file manager in the spirit of Midnight Commander.

The active panel has the highlighted border. Tab switches panels. Type to
write on the command line; Enter runs it in the active directory. With an
empty command line Enter opens directories, archives and files.

PANEL KEYS
  Up/Down PgUp/PgDn Home/End   move the cursor
  Enter / Backspace            enter directory / go to parent
  Insert, Ctrl-T               mark file;  + - *  select / unselect / invert
  Left/Right                   move between columns in brief mode

PREVIEW
  Ctrl-Q, Alt-Q or Ctrl-X q (or click 'Preview' on the menu bar) shows a live
  preview of the file under the cursor in the other half of the screen:
  images, code, Markdown, archives, PDF, video. Click [x] to close it.

GIT
  Inside a repository each file shows a status marker (M modified, S staged,
  A added, ? untracked, D deleted, R renamed, U conflict, ! ignored; Nerd Font
  glyphs when icons are on) and the panel border shows branch, ahead/behind
  and * when dirty. The quick view shows git diff HEAD for modified files
  (d toggles it while the preview is focused).
  Ctrl-G s / u / r     stage / unstage / discard changes of the selection
  Ctrl-G d             side-by-side diff of the file against HEAD
  Ctrl-G l / b         log / blame in the viewer
  Ctrl-G c / g         commit dialog / git status summary

IMAGE ZOOM (in the preview)
  Mouse wheel over the image   zoom in/out toward the pointer
  Drag                         pan the zoomed image
  Double-click                 zoom 2x at the pointer / back to fit
  Alt-= Alt--  Alt-0           zoom in, out, fit (from the file list)
  Alt-arrows                   pan (from the file list)
  + - 0 and arrows / hjkl      same, when the preview panel is focused (Tab)
  The F3 viewer zooms images and SVGs the same way (wheel, drag, + - 0).

PREVIEW CONTENT (keys work when the preview panel is focused)
  CSV / TSV        aligned table; Left/Right scroll wide tables and lines
  JSON             pretty printed; t switches to a collapsible tree
                   (Up/Down move, Left/Right collapse/expand, Enter toggles)
  TOML             invalid files show the parse error on top
  Photos           EXIF summary (camera, lens, exposure, GPS) and orientation
  Fonts            sample sheet with family, style and glyph count
  GIF/APNG/WebP    animations play; Space pauses (zoom is off while animating)
  Executables      ELF / Mach-O / PE summary: arch, type, libraries, sections
  Documents        text of docx, odt, ods, odp, pptx, xlsx and epub files
  Plain text/logs  URLs, dates, numbers, paths and log levels are colored

FUZZY FINDER AND DIRECTORY JUMPS
  Ctrl-P        fuzzy find files below the current directory, with preview
                (respects .gitignore; Alt-H hidden, Alt-I gitignore, Alt-D dirs)
                Enter go there, F3/F4 view/edit, Ctrl-O other panel,
                Tab mark, F2 or Ctrl-Space panelize, Alt-Enter path to cmdline
  Alt-J         jump to a frequently / recently used directory
  z TERMS       on the command line: jump to the best matching frecent dir
  zi            interactive list of frecent directories

TERMINAL PANE
  A real shell in a split below the panels (or click 'Terminal' on the menu bar).
  Alt-Z            show / focus the terminal; from the terminal: back to panels
  Ctrl-X Z         maximize / restore      Ctrl-X K   hide (shell keeps running)
  Ctrl-X + / -     taller / shorter (or drag the pane's top border)
  Shift-PgUp/PgDn  scroll back (mouse wheel too)
  Ctrl-X [         cd the shell to the panel directory
  Ctrl-X ]         move the panel to the shell's directory
  Ctrl-X Ctrl-V    type the selected paths into the shell
  Ctrl-X Ctrl-Z    show the shell AS A PANEL (in the other panel) / back to files
                   (also Left/Right menu > Terminal; [x] in its border closes it).
                   Tab, Alt-Z or a click move focus between shell and file panel;
                   it is the same shell as the bottom pane.
  While the terminal is focused every other key goes to the shell, so vim,
  less and htop work inside it. 'exit' closes the pane. Options: follow the
  panel directory automatically; config terminal_shell picks the program.

MOUSE
  Click selects, double-click opens, right-click marks, wheel scrolls.
  Click the menu bar, the function key bars (also in the viewer and editor)
  and [x] in the top-right corner of any full-screen view to close it.
  macOS: Option+letter works as Alt (config: macos_option_as_alt).

VIRTUAL FILE SYSTEMS
  Enter on an archive (zip, tar, tar.gz/bz2/xz/zst, 7z, gz, bz2, xz, zst...)
  browses it. Leave with '..' at the archive root. Local zip, tar* and 7z
  archives are writable: copy files in with F5, F7 mkdir, F8 delete, chmod,
  symlinks; the archive is rewritten safely when the operation finishes.
  Remote: Ctrl-X Ctrl-R (or Left/Right menu > Remote link) for SFTP, S3 or
  WebDAV, or type on the command line:
    cd sftp://user@host:port/path
    cd s3://bucket/prefix          (credentials from AWS_* or ~/.aws)
    cd davs://user@host/path       (dav:// = http, davs:// = https)
  Password-protected zip files (ZipCrypto, AES-128/192/256) show a lock on
  encrypted entries and ask for the password when you open, view, edit or
  copy them. The password is kept in memory for the session only. Encrypted
  zip files are read-only.
  ISO images (.iso, and .img files that are ISO 9660) browse like archives,
  with Joliet and Rock Ridge long names, permissions and symlinks.
  Disk images (.dmg): Enter mounts them read-only (macOS, via hdiutil; asks
  for the password of encrypted images). They are detached when you leave
  them and when pairdeck exits. Elsewhere they are read through 7-Zip if
  installed. The quick view shows image information without mounting.

TABS
  Ctrl-X n new tab, Ctrl-X w close, Ctrl-X . / Ctrl-X , next / previous,
  Alt-1..9 jump to a tab (Option-1..9 on macOS). Click a tab in the border.
  Tabs are remembered across restarts.

FILE OPERATIONS
  Ctrl-X m   bulk rename: edit the names of the selection as a list
  Ctrl-X e   regex rename with live preview ($1 groups, name/stem/extension)
  Ctrl-X u   undo the last rename, move, copy, mkdir, symlink or trash
  Ctrl-X U   undo history (undo several steps at once)
  Ctrl-X =   find duplicate files (size, then BLAKE3 hash)
  Ctrl-X b   disk usage analyzer (largest first, trash from there)
  Ctrl-X Y   synchronize the left and right directories (recursive):
             left -> right, right -> left or two-way (newer wins); compare by
             size and time, size, or content checksum; optional mirror deletes;
             review and edit the plan before anything changes
  Slow directories load in the background; Esc cancels a pending listing.

IMAGE ACTIONS (marked images or the image under the cursor)
  Ctrl-X g   Image actions: rotate, flip, resize, convert, strip, auto-orient
  Alt-R      rotate right 90 degrees      Alt-L  rotate left 90 degrees
  JPEG rotation only rewrites the EXIF orientation tag (lossless). Resize and
  convert can overwrite, add a suffix ({w}x{h}) or write into a directory.
  Ctrl-X u undoes image actions (originals are kept as backups).

CONFIGURATION
  ~/.config/pairdeck/config.toml      settings and [keys] rebinding
  ~/.config/pairdeck/themes/*.toml    custom themes
  ~/.config/pairdeck/menu             user menu (F2)
  ~/.config/pairdeck/assoc.toml       file associations (Enter / F3 / F4)
  Edits to these files apply immediately (live reload). Files with errors
  are reported and the previous settings stay in effect.
  Options > Key bindings... rebinds keys interactively: type to filter,
  Enter then press the new key or chord, Del removes, Ctrl-R resets all.

KEY BINDINGS (current)
";

pub struct HelpScreen {
    lines: Vec<String>,
    scroll: usize,
    search: Option<String>,
    height: usize,
}

pub fn open(app: &mut App) {
    let mut lines: Vec<String> = INTRO.lines().map(String::from).collect();
    let mut rows: Vec<(String, String)> = Command::ALL
        .iter()
        .map(|c| {
            let mut keys: Vec<String> = app
                .keymap
                .bindings
                .iter()
                .filter(|(_, v)| **v == *c)
                .map(|(k, _)| crate::keymap::chord_label(k))
                .collect();
            keys.sort_by_key(|k| (k.len(), k.clone()));
            (keys.join(", "), format!("{}  ({})", c.desc(), c.name()))
        })
        .collect();
    rows.sort_by_key(|a| a.0.is_empty());
    for (k, d) in rows {
        lines.push(format!("  {}  {}", pad_right(if k.is_empty() { "-" } else { &k }, 28), d));
    }
    lines.push(String::new());
    lines.push("Viewer (F3): / search, n next, h hex, w wrap, F10/q quit.".into());
    lines.push("Editor (F4): F2 save, F7 search, F4 replace, Ctrl-Z undo, F10 quit.".into());
    app.push_screen(Box::new(HelpScreen { lines, scroll: 0, search: None, height: 20 }));
}

impl Screen for HelpScreen {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(if ctx.classic { BorderType::Double } else { BorderType::Rounded })
            .border_style(ctx.theme.border_active)
            .style(ctx.theme.viewer)
            .title(Line::styled(" Help ", ctx.theme.dialog_title).centered())
            .title_bottom(Line::styled(" Esc/F10/q close  / search  n next ", ctx.theme.border).centered());
        let inner = block.inner(area);
        f.render_widget(block, area);
        self.height = inner.height as usize;
        let needle = self.search.as_deref().unwrap_or("").to_lowercase();
        let lines: Vec<Line> = self
            .lines
            .iter()
            .skip(self.scroll)
            .take(self.height)
            .map(|l| {
                let st = if l.chars().next().is_some_and(|c| c.is_ascii_uppercase()) && l.to_uppercase() == *l {
                    ctx.theme.header
                } else {
                    ctx.theme.viewer
                };
                if !needle.is_empty() && l.to_lowercase().contains(&needle) {
                    Line::from(Span::styled(l.clone(), ctx.theme.search_match))
                } else {
                    Line::from(Span::styled(l.clone(), st))
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
        if let Some(s) = &self.search {
            let r = Rect { y: area.bottom() - 1, height: 1, x: area.x + 2, width: area.width.saturating_sub(4) };
            f.render_widget(Paragraph::new(Span::styled(format!("Search: {s}"), ctx.theme.status)), r);
        }
    }

    fn key(&mut self, key: KeyEvent) -> ScreenOutcome {
        if let Some(s) = self.search.as_mut() {
            match key.code {
                KeyCode::Char(c) => s.push(c),
                KeyCode::Backspace => {
                    s.pop();
                }
                KeyCode::Enter => self.find_next(false),
                KeyCode::Esc => self.search = None,
                _ => {}
            }
            if !matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                self.find_next(true);
            }
            return ScreenOutcome::Keep;
        }
        let max = self.lines.len().saturating_sub(self.height);
        match key.code {
            KeyCode::Esc | KeyCode::F(10) | KeyCode::Char('q') | KeyCode::F(1) => return ScreenOutcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll = (self.scroll + 1).min(max),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(self.height),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll = (self.scroll + self.height).min(max),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = max,
            KeyCode::Char('/') => self.search = Some(String::new()),
            KeyCode::Char('n') => self.find_next(false),
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> ScreenOutcome {
        let max = self.lines.len().saturating_sub(self.height);
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => self.scroll = (self.scroll + 3).min(max),
            _ => {}
        }
        ScreenOutcome::Keep
    }

    fn title(&self) -> String {
        "Help".into()
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl HelpScreen {
    fn find_next(&mut self, include_current: bool) {
        let Some(s) = &self.search else { return };
        let needle = s.to_lowercase();
        if needle.is_empty() {
            return;
        }
        let start = if include_current { self.scroll } else { self.scroll + 1 };
        if let Some(i) =
            self.lines.iter().enumerate().skip(start).find(|(_, l)| l.to_lowercase().contains(&needle)).map(|(i, _)| i)
        {
            self.scroll = i.min(self.lines.len().saturating_sub(self.height));
        }
    }
}
