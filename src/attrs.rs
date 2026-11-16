//! chmod, chown and link creation dialogs.

use crate::app::App;
use crate::dialog::{self, Dialog, Form, Outcome, centered, draw_box};
use crate::ui::Ctx;
use crate::util::{self, shell_quote};
use crate::vfs::{self, Entry, EntryKind, Location, VfsHandle};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};
use std::any::Any;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Symlink,
    Relative,
    Hard,
}

// ------------------------------------------------------------------ chmod

const BITS: [(u32, &str); 12] = [
    (0o4000, "set user ID on execution"),
    (0o2000, "set group ID on execution"),
    (0o1000, "sticky bit"),
    (0o0400, "read by owner"),
    (0o0200, "write by owner"),
    (0o0100, "execute/search by owner"),
    (0o0040, "read by group"),
    (0o0020, "write by group"),
    (0o0010, "execute/search by group"),
    (0o0004, "read by others"),
    (0o0002, "write by others"),
    (0o0001, "execute/search by others"),
];

const BUTTONS: [&str; 4] = ["Set", "Set all", "Skip", "Cancel"];

pub struct ChmodDialog {
    loc: Location,
    files: Vec<Entry>,
    idx: usize,
    mode: u32,
    octal: String,
    focus: usize,
    recursive: bool,
    has_dirs: bool,
    errors: Vec<String>,
    changed: usize,
    hits: Vec<(Rect, usize)>,
}

pub fn chmod(app: &mut App) {
    let files = app.panel().selection();
    if files.is_empty() {
        return;
    }
    let loc = app.panel().loc.clone();
    let has_dirs = files.iter().any(|e| matches!(e.kind, EntryKind::Dir));
    let mode = files[0].mode & 0o7777;
    app.push_dialog(Box::new(ChmodDialog {
        loc,
        files,
        idx: 0,
        mode,
        octal: String::new(),
        focus: 12 + usize::from(has_dirs),
        recursive: false,
        has_dirs,
        errors: Vec::new(),
        changed: 0,
        hits: Vec::new(),
    }));
}

impl ChmodDialog {
    fn rows(&self) -> usize {
        12 + usize::from(self.has_dirs)
    }

    fn load_current(&mut self) {
        if let Some(e) = self.files.get(self.idx) {
            self.mode = e.mode & 0o7777;
            self.octal.clear();
        }
    }

    fn apply_one(&mut self, e: &Entry) {
        let path = self.loc.join(&e.name);
        let v = self.loc.vfs.clone();
        let mode = self.mode;
        if self.recursive && matches!(e.kind, EntryKind::Dir) {
            apply_recursive(&v, &path, mode, &mut self.errors, &mut self.changed);
        } else {
            match v.lock().set_mode(&path, mode) {
                Ok(()) => self.changed += 1,
                Err(err) => self.errors.push(format!("{err:#}")),
            }
        }
    }

    fn finish(&mut self) -> Outcome {
        let errors = std::mem::take(&mut self.errors);
        let changed = self.changed;
        Outcome::CloseThen(Box::new(move |app: &mut App| {
            app.panel_mut().marked.clear();
            app.reload_panels();
            if errors.is_empty() {
                app.info(format!("Permissions changed on {changed} item(s)"));
            } else {
                app.push_dialog(dialog::error(&errors.iter().take(15).cloned().collect::<Vec<_>>().join("\n")));
            }
        }))
    }

    fn press(&mut self, b: usize) -> Outcome {
        match b {
            0 => {
                if let Some(e) = self.files.get(self.idx).cloned() {
                    self.apply_one(&e);
                }
                self.idx += 1;
                if self.idx >= self.files.len() {
                    return self.finish();
                }
                self.load_current();
            }
            1 => {
                let rest: Vec<Entry> = self.files[self.idx..].to_vec();
                for e in rest {
                    self.apply_one(&e);
                }
                return self.finish();
            }
            2 => {
                self.idx += 1;
                if self.idx >= self.files.len() {
                    return self.finish();
                }
                self.load_current();
            }
            _ => {
                if self.changed > 0 || !self.errors.is_empty() {
                    return self.finish();
                }
                return Outcome::Close;
            }
        }
        Outcome::Keep
    }

