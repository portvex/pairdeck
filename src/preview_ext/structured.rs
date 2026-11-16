//! JSON / TOML helpers: pretty printing, validation and a collapsible JSON tree.

use super::fg;
use crate::theme::Theme;
use crate::util::{pad_right, str_width, trunc_end};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// Files larger than this are shown as highlighted source only.
pub const MAX_STRUCTURED_BYTES: usize = 8 << 20;
/// Nodes kept in the tree at most.
pub const MAX_NODES: usize = 200_000;

pub fn is_json_ext(ext: &str) -> bool {
    matches!(ext, "json" | "geojson" | "har" | "ipynb" | "webmanifest" | "jsonl" | "ndjson")
}

/// Parse JSON (or JSON Lines: one value per line becomes an array).
pub fn parse_json(text: &str) -> Result<Value, String> {
    if text.len() > MAX_STRUCTURED_BYTES {
        return Err("file too large for structured view".into());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(v) => Ok(v),
        Err(e) => {
            // JSON Lines.
            let mut items = Vec::new();
            for l in text.lines().filter(|l| !l.trim().is_empty()) {
                match serde_json::from_str::<Value>(l) {
                    Ok(v) => items.push(v),
                    Err(_) => return Err(e.to_string()),
                }
            }
            if items.len() > 1 { Ok(Value::Array(items)) } else { Err(e.to_string()) }
        }
    }
}

