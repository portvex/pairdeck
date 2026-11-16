//! README screenshots: drives the real app over this repository, renders each
//! screen to an HTML page, and `scripts/screenshots.sh` turns the pages into
//! PNGs with headless Chrome. Ignored by `cargo test`; run the script instead.
//!
//! The local user and host names are replaced by `demo` / `laptop`, so no real
//! account name ends up in a published image.

mod common;

use common::Ui;
use crossterm::event::KeyCode;
use pairdeck::app::App;
use pairdeck::config::Config;
use ratatui::buffer::{Buffer, Cell};
use ratatui::style::{Color, Modifier};
use ratatui::{Terminal, backend::TestBackend};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use unicode_width::UnicodeWidthStr;

const COLS: u16 = 132;
const ROWS: u16 = 38;
const CELL_W: f32 = 9.0; // JetBrains Mono at 15px
const CELL_H: f32 = 20.0;
const FAKE_USER: &str = "demo";
const FAKE_HOST: &str = "laptop";

fn repo() -> PathBuf {
    std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap()
}

fn ui(left: &Path, right: &Path, theme: &str) -> Ui {
    common::init();
    let cfg = Config {
        use_trash: false,
        auto_refresh: false,
        confirm_exit: false,
        nerd_fonts: true,
        theme: theme.into(),
        ..Config::default()
    };
    let picker = ratatui_image::picker::Picker::halfblocks();
    let app = App::new(cfg, [Some(left.to_path_buf()), Some(right.to_path_buf())], Some(picker));
    let term = Terminal::new(TestBackend::new(COLS, ROWS)).unwrap();
    let mut ui = Ui { app, term };
    ui.settle();
    ui
}

/// A colorful sample picture for the image preview (no personal photos).
fn sample_image(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let p = dir.join("aurora.png");
    let (w, h) = (480u32, 300u32);
    let img = image::RgbImage::from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
        let wave = ((fx * 9.0).sin() * 0.08 + 0.45 - fy).abs();
        let glow = (1.0 - (wave * 6.0).min(1.0)).powf(2.0);
        let sky = [10.0 + 40.0 * fy, 14.0 + 30.0 * fy, 40.0 + 60.0 * fy];
        let aurora = [40.0 + 120.0 * fx, 220.0 - 80.0 * fx, 150.0 + 90.0 * fx];
        let mut px = [0u8; 3];
        for i in 0..3 {
            px[i] = (sky[i] + aurora[i] * glow).min(255.0) as u8;
        }
        // Mountains.
        let ridge = 0.78 - 0.12 * (fx * 5.0 + 1.0).sin().abs() - 0.05 * (fx * 13.0).cos();
        if fy > ridge {
            px = [12, 16, 24];
        }
        image::Rgb(px)
    });
    img.save(&p).unwrap();
    p
}

// ---------------------------------------------------------------- HTML ----

