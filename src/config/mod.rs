//! Load the configuration. The loader parses the YAML and merges in the included files. For each relation it then merges in the profiles, expands variables, checks the result, and builds the relation for the engine.
//!
//! ```yaml
//! state: ~/scripts/replica/live
//! interval: 1m
//! watch: true
//! vars: { projects: ~/Desktop/Projects }
//! profiles:
//!     _: { log: "${state}/logs/${relation}.log" }
//!     two-way: { flow: "source <-> dest [edit create]" }
//! relations:
//!     mylibrary:
//!         profile: two-way
//!         sites: { source: ~/Desktop/mylibrary/, dest: ~/scripts/desktop-library/ }
//!         where: { path: "*.js" }
//!     style-guide:
//!         sites: { source: ~/scripts/rules/main_style.md, dest: "${projects}/{project}/docs/style/main_style.md" }
//!         fanout: [project]
//!         flow: "source -> dest [edit create]"
//! ```
//!
//! Path variables in plain braces, such as `{project}`, belong to patterns. Configuration variables are written `${name}` and are expanded before any pattern is parsed, so the two kinds stay apart.

pub mod merge;
pub mod text;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_norway::{Mapping, Value};

use crate::action::{Actions, DEFAULT_TIMEOUT, Shell, Trigger};
use crate::engine::Relation;
use crate::flow::{Kind, Policy, rule_sites};
use crate::pattern::{Constraint, Pattern};
use crate::site::{Location, Site, compile_ignore, validate_vars};
use text::{CommandText, DashboardText, FileText, RelationText, yaml_kind};

/// The archive cap used when the file sets none.
const DEFAULT_ARCHIVE_CAP: &str = "100MB";

/// The top-level settings of the file, with defaults filled in.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The state folder, which holds the database, the blob store, and the logs. It is None if the file leaves it out, which is fine for `check` and `explain`.
    pub state: Option<PathBuf>,
    pub interval: Duration,
    pub watch: bool,
    pub debounce: Duration,
    pub gate: bool,
    pub ignore: Vec<String>,
    pub archive_cap: u64,
    pub dashboard: Option<DashboardConfig>,
}

/// Dashboard settings.
#[derive(Clone, Debug)]
pub struct DashboardConfig {
    pub port: u16,
    pub recent_events: usize,
    pub refresh: u64,
}

impl Settings {
    /// The database path, inside the state folder.
    pub fn db_path(&self) -> Option<PathBuf> {
        self.state.as_ref().map(|s| s.join("state.db"))
    }

    /// The blob store folder, inside the state folder.
    pub fn blob_dir(&self) -> Option<PathBuf> {
        self.state.as_ref().map(|s| s.join("blobs"))
    }
}

/// A loaded configuration: the settings and the built relations.
pub struct Loaded {
    pub settings: Settings,
    pub relations: Vec<Relation>,
    /// The profiles and expanded variables of each relation, kept for `explain` to show.
    pub provenance: BTreeMap<String, Provenance>,
}

/// The profiles a relation lists by name, and the variables `explain` shows for it, with their expanded values.
#[derive(Clone, Debug, Default)]
pub struct Provenance {
    pub profiles: Vec<String>,
    pub vars: BTreeMap<String, String>,
}

/// Expand a leading tilde and require an absolute path.
pub fn absolute_path(s: &str, what: &str) -> Result<PathBuf> {
    let p = PathBuf::from(shellexpand::tilde(s.trim()).into_owned());
    if !p.is_absolute() {
        bail!("{what}: {s:?} must be an absolute path or start with ~");
    }
    Ok(p)
}

/// Parse a duration like `30s`, `1m`, `2h`, or `500ms`. Zero is rejected.
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

/// Parse a size like `100MB`, `1.5G`, or a byte count.
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

/// Parse YAML text into the typed file form, applying merge keys first.
fn parse_file(yaml: &str, what: &Path) -> Result<FileText> {
    if yaml.trim().is_empty() {
        return Ok(FileText::default());
    }
    // Serde reports the field path and position of a type error only when it reads the YAML straight into the struct, but it ignores merge keys when it does that. A file that uses `<<` is read into a Value first so its merge keys can be applied.
    if !yaml.contains("<<") {
        return serde_norway::from_str(yaml).with_context(|| format!("parsing {}", what.display()));
    }
    let mut value: Value = serde_norway::from_str(yaml).with_context(|| format!("parsing {}", what.display()))?;
    value.apply_merge().with_context(|| format!("parsing {}", what.display()))?;
    serde_norway::from_value(value).with_context(|| format!("parsing {}", what.display()))
}

