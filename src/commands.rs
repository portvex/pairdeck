//! Implementation of panel commands.

use crate::app::{App, External};
use crate::config::HotlistEntry;
use crate::dialog::{self, Form, ListAction, ListDialog, ListItem};
use crate::keymap::Command;
use crate::ops::{CopyOpts, JobSpec, Overwrite};
use crate::panel::{PanelView, SortKey};
use crate::util::{self, shell_quote};
use crate::vfs::{self, Location};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

impl App {
    pub fn exec(&mut self, c: Command) {
        use Command::*;
        match c {
            Help => crate::help::open(self),
            UserMenu => crate::usermenu::open(self),
            View => self.view_current(),
            ViewFile => {
                let d = dialog::input(self, "View file", "File name:", "", "view_file", |app, name| {
                    let loc = app.panel().loc.clone();
                    let p = loc.path.join(util::expand_tilde(&name));
                    crate::viewer::open(app, &loc, &p);
                });
                self.push_dialog(d);
            }
            Edit => self.edit_current(),
            NewFile => {
                let d = dialog::input(self, "Edit new file", "File name:", "", "new_file", |app, name| {
                    if name.is_empty() {
                        return;
                    }
                    let loc = app.panel().loc.clone();
                    let p = loc.path.join(util::expand_tilde(&name));
                    app.open_editor(&loc, &p);
                });
                self.push_dialog(d);
            }
            Copy => self.copy_move_dialog(false, false),
            Move => self.copy_move_dialog(true, false),
            RenameInPlace => self.rename_dialog(),
            Mkdir => self.mkdir_dialog(),
            Delete => self.delete_dialog(),
            Menu => crate::dialog::menu::open(self, if self.active == 0 { 0 } else { 4 }),
            Quit => {
                let running = self.jobs.iter().filter(|j| !j.finished.load(Relaxed)).count();
                let dirty = self.screens.iter().any(|s| s.is_dirty());
                if running > 0 || dirty || self.cfg.confirm_exit {
                    let mut msg = String::from("Do you really want to quit?");
                    if running > 0 {
                        msg.push_str(&format!("\n{running} background job(s) will be aborted."));
                    }
                    self.push_dialog(dialog::confirm("Quit", &msg, |app| {
                        for j in &app.jobs {
                            j.cancel.store(true, Relaxed);
                        }
                        app.quit = true;
                    }));
                } else {
                    self.quit = true;
                }
            }
            ToggleMark => {
                self.panel_mut().toggle_mark_current();
                self.panel_mut().move_by(1);
            }
            SelectGroup | UnselectGroup => {
                let select = c == SelectGroup;
                let h = self.state.input_hist("select").clone();
                let d = Form::new(if select { "Select" } else { "Unselect" })
                    .input_hist("p", "Pattern (shell glob):", "*", "select", h)
                    .check("dirs", "Files and directories", false)
                    .check("case", "Case sensitive", false)
                    .buttons(&["OK", "Cancel"], 0)
                    .on_submit(move |app, r| {
                        if r.button == 0 {
                            let n =
                                app.panel_mut().mark_matching(&r.text("p"), select, r.check("dirs"), r.check("case"));
                            app.info(format!("{n} file(s) {}", if select { "selected" } else { "unselected" }));
                        }
                    });
                self.push_dialog(Box::new(d));
            }
            InvertSelection => self.panel_mut().invert_marks(),
            SwapPanels => {
                self.panels.swap(0, 1);
                self.tabs.swap(0, 1);
                self.active = 1 - self.active;
                self.setup_watcher();
            }
            TogglePanels => self.external = Some(External::ShowOutput),
            SwitchPanel => {
                self.active = 1 - self.active;
                crate::term::sync_focus(self);
                self.previewer.invalidate();
            }
            Refresh => self.reload_panels(),
            ToggleHidden => {
                let v = !self.panel().show_hidden;
                for p in &mut self.panels {
                    p.show_hidden = v;
                    p.tree = None;
                    p.reload();
                }
                self.cfg.show_hidden = v;
                self.info(if v { "Showing hidden files" } else { "Hiding hidden files" });
            }
            QuickSearch => {
                if self.panel().quick_search.is_none() {
                    self.panel_mut().quick_search = Some(String::new());
                } else {
                    let q = self.panel().quick_search.clone().unwrap_or_default();
                    self.panel_mut().quick_search_jump(&q, true);
                }
            }
            QuickCd => {
                let d = dialog::input(self, "Quick cd", "cd", "", "cd", |app, s| {
                    if !s.trim().is_empty() {
                        app.run_cmdline(&format!("cd {s}"));
                    }
                });
                self.push_dialog(d);
            }
            Hotlist => self.hotlist_dialog(),
            AddHotlist => self.add_hotlist(),
            DirHistory => {
                let items: Vec<ListItem> =
                    self.state.dir_history.iter().map(|d| ListItem::new(util::tildify(d))).collect();
                let hist = self.state.dir_history.clone();
                let d = ListDialog::new("Directory history", items, move |app, a| {
                    if let ListAction::Select(i) = a {
                        app.run_cmdline(&format!("cd {}", shell_quote(&hist[i])));
                    }
                })
                .select_last();
                self.push_dialog(Box::new(d));
            }
            HistoryBack => {
                if let Err(e) = self.panel_mut().history_back() {
                    self.error(e);
                }
                self.setup_watcher();
            }
            HistoryForward => {
                if let Err(e) = self.panel_mut().history_forward() {
                    self.error(e);
                }
                self.setup_watcher();
            }
            CmdHistory => {
                let items: Vec<ListItem> = self.state.cmd_history.iter().map(ListItem::new).collect();
                let hist = self.state.cmd_history.clone();
                let d = ListDialog::new("Command history", items, move |app, a| {
                    if let ListAction::Select(i) = a {
                        app.cmdline.set(&hist[i]);
                    }
                })
                .select_last();
                self.push_dialog(Box::new(d));
            }
            FindFile => crate::find::open(self),
            CompareDirs => crate::compare::compare_dirs(self),
            CompareFiles => crate::compare::compare_files(self),
            Chmod => crate::attrs::chmod(self),
            Chown => crate::attrs::chown(self),
            Symlink => crate::attrs::link(self, crate::attrs::LinkKind::Symlink),
            RelSymlink => crate::attrs::link(self, crate::attrs::LinkKind::Relative),
            Hardlink => crate::attrs::link(self, crate::attrs::LinkKind::Hard),
            EditSymlink => crate::attrs::edit_symlink(self),
            QuickView => self.toggle_other_view(PanelView::QuickView),
            InfoPanel => self.toggle_other_view(PanelView::Info),
            TreePanel => {
                let i = self.active;
                if self.panels[i].view == PanelView::Tree {
                    self.panels[i].view = PanelView::List;
                } else if self.panels[i].is_local() {
                    let start = self.panels[i].loc.path.clone();
                    let hidden = self.panels[i].show_hidden;
                    self.panels[i].tree = Some(crate::tree::TreeState::new(&start, hidden));
                    self.panels[i].view = PanelView::Tree;
                } else {
                    self.flash_error("Tree view is only available for local directories");
                }
            }
            CycleListing => {
                let p = self.panel_mut();
                p.mode = p.mode.next();
                let m = format!("Listing: {:?}", self.panel().mode);
                self.info(m);
            }
            SortMenu => self.sort_dialog(),
            ReverseSort => {
                let p = self.panel_mut();
                p.reverse = !p.reverse;
                p.resort();
            }
            Filter => {
                let cur = self.panel().filter.clone().unwrap_or_else(|| "*".into());
                let d =
                    dialog::input(self, "Filter", "Show files matching (empty = all):", &cur, "filter", |app, s| {
                        let p = app.panel_mut();
                        p.filter =
                            if s.trim().is_empty() || s.trim() == "*" { None } else { Some(s.trim().to_string()) };
                        p.reload();
                    });
                self.push_dialog(d);
            }
            OtherToCurrent => {
                let Some(e) = self.panel().current().cloned() else { return };
                let target = if e.is_dir() {
                    vfs::normalize(&self.panel().loc.join(&e.name))
                } else {
                    self.panel().loc.path.clone()
                };
                let loc = self.panel().loc.with_path(target);
                if let Err(err) = self.other_mut().chdir(loc, None) {
                    self.error(err);
                }
                self.setup_watcher();
            }
            OtherSameDir => {
                let loc = self.panel().loc.clone();
                let stack = self.panel().vfs_stack.clone();
                let o = self.other_mut();
                o.view = PanelView::List;
                if let Err(err) = o.chdir(loc, None) {
                    self.error(err);
                } else {
                    self.other_mut().vfs_stack = stack;
                }
                self.setup_watcher();
            }
            PutFilename => {
                if let Some(e) = self.panel().current() {
                    let n = if e.is_parent() { "..".to_string() } else { shell_quote(&e.name) };
                    self.cmdline.insert_str(&format!("{n} "));
                }
            }
            PutPath => {
                if let Some(p) = self.panel().current_path() {
                    let s = shell_quote(&p.display().to_string());
                    self.cmdline.insert_str(&format!("{s} "));
                }
            }
            PutCwd => {
                let s = shell_quote(&self.panel().loc.path.display().to_string());
                self.cmdline.insert_str(&format!("{s} "));
            }
            PutOtherCwd => {
                let s = shell_quote(&self.other().loc.path.display().to_string());
                self.cmdline.insert_str(&format!("{s} "));
            }
            Subshell => {
                let cwd = self.active_local_dir();
                self.external = Some(External::Shell { cwd });
            }
            DirSize => {
                let p = self.panel();
                let names: Vec<String> = if p.marked.is_empty() {
                    p.current()
                        .filter(|e| e.is_dir() && !e.is_parent())
                        .map(|e| vec![e.name.clone()])
                        .unwrap_or_default()
                } else {
                    p.selection().into_iter().filter(|e| e.is_dir()).map(|e| e.name).collect()
                };
                if names.is_empty() {
                    return;
                }
                let loc = p.loc.clone();
                self.start_job(JobSpec::DirSize { loc, names }, false);
                self.info("Computing directory sizes...");
            }
            Jobs => self.jobs_dialog(),
            SftpConnect => crate::vfs::sftp::connect_dialog(self),
            RemoteConnect => crate::vfs::connect::open(self),
            Panelize => {
                let d = dialog::input(
                    self,
                    "External panelize",
                    "Command (output = list of files):",
                    "find . -type f -newer .",
                    "panelize",
                    |app, cmd| {
                        app.panelize(&cmd);
                    },
                );
                self.push_dialog(d);
            }
            OpenExternal => {
                if let Some(p) = self.panel().current_path() {
                    if self.panel().is_local() {
                        match open::that_detached(&p) {
                            Ok(()) => self.info(format!("Opened {}", p.display())),
                            Err(e) => self.error(e),
                        }
                    } else {
                        self.flash_error("Open with default app works on local files only");
                    }
                }
            }
            CompressFiles => crate::vfs::archive::compress_dialog(self),
            Options => crate::options::open(self),
            ThemeCycle => {
                let names = crate::theme::BUILTIN;
                let i = names.iter().position(|n| *n == self.theme.name).map(|i| (i + 1) % names.len()).unwrap_or(0);
                self.cfg.theme = names[i].to_string();
                self.theme = crate::theme::Theme::load(names[i]);
                self.previewer.invalidate();
                self.info(format!("Theme: {}", names[i]));
            }
            Redraw => self.full_redraw = true,
            EditConfig => {
                if let Some(p) = crate::config::Config::path() {
                    if !p.exists() {
                        if let Some(d) = p.parent() {
                            let _ = std::fs::create_dir_all(d);
                        }
                        let _ = std::fs::write(&p, crate::config::SAMPLE_CONFIG);
                    }
                    let loc = Location::local(p.parent().unwrap_or(Path::new("/")));
                    self.open_editor(&loc, &p);
                }
            }
            EditUserMenu => crate::usermenu::edit_menu_file(self),
            EditAssoc => crate::assoc::edit_file(self),
            CopyPathClipboard => {
                let text = match self.panel().current_path() {
                    Some(p) => self.panel().loc.vfs.lock().display(&p),
                    None => self.panel().loc.display(),
                };
                osc52_copy(&text);
                self.info(format!("Copied: {text}"));
            }
            ToggleTerminalPanel => self.cmdline_hidden = !self.cmdline_hidden,
            GitStage | GitUnstage | GitDiscard | GitDiff | GitLog | GitBlame | GitCommit | GitStatus => {
                crate::git::exec(self, c)
            }
            FuzzyFind => crate::fuzzy::open_files(self),
            JumpDir => crate::fuzzy::open_dirs(self),
            BulkRename => crate::bulkrename::open(self),
            RegexRename => crate::bulkrename::regex_dialog(self),
            Undo => crate::undo::undo_last(self),
            UndoHistory => crate::undo::history(self),
            FindDuplicates => crate::dupes::open(self),
            DiskUsage => crate::diskusage::open(self),
            TabNew => crate::tabs::new_tab(self),
            TabClose => crate::tabs::close_tab(self),
            TabNext => crate::tabs::cycle(self, true),
            TabPrev => crate::tabs::cycle(self, false),
            KeyBindings => crate::keyedit::open(self),
            SyncDirs => crate::sync::open(self),
            ToggleTerminal => crate::term::toggle(self),
            HideTerminal => crate::term::hide(self),
            TerminalMaximize => crate::term::toggle_maximize(self),
            TerminalGrow => crate::term::resize_by(self, 5),
            TerminalShrink => crate::term::resize_by(self, -5),
            TerminalCdPanel => crate::term::cd_to_panel(self),
            PanelToTerminalCwd => crate::term::panel_to_cwd(self),
            TerminalSendSelection => crate::term::send_selection(self),
            TerminalPanel => crate::term::toggle_panel(self),
            ImageActions => crate::imageops::open(self),
            ImageRotateRight => crate::imageops::transform(self, crate::imageops::Transform::RotateRight),
            ImageRotateLeft => crate::imageops::transform(self, crate::imageops::Transform::RotateLeft),
            ImageRotate180 => crate::imageops::transform(self, crate::imageops::Transform::Rotate180),
            ImageFlipH => crate::imageops::transform(self, crate::imageops::Transform::FlipH),
            ImageFlipV => crate::imageops::transform(self, crate::imageops::Transform::FlipV),
            ImageResize => crate::imageops::resize_dialog(self),
            ImageConvert => crate::imageops::convert_dialog(self),
            ImageStrip => crate::imageops::strip(self),
            ImageAutoOrient => crate::imageops::auto_orient(self),
            PreviewZoomIn | PreviewZoomOut | PreviewZoomReset | PreviewPanLeft | PreviewPanRight | PreviewPanUp
            | PreviewPanDown => {
                if !self.panels.iter().any(|p| p.view == PanelView::QuickView) {
                    self.flash_error("Open the preview first (Ctrl-Q)");
                    return;
                }
                if !self.previewer.is_image() {
                    return;
                }
                match c {
                    PreviewZoomIn => self.previewer.zoom_in(),
                    PreviewZoomOut => self.previewer.zoom_out(),
                    PreviewZoomReset => self.previewer.zoom_reset(),
                    PreviewPanLeft => self.previewer.pan(-0.2, 0.0),
                    PreviewPanRight => self.previewer.pan(0.2, 0.0),
                    PreviewPanUp => self.previewer.pan(0.0, -0.2),
                    _ => self.previewer.pan(0.0, 0.2),
                }
            }
        }
    }

