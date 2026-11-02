//! Expand relation labels into concrete directory targets and translate identifier components into filesystem paths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::identifier::Identifier;

/// Translate an identifier into a relative path beneath a target root directory.
/// When using `Fixed`, pair the mapping with single-identifier relations to preserve unique path-to-identifier reconstruction.
#[derive(Clone, Debug)]
pub enum Mapping {
    Direct,
    Fixed(String),
}

/// Path pattern containing `{hole}` placeholder segments, such as `~/.claude/skills/{name}/SKILL.md`.
/// Parse incoming paths against the pattern to extract identifier components from placeholder segments.
/// Format outgoing paths by substituting identifier values back into each placeholder.
/// Configure matching placeholder keys across distinct templates to link identical files across different directory layouts.
#[derive(Clone, Debug)]
pub struct PathTemplate {
    pub text: String,
    /// Sorted placeholder keys matching the sequential order of identifier components.
    pub holes: Vec<String>,
    segs: Vec<Seg>,
    regex: regex::Regex,
    /// Base directory preceding the first placeholder segment, scanned during filesystem discovery.
    pub root: PathBuf,
}

/// Parsed template segment representing literal path text or a `{hole}` placeholder.
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
    /// Parse template text into literal and placeholder segments, compile the matching regular expression, and extract the search root directory.
    /// Supply a leading literal directory prefix along with at least one valid placeholder segment.
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

    /// Build a filesystem path by inserting identifier components into template placeholders in sorted order.
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

    /// Extract identifier components from matching placeholder segments within the given path.
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

    /// Count the directory levels below the root at which a matching file sits.
    /// Every hole matches one path segment and every literal is fixed, so the depth is the number of segments after the root.
    pub fn depth(&self) -> usize {
        let rest = &self.text[self.root.as_os_str().len()..];
        rest.trim_start_matches('/').matches('/').count() + 1
    }
}

/// Expand the label to produce concrete directory references during synchronization.
pub struct Label {
    pub name: String,
    pub endpoints: Vec<Endpoint>,
}

pub struct Endpoint {
    pub shape: Shape,
    pub mapping: Mapping,
}

/// Directory layout strategy for expanding an endpoint into target roots or matching whole paths.
pub enum Shape {
    Literal { root: PathBuf },
    /// Scan directory entries within `dir` and substitute each child name into `{hole}` in `template` to generate target roots.
    Scan { template: String, hole: String, dir: PathBuf },
    /// Whole-path pattern extracting identifier components directly from placeholder segments.
    Template(PathTemplate),
}

/// Resolved directory target produced from label expansion, pairing a concrete root path with bound template variables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    pub label: String,
    pub vars: BTreeMap<String, String>,
    pub root: PathBuf,
    pub mapping_kind: MappingKind,
    /// Identifier string computed once at construction: the label, plus sorted `key=value` pairs when vars are present.
    id: String,
}

/// Path resolution strategy attached to a concrete directory reference for locating and identifying files.
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
    /// Expand this label into concrete refs.
    /// For scan endpoints, call `list_dir` on the target directory and substitute each entry into the template path.
    pub fn refs(&self, list_dir: &dyn Fn(&Path) -> Result<Vec<String>>) -> Result<Vec<Ref>> {
        let tag = self.endpoints.len() > 1;
        let mut out = Vec::new();
        for ep in &self.endpoints {
            let kind = MappingKind::from(&ep.mapping);
            // Collect (vars, root, kind) triples, then tag each with its root when the label has several endpoints.
            let found: Vec<(BTreeMap<String, String>, PathBuf, MappingKind)> = match &ep.shape {
                Shape::Template(t) => vec![(BTreeMap::new(), t.root.clone(), MappingKind::Template(t.clone()))],
                Shape::Literal { root } => vec![(BTreeMap::new(), root.clone(), kind.clone())],
                Shape::Scan { template, hole, dir } => {
                    let needle = format!("{{{hole}}}");
                    if !template.contains(&needle) {
                        bail!("scan template {template:?} does not contain hole {needle:?}");
                    }
                    list_dir(dir)?
                        .into_iter()
                        .map(|entry| {
                            let root = PathBuf::from(template.replace(&needle, &entry));
                            (BTreeMap::from([(hole.clone(), entry)]), root, kind.clone())
                        })
                        .collect()
                }
            };
            for (mut vars, root, kind) in found {
                if tag {
                    vars.insert("root".into(), root.display().to_string());
                }
                out.push(Ref::new(self.name.clone(), vars, root, kind));
            }
        }
        Ok(out)
    }
}