/// Load a configuration file from disk.
pub fn load_file(path: &Path) -> Result<Loaded> {
    let yaml = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    load(&yaml, path)
}

/// Load configuration text read from `path`. Includes are resolved relative to its folder, and errors name it.
pub fn load(yaml: &str, path: &Path) -> Result<Loaded> {
    let mut file = parse_file(yaml, path)?;
    let base = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
    for entry in std::mem::take(&mut file.include) {
        for included in merge::included_files(&entry, &base)? {
            let text = std::fs::read_to_string(&included).with_context(|| format!("reading {}", included.display()))?;
            let other = parse_file(&text, &included)?;
            merge::merge_included(&mut file, other, path, &included)?;
        }
    }
    let settings = Settings {
        state: file.state.as_deref().map(|s| absolute_path(s, "state")).transpose()?,
        interval: parse_duration(file.interval.as_deref().unwrap_or("30s")).context("interval")?,
        watch: file.watch.unwrap_or(true),
        debounce: parse_duration(file.debounce.as_deref().unwrap_or("2s")).context("debounce")?,
        gate: file.gate.unwrap_or(false),
        ignore: file.ignore.clone(),
        archive_cap: parse_size(file.archive.as_ref().and_then(|a| a.cap.as_deref()).unwrap_or(DEFAULT_ARCHIVE_CAP)).context("archive.cap")?,
        dashboard: file.dashboard.as_ref().map(|d: &DashboardText| DashboardConfig { port: d.port, recent_events: d.recent_events, refresh: d.refresh }),
    };
    compile_ignore(&settings.ignore).context("ignore")?;
    check_profiles(&file.profiles)?;
    let mut scope = file.vars.clone();
    if let Some(state) = &settings.state {
        scope.entry("state".into()).or_insert_with(|| state.display().to_string());
    }
    let mut relations = Vec::new();
    let mut provenance = BTreeMap::new();
    for (key, value) in &file.relations {
        let name = key.as_str().with_context(|| format!("relation names must be strings, not {}", yaml_kind(key)))?;
        let Value::Mapping(body) = value else { bail!("relation {name:?} must be a mapping") };
        for (copy_name, copy_body, each_vars) in merge::stamp(name, body)? {
            if relations.iter().any(|r: &Relation| r.name == copy_name) {
                bail!("relation {copy_name:?} is defined twice");
            }
            let (relation, prov) = build_relation(&copy_name, &copy_body, &file.profiles, &scope, &each_vars, &settings).with_context(|| format!("relation {copy_name:?}"))?;
            provenance.insert(copy_name, prov);
            relations.push(relation);
        }
    }
    Ok(Loaded { settings, relations, provenance })
}

/// Fail if a profile has an unknown field. The check runs before any relation is built, so a typo is reported against the profile instead of against the first relation that uses it.
fn check_profiles(profiles: &Mapping) -> Result<()> {
    for (k, v) in profiles {
        let name = k.as_str().with_context(|| format!("profile names must be strings, not {}", yaml_kind(k)))?;
        let Value::Mapping(body) = v else { bail!("profile {name:?} must be a mapping of relation fields") };
        let unknown = merge::unknown_fields(body, &text::RELATION_FIELDS);
        if let Some(field) = unknown.iter().next() {
            bail!("profile {name:?}: unknown field {field:?} (relations accept {})", text::RELATION_FIELDS.join(", "));
        }
    }
    Ok(())
}

