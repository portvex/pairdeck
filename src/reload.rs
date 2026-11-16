//! Live reload: watch the config directory and re-apply config.toml, themes,
//! file associations and the user menu without a restart. Invalid files are
//! reported in a dialog and the previous settings stay in effect.

use crate::app::App;
use crate::config::Config;
use crate::keymap::KeyMap;
use crossbeam_channel::unbounded;
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Wait this long after the last change before reloading (editors write in bursts).
const DEBOUNCE: Duration = Duration::from_millis(200);

pub struct ConfigWatch {
    _watcher: notify::RecommendedWatcher,
    pub dir: PathBuf,
}

/// What a changed path means for us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Config,
    Theme(String),
    Assoc,
    Menu,
}

/// Classify a changed path inside the config directory.
pub fn classify(dir: &Path, path: &Path) -> Option<Change> {
    let rel = path.strip_prefix(dir).ok()?;
    let name = rel.file_name()?.to_str()?;
    let parent = rel.parent().and_then(|p| p.to_str()).unwrap_or("");
    match (parent, name) {
        ("", "config.toml") => Some(Change::Config),
        ("", "assoc.toml") => Some(Change::Assoc),
        ("", "menu") => Some(Change::Menu),
        // The themes directory itself (e.g. just created). On Linux a file written
        // right after the directory appears can be missed by inotify, so re-check
        // the active theme. An empty name means "whichever theme is active".
        ("", "themes") => Some(Change::Theme(String::new())),
        ("themes", n) if n.ends_with(".toml") => Some(Change::Theme(n.trim_end_matches(".toml").to_string())),
        _ => None,
    }
}

/// Start (or keep) watching the config directory. Safe to call repeatedly, e.g.
/// after the directory was created by saving a file.
pub fn start(app: &mut App) {
    if !app.cfg.live_reload {
        app.config_watch = None;
        return;
    }
    let Some(dir) = crate::config::config_dir() else { return };
    if !dir.is_dir() || app.config_watch.as_ref().is_some_and(|w| w.dir == dir) {
        return;
    }
    let (ptx, prx) = unbounded::<PathBuf>();
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && !matches!(ev.kind, notify::EventKind::Access(_))
        {
            for p in ev.paths {
                let _ = ptx.send(p);
            }
        }
    });
    let Ok(mut watcher) = watcher else { return };
    if watcher.watch(&dir, RecursiveMode::Recursive).is_err() {
        return;
    }
    let tx = app.tx.clone();
    // Events carry canonical paths (macOS reports /private/var for /var, and a
    // config dir may be a symlink into a dotfiles repo): compare against both.
    let wdir = dir.clone();
    let cdir = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    // Ends when the watcher (and with it the sender) is dropped.
    std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            while let Ok(first) = prx.recv() {
                let mut paths = vec![first];
                let deadline = Instant::now() + DEBOUNCE;
                while let Ok(p) = prx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    paths.push(p);
                }
                let mut changes: Vec<Change> =
                    paths.iter().filter_map(|p| classify(&cdir, p).or_else(|| classify(&wdir, p))).collect();
                changes.dedup();
                changes.sort_by_key(|c| format!("{c:?}"));
                changes.dedup();
                if !changes.is_empty() {
                    crate::app::call(&tx, move |app| apply(app, &changes));
                }
            }
        })
        .ok();
    app.config_watch = Some(ConfigWatch { _watcher: watcher, dir });
}