    fn toggle_other_view(&mut self, v: PanelView) {
        let o = self.other_mut();
        o.view = if o.view == v { PanelView::List } else { v };
        self.previewer.invalidate();
    }

    pub fn view_current(&mut self) {
        let Some(e) = self.panel().current().cloned() else { return };
        if e.is_dir() {
            self.enter();
            return;
        }
        if let Some(a) = crate::assoc::view_override(self, &e)
            && crate::assoc::run_action(self, &e, &a)
        {
            return;
        }
        let loc = self.panel().loc.clone();
        let path = loc.join(&e.name);
        if !self.cfg.use_internal_viewer && loc.is_local() {
            let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
            self.external = Some(External::Program {
                program: pager,
                args: vec![path.display().to_string()],
                cwd: loc.path.clone(),
            });
            return;
        }
        // Encrypted archive entries ask for the password first.
        let names = [e.name.clone()];
        crate::vfs::secrets::ensure_password(self, &loc.clone(), &names, move |app| {
            crate::viewer::open(app, &loc, &path)
        });
    }

    pub fn edit_current(&mut self) {
        let Some(e) = self.panel().current().cloned() else { return };
        if e.is_dir() {
            return;
        }
        if let Some(a) = crate::assoc::edit_override(self, &e)
            && crate::assoc::run_action(self, &e, &a)
        {
            return;
        }
        let loc = self.panel().loc.clone();
        let path = loc.join(&e.name);
        let names = [e.name.clone()];
        crate::vfs::secrets::ensure_password(self, &loc.clone(), &names, move |app| app.open_editor(&loc, &path));
    }

