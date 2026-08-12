use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::engine::Relation;
use crate::identifier::Pattern;
use crate::policy::{ConflictPolicy, ContentPolicy, DeletePolicy, NewPolicy, Policy};
use crate::label::{Mapping as PathMapping, Label, PathTemplate, Shape};

// The user-facing YAML surface. `entries` is a mapping from entry name to entry body;
// label names are arbitrary: any key that is not one of the reserved fields (pattern,
// content, conflict, on_new, on_delete, sync_times, log) declares a label.
// `source`/`dest` are conventions, not keywords; `mirror(x)` / `prefer(x)` may name any
// declared label. Policies are a typed configuration AST, no expression parsing.
//
//     state_db: "~/scripts/replica/live/state.db"
//     interval: "30s"
//     entries:
//         mylibrary:
//             source: "~/Desktop/mylibrary"
//             dest: "~/scripts/desktop-library"
//             pattern: "*.js"
//             content: "bidirectional"        # or: "mirror(source)"
//             conflict: "newer"               # or: "skip" (default), "prefer(source)"
//             on_new: "restrict"              # or: "replicate"
//             on_delete: "restore"            # or: "propagate", "skip"
//             sync_times: true
//             log: "~/scripts/replica/live/logs/mylibrary.log"
//
// An endpoint is a bare path string ("~/x" — a plain root), a whole-path template with
// {name} holes ("~/x/{name}.md" — see PathTemplate), or a mapping for the longer forms
// (root + rename, many, scan).
//
// The special entry `_` holds defaults merged into every other entry where the entry has
// no value of its own. Default keys and string values may contain {placeholders}, resolved
// against the entry's own scalar string fields plus {name}, the entry's key:
//
//     entries:
//         _:
//             content: "bidirectional"
//             log: "~/scripts/replica/live/logs/{name}.log"
//         skills:
//             source: "~/scripts/skills/{name}.md"      # entry values are never rewritten
//             dest: "~/.claude/skills/{name}/SKILL.md"
//
// Expansion is innermost-first and repeats until stable, so defaults can build
// placeholders out of other fields; a reference cycle is an error. A placeholder that
// resolves to nothing stays literal.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub state_db: Option<String>,
    #[serde(default = "default_interval")]
    pub interval: String,
    #[serde(default)]
    pub dashboard: Option<DashboardConfig>,
    pub entries: Mapping,
}

fn default_interval() -> String { "30s".into() }

// What main() consumes; carried out of the config file verbatim.
pub struct Settings {
    pub state_db: Option<String>,
    pub interval: String,
    pub dashboard: Option<DashboardConfig>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    pub port: u16,
    // How many events the dashboard remembers.
    #[serde(default = "default_recent_events")]
    pub recent_events: usize,
    // Seconds between page auto-refreshes.
    #[serde(default = "default_refresh")]
    pub refresh: u64,
}

fn default_recent_events() -> usize { 200 }
fn default_refresh() -> u64 { 10 }

#[derive(Deserialize)]
pub struct EntryConfig {
    #[serde(flatten)]
    pub labels: BTreeMap<String, EndpointSpec>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default = "default_content")]
    pub content: String,
    #[serde(default = "default_conflict")]
    pub conflict: String,
    #[serde(default = "default_on_new")]
    pub on_new: String,
    #[serde(default = "default_on_delete")]
    pub on_delete: String,
    #[serde(default = "default_true")]
    pub sync_times: bool,
    #[serde(default)]
    pub log: Option<String>,
}

fn default_content() -> String { "bidirectional".into() }
fn default_conflict() -> String { "skip".into() }
fn default_on_new() -> String { "restrict".into() }
fn default_on_delete() -> String { "restore".into() }
fn default_true() -> bool { true }

// An endpoint: a bare path string (root, or whole-path template when it has {holes}),
// or the longer mapping forms.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum EndpointSpec {
    Short(String),
    Full(EndpointConfig),
}

// One of: a single root (optionally with a fixed-name rename), many roots, a scan,
// or a whole-path template.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub rename: Option<String>,
    #[serde(default)]
    pub many: Option<Vec<String>>,
    #[serde(default)]
    pub scan: Option<ScanConfig>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Deserialize)]
pub struct ScanConfig {
    pub template: String,
    pub hole: String,
    pub dir: String,
}

pub fn expand_path(s: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(s).into_owned())
}

// "mirror(source)" / "prefer(dest)" style single-argument forms.
fn parse_call<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    s.strip_prefix(name)?.strip_prefix('(')?.strip_suffix(')')
}

pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().with_context(|| format!("bad duration {s:?}"))?;
    Ok(match unit {
        "s" | "" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "ms" => Duration::from_millis(n),
        _ => bail!("bad duration unit {unit:?} in {s:?}"),
    })
}