pub fn pretty_json(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Validate TOML; returns the error message (with line info) if invalid.
pub fn toml_error(text: &str) -> Option<String> {
    if text.len() > MAX_STRUCTURED_BYTES {
        return None;
    }
    toml::from_str::<toml::Table>(text).err().map(|e| e.to_string().lines().next().unwrap_or("").to_string())
}

#[derive(Clone, Debug)]
enum Kind {
    Object(usize),
    Array(usize),
    Scalar(String, Scalar),
}

#[derive(Clone, Copy, Debug)]
enum Scalar {
    Str,
    Num,
    Bool,
    Null,
}

#[derive(Clone, Debug)]
struct Node {
    depth: u16,
    key: Option<String>,
    kind: Kind,
    /// Index just past this node's subtree in `nodes` (pre-order).
    end: usize,
    parent: Option<usize>,
    expanded: bool,
}

/// Flattened pre-order JSON tree with expand / collapse and a cursor.
#[derive(Clone, Debug)]
pub struct JsonTree {
    nodes: Vec<Node>,
    visible: Vec<usize>,
    pub cursor: usize,
    pub offset: usize,
    pub truncated: bool,
}

fn scalar(v: &Value) -> (String, Scalar) {
    match v {
        Value::String(s) => (format!("\"{}\"", s.replace('\n', "\\n")), Scalar::Str),
        Value::Number(n) => (n.to_string(), Scalar::Num),
        Value::Bool(b) => (b.to_string(), Scalar::Bool),
        _ => ("null".into(), Scalar::Null),
    }
}

impl JsonTree {
    pub fn new(v: &Value) -> JsonTree {
        let mut t = JsonTree { nodes: Vec::new(), visible: Vec::new(), cursor: 0, offset: 0, truncated: false };
        t.push(v, None, 0, None);
        t.recompute();
        t
    }

    fn push(&mut self, v: &Value, key: Option<String>, depth: u16, parent: Option<usize>) {
        if self.nodes.len() >= MAX_NODES {
            self.truncated = true;
            return;
        }
        let idx = self.nodes.len();
        let kind = match v {
            Value::Object(m) => Kind::Object(m.len()),
            Value::Array(a) => Kind::Array(a.len()),
            other => {
                let (s, k) = scalar(other);
                Kind::Scalar(s, k)
            }
        };
        self.nodes.push(Node { depth, key, kind, end: idx + 1, parent, expanded: depth < 2 });
        match v {
            Value::Object(m) => {
                for (k, child) in m {
                    self.push(child, Some(k.clone()), depth.saturating_add(1), Some(idx));
                }
            }
            Value::Array(a) => {
                for (i, child) in a.iter().enumerate() {
                    self.push(child, Some(format!("[{i}]")), depth.saturating_add(1), Some(idx));
                }
            }
            _ => {}
        }
        self.nodes[idx].end = self.nodes.len();
    }

    fn recompute(&mut self) {
        let cur = self.visible.get(self.cursor).copied();
        self.visible.clear();
        let mut i = 0;
        while i < self.nodes.len() {
            self.visible.push(i);
            let n = &self.nodes[i];
            i = if n.expanded || matches!(n.kind, Kind::Scalar(..)) { i + 1 } else { n.end };
        }
        if let Some(c) = cur {
            // Keep the cursor on the same node, or its nearest visible ancestor.
            let mut target = Some(c);
            while let Some(t) = target {
                if let Ok(p) = self.visible.binary_search(&t) {
                    self.cursor = p;
                    return;
                }
                target = self.nodes[t].parent;
            }
        }
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }

    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    fn current(&self) -> Option<usize> {
        self.visible.get(self.cursor).copied()
    }

    fn is_container(&self, i: usize) -> bool {
        !matches!(self.nodes[i].kind, Kind::Scalar(..))
    }

    pub fn move_by(&mut self, d: isize) {
        let n = self.visible.len() as isize;
        self.cursor = (self.cursor as isize + d).clamp(0, (n - 1).max(0)) as usize;
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.visible.len().saturating_sub(1);
    }

    /// Enter: toggle the node under the cursor.
    pub fn toggle(&mut self) {
        if let Some(i) = self.current().filter(|&i| self.is_container(i)) {
            self.nodes[i].expanded = !self.nodes[i].expanded;
            self.recompute();
        }
    }

    /// Right: expand, or step into the first child.
    pub fn right(&mut self) {
        let Some(i) = self.current() else { return };
        if !self.is_container(i) {
            return;
        }
        if !self.nodes[i].expanded {
            self.nodes[i].expanded = true;
            self.recompute();
        } else if self.nodes[i].end > i + 1 {
            self.move_by(1);
        }
    }

    /// Left: collapse, or jump to the parent.
    pub fn left(&mut self) {
        let Some(i) = self.current() else { return };
        if self.is_container(i) && self.nodes[i].expanded {
            self.nodes[i].expanded = false;
            self.recompute();
        } else if let Some(p) = self.nodes[i].parent
            && let Ok(pos) = self.visible.binary_search(&p)
        {
            self.cursor = pos;
        }
    }

    /// Lines for a viewport of `height` rows and `width` cells (scrolls to the cursor).
    pub fn render(&mut self, height: usize, width: usize, t: &Theme, focused: bool) -> Vec<Line<'static>> {
        if height == 0 {
            return Vec::new();
        }
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + height {
            self.offset = self.cursor + 1 - height;
        }
        let key_st = t.panel.fg(fg(t.dir, Color::Blue));
        let str_st = t.panel.fg(fg(t.exec, Color::Green));
        let num_st = t.panel.fg(fg(t.media, Color::Magenta));
        let lit_st = t.panel.fg(fg(t.header, Color::Yellow));
        let dim = t.panel.patch(t.hidden);
        let mut out = Vec::new();
        for (row, &i) in self.visible.iter().enumerate().skip(self.offset).take(height) {
            let n = &self.nodes[i];
            let indent = "  ".repeat(n.depth as usize);
            let mut spans = vec![Span::styled(indent, dim)];
            let marker = match n.kind {
                Kind::Scalar(..) => "  ",
                _ if n.expanded => "\u{25be} ",
                _ => "\u{25b8} ",
            };
            spans.push(Span::styled(marker, dim));
            if let Some(k) = &n.key {
                spans.push(Span::styled(k.clone(), key_st.add_modifier(Modifier::BOLD)));
                spans.push(Span::styled(": ", dim));
            }
            match &n.kind {
                Kind::Object(c) => spans
                    .push(Span::styled(if n.expanded { "{".to_string() } else { format!("{{...}} {c} keys") }, dim)),
                Kind::Array(c) => {
                    spans.push(Span::styled(if n.expanded { "[".to_string() } else { format!("[...] {c} items") }, dim))
                }
                Kind::Scalar(s, k) => {
                    let st = match k {
                        Scalar::Str => str_st,
                        Scalar::Num => num_st,
                        Scalar::Bool | Scalar::Null => lit_st,
                    };
                    spans.push(Span::styled(s.clone(), st));
                }
            }
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            let mut line = if str_width(&text) > width {
                Line::styled(trunc_end(&text, width), t.panel)
            } else {
                Line::from(spans)
            };
            if row == self.cursor {
                let st: Style = if focused { t.cursor } else { t.cursor_inactive };
                let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                line = Line::styled(pad_right(&plain, width), st);
            }
            out.push(line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_lines_and_pretty() {
        let v = parse_json("{\"b\":1,\"a\":[1,2]}").unwrap();
        let p = pretty_json(&v);
        assert!(p.find("\"b\"").unwrap() < p.find("\"a\"").unwrap(), "key order preserved");
        let v = parse_json("{\"x\":1}\n{\"x\":2}\n").unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert!(parse_json("{oops").is_err());
    }

    #[test]
    fn toml_validation() {
        assert!(toml_error("a = 1\n[t]\nb = \"x\"\n").is_none());
        assert!(toml_error("a = = 1").is_some());
    }

    #[test]
    fn tree_expand_collapse_navigation() {
        let v = parse_json(r#"{"a":{"b":{"c":1}},"list":[1,2,3],"s":"x"}"#).unwrap();
        let mut t = JsonTree::new(&v);
        // root, a, b (collapsed at depth 2), list, 1, 2, 3, s
        assert_eq!(t.len(), 8);
        t.move_by(2);
        t.right();
        assert_eq!(t.len(), 9, "b expanded shows c");
        t.left();
        assert_eq!(t.len(), 8);
        t.left();
        assert_eq!(t.cursor, 1, "left on collapsed node jumps to parent");
        t.toggle();
        assert_eq!(t.len(), 7, "a collapsed: root, a, list, 1, 2, 3, s");
        let theme = Theme::by_name("dark");
        let lines = t.render(3, 40, &theme, true);
        assert_eq!(lines.len(), 3);
        let shown: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(shown.contains("1 keys"), "{shown}");
    }
}
