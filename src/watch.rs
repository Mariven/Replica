//! Filesystem change notifications for the main loop. The periodic scan remains authoritative. A notification only marks a relation dirty, so the main loop runs a cycle over the dirty relations sooner than the next scheduled scan.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::engine::Relation;
use crate::medium::Medium;
use crate::site::Replica;

/// A message from the watcher to the main loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A path under a watched root changed. `relations` lists the relations whose roots contain it.
    Changed { relations: Vec<String>, path: PathBuf },
}

/// The roots registered with the operating system, each with the relations it belongs to.
#[derive(Default)]
struct Roots {
    by_root: BTreeMap<PathBuf, BTreeSet<String>>,
}

impl Roots {
    /// The relations whose registered roots contain the path, in name order.
    fn relations_for(&self, path: &Path) -> Vec<String> {
        let mut out = BTreeSet::new();
        for (root, relations) in &self.by_root {
            if path.starts_with(root) {
                out.extend(relations.iter().cloned());
            }
        }
        out.into_iter().collect()
    }
}

/// A running watcher over the roots of the watched relations.
pub struct Watch {
    _watcher: RecommendedWatcher,
    roots: Arc<Mutex<Roots>>,
}

impl Watch {
    /// Start watching. For each watched relation this registers the roots of its replicas and the directories where fan-out variables are enumerated. Each filesystem event under those roots produces a notice on `tx`.
    pub fn start(relations: &[Relation], medium: &dyn Medium, tx: Sender<Notice>) -> Result<Watch> {
        let roots = Arc::new(Mutex::new(Roots::default()));
        let shared = roots.clone();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };
            // Access events carry no change. Any other kind may.
            if matches!(event.kind, notify::EventKind::Access(_)) {
                return;
            }
            let guard = shared.lock().unwrap();
            for path in event.paths {
                let relations = guard.relations_for(&path);
                if !relations.is_empty() {
                    let _ = tx.send(Notice::Changed { relations, path });
                }
            }
        }).context("starting the filesystem watcher")?;
        let mut registered = BTreeSet::new();
        for rel in relations.iter().filter(|r| r.watch) {
            for root in watch_roots(rel, medium) {
                roots.lock().unwrap().by_root.entry(root.clone()).or_default().insert(rel.name.clone());
                if registered.insert(root.clone()) && medium.is_dir(&root) {
                    // A root that does not exist yet is left unregistered. The periodic scan picks it up once it appears.
                    if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
                        eprintln!("watch: cannot watch {}: {e}", root.display());
                    }
                }
            }
        }
        Ok(Watch { _watcher: watcher, roots })
    }

    /// The roots under watch, for status output.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.lock().unwrap().by_root.keys().cloned().collect()
    }
}

/// The directories to watch for a relation: each replica's root, plus, for a fan-out pattern, the pattern root where new replicas appear.
pub fn watch_roots(rel: &Relation, medium: &dyn Medium) -> Vec<PathBuf> {
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    for site in &rel.sites {
        for loc in &site.locations {
            out.insert(loc.pattern.root().to_path_buf());
        }
        if let Ok(replicas) = site.expand(&rel.identity, medium) {
            out.extend(replicas.iter().map(|r: &Replica| r.root().to_path_buf()));
        }
    }
    // A root inside another registered root is covered by the recursive watch on the outer one.
    let list: Vec<PathBuf> = out.into_iter().collect();
    list.iter().filter(|p| !list.iter().any(|q| q != *p && p.starts_with(q))).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_map_paths_to_relations() {
        let mut roots = Roots::default();
        roots.by_root.entry(PathBuf::from("/a")).or_default().insert("one".into());
        roots.by_root.entry(PathBuf::from("/a/b")).or_default().insert("two".into());
        roots.by_root.entry(PathBuf::from("/c")).or_default().insert("three".into());
        assert_eq!(roots.relations_for(Path::new("/a/b/f.txt")), vec!["one", "two"]);
        assert_eq!(roots.relations_for(Path::new("/c/x")), vec!["three"]);
        assert!(roots.relations_for(Path::new("/d/x")).is_empty());
    }
}
