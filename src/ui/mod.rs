//! Screen layout and rendering.

pub mod panel;

use crate::app::App;
use crate::config::Config;
use crate::theme::Theme;
use crate::util::{pad_right, str_width, tildify, trunc_left};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};
use ratatui_image::picker::Picker;
use std::cell::RefCell;

/// A clickable region registered during rendering.
#[derive(Clone, Copy, Debug)]
pub enum Hit {
    /// Send this key to whatever is on top (screen or panels).
    Key(crossterm::event::KeyCode),
    /// Run a panel command.
    Cmd(crate::keymap::Command),
    /// Switch panel `n` back to the file list.
    PanelList(usize),
    /// Show tab `k` of panel side `n`.
    Tab(usize, usize),
}

thread_local! {
    static HITS: RefCell<Vec<(Rect, Hit)>> = const { RefCell::new(Vec::new()) };
}

pub fn add_hit(r: Rect, h: Hit) {
    HITS.with(|v| v.borrow_mut().push((r, h)));
}

/// Topmost registered region containing the point.
pub fn hit_at(x: u16, y: u16) -> Option<Hit> {
    HITS.with(|v| v.borrow().iter().rev().find(|(r, _)| r.contains((x, y).into())).map(|(_, h)| *h))
}

/// Draw a clickable "[x]" at the top-right corner of `area` that sends F10.
pub fn close_button(f: &mut Frame, area: Rect, ctx: &Ctx) {
    if area.width < 8 {
        return;
    }
    let r = Rect { x: area.right() - 4, y: area.y, width: 3, height: 1 };
    f.render_widget(Paragraph::new(Span::styled("[x]", ctx.theme.error_border)), r);
    add_hit(r, Hit::Key(crossterm::event::KeyCode::F(10)));
}

/// Read-only render context shared by dialogs and screens.
pub struct Ctx<'a> {
    pub theme: &'a Theme,
    pub cfg: &'a Config,
    pub classic: bool,
    pub picker: Option<&'a Picker>,
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let theme = app.theme.clone();
    let cfg = app.cfg.clone();
    let picker = app.picker.clone();
    let ctx = Ctx { theme: &theme, cfg: &cfg, classic: theme.name == "classic", picker: picker.as_ref() };
    HITS.with(|v| v.borrow_mut().clear());

    if let Some(mut screen) = app.screens.pop() {
        screen.render(f, area, &ctx);
        close_button(f, area, &ctx);
        app.screens.push(screen);
    } else {
        draw_main(f, app, &ctx, area);
    }
    // Dialogs on top (all of them, bottom to top).
    let mut dialogs = std::mem::take(&mut app.dialogs);
    for d in dialogs.iter_mut() {
        d.render(f, area, &ctx);
    }
    // Dialogs pushed during render are kept.
    dialogs.append(&mut app.dialogs);
    app.dialogs = dialogs;
}

fn draw_main(f: &mut Frame, app: &mut App, ctx: &Ctx, area: Rect) {
    // A zero-size or tiny terminal (e.g. not attached to a real tty yet) has no
    // room for the layout; draw nothing rather than underflow.
    if area.width < 4 || area.height < 3 {
        return;
    }
    let mut y = area.y;
    let mut bottom = area.bottom();
    let menubar = if app.cfg.show_menubar {
        let r = Rect { y, height: 1, ..area };
        y += 1;
        r
    } else {
        Rect::default()
    };
    let buttonbar = if app.cfg.show_buttonbar && area.height > 4 && bottom > y {
        bottom -= 1;
        Rect { y: bottom, height: 1, ..area }
    } else {
        Rect::default()
    };
    let cmdline = if app.cfg.show_cmdline && !app.cmdline_hidden && bottom > y {
        bottom -= 1;
        Rect { y: bottom, height: 1, ..area }
    } else {
        Rect::default()
    };
    let full_area = Rect { x: area.x, y, width: area.width, height: bottom.saturating_sub(y) };
    // The embedded terminal pane takes the bottom part (or all of it when maximized).
    let (panels_area, term_area) = crate::term::split(app, full_area);
    let lw = panels_area.width / 2;
    let rects = [
        Rect { width: lw, ..panels_area },
        Rect { x: panels_area.x + lw, width: panels_area.width - lw, ..panels_area },
    ];
    app.layout.menubar = menubar;
    app.layout.panels = rects;
    app.layout.cmdline = cmdline;
    app.layout.buttonbar = buttonbar;

    if menubar.height > 0 {
        draw_menubar(f, app, ctx, menubar);
    }
    if panels_area.height > 0 {
        for (i, rect) in rects.into_iter().enumerate() {
            panel::draw_panel(f, app, i, rect, ctx);
        }
    } else {
        app.layout.panels = [Rect::default(); 2];
        app.layout.lists = [Rect::default(); 2];
    }
    if let Some(r) = term_area {
        crate::term::draw(f, app, r, ctx);
    }
    if cmdline.height > 0 {
        draw_cmdline(f, app, ctx, cmdline);
    } else if let Some((msg, _, err)) = &app.message {
        // No command line: show messages over the bottom of the panels.
        let r = Rect { y: panels_area.bottom().saturating_sub(1), height: 1, ..panels_area };
        let st = if *err { ctx.theme.error } else { ctx.theme.status };
        f.render_widget(Paragraph::new(Span::styled(pad_right(msg, r.width as usize), st)), r);
    }
    if buttonbar.height > 0 {
        draw_buttonbar(f, app, ctx, buttonbar);
    }
}

