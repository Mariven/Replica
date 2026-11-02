//! Load relations and settings from YAML configuration files.
//!
//! ```yaml
//! state_db: "~/scripts/replica/live/state.db"
//! interval: "30s"
//! entries:
//!     mylibrary:
//!         labels:
//!             source: "~/Desktop/mylibrary"
//!             dest: "~/scripts/desktop-library"
//!         pattern: "*.js"
//!         propagate:
//!             change: "source <-> dest"
//!             create: "source -> dest"
//!             delete: "source -> dest"  # deletions at source delete at dest
//!             conflict: "source mtime dest"
//!         sync_times: true
//!         log: "~/scripts/replica/live/logs/mylibrary.log"
//! ```
//!
//! Propagate statements are separated by `;`. Reachability is transitive. A propagate field left blank (YAML null) means no edges.
//! Conflict statements are `a > b` (a wins), `a < b`, `a mtime b` (newer wins), `a size b`, `a name b`, or a bare property for every pair.
//! Quote every statement: `{a, b}` is a YAML mapping otherwise.
//!
//! Endpoints are bare paths (`~/x`), templates with `{name}` holes ([`PathTemplate`]), or mapping blocks combining `root` with `rename`, `scan`, or `path`.
//! Every path must be absolute or start with `~`; the daemon's working directory is never used.
//!
//! The `_` entry provides default fields for entries without explicit settings.
//! Placeholders in default keys and string values resolve against the target entry's top-level string fields plus `{name}`:
//!
//! ```yaml
//! entries:
//!     _:
//!         propagate: { change: "source <-> dest", create: "source <-> dest" }
//!         log: "~/scripts/replica/live/logs/{name}.log"
//!     skills:
//!         labels:
//!             source: "~/scripts/skills/{name}.md"  # entry values remain literal
//!             dest: "~/.claude/skills/{name}/SKILL.md"
//! ```
//!
//! The parser expands placeholders from innermost tokens outward until strings stop changing, assembling default values from adjacent fields. Dependency cycles abort with an error. Unknown placeholders remain unmodified.
//! YAML merge keys (`<<: *anchor`) are applied before any other processing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer};
use serde_yaml::{Mapping, Value};

use crate::engine::Relation;
use crate::identifier::Pattern;
use crate::label::{Endpoint, Label, Mapping as PathMapping, PathTemplate, Shape};
use crate::policy::{Graph, Policy, parse_rules, rule_labels};

/// Raw configuration layout deserialized directly from YAML.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub state_db: Option<String>,
    #[serde(default = "default_interval", deserialize_with = "string_or_number")]
    pub interval: String,
    /// Close the global write gate at startup, deferring filesystem modifications until dashboard activation.
    #[serde(default)]
    pub gate: bool,
    #[serde(default)]
    pub archive: Option<ArchiveSettings>,
    #[serde(default)]
    pub dashboard: Option<DashboardConfig>,
    pub entries: Mapping,
}

/// Return the default poll interval of 30 seconds.
fn default_interval() -> String { "30s".into() }

/// Default archive size limit.
const DEFAULT_ARCHIVE_CAP: &str = "100MB";

/// Settings resolved from the file: paths expanded and absolute, the interval and the archive cap parsed.
pub struct Settings {
    pub state_db: Option<PathBuf>,
    pub interval: Duration,
    pub gate: bool,
    /// Archive directory, `<state_db directory>/archive` when the file names none. None only when `state_db` is absent too.
    pub archive_dir: Option<PathBuf>,
    pub archive_cap: u64,
    pub dashboard: Option<DashboardConfig>,
}

/// Storage limits and directory location for overwritten or deleted file revisions.
/// By default, the archiver stores backups in an `archive` directory beside the state database with a 100 MB ceiling.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ArchiveSettings {
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default = "default_archive_cap", deserialize_with = "string_or_number")]
    pub cap: String,
}

/// Return the default archive size limit of 100 MB.
fn default_archive_cap() -> String { DEFAULT_ARCHIVE_CAP.into() }

