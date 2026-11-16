//! Tabs: several directories per panel side.
//!
//! The visible tab always lives in `app.panels[side]`, so the rest of the program
//! never needs to know about tabs. The hidden tabs of a side are kept in
//! `app.tabs[side]`, and switching swaps the `Panel` values.

use crate::app::App;
use crate::panel::Panel;
use crate::vfs::Location;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The hidden tabs of one panel side.
#[derive(Default)]
pub struct TabSet {
    /// Hidden tabs in order; the visible tab sits at index `pos` of the full list.
    pub others: Vec<Panel>,
    /// Position of the visible tab within the full list.
    pub pos: usize,
}

impl TabSet {
    pub fn count(&self) -> usize {
        self.others.len() + 1
    }

    /// Make tab `k` (index in the full list) visible, swapping it with `active`.
    /// Returns false if `k` is out of range or already visible.
    pub fn switch(&mut self, active: &mut Panel, k: usize) -> bool {
        if k >= self.count() || k == self.pos {
            return false;
        }
        // Index of tab k inside `others` (which skips the visible position).
        let idx = if k < self.pos { k } else { k - 1 };
        let mut incoming = self.others.remove(idx);
        std::mem::swap(active, &mut incoming);
        // `incoming` now holds the previously visible tab: put it back at its slot.
        let back_idx = if self.pos < k { self.pos } else { self.pos - 1 };
        self.others.insert(back_idx, incoming);
        self.pos = k;
        true
    }

    /// Insert a new visible tab right after the current one.
    pub fn open(&mut self, active: &mut Panel, new: Panel) {
        let mut old = new;
        std::mem::swap(active, &mut old);
        self.others.insert(self.pos, old);
        self.pos += 1;
    }

    /// Close the visible tab, showing a neighbour. Returns false if it is the last one.
    pub fn close(&mut self, active: &mut Panel) -> bool {
        if self.others.is_empty() {
            return false;
        }
        // Prefer the tab to the right, like most tabbed applications.
        let idx = self.pos.min(self.others.len() - 1);
        *active = self.others.remove(idx);
        self.pos = idx;
        true
    }

    /// Labels for the tab bar: (full index, short name).
    pub fn labels(&self, active: &Panel) -> Vec<(usize, String)> {
        let mut v = Vec::with_capacity(self.count());
        let mut others = self.others.iter();
        for k in 0..self.count() {
            let p = if k == self.pos { active } else { others.next().expect("tab count") };
            v.push((k, short_name(p)));
        }
        v
    }

    /// Full list of tab directories in order (local tabs only), plus the visible index.
    pub fn paths(&self, active: &Panel) -> (Vec<String>, usize) {
        let mut out = Vec::new();
        let mut active_idx = 0;
        let mut others = self.others.iter();
        for k in 0..self.count() {
            let p = if k == self.pos { active } else { others.next().expect("tab count") };
            if let Some(path) = p.loc.local_path() {
                if k == self.pos {
                    active_idx = out.len();
                }
                out.push(path.display().to_string());
            }
        }
        (out, active_idx)
    }
}

fn short_name(p: &Panel) -> String {
    let path = &p.loc.path;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
    crate::util::trunc_mid(&name, 14)
}

/// Saved tabs of one side.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SavedTabs {
    pub paths: Vec<String>,
    pub active: usize,
}

/// Snapshot both sides for the state file.
pub fn save(app: &App) -> Vec<SavedTabs> {
    (0..2)
        .map(|i| {
            let (paths, active) = app.tabs[i].paths(&app.panels[i]);
            SavedTabs { paths, active }
        })
        .collect()
}

/// Rebuild hidden tabs around the panels created at startup. The visible panel keeps
/// its start directory; the other saved tabs are restored around it.
pub fn restore(app: &mut App) {
    let saved = app.state.tabs.clone();
    for (i, s) in saved.iter().enumerate().take(2) {
        if s.paths.len() < 2 {
            continue;
        }
        let active = s.active.min(s.paths.len() - 1);
        let mut set = TabSet::default();
        for (k, path) in s.paths.iter().enumerate() {
            if k == active {
                continue;
            }
            let p = PathBuf::from(path);
            if p.is_dir() {
                set.others.push(Panel::new(Location::local(p), &app.cfg));
            } else if k < active {
                // Missing directory before the visible tab: shift the visible position.
                continue;
            }
        }
        set.pos = active.min(set.others.len());
        app.tabs[i] = set;
    }
}