    fn activate(&mut self, slot: usize) -> Option<Outcome> {
        if slot < 12 {
            self.mode ^= BITS[slot].0;
            self.octal.clear();
        } else if slot < self.rows() {
            self.recursive = !self.recursive;
        } else {
            return Some(self.press(slot - self.rows()));
        }
        None
    }
}

/// Apply `mode` recursively. Files get execute bits only if they already had one
/// (like chmod's X), directories always get the mode as given.
fn apply_recursive(v: &VfsHandle, p: &Path, mode: u32, errors: &mut Vec<String>, changed: &mut usize) {
    match v.lock().set_mode(p, mode) {
        Ok(()) => *changed += 1,
        Err(e) => errors.push(format!("{e:#}")),
    }
    let list = match v.lock().list(p) {
        Ok(l) => l,
        Err(e) => {
            errors.push(format!("{e:#}"));
            return;
        }
    };
    for c in list {
        let cp = p.join(&c.name);
        match c.kind {
            EntryKind::Dir => apply_recursive(v, &cp, mode, errors, changed),
            EntryKind::Symlink { .. } => {}
            _ => {
                let m = if c.mode & 0o111 != 0 { mode } else { mode & !0o111 };
                match v.lock().set_mode(&cp, m) {
                    Ok(()) => *changed += 1,
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
        }
    }
}

impl Dialog for ChmodDialog {
    fn render(&mut self, f: &mut Frame, area: Rect, ctx: &Ctx) {
        let h = 12 + 9 + u16::from(self.has_dirs);
        let r = centered(area, 56, h);
        let inner = draw_box(f, r, "Chmod command", ctx, false);
        let base = ctx.theme.dialog;
        self.hits.clear();
        let e = self.files.get(self.idx);
        let name = e.map(|e| e.name.clone()).unwrap_or_default();
        let w = inner.width as usize;
        let header = if self.files.len() > 1 {
            format!("File {}/{}: {}", self.idx + 1, self.files.len(), util::trunc_mid(&name, w.saturating_sub(14)))
        } else {
            format!("File: {}", util::trunc_mid(&name, w.saturating_sub(6)))
        };
        f.render_widget(Paragraph::new(Span::styled(header, ctx.theme.dialog_title)), Rect { height: 1, ..inner });
        let owner = e
            .map(|e| format!("Owner: {}  Group: {}", util::user_name(e.uid), util::group_name(e.gid)))
            .unwrap_or_default();
        f.render_widget(Paragraph::new(Span::styled(owner, base)), Rect { y: inner.y + 1, height: 1, ..inner });
        let mut y = inner.y + 3;
        for (i, (bit, label)) in BITS.iter().enumerate() {
            let on = self.mode & bit != 0;
            let st = if self.focus == i { ctx.theme.dialog_focus } else { base };
            let text = format!("[{}] {label}", if on { 'x' } else { ' ' });
            let rr = Rect { y, height: 1, width: (text.len() as u16).min(inner.width), ..inner };
            f.render_widget(Paragraph::new(Span::styled(text, st)), rr);
            self.hits.push((rr, i));
            y += 1;
        }
        if self.has_dirs {
            let st = if self.focus == 12 { ctx.theme.dialog_focus } else { base };
            let text = format!(
                "[{}] recursive (files keep x only if already executable)",
                if self.recursive { 'x' } else { ' ' }
            );
            let rr = Rect { y, height: 1, ..inner };
            f.render_widget(Paragraph::new(Span::styled(util::trunc_end(&text, w), st)), rr);
            self.hits.push((rr, 12));
            y += 1;
        }
        y += 1;
        let oct = if self.octal.is_empty() { format!("{:04o}", self.mode) } else { format!("{}_", self.octal) };
        let perm = {
            let mut tmp = Entry::new("", EntryKind::File);
            tmp.mode = self.mode;
            tmp.perm_string()
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Octal: ", base),
                Span::styled(oct, ctx.theme.input_focus),
                Span::styled(format!("   {perm}   (type digits to set)"), base),
            ])),
            Rect { y, height: 1, ..inner },
        );
        y += 2;
        let total: u16 = BUTTONS.iter().map(|b| b.len() as u16 + 5).sum();
        let mut x = inner.x + inner.width.saturating_sub(total) / 2;
        let rows = self.rows();
        for (k, b) in BUTTONS.iter().enumerate() {
            let text = if k == 0 { format!("[< {b} >]") } else { format!("[ {b} ]") };
            let st = if self.focus == rows + k { ctx.theme.dialog_focus } else { base };
            let tw = text.len() as u16;
            let rr = Rect { x, y: y.min(inner.bottom().saturating_sub(1)), width: tw, height: 1 };
            f.render_widget(Paragraph::new(Span::styled(text, st)), rr);
            self.hits.push((rr, rows + k));
            x += tw + 1;
        }
    }

    fn key(&mut self, key: KeyEvent) -> Outcome {
        let n = self.rows() + BUTTONS.len();
        match key.code {
            KeyCode::Esc => return self.press(3),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return self.press(3),
            KeyCode::Up | KeyCode::BackTab => self.focus = (self.focus + n - 1) % n,
            KeyCode::Down | KeyCode::Tab => self.focus = (self.focus + 1) % n,
            KeyCode::Left if self.focus >= self.rows() => {
                self.focus = if self.focus == self.rows() { n - 1 } else { self.focus - 1 }
            }
            KeyCode::Right if self.focus >= self.rows() => {
                self.focus = if self.focus == n - 1 { self.rows() } else { self.focus + 1 }
            }
            KeyCode::Char(' ') => {
                if let Some(o) = self.activate(self.focus) {
                    return o;
                }
            }
            KeyCode::Enter => {
                if self.focus >= self.rows() {
                    return self.press(self.focus - self.rows());
                }
                return self.press(0);
            }
            KeyCode::Char(c @ '0'..='7') => {
                if self.octal.len() >= 4 {
                    self.octal.clear();
                }
                self.octal.push(c);
                if let Ok(m) = u32::from_str_radix(&self.octal, 8) {
                    self.mode = m & 0o7777;
                }
                if self.octal.len() == 4 {
                    self.octal.clear();
                }
            }
            KeyCode::Backspace => {
                self.octal.pop();
            }
            KeyCode::Char('a') => return self.press(1),
            KeyCode::Char('s') => return self.press(2),
            _ => {}
        }
        Outcome::Keep
    }

    fn mouse(&mut self, m: MouseEvent) -> Outcome {
        if let MouseEventKind::Down(MouseButton::Left) = m.kind
            && let Some((_, slot)) = self.hits.iter().find(|(r, _)| r.contains((m.column, m.row).into())).copied()
        {
            self.focus = slot;
            if let Some(o) = self.activate(slot) {
                return o;
            }
        }
        Outcome::Keep
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

// ------------------------------------------------------------------ chown

fn resolve_uid(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    s.parse().ok().or_else(|| uzers::get_user_by_name(s).map(|u| u.uid()))
}

fn resolve_gid(s: &str) -> Option<u32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    s.parse().ok().or_else(|| uzers::get_group_by_name(s).map(|g| g.gid()))
}

