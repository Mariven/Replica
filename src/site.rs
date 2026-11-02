//! Sites, keys, and replicas. A relation refers to its roles by site name; this module expands each site into concrete locations and identifies the same file across them.
//!
//! A site lists one or more patterns. The variables common to the patterns of all the sites in the relation make up the key of a file. When a variable appears on only some of the sites, each such site fans out into several replicas, one for each binding found on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::medium::{EntryKind, Medium};
use crate::pattern::{Binding, Pattern, describe_binding};

/// Separator between `name=value` pairs in the canonical text of a key. A control character is safe here because the values are path text.
const KEY_SEP: char = '\u{1f}';

/// The identity of a file within a relation: the binding of the identity variables.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(Binding);

impl Key {
    /// A key from a full binding, keeping only the identity variables.
    pub fn from_binding(binding: &Binding, identity: &BTreeSet<String>) -> Key {
        Key(binding.iter().filter(|(k, _)| identity.contains(*k)).map(|(k, v)| (k.clone(), v.clone())).collect())
    }

    /// The key of a relation without identity variables.
    pub fn empty() -> Key {
        Key(Binding::new())
    }

    /// The binding.
    pub fn binding(&self) -> &Binding {
        &self.0
    }

    /// The stable text stored in the database: `name=value` pairs in name order.
    pub fn canonical(&self) -> String {
        self.0.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(&KEY_SEP.to_string())
    }

    /// Parse the canonical text back.
    pub fn parse(text: &str) -> Key {
        if text.is_empty() {
            return Key::empty();
        }
        Key(text.split(KEY_SEP).filter_map(|pair| pair.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect())
    }
}

impl fmt::Display for Key {
    /// Show the lone value when the key has one variable and `name=value` pairs when it has more. The empty key shows as `(the file)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() { f.write_str("(the file)") } else { f.write_str(&describe_binding(&self.0)) }
    }
}

/// A pattern under a site, with a label when the configuration lists several locations for the site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The map key or the pattern text as written, when the site lists several locations.
    pub label: Option<String>,
    pub pattern: Pattern,
}

/// A named role of a relation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Site {
    pub name: String,
    pub locations: Vec<Location>,
}

impl Site {
    /// A site with one pattern.
    pub fn single(name: &str, pattern: Pattern) -> Site {
        Site { name: name.to_owned(), locations: vec![Location { label: None, pattern }] }
    }

    /// The variable names appearing in any of the site's patterns.
    pub fn var_names(&self) -> BTreeSet<String> {
        self.locations.iter().flat_map(|l| l.pattern.var_names().into_iter().map(str::to_owned)).collect()
    }

    /// Expand the site into replicas, enumerating fan-out variables from the medium.
    pub fn expand(&self, identity: &BTreeSet<String>, medium: &dyn Medium) -> Result<Vec<Replica>> {
        let mut out = Vec::new();
        for loc in &self.locations {
            let fanout: BTreeSet<String> = loc.pattern.var_names().into_iter().map(str::to_owned).filter(|v| !identity.contains(v)).collect();
            if fanout.is_empty() {
                out.push(Replica::new(&self.name, loc.label.clone(), Binding::new(), loc.pattern.clone()));
                continue;
            }
            for binding in enumerate_fanout(&loc.pattern, &fanout, medium).with_context(|| format!("expanding site {:?} at {}", self.name, loc.pattern))? {
                let bound = loc.pattern.bind(&binding)?;
                out.push(Replica::new(&self.name, loc.label.clone(), binding, bound));
            }
        }
        Ok(out)
    }
}

/// Walk the segments of a pattern below its root, binding each fan-out variable to the directory names found at its depth. The loader has already checked that fan-out variables come before identity variables, so the walk can stop after the last fan-out segment.
fn enumerate_fanout(pattern: &Pattern, fanout: &BTreeSet<String>, medium: &dyn Medium) -> Result<Vec<Binding>> {
    let last_fanout = pattern.var_segments().iter().filter(|(_, v)| fanout.contains(&v.name)).map(|(d, _)| *d).max().unwrap_or(0);
    let mut frontier: Vec<(PathBuf, Binding)> = vec![(pattern.root().to_path_buf(), Binding::new())];
    for depth in 0..=last_fanout {
        let mut next = Vec::new();
        for (dir, binding) in frontier {
            // A literal segment is appended without listing the directory; a variable segment is matched against each entry name.
            if let Some(name) = pattern.segment_literal(depth) {
                next.push((dir.join(name), binding));
                continue;
            }
            let more_below = depth + 1 < pattern.segments_below_root();
            for entry in medium.list(&dir).with_context(|| format!("listing {}", dir.display()))? {
                if entry.kind == EntryKind::Other || (more_below && entry.kind != EntryKind::Dir) {
                    continue;
                }
                if let Some(Some((name, value))) = pattern.match_segment(depth, &entry.name) {
                    let mut b = binding.clone();
                    b.insert(name, value);
                    next.push((dir.join(&entry.name), b));
                }
            }
        }
        frontier = next;
    }
    Ok(frontier.into_iter().map(|(_, b)| b).collect())
}

