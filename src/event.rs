//! Events the engine emits as it works. Hooks receive them: loggers, counters, the dashboard's activity list, and the actions.

use std::time::SystemTime;

/// Something that happened during a cycle, tied to a relation and, where it applies, to a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A plan was applied. `detail` describes the operations and `ops` counts them. `paths` lists the paths written or deleted, for actions to use.
    Resolved { relation: String, key: String, ops: usize, detail: String, paths: Vec<String> },
    /// A plan exists but a gate holds it.
    Held { relation: String, key: String, detail: String },
    /// A replica of the key is waiting, either because the rules leave its conflict undecided or because a peer is unreadable.
    Skipped { relation: String, key: String, reason: String },
    /// The relation waits as a whole, as when a root with records is missing.
    Pending { relation: String, reason: String },
    /// Something was skipped or degraded without failing the relation.
    Warned { relation: String, key: String, detail: String },
    Errored { relation: String, key: String, error: String },
    /// A journal row from a run that died mid-plan.
    StaleIntent { relation: String, key: String, detail: String },
    /// A replica's pattern differs from the one its records were made under.
    Relocated { relation: String, replica: String, from: String, to: String },
    /// An action ran. `ok` is false when it exited nonzero or timed out.
    Action { relation: String, name: String, ok: bool, detail: String },
    /// The watcher reported a change under a relation's roots.
    Watched { relation: String, path: String },
    /// A cycle finished over the listed relations.
    Cycle { relations: Vec<String>, ops: usize, errors: usize, millis: u128 },
}

/// Color of an event in logs and on the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Neutral,
    Warn,
    Bad,
}

impl Event {
    /// The relation the event belongs to, empty for a whole-cycle event.
    pub fn relation(&self) -> &str {
        match self {
            Event::Resolved { relation, .. } | Event::Held { relation, .. } | Event::Skipped { relation, .. } | Event::Pending { relation, .. }
            | Event::Warned { relation, .. } | Event::Errored { relation, .. } | Event::StaleIntent { relation, .. } | Event::Relocated { relation, .. }
            | Event::Action { relation, .. } | Event::Watched { relation, .. } => relation,
            Event::Cycle { .. } => "",
        }
    }

    /// The key the event concerns, empty for relation-level events.
    pub fn key(&self) -> &str {
        match self {
            Event::Resolved { key, .. } | Event::Held { key, .. } | Event::Skipped { key, .. } | Event::Warned { key, .. } | Event::Errored { key, .. } | Event::StaleIntent { key, .. } => key,
            _ => "",
        }
    }

    /// One or two words for the kind of event.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Resolved { .. } => "resolved",
            Event::Held { .. } => "held",
            Event::Skipped { .. } => "skipped",
            Event::Pending { .. } => "pending",
            Event::Warned { .. } => "warning",
            Event::Errored { .. } => "error",
            Event::StaleIntent { .. } => "stale intent",
            Event::Relocated { .. } => "relocated",
            Event::Action { .. } => "action",
            Event::Watched { .. } => "watched",
            Event::Cycle { .. } => "cycle",
        }
    }

    /// The display tone.
    pub fn tone(&self) -> Tone {
        match self {
            Event::Resolved { .. } => Tone::Ok,
            Event::Held { .. } | Event::Watched { .. } | Event::Cycle { .. } => Tone::Neutral,
            Event::Skipped { .. } | Event::Pending { .. } | Event::Warned { .. } | Event::Relocated { .. } => Tone::Warn,
            Event::Action { ok: true, .. } => Tone::Ok,
            Event::Action { ok: false, .. } | Event::Errored { .. } | Event::StaleIntent { .. } => Tone::Bad,
        }
    }

    /// The free text, without relation or key.
    pub fn text(&self) -> String {
        match self {
            Event::Resolved { detail, .. } => format!("resolved: {detail}"),
            Event::Held { detail, .. } => format!("held by gate: would {detail}"),
            Event::Skipped { reason, .. } => format!("skipped: {reason}"),
            Event::Pending { reason, .. } => format!("pending: {reason}"),
            Event::Warned { detail, .. } => format!("warning: {detail}"),
            Event::Errored { error, .. } => format!("error: {error}"),
            Event::StaleIntent { detail, .. } => format!("stale intent (previous run died mid-operation): {detail}"),
            Event::Relocated { replica, from, to, .. } => format!("replica {replica} moved from {from} to {to}; its records start over"),
            Event::Action { name, ok, detail, .. } => format!("action {name} {}: {detail}", if *ok { "ran" } else { "failed" }),
            Event::Watched { path, .. } => format!("change noticed at {path}"),
            Event::Cycle { relations, ops, errors, millis } => format!("cycle over {} relation(s) in {millis}ms: {ops} operation(s), {errors} error(s)", relations.len()),
        }
    }
}

/// Callback receiving each event.
pub type Hook = Box<dyn Fn(&Event) + Send + Sync>;

/// An event with the time it was emitted, as the dashboard keeps them.
#[derive(Clone, Debug)]
pub struct Stamped {
    pub at: SystemTime,
    pub event: Event,
}

impl Stamped {
    /// Stamp an event with the current time.
    pub fn now(event: &Event) -> Stamped {
        Stamped { at: SystemTime::now(), event: event.clone() }
    }
}
