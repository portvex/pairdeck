//! Frecent directories, zoxide style: every visit bumps a directory's rank, the
//! score weights the rank by how recently it was visited, and old entries age out.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Sum of all ranks above which every rank is scaled down (zoxide's `_ZO_MAXAGE`).
const MAX_AGE: f64 = 10_000.0;
/// Hard cap on remembered directories.
const MAX_ENTRIES: usize = 1000;

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DirVisit {
    pub path: String,
    /// Visit count, decayed by aging.
    pub rank: f64,
    /// Last visit, seconds since the Unix epoch.
    pub last: u64,
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Rank weighted by recency (same buckets as zoxide).
pub fn score(e: &DirVisit, now: u64) -> f64 {
    let age = now.saturating_sub(e.last);
    let factor = if age < HOUR {
        4.0
    } else if age < DAY {
        2.0
    } else if age < WEEK {
        0.5
    } else {
        0.25
    };
    e.rank * factor
}

/// Record a visit to `path`.
pub fn record(list: &mut Vec<DirVisit>, path: &str, now: u64) {
    if path.is_empty() {
        return;
    }
    match list.iter_mut().find(|e| e.path == path) {
        Some(e) => {
            e.rank += 1.0;
            e.last = now;
        }
        None => list.push(DirVisit { path: path.to_string(), rank: 1.0, last: now }),
    }
    age(list, now);
}

/// Scale ranks down once their sum passes MAX_AGE, dropping entries that fall
/// below 1, and keep at most MAX_ENTRIES (lowest scores go first).
pub fn age(list: &mut Vec<DirVisit>, now: u64) {
    let total: f64 = list.iter().map(|e| e.rank).sum();
    if total > MAX_AGE {
        let k = 0.9 * MAX_AGE / total;
        for e in list.iter_mut() {
            e.rank *= k;
        }
        list.retain(|e| e.rank >= 1.0);
    }
    if list.len() > MAX_ENTRIES {
        list.sort_by(|a, b| score(b, now).total_cmp(&score(a, now)));
        list.truncate(MAX_ENTRIES);
    }
}

/// Entries sorted by score, best first.
pub fn ranked(list: &[DirVisit], now: u64) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = list.iter().map(|e| (e.path.clone(), score(e, now))).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// zoxide matching: all terms appear in the path in order (case-insensitive),
/// and the last term matches within the last path component.
pub fn matches(path: &str, terms: &[&str]) -> bool {
    let lower = path.to_lowercase();
    let mut pos = 0;
    for t in terms {
        let t = t.to_lowercase();
        match lower[pos..].find(&t) {
            Some(i) => pos += i + t.len(),
            None => return false,
        }
    }
    match terms.last() {
        Some(last) => {
            let name = lower.rsplit('/').next().unwrap_or("");
            name.contains(&last.to_lowercase())
        }
        None => true,
    }
}

/// Best existing directory for `terms`, skipping `exclude` (usually the current dir).
/// Directories that no longer exist are removed from the list.
pub fn best_match(list: &mut Vec<DirVisit>, terms: &[&str], exclude: Option<&Path>, now: u64) -> Option<String> {
    list.retain(|e| Path::new(&e.path).is_dir());
    ranked(list, now)
        .into_iter()
        .map(|(p, _)| p)
        .find(|p| matches(p, terms) && exclude.is_none_or(|x| x != Path::new(p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit(p: &str, rank: f64, last: u64) -> DirVisit {
        DirVisit { path: p.into(), rank, last }
    }

    #[test]
    fn recency_weights_rank() {
        let now = 100 * WEEK;
        let fresh = visit("/a", 1.0, now - 10);
        let old = visit("/b", 10.0, now - 30 * DAY);
        assert_eq!(score(&fresh, now), 4.0);
        assert_eq!(score(&old, now), 2.5);
        assert_eq!(score(&visit("/c", 1.0, now - 2 * HOUR), now), 2.0);
        assert_eq!(score(&visit("/d", 1.0, now - 3 * DAY), now), 0.5);
    }

    #[test]
    fn record_and_rank() {
        let mut l = Vec::new();
        record(&mut l, "/x", 100);
        record(&mut l, "/y", 100);
        record(&mut l, "/y", 100);
        let r = ranked(&l, 100);
        assert_eq!(r[0].0, "/y");
        assert_eq!(l.len(), 2);
        assert_eq!(l.iter().find(|e| e.path == "/y").unwrap().rank, 2.0);
    }

    #[test]
    fn aging_scales_and_drops() {
        let mut l: Vec<DirVisit> = (0..20).map(|i| visit(&format!("/d{i}"), 600.0, 0)).collect();
        l.push(visit("/rare", 1.0, 0));
        age(&mut l, 0);
        let total: f64 = l.iter().map(|e| e.rank).sum();
        assert!(total <= MAX_AGE, "total {total}");
        assert!(!l.iter().any(|e| e.path == "/rare"), "tiny ranks age out");
        let mut many: Vec<DirVisit> = (0..1500).map(|i| visit(&format!("/m{i}"), 2.0, 0)).collect();
        age(&mut many, 0);
        assert_eq!(many.len(), MAX_ENTRIES);
    }

    #[test]
    fn z_term_matching() {
        assert!(matches("/home/u/projects/pairdeck", &["pairdeck"]));
        assert!(matches("/home/u/projects/pairdeck", &["proj", "pair"]));
        assert!(matches("/home/u/Projects/PAIRDECK", &["projects", "pairdeck"]));
        assert!(!matches("/home/u/pairdeck/src", &["pairdeck"]), "last term must be in the last component");
        assert!(!matches("/home/u/projects/pairdeck", &["pair", "proj"]), "terms must be in order");
        assert!(matches("/anything", &[]));
    }

    #[test]
    fn best_match_prefers_score_and_skips_missing() {
        let tmp = std::env::temp_dir().join(format!("pairdeck-frec-{}", std::process::id()));
        let a = tmp.join("alpha/work");
        let b = tmp.join("beta/work");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (a, b) = (a.display().to_string(), b.display().to_string());
        let now = 10 * WEEK;
        let mut l = vec![visit(&a, 3.0, now), visit(&b, 9.0, now), visit("/no/such/work", 99.0, now)];
        assert_eq!(best_match(&mut l, &["work"], None, now).as_deref(), Some(b.as_str()));
        assert_eq!(l.len(), 2, "missing dirs are pruned");
        assert_eq!(best_match(&mut l, &["alpha", "work"], None, now).as_deref(), Some(a.as_str()));
        assert_eq!(best_match(&mut l, &["work"], Some(Path::new(&b)), now).as_deref(), Some(a.as_str()));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
