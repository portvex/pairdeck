//! Git integration through the `git` command line: per-file status markers in
//! the panels, branch info, diff preview and common commands (stage, unstage,
//! discard, diff, log, blame, commit).
//!
//! Status is collected on background threads with a timeout and cached per
//! repository root; the UI thread only does cheap map lookups.

use crate::app::App;
use crate::dialog::{self, Form};
use crate::keymap::Command;
use crate::theme::Theme;
use anyhow::{Result, anyhow, bail};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Maximum time a `git status` may take before we give up for that refresh.
const STATUS_TIMEOUT: Duration = Duration::from_secs(3);
/// Timeout for interactive commands (stage, log, blame...).
const CMD_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GitStatus {
    Ignored,
    Untracked,
    Staged,
    Added,
    Renamed,
    Deleted,
    Modified,
    Conflict,
}

impl GitStatus {
    /// Higher wins when aggregating into directories.
    fn rank(self) -> u8 {
        match self {
            GitStatus::Ignored => 0,
            GitStatus::Untracked => 1,
            GitStatus::Staged => 2,
            GitStatus::Added => 3,
            GitStatus::Renamed => 4,
            GitStatus::Deleted => 5,
            GitStatus::Modified => 6,
            GitStatus::Conflict => 7,
        }
    }

    /// One-cell marker: Nerd Font glyph or ASCII letter.
    pub fn marker(self, nerd: bool) -> &'static str {
        match (self, nerd) {
            (GitStatus::Modified, true) => "\u{f459}",
            (GitStatus::Staged, true) => "\u{f00c}",
            (GitStatus::Added, true) => "\u{f457}",
            (GitStatus::Untracked, true) => "\u{f128}",
            (GitStatus::Deleted, true) => "\u{f458}",
            (GitStatus::Renamed, true) => "\u{f45a}",
            (GitStatus::Conflict, true) => "\u{f071}",
            (GitStatus::Ignored, true) => "\u{f474}",
            (GitStatus::Modified, false) => "M",
            (GitStatus::Staged, false) => "S",
            (GitStatus::Added, false) => "A",
            (GitStatus::Untracked, false) => "?",
            (GitStatus::Deleted, false) => "D",
            (GitStatus::Renamed, false) => "R",
            (GitStatus::Conflict, false) => "U",
            (GitStatus::Ignored, false) => "!",
        }
    }

    pub fn style(self, t: &Theme) -> Style {
        match self {
            GitStatus::Modified => t.git_modified,
            GitStatus::Staged => t.git_staged,
            GitStatus::Added => t.git_added,
            GitStatus::Untracked => t.git_untracked,
            GitStatus::Deleted => t.git_deleted,
            GitStatus::Renamed => t.git_renamed,
            GitStatus::Conflict => t.git_conflict,
            GitStatus::Ignored => t.git_ignored,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            GitStatus::Modified => "modified",
            GitStatus::Staged => "staged",
            GitStatus::Added => "added",
            GitStatus::Untracked => "untracked",
            GitStatus::Deleted => "deleted",
            GitStatus::Renamed => "renamed",
            GitStatus::Conflict => "conflict",
            GitStatus::Ignored => "ignored",
        }
    }
}

fn better(a: Option<GitStatus>, b: GitStatus) -> GitStatus {
    match a {
        Some(a) if a.rank() >= b.rank() => a,
        _ => b,
    }
}

/// Parsed `git status --porcelain=v2 --branch -z` output for one repository.
#[derive(Clone, Debug, Default)]
pub struct RepoStatus {
    pub root: PathBuf,
    pub branch: Option<String>,
    /// Commit id of HEAD (None for a repository without commits).
    pub oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Status of individual files, paths relative to the root.
    pub files: HashMap<PathBuf, GitStatus>,
    /// Aggregated status of directories (relative to the root).
    pub dirs: HashMap<PathBuf, GitStatus>,
    /// Untracked / ignored directories reported as a whole (everything below inherits).
    pub untracked_dirs: Vec<PathBuf>,
    pub ignored_dirs: Vec<PathBuf>,
    /// Any change other than ignored files.
    pub dirty: bool,
}

fn os_path(b: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(b))
}

