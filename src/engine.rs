//! The engine runs cycles over the configured relations. For each relation it expands the sites into replicas, skips the relation if its roots overlap another's, and finds the keys. For each key it observes the file at each replica, decides what each replica should hold, and applies the plan. A key whose files all match their records is skipped. When the relation is gated, the engine reports the plan and writes nothing.
//!
//! A condition that persists across cycles, such as a conflict, is reported when it appears and again when it changes, and stays quiet in between.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash as _, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use globset::GlobMatcher;

use crate::action::{Actions, Shell, Trigger};
use crate::apply::{self, Executor};
use crate::blobs::Blobs;
use crate::decide::{Assignment, Op, Situation, Slot, decide, describe_plan};
use crate::event::{Event, Hook};
use crate::flow::{Kind, Policy};
use crate::medium::{EntryKind, Medium};
use crate::observe::{Basis, Observation, Snapshot, Transition, classify};
use crate::site::{Key, Replica, Site, discover_keys, display_names};
use crate::store::{Intent, Noted, RecordMap, Store, to_epoch_ms};

/// Set by a signal handler or a dashboard command. The engine checks it between keys and ends the cycle early when it is set.
pub static STOP: AtomicBool = AtomicBool::new(false);

// Keys for tracking relation-wide conditions in `outcomes`. They start with a NUL character so they cannot collide with the canonical text of a real key.
const KEY_EXPAND: &str = "\0expand";
const KEY_OVERLAP: &str = "\0overlap";
const KEY_ROOT: &str = "\0root";
const KEY_WALK: &str = "\0walk";
const KEY_ERROR: &str = "\0error";

/// A relation from the configuration, in the form the engine runs.
pub struct Relation {
    pub name: String,
    pub sites: Vec<Site>,
    pub identity: BTreeSet<String>,
    pub fanout: BTreeSet<String>,
    pub policy: Policy,
    pub ignore: Vec<GlobMatcher>,
    pub ignore_text: Vec<String>,
    pub sync_times: bool,
    /// Whether the relation's gate starts closed.
    pub gate_default: bool,
    /// Whether the watcher registers the relation's roots.
    pub watch: bool,
    pub actions: Actions,
    /// Transform commands for each edge, keyed by (from site, to site).
    pub transforms: BTreeMap<(String, String), Shell>,
    pub log: Option<PathBuf>,
}

impl Relation {
    /// A hash of the settings that affect decisions. If it differs from the stored fingerprint, the engine decides each key again instead of skipping the settled ones, until an ungated cycle stores the new fingerprint.
    pub fn fingerprint(&self) -> String {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for site in &self.sites {
            site.name.hash(&mut h);
            for loc in &site.locations {
                loc.pattern.text().hash(&mut h);
                for v in loc.pattern.vars() {
                    for c in &v.constraints {
                        c.text().hash(&mut h);
                    }
                }
            }
        }
        self.policy.fingerprint_text().hash(&mut h);
        self.ignore_text.hash(&mut h);
        for ((a, b), s) in &self.transforms {
            (a, b, &s.text).hash(&mut h);
        }
        format!("{:016x}", h.finish())
    }
}

/// The global gate and the per-relation gates, held in memory. While a gate is closed, the engine still finds keys and plans, but it writes nothing to the files or the database.
#[derive(Default)]
pub struct Gates {
    global: AtomicBool,
    relations: Mutex<BTreeMap<String, bool>>,
}

impl Gates {
    /// True when the global gate is closed.
    pub fn globally_gated(&self) -> bool {
        self.global.load(Ordering::Relaxed)
    }

    /// True when the relation's own gate is closed, ignoring the global gate.
    pub fn relation_flag(&self, relation: &str) -> bool {
        *self.relations.lock().unwrap().get(relation).unwrap_or(&false)
    }

    /// True when the global gate or the relation's gate is closed.
    pub fn is_gated(&self, relation: &str) -> bool {
        self.globally_gated() || self.relation_flag(relation)
    }

    /// Opens or closes the global gate in memory only.
    pub fn set_global(&self, gated: bool) {
        self.global.store(gated, Ordering::Relaxed);
    }

    /// Opens or closes a relation's gate in memory only.
    pub fn set_relation(&self, relation: &str, gated: bool) {
        self.relations.lock().unwrap().insert(relation.into(), gated);
    }
}

