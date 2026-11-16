//! Rendering of a single panel: listing modes, info view, tree view, quick view.

use super::Ctx;
use crate::app::App;
use crate::icons;
use crate::panel::{ListMode, Panel, PanelView};
use crate::util::{self, grouped, human_size, pad_left, pad_right, short_time, str_width, trunc_left, trunc_mid};
use crate::vfs::Entry;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

pub fn draw_panel(f: &mut Frame, app: &mut App, i: usize, r: Rect, ctx: &Ctx) {
    if r.width < 4 || r.height < 3 {
        return;
    }
    let active = app.active == i && app.screens.is_empty();
    let view = app.panels[i].view;
    if view == PanelView::Terminal {
        if app.term.docked != Some(i) || app.term.session.is_none() {
            // Stale (e.g. restored from a saved tab): back to the file list.
            app.panels[i].view = PanelView::List;
        } else {
            app.term.rect = r;
            app.layout.lists[i] = r;
            crate::term::draw(f, app, r, ctx);
            if r.width > 12 {
                let cr = Rect { x: r.right() - 5, y: r.y, width: 3, height: 1 };
                f.render_widget(Paragraph::new(Span::styled("[x]", ctx.theme.error_border)), cr);
                super::add_hit(cr, super::Hit::PanelList(i));
            }
            return;
        }
    }
    let view = app.panels[i].view;
    let title = match view {
        PanelView::List => app.panels[i].title(),
        PanelView::Info => "Info".to_string(),
        PanelView::Tree => "Tree".to_string(),
        PanelView::QuickView => "Quick view".to_string(),
        PanelView::Terminal => "Terminal".to_string(),
    };
    let border_style = if active { ctx.theme.border_active } else { ctx.theme.border };
    let title_style = if active { ctx.theme.cursor } else { ctx.theme.border };
    // Tab bar (only with more than one tab): " 1 name  2 name " in the top border.
    let tab_labels = if app.tabs[i].count() > 1 { app.tabs[i].labels(&app.panels[i]) } else { Vec::new() };
    let max_bar = (r.width as usize) / 2;
    let full: usize = tab_labels.iter().map(|(k, n)| format!(" {} {n} ", k + 1).len()).sum();
    let compact = full > max_bar;
    let tab_texts: Vec<(usize, String)> = tab_labels
        .iter()
        .map(|(k, n)| (*k, if compact { format!(" {} ", k + 1) } else { format!(" {} {n} ", k + 1) }))
        .collect();
    let bar_w: usize = tab_texts.iter().map(|(_, t)| str_width(t)).sum();
    let tw = (r.width as usize).saturating_sub(14 + bar_w);
    let title_text = format!(" {} ", trunc_left(&title, tw));
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(if ctx.classic { BorderType::Double } else { BorderType::Rounded })
        .border_style(border_style)
        .style(ctx.theme.panel)
        .title(Line::styled(title_text, title_style).centered());
    if view == PanelView::List {
        let p = &app.panels[i];
        let ind = format!(
            "{}{}{}",
            p.sort.short(),
            if p.reverse { "\u{2191}" } else { "" },
            if p.show_hidden { " ." } else { "" }
        );
        block = block.title(Line::styled(format!("[{ind}]"), ctx.theme.border).right_aligned());
        // Bottom: marked summary or disk free.
        let (n, size) = p.marked_stats();
        if n > 0 {
            block = block.title_bottom(
                Line::styled(format!(" {} in {} file(s) ", grouped(size), n), ctx.theme.marked).centered(),
            );
        }
        if let Some(df) = p.loc.local_path().and_then(disk_free) {
            block = block.title_bottom(Line::styled(df, ctx.theme.border).right_aligned());
        }
        if ctx.cfg.git_status
            && let Some(repo) = p.loc.local_path().and_then(|d| app.git.repo_for(d))
        {
            block = block.title_bottom(Line::styled(repo.summary(), ctx.theme.git_branch).left_aligned());
        }
        if let Some(err) = &p.error {
            block = block
                .title_bottom(Line::styled(format!(" {} ", trunc_mid(err, tw / 2)), ctx.theme.error).left_aligned());
        }
    }
    let inner = block.inner(r);
    f.render_widget(block, r);
    if !tab_texts.is_empty() {
        let mut x = r.x + 1;
        let pos = app.tabs[i].pos;
        for (k, t) in &tab_texts {
            let w = str_width(t) as u16;
            if x + w >= r.right().saturating_sub(1) {
                break;
            }
            let st = if *k == pos { title_style } else { ctx.theme.border };
            let tr = Rect { x, y: r.y, width: w, height: 1 };
            f.render_widget(Paragraph::new(Span::styled(t.clone(), st)), tr);
            super::add_hit(tr, super::Hit::Tab(i, *k));
            x += w;
        }
    }
    if view != PanelView::List && r.width > 12 {
        let cr = Rect { x: r.right() - 5, y: r.y, width: 3, height: 1 };
        f.render_widget(Paragraph::new(Span::styled("[x]", ctx.theme.error_border)), cr);
        super::add_hit(cr, super::Hit::PanelList(i));
    }

    match view {
        PanelView::List => draw_list(f, app, i, inner, ctx, active),
        PanelView::Info => draw_info(f, app, i, inner, ctx),
        PanelView::Tree => draw_tree(f, app, i, inner, ctx, active),
        PanelView::QuickView => {
            app.layout.lists[i] = inner;
            let src = &app.panels[1 - i];
            let entry = src.current().cloned();
            let loc = src.loc.clone();
            app.previewer.render(f, inner, &loc, entry.as_ref(), ctx);
        }
        PanelView::Terminal => {}
    }
}

