//! Session-only password store for encrypted archives and disk images, and the
//! error type the VFS layer uses to ask the UI for a password.
//!
//! Passwords live in process memory only: they are never written to disk,
//! logged, or included in error messages.

use crate::app::App;
use crate::dialog::Form;
use crate::vfs::Location;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// Returned (inside `anyhow::Error`) when an operation needs a password.
#[derive(Debug, Clone)]
pub struct PasswordRequired {
    /// Human readable archive name, e.g. `/tmp/a.zip`.
    pub archive: String,
    /// A password was supplied but it is wrong.
    pub wrong: bool,
}

impl std::fmt::Display for PasswordRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.wrong {
            write!(f, "{}: wrong password", self.archive)
        } else {
            write!(f, "{}: password required (press Enter or F3 on the file to unlock)", self.archive)
        }
    }
}

impl std::error::Error for PasswordRequired {}

/// Find a `PasswordRequired` anywhere in an error chain.
pub fn password_required(err: &anyhow::Error) -> Option<&PasswordRequired> {
    err.chain().find_map(|e| e.downcast_ref::<PasswordRequired>())
}

fn store() -> &'static Mutex<HashMap<String, String>> {
    static S: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// Remember a verified password for this session.
pub fn remember(key: &str, password: &str) {
    if let Ok(mut m) = store().lock() {
        m.insert(key.to_string(), password.to_string());
    }
}

/// Password remembered for `key` in this session.
pub fn recall(key: &str) -> Option<String> {
    store().lock().ok().and_then(|m| m.get(key).cloned())
}

/// Forget a password (e.g. after it turned out to be wrong).
pub fn forget(key: &str) {
    if let Ok(mut m) = store().lock() {
        m.remove(key);
    }
}

/// Run `then` once every encrypted item among `names` (in `loc`) is readable,
/// asking for the password first if needed. Wrong passwords ask again.
pub fn ensure_password(app: &mut App, loc: &Location, names: &[String], then: impl FnOnce(&mut App) + 'static) {
    let paths: Vec<PathBuf> = names.iter().map(|n| loc.join(n)).collect();
    let needs = loc.vfs.lock().needs_password(&paths);
    if !needs {
        then(app);
        return;
    }
    ask(app, loc.clone(), false, Box::new(then));
}

fn ask(app: &mut App, loc: Location, wrong: bool, then: Box<dyn FnOnce(&mut App)>) {
    let shown = loc.vfs.lock().display(std::path::Path::new("/"));
    let name = shown.strip_suffix("#/").unwrap_or(&shown).to_string();
    let mut form = Form::new("Password");
    if wrong {
        form = form.text("Wrong password, try again.");
    }
    let form = form
        .text(&format!("{} is encrypted.", crate::util::trunc_left(&name, 60)))
        .password("pw", "Password:")
        .buttons(&["OK", "Cancel"], 0)
        .on_submit(move |app, r| {
            if r.button != 0 {
                return;
            }
            let pw = r.text("pw");
            let res = loc.vfs.lock().set_password(&pw);
            match res {
                Ok(true) => {
                    app.previewer.invalidate();
                    then(app)
                }
                Ok(false) => ask(app, loc, true, then),
                Err(e) => app.error(format!("{e:#}")),
            }
        });
    app.push_dialog(Box::new(form));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_roundtrip_and_error_detection() {
        remember("k1", "secret");
        assert_eq!(recall("k1").as_deref(), Some("secret"));
        forget("k1");
        assert!(recall("k1").is_none());
        let e = anyhow::Error::new(PasswordRequired { archive: "a.zip".into(), wrong: false }).context("cannot read x");
        assert!(password_required(&e).is_some_and(|p| !p.wrong));
        assert!(!format!("{e:#}").contains("secret"));
    }
}
