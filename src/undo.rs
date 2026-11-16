//! Undo journal for reversible operations performed in this session.

use crate::app::App;
use crate::dialog::{self, ListAction, ListDialog, ListItem};
use crate::ops::{CopyOpts, JobSpec, Overwrite};
use crate::vfs::{Location, VfsHandle};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One reversible change.
#[derive(Clone, Debug)]
pub enum UndoOp {
    /// `from` was renamed / moved to `to` (both inside `vfs` when `same_vfs`).
    Move { src: VfsHandle, from: PathBuf, dst: VfsHandle, to: PathBuf },
    /// Directories created (outermost first).
    Mkdir { vfs: VfsHandle, paths: Vec<PathBuf> },
    /// Files or trees created by a copy.
    Copy { vfs: VfsHandle, paths: Vec<PathBuf> },
    /// A symbolic link was created.
    Symlink { vfs: VfsHandle, path: PathBuf },
    /// Local items moved to the trash.
    Trash { items: Vec<PathBuf>, when: SystemTime },
    /// Several operations undone together, last first (e.g. bulk rename).
    Batch(Vec<UndoOp>),
    /// Files rewritten in place: (target, local backup of the original); plus new
    /// files created next to them (image actions).
    Restore { vfs: VfsHandle, items: Vec<(PathBuf, PathBuf)>, created: Vec<PathBuf> },
}

#[derive(Clone, Debug)]
pub struct Record {
    pub label: String,
    pub when: SystemTime,
    pub op: UndoOp,
}

/// Session journal, newest last.
#[derive(Default)]
pub struct Journal {
    pub records: Vec<Record>,
}

const MAX_RECORDS: usize = 200;

impl Journal {
    pub fn push(&mut self, label: impl Into<String>, op: UndoOp) {
        if let UndoOp::Batch(v) = &op
            && v.is_empty()
        {
            return;
        }
        self.records.push(Record { label: label.into(), when: SystemTime::now(), op });
        if self.records.len() > MAX_RECORDS {
            self.records.remove(0);
        }
    }