/// Convert human-readable size strings such as `100MB`, `1.5G`, or byte counts into total bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().with_context(|| format!("bad size {s:?}"))?;
    let mult: u64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1,
        "KB" | "K" => 1 << 10,
        "MB" | "M" => 1 << 20,
        "GB" | "G" => 1 << 30,
        "TB" | "T" => 1 << 40,
        other => bail!("bad size unit {other:?} in {s:?}"),
    };
    Ok((n * mult as f64) as u64)
}

/// Web dashboard network and presentation settings.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DashboardConfig {
    pub port: u16,
    /// Maximum count of recent audit events retained in memory.
    #[serde(default = "default_recent_events")]
    pub recent_events: usize,
    /// Cadence in seconds between dashboard refreshes.
    #[serde(default = "default_refresh")]
    pub refresh: u64,
}

/// Return the default event history capacity of 200 items.
fn default_recent_events() -> usize { 200 }

/// Return the default dashboard refresh cadence of 10 seconds.
fn default_refresh() -> u64 { 10 }

/// Field names an entry accepts. Default keys under `_` are checked against this list after placeholder expansion.
const ENTRY_FIELDS: [&str; 6] = ["labels", "pattern", "propagate", "sync_times", "gate", "log"];

/// Parsed endpoint mappings and propagation rules for a single configured entry.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryConfig {
    /// Labels in the order the file lists them, so dashboard columns follow the configuration.
    #[serde(deserialize_with = "ordered_labels")]
    pub labels: Vec<(String, LabelSpec)>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub propagate: PropagateConfig,
    #[serde(default = "default_true")]
    pub sync_times: bool,
    /// Suspend file writes for this relation at startup while continuing change detection and plan generation.
    #[serde(default)]
    pub gate: bool,
    #[serde(default)]
    pub log: Option<String>,
}

/// Return true as the default timestamp synchronization toggle.
fn default_true() -> bool { true }

/// Read the `labels` mapping in file order, dispatching each value on its YAML type.
fn ordered_labels<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<(String, LabelSpec)>, D::Error> {
    let map = Mapping::deserialize(d)?;
    map.into_iter().map(|(k, v)| {
        let name = k.as_str()
            .ok_or_else(|| serde::de::Error::custom(format!("label names must be strings, not {}", yaml_kind(&k))))?
            .to_owned();
        let spec = LabelSpec::from_value(v).map_err(|e| serde::de::Error::custom(format!("label {name:?}: {e}")))?;
        Ok((name, spec))
    }).collect()
}

/// Accept a bare number as well as a string, so `interval: 0` reaches the duration check instead of failing as a type mismatch.
fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!("expected a string such as \"30s\" or \"100MB\", not {}", yaml_kind(&other)))),
    }
}

/// Accept a string or a YAML null, so a propagate field can be left blank as the example config suggests.
fn string_or_null<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// Describe the YAML type of a value for error messages.
fn yaml_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Sequence(_) => "a list",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
    }
}

/// Propagation statements for edits, new files, and deletions, plus conflict rules, as written in the config.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PropagateConfig {
    #[serde(default, deserialize_with = "string_or_null")]
    pub change: String,
    #[serde(default, deserialize_with = "string_or_null")]
    pub create: String,
    #[serde(default, deserialize_with = "string_or_null")]
    pub delete: String,
    #[serde(default, deserialize_with = "string_or_null")]
    pub conflict: String,
}

/// Endpoint or list of endpoints behind a label.
pub enum LabelSpec {
    One(EndpointSpec),
    Many(Vec<EndpointSpec>),
}

impl LabelSpec {
    /// Dispatch on the YAML type: a list is several endpoints, anything else is one endpoint.
    /// Dispatching by type, instead of trying variants in turn, keeps the unknown-key errors from the endpoint mapping.
    fn from_value(v: Value) -> std::result::Result<LabelSpec, serde_yaml::Error> {
        match v {
            Value::Sequence(items) => Ok(LabelSpec::Many(items.into_iter().map(EndpointSpec::from_value).collect::<std::result::Result<_, _>>()?)),
            other => Ok(LabelSpec::One(EndpointSpec::from_value(other)?)),
        }
    }
}

