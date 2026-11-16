//! Options > Configuration dialog.

use crate::app::App;
use crate::dialog::Form;

pub fn open(app: &mut App) {
    let c = &app.cfg;
    let themes = crate::theme::BUILTIN;
    let theme_idx = themes.iter().position(|t| *t == c.theme).unwrap_or(0);
    let protos = ["auto", "kitty", "sixel", "iterm2", "halfblocks"];
    let proto_idx = protos.iter().position(|p| *p == c.image_protocol).unwrap_or(0);
    let d = Form::new("Configuration")
        .width(60)
        .radio("theme", "Theme:", themes, theme_idx)
        .separator()
        .check("nerd", "Nerd Font icons", c.nerd_fonts)
        .check("hidden", "Show hidden files", c.show_hidden)
        .check("menubar", "Show menu bar", c.show_menubar)
        .check("buttonbar", "Show function key bar", c.show_buttonbar)
        .check("mini", "Show mini status", c.show_mini_status)
        .check("mouse", "Mouse support", c.mouse)
        .check("lynx", "Lynx-like motion (Left/Right = up/enter)", c.lynx_motion)
        .check("dirs_first", "Directories first", c.dirs_first)
        .check("trash", "Delete to trash", c.use_trash)
        .check("cdel", "Confirm delete", c.confirm_delete)
        .check("cexit", "Confirm exit", c.confirm_exit)
        .check("iview", "Use internal viewer", c.use_internal_viewer)
        .check("iedit", "Use internal editor", c.use_internal_editor)
        .check("auto", "Auto-refresh directories", c.auto_refresh)
        .check("live", "Live reload of config, themes, menu", c.live_reload)
        .check("tfollow", "Terminal pane follows the panel directory", c.terminal_follow_panel)
        .check("git", "Git status in panels", c.git_status)
        .check("gitdiff", "Git diff in quick view", c.preview_git_diff)
        .check("images", "Image previews (restart to apply)", c.preview_images)
        .radio("proto", "Image protocol (restart to apply):", &protos, proto_idx)
        .buttons(&["Save", "Apply", "Cancel"], 0)
        .on_submit(move |app, r| {
            if r.button == 2 {
                return;
            }
            let c = &mut app.cfg;
            c.theme = themes[r.radio("theme")].to_string();
            c.nerd_fonts = r.check("nerd");
            c.show_hidden = r.check("hidden");
            c.show_menubar = r.check("menubar");
            c.show_buttonbar = r.check("buttonbar");
            c.show_mini_status = r.check("mini");
            c.mouse = r.check("mouse");
            c.lynx_motion = r.check("lynx");
            c.dirs_first = r.check("dirs_first");
            c.use_trash = r.check("trash");
            c.confirm_delete = r.check("cdel");
            c.confirm_exit = r.check("cexit");
            c.use_internal_viewer = r.check("iview");
            c.use_internal_editor = r.check("iedit");
            c.auto_refresh = r.check("auto");
            c.live_reload = r.check("live");
            c.terminal_follow_panel = r.check("tfollow");
            c.git_status = r.check("git");
            c.preview_git_diff = r.check("gitdiff");
            c.preview_images = r.check("images");
            c.image_protocol = protos[r.radio("proto")].to_string();
            app.theme = crate::theme::Theme::load(&app.cfg.theme);
            let hidden = app.cfg.show_hidden;
            let dirs_first = app.cfg.dirs_first;
            for p in &mut app.panels {
                p.show_hidden = hidden;
                p.dirs_first = dirs_first;
                p.reload();
            }
            app.previewer.invalidate();
            crate::reload::start(app);
            if r.button == 0 {
                match app.cfg.save() {
                    Ok(p) => app.info(format!("Saved {}", p.display())),
                    Err(e) => app.error(e),
                }
            }
        });
    app.push_dialog(Box::new(d));
}