/// Map porcelain v2 XY codes of an ordinary or renamed entry to one status.
fn xy_status(xy: &[u8], renamed: bool) -> GitStatus {
    let (x, y) = (xy.first().copied().unwrap_or(b'.'), xy.get(1).copied().unwrap_or(b'.'));
    if x == b'D' || y == b'D' {
        GitStatus::Deleted
    } else if y == b'M' || y == b'T' {
        GitStatus::Modified
    } else if renamed || x == b'R' || x == b'C' {
        GitStatus::Renamed
    } else if x == b'A' {
        GitStatus::Added
    } else if x != b'.' {
        GitStatus::Staged
    } else {
        GitStatus::Modified
    }
}

/// Skip `n` space-separated fields and return the rest (the path, which may contain spaces).
fn after_fields(rec: &[u8], n: usize) -> Option<&[u8]> {
    let mut seen = 0;
    for (i, b) in rec.iter().enumerate() {
        if *b == b' ' {
            seen += 1;
            if seen == n {
                return Some(&rec[i + 1..]);
            }
        }
    }
    None
}

impl RepoStatus {
    /// Parse NUL-separated porcelain v2 output (paths relative to `root`).
    pub fn parse(root: &Path, data: &[u8]) -> RepoStatus {
        let mut rs = RepoStatus { root: root.to_path_buf(), ..Default::default() };
        let mut recs = data.split(|b| *b == 0).filter(|r| !r.is_empty());
        while let Some(rec) = recs.next() {
            match rec.first() {
                Some(b'#') => {
                    let text = String::from_utf8_lossy(rec);
                    let mut it = text.split_whitespace().skip(1);
                    match it.next() {
                        Some("branch.oid") => {
                            rs.oid = it.next().filter(|s| *s != "(initial)").map(String::from);
                        }
                        Some("branch.head") => {
                            rs.branch = it.next().filter(|s| *s != "(detached)").map(String::from);
                        }
                        Some("branch.upstream") => rs.upstream = it.next().map(String::from),
                        Some("branch.ab") => {
                            for v in it {
                                if let Some(a) = v.strip_prefix('+') {
                                    rs.ahead = a.parse().unwrap_or(0);
                                } else if let Some(b) = v.strip_prefix('-') {
                                    rs.behind = b.parse().unwrap_or(0);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Some(b'1') => {
                    if let Some(p) = after_fields(rec, 8) {
                        rs.add_file(os_path(p), xy_status(&rec[2..4.min(rec.len())], false));
                    }
                }
                Some(b'2') => {
                    if let Some(p) = after_fields(rec, 9) {
                        rs.add_file(os_path(p), xy_status(&rec[2..4.min(rec.len())], true));
                    }
                    // The original path follows as its own NUL-terminated record.
                    let _orig = recs.next();
                }
                Some(b'u') => {
                    if let Some(p) = after_fields(rec, 10) {
                        rs.add_file(os_path(p), GitStatus::Conflict);
                    }
                }
                Some(b'?') => rs.add_untracked_or_ignored(&rec[2.min(rec.len())..], GitStatus::Untracked),
                Some(b'!') => rs.add_untracked_or_ignored(&rec[2.min(rec.len())..], GitStatus::Ignored),
                _ => {}
            }
        }
        rs
    }

    fn add_untracked_or_ignored(&mut self, p: &[u8], st: GitStatus) {
        if let Some(dir) = p.strip_suffix(b"/") {
            let d = os_path(dir);
            if st == GitStatus::Untracked {
                self.untracked_dirs.push(d.clone());
                self.mark_dir_and_ancestors(&d, st);
                self.dirty = true;
            } else {
                self.ignored_dirs.push(d.clone());
                self.dirs.entry(d).or_insert(GitStatus::Ignored);
            }
        } else if st == GitStatus::Ignored {
            self.files.insert(os_path(p), st);
        } else {
            self.add_file(os_path(p), st);
        }
    }

    fn add_file(&mut self, rel: PathBuf, st: GitStatus) {
        self.dirty = true;
        if let Some(parent) = rel.parent() {
            self.mark_dir_and_ancestors(parent, st);
        }
        let prev = self.files.get(&rel).copied();
        self.files.insert(rel, better(prev, st));
    }

    fn mark_dir_and_ancestors(&mut self, dir: &Path, st: GitStatus) {
        for anc in dir.ancestors() {
            if anc.as_os_str().is_empty() {
                break;
            }
            let prev = self.dirs.get(anc).copied().filter(|s| *s != GitStatus::Ignored);
            self.dirs.insert(anc.to_path_buf(), better(prev, st));
        }
    }

    /// Status of `name` inside directory `dir` (absolute paths).
    pub fn status_of(&self, dir: &Path, name: &str, is_dir: bool) -> Option<GitStatus> {
        if name == ".." {
            return None;
        }
        let abs = dir.join(name);
        let rel = abs.strip_prefix(&self.root).ok()?;
        if rel.as_os_str().is_empty() || rel == Path::new(".git") || rel.starts_with(".git") {
            return None;
        }
        let own = if is_dir { self.dirs.get(rel) } else { self.files.get(rel) };
        if let Some(s) = own {
            return Some(*s);
        }
        // Inside an untracked or ignored directory?
        if self.untracked_dirs.iter().any(|d| rel.starts_with(d)) {
            return Some(GitStatus::Untracked);
        }
        if self.ignored_dirs.iter().any(|d| rel.starts_with(d)) {
            return Some(GitStatus::Ignored);
        }
        None
    }

    /// Short summary for the panel border, e.g. " main +2 -1 * ".
    pub fn summary(&self) -> String {
        let mut s = String::from(" ");
        if let Some(b) = &self.branch {
            s.push_str(b);
        } else if let Some(o) = &self.oid {
            s.push_str(&format!("({})", &o[..o.len().min(8)]));
        } else {
            s.push_str("(no commits)");
        }
        if self.ahead > 0 {
            s.push_str(&format!(" +{}", self.ahead));
        }
        if self.behind > 0 {
            s.push_str(&format!(" -{}", self.behind));
        }
        if self.dirty {
            s.push_str(" *");
        }
        s.push(' ');
        s
    }
}

// ---------------------------------------------------------------- running git

/// Find the repository root of `dir` by looking for `.git` (dir or file) upward.
pub fn find_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|a| a.join(".git").exists()).map(Path::to_path_buf)
}

/// Run git with a timeout; returns stdout or an error with stderr.
pub fn run_git(dir: &Path, args: &[&OsStr], timeout: Duration) -> Result<Vec<u8>> {
    let mut child = Proc::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("cannot run git: {e}"))?;
    let mut out = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let mut err = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
    // Read both pipes on threads so a full pipe never blocks the child.
    let t_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });
    let t_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = err.read_to_end(&mut v);
        v
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = t_out.join().unwrap_or_default();
    let stderr = t_err.join().unwrap_or_default();
    if !status.success() {
        let msg = String::from_utf8_lossy(&stderr).trim().to_string();
        bail!("{}", if msg.is_empty() { format!("git exited with {status}") } else { msg });
    }
    Ok(stdout)
}

