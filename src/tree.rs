//! Directory tree view (local file system).

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct TreeNode {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub expanded: bool,
    pub last: bool,
}

#[derive(Clone, Debug)]
pub struct TreeState {
    pub nodes: Vec<TreeNode>,
    pub cursor: usize,
    pub offset: usize,
    pub show_hidden: bool,
}

fn subdirs(path: &Path, hidden: bool) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(path)
        .map(|rd| {
            rd.flatten()
                .filter(|d| hidden || !d.file_name().to_string_lossy().starts_with('.'))
                .filter(|d| std::fs::metadata(d.path()).map(|m| m.is_dir()).unwrap_or(false))
                .map(|d| d.path())
                .collect()
        })
        .unwrap_or_default();
    v.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    v
}

impl TreeState {
    pub fn new(focus: &Path, show_hidden: bool) -> TreeState {
        let mut t = TreeState {
            nodes: vec![TreeNode { path: PathBuf::from("/"), name: "/".into(), depth: 0, expanded: false, last: true }],
            cursor: 0,
            offset: 0,
            show_hidden,
        };
        t.reveal(focus);
        t
    }

    /// Expand all ancestors of `target` and put the cursor on it.
    pub fn reveal(&mut self, target: &Path) {
        let mut idx = 0;
        let ancestors: Vec<&Path> = target.ancestors().collect::<Vec<_>>().into_iter().rev().collect();
        for anc in ancestors.iter().skip(1) {
            if !self.nodes[idx].expanded {
                self.expand(idx);
            }
            match self.nodes.iter().enumerate().skip(idx + 1).find(|(_, n)| n.path == *anc) {
                Some((i, _)) => idx = i,
                None => break,
            }
        }
        self.cursor = idx;
    }

    pub fn expand(&mut self, i: usize) {
        if i >= self.nodes.len() || self.nodes[i].expanded {
            return;
        }
        let depth = self.nodes[i].depth + 1;
        let kids = subdirs(&self.nodes[i].path, self.show_hidden);
        let n = kids.len();
        let new: Vec<TreeNode> = kids
            .into_iter()
            .enumerate()
            .map(|(k, p)| TreeNode {
                name: p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                path: p,
                depth,
                expanded: false,
                last: k + 1 == n,
            })
            .collect();
        self.nodes[i].expanded = true;
        self.nodes.splice(i + 1..i + 1, new);
    }

    pub fn collapse(&mut self, i: usize) {
        if i >= self.nodes.len() || !self.nodes[i].expanded {
            return;
        }
        let depth = self.nodes[i].depth;
        let end =
            self.nodes[i + 1..].iter().position(|n| n.depth <= depth).map(|p| i + 1 + p).unwrap_or(self.nodes.len());
        self.nodes.drain(i + 1..end);
        self.nodes[i].expanded = false;
        if self.cursor > i && self.cursor < end {
            self.cursor = i;
        }
    }

    pub fn toggle(&mut self) {
        let i = self.cursor;
        if self.nodes.get(i).is_some_and(|n| n.expanded) { self.collapse(i) } else { self.expand(i) }
    }

    pub fn selected(&self) -> Option<&Path> {
        self.nodes.get(self.cursor).map(|n| n.path.as_path())
    }

    pub fn move_by(&mut self, d: isize) {
        let n = self.nodes.len() as isize;
        self.cursor = (self.cursor as isize + d).clamp(0, (n - 1).max(0)) as usize;
    }

    pub fn left(&mut self) {
        let i = self.cursor;
        if self.nodes[i].expanded {
            self.collapse(i);
        } else {
            let depth = self.nodes[i].depth;
            if let Some(p) = self.nodes[..i].iter().rposition(|n| n.depth < depth) {
                self.cursor = p;
            }
        }
    }

    pub fn right(&mut self) {
        let i = self.cursor;
        if !self.nodes[i].expanded {
            self.expand(i);
        } else if i + 1 < self.nodes.len() && self.nodes[i + 1].depth > self.nodes[i].depth {
            self.cursor = i + 1;
        }
    }

    /// Tree-drawing prefix ("│ ├ └") for a node.
    pub fn prefix(&self, i: usize) -> String {
        let node = &self.nodes[i];
        if node.depth == 0 {
            return String::new();
        }
        let mut parts = Vec::new();
        // For each ancestor depth, decide whether a vertical line continues.
        let mut d = node.depth;
        let mut j = i;
        let mut own = if node.last { "└─" } else { "├─" };
        parts.push(own.to_string());
        while d > 1 {
            // find ancestor at depth d-1
            if let Some(p) = self.nodes[..j].iter().rposition(|n| n.depth == d - 1) {
                own = if self.nodes[p].last { "  " } else { "│ " };
                parts.push(own.to_string());
                j = p;
            }
            d -= 1;
        }
        parts.reverse();
        parts.concat()
    }
}
