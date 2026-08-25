//! Labels, the folders they expand to, and how a shared name becomes a real path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::identifier::Identifier;

/// Turn an identifier into a real path under a ref's root.
/// Mappings have to be invertible. `Fixed` maps many identifiers onto a single filename, so it is only allowed on relations that have a single identifier.
#[derive(Clone, Debug)]
pub enum Mapping {
    Direct,
    Fixed(String),
}

/// A path with `{name}` holes, e.g. `~/.claude/skills/{name}/SKILL.md`.
/// Each hole matches a single path component. The hole values are the identifier, so two templates with the same holes describe the same files under different layouts.
/// If you fill a template and then match the resulting path, you get the same identifier back.
#[derive(Clone, Debug)]
pub struct PathTemplate {
    pub text: String,
    /// The hole names, sorted. Identifier components follow this order.
    pub holes: Vec<String>,
    segs: Vec<Seg>,
    regex: regex::Regex,
    /// The directory before the first hole. Discovery walks this.
    pub root: PathBuf,
}

/// A literal path fragment or a `{hole}` in a template.
#[derive(Clone, Debug)]
enum Seg {
    Lit(String),
    Hole(String),
}

impl PartialEq for PathTemplate {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}
impl Eq for PathTemplate {}

impl PathTemplate {
    /// Split the template into literal and hole segments, build the match regex, and find the directory to walk.
    /// The string has to start with a literal directory and contain at least one well-formed hole.
    pub fn parse(text: &str) -> Result<PathTemplate> {
        let mut segs = Vec::new();
        let mut rx = String::from("^");
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            let (lit, tail) = rest.split_at(open);
            if !lit.is_empty() {
                rx.push_str(&regex::escape(lit));
                segs.push(Seg::Lit(lit.into()));
            }
            let close = tail.find('}').with_context(|| format!("unclosed {{ in template {text:?}"))?;
            let name = &tail[1..close];
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("bad hole name {name:?} in template {text:?}");
            }
            if segs.iter().any(|s| matches!(s, Seg::Hole(h) if h == name)) {
                bail!("hole {{{name}}} repeats in template {text:?}");
            }
            rx.push_str("([^/]+)");
            segs.push(Seg::Hole(name.into()));
            rest = &tail[close + 1..];
        }
        if !rest.is_empty() {
            rx.push_str(&regex::escape(rest));
            segs.push(Seg::Lit(rest.into()));
        }
        rx.push('$');

        let Some(Seg::Lit(prefix)) = segs.first() else {
            bail!("template {text:?} must start with a literal directory, not a hole");
        };
        if !segs.iter().any(|s| matches!(s, Seg::Hole(_))) {
            bail!("template {text:?} has no {{holes}}; use root: for a plain folder");
        }
        let cut = prefix.rfind('/').with_context(|| format!("template {text:?} needs a directory before its first hole"))?;
        let root = PathBuf::from(&prefix[..cut.max(1)]);

        let mut holes: Vec<String> = segs.iter().filter_map(|s| match s {
            Seg::Hole(h) => Some(h.clone()),
            Seg::Lit(_) => None,
        }).collect();
        holes.sort();
        Ok(PathTemplate { text: text.into(), holes, segs, regex: regex::Regex::new(&rx)?, root })
    }

    /// Fill the holes from the identifier (sorted-hole order) and return the path.
    pub fn substitute(&self, ident: &Identifier) -> PathBuf {
        let mut out = String::new();
        for seg in &self.segs {
            match seg {
                Seg::Lit(l) => out.push_str(l),
                Seg::Hole(h) => {
                    let i = self.holes.iter().position(|n| n == h).unwrap();
                    out.push_str(ident.get(i).map(String::as_str).unwrap_or(""));
                }
            }
        }
        PathBuf::from(out)
    }

    /// If `path` matches this template, pull the identifier out of the hole values.
    pub fn identify(&self, path: &Path) -> Option<Identifier> {
        let text = path.to_string_lossy();
        let caps = self.regex.captures(&text)?;
        let appearance: Vec<&str> = self.segs.iter().filter_map(|s| match s {
            Seg::Hole(h) => Some(h.as_str()),
            Seg::Lit(_) => None,
        }).collect();
        Some(self.holes.iter().map(|name| {
            let i = appearance.iter().position(|n| n == name).unwrap();
            caps.get(i + 1).unwrap().as_str().to_owned()
        }).collect())
    }
}