/// Runs cycles over the relations, using the database, the blob store, and a medium for file operations.
pub struct Engine {
    pub store: Store,
    pub blobs: Blobs,
    pub medium: Box<dyn Medium>,
    pub relations: Vec<Relation>,
    pub hooks: Vec<Hook>,
    pub gates: Arc<Gates>,
    /// Paths the daemon itself writes to. No root may contain one of these paths or lie inside one.
    pub reserved: Vec<PathBuf>,
    /// The directory actions and transforms run in, which is the state folder.
    pub workdir: PathBuf,
    /// A hash of the last reported outcome for each (relation, key). The engine compares new outcomes against it, so a condition that persists is reported only when it appears or changes.
    outcomes: Mutex<BTreeMap<(String, String), u64>>,
    /// Relations whose stored fingerprint differs from the configured one. Their keys are decided in full on each cycle until an ungated cycle stores the new fingerprint.
    stale_policy: Mutex<BTreeSet<String>>,
}

/// Counts of the operations applied and the errors in a cycle.
#[derive(Debug, Default, Clone, Copy)]
pub struct CycleSummary {
    pub ops: usize,
    pub errors: usize,
}

/// The observed state of a key at each replica, used by both the cycle and the report.
struct Inspection {
    situation: Situation,
    /// No replica has the file.
    all_missing: bool,
    /// Each replica matches its record, no hold is open on the key, and no full pass was asked for, so the key can be skipped.
    settled: bool,
}

/// The keys found for a relation, along with the folders the walk could not read and the recorded keys that the patterns or the ignore list now exclude.
struct Discovery {
    keys: BTreeSet<Key>,
    failures: Vec<String>,
    dropped: Vec<Key>,
}

impl Engine {
    /// Builds an engine. A relation's gate starts closed if its configuration or its persisted flag says so.
    pub fn new(store: Store, blobs: Blobs, medium: Box<dyn Medium>, relations: Vec<Relation>, hooks: Vec<Hook>, workdir: PathBuf) -> Engine {
        let gates = Gates::default();
        let mut stale = BTreeSet::new();
        for rel in &relations {
            let persisted = store.flag(&format!("gate.{}", rel.name)).ok().flatten().unwrap_or(false);
            if rel.gate_default || persisted {
                gates.set_relation(&rel.name, true);
            }
            if store.fingerprint(&rel.name).ok().flatten().as_deref() != Some(rel.fingerprint().as_str()) {
                stale.insert(rel.name.clone());
            }
        }
        if store.flag("gate").ok().flatten().unwrap_or(false) {
            gates.set_global(true);
        }
        Engine { store, blobs, medium, relations, hooks, gates: Arc::new(gates), reserved: Vec::new(), workdir, outcomes: Mutex::default(), stale_policy: Mutex::new(stale) }
    }

    /// Opens or closes the global gate and persists the flag across restarts.
    pub fn set_global_gate(&self, gated: bool) {
        self.gates.set_global(gated);
        let _ = self.store.set_flag("gate", gated);
    }

    /// Opens or closes a relation's gate and persists the flag across restarts.
    pub fn set_relation_gate(&self, relation: &str, gated: bool) {
        self.gates.set_relation(relation, gated);
        let _ = self.store.set_flag(&format!("gate.{relation}"), gated);
    }

    /// Passes an event to each hook.
    pub fn emit(&self, event: Event) {
        for hook in &self.hooks {
            hook(&event);
        }
    }