impl EndpointSpec {
    fn into_label(self, name: &str) -> Result<Label> {
        match self {
            EndpointSpec::Short(s) => {
                if !(s.starts_with('~') || s.starts_with('/')) {
                    bail!("endpoint {name:?}: {s:?} does not look like a path (must start with ~ or /)");
                }
                let shape = if s.contains('{') {
                    Shape::Template(PathTemplate::parse(&shellexpand::tilde(&s))
                        .with_context(|| format!("endpoint {name:?}"))?)
                } else {
                    Shape::Literal { root: expand_path(&s) }
                };
                Ok(Label { name: name.into(), shape, mapping: PathMapping::Direct })
            }
            EndpointSpec::Full(ep) => ep.into_label(name),
        }
    }
}

impl EndpointConfig {
    fn into_label(self, name: &str) -> Result<Label> {
        let mapping = match &self.rename {
            Some(leaf) => PathMapping::Fixed(leaf.clone()),
            None => PathMapping::Direct,
        };
        let shape = match (self.root, self.many, self.scan, self.path) {
            (Some(root), None, None, None) => Shape::Literal { root: expand_path(&root) },
            (None, Some(roots), None, None) => Shape::Many { roots: roots.iter().map(|r| expand_path(r)).collect() },
            (None, None, Some(scan), None) => Shape::Scan {
                template: shellexpand::tilde(&scan.template).into_owned(),
                hole: scan.hole,
                dir: expand_path(&scan.dir),
            },
            (None, None, None, Some(path)) => {
                if matches!(mapping, PathMapping::Fixed(_)) {
                    bail!("endpoint {name:?}: rename does not combine with path (the template already names the file)");
                }
                Shape::Template(PathTemplate::parse(&shellexpand::tilde(&path))
                    .with_context(|| format!("endpoint {name:?}"))?)
            }
            _ => bail!("endpoint {name:?} must have exactly one of root / many / scan / path"),
        };
        Ok(Label { name: name.into(), shape, mapping })
    }
}

impl EntryConfig {
    pub fn into_relation(self, name: &str) -> Result<Relation> {
        let content = if self.content == "bidirectional" {
            ContentPolicy::Bidirectional
        } else if let Some(label) = parse_call(&self.content, "mirror") {
            ContentPolicy::Mirror(label.into())
        } else {
            bail!("entry {name:?}: unknown content policy {:?}", self.content)
        };
        let conflict = match self.conflict.as_str() {
            "skip" => ConflictPolicy::Skip,
            "newer" => ConflictPolicy::Newer,
            other => match parse_call(other, "prefer") {
                Some(label) => ConflictPolicy::Prefer(label.into()),
                None => bail!("entry {name:?}: unknown conflict policy {other:?}"),
            },
        };
        let on_new = match self.on_new.as_str() {
            "restrict" => NewPolicy::Restrict,
            "replicate" => NewPolicy::Replicate,
            other => bail!("entry {name:?}: unknown on_new {other:?}"),
        };
        let on_delete = match self.on_delete.as_str() {
            "restore" => DeletePolicy::Restore,
            "propagate" => DeletePolicy::Propagate,
            "skip" => DeletePolicy::Skip,
            other => bail!("entry {name:?}: unknown on_delete {other:?}"),
        };
        let pattern = match &self.pattern {
            None => Pattern::All,
            Some(p) => Pattern::glob(p).with_context(|| format!("entry {name:?}: bad pattern {p:?}"))?,
        };
        if self.labels.len() < 2 {
            bail!("entry {name:?} declares {} label(s); at least two are needed to relate anything", self.labels.len());
        }
        // A policy that names a label must name a declared one.
        for named in [content_label(&content), conflict_label(&conflict)].into_iter().flatten() {
            if !self.labels.contains_key(named) {
                bail!("entry {name:?}: policy references label {named:?}, which is not declared");
            }
        }
        let labels = self.labels.into_iter()
            .map(|(label_name, ep)| ep.into_label(&label_name))
            .collect::<Result<Vec<_>>>()?;
        // Templates carry identity in their hole values, so a template label can only
        // relate to other templates, and all must agree on the hole set.
        if let Some(first) = labels.iter().find_map(|l| match &l.shape {
            Shape::Template(t) => Some((l.name.as_str(), &t.holes)),
            _ => None,
        }) {
            for l in &labels {
                match &l.shape {
                    Shape::Template(t) if t.holes == *first.1 => {}
                    Shape::Template(t) => bail!(
                        "entry {name:?}: labels {:?} and {:?} disagree on holes ({:?} vs {:?})",
                        first.0, l.name, first.1, t.holes
                    ),
                    _ => bail!(
                        "entry {name:?}: label {:?} must also be a path template because label {:?} is",
                        l.name, first.0
                    ),
                }
            }
        }
        Ok(Relation {
            name: name.into(),
            labels,
            pattern,
            policy: Policy { content, conflict, on_new, on_delete },
            sync_times: self.sync_times,
        })
    }
}

fn content_label(c: &ContentPolicy) -> Option<&str> {
    match c {
        ContentPolicy::Mirror(l) => Some(l),
        ContentPolicy::Bidirectional => None,
    }
}

fn conflict_label(c: &ConflictPolicy) -> Option<&str> {
    match c {
        ConflictPolicy::Prefer(l) => Some(l),
        _ => None,
    }
}

// ---- defaults merge and {placeholder} expansion ----