/// Merge a relation's profiles into its body, expand its variables, then check and build the relation. The profiles and variables are returned as well, for `explain`.
fn build_relation(name: &str, body: &Mapping, profiles: &Mapping, scope: &BTreeMap<String, String>, each: &BTreeMap<String, String>, settings: &Settings) -> Result<(Relation, Provenance)> {
    let assembled = merge::assemble(body, profiles)?;
    // A relation's own `vars` come from the assembled body, so a profile can supply them. Their values may refer to variables from the outer scope.
    let mut vars = scope.clone();
    vars.insert("relation".into(), name.to_owned());
    vars.extend(each.iter().map(|(k, v)| (k.clone(), v.clone())));
    if let Some(Value::Mapping(own)) = assembled.get("vars") {
        for (k, v) in own {
            let (Some(k), Some(v)) = (k.as_str(), v.as_str()) else { bail!("vars must map names to strings") };
            vars.insert(k.to_owned(), v.to_owned());
        }
    }
    let mut expanded = match merge::expand_value(&Value::Mapping(assembled), &vars)? {
        Value::Mapping(m) => m,
        _ => unreachable!(),
    };
    text::rename_where(&mut expanded);
    let profiles_used: Vec<String> = match body.get("profile") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Sequence(v)) => v.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    };
    let text: RelationText = serde_norway::from_value(Value::Mapping(expanded)).map_err(|e| anyhow::anyhow!("{}", e.to_string().replace("where_", "where")))?;
    let relation = assemble_relation(name, text, settings)?;
    let mut shown = BTreeMap::new();
    for (k, v) in &vars {
        if !scope.contains_key(k) || k == "state" {
            shown.insert(k.clone(), merge::expand_vars(v, &vars)?);
        }
    }
    let prov = Provenance { profiles: profiles_used, vars: shown };
    Ok((relation, prov))
}

/// Build the sites of a relation from its `sites` mapping. Each entry is a pattern, a list of patterns, or a map from labels to patterns.
fn build_sites(sites: &Mapping) -> Result<Vec<Site>> {
    let mut out = Vec::new();
    for (k, v) in sites {
        let site = k.as_str().with_context(|| format!("site names must be strings, not {}", yaml_kind(k)))?;
        let parse = |s: &str| Pattern::parse(s).with_context(|| format!("site {site:?}"));
        let locations = match v {
            Value::String(s) => vec![Location { label: None, pattern: parse(s)? }],
            Value::Sequence(items) => {
                if items.is_empty() {
                    bail!("site {site:?} lists no locations");
                }
                items.iter().map(|item| {
                    let s = item.as_str().with_context(|| format!("site {site:?}: locations must be strings, not {}", yaml_kind(item)))?;
                    Ok(Location { label: Some(s.trim().to_owned()), pattern: parse(s)? })
                }).collect::<Result<Vec<_>>>()?
            }
            Value::Mapping(m) => {
                if m.is_empty() {
                    bail!("site {site:?} lists no locations");
                }
                m.iter().map(|(label, item)| {
                    let label = label.as_str().with_context(|| format!("site {site:?}: location names must be strings"))?;
                    let s = item.as_str().with_context(|| format!("site {site:?}: location {label:?} must be a string, not {}", yaml_kind(item)))?;
                    Ok(Location { label: Some(label.to_owned()), pattern: parse(s)? })
                }).collect::<Result<Vec<_>>>()?
            }
            other => bail!("site {site:?} must be a pattern, a list of patterns, or a map of names to patterns, not {}", yaml_kind(other)),
        };
        let mut seen = BTreeSet::new();
        for loc in &locations {
            if !seen.insert(loc.pattern.text().to_owned()) {
                bail!("site {site:?} lists {} twice", loc.pattern.text());
            }
        }
        out.push(Site { name: site.to_owned(), locations });
    }
    if out.len() < 2 {
        bail!("declares {} site(s); at least two are needed to relate anything", out.len());
    }
    Ok(out)
}

/// Apply the `where` block. Each entry constrains its variable in each pattern that has that variable.
fn apply_where(sites: &mut [Site], where_: &BTreeMap<String, Value>) -> Result<()> {
    for (var, spec) in where_ {
        let texts: Vec<String> = match spec {
            Value::String(s) => vec![s.clone()],
            Value::Sequence(items) => items.iter().map(|v| v.as_str().map(str::to_owned).with_context(|| format!("where.{var}: constraints must be strings"))).collect::<Result<_>>()?,
            other => bail!("where.{var} must be a glob, a ~regex~, or a list of them, not {}", yaml_kind(other)),
        };
        let mut applied = false;
        for text in texts {
            let constraint = Constraint::parse(&text).with_context(|| format!("where.{var}"))?;
            for site in sites.iter_mut() {
                for loc in &mut site.locations {
                    if loc.pattern.var_names().contains(var.as_str()) {
                        loc.pattern.constrain(var, constraint.clone())?;
                        applied = true;
                    }
                }
            }
        }
        if !applied {
            bail!("where.{var}: no site has a variable {{{var}}}");
        }
    }
    Ok(())
}