    /// Stores a hash of `value` under (relation, key) and returns true if it differs from the last one stored, or if nothing was stored yet.
    fn changed(&self, relation: &str, key: &str, value: &str) -> bool {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut h);
        let digest = h.finish();
        self.outcomes.lock().unwrap().insert((relation.to_owned(), key.to_owned()), digest) != Some(digest)
    }

    /// Clears the stored outcome for (relation, key), so the condition is reported again if it comes back.
    fn clear_outcome(&self, relation: &str, key: &str) {
        self.outcomes.lock().unwrap().remove(&(relation.to_owned(), key.to_owned()));
    }

    /// Reports and removes the intent rows left by a process that died mid-plan.
    pub fn recover(&self) -> Result<Vec<Intent>> {
        let stale = self.store.stale_intents()?;
        for intent in &stale {
            self.emit(Event::StaleIntent { relation: intent.relation.clone(), key: intent.key.to_string(), detail: intent.detail.clone() });
            self.store.clear_intent(&intent.relation, &intent.key)?;
        }
        self.blobs.clean_staging();
        Ok(stale)
    }

    /// Expands a relation's sites into replicas through the engine's medium.
    pub fn expand(&self, rel: &Relation) -> Result<Vec<Replica>> {
        expand(rel, self.medium.as_ref())
    }

    /// Expands each relation, or only the named ones, and returns the successes and the failures separately, by relation index.
    fn expand_all(&self, only: Option<&BTreeSet<String>>) -> (Vec<(usize, Vec<Replica>)>, Vec<(usize, String)>) {
        let mut expanded = Vec::new();
        let mut failed = Vec::new();
        for (i, rel) in self.relations.iter().enumerate() {
            if only.is_some_and(|set| !set.contains(&rel.name)) {
                continue;
            }
            match self.expand(rel) {
                Ok(replicas) => expanded.push((i, replicas)),
                Err(e) => failed.push((i, format!("{e:#}"))),
            }
        }
        (expanded, failed)
    }

    /// Runs a cycle over all relations, or only over the named ones.
    pub fn cycle(&self, only: Option<&BTreeSet<String>>) -> Result<CycleSummary> {
        let started = Instant::now();
        let mut summary = CycleSummary::default();
        let (expanded, failed) = self.expand_all(only);
        for (i, error) in &failed {
            let rel = &self.relations[*i].name;
            summary.errors += 1;
            if self.changed(rel, KEY_EXPAND, error) {
                self.emit(Event::Errored { relation: rel.clone(), key: String::new(), error: error.clone() });
            }
        }
        for (i, _) in &expanded {
            self.clear_outcome(&self.relations[*i].name, KEY_EXPAND);
        }
        // Overlaps are checked across all relations, even ones outside this cycle, because a fan-out replica in a skipped relation can overlap a root in one that runs.
        let all = if only.is_some() { self.expand_all(None).0 } else { expanded.clone() };
        let conflicts = footprint_conflicts(&self.relations, &all, &self.reserved);
        let skip: BTreeSet<usize> = conflicts.iter().flat_map(|c| c.relations.iter().copied()).collect();
        for i in &skip {
            let rel = &self.relations[*i].name;
            let own: Vec<&str> = conflicts.iter().filter(|c| c.relations.contains(i)).map(|c| c.message.as_str()).collect();
            if self.changed(rel, KEY_OVERLAP, &own.join("\n")) {
                for message in own {
                    self.emit(Event::Errored { relation: rel.clone(), key: String::new(), error: message.to_owned() });
                }
            }
        }
        for (i, _) in &expanded {
            if !skip.contains(i) {
                self.clear_outcome(&self.relations[*i].name, KEY_OVERLAP);
            }
        }
        let mut names = Vec::new();
        for (i, replicas) in &expanded {
            if STOP.load(Ordering::Relaxed) {
                return Ok(summary);
            }
            if skip.contains(i) {
                summary.errors += 1;
                continue;
            }
            let rel = &self.relations[*i];
            names.push(rel.name.clone());
            match self.run_relation(rel, replicas) {
                Ok(s) => {
                    self.clear_outcome(&rel.name, KEY_ERROR);
                    summary.ops += s.ops;
                    summary.errors += s.errors;
                }
                Err(e) => {
                    summary.errors += 1;
                    let error = format!("{e:#}");
                    if self.changed(&rel.name, KEY_ERROR, &error) {
                        self.emit(Event::Errored { relation: rel.name.clone(), key: String::new(), error });
                    }
                }
            }
        }
        self.emit(Event::Cycle { relations: names, ops: summary.ops, errors: summary.errors, millis: started.elapsed().as_millis() });
        Ok(summary)
    }

    /// Records each replica's pattern in the database. If a replica's pattern changed, its records are dropped, unless the relation is gated. Returns true if any replica now starts without records.
    fn note_replicas(&self, rel: &Relation, replicas: &[Replica]) -> Result<bool> {
        let gated = self.gates.is_gated(&rel.name);
        let mut fresh = false;
        for r in replicas {
            match self.store.note_replica(&rel.name, r.id(), Path::new(r.pattern.text()))? {
                Noted::Same => {}
                // A new replica has no records, so keys that are settled among the other replicas must be decided again to fill it.
                // Under a gate no files are written, so the row is removed and the replica stays new until an ungated cycle fills it.
                Noted::New => {
                    fresh = true;
                    if gated {
                        self.store.forget_replica(&rel.name, r.id())?;
                    }
                }
                Noted::Moved(previous) => {
                    self.emit(Event::Relocated { relation: rel.name.clone(), replica: r.id().to_owned(), from: previous.clone(), to: r.pattern.text().to_owned() });
                    if !gated {
                        self.store.forget_replica(&rel.name, r.id())?;
                        fresh = true;
                    } else {
                        // Under a gate, the old pattern is written back so the move is reported again when the gate opens.
                        self.store.note_replica(&rel.name, r.id(), Path::new(&previous))?;
                    }
                }
            }
        }
        Ok(fresh)
    }

    /// Finds a relation's keys from the files under each replica and the keys in its records. A recorded key that the patterns or the ignore list now exclude is returned as dropped.
    fn discover(&self, rel: &Relation, replicas: &[Replica], records: &RecordMap) -> Result<Discovery> {
        let mut keys = BTreeSet::new();
        let mut failures = Vec::new();
        for r in replicas {
            let found = discover_keys(r, &rel.identity, &rel.ignore, self.medium.as_ref())?;
            failures.extend(found.failures);
            keys.extend(found.keys);
        }
        // A key is kept if each replica can place it at a path that matches its pattern and is not ignored.
        let admitted = |key: &Key| replicas.iter().all(|r| match r.locate(key) {
            Ok(path) => r.pattern.matches(&path).is_some() && !r.pattern.tail(&path).is_some_and(|t| rel.ignore.iter().any(|g| g.is_match(&t))),
            Err(_) => false,
        });
        keys.retain(|k| admitted(k));
        let mut dropped = Vec::new();
        for text in records.keys() {
            let key = Key::parse(text);
            if admitted(&key) {
                keys.insert(key);
            } else {
                dropped.push(key);
            }
        }
        Ok(Discovery { keys, failures, dropped })
    }

    /// A message about the first replica whose root folder is missing although it has recorded files. Its volume may be unmounted, so the daemon skips the relation instead of treating the files as deleted.
    fn unavailable_root(&self, replicas: &[Replica], records: &RecordMap) -> Option<String> {
        replicas.iter().find(|r| {
            !self.medium.is_dir(r.root()) && records.values().any(|by| matches!(by.get(r.id()), Some(Basis::Snapshot(_))))
        }).map(|r| format!("{} has records but the folder is missing; treating it as unavailable, not deleted", r.root().display()))
    }

    /// Observes a key at each replica and compares each file with its record.
    fn inspect(&self, replicas: &[Replica], key: &Key, records: &RecordMap, unresolved: &BTreeSet<String>, full: bool) -> Result<Inspection> {
        let text = key.canonical();
        let by_replica = records.get(&text);
        let mut slots = Vec::with_capacity(replicas.len());
        let mut all_recorded = true;
        for r in replicas {
            let basis = by_replica.and_then(|m| m.get(r.id())).cloned().unwrap_or(Basis::None);
            let path = r.locate(key)?;
            let observation = observe(self.medium.as_ref(), &path, basis.snapshot());
            // A replica with no file and no record has nothing to decide. A file without a record is new, so the key has to be decided.
            all_recorded &= basis.is_recorded() || observation == Observation::Missing;
            slots.push(Slot { id: r.id().to_owned(), site: r.site.clone(), path, basis, observation });
        }
        let all_missing = slots.iter().all(|s| s.observation == Observation::Missing);
        let settled = !full && all_recorded && slots.iter().all(|s| s.transition() == Transition::Unchanged) && !unresolved.contains(&text);
        Ok(Inspection { situation: Situation { slots }, all_missing, settled })
    }

    /// Runs a relation's part of the cycle. It records the replicas, returns early if a root is unavailable, finds the keys, and then decides and applies each key.
    fn run_relation(&self, rel: &Relation, replicas: &[Replica]) -> Result<CycleSummary> {
        let mut summary = CycleSummary::default();
        let fresh = self.note_replicas(rel, replicas)?;
        let records = self.store.records_for(&rel.name)?;
        if let Some(reason) = self.unavailable_root(replicas, &records) {
            if self.changed(&rel.name, KEY_ROOT, &reason) {
                self.emit(Event::Pending { relation: rel.name.clone(), reason });
            }
            return Ok(summary);
        }
        self.clear_outcome(&rel.name, KEY_ROOT);
        let found = self.discover(rel, replicas, &records)?;
        if self.changed(&rel.name, KEY_WALK, &found.failures.join("\n")) {
            for f in &found.failures {
                self.emit(Event::Warned { relation: rel.name.clone(), key: String::new(), detail: f.clone() });
            }
        }
        let gated = self.gates.is_gated(&rel.name);
        if !gated {
            for key in &found.dropped {
                self.store.forget(&rel.name, key).with_context(|| format!("forgetting {key} after it left the relation"))?;
            }
        }
        let stale = self.stale_policy.lock().unwrap().contains(&rel.name);
        let full = stale || fresh;
        let unresolved = self.store.unresolved_for(&rel.name)?;
        let names = display_names(replicas);
        let name_of: BTreeMap<&str, &str> = replicas.iter().zip(&names).map(|(r, n)| (r.id(), n.as_str())).collect();
        let mut changed_any = false;
        for key in &found.keys {
            if STOP.load(Ordering::Relaxed) {
                return Ok(summary);
            }
            match self.resolve_key(rel, replicas, key, &records, &unresolved, &name_of, full, gated) {
                Ok(ops) => {
                    summary.ops += ops;
                    changed_any |= ops > 0;
                }
                Err(e) => {
                    summary.errors += 1;
                    let error = format!("{e:#}");
                    let text = key.to_string();
                    if self.changed(&rel.name, &key.canonical(), &format!("error {error}")) {
                        self.emit(Event::Errored { relation: rel.name.clone(), key: text.clone(), error: error.clone() });
                        self.fire(rel, Trigger::Error, &[("key", text.as_str()), ("error", error.as_str())]);
                    }
                }
            }
        }
        if stale && !gated {
            self.store.set_fingerprint(&rel.name, &rel.fingerprint())?;
            self.stale_policy.lock().unwrap().remove(&rel.name);
        }
        if changed_any {
            self.fire(rel, Trigger::Changed, &[("ops", &summary.ops.to_string())]);
        }
        Ok(summary)
    }

    /// Runs the relation's action for a trigger, if it has one, with `relation` added to the variables.
    fn fire(&self, rel: &Relation, trigger: Trigger, vars: &[(&str, &str)]) {
        if rel.actions.is_empty() {
            return;
        }
        let mut map: BTreeMap<String, String> = vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        map.insert("relation".into(), rel.name.clone());
        if let Some(event) = rel.actions.fire(trigger, &rel.name, &map, &self.workdir) {
            self.emit(event);
        }
    }

    /// Decides a key and applies the plan, or only reports it if the relation is gated. Returns the number of operations applied.
    fn resolve_key(&self, rel: &Relation, replicas: &[Replica], key: &Key, records: &RecordMap, unresolved: &BTreeSet<String>, names: &BTreeMap<&str, &str>, full: bool, gated: bool) -> Result<usize> {
        let text = key.to_string();
        let canonical = key.canonical();
        let insp = self.inspect(replicas, key, records, unresolved, full)?;
        if insp.all_missing {
            if !gated {
                self.store.forget(&rel.name, key)?;
            }
            self.clear_outcome(&rel.name, &canonical);
            return Ok(0);
        }
        if insp.settled {
            self.clear_outcome(&rel.name, &canonical);
            return Ok(0);
        }
        let assignment = decide(&rel.policy, &insp.situation);
        let ops = assignment.ops(&insp.situation);
        let held_text = |a: &Assignment| a.held.iter().map(|(id, reason)| format!("{}: {reason}", names.get(id.as_str()).unwrap_or(&id.as_str()))).collect::<Vec<_>>();
        if gated {
            // The held plan is reported again only if the plan or the observed content changes.
            let plan = describe_plan(names, &ops);
            let reasons = held_text(&assignment);
            let content: Vec<String> = insp.situation.slots.iter().map(|s| match &s.observation {
                Observation::Present(sn) => crate::observe::short_hash(&sn.hash),
                other => format!("{other:?}"),
            }).collect();
            if self.changed(&rel.name, &canonical, &format!("held {plan}\n{}\n{}", reasons.join("\n"), content.join(" "))) {
                if !ops.is_empty() {
                    self.emit(Event::Held { relation: rel.name.clone(), key: text.clone(), detail: plan.clone() });
                    self.fire(rel, Trigger::Held, &[("key", text.as_str()), ("plan", plan.as_str())]);
                }
                for reason in reasons {
                    self.emit(Event::Skipped { relation: rel.name.clone(), key: text.clone(), reason });
                }
            }
            return Ok(0);
        }
        let n = ops.len();
        let plan = describe_plan(names, &ops);
        let executor = Executor { store: &self.store, blobs: &self.blobs, medium: self.medium.as_ref() };
        let ctx = apply::Context { relation: &rel.name, key, replicas, situation: &insp.situation, sync_times: rel.sync_times, transforms: &rel.transforms, workdir: &self.workdir };
        let applied = executor.apply(&ctx, &assignment)?;
        for w in applied.warnings {
            self.emit(Event::Warned { relation: rel.name.clone(), key: text.clone(), detail: w });
        }
        let paths: Vec<String> = applied.paths.iter().map(|p| p.display().to_string()).collect();
        if n > 0 {
            self.clear_outcome(&rel.name, &canonical);
            self.emit(Event::Resolved { relation: rel.name.clone(), key: text.clone(), ops: n, detail: plan.clone(), paths: paths.clone() });
            for (op, path) in applied.ops.iter().zip(&paths) {
                let (replica, word) = match op {
                    Op::Write { to, .. } => (to.as_str(), "write"),
                    Op::Delete { at } => (at.as_str(), "delete"),
                };
                let site = replicas.iter().find(|r| r.id() == replica).map(|r| r.site.as_str()).unwrap_or("");
                self.fire(rel, Trigger::Resolved, &[("key", text.as_str()), ("path", path.as_str()), ("replica", replica), ("site", site), ("op", word), ("plan", plan.as_str())]);
            }
        } else if assignment.held.is_empty() {
            self.clear_outcome(&rel.name, &canonical);
            self.emit(Event::Resolved { relation: rel.name.clone(), key: text.clone(), ops: 0, detail: "records aligned, no files changed".into(), paths: Vec::new() });
        }
        let reasons = held_text(&assignment);
        if !reasons.is_empty() && self.changed(&rel.name, &canonical, &format!("skipped\n{}", reasons.join("\n"))) {
            for reason in &reasons {
                self.emit(Event::Skipped { relation: rel.name.clone(), key: text.clone(), reason: reason.clone() });
            }
            self.fire(rel, Trigger::Conflict, &[("key", text.as_str()), ("reason", reasons.join("; ").as_str())]);
        }
        Ok(n)
    }

    /// Builds a report of each relation by inspecting its keys the same way a cycle does, without writing anything.
    pub fn report(&self) -> Report {
        let (expanded, failed) = self.expand_all(None);
        let conflicts = footprint_conflicts(&self.relations, &expanded, &self.reserved);
        let mut relations = Vec::new();
        for (i, rel) in self.relations.iter().enumerate() {
            let failed_with = |error: String| RelationReport {
                name: rel.name.clone(), sites: Vec::new(), replicas: Vec::new(), rows: Vec::new(),
                gated: self.gates.relation_flag(&rel.name), unavailable: None, warnings: Vec::new(), error: Some(error),
            };
            let overlaps: Vec<&str> = conflicts.iter().filter(|c| c.relations.contains(&i)).map(|c| c.message.as_str()).collect();
            if !overlaps.is_empty() {
                relations.push(failed_with(overlaps.join("; ")));
                continue;
            }
            let Some((_, replicas)) = expanded.iter().find(|(j, _)| *j == i) else {
                let error = failed.iter().find(|(j, _)| *j == i).map(|(_, e)| e.clone()).unwrap_or_else(|| "could not expand sites".into());
                relations.push(failed_with(error));
                continue;
            };
            match self.report_relation(rel, replicas) {
                Ok(r) => relations.push(r),
                Err(e) => relations.push(failed_with(format!("{e:#}"))),
            }
        }
        Report { relations, globally_gated: self.gates.globally_gated() }
    }

    /// Builds the report for a relation: its sites, its replicas, and a row for each key with the plan for the next cycle.
    fn report_relation(&self, rel: &Relation, replicas: &[Replica]) -> Result<RelationReport> {
        let sites = rel.sites.iter().map(|s| {
            let mut notes = Vec::new();
            if s.locations.len() > 1 {
                notes.push(format!("{} locations", s.locations.len()));
            }
            let fan: Vec<String> = s.var_names().intersection(&rel.fanout).cloned().collect();
            if !fan.is_empty() {
                notes.push(format!("one replica per {}", fan.join(", ")));
            }
            SiteView { name: s.name.clone(), notes }
        }).collect();
        let names = display_names(replicas);
        let views: Vec<ReplicaView> = replicas.iter().zip(&names).map(|(r, name)| ReplicaView {
            site: r.site.clone(), id: r.id().to_owned(), name: name.clone(), root: r.root().to_path_buf(),
            root_exists: self.medium.is_dir(r.root()), pattern: r.pattern.text().to_owned(),
        }).collect();
        let records = self.store.records_for(&rel.name)?;
        let gated = self.gates.relation_flag(&rel.name);
        if let Some(reason) = self.unavailable_root(replicas, &records) {
            return Ok(RelationReport { name: rel.name.clone(), sites, replicas: views, rows: Vec::new(), gated, unavailable: Some(reason), warnings: Vec::new(), error: None });
        }
        let found = self.discover(rel, replicas, &records)?;
        let unresolved = self.store.unresolved_for(&rel.name)?;
        // Each key is decided in full under the same conditions as in a cycle: the policy changed, or the database has not seen one of the replicas.
        let mut full = self.stale_policy.lock().unwrap().contains(&rel.name);
        for r in replicas {
            full |= !self.store.replica_known(&rel.name, r.id())?;
        }
        let name_of: BTreeMap<&str, &str> = replicas.iter().zip(&names).map(|(r, n)| (r.id(), n.as_str())).collect();
        let mut rows = Vec::new();
        for key in &found.keys {
            let insp = self.inspect(replicas, key, &records, &unresolved, full)?;
            let cells = insp.situation.slots.iter().map(|s| CellView { observation: s.observation.clone(), transition: s.transition() }).collect();
            let (plan, note) = if insp.settled || insp.all_missing {
                (Vec::new(), None)
            } else {
                let assignment = decide(&rel.policy, &insp.situation);
                let ops = assignment.ops(&insp.situation);
                let note = (!assignment.held.is_empty()).then(|| {
                    assignment.held.iter().map(|(id, reason)| format!("{}: {reason}", name_of.get(id.as_str()).unwrap_or(&id.as_str()))).collect::<Vec<_>>().join("; ")
                });
                (ops, note)
            };
            let plan_text = describe_plan(&name_of, &plan);
            rows.push(RowView { key: key.to_string(), cells, plan, plan_text, note });
        }
        Ok(RelationReport { name: rel.name.clone(), sites, replicas: views, rows, gated, unavailable: None, warnings: found.failures, error: None })
    }
}