impl Ref {
    /// Build a ref and compute its id once.
    pub fn new(label: String, vars: BTreeMap<String, String>, root: PathBuf, mapping_kind: MappingKind) -> Ref {
        let id = if vars.is_empty() {
            label.clone()
        } else {
            let pairs: Vec<String> = vars.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{label}?{}", pairs.join("&"))
        };
        Ref { label, vars, root, mapping_kind, id }
    }

    /// Return the identifier string: the label, with sorted variable pairs appended as query parameters when present.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Return the value bound to a scan hole, if this ref came from a scan endpoint.
    pub fn hole_value(&self) -> Option<&str> {
        self.vars.iter().find(|(k, _)| k.as_str() != "root").map(|(_, v)| v.as_str())
    }

    /// Return the part of the filesystem this ref claims: the root directory, or the single file a fixed mapping names.
    pub fn footprint(&self) -> PathBuf {
        match &self.mapping_kind {
            MappingKind::Fixed(leaf) => self.root.join(leaf),
            _ => self.root.clone(),
        }
    }

    /// Return how many directory levels below the root a walk must descend to find every file this ref can identify, or None for unbounded.
    /// Templates fix the depth themselves; direct refs take it from the relation pattern; fixed refs are never walked.
    pub fn walk_depth(&self, pattern_depth: Option<usize>) -> Option<usize> {
        match &self.mapping_kind {
            MappingKind::Template(t) => Some(t.depth()),
            MappingKind::Direct => pattern_depth,
            MappingKind::Fixed(_) => Some(0),
        }
    }

    /// Resolve an identifier into a filesystem path under the ref root.
    pub fn locate(&self, ident: &Identifier) -> PathBuf {
        match &self.mapping_kind {
            MappingKind::Direct => ident.iter().fold(self.root.clone(), |p, c| p.join(c)),
            MappingKind::Fixed(leaf) => self.root.join(leaf),
            MappingKind::Template(t) => t.substitute(ident),
        }
    }