/// Endpoint specification in either concise string notation or structured table format.
pub enum EndpointSpec {
    Short(String),
    Full(EndpointConfig),
}

impl EndpointSpec {
    /// Dispatch on the YAML type: a string is a path or template, a mapping is a full endpoint block.
    fn from_value(v: Value) -> std::result::Result<EndpointSpec, serde_yaml::Error> {
        match v {
            Value::String(s) => Ok(EndpointSpec::Short(s)),
            Value::Mapping(_) => Ok(EndpointSpec::Full(serde_yaml::from_value(v)?)),
            other => Err(serde::de::Error::custom(format!(
                "an endpoint is a path string or a mapping with root, rename, scan or path, not {}", yaml_kind(&other)))),
        }
    }
}

/// Structured endpoint configuration supporting root paths, renames, directory scans, or path templates.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EndpointConfig {
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub rename: Option<String>,
    #[serde(default)]
    pub scan: Option<ScanConfig>,
    #[serde(default)]
    pub path: Option<String>,
}

/// Directory scan rule interpolating each child entry into the template hole to discover endpoint roots.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    pub template: String,
    pub hole: String,
    pub dir: String,
}

/// Resolve leading tilde characters to the current user home directory path.
pub fn expand_path(s: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(s).into_owned())
}

/// Expand a leading tilde and require an absolute result, so no path ever resolves against the daemon's working directory.
pub fn absolute_path(s: &str, what: &str) -> Result<PathBuf> {
    let p = expand_path(s);
    if !p.is_absolute() {
        bail!("{what}: {s:?} must be an absolute path or start with ~");
    }
    Ok(p)
}

/// Expand a leading tilde in template text and require an absolute result.
fn absolute_text(s: &str, what: &str) -> Result<String> {
    let text = shellexpand::tilde(s).into_owned();
    if !text.starts_with('/') {
        bail!("{what}: {s:?} must be an absolute path or start with ~");
    }
    Ok(text)
}

/// Parse a duration like `30s`, `1m`, `2h`, or `500ms`. Zero is rejected, since a zero interval runs cycles back to back.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = num.parse().with_context(|| format!("bad duration {s:?}"))?;
    let secs = |mult: u64| n.checked_mul(mult).map(Duration::from_secs).with_context(|| format!("duration {s:?} is too large"));
    let d = match unit.trim() {
        "ms" => Duration::from_millis(n),
        "s" | "" => secs(1)?,
        "m" => secs(60)?,
        "h" => secs(3600)?,
        _ => bail!("bad duration unit {unit:?} in {s:?} (use ms, s, m or h)"),
    };
    if d.is_zero() {
        bail!("duration {s:?} must be greater than zero");
    }
    Ok(d)
}

impl EndpointSpec {
    /// Convert the endpoint specification into an Endpoint. A short string is a `path:` when it has holes and a `root:` otherwise.
    fn into_endpoint(self, name: &str) -> Result<Endpoint> {
        let config = match self {
            EndpointSpec::Short(s) if s.contains('{') => EndpointConfig { path: Some(s), ..Default::default() },
            EndpointSpec::Short(s) => EndpointConfig { root: Some(s), ..Default::default() },
            EndpointSpec::Full(c) => c,
        };
        config.into_endpoint(name)
    }
}

