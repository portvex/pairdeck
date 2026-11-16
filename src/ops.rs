//! Background file operations (copy, move, delete, dir size) with progress and
//! interactive conflict / error resolution over channels.

use crate::app::AppEvent;
use crate::vfs::{Entry, EntryKind, Location, VfsHandle};
use anyhow::{Result, anyhow, bail};
use crossbeam_channel::{Sender, bounded};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct JobProgress {
    pub id: u64,
    pub title: String,
    pub total_bytes: AtomicU64,
    pub done_bytes: AtomicU64,
    pub total_files: AtomicU64,
    pub done_files: AtomicU64,
    pub file_size: AtomicU64,
    pub file_done: AtomicU64,
    pub current: Mutex<String>,
    pub target: Mutex<String>,
    pub cancel: AtomicBool,
    pub paused: AtomicBool,
    pub finished: AtomicBool,
    pub scanning: AtomicBool,
    pub errors: Mutex<Vec<String>>,
    pub started: Instant,
}

impl JobProgress {
    pub fn new(title: String) -> Arc<Self> {
        Arc::new(JobProgress {
            id: NEXT_ID.fetch_add(1, Relaxed),
            title,
            total_bytes: AtomicU64::new(0),
            done_bytes: AtomicU64::new(0),
            total_files: AtomicU64::new(0),
            done_files: AtomicU64::new(0),
            file_size: AtomicU64::new(0),
            file_done: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            target: Mutex::new(String::new()),
            cancel: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            scanning: AtomicBool::new(true),
            errors: Mutex::new(Vec::new()),
            started: Instant::now(),
        })
    }

    pub fn bytes_per_sec(&self) -> f64 {
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        self.done_bytes.load(Relaxed) as f64 / secs
    }

    pub fn eta(&self) -> Option<Duration> {
        let rate = self.bytes_per_sec();
        let left = self.total_bytes.load(Relaxed).saturating_sub(self.done_bytes.load(Relaxed));
        if rate < 1.0 { None } else { Some(Duration::from_secs_f64(left as f64 / rate)) }
    }

