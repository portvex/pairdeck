//! Light highlighting for text without a grammar: URLs, e-mail addresses,
//! dates and times, numbers, file paths and log levels.

use super::fg;
use crate::theme::Theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use regex::Regex;
use std::sync::OnceLock;

fn re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(concat!(
            r"(?P<url>\b(?:https?|ftp|file)://[^\s<>()\[\]{}]+|\bwww\.[^\s<>()\[\]{}]+)",
            r"|(?P<email>\b[\w.+-]+@[\w-]+(?:\.[\w-]+)+\b)",
            r"|(?P<date>\b\d{4}-\d{2}-\d{2}(?:[ T]\d{2}:\d{2}(?::\d{2}(?:[.,]\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?)?\b",
            r"|\b\d{2}:\d{2}:\d{2}(?:[.,]\d+)?\b)",
            r"|(?P<level>\b(?:FATAL|CRITICAL|CRIT|ERROR|ERR|WARNING|WARN|NOTICE|INFO|DEBUG|TRACE)\b)",
            r"|(?P<path>(?:~|\.{1,2})?/[\w.@%+=:,\-]+(?:/[\w.@%+=:,\-]+)*/?)",
            r"|(?P<num>\b0x[0-9a-fA-F]+\b|\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?(?:[a-zA-Z%]{1,3}\b|\b))"
        ))
        .expect("plaintext regex")
    })
}

/// Severity of a log level word: 3 error, 2 warning, 1 info, 0 debug/trace.
fn severity(level: &str) -> u8 {
    match level {
        "FATAL" | "CRITICAL" | "CRIT" | "ERROR" | "ERR" => 3,
        "WARNING" | "WARN" => 2,
        "NOTICE" | "INFO" => 1,
        _ => 0,
    }
}

/// Styles derived from the app theme.
pub struct Palette {
    base: Style,
    url: Style,
    email: Style,
    date: Style,
    path: Style,
    num: Style,
    lvl: [Style; 4],
}

impl Palette {
    pub fn new(t: &Theme) -> Palette {
        let base = t.panel;
        let c = |s: Style, d: Color| base.fg(fg(s, d));
        Palette {
            base,
            url: c(t.symlink, Color::Cyan).add_modifier(Modifier::UNDERLINED),
            email: c(t.symlink, Color::Cyan),
            date: c(t.media, Color::Magenta),
            path: c(t.dir, Color::Blue),
            num: c(t.exec, Color::Green),
            lvl: [
                c(t.hidden, Color::DarkGray),
                c(t.diff_add, Color::Green).add_modifier(Modifier::BOLD),
                c(t.diff_change, Color::Yellow).add_modifier(Modifier::BOLD),
                c(t.diff_del, Color::Red).add_modifier(Modifier::BOLD),
            ],
        }
    }
}

/// Highlight one line. Error and warning lines get a tint for the whole message.
pub fn line(text: &str, p: &Palette) -> Line<'static> {
    let mut parts: Vec<(usize, usize, Option<Style>)> = Vec::new();
    let mut last = 0;
    let mut sev: Option<u8> = None;
    for caps in re().captures_iter(text) {
        let Some(m) = caps.get(0) else { continue };
        if m.start() > last {
            parts.push((last, m.start(), None));
        }
        let style = if caps.name("url").is_some() {
            p.url
        } else if caps.name("email").is_some() {
            p.email
        } else if caps.name("date").is_some() {
            p.date
        } else if let Some(l) = caps.name("level") {
            let s = severity(l.as_str());
            sev = Some(sev.map_or(s, |x| x.max(s)));
            p.lvl[s as usize]
        } else if caps.name("path").is_some() {
            p.path
        } else {
            p.num
        };
        parts.push((m.start(), m.end(), Some(style)));
        last = m.end();
    }
    if last < text.len() {
        parts.push((last, text.len(), None));
    }
    let plain = match sev {
        Some(3) => p.base.fg(fg(p.lvl[3], Color::Red)),
        Some(2) => p.base.fg(fg(p.lvl[2], Color::Yellow)),
        _ => p.base,
    };
    Line::from(
        parts
            .into_iter()
            .map(|(a, b, st)| Span::styled(text[a..b].to_string(), st.unwrap_or(plain)))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tokens() {
        let p = Palette::new(&Theme::by_name("dark"));
        let l = line("2026-11-13 12:00:01 INFO fetched https://example.com/a?b=1 for ann@x.org in /tmp/x.log 42ms", &p);
        let hits: Vec<String> = l.spans.iter().filter(|s| s.style != p.base).map(|s| s.content.to_string()).collect();
        for want in ["2026-11-13 12:00:01", "INFO", "https://example.com/a?b=1", "ann@x.org", "/tmp/x.log", "42ms"] {
            assert!(hits.iter().any(|h| h == want), "missing {want} in {hits:?}");
        }
    }

    #[test]
    fn error_lines_are_tinted_and_text_is_preserved() {
        let p = Palette::new(&Theme::by_name("dark"));
        let l = line("ERROR disk full", &p);
        let joined: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "ERROR disk full");
        assert_ne!(l.spans.last().unwrap().style, p.base);
        assert!(line("", &p).spans.is_empty());
        let l = line("\u{e9}\u{e9} 12 \u{1f600} /a/b", &p);
        let joined: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "\u{e9}\u{e9} 12 \u{1f600} /a/b");
    }
}