fn disk_free(path: &std::path::Path) -> Option<String> {
    let st = nix::sys::statvfs::statvfs(path).ok()?;
    let bs = st.fragment_size() as u64;
    let total = st.blocks() as u64 * bs;
    let free = st.blocks_available() as u64 * bs;
    if total == 0 {
        return None;
    }
    Some(format!(" {}/{} ({}%) ", human_size(free), human_size(total), free * 100 / total))
}

fn name_cell(
    e: &Entry,
    marked: bool,
    ctx: &Ctx,
    width: usize,
    row_style: Style,
    plain_row: bool,
    git: Option<Option<crate::git::GitStatus>>,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut used = 0;
    if ctx.cfg.nerd_fonts {
        let (glyph, color) = icons::icon_for(e);
        let mut st = row_style;
        if plain_row && let Some(c) = color {
            st = st.fg(c);
        }
        spans.push(Span::styled(format!("{glyph} "), st));
        used += 2;
    }
    // Git status cell (reserved for every row while the directory is in a repository).
    if let Some(status) = git {
        match status {
            Some(s) => {
                let mut st = row_style;
                if let Some(fg) = s.style(ctx.theme).fg {
                    st = st.fg(fg);
                }
                st = st.add_modifier(s.style(ctx.theme).add_modifier);
                spans.push(Span::styled(s.marker(ctx.cfg.nerd_fonts).to_string(), st));
            }
            None => spans.push(Span::styled(" ", row_style)),
        }
        used += 1;
    }
    let marker = if marked {
        "*"
    } else if ctx.cfg.nerd_fonts {
        ""
    } else {
        " "
    };
    let suffix = if ctx.cfg.nerd_fonts { String::new() } else { icons::ascii_marker(e).to_string().trim().to_string() };
    // Encrypted archive entries get a lock after the name.
    let lock = match (e.encrypted, ctx.cfg.nerd_fonts) {
        (true, true) => " \u{f023}",
        (true, false) => " [enc]",
        _ => "",
    };
    let name = format!(
        "{marker}{}{}{lock}",
        e.name,
        if e.is_dir() && !e.is_parent() && !ctx.cfg.nerd_fonts { "/" } else { &suffix }
    );
    let avail = width.saturating_sub(used);
    spans.push(Span::styled(pad_right(&trunc_mid(&name, avail), avail), row_style));
    spans
}

