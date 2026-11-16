//! F9 pull-down menu bar.

use super::{Dialog, Outcome, shadow};
use crate::app::App;
use crate::keymap::Command;
use crate::panel::{ListMode, PanelView};
use crate::ui::Ctx;
use crate::util::{pad_right, str_width};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use std::any::Any;
use std::rc::Rc;

type Act = Rc<dyn Fn(&mut App)>;

pub struct Item {
    label: String,
    key: String,
    act: Option<Act>,
}

#[allow(dead_code)]
pub struct Menu {
    pub title: String,
    items: Vec<Item>,
}

pub const TITLES: [&str; 5] = ["Left", "File", "Command", "Options", "Right"];

fn cmd(app: &App, label: &str, c: Command) -> Item {
    Item {
        label: label.into(),
        key: app.keymap.label_for(c).unwrap_or_default(),
        act: Some(Rc::new(move |a: &mut App| a.exec(c))),
    }
}

fn act(label: &str, key: &str, f: impl Fn(&mut App) + 'static) -> Item {
    Item { label: label.into(), key: key.into(), act: Some(Rc::new(f)) }
}

fn sep() -> Item {
    Item { label: String::new(), key: String::new(), act: None }
}

fn panel_menu(app: &App, side: usize) -> Menu {
    let p = &app.panels[side];
    let mark = |b: bool| if b { "* " } else { "  " };
    let mode_item = |label: &str, m: ListMode| {
        act(&format!("{}{label}", mark(p.view == PanelView::List && p.mode == m)), "", move |a: &mut App| {
            a.panels[side].view = PanelView::List;
            a.panels[side].mode = m;
        })
    };
    let view_item = |label: &str, v: PanelView| {
        act(&format!("{}{label}", mark(p.view == v)), "", move |a: &mut App| {
            let pn = &mut a.panels[side];
            if v == PanelView::Tree {
                if !pn.is_local() {
                    return;
                }
                pn.tree = Some(crate::tree::TreeState::new(&pn.loc.path.clone(), pn.show_hidden));
            }
            pn.view = if pn.view == v { PanelView::List } else { v };
            a.previewer.invalidate();
        })
    };
    Menu {
        title: TITLES[if side == 0 { 0 } else { 4 }].into(),
        items: vec![
            mode_item("File listing (full)", ListMode::Full),
            mode_item("Brief listing", ListMode::Brief),
            mode_item("Long listing", ListMode::Long),
            view_item("Info", PanelView::Info),
            view_item("Tree", PanelView::Tree),
            view_item("Quick view", PanelView::QuickView),
            act(&format!("{}Terminal", mark(p.view == PanelView::Terminal)), "", move |a: &mut App| {
                if a.panels[side].view == PanelView::Terminal {
                    crate::term::undock(a);
                } else {
                    crate::term::dock(a, side);
                }
            }),
            sep(),
            act("  Sort order...", "", move |a: &mut App| {
                a.active = side;
                a.exec(Command::SortMenu)
            }),
            act("  Filter...", "", move |a: &mut App| {
                a.active = side;
                a.exec(Command::Filter)
            }),
            act(&format!("{}Show hidden files", mark(p.show_hidden)), "", move |a: &mut App| {
                a.active = side;
                a.exec(Command::ToggleHidden)
            }),
            sep(),
            act("  Remote link (SFTP / S3 / WebDAV)...", "", move |a: &mut App| {
                a.active = side;
                a.exec(Command::RemoteConnect)
            }),
            act("  SFTP link...", "", move |a: &mut App| {
                a.active = side;
                a.exec(Command::SftpConnect)
            }),
            act("  External panelize...", "", move |a: &mut App| {
                a.active = side;
                a.exec(Command::Panelize)
            }),
            act("  Rescan", "C-r", move |a: &mut App| a.panels[side].reload()),
            sep(),
            act("  New tab", "C-x n", move |a: &mut App| {
                a.active = side;
                a.exec(Command::TabNew)
            }),
            act("  Close tab", "C-x w", move |a: &mut App| {
                a.active = side;
                a.exec(Command::TabClose)
            }),
        ],
    }
}