fn chown_walk(p: &Path, uid: Option<u32>, gid: Option<u32>, recursive: bool, errors: &mut Vec<String>, n: &mut usize) {
    let u = uid.map(nix::unistd::Uid::from_raw);
    let g = gid.map(nix::unistd::Gid::from_raw);
    match nix::unistd::fchownat(nix::fcntl::AT_FDCWD, p, u, g, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(()) => *n += 1,
        Err(e) => {
            let hint =
                if e == nix::errno::Errno::EPERM { " (operation not permitted; try running as root)" } else { "" };
            errors.push(format!("{}: {e}{hint}", p.display()));
        }
    }
    if recursive
        && std::fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false)
        && let Ok(rd) = std::fs::read_dir(p)
    {
        for d in rd.flatten() {
            chown_walk(&d.path(), uid, gid, true, errors, n);
        }
    }
}

fn user_hint() -> String {
    // Show a short list of human users to help typing.
    // SAFETY: all_users() iterates getpwent(), which is not thread-safe. It is only
    // called from the UI thread, and no other code in pairdeck enumerates passwd entries.
    #[allow(unsafe_code)]
    let mut names: Vec<String> = unsafe { uzers::all_users() }
        .filter(|u| u.uid() >= 500 || u.uid() == 0)
        .map(|u| u.name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('_'))
        .collect();
    names.sort();
    names.dedup();
    names.truncate(8);
    names.join(", ")
}