/// A side of a relation.
/// You pick the name (`source`, `dest`, or anything else). Nothing else in the program treats any name as special.
/// When we expand a label we get the concrete folders (refs) it currently points at.
pub struct Label {
    pub name: String,
    pub shape: Shape,
    pub mapping: Mapping,
}

/// The folders this label expands to. A template uses a whole-path pattern instead.
pub enum Shape {
    Literal { root: PathBuf },
    Many { roots: Vec<PathBuf> },
    /// Each entry of `dir` is substituted for `{hole}` in `template` to make a root.
    Scan { template: String, hole: String, dir: PathBuf },
    /// A whole-path pattern. The hole values are the identifier.
    Template(PathTemplate),
}

/// A concrete folder this label currently points at, plus the variables that produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub label: String,
    pub vars: BTreeMap<String, String>,
    pub root: PathBuf,
    pub mapping_kind: MappingKind,
}

/// The label's mapping, copied onto the ref so it can locate and identify files on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MappingKind {
    Direct,
    Fixed(String),
    Template(PathTemplate),
}

impl From<&Mapping> for MappingKind {
    fn from(m: &Mapping) -> Self {
        match m {
            Mapping::Direct => MappingKind::Direct,
            Mapping::Fixed(leaf) => MappingKind::Fixed(leaf.clone()),
        }
    }
}

impl Label {
    /// Expand this label into the refs it currently points at.
    /// Scan needs a directory listing. The caller supplies it so this function stays pure: the engine passes a real `read_dir`, tests pass a fixed list.
    pub fn refs(&self, list_dir: &dyn Fn(&Path) -> Result<Vec<String>>) -> Result<Vec<Ref>> {
        let kind = MappingKind::from(&self.mapping);
        let rf = |vars: BTreeMap<String, String>, root: PathBuf| Ref {
            label: self.name.clone(),
            vars,
            root,
            mapping_kind: kind.clone(),
        };
        Ok(match &self.shape {
            Shape::Template(t) => vec![Ref {
                label: self.name.clone(),
                vars: BTreeMap::new(),
                root: t.root.clone(),
                mapping_kind: MappingKind::Template(t.clone()),
            }],
            Shape::Literal { root } => vec![rf(BTreeMap::new(), root.clone())],
            // Each root is its own ref; the root itself is the var so ids stay stable under list reordering.
            Shape::Many { roots } => roots.iter()
                .map(|r| rf(BTreeMap::from([("root".into(), r.display().to_string())]), r.clone()))
                .collect(),
            Shape::Scan { template, hole, dir } => {
                let needle = format!("{{{hole}}}");
                if !template.contains(&needle) {
                    bail!("scan template {template:?} does not contain hole {needle:?}");
                }
                list_dir(dir)?
                    .into_iter()
                    .map(|entry| {
                        let root = PathBuf::from(template.replace(&needle, &entry));
                        rf(BTreeMap::from([(hole.clone(), entry)]), root)
                    })
                    .collect()
            }
        })
    }
}

impl Ref {
    /// A stable id for database rows and plan ops: the label name, plus sorted `key=value` bindings if any.
    pub fn id(&self) -> String {
        if self.vars.is_empty() {
            self.label.clone()
        } else {
            let vars: Vec<String> = self.vars.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{}?{}", self.label, vars.join("&"))
        }
    }

    /// Build the path of this identifier under this ref.
    pub fn locate(&self, ident: &Identifier) -> PathBuf {
        match &self.mapping_kind {
            MappingKind::Direct => ident.iter().fold(self.root.clone(), |p, c| p.join(c)),
            MappingKind::Fixed(leaf) => self.root.join(leaf),
            MappingKind::Template(t) => t.substitute(ident),
        }
    }