pub fn build(app: &App) -> Vec<Menu> {
    use Command::*;
    let file = self::Menu {
        title: "File".into(),
        items: vec![
            cmd(app, "View", View),
            cmd(app, "View file...", ViewFile),
            cmd(app, "Edit", Edit),
            cmd(app, "Edit new file...", NewFile),
            cmd(app, "Open with default app", OpenExternal),
            cmd(app, "Copy", Copy),
            cmd(app, "Chmod", Chmod),
            cmd(app, "Chown", Chown),
            cmd(app, "Hard link", Hardlink),
            cmd(app, "Symlink", Symlink),
            cmd(app, "Relative symlink", RelSymlink),
            cmd(app, "Edit symlink", EditSymlink),
            cmd(app, "Rename/Move", Move),
            cmd(app, "Rename in place", RenameInPlace),
            cmd(app, "Mkdir", Mkdir),
            cmd(app, "Delete", Delete),
            cmd(app, "Compress...", CompressFiles),
            cmd(app, "Image actions...", ImageActions),
            cmd(app, "Rotate image right", ImageRotateRight),
            cmd(app, "Rotate image left", ImageRotateLeft),
            cmd(app, "Bulk rename...", BulkRename),
            cmd(app, "Regex rename...", RegexRename),
            cmd(app, "Undo", Undo),
            cmd(app, "Undo history...", UndoHistory),
            sep(),
            cmd(app, "Quick cd", QuickCd),
            cmd(app, "Select group", SelectGroup),
            cmd(app, "Unselect group", UnselectGroup),
            cmd(app, "Invert selection", InvertSelection),
            cmd(app, "Copy path to clipboard", CopyPathClipboard),
            sep(),
            cmd(app, "Exit", Quit),
        ],
    };
    let command = self::Menu {
        title: "Command".into(),
        items: vec![
            cmd(app, "User menu", UserMenu),
            cmd(app, "Directory tree", TreePanel),
            cmd(app, "Find file", FindFile),
            cmd(app, "Fuzzy find file", FuzzyFind),
            cmd(app, "Jump to frecent directory", JumpDir),
            cmd(app, "Swap panels", SwapPanels),
            cmd(app, "Switch panels on/off", TogglePanels),
            cmd(app, "Compare directories", CompareDirs),
            cmd(app, "Synchronize directories...", SyncDirs),
            cmd(app, "Compare files", CompareFiles),
            cmd(app, "External panelize", Panelize),
            cmd(app, "Show directory sizes", DirSize),
            cmd(app, "Disk usage analyzer", DiskUsage),
            cmd(app, "Find duplicate files", FindDuplicates),
            sep(),
            cmd(app, "Command history", CmdHistory),
            cmd(app, "Directory history", DirHistory),
            cmd(app, "Directory hotlist", Hotlist),
            cmd(app, "Add to hotlist", AddHotlist),
            cmd(app, "Background jobs", Jobs),
            sep(),
            cmd(app, "Edit user menu file", EditUserMenu),
            cmd(app, "Edit file associations", EditAssoc),
            sep(),
            cmd(app, "Terminal pane", ToggleTerminal),
            cmd(app, "Terminal maximize", TerminalMaximize),
            cmd(app, "Terminal: cd to panel dir", TerminalCdPanel),
            cmd(app, "Panel: go to terminal dir", PanelToTerminalCwd),
            cmd(app, "Terminal: type selected paths", TerminalSendSelection),
            cmd(app, "Hide terminal", HideTerminal),
            sep(),
            cmd(app, "Git status", GitStatus),
            cmd(app, "Git stage", GitStage),
            cmd(app, "Git unstage", GitUnstage),
            cmd(app, "Git discard changes", GitDiscard),
            cmd(app, "Git diff vs HEAD", GitDiff),
            cmd(app, "Git log", GitLog),
            cmd(app, "Git blame", GitBlame),
            cmd(app, "Git commit", GitCommit),
        ],
    };
    let options = self::Menu {
        title: "Options".into(),
        items: vec![
            cmd(app, "Configuration...", Options),
            cmd(app, "Next theme", ThemeCycle),
            cmd(app, "Edit config file", EditConfig),
            cmd(app, "Key bindings...", KeyBindings),
            cmd(app, "Show/hide command line", ToggleTerminalPanel),
            act("Save setup", "", |a: &mut App| match a.cfg.save() {
                Ok(p) => a.info(format!("Setup saved to {}", p.display())),
                Err(e) => a.error(e),
            }),
            sep(),
            cmd(app, "Help", Help),
        ],
    };
    vec![panel_menu(app, 0), file, command, options, panel_menu(app, 1)]
}

pub fn open(app: &mut App, idx: usize) {
    let menus = build(app);
    let titles = app.layout.menu_titles.clone();
    app.push_dialog(Box::new(MenuDialog { menus, current: idx.min(4), sel: 0, titles, drop_rect: Rect::default() }));
}

pub struct MenuDialog {
    menus: Vec<Menu>,
    current: usize,
    sel: usize,
    titles: Vec<(Rect, usize)>,
    drop_rect: Rect,
}

impl MenuDialog {
    fn fix_sel(&mut self, dir: isize) {
        let items = &self.menus[self.current].items;
        let n = items.len() as isize;
        let mut s = self.sel as isize;
        for _ in 0..n {
            if items[s.rem_euclid(n) as usize].act.is_some() {
                break;
            }
            s += dir;
        }
        self.sel = s.rem_euclid(n) as usize;
    }