pub fn chown(app: &mut App) {
    let sel = app.panel().selection();
    if sel.is_empty() {
        return;
    }
    let Some(dir) = app.panel().loc.local_path().map(Path::to_path_buf) else {
        app.error("Chown works on local files only");
        return;
    };
    let first = &sel[0];
    let what = if sel.len() == 1 {
        format!("\"{}\"", util::trunc_mid(&first.name, 40))
    } else {
        format!("{} items", sel.len())
    };
    let names: Vec<String> = sel.iter().map(|e| e.name.clone()).collect();
    let has_dirs = sel.iter().any(|e| matches!(e.kind, EntryKind::Dir));
    let mut form = Form::new("Chown")
        .width(56)
        .text(&format!("Change owner of {what}"))
        .input("user", "Owner (name or uid, empty = keep):", &util::user_name(first.uid))
        .input("group", "Group (name or gid, empty = keep):", &util::group_name(first.gid))
        .text(&format!("Users: {}", user_hint()));
    if has_dirs {
        form = form.check("rec", "Recursive", false);
    }
    let form = form.buttons(&["Set", "Cancel"], 0).on_submit(move |app, r| {
        if r.button != 0 {
            return;
        }
        let (us, gs) = (r.text("user"), r.text("group"));
        let uid = resolve_uid(&us);
        let gid = resolve_gid(&gs);
        if !us.trim().is_empty() && uid.is_none() {
            app.error(format!("Unknown user: {us}"));
            return;
        }
        if !gs.trim().is_empty() && gid.is_none() {
            app.error(format!("Unknown group: {gs}"));
            return;
        }
        let mut errors = Vec::new();
        let mut n = 0;
        for name in &names {
            chown_walk(&dir.join(name), uid, gid, r.check("rec"), &mut errors, &mut n);
        }
        app.panel_mut().marked.clear();
        app.reload_panels();
        if errors.is_empty() {
            app.info(format!("Owner changed on {n} item(s)"));
        } else {
            app.error(errors.iter().take(15).cloned().collect::<Vec<_>>().join("\n"));
        }
    });
    app.push_dialog(Box::new(form));
}

// ------------------------------------------------------------------ links

/// Relative path from directory `from` to `to` (both absolute, normalized).
pub fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let a: Vec<Component> = from.components().collect();
    let b: Vec<Component> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..a.len() {
        out.push("..");
    }
    for c in &b[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

pub fn link(app: &mut App, kind: LinkKind) {
    let Some(e) = app.panel().current().cloned() else { return };
    if e.is_parent() {
        return;
    }
    let src_loc = app.panel().loc.clone();
    let other_loc = app.other().loc.clone();
    if kind == LinkKind::Hard && !(src_loc.is_local() && other_loc.is_local()) {
        app.error("Hard links work on local files only");
        return;
    }
    let existing = src_loc.join(&e.name).display().to_string();
    let base =
        Path::new(&e.name).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| e.name.clone());
    let link_default = other_loc.join(&base).display().to_string();
    let title = match kind {
        LinkKind::Symlink => "Symbolic link",
        LinkKind::Relative => "Relative symlink",
        LinkKind::Hard => "Hard link",
    };
    let form = Form::new(title)
        .width(64)
        .input("from", "Existing filename (filename symlink will point to):", &existing)
        .input("to", "Symbolic link filename:", &link_default)
        .buttons(&["OK", "Cancel"], 0)
        .on_submit(move |app, r| {
            if r.button != 0 {
                return;
            }
            let from = r.text("from");
            let to = r.text("to");
            if from.trim().is_empty() || to.trim().is_empty() {
                return;
            }
            let res = make_link(kind, &from, &to, &src_loc, &other_loc, &base);
            match res {
                Ok((msg, undo)) => {
                    if let Some(op) = undo {
                        app.undo.push(msg.clone(), op);
                    }
                    app.reload_panels();
                    app.info(msg);
                }
                Err(err) => app.error(format!("{err:#}")),
            }
        });
    app.push_dialog(Box::new(form));
}