fn args<'a>(v: &'a [&'a str]) -> Vec<&'a OsStr> {
    v.iter().map(OsStr::new).collect()
}

/// Collect the status of the repository at `root` (blocking; call off the UI thread).
pub fn collect_status(root: &Path) -> Result<RepoStatus> {
    let out = run_git(
        root,
        &args(&["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=normal", "--ignored=matching"]),
        STATUS_TIMEOUT,
    )?;
    Ok(RepoStatus::parse(root, &out))
}

/// Per-repository status cache owned by the App.
#[derive(Default)]
pub struct GitState {
    repos: HashMap<PathBuf, Arc<RepoStatus>>,
    inflight: HashSet<PathBuf>,
    again: HashSet<PathBuf>,
}

impl GitState {
    /// Status of the repository containing `dir`, if known.
    pub fn repo_for(&self, dir: &Path) -> Option<&RepoStatus> {
        self.repos
            .iter()
            .filter(|(root, _)| dir.starts_with(root))
            .max_by_key(|(r, _)| r.as_os_str().len())
            .map(|(_, s)| s.as_ref())
    }
}

impl App {
    /// Re-read git status for the repositories shown in both panels (non-blocking).
    pub fn refresh_git(&mut self) {
        if !self.cfg.git_status {
            self.git.repos.clear();
            return;
        }
        let roots: Vec<PathBuf> = self
            .panels
            .iter()
            .filter_map(|p| p.loc.local_path().and_then(find_root))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        // Forget repositories no longer shown.
        self.git.repos.retain(|r, _| roots.contains(r));
        for root in roots {
            self.request_git(root);
        }
    }

    fn request_git(&mut self, root: PathBuf) {
        if self.git.inflight.contains(&root) {
            self.git.again.insert(root);
            return;
        }
        self.git.inflight.insert(root.clone());
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("git-status".into())
            .spawn(move || {
                let res = collect_status(&root);
                crate::app::call(&tx, move |app| {
                    app.git.inflight.remove(&root);
                    match res {
                        Ok(rs) => {
                            app.git.repos.insert(root.clone(), Arc::new(rs));
                        }
                        Err(_) => {
                            // Timeout, git missing or not a repository: show nothing.
                            app.git.repos.remove(&root);
                        }
                    }
                    if app.git.again.remove(&root) {
                        app.request_git(root);
                    }
                });
            })
            .ok();
    }
}

// ---------------------------------------------------------------- preview

/// Colored `git diff HEAD` of a modified tracked file, or None if unchanged,
/// untracked or not in a repository. Blocking; used by the preview worker.
pub fn preview_diff(path: &Path, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let dir = path.parent()?;
    find_root(dir)?;
    let name = path.file_name()?;
    let out = run_git(
        dir,
        &[
            OsStr::new("diff"),
            OsStr::new("HEAD"),
            OsStr::new("--no-color"),
            OsStr::new("--no-ext-diff"),
            OsStr::new("--"),
            name,
        ],
        STATUS_TIMEOUT,
    )
    .ok()?;
    if out.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&out);
    let mut lines = vec![
        Line::styled("git diff HEAD (staged + unstaged). Press d in the preview to show the file.", theme.header),
        Line::raw(""),
    ];
    lines.extend(diff_lines(&text, theme));
    Some(lines)
}

