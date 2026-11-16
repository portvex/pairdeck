//! End-to-end UI flows: real App, real keys and mouse events, real file system.

mod common;

use common::{Ui, workdir, write};
use crossterm::event::{KeyCode, KeyModifiers};
use std::io::Write as _;

/// Two panels: `a` (left, active) with some files, `b` (right) empty.
fn setup(name: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let root = workdir(name);
    let a = root.join("a");
    let b = root.join("b");
    write(&a.join("one.txt"), "first file\n");
    write(&a.join("two.txt"), "second file\n");
    write(&a.join("sub/deep.md"), "# deep\n");
    std::fs::create_dir_all(&b).unwrap();
    (root, a, b)
}

#[test]
fn navigate_enter_and_go_up() {
    let (_, a, b) = setup("nav");
    let mut ui = Ui::new(&a, &b);
    assert!(ui.screen().contains("one.txt"));
    ui.select("sub");
    ui.key(KeyCode::Enter);
    assert_eq!(ui.active_dir(), a.join("sub"));
    ui.wait_text("deep.md");
    // Backspace goes up and puts the cursor back on the directory we came from.
    ui.key(KeyCode::Backspace);
    assert_eq!(ui.active_dir(), a);
    assert_eq!(ui.current(), "sub");
    // Tab switches to the other panel.
    ui.key(KeyCode::Tab);
    assert_eq!(ui.active_dir(), b);
}

#[test]
fn copy_with_conflict_choosing_rename() {
    let (_, a, b) = setup("copy");
    write(&b.join("one.txt"), "existing\n");
    let mut ui = Ui::new(&a, &b);
    ui.select("one.txt");
    ui.f(5);
    ui.wait_text("Copy \"one.txt\" to:");
    ui.key(KeyCode::Enter);
    ui.wait_text("File exists");
    // Hotkey R = Rename.
    ui.typ("r");
    let target = b.join("one (1).txt");
    ui.wait_for("renamed copy", |_| target.exists());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "first file\n");
    assert_eq!(std::fs::read_to_string(b.join("one.txt")).unwrap(), "existing\n");
}

#[test]
fn copy_directory_recursively() {
    let (_, a, b) = setup("copydir");
    let mut ui = Ui::new(&a, &b);
    ui.select("sub");
    ui.f(5);
    ui.key(KeyCode::Enter);
    let target = b.join("sub/deep.md");
    ui.wait_for("recursive copy", |_| target.exists());
    assert!(a.join("sub/deep.md").exists(), "copy keeps the source");
}

#[test]
fn move_file_to_other_panel() {
    let (_, a, b) = setup("move");
    let mut ui = Ui::new(&a, &b);
    ui.select("two.txt");
    ui.f(6);
    ui.wait_text("Move \"two.txt\" to:");
    ui.key(KeyCode::Enter);
    let target = b.join("two.txt");
    ui.wait_for("moved file", |_| target.exists());
    assert!(!a.join("two.txt").exists());
    ui.wait_for("left panel refreshed", |ui| !ui.app.panels[0].entries.iter().any(|e| e.name == "two.txt"));
}

#[test]
fn rename_in_place() {
    let (_, a, b) = setup("rename");
    let mut ui = Ui::new(&a, &b);
    ui.select("one.txt");
    ui.key_mod(KeyCode::F(6), KeyModifiers::SHIFT);
    ui.wait_text("New name:");
    ui.ctrl('a');
    ui.ctrl('k');
    ui.typ("renamed.txt");
    ui.key(KeyCode::Enter);
    assert!(a.join("renamed.txt").exists());
    assert!(!a.join("one.txt").exists());
    assert_eq!(ui.current(), "renamed.txt");
}

#[test]
fn nested_mkdir() {
    let (_, a, b) = setup("mkdir");
    let mut ui = Ui::new(&a, &b);
    ui.f(7);
    ui.wait_text("Create a new Directory");
    ui.typ("x/y/z");
    ui.key(KeyCode::Enter);
    assert!(a.join("x/y/z").is_dir());
    assert_eq!(ui.current(), "x");
}

#[test]
fn delete_permanently() {
    let (_, a, b) = setup("delete");
    let mut ui = Ui::new(&a, &b);
    ui.select("sub");
    ui.f(8);
    ui.wait_text("Delete \"sub\"?");
    // With use_trash = false the default button is Delete.
    ui.key(KeyCode::Enter);
    let gone = a.join("sub");
    ui.wait_for("directory deleted", |_| !gone.exists());
    ui.wait_for("panel refreshed", |ui| !ui.names().contains(&"sub".to_string()));
}