/// Apply a set of changes (also called directly by tests).
pub fn apply(app: &mut App, changes: &[Change]) {
    let mut done = Vec::new();
    for c in changes {
        match c {
            Change::Config => match reload_config(app) {
                Ok(true) => done.push("configuration"),
                Ok(false) => {}
                Err(e) => {
                    app.error(format!("{e}\n\nKeeping the previous configuration."));
                    return;
                }
            },
            Change::Theme(name) if name.is_empty() || *name == app.cfg.theme => {
                match validate_toml(&theme_path(&app.cfg.theme)) {
                    Ok(()) => {
                        app.theme = crate::theme::Theme::load(&app.cfg.theme.clone());
                        app.previewer.invalidate();
                        done.push("theme");
                    }
                    Err(e) => app.error(format!("{e}\n\nKeeping the previous theme.")),
                }
            }
            Change::Theme(_) => {}
            Change::Assoc => match validate_assoc() {
                Ok(()) => done.push("file associations"),
                Err(e) => app.error(format!("{e}\n\nThe built-in associations are used until it is fixed.")),
            },
            Change::Menu => done.push("user menu"),
        }
    }
    if !done.is_empty() {
        app.info(format!("Reloaded {}", done.join(", ")));
    }
}

fn theme_path(name: &str) -> PathBuf {
    crate::config::config_dir().unwrap_or_default().join("themes").join(format!("{name}.toml"))
}

fn validate_toml(path: &Path) -> Result<(), String> {
    match std::fs::read_to_string(path) {
        Ok(t) => toml::from_str::<toml::Table>(&t).map(|_| ()).map_err(|e| format!("{}:\n{e}", path.display())),
        // Deleted file: fall back to built-ins, nothing to report.
        Err(_) => Ok(()),
    }
}

fn validate_assoc() -> Result<(), String> {
    let path = crate::config::config_dir().unwrap_or_default().join("assoc.toml");
    let Ok(text) = std::fs::read_to_string(&path) else { return Ok(()) };
    let table = toml::from_str::<toml::Table>(&text).map_err(|e| format!("{}:\n{e}", path.display()))?;
    if let Some(rules) = table.get("rule") {
        let rules = rules.as_array().ok_or_else(|| format!("{}: `rule` must be an array of tables", path.display()))?;
        for (i, r) in rules.iter().enumerate() {
            if r.get("glob").and_then(|g| g.as_str()).is_none() {
                return Err(format!("{}: rule {} has no `glob`", path.display(), i + 1));
            }
        }
    }
    Ok(())
}

/// Re-read config.toml. Ok(false) when nothing changed (e.g. our own save).
fn reload_config(app: &mut App) -> Result<bool, String> {
    let Some(path) = Config::path() else { return Ok(false) };
    let new: Config = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("{}:\n{e}", path.display()))?,
        Err(_) => Config::default(),
    };
    if new == app.cfg {
        return Ok(false);
    }
    apply_config(app, new);
    Ok(true)
}

/// Install a new configuration: settings, theme, key bindings, panel options.
pub fn apply_config(app: &mut App, new: Config) {
    let (keymap, errors) = KeyMap::new(&new.keys);
    app.keymap = keymap;
    app.theme = crate::theme::Theme::load(&new.theme);
    for p in &mut app.panels {
        p.show_hidden = new.show_hidden;
        p.dirs_first = new.dirs_first;
        p.case_sensitive = new.case_sensitive_sort;
        p.reload();
    }
    app.cfg = new;
    app.previewer.invalidate();
    app.setup_watcher();
    if !errors.is_empty() {
        app.error(errors.join("\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_config_files() {
        let d = Path::new("/c");
        assert_eq!(classify(d, Path::new("/c/config.toml")), Some(Change::Config));
        assert_eq!(classify(d, Path::new("/c/assoc.toml")), Some(Change::Assoc));
        assert_eq!(classify(d, Path::new("/c/menu")), Some(Change::Menu));
        assert_eq!(classify(d, Path::new("/c/themes/mine.toml")), Some(Change::Theme("mine".into())));
        assert_eq!(classify(d, Path::new("/c/config.toml.swp")), None);
        assert_eq!(classify(d, Path::new("/c/themes/x/y.toml")), None);
        assert_eq!(classify(d, Path::new("/c/themes")), Some(Change::Theme(String::new())));
        assert_eq!(classify(d, Path::new("/elsewhere/config.toml")), None);
    }
}
