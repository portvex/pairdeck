//! Embedded terminal pane through the real App: open, run a command, focus switching.

mod common;

use common::{Ui, test_config, workdir, write};
use crossterm::event::{KeyCode, KeyModifiers};

#[test]
fn terminal_pane_runs_commands() {
    let root = workdir("term");
    let a = root.join("a");
    let b = root.join("b");
    write(&a.join("one.txt"), "x\n");
    std::fs::create_dir_all(&b).unwrap();
    let mut cfg = test_config();
    // A predictable shell instead of the user's $SHELL with its rc files.
    cfg.terminal_shell = Some("/bin/sh".into());
    let mut ui = Ui::with_config(&a, &b, cfg);

    // Alt-Z opens the pane focused; typed keys go to the shell.
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    assert!(ui.app.term.shown() && ui.app.term.focused);
    ui.wait_text("Alt-Z panels");
    // The arithmetic makes the output differ from the typed command line.
    ui.typ("echo pairdeck-$((40+2))");
    ui.key(KeyCode::Enter);
    ui.wait_text("pairdeck-42");
    // The shell started in the active panel's directory.
    ui.typ("pwd");
    ui.key(KeyCode::Enter);
    let dir = a.canonicalize().unwrap().display().to_string();
    let tail: String = dir.chars().rev().take(30).collect::<Vec<_>>().into_iter().rev().collect();
    ui.wait_text(&tail);

    // The panels stay visible above the pane.
    assert!(ui.screen().contains("one.txt"));

    // Alt-Z gives focus back to the panels; keys move the cursor again.
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    assert!(ui.app.term.shown() && !ui.app.term.focused);
    ui.select("one.txt");
    assert_eq!(ui.current(), "one.txt");

    // Exiting the shell closes the pane.
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    ui.typ("exit");
    ui.key(KeyCode::Enter);
    ui.wait_for("pane closes", |ui| !ui.app.term.shown());
    assert!(ui.app.term.session.is_none());
}

#[test]
fn terminal_pane_maximize_and_hide() {
    let root = workdir("term-max");
    let a = root.join("a");
    write(&a.join("f.txt"), "x\n");
    let mut cfg = test_config();
    cfg.terminal_shell = Some("/bin/sh".into());
    let mut ui = Ui::with_config(&a, &a, cfg);
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    ui.wait_text("Alt-Z panels");
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    // Ctrl-X Z maximizes: the panels are gone, the pane fills the area.
    ui.ctrl('x');
    ui.key(KeyCode::Char('Z'));
    ui.settle();
    assert!(ui.app.term.maximized);
    assert!(!ui.screen().contains("f.txt"));
    // Ctrl-X K hides the pane; the shell keeps running and Alt-Z brings it back.
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    ui.ctrl('x');
    ui.key(KeyCode::Char('k'));
    ui.settle();
    assert!(!ui.app.term.visible && ui.app.term.session.is_some());
    assert!(ui.screen().contains("f.txt"));
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    assert!(ui.app.term.shown() && ui.app.term.focused);
}

/// The shell docked as a panel: Ctrl-X Ctrl-Z puts it in the other panel next to
/// the file list, focus follows Tab / Alt-Z / clicks, [x] returns the file list,
/// and the session survives moving between panel and bottom split.
#[test]
fn terminal_docked_in_a_panel() {
    use pairdeck::panel::PanelView;
    let root = workdir("term-dock");
    let a = root.join("a");
    let b = root.join("b");
    write(&a.join("left.txt"), "x\n");
    write(&b.join("right.txt"), "x\n");
    let mut cfg = test_config();
    cfg.terminal_shell = Some("/bin/sh".into());
    let mut ui = Ui::with_config(&a, &b, cfg);
    assert_eq!(ui.app.active, 0);

    // Dock into the other (right) panel; it becomes active and focused.
    ui.ctrl('x');
    ui.ctrl('z');
    assert_eq!(ui.app.panels[1].view, PanelView::Terminal);
    assert_eq!(ui.app.term.docked, Some(1));
    assert!(ui.app.term.focused && ui.app.active == 1);
    ui.typ("echo dock-$((6*7))");
    ui.key(KeyCode::Enter);
    ui.wait_text("dock-42");
    // The left file list stays visible; the right file list is replaced.
    let screen = ui.screen();
    assert!(screen.contains("left.txt") && !screen.contains("right.txt"));
    // The shell output is drawn inside the right panel's rectangle.
    let right = ui.app.layout.panels[1];
    assert!(ui.app.term.rect == right, "docked rect {:?} != panel {:?}", ui.app.term.rect, right);
    let pid = ui.app.term.session.as_ref().map(|s| s.id).unwrap();

    // Alt-Z: focus back to the file panel; Tab: into the shell again.
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    assert!(!ui.app.term.focused && ui.app.active == 0);
    ui.select("left.txt");
    assert_eq!(ui.current(), "left.txt");
    ui.key(KeyCode::Tab);
    assert!(ui.app.term.focused && ui.app.active == 1);

    // Clicking the file panel unfocuses the shell; clicking the shell focuses it.
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    let lp = ui.app.layout.panels[0];
    ui.click(lp.x + 3, lp.y + 3);
    assert!(!ui.app.term.focused && ui.app.active == 0);
    ui.click(right.x + 5, right.y + 3);
    assert!(ui.app.term.focused && ui.app.active == 1);

    // [x] in the docked panel's border returns the file list; the shell keeps
    // running and Alt-Z shows the same session in the bottom split.
    ui.click(right.right() - 4, right.y);
    assert_eq!(ui.app.panels[1].view, PanelView::List);
    assert_eq!(ui.app.term.docked, None);
    assert!(ui.screen().contains("right.txt"));
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    assert!(ui.app.term.shown());
    assert_eq!(ui.app.term.session.as_ref().map(|s| s.id), Some(pid), "same shell session");
    ui.wait_text("dock-42");

    // Docking from the Left menu works too, and exiting the shell restores the list.
    ui.key_mod(KeyCode::Char('z'), KeyModifiers::ALT);
    pairdeck::term::dock(&mut ui.app, 0);
    assert_eq!(ui.app.panels[0].view, PanelView::Terminal);
    ui.typ("exit");
    ui.key(KeyCode::Enter);
    ui.wait_for("shell exits", |ui| ui.app.term.session.is_none());
    ui.draw();
    assert_eq!(ui.app.panels[0].view, PanelView::List);
    assert!(ui.screen().contains("left.txt"));
}