    pub fn open_editor(&mut self, loc: &Location, path: &Path) {
        if !self.cfg.use_internal_editor && loc.is_local() {
            let ed = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
            let mut parts = ed.split_whitespace().map(String::from);
            let program = parts.next().unwrap_or_else(|| "vi".into());
            let mut args: Vec<String> = parts.collect();
            args.push(path.display().to_string());
            self.external = Some(External::Program { program, args, cwd: loc.path.clone() });
            return;
        }
        crate::editor::open(self, loc, path);
    }

    // ------------------------------------------------------------ copy / move

    pub fn copy_move_dialog(&mut self, is_move: bool, same_panel: bool) {
        let sel = self.panel().selection();
        if sel.is_empty() {
            return;
        }
        let names: Vec<String> = sel.iter().map(|e| e.name.clone()).collect();
        // Copying out of an encrypted archive: get the password before the job starts.
        let src_loc = self.panel().loc.clone();
        let paths: Vec<std::path::PathBuf> = names.iter().map(|n| src_loc.join(n)).collect();
        let locked = src_loc.vfs.lock().needs_password(&paths);
        if locked {
            crate::vfs::secrets::ensure_password(self, &src_loc, &names, move |app| {
                app.copy_move_dialog(is_move, same_panel)
            });
            return;
        }
        let verb = if is_move { "Move" } else { "Copy" };
        let what = if names.len() == 1 {
            format!("\"{}\"", util::trunc_mid(&names[0], 40))
        } else {
            format!("{} files/directories", names.len())
        };
        let default_target = if same_panel {
            names[0].clone()
        } else {
            let o = self.other();
            let mut d = o.loc.display();
            if !d.ends_with('/') {
                d.push('/');
            }
            d
        };
        let hist_key = if is_move { "move" } else { "copy" };
        let hist = self.state.input_hist(hist_key).clone();
        let src = self.panel().loc.clone();
        let dst_panel_loc = self.other().loc.clone();
        let other_display = self.other().loc.display();
        let form = Form::new(verb)
            .width(64)
            .input_hist("to", &format!("{verb} {what} to:"), &default_target, hist_key, hist)
            .separator()
            .check("preserve", "Preserve attributes", true)
            .check("follow", "Follow links", false)
            .check("dive", "Dive into subdirectory if exists", true)
            .radio(
                "ow",
                "If target exists:",
                &["Ask", "Overwrite all", "Skip all", "Overwrite older", "Size differs"],
                0,
            )
            .buttons(&["OK", "Background", "Cancel"], 0)
            .on_submit(move |app, r| {
                if r.button == 2 {
                    return;
                }
                let foreground = r.button == 0;
                let opts = CopyOpts {
                    overwrite: match r.radio("ow") {
                        1 => Overwrite::Always,
                        2 => Overwrite::Never,
                        3 => Overwrite::Older,
                        4 => Overwrite::SizeDiffers,
                        _ => Overwrite::Ask,
                    },
                    preserve: r.check("preserve"),
                    follow_links: r.check("follow"),
                    dive_into: r.check("dive"),
                };
                let to = r.text("to");
                match resolve_target(&to, &other_display, &dst_panel_loc, &src) {
                    Ok((dst, target)) => {
                        app.start_job(
                            JobSpec::Copy {
                                src: src.clone(),
                                names: names.clone(),
                                dst,
                                target,
                                opts,
                                remove_src: is_move,
                            },
                            foreground,
                        );
                        app.panel_mut().marked.clear();
                    }
                    Err(e) => app.error(format!("{e:#}")),
                }
            });
        self.push_dialog(Box::new(form));
    }