/// Expands a relation's sites into replicas through a medium.
pub fn expand(rel: &Relation, medium: &dyn Medium) -> Result<Vec<Replica>> {
    let mut out = Vec::new();
    for site in &rel.sites {
        out.extend(site.expand(&rel.identity, medium)?);
    }
    Ok(out)
}

/// Observes the file at a path. If its size and mtime match the record to the millisecond, the recorded hash is reused instead of reading the file.
pub fn observe(medium: &dyn Medium, path: &Path, record: Option<&Snapshot>) -> Observation {
    let meta = match medium.stat(path) {
        Ok(Some(m)) => m,
        Ok(None) => return Observation::Missing,
        Err(e) => return Observation::Unreadable(e.to_string()),
    };
    if meta.kind != EntryKind::File {
        return Observation::Unreadable(format!("{} is not a regular file", path.display()));
    }
    if let Some(r) = record {
        if to_epoch_ms(r.mtime) == to_epoch_ms(meta.mtime) && r.size == meta.size {
            return Observation::Present(Snapshot { mtime: meta.mtime, size: meta.size, hash: r.hash });
        }
    }
    let hashed = medium.open(path).and_then(|mut f| {
        let mut hasher = blake3::Hasher::new();
        std::io::copy(&mut f, &mut hasher)?;
        Ok(*hasher.finalize().as_bytes())
    });
    match hashed {
        Ok(hash) => Observation::Present(Snapshot { mtime: meta.mtime, size: meta.size, hash }),
        Err(e) => Observation::Unreadable(e.to_string()),
    }
}