fn palette(i: u8) -> (u8, u8, u8) {
    // The classic VGA palette, so the MC-style theme gets its dark blue panels.
    const BASE: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00),
        (0xaa, 0x00, 0x00),
        (0x00, 0xaa, 0x00),
        (0xaa, 0x55, 0x00),
        (0x00, 0x00, 0xaa),
        (0xaa, 0x00, 0xaa),
        (0x00, 0xaa, 0xaa),
        (0xaa, 0xaa, 0xaa),
        (0x55, 0x55, 0x55),
        (0xff, 0x55, 0x55),
        (0x55, 0xff, 0x55),
        (0xff, 0xff, 0x55),
        (0x55, 0x55, 0xff),
        (0xff, 0x55, 0xff),
        (0x55, 0xff, 0xff),
        (0xff, 0xff, 0xff),
    ];
    match i {
        0..=15 => BASE[i as usize],
        16..=231 => {
            let i = i - 16;
            let c = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (c(i / 36), c((i / 6) % 6), c(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

fn css(c: Color, default: &str) -> String {
    let named = |i: u8| {
        let (r, g, b) = palette(i);
        format!("#{r:02x}{g:02x}{b:02x}")
    };
    match c {
        Color::Reset => default.to_string(),
        Color::Black => named(0),
        Color::Red => named(1),
        Color::Green => named(2),
        Color::Yellow => named(3),
        Color::Blue => named(4),
        Color::Magenta => named(5),
        Color::Cyan => named(6),
        Color::Gray => named(7),
        Color::DarkGray => named(8),
        Color::LightRed => named(9),
        Color::LightGreen => named(10),
        Color::LightYellow => named(11),
        Color::LightBlue => named(12),
        Color::LightMagenta => named(13),
        Color::LightCyan => named(14),
        Color::White => named(15),
        Color::Indexed(i) => named(i),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Replace `from` with `to` in a row of cells, shifting the rest of the row.
fn replace_in_row(row: &mut Vec<Cell>, from: &str, to: &str) {
    let syms: Vec<String> = row.iter().map(|c| c.symbol().to_string()).collect();
    let n = from.chars().count();
    let mut x = 0;
    while x + n <= syms.len() {
        if syms[x..x + n].concat() == from {
            let style = row[x].clone();
            let new: Vec<Cell> = to
                .chars()
                .map(|ch| {
                    let mut c = style.clone();
                    c.set_char(ch);
                    c
                })
                .collect();
            // Only the run of cells styled like the match moves (an input field, the
            // prompt); everything after it, such as dialog borders, stays in place.
            let same = |c: &Cell| c.fg == style.fg && c.bg == style.bg && c.modifier == style.modifier;
            let end = (x + n..row.len()).find(|&i| !same(&row[i])).unwrap_or(row.len());
            let mut run: Vec<Cell> = new;
            run.extend_from_slice(&row[x + n..end]);
            while run.len() < end - x {
                let mut c = style.clone();
                c.set_char(' ');
                run.push(c);
            }
            run.truncate(end - x);
            row.splice(x..end, run);
            return replace_in_row(row, from, to);
        }
        x += 1;
    }
}

fn to_html(buf: &Buffer, title: &str) -> (String, u32, u32) {
    let (fg0, bg0) = ("#cdd6f4", "#1e1e2e");
    let user = pairdeck::util::current_user();
    let host = pairdeck::util::hostname();
    let mut bgs = String::new();
    let mut text = String::new();
    for y in 0..buf.area.height {
        let mut row: Vec<Cell> = (0..buf.area.width).map(|x| buf[(x, y)].clone()).collect();
        for (from, to) in [(&user, FAKE_USER), (&host, FAKE_HOST)] {
            if from.len() > 1 {
                replace_in_row(&mut row, from, to);
            }
        }
        let top = y as f32 * CELL_H;
        for (x, cell) in row.iter().enumerate() {
            let left = x as f32 * CELL_W;
            let (mut fg, mut bg) = (css(cell.fg, fg0), css(cell.bg, bg0));
            if cell.modifier.contains(Modifier::REVERSED) {
                std::mem::swap(&mut fg, &mut bg);
            }
            let sym = cell.symbol();
            let paint = match sym {
                "▀" => format!("linear-gradient({fg} 50%,{bg} 50%)"),
                "▄" => format!("linear-gradient({bg} 50%,{fg} 50%)"),
                "█" => fg.clone(),
                _ => bg.clone(),
            };
            if paint != bg0 {
                let _ = write!(bgs, "<i style=\"left:{left}px;top:{top}px;background:{paint}\"></i>",);
            }
            if matches!(sym, "▀" | "▄" | "█" | " " | "") {
                continue;
            }
            let mut st = format!("left:{left}px;top:{top}px;color:{fg}");
            if cell.modifier.contains(Modifier::BOLD) {
                st.push_str(";font-weight:700");
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                st.push_str(";font-style:italic");
            }
            if cell.modifier.contains(Modifier::DIM) {
                st.push_str(";opacity:.6");
            }
            if cell.modifier.contains(Modifier::UNDERLINED) {
                st.push_str(";text-decoration:underline");
            }
            let w = UnicodeWidthStr::width(sym).max(1) as f32 * CELL_W;
            let _ = write!(text, "<b style=\"{st};width:{w}px\">{}</b>", esc(sym));
        }
    }
    let (w, h) = ((COLS as f32 * CELL_W) as u32 + 32, (ROWS as f32 * CELL_H) as u32 + 32);
    let html = format!(
        r#"<!doctype html><meta charset="utf-8"><title>{title}</title>
<style>
html,body{{margin:0;background:{bg0}}}
#t{{position:relative;margin:16px;width:{}px;height:{}px;overflow:hidden;
  font:15px/{CELL_H}px "JetBrainsMono Nerd Font Mono","JetBrainsMono Nerd Font","JetBrains Mono",Menlo,monospace;
  font-variant-ligatures:none}}
#t i{{position:absolute;width:{CELL_W}px;height:{CELL_H}px}}
#t b{{position:absolute;height:{CELL_H}px;font-weight:400;white-space:pre;text-align:left;overflow:visible}}
</style><div id="t">{bgs}{text}</div>"#,
        COLS as f32 * CELL_W,
        ROWS as f32 * CELL_H
    );
    (html, w, h)
}

fn shot(ui: &mut Ui, out: &Path, name: &str) {
    ui.settle();
    let (html, w, h) = to_html(ui.term.backend().buffer(), name);
    std::fs::write(out.join(format!("{name}.html")), html).unwrap();
    println!("SHOT {name} {w} {h}");
}

// ---------------------------------------------------------------- shots ----

#[test]
#[ignore = "run scripts/screenshots.sh"]
fn readme_screenshots() {
    let out = PathBuf::from(std::env::var("PAIRDECK_SHOTS_DIR").expect("run scripts/screenshots.sh"));
    std::fs::create_dir_all(&out).unwrap();
    let repo = repo();
    let src = repo.join("src");

    // 1. Twin panels: the project and its sources.
    let mut u = ui(&repo, &src, "dark");
    u.select("Cargo.toml");
    shot(&mut u, &out, "panels");

    // 2. Quick view of a Rust file with syntax highlighting.
    let mut u = ui(&src, &repo, "dark");
    u.select("panel.rs");
    u.ctrl('q');
    u.wait_text("fn ");
    shot(&mut u, &out, "preview-code");

    // 3. Image preview.
    let pics = repo.join("target/pics");
    let img = sample_image(&pics);
    let mut u = ui(&pics, &repo, "dark");
    u.select(img.file_name().unwrap().to_str().unwrap());
    u.ctrl('q');
    u.wait_for("image drawn", |ui| ui.screen().contains('▀'));
    shot(&mut u, &out, "preview-image");

    // 4. Fuzzy finder with live preview.
    let mut u = ui(&repo, &src, "dark");
    u.ctrl('p');
    u.typ("preview ext");
    u.wait_text("preview_ext");
    std::thread::sleep(std::time::Duration::from_millis(300));
    shot(&mut u, &out, "fuzzy");

    // 5. The classic Midnight Commander look, with the copy dialog.
    let mut u = ui(&src, &repo, "classic");
    u.select("app.rs");
    u.key(KeyCode::Insert);
    u.key(KeyCode::Insert);
    u.f(5);
    shot(&mut u, &out, "classic");
}