#[test]
fn mark_with_insert_and_glob() {
    let (_, a, b) = setup("mark");
    write(&a.join("pic.png"), "x");
    let mut ui = Ui::new(&a, &b);
    ui.select("one.txt");
    ui.key(KeyCode::Insert);
    assert!(ui.app.panel().marked.contains("one.txt"));
    assert_eq!(ui.current(), "pic.png", "Insert moves the cursor down");
    ui.key(KeyCode::Insert);
    assert_eq!(ui.app.panel().marked.len(), 2);
    // `*` inverts the selection of files (directories stay unmarked).
    ui.typ("*");
    assert!(ui.app.panel().marked.contains("two.txt") && !ui.app.panel().marked.contains("one.txt"));
    // Start over and select by glob.
    ui.app.panel_mut().marked.clear();
    ui.typ("+");
    ui.wait_text("Pattern (shell glob):");
    ui.typ("*.txt");
    ui.key(KeyCode::Enter);
    let marked = &ui.app.panel().marked;
    assert!(marked.contains("one.txt") && marked.contains("two.txt") && !marked.contains("pic.png"), "{marked:?}");
    ui.wait_text("in 2 file(s)");
}

#[test]
fn quick_search_jumps() {
    let (_, a, b) = setup("qsearch");
    let mut ui = Ui::new(&a, &b);
    ui.ctrl('s');
    ui.typ("tw");
    assert_eq!(ui.current(), "two.txt");
    ui.wait_text("Quick search: tw");
    ui.key(KeyCode::Esc);
    assert!(ui.app.panel().quick_search.is_none());
}

#[test]
fn browse_zip_and_copy_out() {
    let (_, a, b) = setup("zip");
    {
        let f = std::fs::File::create(a.join("pack.zip")).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opt = zip::write::SimpleFileOptions::default();
        z.add_directory("inner/", opt).unwrap();
        z.start_file("inner/note.txt", opt).unwrap();
        z.write_all(b"from the zip\n").unwrap();
        z.finish().unwrap();
    }
    let mut ui = Ui::new(&a, &b);
    ui.select("pack.zip");
    ui.key(KeyCode::Enter);
    ui.wait_text("pack.zip#/");
    ui.select("inner");
    ui.key(KeyCode::Enter);
    ui.select("note.txt");
    ui.f(5);
    ui.key(KeyCode::Enter);
    let out = b.join("note.txt");
    ui.wait_for("file copied out of the zip", |_| out.exists());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "from the zip\n");
    // Leaving the archive root returns to the containing directory.
    ui.key(KeyCode::Backspace);
    ui.key(KeyCode::Backspace);
    assert_eq!(ui.active_dir(), a);
    assert_eq!(ui.current(), "pack.zip");
}

#[test]
fn quick_view_shows_text_preview() {
    let (_, a, b) = setup("qview");
    write(&a.join("preview.txt"), "hello from the preview pane\n");
    let mut ui = Ui::new(&a, &b);
    ui.select("preview.txt");
    ui.ctrl('q');
    ui.wait_text("Quick view");
    ui.wait_text("hello from the preview pane");
    ui.ctrl('q');
    ui.wait_for("quick view closed", |ui| !ui.screen().contains("Quick view"));
}

#[test]
fn menu_navigation() {
    let (_, a, b) = setup("menu");
    let mut ui = Ui::new(&a, &b);
    ui.f(9);
    ui.wait_text("File listing (full)");
    ui.key(KeyCode::Right);
    ui.wait_text("View file...");
    ui.key(KeyCode::Right);
    ui.wait_text("Find file");
    ui.key(KeyCode::Esc);
    assert!(ui.app.dialogs.is_empty());
}

#[test]
fn mouse_selects_file_and_clicks_button_bar() {
    let (_, a, b) = setup("mouse");
    let mut ui = Ui::new(&a, &b);
    let list = ui.app.layout.lists[0];
    let idx = ui.names().iter().position(|n| n == "two.txt").unwrap();
    ui.click(list.x + 3, list.y + idx as u16);
    assert_eq!(ui.current(), "two.txt");
    // Clicking the other panel makes it active.
    let other = ui.app.layout.lists[1];
    ui.click(other.x + 3, other.y);
    assert_eq!(ui.app.active, 1);
    // F7 on the function key bar opens the mkdir dialog.
    let (r, _) = ui.app.layout.buttons.iter().find(|(_, n)| *n == 7).cloned().unwrap();
    ui.click(r.x + 1, r.y);
    ui.wait_text("Create a new Directory");
}