    /// Extract identifier components from a path under the ref root.
    /// Fixed mappings return None, as callers supply identifiers from the paired endpoint instead.
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

/// Name every ref so that refs sharing a label stay distinguishable: the label alone when unique, `label (hole value)` for scan refs, otherwise `label (shortest unique path suffix)`.
pub fn display_names(refs: &[Ref]) -> Vec<String> {
    refs.iter().map(|rf| {
        let siblings: Vec<&Ref> = refs.iter().filter(|o| o.label == rf.label).collect();
        if siblings.len() == 1 {
            return rf.label.clone();
        }
        // A scan hole value names the ref unless another endpoint of the same label produced the same value.
        if let Some(v) = rf.hole_value() {
            if siblings.iter().filter(|o| o.hole_value() == Some(v)).count() == 1 {
                return format!("{} ({v})", rf.label);
            }
        }
        let comps: Vec<Vec<String>> = siblings.iter()
            .map(|o| o.footprint().iter().map(|c| c.to_string_lossy().into_owned()).collect())
            .collect();
        let own: Vec<String> = rf.footprint().iter().map(|c| c.to_string_lossy().into_owned()).collect();
        // Grow the suffix one component at a time until no sibling shares it.
        let mut k = 1;
        while k < own.len() {
            let suffix = &own[own.len() - k..];
            let shared = comps.iter().filter(|c| c.len() >= k && &c[c.len() - k..] == suffix).count();
            if shared == 1 {
                break;
            }
            k += 1;
        }
        format!("{} ({})", rf.label, own[own.len() - k.min(own.len())..].join("/"))
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identifier::identifier_from_string;

    /// Create a directory listing closure returning the given entries for any path.
    fn fixed_listing(entries: &[&str]) -> impl Fn(&Path) -> Result<Vec<String>> {
        let entries: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        move |_| Ok(entries.clone())
    }

    fn direct(label: &str, root: &str) -> Ref {
        Ref::new(label.into(), BTreeMap::new(), root.into(), MappingKind::Direct)
    }

    #[test]
    fn direct_round_trip() {
        let rf = direct("source", "/a/b");
        let ident = identifier_from_string("docs/style/main.md");
        let path = rf.locate(&ident);
        assert_eq!(path, PathBuf::from("/a/b/docs/style/main.md"));
        assert_eq!(rf.identify(&path), Some(ident));
        assert_eq!(rf.identify(Path::new("/elsewhere/x")), None);
    }

    #[test]
    fn fixed_locates_but_never_identifies() {
        let rf = Ref::new("dest".into(), BTreeMap::new(), "/home/desk".into(), MappingKind::Fixed("nginx-conf.txt".into()));
        assert_eq!(rf.locate(&identifier_from_string("default.conf")), PathBuf::from("/home/desk/nginx-conf.txt"));
        assert_eq!(rf.identify(Path::new("/home/desk/nginx-conf.txt")), None);
        // A fixed ref claims one file, not the folder around it.
        assert_eq!(rf.footprint(), PathBuf::from("/home/desk/nginx-conf.txt"));
        assert_eq!(direct("x", "/home/desk").footprint(), PathBuf::from("/home/desk"));
    }

    #[test]
    fn template_round_trip() {
        let t = PathTemplate::parse("/u/.claude/skills/{name}/SKILL.md").unwrap();
        assert_eq!(t.root, PathBuf::from("/u/.claude/skills"));
        assert_eq!(t.holes, vec!["name".to_string()]);
        assert_eq!(t.substitute(&vec!["prose".into()]), PathBuf::from("/u/.claude/skills/prose/SKILL.md"));
        assert_eq!(t.identify(Path::new("/u/.claude/skills/prose/SKILL.md")), Some(vec!["prose".to_string()]));
        // Match holes within single path segments only. Nested paths and mismatched filenames return None.
        assert_eq!(t.identify(Path::new("/u/.claude/skills/a/b/SKILL.md")), None);
        assert_eq!(t.identify(Path::new("/u/.claude/skills/prose/OTHER.md")), None);
        assert_eq!(t.depth(), 2);

        // Match and substitute against a template with a file extension.
        let s = PathTemplate::parse("/u/scripts/skills/{name}.md").unwrap();
        assert_eq!(s.identify(Path::new("/u/scripts/skills/prose.md")), Some(vec!["prose".to_string()]));
        assert_eq!(s.substitute(&vec!["prose".into()]), PathBuf::from("/u/scripts/skills/prose.md"));
        assert_eq!(s.depth(), 1);
        assert_eq!(PathTemplate::parse("/{name}.md").unwrap().depth(), 1);
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
        assert!(PathTemplate::parse("/a/plain.md").is_err());  // path without hole definitions
        assert!(PathTemplate::parse("{name}/x.md").is_err());  // variable preceding root prefix
        assert!(PathTemplate::parse("/a/{name}/{name}.md").is_err());  // duplicate variable name
        assert!(PathTemplate::parse("/a/{bad name}.md").is_err());  // invalid characters in variable name
        assert!(PathTemplate::parse("/a/{oops.md").is_err());  // unclosed delimiter
    }

    #[test]
    fn scan_expands_per_entry() {
        let scan = Endpoint {
            shape: Shape::Scan {
                template: "/p/{project}/docs/style".into(),
                hole: "project".into(),
                dir: "/p".into(),
            },
            mapping: Mapping::Direct,
        };
        let label = Label { name: "dest".into(), endpoints: vec![scan] };
        let refs = label.refs(&fixed_listing(&["bouncer", "duckmaster"])).unwrap();
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].root, PathBuf::from("/p/bouncer/docs/style"));
        assert_eq!(refs[0].id(), "dest?project=bouncer");
        assert_eq!(refs[1].id(), "dest?project=duckmaster");
        assert_eq!(refs[0].hole_value(), Some("bouncer"));
    }

    #[test]
    fn several_endpoints_tag_refs_with_their_root() {
        let lit = |root: &str| Endpoint { shape: Shape::Literal { root: root.into() }, mapping: Mapping::Direct };
        let one = Label { name: "dest".into(), endpoints: vec![lit("/x")] };
        assert_eq!(one.refs(&fixed_listing(&[])).unwrap()[0].id(), "dest");
        let two = Label { name: "dest".into(), endpoints: vec![lit("/x"), lit("/y")] };
        let ids: Vec<String> = two.refs(&fixed_listing(&[])).unwrap().iter().map(|r| r.id().to_owned()).collect();
        assert_eq!(ids, vec!["dest?root=/x", "dest?root=/y"]);
    }

    #[test]
    fn display_names_use_hole_values_or_shortest_unique_suffix() {
        let source = direct("source", "/u/rules");
        let a = direct("dest", "/u/Projects/alpha/docs/style");
        let b = direct("dest", "/u/Projects/beta/docs/style");
        let names = display_names(&[source.clone(), a, b]);
        assert_eq!(names, vec!["source", "dest (alpha/docs/style)", "dest (beta/docs/style)"]);

        let scan_a = Ref::new("dest".into(), BTreeMap::from([("project".to_string(), "alpha".to_string())]), "/u/Projects/alpha/docs/style".into(), MappingKind::Direct);
        let scan_b = Ref::new("dest".into(), BTreeMap::from([("project".to_string(), "beta".to_string())]), "/u/Projects/beta/docs/style".into(), MappingKind::Direct);
        assert_eq!(display_names(&[source.clone(), scan_a, scan_b]), vec!["source", "dest (alpha)", "dest (beta)"]);

        let x = direct("dest", "/u/x/rules");
        let y = direct("dest", "/u/y/rules");
        assert_eq!(display_names(&[source.clone(), x, y]), vec!["source", "dest (x/rules)", "dest (y/rules)"]);

        // Two scan endpoints under one label yielding the same hole value fall back to the path suffix.
        let a1 = Ref::new("dest".into(), BTreeMap::from([("p".to_string(), "alpha".to_string())]), "/u/A/alpha/docs".into(), MappingKind::Direct);
        let a2 = Ref::new("dest".into(), BTreeMap::from([("p".to_string(), "alpha".to_string())]), "/u/B/alpha/docs".into(), MappingKind::Direct);
        assert_eq!(display_names(&[source, a1, a2]), vec!["source", "dest (A/alpha/docs)", "dest (B/alpha/docs)"]);
    }

    #[test]
    fn walk_depth_comes_from_the_template_or_the_pattern() {
        let t = PathTemplate::parse("/u/.claude/skills/{name}/SKILL.md").unwrap();
        let tr = Ref::new("dest".into(), BTreeMap::new(), t.root.clone(), MappingKind::Template(t));
        assert_eq!(tr.walk_depth(None), Some(2));
        assert_eq!(direct("x", "/a").walk_depth(Some(1)), Some(1));
        assert_eq!(direct("x", "/a").walk_depth(None), None);
    }
}