impl EndpointConfig {
    /// Construct an Endpoint from the configuration, selecting among literal root, scan directory, or path template definitions.
    fn into_endpoint(self, name: &str) -> Result<Endpoint> {
        let what = format!("endpoint {name:?}");
        let mapping = match &self.rename {
            Some(leaf) => PathMapping::Fixed(leaf.clone()),
            None => PathMapping::Direct,
        };
        // Match mutually exclusive target variants across literal roots, directory scans, and templates.
        let shape = match (self.root, self.scan, self.path) {
            (Some(root), None, None) => Shape::Literal { root: absolute_path(&root, &what)? },
            (None, Some(scan), None) => Shape::Scan {
                template: absolute_text(&scan.template, &format!("{what}: scan.template"))?,
                hole: scan.hole,
                dir: absolute_path(&scan.dir, &format!("{what}: scan.dir"))?,
            },
            (None, None, Some(path)) => {
                if matches!(mapping, PathMapping::Fixed(_)) {
                    bail!("{what}: rename does not combine with path (the template already names the file)");
                }
                Shape::Template(PathTemplate::parse(&absolute_text(&path, &what)?).with_context(|| what.clone())?)
            }
            _ => bail!("{what} must have exactly one of root / scan / path"),
        };
        Ok(Endpoint { shape, mapping })
    }
}

impl LabelSpec {
    /// Convert single or multiple endpoint specifications into a Label.
    fn into_label(self, name: &str) -> Result<Label> {
        let specs = match self {
            LabelSpec::One(ep) => vec![ep],
            LabelSpec::Many(eps) => eps,
        };
        if specs.is_empty() {
            bail!("label {name:?} lists no endpoints");
        }
        let endpoints = specs.into_iter().map(|ep| ep.into_endpoint(name)).collect::<Result<Vec<_>>>()?;
        Ok(Label { name: name.into(), endpoints })
    }
}

impl EntryConfig {
    /// Parse propagation policies, validate label references, and construct a Relation.
    pub fn into_relation(self, name: &str) -> Result<Relation> {
        let graph = |field: &str, text: &str| Graph::parse(text).with_context(|| format!("entry {name:?}: propagate.{field}"));
        let policy = Policy {
            change: graph("change", &self.propagate.change)?,
            create: graph("create", &self.propagate.create)?,
            delete: graph("delete", &self.propagate.delete)?,
            conflict: parse_rules(&self.propagate.conflict).with_context(|| format!("entry {name:?}: propagate.conflict"))?,
        };
        if policy.change.is_empty() && policy.create.is_empty() && policy.delete.is_empty() {
            bail!("entry {name:?}: propagate defines no edges and nothing would sync");
        }
        let pattern = match &self.pattern {
            None => Pattern::All,
            Some(p) => Pattern::glob(p).with_context(|| format!("entry {name:?}: bad pattern {p:?}"))?,
        };
        if self.labels.len() < 2 {
            bail!("entry {name:?} declares {} label(s); at least two are needed to relate anything", self.labels.len());
        }
        // Confirm every label referenced in propagation rules is declared.
        let mentioned = policy.change.labels().into_iter()
            .chain(policy.create.labels())
            .chain(policy.delete.labels())
            .chain(rule_labels(&policy.conflict));
        for named in mentioned {
            if !self.labels.iter().any(|(n, _)| n == named) {
                bail!("entry {name:?}: propagate references undeclared label {named:?}");
            }
        }
        // Resolve declared label specifications into concrete endpoints, naming the entry in any error.
        let labels = self.labels.into_iter()
            .map(|(label_name, spec)| spec.into_label(&label_name).with_context(|| format!("entry {name:?}")))
            .collect::<Result<Vec<_>>>()?;
        // A rename endpoint is never walked, so a relation where every endpoint renames would find no files at all.
        let renames = labels.iter().flat_map(|l| &l.endpoints).filter(|ep| matches!(ep.mapping, PathMapping::Fixed(_))).count();
        if renames > 0 && renames == labels.iter().map(|l| l.endpoints.len()).sum::<usize>() {
            bail!("entry {name:?}: every endpoint uses rename, so no files could be discovered; at least one endpoint needs a root, scan, or path without rename");
        }
        // A rename endpoint holds one file, so the relation must say which one; without a pattern a second file in the folder fails every cycle.
        if renames > 0 && self.pattern.is_none() {
            bail!("entry {name:?}: a rename endpoint needs a pattern naming the one file to carry, such as pattern: default.conf");
        }
        // Validate template consistency across endpoints: compare all hole sets against the first template, rejecting mismatches or mixed literal endpoints.
        let shapes: Vec<(&str, &Shape)> = labels.iter().flat_map(|l| l.endpoints.iter().map(move |ep| (l.name.as_str(), &ep.shape))).collect();
        if let Some(first) = shapes.iter().find_map(|(n, shape)| match shape {
            Shape::Template(t) => Some((*n, &t.holes)),
            _ => None,
        }) {
            for (n, shape) in &shapes {
                match shape {
                    Shape::Template(t) if t.holes == *first.1 => {}
                    Shape::Template(t) => bail!(
                        "entry {name:?}: labels {:?} and {:?} disagree on holes ({:?} vs {:?})",
                        first.0, n, first.1, t.holes
                    ),
                    _ => bail!(
                        "entry {name:?}: label {:?} must also be a path template because label {:?} is",
                        n, first.0
                    ),
                }
            }
        }
        Ok(Relation {
            name: name.into(),
            labels,
            pattern,
            policy,
            sync_times: self.sync_times,
            gate_default: self.gate,
        })
    }
}