    fn rename_dialog(&mut self) {
        let Some(e) = self.panel().current().cloned() else { return };
        if e.is_parent() {
            return;
        }
        let stem_end = match e.name.rfind('.') {
            Some(i) if i > 0 && !e.is_dir() => e.name[..i].chars().count(),
            _ => e.name.chars().count(),
        };
        let loc = self.panel().loc.clone();
        let old = e.name.clone();
        let d = Form::new("Rename")
            .input_at("n", "New name:", &e.name, stem_end)
            .buttons(&["OK", "Cancel"], 0)
            .on_submit(move |app, r| {
                let new = r.text("n");
                if r.button != 0 || new.is_empty() || new == old {
                    return;
                }
                let to = loc.path.join(&new);
                if loc.vfs.lock().stat(&to).is_ok() && !new.eq_ignore_ascii_case(&old) {
                    app.error(format!("\"{new}\" already exists"));
                    return;
                }
                let res = loc.vfs.lock().rename(&loc.path.join(&old), &to);
                match res {
                    Ok(()) => {
                        app.undo.push(
                            format!("Rename \"{old}\" to \"{new}\""),
                            crate::undo::UndoOp::Move {
                                src: loc.vfs.clone(),
                                from: loc.path.join(&old),
                                dst: loc.vfs.clone(),
                                to: to.clone(),
                            },
                        );
                        app.panel_mut().reload();
                        app.panel_mut().focus(&new);
                    }
                    Err(e) => app.error(format!("{e:#}")),
                }
            });
        self.push_dialog(Box::new(d));
    }

