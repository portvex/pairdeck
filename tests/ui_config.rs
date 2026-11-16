//! Live config reload and the key binding editor. These tests write files in
//! the (temporary, per-process) config directory, so they run one at a time.

mod common;

use common::{Ui, test_config, workdir, write};
use crossterm::event::{KeyCode, KeyModifiers};
use pairdeck::keymap::{Command, parse_chord};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

fn config_path() -> std::path::PathBuf {
    common::init();
    pairdeck::config::Config::path().unwrap()
}

#[test]
fn live_reload_applies_config_and_rejects_bad_files() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = workdir("reload");
    write(&dir.join(".hidden"), "x");
    write(&dir.join("visible.txt"), "x");
    let _ = std::fs::remove_file(config_path());
    let mut ui = Ui::with_config(&dir, &dir, test_config());
    assert!(ui.app.config_watch.is_some(), "config dir is watched");
    assert!(!ui.names().contains(&".hidden".to_string()));

    write(
        &config_path(),
        "theme = \"nord\"\nshow_hidden = true\nuse_trash = false\n\n[keys]\n\"F12\" = \"find_file\"\n",
    );
    ui.wait_for("config reloaded", |ui| ui.app.cfg.theme == "nord");
    assert_eq!(ui.app.theme.name, "nord");
    assert!(ui.names().contains(&".hidden".to_string()), "show_hidden applied to panels");
    assert_eq!(ui.app.keymap.bindings.get(&parse_chord("F12").unwrap()), Some(&Command::FindFile));
    ui.wait_text("Reloaded configuration");

    // A broken file keeps the previous configuration and says so.
    write(&config_path(), "theme = \"light\"\nthis is not toml\n");
    ui.wait_text("Keeping the previous configuration");
    assert_eq!(ui.app.cfg.theme, "nord");
    ui.key(KeyCode::Esc);

    // A user theme file for the active theme is picked up too.
    write(&config_path(), "theme = \"mine\"\n");
    ui.wait_for("theme switched", |ui| ui.app.cfg.theme == "mine");
    let theme_file = config_path().parent().unwrap().join("themes/mine.toml");
    write(&theme_file, "base = \"gruvbox\"\n");
    ui.wait_for("user theme reloaded", |ui| ui.app.theme.syntax == "gruvbox-dark");
    let _ = std::fs::remove_file(config_path());
    let _ = std::fs::remove_file(theme_file);
}

#[test]
fn key_binding_editor_binds_replaces_removes_and_resets() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = workdir("keyedit");
    let mut cfg = test_config();
    cfg.live_reload = false;
    write(&config_path(), "# user comment\ntheme = \"dark\"\n");
    let mut ui = Ui::with_config(&dir, &dir, cfg);
    ui.app.exec(Command::KeyBindings);
    ui.wait_text("Key bindings");
    ui.typ("find file");
    ui.wait_text("Find file  (find_file)");

    // Ctrl-T is taken by toggle_mark: the editor asks before replacing it.
    ui.key(KeyCode::Enter);
    ui.wait_text("Press the new key");
    ui.ctrl('t');
    ui.wait_text("Replace it? (y/n)");
    ui.typ("y");
    let ctrl_t = parse_chord("ctrl-t").unwrap();
    assert_eq!(ui.app.keymap.bindings.get(&ctrl_t), Some(&Command::FindFile));
    let text = std::fs::read_to_string(config_path()).unwrap();
    assert!(text.contains("# user comment"), "comments preserved: {text}");
    let saved: pairdeck::config::Config = toml::from_str(&text).unwrap();
    assert_eq!(saved.keys.get("ctrl-t").map(String::as_str), Some("find_file"), "{text}");

    // Chords: Ctrl-X is a prefix, so the editor waits for the second key.
    ui.key(KeyCode::Enter);
    ui.ctrl('x');
    ui.wait_text("C-x ...");
    ui.typ("z");
    assert_eq!(ui.app.keymap.bindings.get(&parse_chord("ctrl-x z").unwrap()), Some(&Command::FindFile));

    // Del removes every key of the command (defaults are written as "none").
    ui.key(KeyCode::Delete);
    assert!(!ui.app.keymap.bindings.values().any(|c| *c == Command::FindFile));
    let saved: pairdeck::config::Config = toml::from_str(&std::fs::read_to_string(config_path()).unwrap()).unwrap();
    assert_eq!(saved.keys.get("alt-?").map(String::as_str), Some("none"));

    // Ctrl-R resets everything.
    ui.key_mod(KeyCode::Char('r'), KeyModifiers::CONTROL);
    ui.wait_text("Reset ALL key bindings");
    ui.typ("y");
    assert_eq!(ui.app.keymap.bindings, pairdeck::keymap::default_bindings());
    let saved: pairdeck::config::Config = toml::from_str(&std::fs::read_to_string(config_path()).unwrap()).unwrap();
    assert!(saved.keys.is_empty(), "{:?}", saved.keys);
    ui.key(KeyCode::Esc);
    assert!(ui.app.dialogs.is_empty());
    let _ = std::fs::remove_file(config_path());
}