/// Classifies a slot's transition, for the dashboard.
pub fn transition_of(basis: &Basis, observation: &Observation) -> Transition {
    classify(basis, observation)
}

/// An overlap between two replica footprints, or between a footprint and a reserved path.
pub struct Conflict {
    /// The relations involved, as indexes into the engine's relation list.
    pub relations: Vec<usize>,
    pub message: String,
    /// True if the overlap is with a reserved path instead of another replica.
    pub reserved: bool,
}

/// Finds each pair of replica footprints where one lies inside the other, and each footprint that overlaps a reserved path.
pub fn footprint_conflicts(relations: &[Relation], expanded: &[(usize, Vec<Replica>)], reserved: &[PathBuf]) -> Vec<Conflict> {
    let mut items: Vec<(PathBuf, usize, &Replica)> = expanded.iter().flat_map(|(i, rs)| rs.iter().map(move |r| (r.pattern.footprint(), *i, r))).collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (i, (fa, ra, a)) in items.iter().enumerate() {
        // The list is sorted, so footprints under fa come right after it and the scan can stop at the first one outside it.
        for (fb, rb, b) in items[i + 1..].iter().take_while(|(fb, _, _)| fb.starts_with(fa)) {
            out.push(Conflict {
                relations: if ra == rb { vec![*ra] } else { vec![(*ra).min(*rb), (*ra).max(*rb)] },
                message: format!("overlapping roots: {} ({}/{}) and {} ({}/{})", fa.display(), relations[*ra].name, a.id(), fb.display(), relations[*rb].name, b.id()),
                reserved: false,
            });
        }
        for r in reserved {
            if r.starts_with(fa) || fa.starts_with(r) {
                out.push(Conflict {
                    relations: vec![*ra],
                    message: format!("{} ({}/{}) overlaps {}, which replica itself writes", fa.display(), relations[*ra].name, a.id(), r.display()),
                    reserved: true,
                });
            }
        }
    }
    out
}