    fn mkdir_dialog(&mut self) {
        let default = self.panel().current().filter(|e| !e.is_parent()).map(|e| e.name.clone()).unwrap_or_default();
        let loc = self.panel().loc.clone();
        let d = dialog::input(
            self,
            "Create a new Directory",
            "Enter directory name:",
            &default,
            "mkdir",
            move |app, name| {
                let name = name.trim().to_string();
                if name.is_empty() {
                    return;
                }
                let target = vfs::normalize(&loc.path.join(util::expand_tilde(&name)));
                // Create intermediate directories too.
                let mut to_make = Vec::new();
                let mut p = target.clone();
                {
                    let mut v = loc.vfs.lock();
                    while v.stat(&p).is_err() {
                        to_make.push(p.clone());
                        if !p.pop() {
                            break;
                        }
                    }
                    for d in to_make.iter().rev() {
                        if let Err(e) = v.mkdir(d, 0o755) {
                            drop(v);
                            app.error(format!("{e:#}"));
                            return;
                        }
                    }
                }
                if !to_make.is_empty() {
                    let paths: Vec<PathBuf> = to_make.iter().rev().cloned().collect();
                    app.undo.push(
                        format!("Make directory \"{name}\""),
                        crate::undo::UndoOp::Mkdir { vfs: loc.vfs.clone(), paths },
                    );
                }
                app.panel_mut().reload();
                let first = name.split('/').next().unwrap_or(&name).to_string();
                app.panel_mut().focus(&first);
            },
        );
        self.push_dialog(d);
    }