    pub fn fraction(&self) -> f64 {
        let t = self.total_bytes.load(Relaxed);
        if t == 0 {
            let tf = self.total_files.load(Relaxed);
            if tf == 0 { 0.0 } else { self.done_files.load(Relaxed) as f64 / tf as f64 }
        } else {
            self.done_bytes.load(Relaxed) as f64 / t as f64
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overwrite {
    Ask,
    Always,
    Never,
    Older,
    SizeDiffers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictChoice {
    Overwrite,
    OverwriteAll,
    Skip,
    SkipAll,
    OlderAll,
    Rename,
    Abort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorChoice {
    Skip,
    SkipAll,
    Retry,
    Abort,
}

#[allow(dead_code)]
pub enum JobMsg {
    Conflict {
        id: u64,
        src: Box<Entry>,
        dst: Box<Entry>,
        dst_path: String,
        reply: Sender<ConflictChoice>,
    },
    Error {
        id: u64,
        msg: String,
        reply: Sender<ErrorChoice>,
    },
    /// `undo` lists what the job changed, for the undo journal.
    Finished {
        id: u64,
        summary: String,
        cancelled: bool,
        errors: Vec<String>,
        undo: Vec<crate::undo::UndoOp>,
    },
    DirSize {
        id: u64,
        dir: PathBuf,
        name: String,
        size: u64,
    },
}

#[derive(Clone, Debug)]
pub struct CopyOpts {
    pub overwrite: Overwrite,
    pub preserve: bool,
    pub follow_links: bool,
    /// Copy into an existing directory as a subdirectory (MC "dive into subdir").
    pub dive_into: bool,
}

impl Default for CopyOpts {
    fn default() -> Self {
        CopyOpts { overwrite: Overwrite::Ask, preserve: true, follow_links: false, dive_into: true }
    }
}

pub enum JobSpec {
    Copy {
        src: Location,
        names: Vec<String>,
        dst: Location,
        target: Option<String>,
        opts: CopyOpts,
        remove_src: bool,
    },
    Delete {
        loc: Location,
        names: Vec<String>,
        trash: bool,
    },
    DirSize {
        loc: Location,
        names: Vec<String>,
    },
    /// Execute a directory synchronization plan between two locations (see sync.rs).
    Sync {
        left: Location,
        right: Location,
        steps: Vec<SyncStep>,
        trash: bool,
        follow_links: bool,
    },
}

/// One step of a sync plan. `rel` is relative to the left or right root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncStep {
    /// Create a directory on one side (parents are created first by plan order).
    Mkdir { to_left: bool, rel: String },
    /// Copy a file (or symlink) to the other side. `existed` = overwriting.
    Copy { from_left: bool, rel: String, size: u64, existed: bool },
    /// Delete a file, or a whole directory tree when `tree` is set (trash when local).
    Delete { on_left: bool, rel: String, size: u64, tree: bool },
    /// Remove a directory if it is empty; failures are ignored.
    Rmdir { on_left: bool, rel: String },
}

impl JobSpec {
    fn title(&self) -> String {
        match self {
            JobSpec::Copy { names, remove_src, .. } => {
                let verb = if *remove_src { "Move" } else { "Copy" };
                if names.len() == 1 {
                    format!("{verb} \"{}\"", names[0])
                } else {
                    format!("{verb} {} items", names.len())
                }
            }
            JobSpec::Delete { names, trash, .. } => {
                let verb = if *trash { "Trash" } else { "Delete" };
                if names.len() == 1 {
                    format!("{verb} \"{}\"", names[0])
                } else {
                    format!("{verb} {} items", names.len())
                }
            }
            JobSpec::DirSize { .. } => "Directory size".into(),
            JobSpec::Sync { steps, .. } => format!("Synchronize {} items", steps.len()),
        }
    }
}

pub fn spawn(spec: JobSpec, tx: Sender<AppEvent>) -> Arc<JobProgress> {
    let progress = JobProgress::new(spec.title());
    let p = progress.clone();
    std::thread::spawn(move || {
        let mut w = Worker {
            p: p.clone(),
            tx: tx.clone(),
            overwrite: Overwrite::Ask,
            skip_errors: false,
            renames: 0,
            undo: Vec::new(),
            trashed: Vec::new(),
        };
        let res = w.run(spec);
        p.finished.store(true, Relaxed);
        let cancelled = p.cancel.load(Relaxed) || res.is_err();
        if let Err(e) = res {
            let msg = e.to_string();
            if msg != "aborted" {
                p.errors.lock().unwrap().push(msg);
            }
        }
        let errors = p.errors.lock().unwrap().clone();
        let summary = format!(
            "{}: {} file(s), {}",
            p.title,
            p.done_files.load(Relaxed),
            crate::util::human_size(p.done_bytes.load(Relaxed))
        );
        let undo = std::mem::take(&mut w.undo);
        let _ = tx.send(AppEvent::Job(JobMsg::Finished { id: p.id, summary, cancelled, errors, undo }));
    });
    progress
}

struct Worker {
    p: Arc<JobProgress>,
    tx: Sender<AppEvent>,
    overwrite: Overwrite,
    skip_errors: bool,
    renames: usize,
    /// Reversible changes made so far (top-level items only).
    undo: Vec<crate::undo::UndoOp>,
    /// Paths moved to the trash by this job.
    trashed: Vec<PathBuf>,
}

const BUF: usize = 1 << 20;

impl Worker {
    fn check(&self) -> Result<()> {
        while self.p.paused.load(Relaxed) && !self.p.cancel.load(Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.p.cancel.load(Relaxed) {
            bail!("aborted");
        }
        Ok(())
    }

    fn set_current(&self, s: &str, t: &str) {
        *self.p.current.lock().unwrap() = s.to_string();
        *self.p.target.lock().unwrap() = t.to_string();
    }

    /// Run `f`, asking the user what to do on failure. Ok(None) means skipped.
    fn attempt<T>(&mut self, what: &str, mut f: impl FnMut(&mut Self) -> Result<T>) -> Result<Option<T>> {
        loop {
            self.check()?;
            match f(self) {
                Ok(v) => return Ok(Some(v)),
                Err(e) if e.to_string() == "aborted" => return Err(e),
                Err(e) => {
                    let msg = format!("{what}\n{e:#}");
                    if self.skip_errors {
                        self.p.errors.lock().unwrap().push(msg);
                        return Ok(None);
                    }
                    let (rtx, rrx) = bounded(1);
                    let _ = self.tx.send(AppEvent::Job(JobMsg::Error { id: self.p.id, msg: msg.clone(), reply: rtx }));
                    match rrx.recv().unwrap_or(ErrorChoice::Abort) {
                        ErrorChoice::Retry => continue,
                        ErrorChoice::Skip => {
                            self.p.errors.lock().unwrap().push(msg);
                            return Ok(None);
                        }
                        ErrorChoice::SkipAll => {
                            self.skip_errors = true;
                            self.p.errors.lock().unwrap().push(msg);
                            return Ok(None);
                        }
                        ErrorChoice::Abort => {
                            self.p.cancel.store(true, Relaxed);
                            bail!("aborted");
                        }
                    }
                }
            }
        }
    }

    fn run(&mut self, spec: JobSpec) -> Result<()> {
        match spec {
            JobSpec::Copy { src, names, dst, target, opts, remove_src } => {
                self.overwrite = opts.overwrite;
                let res = self.copy_job(&src, &names, &dst, target.as_deref(), &opts, remove_src);
                // Archives buffer writes; persist them even after a partial copy.
                let committed = dst.vfs.lock().commit();
                let src_committed =
                    if remove_src && !src.vfs.same(&dst.vfs) { src.vfs.lock().commit() } else { Ok(()) };
                res.and(committed).and(src_committed)
            }
            JobSpec::Delete { loc, names, trash } => {
                let res = self.delete_job(&loc, &names, trash);
                res.and(loc.vfs.lock().commit())
            }
            JobSpec::Sync { left, right, steps, trash, follow_links } => {
                let res = self.sync_job(&left, &right, &steps, trash, follow_links);
                let lc = left.vfs.lock().commit();
                let rc = if right.vfs.same(&left.vfs) { Ok(()) } else { right.vfs.lock().commit() };
                res.and(lc).and(rc)
            }
            JobSpec::DirSize { loc, names } => {
                self.p.scanning.store(false, Relaxed);
                for n in names {
                    self.check()?;
                    let path = loc.join(&n);
                    let mut size = 0;
                    let mut files = 0;
                    scan(&loc.vfs, &path, true, &mut size, &mut files, &self.p.cancel);
                    let _ = self.tx.send(AppEvent::Job(JobMsg::DirSize {
                        id: self.p.id,
                        dir: loc.path.clone(),
                        name: n,
                        size,
                    }));
                }
                Ok(())
            }
        }
    }

    // ---------------------------------------------------------------- copy / move

    fn copy_job(
        &mut self,
        src: &Location,
        names: &[String],
        dst: &Location,
        target: Option<&str>,
        opts: &CopyOpts,
        remove_src: bool,
    ) -> Result<()> {
        // Scan for totals.
        let mut bytes = 0;
        let mut files = 0;
        for n in names {
            scan(&src.vfs, &src.join(n), opts.follow_links, &mut bytes, &mut files, &self.p.cancel);
        }
        self.p.total_bytes.store(bytes, Relaxed);
        self.p.total_files.store(files, Relaxed);
        self.p.scanning.store(false, Relaxed);

        let dst_is_dir = dst.vfs.lock().stat(&dst.path).map(|e| e.is_dir()).unwrap_or(false);
        for name in names {
            self.check()?;
            let sp = src.join(name);
            let base =
                Path::new(name).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| name.clone());
            let dp = match target {
                Some(t) if t.contains('*') => dst.path.join(apply_mask(&base, t)),
                Some(t) if names.len() == 1 && !(dst_is_dir && t.is_empty()) => {
                    let tp = dst.path.join(t);
                    let exists_dir = dst.vfs.lock().stat(&tp).map(|e| e.is_dir()).unwrap_or(false);
                    if exists_dir && opts.dive_into { tp.join(&base) } else { tp }
                }
                _ => dst.path.join(&base),
            };
            let Some(se) = self.attempt(&format!("Cannot stat {}", sp.display()), |_| src.vfs.lock().stat(&sp))? else {
                continue;
            };
            if src.vfs.same(&dst.vfs) {
                if crate::vfs::normalize(&sp) == crate::vfs::normalize(&dp) {
                    self.p.errors.lock().unwrap().push(format!("\"{}\": cannot copy onto itself", sp.display()));
                    continue;
                }
                if se.is_dir() && crate::vfs::normalize(&dp).starts_with(crate::vfs::normalize(&sp)) {
                    self.p.errors.lock().unwrap().push(format!("Cannot copy \"{}\" into itself", sp.display()));
                    continue;
                }
            }
            // Only items that did not exist at the target can be undone safely.
            let existed = dst.vfs.lock().stat(&dp).is_ok();
            let record = |w: &mut Self| {
                if existed {
                    return;
                }
                w.undo.push(if remove_src {
                    crate::undo::UndoOp::Move {
                        src: src.vfs.clone(),
                        from: sp.clone(),
                        dst: dst.vfs.clone(),
                        to: dp.clone(),
                    }
                } else {
                    crate::undo::UndoOp::Copy { vfs: dst.vfs.clone(), paths: vec![dp.clone()] }
                });
            };
            // Fast path: rename within the same file system.
            if (remove_src && src.vfs.same(&dst.vfs) || remove_src && src.is_local() && dst.is_local())
                && self.try_rename(src, &sp, &se, dst, &dp)?
            {
                record(self);
                continue;
            }
            if self.copy_any(&src.vfs, &sp, &se, &dst.vfs, &dp, opts, remove_src)? {
                record(self);
            }
        }
        Ok(())
    }

    fn try_rename(&mut self, src: &Location, sp: &Path, se: &Entry, dst: &Location, dp: &Path) -> Result<bool> {
        let exists = dst.vfs.lock().stat(dp).ok();
        if let Some(de) = &exists {
            // Let the slow path handle merges and overwrite decisions.
            if de.is_dir() || self.overwrite != Overwrite::Always {
                return Ok(false);
            }
        }
        self.set_current(&sp.display().to_string(), &dp.display().to_string());
        if src.vfs.lock().rename(sp, dp).is_ok() {
            let mut b = 0;
            let mut f = 0;
            if se.is_dir() {
                scan(&dst.vfs, dp, false, &mut b, &mut f, &self.p.cancel);
            } else {
                b = se.size;
                f = 1;
            }
            self.p.done_bytes.fetch_add(b, Relaxed);
            self.p.done_files.fetch_add(f, Relaxed);
            return Ok(true);
        }
        Ok(false)
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_any(
        &mut self,
        sv: &VfsHandle,
        sp: &Path,
        se: &Entry,
        dv: &VfsHandle,
        dp: &Path,
        opts: &CopyOpts,
        remove_src: bool,
    ) -> Result<bool> {
        self.check()?;
        let is_link = se.is_symlink();
        if is_link && !opts.follow_links {
            let target = se.link_target().unwrap_or_default().to_string();
            let done = self.attempt(&format!("Cannot create symlink {}", dp.display()), |w| {
                let found = dv.lock().stat(dp);
                if let Ok(existing) = found {
                    if !w.resolve_conflict(se, &existing, dp)? {
                        return Ok(false);
                    }
                    let _ = dv.lock().remove_file(dp);
                }
                dv.lock().symlink(&target, dp)?;
                Ok(true)
            })?;
            match done {
                Some(true) => {
                    self.p.done_files.fetch_add(1, Relaxed);
                    if remove_src {
                        let _ = sv.lock().remove_file(sp);
                    }
                    return Ok(true);
                }
                Some(false) => return Ok(false),
                None => {
                    // Symlinks unsupported on the target: copy the content instead.
                    if !se.is_dir() {
                        return self.copy_file_checked(sv, sp, se, dv, dp, opts, remove_src);
                    }
                    return Ok(false);
                }
            }
        }
        if se.is_dir() {
            let existing = dv.lock().stat(dp).ok();
            match existing {
                Some(e) if e.is_dir() => {}
                Some(_) => {
                    self.p.errors.lock().unwrap().push(format!("{}: exists and is not a directory", dp.display()));
                    return Ok(false);
                }
                None => {
                    let mode = se.mode & 0o7777;
                    if self
                        .attempt(&format!("Cannot create directory {}", dp.display()), |_| {
                            dv.lock().mkdir(dp, mode | 0o700)
                        })?
                        .is_none()
                    {
                        return Ok(false);
                    }
                }
            }
            let Some(children) = self.attempt(&format!("Cannot read {}", sp.display()), |_| sv.lock().list(sp))? else {
                return Ok(false);
            };
            let mut all_ok = true;
            for c in children {
                let ok = self.copy_any(sv, &sp.join(&c.name), &c, dv, &dp.join(&c.name), opts, remove_src)?;
                all_ok &= ok;
            }
            if opts.preserve {
                let _ = dv.lock().set_mode(dp, se.mode & 0o7777);
                if let Some(t) = se.mtime {
                    let _ = dv.lock().set_mtime(dp, t);
                }
            }
            if remove_src && all_ok {
                self.attempt(&format!("Cannot remove directory {}", sp.display()), |_| sv.lock().remove_dir(sp))?;
            }
            return Ok(all_ok);
        }
        if matches!(se.kind, EntryKind::Other) {
            self.p.errors.lock().unwrap().push(format!("{}: special file skipped", sp.display()));
            return Ok(false);
        }
        self.copy_file_checked(sv, sp, se, dv, dp, opts, remove_src)
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_file_checked(
        &mut self,
        sv: &VfsHandle,
        sp: &Path,
        se: &Entry,
        dv: &VfsHandle,
        dp: &Path,
        opts: &CopyOpts,
        remove_src: bool,
    ) -> Result<bool> {
        let mut dp = dp.to_path_buf();
        let found = dv.lock().stat(&dp);
        if let Ok(existing) = found {
            match self.conflict(se, &existing, &dp)? {
                ConflictOutcome::Write => {}
                ConflictOutcome::Skip => {
                    self.p.done_bytes.fetch_add(se.size, Relaxed);
                    self.p.done_files.fetch_add(1, Relaxed);
                    return Ok(false);
                }
                ConflictOutcome::Rename => {
                    dp = self.unique_name(dv, &dp);
                }
            }
        }
        let before = self.p.done_bytes.load(Relaxed);
        let res = self.attempt(&format!("Cannot copy {} to {}", sp.display(), dp.display()), |w| {
            w.p.done_bytes.store(before, Relaxed);
            w.copy_file(sv, sp, se, dv, &dp, opts)
        })?;
        if res.is_none() {
            self.p.done_bytes.store(before + se.size, Relaxed);
            return Ok(false);
        }
        self.p.done_files.fetch_add(1, Relaxed);
        if remove_src {
            self.attempt(&format!("Cannot remove {}", sp.display()), |_| sv.lock().remove_file(sp))?;
        }
        Ok(true)
    }

    fn unique_name(&mut self, dv: &VfsHandle, dp: &Path) -> PathBuf {
        let stem = dp.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = dp.extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
        for i in 1.. {
            let cand = dp.with_file_name(format!("{stem} ({i}){ext}"));
            if dv.lock().stat(&cand).is_err() {
                self.renames += 1;
                return cand;
            }
        }
        unreachable!()
    }

    fn copy_file(
        &mut self,
        sv: &VfsHandle,
        sp: &Path,
        se: &Entry,
        dv: &VfsHandle,
        dp: &Path,
        opts: &CopyOpts,
    ) -> Result<()> {
        self.set_current(&sp.display().to_string(), &dp.display().to_string());
        self.p.file_size.store(se.size, Relaxed);
        self.p.file_done.store(0, Relaxed);
        let mut r: Box<dyn Read + Send> = sv.lock().open_read(sp)?;
        let mut wr: Box<dyn Write + Send> = dv.lock().create_write(dp, se.mode & 0o7777)?;
        let mut buf = vec![0u8; BUF];
        loop {
            if let Err(e) = self.check() {
                drop(wr);
                let _ = dv.lock().remove_file(dp);
                return Err(e);
            }
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            wr.write_all(&buf[..n])?;
            self.p.done_bytes.fetch_add(n as u64, Relaxed);
            self.p.file_done.fetch_add(n as u64, Relaxed);
        }
        wr.flush()?;
        drop(wr);
        if opts.preserve {
            let mut v = dv.lock();
            let _ = v.set_mode(dp, se.mode & 0o7777);
            if let Some(t) = se.mtime {
                let _ = v.set_mtime(dp, t);
            }
        }
        Ok(())
    }

    fn resolve_conflict(&mut self, se: &Entry, de: &Entry, dp: &Path) -> Result<bool> {
        Ok(matches!(self.conflict(se, de, dp)?, ConflictOutcome::Write))
    }

    fn conflict(&mut self, se: &Entry, de: &Entry, dp: &Path) -> Result<ConflictOutcome> {
        let decide = |w: Overwrite| -> Option<ConflictOutcome> {
            match w {
                Overwrite::Always => Some(ConflictOutcome::Write),
                Overwrite::Never => Some(ConflictOutcome::Skip),
                Overwrite::Older => {
                    Some(if de.mtime < se.mtime { ConflictOutcome::Write } else { ConflictOutcome::Skip })
                }
                Overwrite::SizeDiffers => {
                    Some(if de.size != se.size { ConflictOutcome::Write } else { ConflictOutcome::Skip })
                }
                Overwrite::Ask => None,
            }
        };
        if let Some(o) = decide(self.overwrite) {
            return Ok(o);
        }
        let (rtx, rrx) = bounded(1);
        let _ = self.tx.send(AppEvent::Job(JobMsg::Conflict {
            id: self.p.id,
            src: Box::new(se.clone()),
            dst: Box::new(de.clone()),
            dst_path: dp.display().to_string(),
            reply: rtx,
        }));
        let choice = rrx.recv().unwrap_or(ConflictChoice::Abort);
        Ok(match choice {
            ConflictChoice::Overwrite => ConflictOutcome::Write,
            ConflictChoice::OverwriteAll => {
                self.overwrite = Overwrite::Always;
                ConflictOutcome::Write
            }
            ConflictChoice::Skip => ConflictOutcome::Skip,
            ConflictChoice::SkipAll => {
                self.overwrite = Overwrite::Never;
                ConflictOutcome::Skip
            }
            ConflictChoice::OlderAll => {
                self.overwrite = Overwrite::Older;
                decide(Overwrite::Older).unwrap()
            }
            ConflictChoice::Rename => ConflictOutcome::Rename,
            ConflictChoice::Abort => {
                self.p.cancel.store(true, Relaxed);
                bail!("aborted")
            }
        })
    }

    // ---------------------------------------------------------------- sync

    fn sync_job(
        &mut self,
        left: &Location,
        right: &Location,
        steps: &[SyncStep],
        trash: bool,
        follow: bool,
    ) -> Result<()> {
        let bytes: u64 = steps.iter().map(|s| if let SyncStep::Copy { size, .. } = s { *size } else { 0 }).sum();
        let files = steps.iter().filter(|s| matches!(s, SyncStep::Copy { .. } | SyncStep::Delete { .. })).count();
        self.p.total_bytes.store(bytes, Relaxed);
        self.p.total_files.store(files as u64, Relaxed);
        self.p.scanning.store(false, Relaxed);
        // The plan already decided every overwrite.
        self.overwrite = Overwrite::Always;
        let opts = CopyOpts { overwrite: Overwrite::Always, preserve: true, follow_links: follow, dive_into: false };
        let side = |is_left: bool| if is_left { left } else { right };
        let mut made: Vec<(bool, PathBuf)> = Vec::new();
        let mut copied: Vec<(bool, PathBuf)> = Vec::new();
        for step in steps {
            self.check()?;
            match step {
                SyncStep::Mkdir { to_left, rel } => {
                    let loc = side(*to_left);
                    let p = loc.join(rel);
                    if loc.vfs.lock().stat(&p).is_ok() {
                        continue;
                    }
                    self.set_current(&p.display().to_string(), "");
                    if self
                        .attempt(&format!("Cannot create directory {}", p.display()), |_| {
                            loc.vfs.lock().mkdir(&p, 0o755)
                        })?
                        .is_some()
                    {
                        made.push((*to_left, p));
                    }
                }
                SyncStep::Copy { from_left, rel, existed, .. } => {
                    let (src, dst) = (side(*from_left), side(!*from_left));
                    let (sp, dp) = (src.join(rel), dst.join(rel));
                    let Some(se) =
                        self.attempt(&format!("Cannot stat {}", sp.display()), |_| src.vfs.lock().stat(&sp))?
                    else {
                        continue;
                    };
                    let ok = self.copy_any(&src.vfs, &sp, &se, &dst.vfs, &dp, &opts, false)?;
                    if ok && !existed {
                        copied.push((!*from_left, dp));
                    }
                }
                SyncStep::Delete { on_left, rel, size, tree } => {
                    let loc = side(*on_left);
                    let p = loc.join(rel);
                    self.set_current(&p.display().to_string(), if trash && loc.is_local() { "Trash" } else { "" });
                    if trash && loc.is_local() {
                        if self
                            .attempt(&format!("Cannot move {} to trash", p.display()), |_| {
                                trash::delete(&p).map_err(|e| anyhow!("{e}"))
                            })?
                            .is_some()
                        {
                            self.p.done_files.fetch_add(1, Relaxed);
                            self.trashed.push(p);
                        }
                        continue;
                    }
                    let Some(e) = self.attempt(&format!("Cannot stat {}", p.display()), |_| loc.vfs.lock().stat(&p))?
                    else {
                        continue;
                    };
                    if e.is_dir() && !e.is_symlink() && !tree {
                        continue;
                    }
                    self.delete_any(&loc.vfs, &p, &e)?;
                    let _ = size;
                }
                SyncStep::Rmdir { on_left, rel } => {
                    let loc = side(*on_left);
                    let _ = loc.vfs.lock().remove_dir(&loc.join(rel));
                }
            }
        }
        // Undo: new copies, created directories (removed if empty), trashed items.
        for is_left in [true, false] {
            let vfs = side(is_left).vfs.clone();
            let paths: Vec<PathBuf> = copied.iter().filter(|(l, _)| *l == is_left).map(|(_, p)| p.clone()).collect();
            if !paths.is_empty() {
                self.undo.push(crate::undo::UndoOp::Copy { vfs: vfs.clone(), paths });
            }
            let dirs: Vec<PathBuf> = made.iter().filter(|(l, _)| *l == is_left).map(|(_, p)| p.clone()).collect();
            if !dirs.is_empty() {
                self.undo.push(crate::undo::UndoOp::Mkdir { vfs, paths: dirs });
            }
        }
        if !self.trashed.is_empty() {
            let items = std::mem::take(&mut self.trashed);
            self.undo.push(crate::undo::UndoOp::Trash { items, when: std::time::SystemTime::now() });
        }
        Ok(())
    }

    // ---------------------------------------------------------------- delete

    fn delete_job(&mut self, loc: &Location, names: &[String], trash: bool) -> Result<()> {
        if trash && loc.is_local() {
            self.p.scanning.store(false, Relaxed);
            self.p.total_files.store(names.len() as u64, Relaxed);
            for n in names {
                self.check()?;
                let p = loc.join(n);
                self.set_current(&p.display().to_string(), "Trash");
                if self
                    .attempt(&format!("Cannot move {} to trash", p.display()), |_| {
                        trash::delete(&p).map_err(|e| anyhow!("{e}"))
                    })?
                    .is_some()
                {
                    self.p.done_files.fetch_add(1, Relaxed);
                    self.trashed.push(p.clone());
                }
            }
            if !self.trashed.is_empty() {
                let items = std::mem::take(&mut self.trashed);
                self.undo.push(crate::undo::UndoOp::Trash { items, when: std::time::SystemTime::now() });
            }
            return Ok(());
        }
        let mut bytes = 0;
        let mut files = 0;
        for n in names {
            scan(&loc.vfs, &loc.join(n), false, &mut bytes, &mut files, &self.p.cancel);
        }
        self.p.total_files.store(files, Relaxed);
        self.p.scanning.store(false, Relaxed);
        for n in names {
            self.check()?;
            let p = loc.join(n);
            let Some(e) = self.attempt(&format!("Cannot stat {}", p.display()), |_| loc.vfs.lock().stat(&p))? else {
                continue;
            };
            self.delete_any(&loc.vfs, &p, &e)?;
        }
        Ok(())
    }

    fn delete_any(&mut self, v: &VfsHandle, p: &Path, e: &Entry) -> Result<()> {
        self.check()?;
        if matches!(e.kind, EntryKind::Dir) {
            if let Some(children) = self.attempt(&format!("Cannot read {}", p.display()), |_| v.lock().list(p))? {
                for c in children {
                    self.delete_any(v, &p.join(&c.name), &c)?;
                }
            }
            self.set_current(&p.display().to_string(), "");
            self.attempt(&format!("Cannot remove directory {}", p.display()), |_| v.lock().remove_dir(p))?;
        } else {
            self.set_current(&p.display().to_string(), "");
            if self.attempt(&format!("Cannot delete {}", p.display()), |_| v.lock().remove_file(p))?.is_some() {
                self.p.done_files.fetch_add(1, Relaxed);
                self.p.done_bytes.fetch_add(e.size, Relaxed);
            }
        }
        Ok(())
    }
}

enum ConflictOutcome {
    Write,
    Skip,
    Rename,
}

/// Recursively total sizes and file counts.
pub fn scan(v: &VfsHandle, p: &Path, follow: bool, bytes: &mut u64, files: &mut u64, cancel: &AtomicBool) {
    if cancel.load(Relaxed) {
        return;
    }
    let Ok(e) = v.lock().stat(p) else { return };
    let is_dir = matches!(e.kind, EntryKind::Dir) || (follow && e.is_dir());
    if is_dir {
        let list = v.lock().list(p).unwrap_or_default();
        for c in list {
            if matches!(c.kind, EntryKind::Dir) || (follow && c.is_dir()) {
                scan(v, &p.join(&c.name), follow, bytes, files, cancel);
            } else {
                *bytes += c.size;
                *files += 1;
            }
        }
    } else {
        *bytes += e.size;
        *files += 1;
    }
}

/// MC-style target mask: "*.bak" turns "a.txt" into "a.bak", "x_*" into "x_a.txt".
pub fn apply_mask(name: &str, mask: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    };
    if let Some((left, right)) = mask.split_once('*') {
        if right.starts_with('.') && right.len() > 1 {
            // "*.ext" or "pre*.ext"
            let right = right.replace('*', ext);
            return format!("{left}{stem}{right}");
        }
        return format!("{left}{name}{}", right.replace('*', ""));
    }
    mask.to_string()
}