/// Split the variables into identity and fan-out sets. A variable on all sites is an identity variable. A variable on only some sites has to be listed under `fanout`. Anything else is an error, and the message suggests a fix.
fn classify(sites: &[Site], declared: &[String]) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let per_site: Vec<(&str, BTreeSet<String>)> = sites.iter().map(|s| (s.name.as_str(), s.var_names())).collect();
    let all: BTreeSet<String> = per_site.iter().flat_map(|(_, v)| v.iter().cloned()).collect();
    let fanout: BTreeSet<String> = declared.iter().cloned().collect();
    for f in &fanout {
        if !all.contains(f) {
            bail!("fanout lists {{{f}}}, but no site has that variable");
        }
        if per_site.iter().all(|(_, v)| v.contains(f)) {
            bail!("fanout lists {{{f}}}, but it appears on every site, which makes it part of the file identity; remove it from fanout, or use each to stamp out one relation per value");
        }
    }
    let mut identity = BTreeSet::new();
    for var in &all {
        if fanout.contains(var) {
            continue;
        }
        let with: Vec<&str> = per_site.iter().filter(|(_, v)| v.contains(var)).map(|(n, _)| *n).collect();
        let without: Vec<&str> = per_site.iter().filter(|(_, v)| !v.contains(var)).map(|(n, _)| *n).collect();
        if without.is_empty() {
            identity.insert(var.clone());
            continue;
        }
        let hint = if var == "path" {
            format!(" If {} should be a folder, add a trailing slash to its pattern.", without.join(" and "))
        } else {
            String::new()
        };
        bail!(
            "variable {{{var}}} appears on {} but not on {}. Add it to every site to make it part of the file identity, or list it under fanout to give {} one replica per value.{hint}",
            with.join(", "), without.join(", "), with.join(", ")
        );
    }
    validate_vars(sites, &identity, &fanout)?;
    Ok((identity, fanout))
}

/// Build the shell command for an `on` or `transform` entry, with its own timeout or the default.
fn shell_of(cmd: &CommandText, what: &str) -> Result<Shell> {
    let timeout = match &cmd.timeout {
        Some(t) => parse_duration(t).with_context(|| format!("{what}: timeout"))?,
        None => DEFAULT_TIMEOUT,
    };
    if cmd.run.trim().is_empty() {
        bail!("{what}: the command is empty");
    }
    Ok(Shell { text: cmd.run.clone(), timeout })
}