    fn delete_dialog(&mut self) {
        let sel = self.panel().selection();
        if sel.is_empty() {
            return;
        }
        let loc = self.panel().loc.clone();
        let names: Vec<String> = sel.iter().map(|e| e.name.clone()).collect();
        let what = if names.len() == 1 {
            format!("Delete \"{}\"?", util::trunc_mid(&names[0], 50))
        } else {
            format!("Delete {} files/directories?", names.len())
        };
        let can_trash = self.cfg.use_trash && loc.is_local();
        let buttons: &[&str] = if can_trash { &["Trash", "Delete", "Cancel"] } else { &["Delete", "Cancel"] };
        if !self.cfg.confirm_delete {
            self.start_job(JobSpec::Delete { loc, names, trash: can_trash }, true);
            return;
        }
        let d = Form::new("Delete").text(&what).buttons(buttons, 0).error().on_submit(move |app, r| {
            let (go, trash) = if can_trash { (r.button < 2, r.button == 0) } else { (r.button == 0, false) };
            if !go {
                return;
            }
            let many = names.len() > 1 && !trash;
            let run = move |app: &mut App| {
                app.start_job(JobSpec::Delete { loc, names, trash }, true);
                app.panel_mut().marked.clear();
            };
            if many {
                app.push_dialog(dialog::confirm("Delete", "Permanently delete ALL selected files?", run));
            } else {
                run(app);
            }
        });
        self.push_dialog(Box::new(d));
    }

