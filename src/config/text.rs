//! The configuration file as typed structures, before validation. Unknown fields are rejected at every level.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_norway::{Mapping, Value};

/// The top level of the main file and of included files.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FileText {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default, deserialize_with = "string_or_number")]
    pub interval: Option<String>,
    #[serde(default)]
    pub watch: Option<bool>,
    #[serde(default, deserialize_with = "string_or_number")]
    pub debounce: Option<String>,
    #[serde(default)]
    pub gate: Option<bool>,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default)]
    pub archive: Option<ArchiveText>,
    #[serde(default)]
    pub dashboard: Option<DashboardText>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// Profiles stay raw until a relation merges them, so a typo inside one is reported against the profile.
    #[serde(default)]
    pub profiles: Mapping,
    /// Relations stay raw until profiles and variables are merged in.
    #[serde(default)]
    pub relations: Mapping,
}

/// Archive settings.
#[derive(Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ArchiveText {
    #[serde(default, deserialize_with = "string_or_number")]
    pub cap: Option<String>,
}

/// Dashboard settings.
#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DashboardText {
    pub port: u16,
    #[serde(default = "default_recent_events")]
    pub recent_events: usize,
    #[serde(default = "default_refresh")]
    pub refresh: u64,
}

fn default_recent_events() -> usize { 200 }
fn default_refresh() -> u64 { 10 }

/// A relation after profiles are merged and variables expanded.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RelationText {
    #[serde(default)]
    pub profile: Vec<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub each: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub sites: Mapping,
    #[serde(default)]
    pub where_: BTreeMap<String, Value>,
    #[serde(default)]
    pub fanout: Vec<String>,
    #[serde(default, deserialize_with = "string_or_null")]
    pub flow: String,
    #[serde(default, deserialize_with = "string_or_null")]
    pub resolve: String,
    #[serde(default)]
    pub ignore: Vec<String>,
    #[serde(default = "default_true")]
    pub sync_times: bool,
    #[serde(default)]
    pub gate: bool,
    #[serde(default = "default_true")]
    pub watch: bool,
    #[serde(default)]
    pub log: Option<String>,
    #[serde(default)]
    pub on: BTreeMap<String, CommandText>,
    #[serde(default)]
    pub transform: BTreeMap<String, CommandText>,
}

fn default_true() -> bool { true }

/// The field names a relation accepts, for errors about unknown fields inside profiles.
pub const RELATION_FIELDS: [&str; 16] = ["profile", "vars", "each", "sites", "where", "fanout", "flow", "resolve", "ignore", "sync_times", "gate", "watch", "log", "on", "transform", "profiles"];

/// A command as a bare string or as a mapping with its timeout.
#[derive(Clone, Debug)]
pub struct CommandText {
    pub run: String,
    pub timeout: Option<String>,
}

impl<'de> Deserialize<'de> for CommandText {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<CommandText, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full { run: String, #[serde(default, deserialize_with = "string_or_number")] timeout: Option<String> }
        match Value::deserialize(d)? {
            Value::String(run) => Ok(CommandText { run, timeout: None }),
            v @ Value::Mapping(_) => {
                let full: Full = serde_norway::from_value(v).map_err(serde::de::Error::custom)?;
                Ok(CommandText { run: full.run, timeout: full.timeout })
            }
            other => Err(serde::de::Error::custom(format!("a command is a string or a mapping with run and timeout, not {}", yaml_kind(&other)))),
        }
    }
}

/// Accept a bare number as well as a string, so `interval: 0` reaches the duration check instead of failing as a type mismatch.
pub fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s)),
        Value::Number(n) => Ok(Some(n.to_string())),
        other => Err(serde::de::Error::custom(format!("expected a string such as \"30s\" or \"100MB\", not {}", yaml_kind(&other)))),
    }
}

/// Accept a string or a YAML null, so a statement field can be left blank.
pub fn string_or_null<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// Describe the YAML type of a value for error messages.
pub fn yaml_kind(v: &Value) -> &'static str {
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

/// Rename `where_` to `where` on the way in, since `where` is a Rust keyword.
pub fn rename_where(body: &mut Mapping) {
    if let Some(v) = body.remove(Value::String("where".into())) {
        body.insert(Value::String("where_".into()), v);
    }
}