/// A problem found by `check`, with a flag for whether it involves a reserved path.
pub struct Problem {
    pub message: String,
    pub reserved: bool,
}

/// Expands each relation and lists the expansion failures and overlapping footprints. The check subcommand and the daemon's startup both use it.
pub fn check(relations: &[Relation], reserved: &[PathBuf], medium: &dyn Medium) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut expanded = Vec::new();
    for (i, rel) in relations.iter().enumerate() {
        match expand(rel, medium) {
            Ok(rs) => expanded.push((i, rs)),
            Err(e) => problems.push(Problem { message: format!("{}: {e:#}", rel.name), reserved: false }),
        }
    }
    problems.extend(footprint_conflicts(relations, &expanded, reserved).into_iter().map(|c| Problem { message: c.message, reserved: c.reserved }));
    problems
}

/// The state of each relation, with the global gate.
pub struct Report {
    pub relations: Vec<RelationReport>,
    pub globally_gated: bool,
}

/// The state of a relation. The variants run from worst to best, so the minimum across relations gives the overall state.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RelationState {
    Error,
    /// A root with recorded files is missing, so the relation is skipped until it comes back.
    Unavailable,
    /// Some keys await a sync.
    Awaiting(usize),
    /// Some keys have plans, but a gate holds them back.
    Held(usize),
    /// Part of a tree could not be read.
    Degraded,
    /// A configured folder does not exist yet.
    FolderMissing,
    InSync,
}