const MAX_EXPANSION_PASSES: usize = 8;

/// Substitute innermost `{word}` placeholders present in scope, leaving unknown tokens unchanged.
/// Evaluate inner braces first so substituted text can supply parts of outer placeholder keys.
fn expand_once(s: &str, scope: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c == '}' {
            // Search backward in the output buffer for the matching open brace. Substituted values remain in this buffer and can form parts of outer keys.
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

/// Expand `{placeholders}` through successive passes until the string reaches a fixed point, returning an error on reference cycles.
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

/// Recursively expand placeholders across all strings, mapping keys, and sequences in a YAML value.
fn expand_yaml(v: &Value, scope: &BTreeMap<String, String>) -> Result<Value> {
    Ok(match v {
        Value::String(s) => Value::String(expand_str(s, scope)?),
        // Recursively expand mapping keys and values.
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

/// Build a scope map from existing string fields and the relation identifier, expand default template keys and values against that scope, then insert missing fields into the entry.
/// Reject a default key that expands to something no entry accepts, naming `_` rather than the entry that happened to merge it first.
fn merge_defaults(name: &str, entry: &mut Mapping, defaults: &Mapping) -> Result<()> {
    let mut scope: BTreeMap<String, String> = entry.iter()
        .filter_map(|(k, v)| Some((k.as_str()?.to_owned(), v.as_str()?.to_owned())))
        .collect();
    scope.insert("name".into(), name.to_owned());
    for (k, v) in defaults {
        let key = k.as_str().with_context(|| format!("entry \"_\": non-string key {k:?}"))?;
        let key = expand_str(key, &scope).with_context(|| format!("entry {name:?}"))?;
        if !ENTRY_FIELDS.contains(&key.as_str()) {
            bail!("entry \"_\": unknown field {key:?} (entries accept {})", ENTRY_FIELDS.join(", "));
        }
        if entry.contains_key(Value::String(key.clone())) {
            continue;
        }
        let value = expand_yaml(v, &scope).with_context(|| format!("entry {name:?}, field {key:?}"))?;
        entry.insert(Value::String(key), value);
    }
    Ok(())
}

/// Parse configuration YAML into raw entries, extract the default mapping, merge defaults into each relation map, then construct settings, relation definitions, and log file paths.
pub fn load(yaml: &str) -> Result<(Settings, Vec<Relation>, Vec<(String, PathBuf)>)> {
    // Parsing straight into the struct keeps the field path and line/column in type errors; merge keys need the Value detour.
    let file: ConfigFile = if yaml.contains("<<") {
        let mut value: Value = serde_yaml::from_str(yaml).context("parsing config")?;
        value.apply_merge().context("parsing config")?;
        serde_yaml::from_value(value).context("parsing config")?
    } else {
        serde_yaml::from_str(yaml).context("parsing config")?
    };
    let interval = parse_duration(&file.interval).context("interval")?;
    let state_db = file.state_db.as_deref().map(|s| absolute_path(s, "state_db")).transpose()?;
    let archive_cap = parse_size(file.archive.as_ref().map(|a| a.cap.as_str()).unwrap_or(DEFAULT_ARCHIVE_CAP)).context("archive.cap")?;
    let archive_dir = match file.archive.as_ref().and_then(|a| a.dir.as_deref()) {
        Some(dir) => Some(absolute_path(dir, "archive.dir")?),
        None => state_db.as_ref().map(|db| db.parent().unwrap_or(Path::new("/")).join("archive")),
    };
    let mut entries = file.entries;
    // shift_remove keeps the remaining entries in file order; remove would move the last entry into the gap.
    let defaults = match entries.shift_remove("_") {
        Some(Value::Mapping(m)) => m,
        Some(_) => bail!("entries._ must be a mapping of default fields"),
        None => Mapping::new(),
    };
    // Keys without placeholders can be checked once here, so a typo under `_` is caught even when no entry merges it.
    for k in defaults.keys() {
        if let Some(key) = k.as_str() {
            if !key.contains('{') && !ENTRY_FIELDS.contains(&key) {
                bail!("entry \"_\": unknown field {key:?} (entries accept {})", ENTRY_FIELDS.join(", "));
            }
        }
    }
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
            logs.push((name.clone(), absolute_path(log, &format!("entry {name:?}: log"))?));
        }
        relations.push(entry.into_relation(&name)?);
    }
    Ok((
        Settings {
            state_db,
            interval,
            gate: file.gate,
            archive_dir,
            archive_cap,
            dashboard: file.dashboard,
        },
        relations,
        logs,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect string slice pairs into an expansion scope map for testing.
    fn scope(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Load a config and render its error chain as one string.
    fn load_err(yaml: &str) -> String {
        match load(yaml) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("config was accepted:\n{yaml}"),
        }
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
        // Substitution order for `{b}{c}{d}`:
        // 1. Expand `{b}` and `{c}` to assemble `{{a}}`.
        // 2. Resolve inner `{a}` to yield `{e}`.
        // 3. Resolve `{e}` to produce `f`.
        // 4. Keep unmapped `{d}` as literal text.
        let sc = scope(&[("a", "e"), ("b", "{{a"), ("c", "}}"), ("e", "f")]);
        assert_eq!(expand_str("{b}{c}{d}", &sc).unwrap(), "f{d}");
        // Evaluate placeholder prefixes directly to confirm expansion on mapping keys.
        assert_eq!(expand_str("{a}x", &sc).unwrap(), "ex");
    }

    #[test]
    fn circular_expansion_errors() {
        let sc = scope(&[("a", "{b}"), ("b", "{a}")]);
        let err = expand_str("{a}", &sc).unwrap_err().to_string();
        assert!(err.contains("circular"), "unexpected error: {err}");
    }

    /// A list of exactly two paths is two endpoints, never a `{root, rename}` pair read positionally.
    #[test]
    fn two_path_list_is_two_endpoints() {
        let yaml = "entries:\n  r:\n    labels:\n      source: /s\n      dest:\n        - /a\n        - /b\n    propagate: { change: \"source <-> dest\" }\n";
        let (_, relations, _) = load(yaml).unwrap();
        let dest = relations[0].labels.iter().find(|l| l.name == "dest").unwrap();
        assert_eq!(dest.endpoints.len(), 2);
        assert!(dest.endpoints.iter().all(|ep| matches!(ep.mapping, PathMapping::Direct)));
        let roots: Vec<&Path> = dest.endpoints.iter().map(|ep| match &ep.shape { Shape::Literal { root } => root.as_path(), _ => panic!() }).collect();
        assert_eq!(roots, vec![Path::new("/a"), Path::new("/b")]);
    }

    /// Endpoint errors name the entry, the label, and the offending key.
    #[test]
    fn endpoint_errors_name_entry_label_and_field() {
        let err = load_err("entries:\n  r:\n    labels:\n      source: { rooot: /s }\n      dest: /d\n    propagate: { change: \"source <-> dest\" }\n");
        assert!(err.contains("entry \"r\""), "{err}");
        assert!(err.contains("label \"source\""), "{err}");
        assert!(err.contains("unknown field `rooot`"), "{err}");
        let err = load_err("entries:\n  r:\n    labels:\n      source: 12\n      dest: /d\n    propagate: { change: \"source <-> dest\" }\n");
        assert!(err.contains("not a number"), "{err}");
        let err = load_err("entries:\n  r:\n    labels:\n      source: { scan: { template: \"/p/{x}\", hole: x, dir: /p, extra: 1 } }\n      dest: /d\n    propagate: { change: \"source <-> dest\" }\n");
        assert!(err.contains("unknown field `extra`"), "{err}");
        let err = load_err("entries:\n  r:\n    labels:\n      source: relative/dir\n      dest: /d\n    propagate: { change: \"source <-> dest\" }\n");
        assert!(err.contains("entry \"r\"") && err.contains("absolute"), "{err}");
    }

    /// Labels keep file order instead of sorting alphabetically.
    #[test]
    fn labels_keep_file_order() {
        let yaml = "entries:\n  r:\n    labels:\n      zeta: /z\n      alpha: /a\n    propagate: { change: \"zeta <-> alpha\" }\n";
        let (_, relations, _) = load(yaml).unwrap();
        let names: Vec<&str> = relations[0].labels.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["zeta", "alpha"]);
    }

    /// Entries keep file order too, since removing `_` must not swap the last entry into its place.
    #[test]
    fn entries_keep_file_order_after_removing_defaults() {
        let yaml = "entries:\n  _:\n    propagate: { change: \"a <-> b\" }\n  first:\n    labels: { a: /1a, b: /1b }\n  second:\n    labels: { a: /2a, b: /2b }\n  third:\n    labels: { a: /3a, b: /3b }\n";
        let (_, relations, _) = load(yaml).unwrap();
        let names: Vec<&str> = relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn durations_reject_zero_and_accept_hours() {
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("0m").is_err());
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        let err = load_err("interval: 0\nentries:\n  r:\n    labels: { a: /a, b: /b }\n    propagate: { change: \"a <-> b\" }\n");
        assert!(err.starts_with("interval:"), "{err}");
        let err = load_err("archive: { cap: 10 parsecs }\nentries:\n  r:\n    labels: { a: /a, b: /b }\n    propagate: { change: \"a <-> b\" }\n");
        assert!(err.starts_with("archive.cap:"), "{err}");
    }

    /// A blank propagate field is an empty graph, matching the example config's advice to leave `delete:` blank.
    #[test]
    fn null_propagate_field_means_no_edges() {
        let yaml = "entries:\n  r:\n    labels: { a: /a, b: /b }\n    propagate:\n      change: \"a <-> b\"\n      delete:\n";
        let (_, relations, _) = load(yaml).unwrap();
        assert!(relations[0].policy.delete.is_empty());
    }

    /// YAML merge keys work in entries.
    #[test]
    fn merge_keys_are_applied() {
        let yaml = "entries:\n  base: &base\n    labels: { a: /a, b: /b }\n    propagate: { change: \"a <-> b\" }\n  other:\n    <<: *base\n    pattern: \"*.md\"\n";
        let (_, relations, _) = load(yaml).unwrap();
        assert_eq!(relations.len(), 2);
        assert!(relations[1].pattern.matches(&vec!["x.md".to_string()]));
    }

    /// A typo under `_` is reported against `_`, not against whichever entry merged it first.
    #[test]
    fn default_typos_name_the_defaults_block() {
        let err = load_err("entries:\n  _:\n    patern: \"*.md\"\n  r:\n    labels: { a: /a, b: /b }\n    propagate: { change: \"a <-> b\" }\n");
        assert!(err.contains("entry \"_\": unknown field \"patern\""), "{err}");
    }

    /// Relative paths are rejected everywhere a path is accepted.
    #[test]
    fn relative_paths_are_rejected() {
        let body = "entries:\n  r:\n    labels: { a: /a, b: /b }\n    propagate: { change: \"a <-> b\" }\n";
        assert!(load_err(&format!("state_db: live/state.db\n{body}")).starts_with("state_db:"));
        assert!(load_err(&format!("archive: {{ dir: live/archive }}\n{body}")).starts_with("archive.dir:"));
        let err = load_err("entries:\n  r:\n    labels: { a: /a, b: /b }\n    propagate: { change: \"a <-> b\" }\n    log: logs/r.log\n");
        assert!(err.contains("log") && err.contains("absolute"), "{err}");
        let err = load_err("entries:\n  r:\n    labels: { a: { path: \"x/{n}.md\" }, b: { path: \"/y/{n}.md\" } }\n    propagate: { change: \"a <-> b\" }\n");
        assert!(err.contains("absolute"), "{err}");
        let err = load_err("entries:\n  r:\n    labels: { a: { scan: { template: \"p/{x}\", hole: x, dir: /p } }, b: /b }\n    propagate: { change: \"a <-> b\" }\n");
        assert!(err.contains("scan.template") && err.contains("absolute"), "{err}");
    }

    /// Two rename endpoints would discover nothing, and a rename without a pattern fails at the second file, so both are rejected at load time.
    #[test]
    fn all_rename_entries_are_rejected() {
        let err = load_err("entries:\n  r:\n    labels:\n      a: { root: /a, rename: x.txt }\n      b: { root: /b, rename: y.txt }\n    pattern: f.txt\n    propagate: { change: \"a <-> b\" }\n");
        assert!(err.contains("every endpoint uses rename"), "{err}");
        let err = load_err("entries:\n  r:\n    labels:\n      a: /a\n      b: { root: /b, rename: y.txt }\n    propagate: { change: \"a <-> b\" }\n");
        assert!(err.contains("needs a pattern"), "{err}");
        assert!(load("entries:\n  r:\n    labels:\n      a: /a\n      b: { root: /b, rename: y.txt }\n    pattern: f.txt\n    propagate: { change: \"a <-> b\" }\n").is_ok());
    }

    /// The shipped example loads, and a typo under `_` is caught even with no other entry.
    #[test]
    fn example_config_loads_and_lone_default_typos_are_caught() {
        let (settings, relations, logs) = load(include_str!("../config.example.yaml")).unwrap();
        assert_eq!(relations.len(), 5);
        assert_eq!(logs.len(), 5);
        assert!(settings.state_db.is_some());
        let err = load_err("entries:\n  _:\n    patern: \"*.md\"\n");
        assert!(err.contains("entry \"_\": unknown field \"patern\""), "{err}");
        // Top-level type errors keep their field name and position.
        let err = load_err("dashboard: { port: abc }\nentries:\n  r:\n    labels: { a: /a, b: /b }\n    propagate: { change: \"a <-> b\" }\n");
        assert!(err.contains("dashboard.port") && err.contains("line 1"), "{err}");
    }

    /// Settings come back typed, with the archive directory defaulting beside the database.
    #[test]
    fn settings_are_resolved() {
        let (settings, _, _) = load("state_db: /var/replica/state.db\ninterval: 2m\nentries:\n  r:\n    labels: { a: /a, b: /b }\n    propagate: { change: \"a <-> b\" }\n").unwrap();
        assert_eq!(settings.interval, Duration::from_secs(120));
        assert_eq!(settings.state_db.as_deref(), Some(Path::new("/var/replica/state.db")));
        assert_eq!(settings.archive_dir.as_deref(), Some(Path::new("/var/replica/archive")));
        assert_eq!(settings.archive_cap, 100 << 20);
    }
}