fn size_text(p: &Panel, e: &Entry, w: usize) -> String {
    let s = if e.is_parent() {
        "UP--DIR".to_string()
    } else if e.is_dir() {
        match p.dir_sizes.get(&e.name) {
            Some(sz) => human_size(*sz),
            None => {
                if e.is_symlink() {
                    "SYMLINK".into()
                } else {
                    "DIR".into()
                }
            }
        }
    } else {
        human_size(e.size)
    };
    pad_left(&s, w)
}

fn draw_list(f: &mut Frame, app: &mut App, i: usize, inner: Rect, ctx: &Ctx, active: bool) {
    let mini = ctx.cfg.show_mini_status && inner.height > 6;
    let header_h = 1u16;
    let footer_h = if mini { 2 } else { 0 };
    let list = Rect { y: inner.y + header_h, height: inner.height.saturating_sub(header_h + footer_h), ..inner };
    app.layout.lists[i] = list;
    let p = &mut app.panels[i];
    let rows = list.height as usize;
    let mode = p.mode;
    let cols = if mode == ListMode::Brief { 2 } else { 1 };
    p.page_rows = rows.max(1);
    p.columns = cols;
    p.scroll_into_view(rows * cols);
    // Brief mode scrolls by whole columns.
    if cols > 1 && rows > 0 {
        p.offset = (p.offset / rows) * rows;
        if p.cursor >= p.offset + rows * cols {
            p.offset = (p.cursor / rows + 1 - cols) * rows;
        }
    }
    let width = inner.width as usize;
    let theme = ctx.theme;

    // Header.
    let header = match mode {
        ListMode::Full => {
            let name_w = width.saturating_sub(7 + 12 + 2);
            format!("{}│{}│{}", center("Name", name_w), center("Size", 7), center("Modify time", 12))
        }
        ListMode::Brief => {
            let cw = width / 2;
            format!("{}│{}", center("Name", cw.saturating_sub(1)), center("Name", width - cw))
        }
        ListMode::Long => {
            let name_w = width.saturating_sub(10 + 4 + 9 + 9 + 7 + 12 + 6);
            format!(
                "{} {} {} {} {} {} {}",
                center("Perm", 10),
                pad_left("Nl", 3),
                pad_right("Owner", 8),
                pad_right("Group", 8),
                center("Size", 7),
                center("Modified", 12),
                center("Name", name_w)
            )
        }
    };
    f.render_widget(Paragraph::new(Span::styled(pad_right(&header, width), theme.header)), Rect { height: 1, ..inner });

    let mut lines: Vec<Line> = Vec::with_capacity(rows);
    let git_repo = if ctx.cfg.git_status { p.loc.local_path().and_then(|d| app.git.repo_for(d)) } else { None };
    let git_dir = p.loc.path.clone();
    let git_of = |e: &Entry| git_repo.map(|r| r.status_of(&git_dir, &e.name, e.is_dir()));
    for row in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        for col in 0..cols {
            let idx = p.offset + col * rows + row;
            let cw = if cols == 1 {
                width
            } else if col == 0 {
                width / 2 - 1
            } else {
                width - width / 2
            };
            if col > 0 {
                spans.push(Span::styled("│", theme.border));
            }
            let Some(e) = p.entries.get(idx) else {
                spans.push(Span::styled(" ".repeat(cw), theme.panel));
                continue;
            };
            let marked = p.marked.contains(&e.name);
            let is_cur = idx == p.cursor;
            let base = icons::style_for(e, theme);
            let row_style = match (is_cur && active, is_cur, marked) {
                (true, _, true) => theme.marked_cursor,
                (true, _, false) => theme.cursor,
                (false, true, true) => {
                    theme.marked.patch(Style::default().bg(theme.cursor_inactive.bg.unwrap_or_default()))
                }
                (false, true, false) => theme.cursor_inactive,
                (false, false, true) => theme.marked,
                _ => base,
            };
            let plain = !(is_cur && active) && !marked;
            match mode {
                ListMode::Full => {
                    let name_w = cw.saturating_sub(7 + 12 + 2);
                    spans.extend(name_cell(e, marked, ctx, name_w, row_style, plain, git_of(e)));
                    let sep = if is_cur && active { row_style } else { theme.border };
                    spans.push(Span::styled("│", sep));
                    spans.push(Span::styled(size_text(p, e, 7), row_style));
                    spans.push(Span::styled("│", sep));
                    spans.push(Span::styled(pad_right(&short_time(e.mtime), 12), row_style));
                }
                ListMode::Brief => spans.extend(name_cell(e, marked, ctx, cw, row_style, plain, git_of(e))),
                ListMode::Long => {
                    let name_w = cw.saturating_sub(10 + 4 + 9 + 9 + 7 + 12 + 6);
                    let pre = format!(
                        "{} {} {} {} {} {} ",
                        if e.is_parent() { "drwxr-xr-x".to_string() } else { e.perm_string() },
                        pad_left(&e.nlink.to_string(), 3),
                        pad_right(&trunc_mid(&util::user_name(e.uid), 8), 8),
                        pad_right(&trunc_mid(&util::group_name(e.gid), 8), 8),
                        size_text(p, e, 7),
                        pad_right(&short_time(e.mtime), 12),
                    );
                    spans.push(Span::styled(pre, row_style));
                    spans.extend(name_cell(e, marked, ctx, name_w, row_style, plain, git_of(e)));
                }
            }
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines).style(theme.panel), list);
    if let Some(l) = &p.loading
        && list.height > 2
    {
        // Spinner while a slow directory (network, SFTP, huge folder) is read.
        const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
        let ms = l.started.elapsed().as_millis();
        let text = format!("{} Loading... {:.1}s  (Esc cancels)", FRAMES[(ms / 120) as usize % 4], ms as f64 / 1000.0);
        let row = Rect { y: list.y + list.height / 2, height: 1, ..list };
        f.render_widget(Paragraph::new(Line::styled(text, theme.header).centered()).style(theme.panel), row);
    }

    if mini {
        let sep_y = list.bottom();
        let line = "─".repeat(width);
        f.render_widget(Paragraph::new(Span::styled(line, theme.border)), Rect { y: sep_y, height: 1, ..inner });
        let status = match p.current() {
            Some(e) if e.is_parent() => "UP--DIR".to_string(),
            Some(e) => {
                let right = if let Some(t) = e.link_target() {
                    format!("-> {t}")
                } else if e.is_dir() {
                    p.dir_sizes.get(&e.name).map(|s| format!("{} bytes", grouped(*s))).unwrap_or_else(|| "DIR".into())
                } else {
                    format!("{} {}", grouped(e.size), e.perm_string())
                };
                let rw = str_width(&right);
                let left = trunc_mid(&e.name, width.saturating_sub(rw + 1));
                format!("{}{right}", pad_right(&left, width.saturating_sub(rw)))
            }
            None => String::new(),
        };
        f.render_widget(
            Paragraph::new(Span::styled(pad_right(&status, width), theme.panel)),
            Rect { y: sep_y + 1, height: 1, ..inner },
        );
    }
}