/// Check the expanded text of a relation and build it.
fn assemble_relation(name: &str, text: RelationText, settings: &Settings) -> Result<Relation> {
    let mut sites = build_sites(&text.sites)?;
    apply_where(&mut sites, &text.where_)?;
    let (identity, fanout) = classify(&sites, &text.fanout)?;
    let policy = Policy::parse(&text.flow, &text.resolve).context("flow")?;
    if policy.flow.is_empty() {
        bail!("flow carries nothing, so nothing would sync; write at least one edge such as \"source <-> dest\"");
    }
    let declared: BTreeSet<&str> = sites.iter().map(|s| s.name.as_str()).collect();
    for site in policy.flow.sites().iter().chain(rule_sites(&policy.resolve).iter()) {
        if !declared.contains(site) {
            bail!("flow or resolve refers to undeclared site {site:?} (declared: {})", declared.iter().cloned().collect::<Vec<_>>().join(", "));
        }
    }
    let mut transforms = BTreeMap::new();
    for (edge, cmd) in &text.transform {
        let (from, to) = edge.split_once("->").with_context(|| format!("transform {edge:?}: write the edge as \"from -> to\""))?;
        let (from, to) = (from.trim(), to.trim());
        for s in [from, to] {
            if !declared.contains(s) {
                bail!("transform {edge:?} refers to undeclared site {s:?}");
            }
        }
        if Kind::ALL.iter().any(|k| policy.flow.has_outgoing(*k, to)) {
            bail!("transform {edge:?}: site {to:?} receives transformed content, so it may have no outgoing edges in flow");
        }
        transforms.insert((from.to_owned(), to.to_owned()), shell_of(cmd, &format!("transform {edge:?}"))?);
    }
    let mut actions = Actions::default();
    for (word, cmd) in &text.on {
        let trigger = Trigger::parse(word).with_context(|| format!("on.{word}: unknown event (use changed, resolved, held, conflict or error)"))?;
        actions.on.insert(trigger, shell_of(cmd, &format!("on.{word}"))?);
    }
    let mut ignore_text = settings.ignore.clone();
    ignore_text.extend(text.ignore.iter().cloned());
    let ignore = compile_ignore(&ignore_text).context("ignore")?;
    let log = text.log.as_deref().map(|l| absolute_path(l, "log")).transpose()?;
    Ok(Relation {
        name: name.to_owned(),
        sites,
        identity,
        fanout,
        policy,
        ignore,
        ignore_text,
        sync_times: text.sync_times,
        gate_default: text.gate,
        watch: text.watch,
        actions,
        transforms,
        log,
    })
}

