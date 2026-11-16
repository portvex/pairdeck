//! File associations: what Enter (and optionally F3/F4) does for a file type.
//!
//! Rules live in `~/.config/pairdeck/assoc.toml`. The first rule whose glob matches wins.
//! Actions: `builtin:view`, `builtin:edit`, `system` (default desktop app), or a
//! shell command where `%f` is the quoted file path and `%d` the quoted directory.

use crate::app::{App, External};
use crate::util::shell_quote;
use crate::vfs::{Entry, Location};
use serde::Deserialize;
use std::path::PathBuf;

#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
pub struct Rule {
    pub glob: String,
    pub open: Option<String>,
    pub view: Option<String>,
    pub edit: Option<String>,
}

#[derive(Deserialize, Default)]
struct AssocFile {
    #[serde(default)]
    rule: Vec<Rule>,
}

pub const DEFAULT_ASSOC: &str = r#"# pairdeck file associations.
# The first rule whose glob matches the file name wins (case-insensitive).
# Actions:
#   "builtin:view"  internal viewer (shows images full screen)
#   "builtin:edit"  internal editor (or $EDITOR when configured)
#   "system"        open with the default desktop application
#   any other text  shell command; %f = quoted file path, %d = quoted directory
# Keys: open = Enter, view = F3 override, edit = F4 override (all optional).

[[rule]]
glob = "*.{png,jpg,jpeg,gif,bmp,webp,tiff,tif,ico,qoi,tga,pnm,ppm,pgm,avif,svg,svgz}"
open = "builtin:view"

[[rule]]
glob = "*.{pdf,doc,docx,odt,rtf,xls,xlsx,ods,ppt,pptx,odp,epub,djvu}"
open = "system"

[[rule]]
glob = "*.{mp4,mkv,webm,avi,mov,wmv,flv,m4v,mpg,mpeg,mp3,flac,wav,ogg,opus,m4a,aac,aiff}"
open = "system"

[[rule]]
glob = "*.{html,htm,heic}"
open = "system"

# Example: open Markdown files with glow
# [[rule]]
# glob = "*.md"
# open = "glow -p %f"
"#;

fn assoc_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("assoc.toml"))
}

fn rules() -> Vec<Rule> {
    let text = assoc_path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_else(|| DEFAULT_ASSOC.to_string());
    toml::from_str::<AssocFile>(&text)
        .or_else(|_| toml::from_str::<AssocFile>(DEFAULT_ASSOC))
        .map(|f| f.rule)
        .unwrap_or_default()
}

fn find_rule(name: &str) -> Option<Rule> {
    rules().into_iter().find(|r| {
        globset::GlobBuilder::new(&r.glob)
            .case_insensitive(true)
            .build()
            .map(|g| g.compile_matcher().is_match(name))
            .unwrap_or(false)
    })
}

#[allow(dead_code)]
/// F3 override for this entry, if a rule defines one.
pub fn view_override(_app: &App, e: &Entry) -> Option<String> {
    find_rule(&e.name).and_then(|r| r.view)
}

#[allow(dead_code)]
/// F4 override for this entry, if a rule defines one.
pub fn edit_override(_app: &App, e: &Entry) -> Option<String> {
    find_rule(&e.name).and_then(|r| r.edit)
}

/// Run an association action for `e` in the active panel. Returns true if handled.
pub fn run_action(app: &mut App, e: &Entry, action: &str) -> bool {
    let loc = app.panel().loc.clone();
    let path = loc.join(&e.name);
    match action.trim() {
        "builtin:view" => {
            crate::viewer::open(app, &loc, &path);
            true
        }
        "builtin:edit" => {
            app.open_editor(&loc, &path);
            true
        }
        "system" => {
            if !loc.is_local() {
                return false;
            }
            match open::that_detached(&path) {
                Ok(()) => app.info(format!("Opened {}", e.name)),
                Err(err) => app.error(format!("Cannot open {}:\n{err}", path.display())),
            }
            true
        }
        "" => false,
        cmd => {
            if !loc.is_local() {
                return false;
            }
            let expanded = cmd
                .replace("%f", &shell_quote(&path.display().to_string()))
                .replace("%d", &shell_quote(&loc.path.display().to_string()));
            app.external = Some(External::Command { cmd: expanded, cwd: loc.path.clone(), pause: false });
            true
        }
    }
}

/// Open an entry according to associations (Enter). Returns true if handled.
pub fn open_entry(app: &mut App, e: &Entry) -> bool {
    if e.is_dir() || e.is_parent() {
        return false;
    }
    let Some(rule) = find_rule(&e.name) else { return false };
    let Some(action) = rule.open else { return false };
    run_action(app, e, &action)
}

pub fn edit_file(app: &mut App) {
    let Some(path) = assoc_path() else {
        app.error("No configuration directory");
        return;
    };
    let dir = path.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    if !path.exists() {
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = std::fs::write(&path, DEFAULT_ASSOC) {
            app.error(format!("Cannot write {}:\n{e}", path.display()));
            return;
        }
    }
    app.open_editor(&Location::local(dir), &path);
}