    pub fn pop(&mut self) -> Option<Record> {
        self.records.pop()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Result of undoing one operation.
pub enum Outcome {
    Done(String),
    /// Needs confirmation or a background job; the caller decides.
    Deferred,
}

/// Undo the most recent operation.
pub fn undo_last(app: &mut App) {
    match app.undo.pop() {
        Some(r) => run(app, r),
        None => app.info("Nothing to undo"),
    }
}

/// Show the journal; Enter undoes the chosen entry and everything after it.
pub fn history(app: &mut App) {
    if app.undo.is_empty() {
        app.push_dialog(dialog::message("Undo history", "Nothing to undo in this session."));
        return;
    }
    let items: Vec<ListItem> = app
        .undo
        .records
        .iter()
        .rev()
        .map(|r| ListItem::new(&r.label).hint(crate::util::short_time(Some(r.when))))
        .collect();
    let n = app.undo.len();
    let d = ListDialog::new("Undo history (newest first)", items, move |app, a| {
        if let ListAction::Select(i) = a {
            // Row i is record n-1-i: undo everything newer than it, then it.
            for _ in 0..=i.min(n - 1) {
                if let Some(r) = app.undo.pop() {
                    run(app, r);
                }
            }
        }
    })
    .footer("Enter: undo this and all newer operations");
    app.push_dialog(Box::new(d));
}

fn run(app: &mut App, r: Record) {
    let label = r.label.clone();
    match &r.op {
        UndoOp::Copy { vfs, paths } => {
            // Deleting is destructive: ask first, and prefer the trash.
            let vfs = vfs.clone();
            let paths = paths.clone();
            let text = format!("Undo \"{label}\" by deleting {} copied item(s)?", paths.len());
            app.push_dialog(dialog::confirm("Undo copy", &text, move |app| {
                start_delete(app, &vfs, &paths);
            }));
        }
        UndoOp::Trash { items, when } => {
            // Restoring may need Finder (and a one-time macOS permission prompt), so it
            // runs off the UI thread.
            let (items, when) = (items.clone(), *when);
            let tx = app.tx.clone();
            app.info(format!("Restoring from the trash: {label}"));
            std::thread::spawn(move || {
                let res = restore_from_trash(&items, when);
                crate::app::call(&tx, move |app| {
                    match res {
                        Ok(n) => app.info(format!("Restored {n} item(s) from the trash")),
                        Err(e) => app.error(format!("Cannot undo \"{label}\":\n{e:#}")),
                    }
                    app.reload_panels();
                });
            });
        }
        _ => match undo_op(app, &r.op) {
            Ok(Outcome::Done(msg)) => {
                app.info(if msg.is_empty() { format!("Undone: {label}") } else { msg });
                app.reload_panels();
            }
            Ok(Outcome::Deferred) => app.info(format!("Undoing: {label}")),
            Err(e) => app.error(format!("Cannot undo \"{label}\":\n{e:#}")),
        },
    }
}

fn start_delete(app: &mut App, vfs: &VfsHandle, paths: &[PathBuf]) {
    let local = vfs.is_local();
    let loc = Location { vfs: vfs.clone(), path: PathBuf::from("/") };
    let names: Vec<String> =
        paths.iter().map(|p| p.strip_prefix("/").unwrap_or(p).to_string_lossy().into_owned()).collect();
    app.start_job(JobSpec::Delete { loc, names, trash: local && app.cfg.use_trash }, true);
}

fn undo_op(app: &mut App, op: &UndoOp) -> anyhow::Result<Outcome> {
    match undo_direct(op)? {
        Direct::Done(msg) => Ok(Outcome::Done(msg)),
        Direct::NeedsMoveJob { src, from, dst, to } => {
            move_back(app, &dst, &to, &src, &from);
            Ok(Outcome::Deferred)
        }
        Direct::NeedsBatch(ops) => {
            for o in ops.iter().rev() {
                undo_op(app, o)?;
            }
            Ok(Outcome::Done(String::new()))
        }
    }
}

/// Result of the part of an undo that needs no App.
pub enum Direct {
    Done(String),
    /// A cross file system move that must be copied back by a background job.
    NeedsMoveJob {
        src: VfsHandle,
        from: PathBuf,
        dst: VfsHandle,
        to: PathBuf,
    },
    NeedsBatch(Vec<UndoOp>),
}

/// Undo what can be undone directly (renames, mkdir, symlinks, trash).
pub fn undo_direct(op: &UndoOp) -> anyhow::Result<Direct> {
    match op {
        UndoOp::Move { src, from, dst, to } => {
            let job =
                || Direct::NeedsMoveJob { src: src.clone(), from: from.clone(), dst: dst.clone(), to: to.clone() };
            if !(src.same(dst) || (src.is_local() && dst.is_local())) {
                return Ok(job());
            }
            if src.lock().stat(from).is_ok() {
                anyhow::bail!("{} exists again", from.display());
            }
            let res = dst.lock().rename(to, from);
            match res {
                Ok(()) => Ok(Direct::Done(String::new())),
                Err(e) if src.same(dst) => Err(e),
                // Different local devices: move back with a background job.
                Err(_) => Ok(job()),
            }
        }
        UndoOp::Batch(ops) => {
            // Pure renames (bulk rename) are undone right here, last first.
            if ops.iter().all(|o| matches!(o, UndoOp::Move { src, dst, .. } if src.same(dst))) {
                for o in ops.iter().rev() {
                    undo_direct(o)?;
                }
                return Ok(Direct::Done(String::new()));
            }
            Ok(Direct::NeedsBatch(ops.clone()))
        }
        UndoOp::Copy { .. } => anyhow::bail!("copies are undone by deleting them (needs confirmation)"),
        other => undo_simple(other).map(Direct::Done),
    }
}

fn undo_simple(op: &UndoOp) -> anyhow::Result<String> {
    match op {
        UndoOp::Mkdir { vfs, paths } => {
            let mut v = vfs.lock();
            for p in paths.iter().rev() {
                v.remove_dir(p).map_err(|e| anyhow::anyhow!("{e:#} (directory not empty?)"))?;
            }
            Ok(String::new())
        }
        UndoOp::Symlink { vfs, path } => {
            let mut v = vfs.lock();
            match v.stat(path) {
                Ok(e) if e.is_symlink() => v.remove_file(path)?,
                Ok(_) => anyhow::bail!("{} is no longer a symlink", path.display()),
                Err(_) => {}
            }
            Ok(String::new())
        }
        UndoOp::Trash { items, when } => {
            let restored = restore_from_trash(items, *when)?;
            Ok(format!("Restored {restored} item(s) from the trash"))
        }
        UndoOp::Restore { vfs, items, created } => crate::imageops::undo_restore(vfs, items, created),
        UndoOp::Move { .. } | UndoOp::Batch(_) | UndoOp::Copy { .. } => {
            anyhow::bail!("internal: not a simple undo operation")
        }
    }
}

/// Move `to` back to `from` across file systems with a copy job.
fn move_back(app: &mut App, dst: &VfsHandle, to: &Path, src: &VfsHandle, from: &Path) {
    let parent = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
    let name = to.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let target = from.file_name().map(|n| n.to_string_lossy().into_owned());
    let spec = JobSpec::Copy {
        src: Location { vfs: dst.clone(), path: parent(to) },
        names: vec![name],
        dst: Location { vfs: src.clone(), path: parent(from) },
        target,
        opts: CopyOpts { overwrite: Overwrite::Never, ..CopyOpts::default() },
        remove_src: true,
    };
    app.start_job(spec, true);
}

/// Put trashed items back where they were. Returns how many were restored.
pub fn restore_from_trash(items: &[PathBuf], when: SystemTime) -> anyhow::Result<usize> {
    let mut restored = 0;
    let mut missing = Vec::new();
    for orig in items {
        if orig.exists() {
            missing.push(format!("{} already exists", orig.display()));
            continue;
        }
        match restore_one(orig, when) {
            Ok(()) => restored += 1,
            Err(e) => missing.push(format!("{}: {e:#}", orig.display())),
        }
    }
    if restored == 0 && !missing.is_empty() {
        anyhow::bail!("{}", missing.join("\n"));
    }
    Ok(restored)
}

#[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
fn restore_one(orig: &Path, _when: SystemTime) -> anyhow::Result<()> {
    let items = trash::os_limited::list().map_err(|e| anyhow::anyhow!("{e}"))?;
    let name = orig.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    let parent = orig.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut found: Vec<_> = items.into_iter().filter(|i| i.name == name && i.original_parent == parent).collect();
    found.sort_by_key(|i| i.time_deleted);
    let item = found.pop().ok_or_else(|| anyhow::anyhow!("not found in the trash"))?;
    trash::os_limited::restore_all([item]).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd")))]
fn restore_one(orig: &Path, when: SystemTime) -> anyhow::Result<()> {
    // macOS and others: look for the item in the user's trash by name. The system
    // may add a suffix (" 2", " 12.34.56") when an item of that name is already there.
    let trash_dir = crate::util::home().join(".Trash");
    if std::fs::read_dir(&trash_dir).is_err() {
        // macOS privacy protection hides ~/.Trash from terminal apps without Full
        // Disk Access. Finder can still put the item back.
        return finder_put_back(orig);
    }
    let cand = find_in_trash(&trash_dir, orig, when)
        .ok_or_else(|| anyhow::anyhow!("not found in {} (it may be on another volume)", trash_dir.display()))?;
    std::fs::rename(&cand, orig).map_err(|e| anyhow::anyhow!("cannot move it back: {e}"))
}

/// Ask Finder to move the trashed item named like `orig` back to its folder.
/// Name and folder are passed as arguments, never interpolated into the script.
#[cfg(target_os = "macos")]
fn finder_put_back(orig: &Path) -> anyhow::Result<()> {
    const SCRIPT: &str = r#"on run argv
    set n to item 1 of argv
    set d to item 2 of argv
    tell application "Finder"
        set f to (first item of trash whose name is n)
        move f to (POSIX file d as alias)
    end tell
end run"#;
    let name = orig.file_name().ok_or_else(|| anyhow::anyhow!("no file name"))?;
    let dir = orig.parent().ok_or_else(|| anyhow::anyhow!("no parent folder"))?;
    let mut child = std::process::Command::new("osascript")
        .arg("-e")
        .arg(SCRIPT)
        .arg(name)
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("cannot run osascript: {e}"))?;
    // The first use may show a permission prompt; give the user time to answer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            anyhow::bail!("Finder did not answer");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    if status.success() && orig.exists() {
        return Ok(());
    }
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = std::io::Read::read_to_string(&mut e, &mut err);
    }
    anyhow::bail!(
        "macOS does not allow reading the Trash ({}). Use Finder: open the Trash, select the item, File > Put Back, or give your terminal Full Disk Access.",
        err.trim().lines().last().unwrap_or("access denied")
    )
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
fn finder_put_back(_orig: &Path) -> anyhow::Result<()> {
    anyhow::bail!("cannot read the trash folder")
}

/// Best candidate in `trash_dir` for `orig`: an exact name match, or the same stem
/// followed by a suffix, preferring items changed closest to (and after) `when`.
pub fn find_in_trash(trash_dir: &Path, orig: &Path, when: SystemTime) -> Option<PathBuf> {
    let name = orig.file_name()?.to_string_lossy().into_owned();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (name[..i].to_string(), name[i..].to_string()),
        _ => (name.clone(), String::new()),
    };
    let mut best: Option<(u8, std::time::Duration, PathBuf)> = None;
    for de in std::fs::read_dir(trash_dir).ok()?.flatten() {
        let n = de.file_name().to_string_lossy().into_owned();
        let rank = if n == name {
            0
        } else if n.starts_with(&format!("{stem} ")) && n.ends_with(&ext) {
            1
        } else {
            continue;
        };
        let changed = de.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
        let dist = changed.duration_since(when).or_else(|e| Ok::<_, ()>(e.duration())).unwrap_or_default();
        let better = match &best {
            None => true,
            Some((r, d, _)) => (rank, dist) < (*r, *d),
        };
        if better {
            best = Some((rank, dist, de.path()));
        }
    }
    best.map(|(_, _, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_push_pop_and_cap() {
        let mut j = Journal::default();
        let v = VfsHandle::local();
        for i in 0..(MAX_RECORDS + 5) {
            j.push(format!("op {i}"), UndoOp::Symlink { vfs: v.clone(), path: PathBuf::from(format!("/x{i}")) });
        }
        assert_eq!(j.len(), MAX_RECORDS);
        assert_eq!(j.pop().unwrap().label, format!("op {}", MAX_RECORDS + 4));
        j.push("empty batch", UndoOp::Batch(Vec::new()));
        assert_eq!(j.len(), MAX_RECORDS - 1, "empty batches are not recorded");
    }

    #[test]
    fn undo_rename_mkdir_symlink_and_batch() {
        let base = std::env::temp_dir().join(format!("pairdeck-undo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let v = VfsHandle::local();
        let a = base.join("a.txt");
        let b = base.join("b.txt");
        std::fs::write(&a, "x").unwrap();
        std::fs::rename(&a, &b).unwrap();
        let mv = UndoOp::Move { src: v.clone(), from: a.clone(), dst: v.clone(), to: b.clone() };
        std::fs::create_dir_all(base.join("n1/n2")).unwrap();
        let mk = UndoOp::Mkdir { vfs: v.clone(), paths: vec![base.join("n1"), base.join("n1/n2")] };
        std::os::unix::fs::symlink("a.txt", base.join("ln")).unwrap();
        let sl = UndoOp::Symlink { vfs: v.clone(), path: base.join("ln") };
        for op in [mv, mk, sl] {
            assert!(matches!(undo_direct(&op).unwrap(), Direct::Done(_)));
        }
        assert!(a.exists() && !b.exists(), "rename undone");
        assert!(!base.join("n1").exists(), "mkdir undone");
        assert!(std::fs::symlink_metadata(base.join("ln")).is_err(), "symlink removed");
        // Undoing the rename again must refuse: the original name exists.
        let again = UndoOp::Move { src: v.clone(), from: a.clone(), dst: v.clone(), to: b.clone() };
        assert!(undo_direct(&again).is_err());

        // A bulk-rename style batch (a swap through a temp name) is undone in reverse.
        let x = base.join("x");
        let y = base.join("y");
        std::fs::write(&x, "X").unwrap();
        std::fs::write(&y, "Y").unwrap();
        let t = base.join(".tmp");
        let steps = [(x.clone(), t.clone()), (y.clone(), x.clone()), (t.clone(), y.clone())];
        let mut ops = Vec::new();
        for (from, to) in steps {
            std::fs::rename(&from, &to).unwrap();
            ops.push(UndoOp::Move { src: v.clone(), from, dst: v.clone(), to });
        }
        assert_eq!(std::fs::read_to_string(&x).unwrap(), "Y", "swapped");
        assert!(matches!(undo_direct(&UndoOp::Batch(ops)).unwrap(), Direct::Done(_)));
        assert_eq!(std::fs::read_to_string(&x).unwrap(), "X", "swap undone");
        assert_eq!(std::fs::read_to_string(&y).unwrap(), "Y");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn finds_trashed_item_with_suffix() {
        let base = std::env::temp_dir().join(format!("pairdeck-trashfind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let when = SystemTime::now();
        std::fs::write(base.join("report.pdf"), "old").unwrap();
        std::fs::write(base.join("report 2.pdf"), "new").unwrap();
        std::fs::write(base.join("other.pdf"), "x").unwrap();
        let orig = Path::new("/somewhere/report.pdf");
        assert_eq!(find_in_trash(&base, orig, when).unwrap(), base.join("report.pdf"), "exact name wins");
        std::fs::remove_file(base.join("report.pdf")).unwrap();
        assert_eq!(find_in_trash(&base, orig, when).unwrap(), base.join("report 2.pdf"));
        assert!(find_in_trash(&base, Path::new("/x/missing.txt"), when).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}