    // ------------------------------------------------------------ hotlist, sort, jobs

    fn hotlist_dialog(&mut self) {
        let items: Vec<ListItem> =
            self.state.hotlist.iter().map(|h| ListItem::new(&h.name).hint(util::tildify(&h.path))).collect();
        let list = self.state.hotlist.clone();
        let d = ListDialog::new("Directory hotlist", items, move |app, a| match a {
            ListAction::Select(i) => {
                let target = list[i].path.clone();
                app.run_cmdline(&format!("cd {}", shell_quote(&target)));
            }
            ListAction::Delete(i) => {
                app.state.hotlist.remove(i);
                app.state.save();
                app.exec(Command::Hotlist);
            }
            ListAction::Add => {
                app.add_hotlist();
            }
            ListAction::Edit(i) => {
                let h = app.state.hotlist[i].clone();
                let d = Form::new("Edit hotlist entry")
                    .input("name", "Name:", &h.name)
                    .input("path", "Path:", &h.path)
                    .buttons(&["OK", "Cancel"], 0)
                    .on_submit(move |app, r| {
                        if r.button == 0 {
                            app.state.hotlist[i] = HotlistEntry { name: r.text("name"), path: r.text("path") };
                            app.state.save();
                        }
                        app.exec(Command::Hotlist);
                    });
                app.push_dialog(Box::new(d));
            }
        })
        .deletable()
        .addable()
        .editable()
        .footer("Enter: go  Ins: add current  F4: edit  Del: remove");
        self.push_dialog(Box::new(d));
    }

    fn add_hotlist(&mut self) {
        let path = self.panel().loc.display();
        let name = util::tildify(&path);
        let d = Form::new("Add to hotlist")
            .input("name", "Name:", &name)
            .input("path", "Path:", &path)
            .buttons(&["OK", "Cancel"], 0)
            .on_submit(|app, r| {
                if r.button == 0 {
                    app.state.hotlist.push(HotlistEntry { name: r.text("name"), path: r.text("path") });
                    app.state.save();
                    app.info("Added to hotlist");
                }
            });
        self.push_dialog(Box::new(d));
    }