/// The paths the daemon writes to: the state folder and each relation's log file. A site root may not contain one of these or lie inside one.
pub fn reserved_paths(settings: &Settings, relations: &[Relation]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = settings.state.iter().cloned().collect();
    out.extend(relations.iter().filter_map(|r| r.log.clone()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_text(yaml: &str) -> Result<Loaded> {
        load(yaml, Path::new("/cfg/config.yaml"))
    }

    fn load_err(yaml: &str) -> String {
        match load_text(yaml) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("config was accepted:\n{yaml}"),
        }
    }

    const PAIR: &str = "relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    flow: \"source <-> dest\"\n";

    #[test]
    fn a_plain_pair_loads_with_defaults() {
        let loaded = load_text(PAIR).unwrap();
        assert_eq!(loaded.settings.interval, Duration::from_secs(30));
        assert!(loaded.settings.watch);
        let r = &loaded.relations[0];
        assert_eq!(r.identity, BTreeSet::from(["path".to_string()]));
        assert!(r.fanout.is_empty());
        assert!(r.policy.flow.carries(Kind::Create, "source", "dest"));
        assert!(!r.policy.flow.carries(Kind::Delete, "source", "dest"));
        assert!(r.sync_times && r.watch && !r.gate_default);
    }

    #[test]
    fn settings_are_resolved() {
        let loaded = load_text(&format!("state: /var/replica\ninterval: 2m\ndebounce: 500ms\ngate: true\nignore: [\"**/.DS_Store\"]\narchive: {{ cap: 10M }}\ndashboard: {{ port: 1 }}\n{PAIR}")).unwrap();
        let s = &loaded.settings;
        assert_eq!(s.db_path().unwrap(), PathBuf::from("/var/replica/state.db"));
        assert_eq!(s.blob_dir().unwrap(), PathBuf::from("/var/replica/blobs"));
        assert_eq!(s.interval, Duration::from_secs(120));
        assert_eq!(s.debounce, Duration::from_millis(500));
        assert!(s.gate);
        assert_eq!(s.archive_cap, 10 << 20);
        assert_eq!(s.dashboard.as_ref().unwrap().port, 1);
        assert_eq!(loaded.relations[0].ignore_text, vec!["**/.DS_Store"]);
        assert!(load_err(&format!("interval: 0\n{PAIR}")).starts_with("interval:"));
        assert!(load_err(&format!("archive: {{ cap: 10 parsecs }}\n{PAIR}")).starts_with("archive.cap:"));
        assert!(load_err(&format!("dashboard: {{ port: abc }}\n{PAIR}")).contains("dashboard.port"));
        assert!(load_err(&format!("state: live\n{PAIR}")).contains("absolute"));
    }

    #[test]
    fn profiles_variables_and_each_compose() {
        let yaml = "state: /live\nvars: { projects: /p }\nprofiles:\n  _: { log: \"${state}/logs/${relation}.log\" }\n  mirror: { flow: \"source -> dest [all]\", resolve: mtime }\nrelations:\n  rules:\n    profile: mirror\n    each: { project: [alpha, beta] }\n    sites: { source: /rules/, dest: \"${projects}/${project}/docs/\" }\n    vars: { extra: \"${project}-x\" }\n";
        let loaded = load_text(yaml).unwrap();
        let names: Vec<&str> = loaded.relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["rules-alpha", "rules-beta"]);
        let r = &loaded.relations[1];
        assert_eq!(r.sites[1].locations[0].pattern.text(), "/p/beta/docs/{path...}");
        assert_eq!(r.log.as_deref(), Some(Path::new("/live/logs/rules-beta.log")));
        assert!(r.policy.flow.carries(Kind::Delete, "source", "dest"));
        let prov = &loaded.provenance["rules-beta"];
        assert_eq!(prov.profiles, vec!["mirror"]);
        assert_eq!(prov.vars["extra"], "beta-x");
    }

    #[test]
    fn errors_name_the_relation_site_and_field() {
        let err = load_err("relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    flow: \"source <-> dest\"\n    patern: x\n");
        assert!(err.contains("relation \"r\"") && err.contains("unknown field `patern`"), "{err}");
        let err = load_err("profiles:\n  p: { patern: x }\nrelations:\n  r:\n    profile: p\n    sites: { source: /a/, dest: /b/ }\n    flow: \"source <-> dest\"\n");
        assert!(err.contains("profile \"p\"") && err.contains("patern"), "{err}");
        let err = load_err("relations:\n  r:\n    profile: nope\n    sites: { source: /a/, dest: /b/ }\n    flow: \"source <-> dest\"\n");
        assert!(err.contains("profile \"nope\" is not defined"), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    flow: \"source <-> other\"\n");
        assert!(err.contains("undeclared site \"other\""), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    flow: \"\"\n");
        assert!(err.contains("carries nothing"), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    flow: \"source <-> dest\"\n    log: logs/r.log\n");
        assert!(err.contains("log") && err.contains("absolute"), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { source: relative/, dest: /b/ }\n    flow: \"source <-> dest\"\n");
        assert!(err.contains("site \"source\"") && err.contains("absolute"), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { source: /a/ }\n    flow: \"source <-> dest\"\n");
        assert!(err.contains("at least two"), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    flow: \"source <-> dest\"\n    on: { lunch: echo }\n");
        assert!(err.contains("on.lunch"), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    flow: \"source <-> dest\"\n    log: \"${nope}/x.log\"\n");
        assert!(err.contains("undefined variable"), "{err}");
    }

    #[test]
    fn variables_on_some_sites_must_be_declared_fanout() {
        let err = load_err("relations:\n  r:\n    sites: { source: /rules/main.md, dest: \"/p/{project}/docs/main.md\" }\n    flow: \"source -> dest\"\n");
        assert!(err.contains("{project}") && err.contains("fanout"), "{err}");
        let ok = load_text("relations:\n  r:\n    sites: { source: /rules/main.md, dest: \"/p/{project}/docs/main.md\" }\n    fanout: [project]\n    flow: \"source -> dest\"\n").unwrap();
        assert!(ok.relations[0].identity.is_empty());
        assert_eq!(ok.relations[0].fanout, BTreeSet::from(["project".to_string()]));
        // A file pattern on one site and a folder pattern on the other gets the trailing-slash hint.
        let err = load_err("relations:\n  r:\n    sites: { source: /a, dest: /b/ }\n    flow: \"source <-> dest\"\n");
        assert!(err.contains("trailing slash"), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    fanout: [zzz]\n    flow: \"source <-> dest\"\n");
        assert!(err.contains("no site has that variable"), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { source: \"/a/{p}/\", dest: \"/b/{p}/\" }\n    fanout: [p]\n    flow: \"source <-> dest\"\n");
        assert!(err.contains("every site"), "{err}");
        let err = load_err("relations:\n  r:\n    sites: { a: \"/x/{name}.md\", b: \"/y/{other}/f.md\" }\n    flow: \"a <-> b\"\n");
        assert!(err.contains("{name}") || err.contains("{other}"), "{err}");
    }

    #[test]
    fn where_constrains_variables_and_lists_are_locations() {
        let loaded = load_text("relations:\n  r:\n    sites:\n      source: /a/\n      dest: [/b/, /c/]\n      named: { x: /d/, y: /e/ }\n    where: { path: [\"*.js\", \"!*.min.js\"] }\n    flow: \"source -> {dest, named}\"\n").unwrap();
        let r = &loaded.relations[0];
        assert_eq!(r.sites[1].locations.iter().map(|l| l.label.clone().unwrap()).collect::<Vec<_>>(), vec!["/b/", "/c/"]);
        assert_eq!(r.sites[2].locations[0].label.as_deref(), Some("x"));
        assert!(r.sites[0].locations[0].pattern.matches(Path::new("/a/app.js")).is_some());
        assert!(r.sites[0].locations[0].pattern.matches(Path::new("/a/app.min.js")).is_none());
        assert!(load_err("relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    where: { nope: \"*\" }\n    flow: \"source <-> dest\"\n").contains("no site has a variable {nope}"));
        assert!(load_err("relations:\n  r:\n    sites: { source: /a/, dest: /b/ }\n    where: { path: \"[\" }\n    flow: \"source <-> dest\"\n").contains("where.path"));
    }

    #[test]
    fn transforms_need_a_sink_site() {
        let yaml = "relations:\n  r:\n    sites: { src: /a/, out: /b/ }\n    flow: \"src -> out [edit create]\"\n    transform: { \"src -> out\": \"sassc ${in} ${out}\" }\n";
        let loaded = load_text(yaml).unwrap();
        assert_eq!(loaded.relations[0].transforms[&("src".to_string(), "out".to_string())].text, "sassc ${in} ${out}");
        let err = load_err("relations:\n  r:\n    sites: { src: /a/, out: /b/ }\n    flow: \"src <-> out\"\n    transform: { \"src -> out\": \"cmd\" }\n");
        assert!(err.contains("no outgoing edges"), "{err}");
    }

    #[test]
    fn includes_merge_and_refuse_duplicates() {
        let tmp = tempfile::TempDir::new().unwrap();
        let main = tmp.path().join("config.yaml");
        std::fs::create_dir_all(tmp.path().join("rel")).unwrap();
        std::fs::write(tmp.path().join("rel/a.yaml"), "relations:\n  a:\n    sites: { s: /a/, d: /b/ }\n    flow: \"s <-> d\"\n").unwrap();
        std::fs::write(&main, "include: [\"rel/*.yaml\"]\nrelations:\n  b:\n    sites: { s: /c/, d: /d/ }\n    flow: \"s <-> d\"\n").unwrap();
        let loaded = load_file(&main).unwrap();
        let mut names: Vec<&str> = loaded.relations.iter().map(|r| r.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
        std::fs::write(&main, "include: [\"rel/*.yaml\"]\nrelations:\n  a:\n    sites: { s: /c/, d: /d/ }\n    flow: \"s <-> d\"\n").unwrap();
        let err = match load_file(&main) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("a duplicate relation was accepted"),
        };
        assert!(err.contains("defined in both"), "{err}");
    }

    #[test]
    fn merge_keys_and_file_order_are_kept() {
        let yaml = "relations:\n  base: &base\n    sites: { a: /a/, b: /b/ }\n    flow: \"a <-> b\"\n  other:\n    <<: *base\n    where: { path: \"*.md\" }\n  zeta:\n    sites: { a: /z/, b: /y/ }\n    flow: \"a <-> b\"\n";
        let loaded = load_text(yaml).unwrap();
        let names: Vec<&str> = loaded.relations.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["base", "other", "zeta"]);
        assert!(loaded.relations[1].sites[0].locations[0].pattern.matches(Path::new("/a/x.md")).is_some());
    }

    #[test]
    fn durations_and_sizes_parse() {
        assert!(parse_duration("0").is_err());
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_size("1.5G").unwrap(), (1.5 * (1u64 << 30) as f64) as u64);
        assert_eq!(parse_size("42").unwrap(), 42);
    }
}
