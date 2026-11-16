use super::*;
use crate::app::AppEvent;
use crate::ops::JobMsg;
use std::path::PathBuf;
use std::time::Duration;

fn root(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pairdeck-sync-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("L")).unwrap();
    std::fs::create_dir_all(d.join("R")).unwrap();
    d
}

/// Write `text` at `rel` under `dir`, with mtime `t` seconds after a fixed epoch.
fn put(dir: &Path, rel: &str, text: &str, t: u64) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    let f = std::fs::File::options().write(true).open(&p).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + t)).unwrap();
}

fn plan(r: &Path, opts: SyncOptions) -> Plan {
    let (c, k) = (AtomicU64::new(0), AtomicBool::new(false));
    compute_plan(&Location::local(r.join("L")), &Location::local(r.join("R")), &opts, &c, &k).unwrap()
}

fn action_of(p: &Plan, rel: &str) -> Option<Action> {
    p.items.iter().find(|i| i.rel == rel).map(|i| i.action)
}

fn opts(direction: Direction) -> SyncOptions {
    SyncOptions { direction, ..SyncOptions::default() }
}

#[test]
fn one_way_left_to_right() {
    let r = root("lr");
    let (l, rr) = (r.join("L"), r.join("R"));
    put(&l, "new.txt", "n", 10);
    put(&l, "changed.txt", "left version", 20);
    put(&rr, "changed.txt", "old", 10);
    put(&l, "same.txt", "s", 10);
    put(&rr, "same.txt", "s", 10);
    put(&rr, "extra.txt", "x", 10);
    let p = plan(&r, opts(Direction::LeftToRight));
    assert_eq!(action_of(&p, "new.txt"), Some(Action::CopyLR));
    assert_eq!(action_of(&p, "changed.txt"), Some(Action::CopyLR));
    assert_eq!(action_of(&p, "same.txt"), None, "identical files are not listed");
    assert_eq!(action_of(&p, "extra.txt"), Some(Action::Skip));
    let p = plan(&r, SyncOptions { mirror: true, ..opts(Direction::LeftToRight) });
    assert_eq!(action_of(&p, "extra.txt"), Some(Action::DeleteR));
    let s = p.summary();
    assert_eq!((s.lr_files, s.del_right), (2, 1));
}

#[test]
fn one_way_right_to_left_marks_target_newer() {
    let r = root("rl");
    let (l, rr) = (r.join("L"), r.join("R"));
    put(&l, "f.txt", "newer on left", 50);
    put(&rr, "f.txt", "older", 10);
    put(&rr, "only_r.txt", "r", 10);
    let p = plan(&r, opts(Direction::RightToLeft));
    let it = p.items.iter().find(|i| i.rel == "f.txt").unwrap();
    assert_eq!(it.action, Action::CopyRL);
    assert_eq!(it.note, "target newer");
    assert_eq!(action_of(&p, "only_r.txt"), Some(Action::CopyRL));
}

#[test]
fn two_way_newer_wins_and_conflicts() {
    let r = root("two");
    let (l, rr) = (r.join("L"), r.join("R"));
    put(&l, "a.txt", "left newer", 100);
    put(&rr, "a.txt", "right", 10);
    put(&l, "b.txt", "left", 10);
    put(&rr, "b.txt", "right newer", 100);
    put(&l, "c.txt", "same time, size A", 10);
    put(&rr, "c.txt", "same time, other size", 10);
    put(&l, "onlyl.txt", "x", 1);
    put(&rr, "onlyr.txt", "y", 1);
    let p = plan(&r, opts(Direction::TwoWay));
    assert_eq!(action_of(&p, "a.txt"), Some(Action::CopyLR));
    assert_eq!(action_of(&p, "b.txt"), Some(Action::CopyRL));
    assert_eq!(action_of(&p, "c.txt"), Some(Action::Conflict));
    assert_eq!(action_of(&p, "onlyl.txt"), Some(Action::CopyLR));
    assert_eq!(action_of(&p, "onlyr.txt"), Some(Action::CopyRL));
    assert_eq!(p.summary().conflicts, 1);
}

#[test]
fn mtime_tolerance() {
    let r = root("tol");
    let (l, rr) = (r.join("L"), r.join("R"));
    put(&l, "close.txt", "same", 10);
    put(&rr, "close.txt", "same", 11);
    put(&l, "far.txt", "same", 10);
    put(&rr, "far.txt", "same", 15);
    let p = plan(&r, opts(Direction::LeftToRight));
    assert_eq!(action_of(&p, "close.txt"), None, "1 s apart is within the 2 s tolerance");
    assert_eq!(action_of(&p, "far.txt"), Some(Action::CopyLR));
    let p = plan(&r, SyncOptions { compare: CompareBy::Size, ..opts(Direction::LeftToRight) });
    assert!(p.items.is_empty(), "size-only ignores times");
}

#[test]
fn excludes_and_includes() {
    let r = root("exc");
    let l = r.join("L");
    put(&l, ".git/config", "x", 1);
    put(&l, ".DS_Store", "x", 1);
    put(&l, "keep.txt", "k", 1);
    put(&l, "skip.log", "s", 1);
    put(&l, "sub/inner.txt", "i", 1);
    let p = plan(&r, opts(Direction::LeftToRight));
    let rels: Vec<&str> = p.items.iter().map(|i| i.rel.as_str()).collect();
    assert_eq!(rels, vec!["keep.txt", "skip.log", "sub"]);
    let p = plan(&r, SyncOptions { include: vec!["*.txt".into()], ..opts(Direction::LeftToRight) });
    let rels: Vec<&str> = p.items.iter().map(|i| i.rel.as_str()).collect();
    assert_eq!(rels, vec!["keep.txt", "sub"]);
    // The new directory is one item with its file count.
    let sub = p.items.iter().find(|i| i.rel == "sub").unwrap();
    assert_eq!(sub.left.as_ref().unwrap().files, 1);
}

