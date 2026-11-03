//! Events the engine reports as it works. Each event goes to the hooks, which write log lines, update the counters, and fill the dashboard's list of recent events.

use std::time::SystemTime;

/// Something that happened during a cycle, tied to a relation and, where it applies, to a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A key's plan was applied. `detail` describes the operations, `ops` counts them, and `paths` lists the files written or deleted.
    Resolved { relation: String, key: String, ops: usize, detail: String, paths: Vec<String> },
    /// The key has a plan, but a closed gate holds it back.
    Held { relation: String, key: String, detail: String },
    /// A replica of the key is held. `reason` says why: a conflict that no resolver rule decides, or an unreadable file.
    Skipped { relation: String, key: String, reason: String },
    /// The whole relation is waiting. `reason` says why, for example a missing folder for a replica that has records.
    Pending { relation: String, reason: String },
    /// A problem that is reported but leaves the relation running, such as a failure during the key walk.
    Warned { relation: String, key: String, detail: String },
    /// Deciding or applying a key failed. The key is empty if the relation failed as a whole.
    Errored { relation: String, key: String, error: String },
    /// An intent row left by a process that died while applying a plan.
    StaleIntent { relation: String, key: String, detail: String },
    /// The pattern behind a replica's id has changed since its records were made.
    Relocated { relation: String, replica: String, from: String, to: String },
    /// An action ran. `ok` is false if the command failed to start, exited nonzero, or timed out.
    Action { relation: String, name: String, ok: bool, detail: String },
    /// The watcher reported a change under a relation's roots.
    Watched { relation: String, path: String },
    /// A cycle over the listed relations finished.
    Cycle { relations: Vec<String>, ops: usize, errors: usize, millis: u128 },
}

/// The color an event gets in logs and on the dashboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Neutral,
    Warn,
    Bad,
}

impl Event {
    /// The event's relation, or an empty string for a cycle event.
    pub fn relation(&self) -> &str {
        match self {
            Event::Resolved { relation, .. } | Event::Held { relation, .. } | Event::Skipped { relation, .. } | Event::Pending { relation, .. }
            | Event::Warned { relation, .. } | Event::Errored { relation, .. } | Event::StaleIntent { relation, .. } | Event::Relocated { relation, .. }
            | Event::Action { relation, .. } | Event::Watched { relation, .. } => relation,
            Event::Cycle { .. } => "",
        }
    }

    /// The event's key, or an empty string if the event is about the relation as a whole.
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

    /// The tone used to color the event.
    pub fn tone(&self) -> Tone {
        match self {
            Event::Resolved { .. } => Tone::Ok,
            Event::Held { .. } | Event::Watched { .. } | Event::Cycle { .. } => Tone::Neutral,
            Event::Skipped { .. } | Event::Pending { .. } | Event::Warned { .. } | Event::Relocated { .. } => Tone::Warn,
            Event::Action { ok: true, .. } => Tone::Ok,
            Event::Action { ok: false, .. } | Event::Errored { .. } | Event::StaleIntent { .. } => Tone::Bad,
        }
    }

    /// The event's message, without the relation or key.
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

/// A callback the engine calls with each event it emits.
pub type Hook = Box<dyn Fn(&Event) + Send + Sync>;

/// An event and the time it was emitted, as kept in the dashboard's list of recent events.
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
