//! Small helpers shared across modules.

use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

pub fn expand_tilde(s: &str) -> String {
    if s == "~" {
        return home().display().to_string();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home().join(rest).display().to_string();
    }
    s.to_string()
}

/// Replace the home prefix with `~` for display.
pub fn tildify(p: &str) -> String {
    let h = home().display().to_string();
    if h != "/" && (p == h || p.starts_with(&format!("{h}/"))) { format!("~{}", &p[h.len()..]) } else { p.to_string() }
}

pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    if n < 1024 {
        return format!("{n}");
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 { format!("{v:.1}{}", UNITS[u]) } else { format!("{v:.0}{}", UNITS[u]) }
}

/// Size with thousands separators, e.g. 1 234 567.
pub fn grouped(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// MC-style date: "Mar 14 12:30" for recent files, "Mar 14  2021" for older ones.
pub fn short_time(t: Option<SystemTime>) -> String {
    let Some(t) = t else { return "            ".into() };
    let dt: DateTime<Local> = t.into();
    let now = Local::now();
    let age = now.signed_duration_since(dt);
    if age.num_days() < 180 && age.num_days() > -1 {
        dt.format("%b %e %H:%M").to_string()
    } else {
        dt.format("%b %e  %Y").to_string()
    }
}

pub fn full_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => DateTime::<Local>::from(t).format("%Y-%m-%d %H:%M:%S").to_string(),
        None => "-".into(),
    }
}

pub fn user_name(uid: u32) -> String {
    static CACHE: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    let m = CACHE.get_or_init(Default::default);
    let mut m = m.lock().unwrap();
    m.entry(uid)
        .or_insert_with(|| {
            uzers::get_user_by_uid(uid)
                .map(|u| u.name().to_string_lossy().into_owned())
                .unwrap_or_else(|| uid.to_string())
        })
        .clone()
}

pub fn group_name(gid: u32) -> String {
    static CACHE: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    let m = CACHE.get_or_init(Default::default);
    let mut m = m.lock().unwrap();
    m.entry(gid)
        .or_insert_with(|| {
            uzers::get_group_by_gid(gid)
                .map(|g| g.name().to_string_lossy().into_owned())
                .unwrap_or_else(|| gid.to_string())
        })
        .clone()
}

pub fn str_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate to display width, cutting the end and adding `~` (MC style).
pub fn trunc_end(s: &str, width: usize) -> String {
    if str_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for g in s.graphemes(true) {
        let gw = str_width(g);
        if w + gw + 1 > width {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out.push('~');
    out
}

/// Truncate in the middle: "very_lo~ame.txt".
pub fn trunc_mid(s: &str, width: usize) -> String {
    if str_width(s) <= width {
        return s.to_string();
    }
    if width < 4 {
        return trunc_end(s, width);
    }
    let g: Vec<&str> = s.graphemes(true).collect();
    let keep_right = (width - 1) / 2;
    let keep_left = width - 1 - keep_right;
    let mut left = String::new();
    let mut lw = 0;
    for x in &g {
        let w = str_width(x);
        if lw + w > keep_left {
            break;
        }
        left.push_str(x);
        lw += w;
    }
    let mut right: Vec<&str> = Vec::new();
    let mut rw = 0;
    for x in g.iter().rev() {
        let w = str_width(x);
        if rw + w > keep_right {
            break;
        }
        right.push(x);
        rw += w;
    }
    right.reverse();
    format!("{left}~{}", right.concat())
}

/// Truncate a path from the left: "~/…/deep/dir".
pub fn trunc_left(s: &str, width: usize) -> String {
    if str_width(s) <= width {
        return s.to_string();
    }
    if width <= 1 {
        return "~".chars().take(width).collect();
    }
    let g: Vec<&str> = s.graphemes(true).collect();
    let mut out: Vec<&str> = Vec::new();
    let mut w = 0;
    for x in g.iter().rev() {
        let xw = str_width(x);
        if w + xw + 1 > width {
            break;
        }
        out.push(x);
        w += xw;
    }
    out.reverse();
    format!("~{}", out.concat())
}

pub fn pad_right(s: &str, width: usize) -> String {
    let w = str_width(s);
    if w >= width { s.to_string() } else { format!("{s}{}", " ".repeat(width - w)) }
}

pub fn pad_left(s: &str, width: usize) -> String {
    let w = str_width(s);
    if w >= width { s.to_string() } else { format!("{}{s}", " ".repeat(width - w)) }
}

/// Quote a string for POSIX shells.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./+,:@%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Shell glob (`*.rs`, `file?.txt`, `[ab]*`) to a matcher.
pub fn glob_matcher(pattern: &str, case_sensitive: bool) -> Option<globset::GlobMatcher> {
    globset::GlobBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .literal_separator(false)
        .build()
        .ok()
        .map(|g| g.compile_matcher())
}

pub fn hostname() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|h| h.split('.').next().unwrap_or("").to_string())
        .unwrap_or_else(|| "localhost".into())
}

pub fn current_user() -> String {
    user_name(nix::unistd::getuid().as_raw())
}

/// Detect binary content by looking for NUL bytes / invalid UTF-8 in a sample.
pub fn looks_binary(data: &[u8]) -> bool {
    let sample = &data[..data.len().min(8192)];
    if sample.contains(&0) {
        return true;
    }
    match std::str::from_utf8(sample) {
        Ok(_) => false,
        Err(e) => e.valid_up_to() < sample.len().saturating_sub(4),
    }
}