/// Color unified diff text.
pub fn diff_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    text.lines()
        .map(|l| {
            let l = l.replace('\t', "    ");
            let st =
                if l.starts_with("+++") || l.starts_with("---") || l.starts_with("diff ") || l.starts_with("index ") {
                    theme.header
                } else if l.starts_with('+') {
                    theme.diff_add
                } else if l.starts_with('-') {
                    theme.diff_del
                } else if l.starts_with("@@") {
                    theme.git_renamed
                } else {
                    theme.panel
                };
            Line::from(Span::styled(l, st))
        })
        .collect()
}

// ---------------------------------------------------------------- commands

/// Paths of the selection relative to the repository root, plus the root.
fn selection(app: &mut App) -> Option<(PathBuf, Vec<PathBuf>)> {
    let p = app.panel();
    let Some(dir) = p.loc.local_path().map(Path::to_path_buf) else {
        app.flash_error("Git commands work on local directories only");
        return None;
    };
    let Some(root) = find_root(&dir) else {
        app.flash_error("Not inside a git repository");
        return None;
    };
    let rels: Vec<PathBuf> = p
        .selection()
        .iter()
        .filter_map(|e| dir.join(&e.name).strip_prefix(&root).ok().map(Path::to_path_buf))
        .collect();
    Some((root, rels))
}

fn run_on_paths(root: &Path, base: &[&str], paths: &[PathBuf]) -> Result<Vec<u8>> {
    let mut a: Vec<&OsStr> = base.iter().map(OsStr::new).collect();
    a.push(OsStr::new("--"));
    a.extend(paths.iter().map(|p| p.as_os_str()));
    run_git(root, &a, CMD_TIMEOUT)
}

fn after_change(app: &mut App, msg: String) {
    app.info(msg);
    app.reload_panels();
    app.refresh_git();
    app.previewer.invalidate();
}

/// Execute one of the git commands.
pub fn exec(app: &mut App, c: Command) {
    match c {
        Command::GitStatus => status_summary(app),
        Command::GitCommit => commit_dialog(app),
        Command::GitLog => log(app),
        _ => {
            let Some((root, paths)) = selection(app) else { return };
            if paths.is_empty() {
                app.flash_error("Nothing selected");
                return;
            }
            match c {
                Command::GitStage => match run_on_paths(&root, &["add"], &paths) {
                    Ok(_) => after_change(app, format!("Staged {} item(s)", paths.len())),
                    Err(e) => app.error(format!("git add failed:\n{e:#}")),
                },
                Command::GitUnstage => {
                    let res = run_on_paths(&root, &["restore", "--staged"], &paths)
                        .or_else(|_| run_on_paths(&root, &["reset", "-q", "HEAD"], &paths));
                    match res {
                        Ok(_) => after_change(app, format!("Unstaged {} item(s)", paths.len())),
                        Err(e) => app.error(format!("git restore --staged failed:\n{e:#}")),
                    }
                }
                Command::GitDiscard => {
                    let what = if paths.len() == 1 {
                        format!("\"{}\"", paths[0].display())
                    } else {
                        format!("{} items", paths.len())
                    };
                    let text = format!(
                        "Discard all changes to {what}?\nStaged and unstaged changes are reset to HEAD.\nUntracked files are not touched. This cannot be undone."
                    );
                    app.push_dialog(dialog::confirm("Discard changes", &text, move |app| {
                        match run_on_paths(&root, &["restore", "--source=HEAD", "--staged", "--worktree"], &paths) {
                            Ok(_) => after_change(app, format!("Discarded changes in {} item(s)", paths.len())),
                            Err(e) => app.error(format!("git restore failed:\n{e:#}")),
                        }
                    }));
                }
                Command::GitDiff => diff_vs_head(app, &root, &paths[0]),
                Command::GitBlame => blame(app, &root, &paths[0]),
                _ => {}
            }
        }
    }
}