fn center(s: &str, w: usize) -> String {
    let sw = str_width(s);
    if sw >= w {
        return s.chars().take(w).collect();
    }
    let l = (w - sw) / 2;
    format!("{}{s}{}", " ".repeat(l), " ".repeat(w - sw - l))
}

fn draw_info(f: &mut Frame, app: &mut App, i: usize, inner: Rect, ctx: &Ctx) {
    app.layout.lists[i] = inner;
    let src = &app.panels[1 - i];
    let theme = ctx.theme;
    let mut lines = vec![];
    let label = |k: &str| Span::styled(format!("{k:>12}: "), theme.header);
    let val = |v: String| Span::styled(v, theme.panel);
    if let Some(e) = src.current() {
        let path = src.current_path().map(|p| src.loc.vfs.lock().display(&p)).unwrap_or_else(|| src.loc.display());
        let (glyph, color) = icons::icon_for(e);
        let mut st = theme.dir;
        if let Some(c) = color {
            st = st.fg(c);
        }
        lines.push(Line::from(vec![Span::styled(format!(" {glyph}  "), st), Span::styled(e.name.clone(), theme.dir)]));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![label("Location"), val(trunc_left(&path, inner.width as usize - 15))]));
        lines.push(Line::from(vec![label("Type"), val(format!("{:?}", icons::category(e)))]));
        if let Some(t) = e.link_target() {
            lines.push(Line::from(vec![label("Link to"), val(t.to_string())]));
        }
        lines.push(Line::from(vec![label("Size"), val(format!("{} ({})", grouped(e.size), human_size(e.size)))]));
        lines.push(Line::from(vec![label("Mode"), val(format!("{} ({:04o})", e.perm_string(), e.mode & 0o7777))]));
        lines.push(Line::from(vec![
            label("Owner"),
            val(format!("{}/{}", util::user_name(e.uid), util::group_name(e.gid))),
        ]));
        lines.push(Line::from(vec![label("Links"), val(e.nlink.to_string())]));
        lines.push(Line::from(vec![label("Inode"), val(format!("{}  dev {:#x}", e.inode, e.dev))]));
        lines.push(Line::from(vec![label("Modified"), val(util::full_time(e.mtime))]));
        lines.push(Line::from(vec![label("Accessed"), val(util::full_time(e.atime))]));
        lines.push(Line::from(vec![label("Changed"), val(util::full_time(e.ctime))]));
    }
    lines.push(Line::raw(""));
    if let Some(dir) = src.loc.local_path()
        && let Ok(st) = nix::sys::statvfs::statvfs(dir)
    {
        let bs = st.fragment_size();
        let total = st.blocks() as u64 * bs;
        let free = st.blocks_available() as u64 * bs;
        lines.push(Line::from(vec![
            label("Filesystem"),
            val(format!("{} free of {}", human_size(free), human_size(total))),
        ]));
        lines.push(Line::from(vec![label("Inodes"), val(format!("{} free of {}", st.files_available(), st.files()))]));
        let used = if total > 0 { (total - free) as f64 / total as f64 } else { 0.0 };
        let bw = (inner.width as usize).saturating_sub(16);
        let filled = (used * bw as f64) as usize;
        lines.push(Line::from(vec![
            label("Usage"),
            Span::styled("\u{2588}".repeat(filled), theme.gauge),
            Span::styled("\u{2591}".repeat(bw - filled), theme.border),
        ]));
    }
    let (n, size) = src.marked_stats();
    lines.push(Line::from(vec![
        label("Directory"),
        val(format!("{} entries, {} marked ({})", src.entries.len().saturating_sub(1), n, human_size(size))),
    ]));
    f.render_widget(Paragraph::new(lines).style(theme.panel), inner);
}