impl RelationState {
    /// The state's name in `status.json`.
    pub fn word(&self) -> &'static str {
        match self {
            RelationState::Error => "error",
            RelationState::Unavailable => "unavailable",
            RelationState::Awaiting(_) => "awaiting",
            RelationState::Held(_) => "held",
            RelationState::Degraded => "degraded",
            RelationState::FolderMissing => "folder missing",
            RelationState::InSync => "in sync",
        }
    }
}

/// The report for a relation, as the dashboard shows it.
pub struct RelationReport {
    pub name: String,
    pub sites: Vec<SiteView>,
    pub replicas: Vec<ReplicaView>,
    pub rows: Vec<RowView>,
    pub gated: bool,
    pub unavailable: Option<String>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

impl RelationReport {
    /// Counts the rows awaiting a sync and the rows a gate holds back, in that order. Only rows with a plan can be held. A row with a hold note and no plan counts as awaiting whether the gate is open or closed.
    pub fn counts(&self, globally_gated: bool) -> (usize, usize) {
        let gated = globally_gated || self.gated;
        let planned = self.rows.iter().filter(|r| !r.plan.is_empty()).count();
        let noted = self.rows.iter().filter(|r| r.plan.is_empty() && r.note.is_some()).count();
        if gated { (noted, planned) } else { (planned + noted, 0) }
    }

