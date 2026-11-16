//! End-to-end tests of the extended previews through the real preview worker.

use super::*;
use crate::app::AppEvent;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};
use ratatui_image::FontSize;

struct Harness {
    pv: Previewer,
    rx: crossbeam_channel::Receiver<AppEvent>,
    term: Terminal<TestBackend>,
    theme: Theme,
    cfg: crate::config::Config,
    picker: Picker,
    dir: PathBuf,
}

impl Harness {
    #[allow(deprecated)]
    fn new(tag: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("pairdeck-ext-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let picker = Picker::from_fontsize(FontSize::new(8, 16));
        let (tx, rx) = crossbeam_channel::unbounded::<AppEvent>();
        Harness {
            pv: Previewer::new(tx, Some(picker.clone())),
            rx,
            term: Terminal::new(TestBackend::new(70, 30)).unwrap(),
            theme: Theme::by_name("dark"),
            cfg: crate::config::Config::default(),
            picker,
            dir,
        }
    }

    /// Preview `name` (written with `data`) and wait for the worker result.
    fn show(&mut self, name: &str, data: &[u8]) -> String {
        let path = self.dir.join(name);
        std::fs::write(&path, data).unwrap();
        let loc = Location::local(&self.dir);
        let entry = crate::vfs::local::stat_local(&path).unwrap();
        let ctx = crate::ui::Ctx { theme: &self.theme, cfg: &self.cfg, classic: false, picker: Some(&self.picker) };
        let area = Rect::new(0, 0, 70, 30);
        self.term.draw(|f| self.pv.render(f, area, &loc, Some(&entry), &ctx)).unwrap();
        if let AppEvent::Preview(m) = self.rx.recv_timeout(Duration::from_secs(10)).expect("preview result") {
            self.pv.on_msg(m);
        }
        self.redraw(name)
    }

    fn redraw(&mut self, name: &str) -> String {
        let path = self.dir.join(name);
        let loc = Location::local(&self.dir);
        let entry = crate::vfs::local::stat_local(&path).unwrap();
        let ctx = crate::ui::Ctx { theme: &self.theme, cfg: &self.cfg, classic: false, picker: Some(&self.picker) };
        let area = Rect::new(0, 0, 70, 30);
        let frame = self.term.draw(|f| self.pv.render(f, area, &loc, Some(&entry), &ctx)).unwrap();
        let buf = frame.buffer;
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

#[test]
fn csv_renders_as_table() {
    let mut h = Harness::new("csv");
    let screen = h.show("data.csv", b"city,pop\nParis,2100000\nRome,2800000\n");
    assert!(screen.contains("2 rows x 2 columns"), "{screen}");
    assert!(screen.contains("Paris"));
    assert!(screen.contains("\u{2502}"), "column separator");
}

#[test]
fn json_pretty_then_tree_toggle() {
    let mut h = Harness::new("json");
    let screen = h.show("a.json", br#"{"name":"pairdeck","deps":{"a":1,"b":[1,2,3]}}"#);
    assert!(screen.contains("JSON (t: tree view)"), "{screen}");
    assert!(screen.contains("\"name\": \"pairdeck\""), "pretty printed: {screen}");
    assert!(h.pv.key(&key(KeyCode::Char('t'))));
    let screen = h.redraw("a.json");
    assert!(screen.contains("tree 1/"), "{screen}");
    // Collapse the root: only one row left.
    assert!(h.pv.key(&key(KeyCode::Left)));
    let screen = h.redraw("a.json");
    assert!(screen.contains("2 keys"), "{screen}");
}

#[test]
fn invalid_toml_shows_error_banner() {
    let mut h = Harness::new("toml");
    let screen = h.show("bad.toml", b"a = = 1\n");
    assert!(screen.contains("Invalid TOML"), "{screen}");
}

#[test]
fn plain_log_text_is_highlighted() {
    let mut h = Harness::new("log");
    h.show("app.log", b"2026-11-13 10:00:00 ERROR boom at /var/log/x\n");
    let Some(Body::Lines(lines)) = &h.pv.body else { panic!("lines body") };
    let styles: std::collections::HashSet<_> = lines[0].spans.iter().map(|s| s.style).collect();
    assert!(styles.len() >= 3, "several styles on a log line: {styles:?}");
}

#[test]
fn animated_gif_plays() {
    let mut h = Harness::new("gif");
    let mut data = Vec::new();
    {
        use image::codecs::gif::{GifEncoder, Repeat};
        let mut enc = GifEncoder::new(&mut data);
        enc.set_repeat(Repeat::Infinite).unwrap();
        for i in 0..4u8 {
            let img = image::RgbaImage::from_pixel(16, 12, image::Rgba([i * 60, 20, 200, 255]));
            enc.encode_frame(image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(30, 1))).unwrap();
        }
    }
    let screen = h.show("spin.gif", &data);
    assert!(screen.contains("4 frames"), "{screen}");
    assert!(h.pv.animating());
    std::thread::sleep(Duration::from_millis(120));
    h.pv.tick();
    let Some(Body::Anim { current, .. }) = &h.pv.body else { panic!("anim body") };
    assert!(*current > 0, "frame advanced");
    assert!(h.pv.key(&key(KeyCode::Char(' '))), "space pauses");
    assert!(!h.pv.animating());
}

#[test]
fn executable_summary() {
    let mut h = Harness::new("exe");
    let data = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    // Keep the test fast: the head is enough for the header rows.
    let screen = h.show("tool", &data[..data.len().min(1 << 20)]);
    assert!(screen.contains("Format"), "{screen}");
    assert!(screen.contains("Architecture"), "{screen}");
}

#[test]
fn wide_text_scrolls_horizontally() {
    let mut h = Harness::new("wide");
    let line = format!("{}END", "x".repeat(200));
    let screen = h.show("wide.csv", format!("a,b\n{line},1\n").as_bytes());
    assert!(!screen.contains("END"));
    for _ in 0..30 {
        h.pv.key(&key(KeyCode::Right));
    }
    let _ = h.redraw("wide.csv");
    assert!(h.pv.hscroll > 0);
}
