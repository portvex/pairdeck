//! Tests for asynchronous directory listing.

use super::*;

fn wait_for(p: &mut Panel) -> Option<Result<()>> {
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        p.loading.as_ref()?;
        if let Some(r) = p.poll_load() {
            return Some(r);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("listing never finished");
}

fn tree(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("pairdeck-async-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("sub")).unwrap();
    for n in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(base.join("sub").join(n), n).unwrap();
    }
    std::fs::create_dir_all(base.join("other")).unwrap();
    base
}

#[test]
fn async_chdir_applies_focus_after_listing() {
    let base = tree("focus");
    set_sync_wait(0);
    let mut p = Panel::new(Location::local(&base), &Config::default());
    wait_for(&mut p);
    p.chdir(Location::local(base.join("sub")), Some("b.txt")).unwrap();
    let r = wait_for(&mut p);
    assert!(r.is_none_or(|r| r.is_ok()));
    assert_eq!(p.current_name().as_deref(), Some("b.txt"), "focus applied after the async listing");
    assert_eq!(p.entries.len(), 4, ".. plus three files");
    assert_eq!(p.display_name(), base.join("sub").display().to_string());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn failed_async_listing_goes_back() {
    let base = tree("fail");
    set_sync_wait(0);
    let mut p = Panel::new(Location::local(&base), &Config::default());
    wait_for(&mut p);
    p.focus("sub");
    let missing = base.join("does-not-exist");
    match p.chdir(Location::local(&missing), None) {
        // The listing failed before the zero wait elapsed: plain error, nothing changed.
        Err(_) => {}
        Ok(()) => {
            let r = wait_for(&mut p).expect("a result");
            assert!(r.is_err());
        }
    }
    wait_for(&mut p);
    assert_eq!(p.loc.path, base, "back in the previous directory");
    assert_eq!(p.current_name().as_deref(), Some("sub"), "cursor restored");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn cancel_restores_previous_directory() {
    let base = tree("cancel");
    set_sync_wait(0);
    let mut p = Panel::new(Location::local(&base), &Config::default());
    wait_for(&mut p);
    p.chdir(Location::local(base.join("other")), None).unwrap();
    if p.is_loading() {
        assert!(p.cancel_load());
        wait_for(&mut p);
        assert_eq!(p.loc.path, base, "cancel goes back");
    }
    assert!(!p.cancel_load(), "nothing left to cancel");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn newer_load_supersedes_older() {
    let base = tree("supersede");
    set_sync_wait(0);
    let mut p = Panel::new(Location::local(&base), &Config::default());
    wait_for(&mut p);
    // Two quick directory changes: only the last one may win.
    p.chdir(Location::local(base.join("other")), None).unwrap();
    p.chdir(Location::local(base.join("sub")), Some("c.txt")).unwrap();
    wait_for(&mut p);
    assert_eq!(p.loc.path, base.join("sub"));
    assert_eq!(p.current_name().as_deref(), Some("c.txt"));
    let _ = std::fs::remove_dir_all(&base);
}