pub fn new_tab(app: &mut App) {
    let side = app.active;
    let mut tab = Panel::new(app.panels[side].loc.clone(), &app.cfg);
    tab.vfs_stack = app.panels[side].vfs_stack.clone();
    tab.show_hidden = app.panels[side].show_hidden;
    tab.mode = app.panels[side].mode;
    tab.sort = app.panels[side].sort;
    tab.reverse = app.panels[side].reverse;
    let (panels, tabs) = (&mut app.panels, &mut app.tabs);
    tabs[side].open(&mut panels[side], tab);
    after_switch(app);
}

pub fn close_tab(app: &mut App) {
    let side = app.active;
    let (panels, tabs) = (&mut app.panels, &mut app.tabs);
    if !tabs[side].close(&mut panels[side]) {
        app.flash_error("This is the last tab");
        return;
    }
    after_switch(app);
}

pub fn switch_to(app: &mut App, side: usize, k: usize) {
    let (panels, tabs) = (&mut app.panels, &mut app.tabs);
    if tabs[side].switch(&mut panels[side], k) {
        app.active = side;
        after_switch(app);
    }
}

pub fn cycle(app: &mut App, forward: bool) {
    let side = app.active;
    let n = app.tabs[side].count();
    if n < 2 {
        app.info("Only one tab (Ctrl-X n opens a new one)");
        return;
    }
    let pos = app.tabs[side].pos;
    let k = if forward { (pos + 1) % n } else { (pos + n - 1) % n };
    switch_to(app, side, k);
}

fn after_switch(app: &mut App) {
    let side = app.active;
    app.panels[side].reload();
    app.previewer.invalidate();
    app.setup_watcher();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn panel(dir: &std::path::Path) -> Panel {
        Panel::new(Location::local(dir), &Config::default())
    }

    #[test]
    fn open_switch_close_keep_order() {
        let base = std::env::temp_dir().join(format!("pairdeck-tabs-{}", std::process::id()));
        let dirs: Vec<PathBuf> = (0..3).map(|i| base.join(format!("d{i}"))).collect();
        for d in &dirs {
            std::fs::create_dir_all(d).unwrap();
        }
        let mut set = TabSet::default();
        let mut active = panel(&dirs[0]);
        set.open(&mut active, panel(&dirs[1]));
        set.open(&mut active, panel(&dirs[2]));
        assert_eq!(set.count(), 3);
        assert_eq!(set.pos, 2);
        assert_eq!(active.loc.path, dirs[2]);
        let names: Vec<String> = set.labels(&active).into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, ["d0", "d1", "d2"]);

        assert!(set.switch(&mut active, 0));
        assert_eq!(active.loc.path, dirs[0]);
        let (paths, idx) = set.paths(&active);
        assert_eq!(idx, 0);
        assert_eq!(paths, dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>());

        assert!(set.switch(&mut active, 2));
        assert_eq!(active.loc.path, dirs[2]);
        assert!(!set.switch(&mut active, 2), "already visible");
        assert!(!set.switch(&mut active, 7), "out of range");

        // Closing the last tab shows its left neighbour.
        assert!(set.close(&mut active));
        assert_eq!(active.loc.path, dirs[1]);
        assert!(set.close(&mut active));
        assert_eq!(active.loc.path, dirs[0]);
        assert!(!set.close(&mut active), "cannot close the last tab");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn saved_tabs_roundtrip_through_toml() {
        let s = SavedTabs { paths: vec!["/a".into(), "/b".into()], active: 1 };
        let state = crate::config::State { tabs: vec![s.clone(), SavedTabs::default()], ..Default::default() };
        let text = toml::to_string(&state).unwrap();
        let back: crate::config::State = toml::from_str(&text).unwrap();
        assert_eq!(back.tabs[0], s);
    }
}