    fn sort_dialog(&mut self) {
        let p = self.panel();
        let cur = SortKey::ALL.iter().position(|k| *k == p.sort).unwrap_or(0);
        let labels: Vec<&str> = SortKey::ALL.iter().map(|k| k.label()).collect();
        let d = Form::new("Sort order")
            .radio("key", "", &labels, cur)
            .separator()
            .check("rev", "Reverse", p.reverse)
            .check("dirs", "Directories first", p.dirs_first)
            .check("case", "Case sensitive", p.case_sensitive)
            .buttons(&["OK", "Cancel"], 0)
            .on_submit(|app, r| {
                if r.button != 0 {
                    return;
                }
                let p = app.panel_mut();
                p.sort = SortKey::ALL[r.radio("key")];
                p.reverse = r.check("rev");
                p.dirs_first = r.check("dirs");
                p.case_sensitive = r.check("case");
                p.resort();
            });
        self.push_dialog(Box::new(d));
    }

    fn jobs_dialog(&mut self) {
        let jobs: Vec<_> = self.jobs.iter().filter(|j| !j.finished.load(Relaxed)).cloned().collect();
        if jobs.is_empty() {
            self.push_dialog(dialog::message("Background jobs", "No background jobs."));
            return;
        }
        let items: Vec<ListItem> = jobs
            .iter()
            .map(|j| {
                let state = if j.paused.load(Relaxed) { "paused" } else { "running" };
                ListItem::new(&j.title).hint(format!("{:>3.0}% {state}", j.fraction() * 100.0))
            })
            .collect();
        let d = ListDialog::new("Background jobs", items, move |app, a| match a {
            ListAction::Select(i) => {
                app.push_dialog(Box::new(crate::dialog::progress::ProgressDialog::new(jobs[i].clone())));
            }
            ListAction::Delete(i) => {
                jobs[i].cancel.store(true, Relaxed);
                app.info("Job cancelled");
            }
            _ => {}
        })
        .deletable()
        .footer("Enter: show progress  Del: abort job");
        self.push_dialog(Box::new(d));
    }

    fn panelize(&mut self, cmd: &str) {
        let Some(dir) = self.panel().loc.local_path().map(Path::to_path_buf) else {
            self.flash_error("Panelize works on local directories only");
            return;
        };
        let out = std::process::Command::new("sh").arg("-c").arg(cmd).current_dir(&dir).output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                let files: Vec<String> = text
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .map(|l| l.strip_prefix("./").unwrap_or(l).to_string())
                    .collect();
                if files.is_empty() {
                    self.flash_error("Command produced no file names");
                    return;
                }
                self.panelize_list(dir, files);
            }
            Err(e) => self.error(e),
        }
    }

    /// Show an arbitrary list of files (relative to `base`, or absolute) in the active panel.
    pub fn panelize_list(&mut self, base: PathBuf, files: Vec<String>) {
        let p = self.panel_mut();
        p.loc = Location::local(base);
        p.vfs_stack.clear();
        p.view = PanelView::List;
        p.panelized = Some(files);
        p.marked.clear();
        p.cursor = 0;
        if let Err(e) = p.load() {
            p.panelized = None;
            let _ = p.load();
            self.error(e);
        }
    }
}

/// Work out the destination location and optional target name / mask from typed text.
fn resolve_target(
    to: &str,
    other_display: &str,
    other: &Location,
    src: &Location,
) -> anyhow::Result<(Location, Option<String>)> {
    let to = to.trim();
    if to.is_empty() {
        anyhow::bail!("No destination given");
    }
    let trimmed = to.trim_end_matches('/');
    if trimmed == other_display.trim_end_matches('/') {
        return Ok((other.clone(), None));
    }
    let base = if to.starts_with('/') || to.starts_with('~') || to.contains("://") { other } else { src };
    let loc = vfs::parse_location(to, base)?;
    if to.ends_with('/') {
        return Ok((loc, None));
    }
    let is_dir = loc.vfs.lock().stat(&loc.path).map(|e| e.is_dir()).unwrap_or(false);
    if is_dir && !to.contains('*') {
        return Ok((loc, None));
    }
    let name = loc.path.file_name().map(|n| n.to_string_lossy().into_owned());
    let parent = loc.path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
    Ok((loc.with_path(parent), name))
}

/// Copy text to the system clipboard through the terminal (OSC 52).
pub fn osc52_copy(text: &str) {
    use std::io::Write;
    let enc = base64(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{enc}\x07");
    let _ = out.flush();
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}
