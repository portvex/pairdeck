//! Directory sync through the real UI: Ctrl-X Y, options dialog, plan screen, execute.

mod common;

use common::{Ui, workdir, write};
use crossterm::event::KeyCode;
use std::time::{Duration, SystemTime};

fn age(path: &std::path::Path, secs_ago: u64) {
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago)).unwrap();
}

#[test]
fn sync_left_to_right_with_plan_edit() {
    let root = workdir("sync");
    let (a, b) = (root.join("a"), root.join("b"));
    write(&a.join("one.txt"), "new content\n");
    write(&a.join("sub/deep.md"), "# deep\n");
    write(&b.join("one.txt"), "old\n");
    write(&b.join("old.txt"), "only on the right\n");
    age(&b.join("one.txt"), 3600);

    let mut ui = Ui::new(&a, &b);
    ui.ctrl('x');
    ui.typ("Y");
    ui.wait_text("Synchronize directories");
    // Defaults: left -> right, size and time, no mirror.
    ui.key(KeyCode::Enter);
    ui.wait_text("-> 2 file(s)");
    let screen = ui.screen();
    assert!(screen.contains("one.txt") && screen.contains("sub/"), "{screen}");
    assert!(screen.contains("old.txt"), "right-only file is listed as skipped:\n{screen}");

    // Items are sorted: old.txt (skipped), one.txt, sub. Left on old.txt = right wins.
    ui.key(KeyCode::Home);
    ui.key(KeyCode::Left);
    ui.wait_text("<- 1 file(s)");
    ui.key(KeyCode::Enter);

    let deep = b.join("sub/deep.md");
    let back = a.join("old.txt");
    ui.wait_for("sync job", |_| deep.exists() && back.exists());
    assert_eq!(std::fs::read_to_string(b.join("one.txt")).unwrap(), "new content\n");
    assert_eq!(std::fs::read_to_string(&back).unwrap(), "only on the right\n");
    assert!(b.join("old.txt").exists(), "nothing is deleted without mirror");

    // Running it again finds the trees in sync.
    ui.ctrl('x');
    ui.typ("Y");
    ui.wait_text("Synchronize directories");
    ui.key(KeyCode::Enter);
    ui.wait_text("The directories are in sync.");
    ui.key(KeyCode::Esc);
}