#[test]
fn checksum_detects_same_size_and_time() {
    let r = root("sum");
    let (l, rr) = (r.join("L"), r.join("R"));
    put(&l, "f.bin", "AAAA", 10);
    put(&rr, "f.bin", "BBBB", 10);
    assert!(plan(&r, opts(Direction::LeftToRight)).items.is_empty());
    let p = plan(&r, SyncOptions { compare: CompareBy::Checksum, ..opts(Direction::LeftToRight) });
    assert_eq!(action_of(&p, "f.bin"), Some(Action::CopyLR));
}

#[test]
fn new_dir_and_mirror_delete_steps() {
    let r = root("steps");
    let (l, rr) = (r.join("L"), r.join("R"));
    put(&l, "d/a.txt", "a", 1);
    put(&l, "d/e/b.txt", "bb", 1);
    put(&rr, "gone/x.txt", "x", 1);
    put(&rr, "kept/y.txt", "y", 1);
    put(&rr, "kept/.DS_Store", "z", 1);
    let p = plan(&r, SyncOptions { mirror: true, ..opts(Direction::LeftToRight) });
    let steps = p.steps();
    let pos = |want: &str| {
        steps
            .iter()
            .position(|s| match s {
                SyncStep::Mkdir { to_left: false, rel } => rel == want,
                SyncStep::Copy { from_left: true, rel, .. } => rel == want,
                _ => false,
            })
            .unwrap_or_else(|| panic!("no step for {want}: {steps:?}"))
    };
    // Parent directories come before their contents.
    assert!(pos("d") < pos("d/a.txt"));
    assert!(pos("d") < pos("d/e") && pos("d/e") < pos("d/e/b.txt"));
    // A dir without excluded content is removed as a whole ...
    assert!(
        steps.iter().any(|s| matches!(s, SyncStep::Delete { on_left: false, rel, tree: true, .. } if rel == "gone"))
    );
    // ... one with excluded content (.DS_Store) keeps it: files deleted, then rmdir (fails harmlessly).
    assert!(steps.iter().any(|s| matches!(s, SyncStep::Delete { rel, tree: false, .. } if rel == "kept/y.txt")));
    assert!(steps.iter().any(|s| matches!(s, SyncStep::Rmdir { rel, .. } if rel == "kept")));
}

#[test]
fn type_mismatch() {
    let r = root("type");
    let (l, rr) = (r.join("L"), r.join("R"));
    put(&l, "x", "file", 1);
    put(&rr, "x/inner.txt", "dir", 1);
    let p = plan(&r, opts(Direction::TwoWay));
    let it = p.items.iter().find(|i| i.rel == "x").unwrap();
    assert_eq!((it.kind, it.action), (Kind::TypeMismatch, Action::Conflict));
    assert_eq!(p.items.len(), 1, "children of the mismatched dir are not listed separately");
}

/// Run the plan through the real job worker (permanent deletes, never the trash).
fn execute(p: &Plan) {
    let (tx, rx) = crossbeam_channel::unbounded::<AppEvent>();
    let steps = p.steps();
    crate::ops::spawn(
        JobSpec::Sync { left: p.left.clone(), right: p.right.clone(), steps, trash: false, follow_links: false },
        tx,
    );
    loop {
        match rx.recv_timeout(Duration::from_secs(20)).expect("job finished") {
            AppEvent::Job(JobMsg::Finished { errors, cancelled, .. }) => {
                assert!(errors.is_empty() && !cancelled, "sync errors: {errors:?}");
                return;
            }
            AppEvent::Job(JobMsg::Conflict { .. }) | AppEvent::Job(JobMsg::Error { .. }) => {
                panic!("sync job asked a question")
            }
            _ => {}
        }
    }
}

#[test]
fn executing_two_way_plan_makes_trees_identical() {
    let r = root("exec2");
    let (l, rr) = (r.join("L"), r.join("R"));
    put(&l, "a.txt", "left newer", 100);
    put(&rr, "a.txt", "right", 10);
    put(&rr, "b.txt", "right newer", 100);
    put(&l, "b.txt", "left", 10);
    put(&l, "dir/one.txt", "1", 1);
    put(&l, "dir/deep/two.txt", "22", 1);
    put(&rr, "rdir/three.txt", "333", 1);
    let p = plan(&r, opts(Direction::TwoWay));
    execute(&p);
    assert!(trees_identical(&l, &rr));
    assert_eq!(std::fs::read_to_string(rr.join("a.txt")).unwrap(), "left newer");
    assert_eq!(std::fs::read_to_string(l.join("b.txt")).unwrap(), "right newer");
    // A second plan finds nothing to do.
    assert!(plan(&r, opts(Direction::TwoWay)).items.is_empty());
}

#[test]
fn executing_mirror_plan_deletes_extras() {
    let r = root("mirror");
    let (l, rr) = (r.join("L"), r.join("R"));
    put(&l, "keep.txt", "k", 1);
    put(&rr, "extra.txt", "e", 1);
    put(&rr, "old/stuff.txt", "s", 1);
    put(&l, "x", "now a file", 5);
    put(&rr, "x/was_dir.txt", "d", 1);
    let p = plan(&r, SyncOptions { mirror: true, ..opts(Direction::LeftToRight) });
    execute(&p);
    assert!(trees_identical(&l, &rr));
    assert!(!rr.join("old").exists() && rr.join("x").is_file());
}