    /// If this path belongs to this ref, recover its identifier.
    /// Fixed mappings always return `None`: their identifiers come from the other side of the relation.
    pub fn identify(&self, path: &Path) -> Option<Identifier> {
        match &self.mapping_kind {
            MappingKind::Direct => {
                let rel = path.strip_prefix(&self.root).ok()?;
                let ident: Identifier = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                if ident.is_empty() { None } else { Some(ident) }
            }
            MappingKind::Fixed(_) => None,
            MappingKind::Template(t) => t.identify(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identifier::identifier_from_string;

    /// A listing callback that ignores the path and returns these names.
    fn fixed_listing(entries: &[&str]) -> impl Fn(&Path) -> Result<Vec<String>> {
        let entries: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        move |_| Ok(entries.clone())
    }

    #[test]
    fn direct_round_trip() {
        let rf = Ref {
            label: "source".into(),
            vars: BTreeMap::new(),
            root: "/a/b".into(),
            mapping_kind: MappingKind::Direct,
        };
        let ident = identifier_from_string("docs/style/main.md");
        let path = rf.locate(&ident);
        assert_eq!(path, PathBuf::from("/a/b/docs/style/main.md"));
        assert_eq!(rf.identify(&path), Some(ident));
        assert_eq!(rf.identify(Path::new("/elsewhere/x")), None);
    }

    #[test]
    fn fixed_locates_but_never_identifies() {
        let rf = Ref {
            label: "dest".into(),
            vars: BTreeMap::new(),
            root: "/home/desk".into(),
            mapping_kind: MappingKind::Fixed("nginx-conf.txt".into()),
        };
        assert_eq!(rf.locate(&identifier_from_string("default.conf")), PathBuf::from("/home/desk/nginx-conf.txt"));
        assert_eq!(rf.identify(Path::new("/home/desk/nginx-conf.txt")), None);
    }

    #[test]
    fn template_round_trip() {
        let t = PathTemplate::parse("/u/.claude/skills/{name}/SKILL.md").unwrap();
        assert_eq!(t.root, PathBuf::from("/u/.claude/skills"));
        assert_eq!(t.holes, vec!["name".to_string()]);
        assert_eq!(t.substitute(&vec!["prose".into()]), PathBuf::from("/u/.claude/skills/prose/SKILL.md"));
        assert_eq!(t.identify(Path::new("/u/.claude/skills/prose/SKILL.md")), Some(vec!["prose".to_string()]));
        // A hole never crosses `/`, and stray files under root do not match.
        assert_eq!(t.identify(Path::new("/u/.claude/skills/a/b/SKILL.md")), None);
        assert_eq!(t.identify(Path::new("/u/.claude/skills/prose/OTHER.md")), None);

        // The flat side of the same relation.
        let s = PathTemplate::parse("/u/scripts/skills/{name}.md").unwrap();
        assert_eq!(s.identify(Path::new("/u/scripts/skills/prose.md")), Some(vec!["prose".to_string()]));
        assert_eq!(s.substitute(&vec!["prose".into()]), PathBuf::from("/u/scripts/skills/prose.md"));
    }

    #[test]
    fn template_multiple_holes_sorted_by_name() {
        let t = PathTemplate::parse("/logs/{year}/{app}.log").unwrap();
        assert_eq!(t.holes, vec!["app".to_string(), "year".to_string()]);
        let ident = t.identify(Path::new("/logs/2026/replica.log")).unwrap();
        assert_eq!(ident, vec!["replica".to_string(), "2026".to_string()]);
        assert_eq!(t.substitute(&ident), PathBuf::from("/logs/2026/replica.log"));
    }

    #[test]
    fn template_rejects_malformed() {
        assert!(PathTemplate::parse("/a/plain.md").is_err());          // no holes
        assert!(PathTemplate::parse("{name}/x.md").is_err());          // starts with a hole
        assert!(PathTemplate::parse("/a/{name}/{name}.md").is_err());  // repeated hole
        assert!(PathTemplate::parse("/a/{bad name}.md").is_err());     // bad hole name
        assert!(PathTemplate::parse("/a/{oops.md").is_err());          // unclosed
    }

    #[test]
    fn scan_expands_per_entry() {
        let label = Label {
            name: "dest".into(),
            shape: Shape::Scan {
                template: "/p/{project}/docs/style".into(),
                hole: "project".into(),
                dir: "/p".into(),
            },
            mapping: Mapping::Direct,
        };
        let refs = label.refs(&fixed_listing(&["bouncer", "duckmaster"])).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].root, PathBuf::from("/p/bouncer/docs/style"));
        assert_eq!(refs[0].id(), "dest?project=bouncer");
        assert_eq!(refs[1].id(), "dest?project=duckmaster");
    }
}
