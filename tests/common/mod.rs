//! Test harness that drives the real `App` without a terminal: renders into a
//! ratatui `TestBackend`, injects key and mouse events, and pumps the app's
//! event channel (jobs, previews, worker callbacks) until a condition holds.

#![allow(dead_code)]

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use pairdeck::app::{App, AppEvent};
use pairdeck::config::Config;
use ratatui::{Terminal, backend::TestBackend};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const W: u16 = 120;
pub const H: u16 = 34;
const TIMEOUT: Duration = Duration::from_secs(10);

static INIT: Once = Once::new();
static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Root of all temp data for this test process.
pub fn root() -> PathBuf {
    std::env::temp_dir().join(format!("pairdeck-uitest-{}", std::process::id()))
}

/// Point config and state at temp dirs before any App exists, so tests never
/// read or write the user's real ~/.config/pairdeck or ~/.local/share/pairdeck.
pub fn init() {
    INIT.call_once(|| {
        let r = root();
        let _ = std::fs::remove_dir_all(&r);
        std::fs::create_dir_all(r.join("config")).unwrap();
        std::fs::create_dir_all(r.join("data")).unwrap();
        pairdeck::config::override_dirs(r.join("config"), r.join("data"));
    });
}

/// A fresh, empty working directory for one test.
pub fn workdir(name: &str) -> PathBuf {
    init();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let d = root().join(format!("{name}-{n}"));
    std::fs::create_dir_all(&d).unwrap();
    // Canonical path: macOS /var is a symlink to /private/var.
    std::fs::canonicalize(&d).unwrap()
}

pub fn write(path: &Path, text: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

/// Deterministic test configuration.
pub fn test_config() -> Config {
    Config {
        use_trash: false,
        auto_refresh: false,
        confirm_exit: false,
        preview_images: false,
        nerd_fonts: false,
        theme: "dark".into(),
        ..Config::default()
    }
}

pub struct Ui {
    pub app: App,
    pub term: Terminal<TestBackend>,
}

impl Ui {
    pub fn new(left: &Path, right: &Path) -> Ui {
        Self::with_config(left, right, test_config())
    }

    pub fn with_config(left: &Path, right: &Path, cfg: Config) -> Ui {
        init();
        let app = App::new(cfg, [Some(left.to_path_buf()), Some(right.to_path_buf())], None);
        let term = Terminal::new(TestBackend::new(W, H)).unwrap();
        let mut ui = Ui { app, term };
        ui.settle();
        ui
    }

    pub fn draw(&mut self) {
        let app = &mut self.app;
        self.term.draw(|f| pairdeck::ui::draw(f, app)).unwrap();
    }

    /// Process queued events and ticks once; returns true if anything happened.
    fn pump_once(&mut self) -> bool {
        let mut busy = false;
        while let Ok(ev) = self.app.rx.try_recv() {
            self.app.handle_event(ev);
            busy = true;
        }
        self.app.tick();
        self.draw();
        busy
    }

    /// Pump until no events arrive for a short quiet period.
    pub fn settle(&mut self) {
        let start = Instant::now();
        let mut quiet_since = Instant::now();
        loop {
            if self.pump_once() {
                quiet_since = Instant::now();
            }
            if quiet_since.elapsed() > Duration::from_millis(60) || start.elapsed() > TIMEOUT {
                break;
            }
            std::thread::sleep(Duration::from_millis(3));
        }
    }

    /// Pump until `cond` holds; panics with the screen on timeout.
    pub fn wait_for(&mut self, what: &str, cond: impl Fn(&Ui) -> bool) {
        let start = Instant::now();
        loop {
            self.pump_once();
            if cond(self) {
                self.settle();
                return;
            }
            if start.elapsed() > TIMEOUT {
                panic!("timed out waiting for: {what}\n{}", self.screen());
            }
            std::thread::sleep(Duration::from_millis(3));
        }
    }

    pub fn wait_text(&mut self, text: &str) {
        let t = text.to_string();
        self.wait_for(&format!("screen text {text:?}"), move |ui| ui.screen().contains(&t));
    }

    pub fn send(&mut self, ev: Event) {
        self.app.handle_event(AppEvent::Term(ev));
        self.settle();
    }

    pub fn key_mod(&mut self, code: KeyCode, mods: KeyModifiers) {
        self.send(Event::Key(KeyEvent::new(code, mods)));
    }

    pub fn key(&mut self, code: KeyCode) {
        self.key_mod(code, KeyModifiers::NONE);
    }

    pub fn ctrl(&mut self, c: char) {
        self.key_mod(KeyCode::Char(c), KeyModifiers::CONTROL);
    }

    pub fn f(&mut self, n: u8) {
        self.key(KeyCode::F(n));
    }

    /// Type text one key at a time.
    pub fn typ(&mut self, text: &str) {
        for c in text.chars() {
            self.app.handle_event(AppEvent::Term(Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))));
        }
        self.settle();
    }

    pub fn click(&mut self, col: u16, row: u16) {
        for kind in [MouseEventKind::Down(MouseButton::Left), MouseEventKind::Up(MouseButton::Left)] {
            let m = MouseEvent { kind, column: col, row, modifiers: KeyModifiers::NONE };
            self.app.handle_event(AppEvent::Term(Event::Mouse(m)));
        }
        self.settle();
    }

    /// The rendered screen as text, one line per row.
    pub fn screen(&self) -> String {
        let buf = self.term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    pub fn current(&self) -> String {
        self.app.panel().current_name().unwrap_or_default()
    }

    /// Move the active panel cursor onto `name`.
    pub fn select(&mut self, name: &str) {
        assert!(self.app.panel_mut().focus(name), "no entry {name:?} in {:?}", self.names());
        self.settle();
    }

    pub fn names(&self) -> Vec<String> {
        self.app.panel().entries.iter().map(|e| e.name.clone()).collect()
    }

    pub fn active_dir(&self) -> PathBuf {
        self.app.panel().loc.path.clone()
    }
}