const MAX_EXPANSION_PASSES: usize = 8;

// One expansion pass: replace each innermost {word} whose word is in scope; anything
// unresolvable stays literal. Innermost-first is what lets values assemble new
// placeholders out of other fields.
fn expand_once(s: &str, scope: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c == '}' {
            // Backtrack in the output for the nearest open brace; substituted text
            // participates, which is what lets values assemble new placeholders.
            if let Some(open) = out.rfind('{') {
                if let Some(v) = scope.get(&out[open + 1..]) {
                    let v = v.clone();
                    out.truncate(open);
                    out.push_str(&v);
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}

fn expand_str(s: &str, scope: &BTreeMap<String, String>) -> Result<String> {
    let mut cur = s.to_string();
    for _ in 0..MAX_EXPANSION_PASSES {
        let next = expand_once(&cur, scope);
        if next == cur {
            return Ok(cur);
        }
        cur = next;
    }
    bail!("circular reference expanding {s:?}")
}

fn expand_yaml(v: &Value, scope: &BTreeMap<String, String>) -> Result<Value> {
    Ok(match v {
        Value::String(s) => Value::String(expand_str(s, scope)?),
        Value::Mapping(m) => {
            let mut out = Mapping::new();
            for (k, val) in m {
                let k = match k.as_str() {
                    Some(s) => Value::String(expand_str(s, scope)?),
                    None => k.clone(),
                };
                out.insert(k, expand_yaml(val, scope)?);
            }
            Value::Mapping(out)
        }
        Value::Sequence(seq) => Value::Sequence(seq.iter().map(|v| expand_yaml(v, scope)).collect::<Result<_>>()?),
        other => other.clone(),
    })
}

// Merge `_` defaults into one entry. Local values win; default keys and values are
// expanded against the entry's own scalar string fields plus {name}, the entry's key.
// Entry values themselves are never rewritten.
fn merge_defaults(name: &str, entry: &mut Mapping, defaults: &Mapping) -> Result<()> {
    let mut scope: BTreeMap<String, String> = entry.iter()
        .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), v.as_str()?.to_owned())))
        .collect();
    scope.insert("name".into(), name.to_owned());
    for (k, v) in defaults {
        let key = k.as_str().with_context(|| format!("entry \"_\": non-string key {k:?}"))?;
        let key = expand_str(key, &scope).with_context(|| format!("entry {name:?}"))?;
        if entry.contains_key(Value::String(key.clone())) {
            continue;
        }
        let value = expand_yaml(v, &scope).with_context(|| format!("entry {name:?}, field {key:?}"))?;
        entry.insert(Value::String(key), value);
    }
    Ok(())
}

pub fn load(yaml: &str) -> Result<(Settings, Vec<Relation>, Vec<(String, PathBuf)>)> {
    let file: ConfigFile = serde_yaml::from_str(yaml).context("parsing config")?;
    let mut entries = file.entries;
    let defaults = match entries.remove("_") {
        Some(Value::Mapping(m)) => m,
        Some(_) => bail!("entries._ must be a mapping of default fields"),
        None => Mapping::new(),
    };
    let mut logs = Vec::new();
    let mut relations = Vec::new();
    for (key, value) in entries {
        let name = key.as_str().context("entry names must be strings")?.to_owned();
        let Value::Mapping(mut body) = value else {
            bail!("entry {name:?} must be a mapping");
        };
        merge_defaults(&name, &mut body, &defaults)?;
        let entry: EntryConfig = serde_yaml::from_value(Value::Mapping(body))
            .with_context(|| format!("entry {name:?}"))?;
        if let Some(log) = &entry.log {
            logs.push((name.clone(), expand_path(log)));
        }
        relations.push(entry.into_relation(&name)?);
    }
    Ok((
        Settings { state_db: file.state_db, interval: file.interval, dashboard: file.dashboard },
        relations,
        logs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn expansion_resolves_fields_and_leaves_unknowns() {
        let sc = scope(&[("a", "y"), ("name", "skills")]);
        assert_eq!(expand_str("x{a}z", &sc).unwrap(), "xyz");
        assert_eq!(expand_str("~/logs/{name}.log", &sc).unwrap(), "~/logs/skills.log");
        assert_eq!(expand_str("x{missing}z", &sc).unwrap(), "x{missing}z");
    }

    #[test]
    fn expansion_is_recursive_innermost_first() {
        // {b}{c}{d} with b="{{a", c="}}" assembles {{a}} -> {e} -> f; {d} stays literal.
        let sc = scope(&[("a", "e"), ("b", "{{a"), ("c", "}}"), ("e", "f")]);
        assert_eq!(expand_str("{b}{c}{d}", &sc).unwrap(), "f{d}");
        // and keys expand too, via the same function
        assert_eq!(expand_str("{a}x", &sc).unwrap(), "ex");
    }

    #[test]
    fn circular_expansion_errors() {
        let sc = scope(&[("a", "{b}"), ("b", "{a}")]);
        let err = expand_str("{a}", &sc).unwrap_err().to_string();
        assert!(err.contains("circular"), "unexpected error: {err}");
    }
}