#[test]
fn viewer_open_and_close() {
    let (_, a, b) = setup("viewer");
    write(&a.join("code.rs"), "fn main() {\n    println!(\"viewer test\");\n}\n");
    let mut ui = Ui::new(&a, &b);
    ui.select("code.rs");
    ui.f(3);
    ui.wait_text("viewer test");
    assert_eq!(ui.app.screens.len(), 1);
    ui.typ("q");
    assert!(ui.app.screens.is_empty());
    ui.wait_text("code.rs");
}

#[test]
fn editor_type_and_save() {
    let (_, a, b) = setup("editor");
    write(&a.join("notes.txt"), "line one\n");
    let mut ui = Ui::new(&a, &b);
    ui.select("notes.txt");
    ui.f(4);
    ui.wait_text("line one");
    ui.key_mod(KeyCode::Home, KeyModifiers::CONTROL);
    ui.typ("hello ");
    ui.f(2);
    ui.f(10);
    assert!(ui.app.screens.is_empty(), "editor closed after save");
    assert_eq!(std::fs::read_to_string(a.join("notes.txt")).unwrap(), "hello line one\n");
}

#[test]
fn hotlist_add_and_jump() {
    let (_, a, b) = setup("hotlist");
    let mut ui = Ui::new(&a, &b);
    ui.ctrl('x');
    ui.typ("h");
    ui.wait_text("Add to hotlist");
    ui.key(KeyCode::Enter);
    assert!(ui.app.state.hotlist.iter().any(|h| h.path == a.display().to_string()));
    ui.select("sub");
    ui.key(KeyCode::Enter);
    assert_eq!(ui.active_dir(), a.join("sub"));
    ui.ctrl('\\');
    ui.wait_text("Directory hotlist");
    // Filter to our entry (the shared state may hold others), then jump.
    let name = a.display().to_string();
    let tail = &name[name.len() - 12..];
    ui.typ(tail);
    ui.key(KeyCode::Enter);
    assert_eq!(ui.active_dir(), a);
}

#[test]
fn cd_builtin_on_command_line() {
    let (_, a, b) = setup("cd");
    let mut ui = Ui::new(&a, &b);
    ui.typ("cd sub");
    ui.wait_text("cd sub");
    ui.key(KeyCode::Enter);
    assert_eq!(ui.active_dir(), a.join("sub"));
    assert!(ui.app.external.is_none(), "cd is handled internally");
    ui.typ("cd ..");
    ui.key(KeyCode::Enter);
    assert_eq!(ui.active_dir(), a);
}

#[test]
fn panelize_command_output() {
    let (_, a, b) = setup("panelize");
    let mut ui = Ui::new(&a, &b);
    ui.ctrl('x');
    ui.typ("!");
    ui.wait_text("External panelize");
    ui.typ("find . -name '*.md'");
    ui.key(KeyCode::Enter);
    ui.wait_for("panelized listing", |ui| ui.app.panel().panelized.is_some());
    let names = ui.names();
    assert!(names.contains(&"sub/deep.md".to_string()), "{names:?}");
    assert!(!names.contains(&"one.txt".to_string()));
    // '..' leaves the panelized view.
    ui.key(KeyCode::Home);
    ui.key(KeyCode::Enter);
    assert!(ui.app.panel().panelized.is_none());
    assert!(ui.names().contains(&"one.txt".to_string()));
}

/// `+ - *` select files only while the command line is empty; inside a command
/// they are ordinary characters (regression: `ls -la` used to open "Unselect").
#[test]
fn selection_keys_are_text_inside_a_command() {
    let (_, a, b) = setup("cmdkeys");
    let mut ui = Ui::new(&a, &b);
    ui.typ("ls -la a+b c*d");
    assert!(ui.app.dialogs.is_empty(), "a selection dialog opened while typing a command");
    assert_eq!(ui.app.cmdline.text(), "ls -la a+b c*d");
    // With an empty command line `*` still inverts the selection.
    ui.key(KeyCode::Esc);
    assert!(ui.app.cmdline.is_empty());
    ui.typ("*");
    assert!(!ui.app.panel().marked.is_empty(), "`*` should invert the selection");
}

/// Drawing must not panic at tiny or zero terminal sizes (regression: the
/// bottom-bar layout used to underflow when the terminal reported no rows).
#[test]
fn tiny_terminal_sizes_do_not_panic() {
    let (_, a, b) = setup("tiny");
    let mut ui = Ui::new(&a, &b);
    for (w, h) in [(0, 0), (1, 1), (2, 3), (10, 2), (20, 4), (3, 40), (120, 1), (20, 5)] {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| pairdeck::ui::draw(f, &mut ui.app)).unwrap();
    }
}
