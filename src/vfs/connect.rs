//! "Remote link" dialog: SFTP, S3 or WebDAV, connected on a background thread.

use super::{Location, s3, webdav};
use crate::app::App;
use crate::config::State;
use crate::dialog::{Form, ListAction, ListDialog, ListItem};
use anyhow::Result;

pub fn open(app: &mut App) {
    let items = vec![
        ListItem::new("SFTP (SSH)").hint("s"),
        ListItem::new("Amazon S3 / S3 compatible").hint("3"),
        ListItem::new("WebDAV (Nextcloud, ownCloud...)").hint("w"),
    ];
    let mut d = ListDialog::new("Remote link", items, |app, a| {
        if let ListAction::Select(i) = a {
            match i {
                0 => super::sftp::connect_dialog(app),
                1 => s3_dialog(app),
                _ => dav_dialog(app),
            }
        }
    });
    d.filterable = false;
    app.push_dialog(Box::new(d));
}

fn opt(s: String) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn s3_dialog(app: &mut App) {
    let hist = app.state.input_hist("s3_target").clone();
    let last = hist.last().cloned().unwrap_or_default();
    let ep_hist = app.state.input_hist("s3_endpoint").clone();
    let last_ep = ep_hist.last().cloned().unwrap_or_default();
    let d = Form::new("S3 link")
        .width(66)
        .input_hist("target", "Bucket and prefix (bucket/path):", &last, "s3_target", hist)
        .input_hist(
            "endpoint",
            "Endpoint URL (empty = AWS; e.g. http://localhost:9000):",
            &last_ep,
            "s3_endpoint",
            ep_hist,
        )
        .input("region", "Region (empty = AWS_REGION / profile / us-east-1):", "")
        .input("profile", "Profile in ~/.aws (empty = AWS_PROFILE / default):", "")
        .input("key", "Access key (empty = environment / profile):", "")
        .password("secret", "Secret key:")
        .check("anon", "Anonymous (public bucket, unsigned requests)", false)
        .buttons(&["Connect", "Cancel"], 0)
        .on_submit(|app, r| {
            if r.button != 0 {
                return;
            }
            let target = r.text("target").trim().trim_start_matches("s3://").to_string();
            let (bucket, path) = match target.find('/') {
                Some(i) => (target[..i].to_string(), target[i..].to_string()),
                None => (target.clone(), "/".to_string()),
            };
            if bucket.is_empty() {
                app.error("Enter a bucket name");
                return;
            }
            let cfg = s3::S3Config {
                bucket,
                path,
                endpoint: opt(r.text("endpoint")),
                region: opt(r.text("region")),
                profile: opt(r.text("profile")),
                access_key: opt(r.text("key")),
                secret_key: opt(r.text("secret")),
                session_token: None,
                anonymous: r.check("anon"),
            };
            spawn_connect(app, format!("s3://{target}"), move || s3::connect(&cfg));
        });
    app.push_dialog(Box::new(d));
}

fn dav_dialog(app: &mut App) {
    let hist = app.state.input_hist("dav_url").clone();
    let last = hist.last().cloned().unwrap_or_default();
    let d = Form::new("WebDAV link")
        .width(66)
        .input_hist("url", "URL (davs://host/path, https://host/remote.php/dav/files/me):", &last, "dav_url", hist)
        .input("user", "User (empty = none or from the URL):", "")
        .password("pw", "Password:")
        .buttons(&["Connect", "Cancel"], 0)
        .on_submit(|app, r| {
            if r.button != 0 || r.text("url").trim().is_empty() {
                return;
            }
            let url = r.text("url").trim().to_string();
            let mut t = match webdav::parse_url(&url) {
                Ok(t) => t,
                Err(e) => {
                    app.error(format!("{e:#}"));
                    return;
                }
            };
            if let Some(u) = opt(r.text("user")) {
                t.user = Some(u);
            }
            if !r.text("pw").is_empty() {
                t.password = Some(r.text("pw"));
            }
            spawn_connect(app, url, move || webdav::connect(&t));
        });
    app.push_dialog(Box::new(d));
}

/// Run `connect` on a worker thread and enter the location in the active panel.
pub fn spawn_connect(app: &mut App, label: String, connect: impl FnOnce() -> Result<Location> + Send + 'static) {
    app.info(format!("Connecting to {label}..."));
    let tx = app.tx.clone();
    std::thread::spawn(move || {
        let res = connect().map_err(|e| format!("{e:#}"));
        crate::app::call(&tx, move |app| match res {
            Ok(loc) => {
                let disp = loc.display();
                if let Err(e) = app.panel_mut().enter_vfs(loc) {
                    app.error(format!("{e:#}"));
                    return;
                }
                State::push_history(&mut app.state.dir_history, &disp);
                app.setup_watcher();
                app.info(format!("Connected: {disp}"));
            }
            Err(msg) => app.error(format!("Connection to {label} failed:\n{msg}")),
        });
    });
}