    fn activate(&mut self) -> Outcome {
        let item = &self.menus[self.current].items[self.sel];
        match &item.act {
            Some(a) => {
                let a = a.clone();
                Outcome::CloseThen(Box::new(move |app| a(app)))
            }
            None => Outcome::Keep,
        }
    }
}

impl Dialog for MenuDialog {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        // Menubar line.
        let bar = Rect { height: 1, ..area };
        let mut spans = Vec::new();
        let mut x = 2u16;
        self.titles.clear();
        spans.push(Span::styled("  ", ctx.theme.menubar));
        for (i, t) in TITLES.iter().enumerate() {
            let label = format!(" {t} ");
            let w = str_width(&label) as u16;
            self.titles.push((Rect { x: area.x + x, y: area.y, width: w, height: 1 }, i));
            spans.push(Span::styled(label, if i == self.current { ctx.theme.menu_sel } else { ctx.theme.menubar }));
            spans.push(Span::styled("  ", ctx.theme.menubar));
            x += w + 2;
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(ctx.theme.menubar), bar);
        // Dropdown.
        let menu = &self.menus[self.current];
        let lw = menu.items.iter().map(|i| str_width(&i.label)).max().unwrap_or(10);
        let kw = menu.items.iter().map(|i| str_width(&i.key)).max().unwrap_or(0);
        let w = (lw + kw + 6) as u16;
        let h = menu.items.len() as u16 + 2;
        let tx = self.titles.get(self.current).map(|(r, _)| r.x).unwrap_or(area.x);
        let x = tx.min(area.right().saturating_sub(w + 1));
        let r = Rect { x, y: area.y + 1, width: w.min(area.width), height: h.min(area.height.saturating_sub(1)) };
        self.drop_rect = r;
        shadow(f, r);
        f.render_widget(Clear, r);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(if ctx.classic { BorderType::Double } else { BorderType::Rounded })
            .border_style(ctx.theme.menu)
            .style(ctx.theme.menu);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let iw = inner.width as usize;
        let lines: Vec<Line> = menu
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                if it.act.is_none() {
                    return Line::styled("─".repeat(iw), ctx.theme.menu);
                }
                let st = if i == self.sel { ctx.theme.menu_sel } else { ctx.theme.menu };
                let left = pad_right(&format!(" {}", it.label), iw.saturating_sub(kw + 1));
                Line::from(vec![
                    Span::styled(left, st),
                    Span::styled(pad_right(&it.key, kw + 1), if i == self.sel { st } else { ctx.theme.menu_hotkey }),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        let n = self.menus[self.current].items.len();
        match key.code {
            KeyCode::Esc | KeyCode::F(9) | KeyCode::F(10) => return Outcome::Close,
            KeyCode::Left => {
                self.current = (self.current + 4) % 5;
                self.sel = 0;
                self.fix_sel(1);
            }
            KeyCode::Right => {
                self.current = (self.current + 1) % 5;
                self.sel = 0;
                self.fix_sel(1);
            }
            KeyCode::Up => {
                self.sel = (self.sel + n - 1) % n;
                self.fix_sel(-1);
            }
            KeyCode::Down => {
                self.sel = (self.sel + 1) % n;
                self.fix_sel(1);
            }
            KeyCode::Home => {
                self.sel = 0;
                self.fix_sel(1);
            }
            KeyCode::End => {
                self.sel = n - 1;
                self.fix_sel(-1);
            }
            KeyCode::Enter => return self.activate(),
            KeyCode::Char(c) => {
                let c = c.to_ascii_lowercase();
                if let Some(i) = self.menus[self.current].items.iter().position(|it| {
                    it.act.is_some() && it.label.trim_start_matches(['*', ' ']).to_ascii_lowercase().starts_with(c)
                }) {
                    self.sel = i;
                    return self.activate();
                }
            }
            _ => {}
        }
        Outcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> Outcome {
        let pos = (m.column, m.row).into();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, i)) = self.titles.iter().find(|(r, _)| r.contains(pos)) {
                    self.current = *i;
                    self.sel = 0;
                    self.fix_sel(1);
                    return Outcome::Keep;
                }
                let r = self.drop_rect;
                if r.contains(pos) {
                    let idx = (m.row as i32 - r.y as i32 - 1) as usize;
                    if idx < self.menus[self.current].items.len() {
                        self.sel = idx;
                        return self.activate();
                    }
                    return Outcome::Keep;
                }
                return Outcome::Close;
            }
            MouseEventKind::Moved => {
                let r = self.drop_rect;
                if r.contains(pos) && m.row > r.y && m.row < r.bottom() - 1 {
                    let idx = (m.row - r.y - 1) as usize;
                    if self.menus[self.current].items.get(idx).is_some_and(|i| i.act.is_some()) {
                        self.sel = idx;
                    }
                }
            }
            _ => {}
        }
        Outcome::Keep
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