/// A concrete location. It records the site, a label when the site listed several patterns, the fan-out binding, and the pattern with that binding applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replica {
    pub site: String,
    pub label: Option<String>,
    pub fanout: Binding,
    /// The pattern after the fan-out variables are bound, so only identity variables remain in it.
    pub pattern: Pattern,
    /// The stable identity under which records are kept: the site, then `[label]`, then `[name=value, ...]` as applicable.
    id: String,
}

impl Replica {
    /// Build a replica and compute its identity once.
    pub fn new(site: &str, label: Option<String>, fanout: Binding, pattern: Pattern) -> Replica {
        let mut id = site.to_owned();
        if let Some(l) = &label {
            id.push_str(&format!("[{l}]"));
        }
        if !fanout.is_empty() {
            let pairs: Vec<String> = fanout.iter().map(|(k, v)| format!("{k}={v}")).collect();
            id.push_str(&format!("[{}]", pairs.join(", ")));
        }
        Replica { site: site.to_owned(), label, fanout, pattern, id }
    }

    /// The identity text.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The directory holding the replica's files, or the parent directory of its single file.
    pub fn root(&self) -> &Path {
        self.pattern.root()
    }

    /// The path of a key at this replica.
    pub fn locate(&self, key: &Key) -> Result<PathBuf> {
        self.pattern.substitute(key.binding())
    }

    /// The key of a path under this replica, when the path matches the pattern and its constraints.
    pub fn identify(&self, path: &Path, identity: &BTreeSet<String>) -> Option<Key> {
        self.pattern.matches(path).map(|b| Key::from_binding(&b, identity))
    }

    /// The qualifier shown after the site name: the fan-out values when there are any, otherwise the label, which may be absent.
    pub fn qualifier(&self) -> Option<String> {
        if !self.fanout.is_empty() {
            Some(self.fanout.values().cloned().collect::<Vec<_>>().join(", "))
        } else {
            self.label.clone()
        }
    }
}

/// Display names for a list of replicas. A site with a single replica shows as its bare name. Otherwise the name is `site (qualifier)`, falling back to a path suffix when several siblings share a qualifier.
pub fn display_names(replicas: &[Replica]) -> Vec<String> {
    replicas.iter().map(|r| {
        let siblings: Vec<&Replica> = replicas.iter().filter(|o| o.site == r.site).collect();
        if siblings.len() == 1 {
            return r.site.clone();
        }
        if let Some(q) = r.qualifier() {
            if siblings.iter().filter(|o| o.qualifier().as_deref() == Some(q.as_str())).count() == 1 {
                return format!("{} ({q})", r.site);
            }
        }
        // Grow a path suffix one component at a time until no sibling shares it.
        let comps = |x: &Replica| -> Vec<String> { x.pattern.footprint().iter().map(|c| c.to_string_lossy().into_owned()).collect() };
        let own = comps(r);
        let others: Vec<Vec<String>> = siblings.iter().map(|o| comps(o)).collect();
        let mut k = 1;
        while k < own.len() {
            let suffix = &own[own.len() - k..];
            if others.iter().filter(|c| c.len() >= k && &c[c.len() - k..] == suffix).count() == 1 {
                break;
            }
            k += 1;
        }
        format!("{} ({})", r.site, own[own.len() - k.min(own.len())..].join("/"))
    }).collect()
}

/// Check the variables of a relation against the identity and fan-out sets derived by the loader. Each pattern of a site must carry the full set of identity variables; a fan-out variable must match a single segment and must come before any identity variable.
pub fn validate_vars(sites: &[Site], identity: &BTreeSet<String>, fanout: &BTreeSet<String>) -> Result<()> {
    for site in sites {
        for loc in &site.locations {
            let own: BTreeSet<String> = loc.pattern.var_names().into_iter().map(str::to_owned).collect();
            let missing: Vec<&String> = identity.iter().filter(|v| !own.contains(*v)).collect();
            if !missing.is_empty() {
                bail!("site {:?}: pattern {} lacks identity variable(s) {:?}, which its sibling patterns have", site.name, loc.pattern, missing);
            }
            for var in loc.pattern.vars() {
                if fanout.contains(&var.name) && var.multi {
                    bail!("site {:?}: fan-out variable {{{}...}} must match a single segment, since replicas are enumerated from directory entries", site.name, var.name);
                }
            }
            // Fan-out variables come before identity variables, so a replica's root is a directory above its keys.
            let segs = loc.pattern.var_segments();
            let first_identity = segs.iter().filter(|(_, v)| identity.contains(&v.name)).map(|(d, _)| *d).min();
            let last_fanout = segs.iter().filter(|(_, v)| fanout.contains(&v.name)).map(|(d, _)| *d).max();
            if let (Some(fi), Some(lf)) = (first_identity, last_fanout) {
                if lf > fi {
                    bail!("site {:?}: pattern {} has a fan-out variable after an identity variable; fan-out variables must come first", site.name, loc.pattern);
                }
            }
        }
    }
    Ok(())
}