fn diff_vs_head(app: &mut App, root: &Path, rel: &Path) {
    let abs = root.join(rel);
    if abs.is_dir() {
        // Directories: show the textual diff in the viewer instead.
        match run_on_paths(root, &["diff", "HEAD", "--no-color", "--no-ext-diff"], &[rel.to_path_buf()]) {
            Ok(out) if out.is_empty() => app.info("No changes against HEAD"),
            Ok(out) => {
                crate::viewer::open_text(app, &format!("git diff HEAD -- {}", rel.display()), "changes.diff", out)
            }
            Err(e) => app.error(format!("git diff failed:\n{e:#}")),
        }
        return;
    }
    let spec = format!("HEAD:{}", rel.display());
    // A file that is new (not in HEAD) diffs against empty content.
    let head = run_git(root, &[OsStr::new("show"), OsStr::new(&spec)], CMD_TIMEOUT).unwrap_or_default();
    let work = std::fs::read(&abs).unwrap_or_default();
    if crate::util::looks_binary(&head) || crate::util::looks_binary(&work) {
        app.push_dialog(dialog::message("Git diff", "Binary file: cannot show a text diff."));
        return;
    }
    let (a, b) = (String::from_utf8_lossy(&head).into_owned(), String::from_utf8_lossy(&work).into_owned());
    if a == b {
        app.info("No changes against HEAD");
        return;
    }
    let name = rel.display().to_string();
    app.push_screen(Box::new(crate::compare::DiffScreen::new(
        format!("HEAD:{name}"),
        &a,
        format!("{name} (working tree)"),
        &b,
    )));
}

fn log(app: &mut App) {
    let p = app.panel();
    let Some(dir) = p.loc.local_path().map(Path::to_path_buf) else {
        app.flash_error("Git commands work on local directories only");
        return;
    };
    let Some(root) = find_root(&dir) else {
        app.flash_error("Not inside a git repository");
        return;
    };
    // Current file (follow renames) or the whole directory.
    let target = p.current().filter(|e| !e.is_parent()).map(|e| dir.join(&e.name)).unwrap_or_else(|| dir.clone());
    let rel = target.strip_prefix(&root).map(Path::to_path_buf).unwrap_or_default();
    let mut a = args(&["log", "--stat", "--no-color", "--date=iso", "-n", "200"]);
    if target.is_file() {
        a.push(OsStr::new("--follow"));
    }
    a.push(OsStr::new("--"));
    let rel_os = if rel.as_os_str().is_empty() { OsStr::new(".") } else { rel.as_os_str() };
    a.push(rel_os);
    match run_git(&root, &a, CMD_TIMEOUT) {
        Ok(out) if out.is_empty() => app.info("No history"),
        Ok(out) => {
            crate::viewer::open_text(app, &format!("git log -- {}", rel_os.to_string_lossy()), "history.gitlog", out)
        }
        Err(e) => app.error(format!("git log failed:\n{e:#}")),
    }
}

fn blame(app: &mut App, root: &Path, rel: &Path) {
    if root.join(rel).is_dir() {
        app.flash_error("Blame needs a file");
        return;
    }
    match run_on_paths(root, &["blame", "--date=short"], &[rel.to_path_buf()]) {
        Ok(out) => crate::viewer::open_text(app, &format!("git blame {}", rel.display()), "blame.txt", out),
        Err(e) => app.error(format!("git blame failed:\n{e:#}")),
    }
}

