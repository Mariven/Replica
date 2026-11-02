//! Sites, keys, and replicas. A relation refers to its sites by name. This module expands each site into replicas on disk and works out which files at different replicas are copies of each other.
//!
//! A site lists one or more patterns. The variables that appear on all the sites of a relation are its identity variables, and their values make up a file's key. When a variable appears on only some of the sites, each of those sites becomes several replicas, one for each set of values found on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::medium::{EntryKind, Medium};
use crate::pattern::{Binding, Pattern, describe_binding};

/// Separates the `name=value` pairs in a key's canonical text. It is a control character, which is safe to use because the values are paths.
const KEY_SEP: char = '\u{1f}';

/// The identity of a file within a relation: the values of its identity variables.
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

    /// The identity variables and their values.
    pub fn binding(&self) -> &Binding {
        &self.0
    }

    /// The stable text stored in the database: `name=value` pairs in name order.
    pub fn canonical(&self) -> String {
        self.0.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(&KEY_SEP.to_string())
    }

    /// Parse a key from its canonical text.
    pub fn parse(text: &str) -> Key {
        if text.is_empty() {
            return Key::empty();
        }
        Key(text.split(KEY_SEP).filter_map(|pair| pair.split_once('=')).map(|(k, v)| (k.to_owned(), v.to_owned())).collect())
    }
}

impl fmt::Display for Key {
    /// Show the value alone if the key has one variable, and `name=value` pairs if it has more. The empty key shows as `(the file)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() { f.write_str("(the file)") } else { f.write_str(&describe_binding(&self.0)) }
    }
}

/// A pattern of a site, with a label if the site lists several patterns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// If the site lists several patterns, the map key or the pattern text as written.
    pub label: Option<String>,
    pub pattern: Pattern,
}

/// A site of a relation, with the patterns listed for it.
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

    /// The replicas of the site. A pattern with fan-out variables gives a replica for each set of values found on disk.
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

/// Find the values of the fan-out variables by listing directories below the pattern's root, a segment at a time. The loader checks that fan-out variables come before identity variables, so the walk stops after the last fan-out segment.
fn enumerate_fanout(pattern: &Pattern, fanout: &BTreeSet<String>, medium: &dyn Medium) -> Result<Vec<Binding>> {
    let last_fanout = pattern.var_segments().iter().filter(|(_, v)| fanout.contains(&v.name)).map(|(d, _)| *d).max().unwrap_or(0);
    let mut frontier: Vec<(PathBuf, Binding)> = vec![(pattern.root().to_path_buf(), Binding::new())];
    for depth in 0..=last_fanout {
        let mut next = Vec::new();
        for (dir, binding) in frontier {
            // A literal segment is appended to the path without listing the directory. A variable segment is matched against each entry in it.
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

/// A location of a site on disk. It has the site name, a label if the site lists several patterns, the fan-out values, and the pattern with those values filled in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replica {
    pub site: String,
    pub label: Option<String>,
    pub fanout: Binding,
    /// The pattern with the fan-out values filled in, leaving only identity variables.
    pub pattern: Pattern,
    /// The id the database keeps records under: the site name, followed by `[label]` and `[name=value, ...]` when they apply.
    id: String,
}

impl Replica {
    /// Build a replica and compute its id.
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

    /// The id the database keeps this replica's records under.
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

    /// The key of a path, or None if the path does not match the replica's pattern.
    pub fn identify(&self, path: &Path, identity: &BTreeSet<String>) -> Option<Key> {
        self.pattern.matches(path).map(|b| Key::from_binding(&b, identity))
    }

    /// The text shown after the site name: the fan-out values if there are any, otherwise the label if there is one.
    pub fn qualifier(&self) -> Option<String> {
        if !self.fanout.is_empty() {
            Some(self.fanout.values().cloned().collect::<Vec<_>>().join(", "))
        } else {
            self.label.clone()
        }
    }
}

/// Names to show for a list of replicas. A site with one replica shows as the site name alone. Otherwise a replica shows as `site (qualifier)`, or as `site (path suffix)` if it has no qualifier or shares it with another replica of the site.
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
        // Lengthen the path suffix a component at a time until no other replica of the site ends the same way.
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

/// Check the variables of a relation against the identity and fan-out sets from the loader. Each pattern must have all the identity variables. A fan-out variable must match a single segment and come before any identity variable.
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
            // Fan-out variables must come first so that a replica's root is a directory above all of its files.
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

/// Walk a replica and collect the keys of its files. The walk goes down to the pattern's depth, skips files whose tail matches an ignore glob, and matches the other regular files against the pattern.
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
                    // Files in this subdirectory are depth + 2 segments below the root, so descend only if that is within max_depth.
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

/// Compile the ignore globs. Each is matched against the tail of a path, with `*` staying within a segment.
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
        // A site without fan-out variables has a single replica, shown by the site name alone.
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