fn make_link(
    kind: LinkKind,
    from: &str,
    to: &str,
    src: &Location,
    other: &Location,
    name: &str,
) -> anyhow::Result<(String, Option<crate::undo::UndoOp>)> {
    let from_p = PathBuf::from(util::expand_tilde(from.trim()));
    let to_raw = PathBuf::from(util::expand_tilde(to.trim()));
    let mut to_p = if to_raw.is_absolute() { to_raw } else { other.path.join(to_raw) };
    to_p = vfs::normalize(&to_p);
    // Pick the VFS the link lives in.
    let to_parent = to_p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
    let vfs = if !other.is_local() && to_parent.starts_with(&other.path) {
        other.vfs.clone()
    } else if !src.is_local() && to_parent.starts_with(&src.path) {
        src.vfs.clone()
    } else {
        VfsHandle::local()
    };
    if vfs.lock().stat(&to_p).map(|e| e.is_dir() && !e.is_symlink()).unwrap_or(false) {
        to_p = to_p.join(name);
    }
    match kind {
        LinkKind::Hard => {
            std::fs::hard_link(&from_p, &to_p)?;
            Ok((format!("Hard link {} created", to_p.display()), None))
        }
        LinkKind::Symlink => {
            vfs.lock().symlink(from.trim(), &to_p)?;
            let undo = crate::undo::UndoOp::Symlink { vfs: vfs.clone(), path: to_p.clone() };
            Ok((format!("Symlink {} -> {}", to_p.display(), from.trim()), Some(undo)))
        }
        LinkKind::Relative => {
            let abs_from =
                if from_p.is_absolute() { vfs::normalize(&from_p) } else { vfs::normalize(&src.path.join(&from_p)) };
            let link_dir = to_p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
            let rel = relative_path(&link_dir, &abs_from);
            let rel_s = rel.display().to_string();
            vfs.lock().symlink(&rel_s, &to_p)?;
            let undo = crate::undo::UndoOp::Symlink { vfs: vfs.clone(), path: to_p.clone() };
            Ok((format!("Symlink {} -> {}", to_p.display(), rel_s), Some(undo)))
        }
    }
}

pub fn edit_symlink(app: &mut App) {
    let Some(e) = app.panel().current().cloned() else { return };
    let Some(target) = e.link_target().map(String::from) else {
        app.flash_error(format!("\"{}\" is not a symbolic link", e.name));
        return;
    };
    let loc = app.panel().loc.clone();
    let initial = target.clone();
    let d = dialog::input(
        app,
        "Edit symlink",
        &format!("Symlink \"{}\" points to:", e.name),
        &initial,
        "symlink",
        move |app, new| {
            if new.trim().is_empty() || new == target {
                return;
            }
            let p = loc.join(&e.name);
            let res = {
                let mut v = loc.vfs.lock();
                v.remove_file(&p).and_then(|_| v.symlink(new.trim(), &p))
            };
            match res {
                Ok(()) => {
                    app.reload_panels();
                    app.info(format!("{} -> {}", e.name, shell_quote(new.trim())));
                }
                Err(err) => app.error(format!("{err:#}")),
            }
        },
    );
    app.push_dialog(d);
}