fn status_summary(app: &mut App) {
    let Some(dir) = app.panel().loc.local_path().map(Path::to_path_buf) else {
        app.flash_error("Git commands work on local directories only");
        return;
    };
    let Some(root) = find_root(&dir) else {
        app.flash_error("Not inside a git repository");
        return;
    };
    match run_git(&root, &args(&["status", "--no-color"]), CMD_TIMEOUT) {
        Ok(out) => crate::viewer::open_text(app, &format!("git status ({})", root.display()), "status.txt", out),
        Err(e) => app.error(format!("git status failed:\n{e:#}")),
    }
}

fn commit_dialog(app: &mut App) {
    let Some(dir) = app.panel().loc.local_path().map(Path::to_path_buf) else {
        app.flash_error("Git commands work on local directories only");
        return;
    };
    let Some(root) = find_root(&dir) else {
        app.flash_error("Not inside a git repository");
        return;
    };
    let staged = app
        .git
        .repo_for(&dir)
        .map(|r| {
            r.files.values().filter(|s| matches!(s, GitStatus::Staged | GitStatus::Added | GitStatus::Renamed)).count()
        })
        .unwrap_or(0);
    let hist = app.state.input_hist("git_commit").clone();
    let d = Form::new("Git commit")
        .width(70)
        .text(&format!("Repository: {}\nStaged files: {staged}", crate::util::tildify(&root.display().to_string())))
        .input_hist("msg", "Commit message:", "", "git_commit", hist)
        .check("all", "Commit all tracked changes (-a)", staged == 0)
        .check("amend", "Amend the last commit", false)
        .buttons(&["Commit", "Cancel"], 0)
        .on_submit(move |app, r| {
            if r.button != 0 {
                return;
            }
            let msg = r.text("msg");
            if msg.trim().is_empty() && !r.check("amend") {
                app.error("Empty commit message");
                return;
            }
            let mut a: Vec<String> = vec!["commit".into()];
            if r.check("all") {
                a.push("-a".into());
            }
            if r.check("amend") {
                a.push("--amend".into());
                if msg.trim().is_empty() {
                    a.push("--no-edit".into());
                }
            }
            if !msg.trim().is_empty() {
                a.push("-m".into());
                a.push(msg);
            }
            app.info("Committing...");
            let tx = app.tx.clone();
            let root = root.clone();
            std::thread::spawn(move || {
                let os: Vec<&OsStr> = a.iter().map(OsStr::new).collect();
                // Hooks can be slow; allow plenty of time.
                let res = run_git(&root, &os, Duration::from_secs(600));
                crate::app::call(&tx, move |app| match res {
                    Ok(out) => {
                        let first = String::from_utf8_lossy(&out).lines().next().unwrap_or("Committed").to_string();
                        after_change(app, first);
                    }
                    Err(e) => app.error(format!("git commit failed:\n{e:#}")),
                });
            });
        });
    app.push_dialog(Box::new(d));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(parts: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p);
            v.push(0);
        }
        v
    }

    #[test]
    fn parses_porcelain_v2() {
        let data = rec(&[
            b"# branch.oid 0123456789abcdef0123456789abcdef01234567",
            b"# branch.head main",
            b"# branch.upstream origin/main",
            b"# branch.ab +2 -1",
            b"1 .M N... 100644 100644 100644 aaaa bbbb src/main.rs",
            b"1 M. N... 100644 100644 100644 aaaa bbbb staged file.txt",
            b"1 A. N... 000000 100644 100644 0000 bbbb docs/new.md",
            b"2 R. N... 100644 100644 100644 aaaa aaaa R100 docs/renamed name.md",
            b"docs/old name.md",
            b"u UU N... 100644 100644 100644 100644 aaaa bbbb cccc conflict.rs",
            b"1 .D N... 100644 100644 000000 aaaa aaaa gone/deleted.rs",
            b"? weird\nname.txt",
            b"? untracked_dir/",
            b"! target/",
            b"! debug.log",
        ]);
        let rs = RepoStatus::parse(Path::new("/repo"), &data);
        assert_eq!(rs.branch.as_deref(), Some("main"));
        assert_eq!((rs.ahead, rs.behind), (2, 1));
        assert_eq!(rs.upstream.as_deref(), Some("origin/main"));
        assert!(rs.dirty);
        let f = |p: &str| rs.files.get(Path::new(p)).copied();
        assert_eq!(f("src/main.rs"), Some(GitStatus::Modified));
        assert_eq!(f("staged file.txt"), Some(GitStatus::Staged));
        assert_eq!(f("docs/new.md"), Some(GitStatus::Added));
        assert_eq!(f("docs/renamed name.md"), Some(GitStatus::Renamed));
        assert_eq!(f("docs/old name.md"), None, "original path of a rename is not a file entry");
        assert_eq!(f("conflict.rs"), Some(GitStatus::Conflict));
        assert_eq!(f("gone/deleted.rs"), Some(GitStatus::Deleted));
        assert_eq!(f("weird\nname.txt"), Some(GitStatus::Untracked));
        assert_eq!(f("debug.log"), Some(GitStatus::Ignored));
        // Directory aggregation: highest rank below wins.
        let d = |p: &str| rs.dirs.get(Path::new(p)).copied();
        assert_eq!(d("src"), Some(GitStatus::Modified));
        assert_eq!(d("docs"), Some(GitStatus::Renamed));
        assert_eq!(d("gone"), Some(GitStatus::Deleted));
        assert_eq!(d("untracked_dir"), Some(GitStatus::Untracked));
        assert_eq!(d("target"), Some(GitStatus::Ignored));
        // Lookups through the panel API, including files inside reported directories.
        let root = Path::new("/repo");
        assert_eq!(rs.status_of(root, "src", true), Some(GitStatus::Modified));
        assert_eq!(rs.status_of(&root.join("untracked_dir"), "a.txt", false), Some(GitStatus::Untracked));
        assert_eq!(rs.status_of(&root.join("target/debug"), "x", false), Some(GitStatus::Ignored));
        assert_eq!(rs.status_of(root, "clean.rs", false), None);
        assert_eq!(rs.status_of(root, "..", true), None);
        assert_eq!(rs.summary(), " main +2 -1 * ");
    }

    #[test]
    fn detached_and_initial() {
        let rs = RepoStatus::parse(Path::new("/r"), &rec(&[b"# branch.oid (initial)", b"# branch.head main"]));
        assert_eq!(rs.oid, None);
        assert!(!rs.dirty);
        assert_eq!(rs.summary(), " main ");
        let rs =
            RepoStatus::parse(Path::new("/r"), &rec(&[b"# branch.oid abcdef1234567890", b"# branch.head (detached)"]));
        assert_eq!(rs.summary(), " (abcdef12) ");
    }

    #[test]
    fn ignored_does_not_make_parents_dirty() {
        let rs = RepoStatus::parse(Path::new("/r"), &rec(&[b"! build/out.o"]));
        assert!(!rs.dirty);
        assert_eq!(rs.dirs.get(Path::new("build")), None);
    }

    #[test]
    fn real_repository() {
        if Proc::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("pairdeck-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let git = |a: &[&str]| {
            let ok = Proc::new("git")
                .arg("-C")
                .arg(&dir)
                .args(a)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {a:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "two\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.join("sub/new file.txt"), "x\n").unwrap();
        std::fs::write(dir.join("debug.log"), "x\n").unwrap();
        std::fs::write(dir.join("staged.txt"), "s\n").unwrap();
        git(&["add", "staged.txt"]);

        let root = find_root(&dir.join("sub")).unwrap();
        assert_eq!(root, dir);
        let rs = collect_status(&root).unwrap();
        assert_eq!(rs.branch.as_deref(), Some("main"));
        assert!(rs.dirty);
        assert_eq!(rs.status_of(&dir, "a.txt", false), Some(GitStatus::Modified));
        assert_eq!(rs.status_of(&dir, "staged.txt", false), Some(GitStatus::Added));
        assert_eq!(rs.status_of(&dir.join("sub"), "new file.txt", false), Some(GitStatus::Untracked));
        assert_eq!(rs.status_of(&dir.join("sub"), "b.txt", false), None);
        assert_eq!(rs.status_of(&dir, "sub", true), Some(GitStatus::Untracked));
        assert_eq!(rs.status_of(&dir, "debug.log", false), Some(GitStatus::Ignored));

        let theme = Theme::by_name("dark");
        let diff = preview_diff(&dir.join("a.txt"), &theme).expect("diff for modified file");
        let text: String = diff.iter().flat_map(|l| l.spans.iter().map(|s| s.content.to_string())).collect();
        assert!(text.contains("+changed") && text.contains("-one"), "{text}");
        assert!(preview_diff(&dir.join("sub/b.txt"), &theme).is_none(), "clean file has no diff");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