fn draw_menubar(f: &mut Frame, app: &mut App, ctx: &Ctx, r: Rect) {
    let mut spans = vec![Span::styled("  ", ctx.theme.menubar)];
    let mut x = 2u16;
    app.layout.menu_titles.clear();
    for (i, t) in crate::dialog::menu::TITLES.iter().enumerate() {
        let label = format!(" {t} ");
        let w = str_width(&label) as u16;
        app.layout.menu_titles.push((Rect { x: r.x + x, y: r.y, width: w, height: 1 }, i));
        spans.push(Span::styled(label, ctx.theme.menubar));
        spans.push(Span::styled("  ", ctx.theme.menubar));
        x += w + 2;
    }
    // Clickable quick view toggle.
    let on = app.panels.iter().any(|p| p.view == crate::panel::PanelView::QuickView);
    let icon = if ctx.cfg.nerd_fonts { "\u{f06e} " } else { "" };
    let label = format!(" {icon}Preview{} ", if on { " on" } else { "" });
    let lw = str_width(&label) as u16;
    add_hit(Rect { x: r.x + x, y: r.y, width: lw, height: 1 }, Hit::Cmd(crate::keymap::Command::QuickView));
    spans.push(Span::styled(label, if on { ctx.theme.menu_sel } else { ctx.theme.menubar_key }));
    let x = x + lw;
    // Clickable terminal pane toggle.
    let on = app.term.shown();
    let icon = if ctx.cfg.nerd_fonts { "\u{f120} " } else { "" };
    let label = format!(" {icon}Terminal{} ", if on { " on" } else { "" });
    let lw = str_width(&label) as u16;
    add_hit(Rect { x: r.x + x, y: r.y, width: lw, height: 1 }, Hit::Cmd(crate::keymap::Command::ToggleTerminal));
    spans.push(Span::styled(label, if on { ctx.theme.menu_sel } else { ctx.theme.menubar_key }));
    // Right side: running jobs indicator and clock.
    let running = app.jobs.len();
    let mut right = String::new();
    if running > 0 {
        let pct = app.jobs.iter().map(|j| j.fraction()).sum::<f64>() / running as f64 * 100.0;
        right.push_str(&format!(" {running} job(s) {pct:.0}% "));
    }
    right.push_str(&chrono::Local::now().format(" %H:%M ").to_string());
    let used: usize = spans.iter().map(|s| str_width(&s.content)).sum();
    let gap = (r.width as usize).saturating_sub(used + str_width(&right));
    spans.push(Span::styled(" ".repeat(gap), ctx.theme.menubar));
    spans.push(Span::styled(right, ctx.theme.menubar_key));
    f.render_widget(Paragraph::new(Line::from(spans)).style(ctx.theme.menubar), r);
}

fn draw_cmdline(f: &mut Frame, app: &mut App, ctx: &Ctx, r: Rect) {
    let p = app.panel();
    let dir = if p.is_local() { tildify(&p.loc.path.display().to_string()) } else { p.display_name().to_string() };
    let sym = if nix::unistd::getuid().is_root() { '#' } else { '$' };
    let prompt_full = format!("{}@{}:{}{sym} ", crate::util::current_user(), crate::util::hostname(), dir);
    let max_prompt = (r.width as usize / 2).max(10);
    let prompt = if str_width(&prompt_full) > max_prompt {
        format!("{} ", trunc_left(prompt_full.trim_end(), max_prompt - 1))
    } else {
        prompt_full
    };
    let pw = str_width(&prompt) as u16;
    f.render_widget(
        Paragraph::new(Span::styled(prompt.clone(), ctx.theme.prompt)),
        Rect { width: pw.min(r.width), ..r },
    );
    let input = Rect { x: r.x + pw, width: r.width.saturating_sub(pw), ..r };
    let has_dialog = !app.dialogs.is_empty();
    let quick = app.panel().quick_search.clone();
    if let Some(q) = quick {
        let text = format!("Quick search: {q}");
        f.render_widget(Paragraph::new(Span::styled(pad_right(&text, input.width as usize), ctx.theme.status)), input);
        return;
    }
    if app.cmdline.is_empty()
        && let Some((msg, _, err)) = &app.message
    {
        let st = if *err { ctx.theme.error } else { ctx.theme.cmdline.patch(ctx.theme.header) };
        let text = crate::util::trunc_end(msg, r.width as usize);
        f.render_widget(Paragraph::new(Span::styled(pad_right(&text, r.width as usize), st)), r);
        return;
    }
    // The focused terminal pane owns the cursor.
    let term_focused = app.term.shown() && app.term.focused;
    app.cmdline.render(f, input, ctx.theme.cmdline, !has_dialog && !term_focused);
}

const BUTTONS: [&str; 10] = ["Help", "Menu", "View", "Edit", "Copy", "RenMov", "Mkdir", "Delete", "PullDn", "Quit"];

fn draw_buttonbar(f: &mut Frame, app: &mut App, ctx: &Ctx, r: Rect) {
    let n = 10u16;
    let slot = (r.width / n).max(1);
    let mut spans = Vec::new();
    app.layout.buttons.clear();
    for i in 0..n {
        let num = format!("{}", i + 1);
        let w = if i == n - 1 { r.width.saturating_sub(slot * (n - 1)) } else { slot };
        let label_w = (w as usize).saturating_sub(num.len());
        let label = BUTTONS[i as usize];
        let label = pad_right(&label.chars().take(label_w).collect::<String>(), label_w);
        app.layout.buttons.push((Rect { x: r.x + i * slot, y: r.y, width: w, height: 1 }, (i + 1) as u8));
        spans.push(Span::styled(num, ctx.theme.button_key));
        spans.push(Span::styled(label, ctx.theme.button_label));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), r);
}