/// Walk a replica and collect the keys of its files. The walk visits regular files under the root down to the pattern's depth, drops any whose tail matches an ignore glob, and matches the rest against the pattern.
pub fn discover_keys(replica: &Replica, identity: &BTreeSet<String>, ignore: &[globset::GlobMatcher], medium: &dyn Medium) -> Result<Discovery> {
    let mut found = Discovery::default();
    let root = replica.root();
    let max_depth = replica.pattern.depth();
    if max_depth == Some(0) {
        return Ok(found);
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        let depth = rel.components().count();
        let entries = match medium.list(&dir) {
            Ok(e) => e,
            Err(e) if depth == 0 => return Err(e).with_context(|| format!("walking {}", dir.display())),
            Err(e) => {
                found.failures.push(format!("skipped {}: {e}", dir.display()));
                continue;
            }
        };
        for entry in entries {
            let child = rel.join(&entry.name);
            match entry.kind {
                EntryKind::Dir => {
                    // Files inside this subdirectory lie at depth+2, so descend only when depth+2 is at most max_depth.
                    if max_depth.is_none_or(|max| depth + 1 < max) {
                        stack.push(child);
                    }
                }
                EntryKind::File if !crate::medium::is_own_file(&entry.name) => {
                    let tail = child.to_string_lossy().into_owned();
                    if ignore.iter().any(|g| g.is_match(&tail)) {
                        continue;
                    }
                    if let Some(key) = replica.identify(&root.join(&child), identity) {
                        found.keys.insert(key);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(found)
}

/// Keys found under a replica, along with a message for each unreadable subfolder.
#[derive(Default, Debug)]
pub struct Discovery {
    pub keys: BTreeSet<Key>,
    pub failures: Vec<String>,
}

/// Compile ignore globs, each matched against the tail of a path with `*` confined to one segment.
pub fn compile_ignore(globs: &[String]) -> Result<Vec<globset::GlobMatcher>> {
    globs.iter().map(|g| {
        Ok(globset::GlobBuilder::new(g).literal_separator(true).build().with_context(|| format!("bad ignore glob {g:?}"))?.compile_matcher())
    }).collect()
}

/// Map replica ids to replicas for quick lookup.
pub fn by_id(replicas: &[Replica]) -> BTreeMap<&str, &Replica> {
    replicas.iter().map(|r| (r.id(), r)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::medium::Local;
    use std::fs;
    use tempfile::TempDir;

    fn site(name: &str, text: &str) -> Site {
        Site::single(name, Pattern::parse(text).unwrap())
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keys_round_trip_and_display() {
        let b: Binding = [("path", "docs/a.md"), ("project", "x")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let key = Key::from_binding(&b, &set(&["path"]));
        assert_eq!(key.to_string(), "docs/a.md");
        assert_eq!(Key::parse(&key.canonical()), key);
        assert_eq!(Key::empty().canonical(), "");
        assert_eq!(Key::parse(""), Key::empty());
        assert_eq!(Key::empty().to_string(), "(the file)");
        let two = Key::from_binding(&b, &set(&["path", "project"]));
        assert_eq!(two.to_string(), "path=docs/a.md, project=x");
        assert_eq!(Key::parse(&two.canonical()), two);
    }

    #[test]
    fn validate_vars_checks_consistency_arity_and_order() {
        let sites = vec![site("source", "/u/rules/main.md"), site("dest", "/u/Projects/{project}/docs/style/main.md")];
        assert!(validate_vars(&sites, &set(&[]), &set(&["project"])).is_ok());
        let bad = vec![site("a", "/x/{name}/{project}/f.md"), site("b", "/y/{name}.md")];
        assert!(validate_vars(&bad, &set(&["name"]), &set(&["project"])).unwrap_err().to_string().contains("fan-out variable after"));
        let multi = vec![site("a", "/x/{sub...}/f.md"), site("b", "/y/f.md")];
        assert!(validate_vars(&multi, &set(&[]), &set(&["sub"])).unwrap_err().to_string().contains("single segment"));
        let mixed = Site { name: "d".into(), locations: vec![
            Location { label: None, pattern: Pattern::parse("/p/{path...}").unwrap() },
            Location { label: None, pattern: Pattern::parse("/q/fixed.md").unwrap() },
        ] };
        assert!(validate_vars(&[site("s", "/s/{path...}"), mixed], &set(&["path"]), &set(&[])).unwrap_err().to_string().contains("lacks identity variable"));
    }

    #[test]
    fn expansion_enumerates_fanout_directories() {
        let tmp = TempDir::new().unwrap();
        let projects = tmp.path().join("Projects");
        for p in ["alpha", "beta", ".git"] {
            fs::create_dir_all(projects.join(p).join("docs")).unwrap();
        }
        fs::write(projects.join("stray.txt"), "").unwrap();
        let text = format!("{}/{{project: [a-z]*}}/docs/{{path...}}", projects.display());
        let dest = site("dest", &text);
        let replicas = dest.expand(&set(&["path"]), &Local).unwrap();
        let ids: Vec<&str> = replicas.iter().map(|r| r.id()).collect();
        assert_eq!(ids, vec!["dest[project=alpha]", "dest[project=beta]"]);
        assert_eq!(replicas[0].root(), projects.join("alpha/docs"));
        let key = Key::parse("path=guide.md");
        assert_eq!(replicas[1].locate(&key).unwrap(), projects.join("beta/docs/guide.md"));
        assert_eq!(replicas[1].identify(&projects.join("beta/docs/guide.md"), &set(&["path"])), Some(key));
        assert_eq!(display_names(&replicas), vec!["dest (alpha)", "dest (beta)"]);
        // A site without fan-out variables expands to a single replica, named by the bare site name.
        let plain = site("source", "/u/rules/");
        let one = plain.expand(&set(&["path"]), &Local).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].id(), "source");
        assert_eq!(display_names(&one), vec!["source"]);
    }

    #[test]
    fn list_locations_get_labels_and_unique_names() {
        let dest = Site { name: "dest".into(), locations: vec![
            Location { label: Some("~/x/rules".into()), pattern: Pattern::parse("/u/x/rules/").unwrap() },
            Location { label: Some("~/y/rules".into()), pattern: Pattern::parse("/u/y/rules/").unwrap() },
        ] };
        let replicas = dest.expand(&set(&["path"]), &Local).unwrap();
        assert_eq!(replicas.iter().map(|r| r.id()).collect::<Vec<_>>(), vec!["dest[~/x/rules]", "dest[~/y/rules]"]);
        assert_eq!(display_names(&replicas), vec!["dest (~/x/rules)", "dest (~/y/rules)"]);
        let unlabeled: Vec<Replica> = replicas.iter().map(|r| Replica::new("dest", None, Binding::new(), r.pattern.clone())).collect();
        assert_eq!(display_names(&unlabeled), vec!["dest (x/rules)", "dest (y/rules)"]);
    }

    #[test]
    fn discovery_respects_depth_ignore_and_own_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("top.md"), "").unwrap();
        fs::write(root.join(".DS_Store"), "").unwrap();
        fs::write(root.join("x.replica.tmp"), "").unwrap();
        fs::create_dir_all(root.join("docs/sub")).unwrap();
        fs::write(root.join("docs/mid.md"), "").unwrap();
        fs::write(root.join("docs/sub/deep.md"), "").unwrap();
        let identity = set(&["path"]);
        let ignore = compile_ignore(&["**/.DS_Store".into(), ".DS_Store".into()]).unwrap();
        let names = |r: &Replica| -> Vec<String> {
            discover_keys(r, &identity, &ignore, &Local).unwrap().keys.iter().map(|k| k.to_string()).collect()
        };
        let all = Replica::new("s", None, Binding::new(), Pattern::parse(&format!("{}/", root.display())).unwrap());
        assert_eq!(names(&all), vec!["docs/mid.md", "docs/sub/deep.md", "top.md"]);
        let one = Replica::new("s", None, Binding::new(), Pattern::parse(&format!("{}/{{path...: *.md}}", root.display())).unwrap());
        assert_eq!(names(&one), vec!["top.md"]);
        let two = Replica::new("s", None, Binding::new(), Pattern::parse(&format!("{}/{{path...: docs/*.md}}", root.display())).unwrap());
        assert_eq!(names(&two), vec!["docs/mid.md"]);
        let absent = Replica::new("s", None, Binding::new(), Pattern::parse(&format!("{}/absent/", root.display())).unwrap());
        assert!(names(&absent).is_empty());
    }
}