    /// The relation's overall state, taken from the worst condition that applies.
    pub fn state(&self, globally_gated: bool) -> RelationState {
        if self.error.is_some() {
            return RelationState::Error;
        }
        if self.unavailable.is_some() {
            return RelationState::Unavailable;
        }
        let (awaiting, held) = self.counts(globally_gated);
        if awaiting > 0 {
            return RelationState::Awaiting(awaiting);
        }
        if held > 0 {
            return RelationState::Held(held);
        }
        if !self.warnings.is_empty() {
            return RelationState::Degraded;
        }
        if self.replicas.iter().any(|r| !r.root_exists) {
            return RelationState::FolderMissing;
        }
        RelationState::InSync
    }
}

/// A site as the dashboard lists it.
pub struct SiteView {
    pub name: String,
    pub notes: Vec<String>,
}

/// A replica, shown as a column on the dashboard.
pub struct ReplicaView {
    pub site: String,
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub root_exists: bool,
    pub pattern: String,
}

/// A key's row on the dashboard: a cell for each replica, the plan for the next cycle, and a note explaining any hold.
pub struct RowView {
    pub key: String,
    pub cells: Vec<CellView>,
    pub plan: Vec<Op>,
    pub plan_text: String,
    pub note: Option<String>,
}

impl RowView {
    /// True if the row has a plan or a hold note.
    pub fn needs_attention(&self) -> bool {
        !self.plan.is_empty() || self.note.is_some()
    }
}

/// A cell holds the observation at a replica and the transition from its basis.
pub struct CellView {
    pub observation: Observation,
    pub transition: Transition,
}

/// The kinds of change the flow carries into a site, for the explain command.
pub fn incoming_kinds(policy: &Policy, site: &str) -> Vec<Kind> {
    Kind::ALL.into_iter().filter(|k| policy.flow.has_incoming(*k, site)).collect()
}

/// Fails if the configuration defines no relations.
pub fn ensure_relations(relations: &[Relation]) -> Result<()> {
    if relations.is_empty() {
        bail!("the configuration defines no relations");
    }
    Ok(())
}