fn draw_tree(f: &mut Frame, app: &mut App, i: usize, inner: Rect, ctx: &Ctx, active: bool) {
    app.layout.lists[i] = inner;
    let p = &mut app.panels[i];
    if p.tree.is_none() {
        p.tree = Some(crate::tree::TreeState::new(&p.loc.path.clone(), p.show_hidden));
    }
    let t = p.tree.as_mut().unwrap();
    let rows = inner.height as usize;
    if t.cursor < t.offset {
        t.offset = t.cursor;
    } else if rows > 0 && t.cursor >= t.offset + rows {
        t.offset = t.cursor + 1 - rows;
    }
    let width = inner.width as usize;
    let mut lines = Vec::new();
    for idx in t.offset..(t.offset + rows).min(t.nodes.len()) {
        let n = &t.nodes[idx];
        let prefix = t.prefix(idx);
        let icon = if ctx.cfg.nerd_fonts { if n.expanded { "\u{f07c} " } else { "\u{f07b} " } } else { "" };
        let text = format!("{prefix}{icon}{}", n.name);
        let st = if idx == t.cursor {
            if active { ctx.theme.cursor } else { ctx.theme.cursor_inactive }
        } else {
            ctx.theme.dir
        };
        lines.push(Line::styled(pad_right(&trunc_mid(&text, width), width), st));
    }
    f.render_widget(Paragraph::new(lines).style(ctx.theme.panel), inner);
}
