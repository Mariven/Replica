//! Run cycles: expand sites into replicas, refuse overlaps, discover keys, observe, decide, apply or hold, report.
//!
//! A cycle visits each relation in turn. For each key it reads one observation per replica, skips the key when nothing moved since the last agreement, otherwise decides and applies (or reports the plan under a closed gate). Conditions persisting across cycles are reported when they appear or change, not on each pass.

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
use crate::store::{Intent, RecordMap, Store, to_epoch_ms};

/// Set by a signal handler or a dashboard command. The engine checks it between keys and returns from the cycle early.
pub static STOP: AtomicBool = AtomicBool::new(false);

// Outcome keys for relation-level conditions. A leading NUL keeps them apart from canonical key texts.
const KEY_EXPAND: &str = "\0expand";
const KEY_OVERLAP: &str = "\0overlap";
const KEY_ROOT: &str = "\0root";
const KEY_WALK: &str = "\0walk";
const KEY_ERROR: &str = "\0error";

/// A relation as the engine runs it.
pub struct Relation {
    pub name: String,
    pub sites: Vec<Site>,
    pub identity: BTreeSet<String>,
    pub fanout: BTreeSet<String>,
    pub policy: Policy,
    pub ignore: Vec<GlobMatcher>,
    pub ignore_text: Vec<String>,
    pub sync_times: bool,
    /// Start with this relation's gate closed.
    pub gate_default: bool,
    /// Register this relation's roots with the watcher.
    pub watch: bool,
    pub actions: Actions,
    /// Commands rewriting content on an edge, keyed by (from site, to site).
    pub transforms: BTreeMap<(String, String), Shell>,
    pub log: Option<PathBuf>,
}

impl Relation {
    /// A digest of everything that changes how keys are decided, so a changed policy forces one full evaluation.
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

/// Write barriers. A closed gate lets discovery and planning continue while no file or record changes.
#[derive(Default)]
pub struct Gates {
    global: AtomicBool,
    relations: Mutex<BTreeMap<String, bool>>,
}

impl Gates {
    pub fn globally_gated(&self) -> bool {
        self.global.load(Ordering::Relaxed)
    }

    /// The relation's own flag, apart from the global gate.
    pub fn relation_flag(&self, relation: &str) -> bool {
        *self.relations.lock().unwrap().get(relation).unwrap_or(&false)
    }

    /// True when the global gate or the relation's gate is closed.
    pub fn is_gated(&self, relation: &str) -> bool {
        self.globally_gated() || self.relation_flag(relation)
    }

    pub fn set_global(&self, gated: bool) {
        self.global.store(gated, Ordering::Relaxed);
    }

    pub fn set_relation(&self, relation: &str, gated: bool) {
        self.relations.lock().unwrap().insert(relation.into(), gated);
    }
}

/// The engine over one store, blob store, medium, and set of relations.
pub struct Engine {
    pub store: Store,
    pub blobs: Blobs,
    pub medium: Box<dyn Medium>,
    pub relations: Vec<Relation>,
    pub hooks: Vec<Hook>,
    pub gates: Arc<Gates>,
    /// Paths the daemon itself writes. A root containing or inside one is refused.
    pub reserved: Vec<PathBuf>,
    /// Working directory for actions and transforms: the state directory.
    pub workdir: PathBuf,
    /// Last reported outcome per (relation, key), hashed, so a persisting condition is reported once.
    outcomes: Mutex<BTreeMap<(String, String), u64>>,
    /// Relations whose stored fingerprint differs from the loaded one; each key is evaluated once, then the fingerprint is stored.
    stale_policy: Mutex<BTreeSet<String>>,
}

/// What a cycle did.
#[derive(Debug, Default, Clone, Copy)]
pub struct CycleSummary {
    pub ops: usize,
    pub errors: usize,
}

/// Slots of one key with the facts the cycle and the report both need.
struct Inspection {
    situation: Situation,
    /// No replica has the file.
    all_missing: bool,
    /// Nothing moved since the last agreement and no conflict is open, so the policy need not run.
    settled: bool,
}

/// Keys found for a relation, the subfolders the walk could not read, the recorded keys the constraints no longer admit.
struct Discovery {
    keys: BTreeSet<Key>,
    failures: Vec<String>,
    dropped: Vec<Key>,
}

impl Engine {
    /// Build an engine. Gate flags come from the relation defaults and the persisted flags; a closed gate on either side closes the gate.
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

    /// Set the global gate and persist it.
    pub fn set_global_gate(&self, gated: bool) {
        self.gates.set_global(gated);
        let _ = self.store.set_flag("gate", gated);
    }

    /// Set a relation's gate and persist it.
    pub fn set_relation_gate(&self, relation: &str, gated: bool) {
        self.gates.set_relation(relation, gated);
        let _ = self.store.set_flag(&format!("gate.{relation}"), gated);
    }

    /// Send an event to each hook.
    pub fn emit(&self, event: Event) {
        for hook in &self.hooks {
            hook(&event);
        }
    }

    /// True when `value` differs from what was last recorded under (relation, key), recording it. A first sighting counts as a change.
    fn changed(&self, relation: &str, key: &str, value: &str) -> bool {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut h);
        let digest = h.finish();
        self.outcomes.lock().unwrap().insert((relation.to_owned(), key.to_owned()), digest) != Some(digest)
    }

    /// Forget the recorded outcome under (relation, key), so the next occurrence is reported again.
    fn clear_outcome(&self, relation: &str, key: &str) {
        self.outcomes.lock().unwrap().remove(&(relation.to_owned(), key.to_owned()));
    }

    /// Report and clear journal rows left by a run that died mid-plan.
    pub fn recover(&self) -> Result<Vec<Intent>> {
        let stale = self.store.stale_intents()?;
        for intent in &stale {
            self.emit(Event::StaleIntent { relation: intent.relation.clone(), key: intent.key.to_string(), detail: intent.detail.clone() });
            self.store.clear_intent(&intent.relation, &intent.key)?;
        }
        self.blobs.clean_staging();
        Ok(stale)
    }

    /// Expand a relation's sites into replicas.
    pub fn expand(&self, rel: &Relation) -> Result<Vec<Replica>> {
        expand(rel, self.medium.as_ref())
    }

    /// Expand each relation, separating failures.
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

    /// Run a cycle over all relations, or over the named ones alone.
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
        // Overlaps are checked against every relation, not only the ones this cycle runs, since a fan-out elsewhere can produce one.
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

    /// Note each replica's pattern in the store; a replica whose pattern changed starts over, unless a gate holds the relation.
    fn note_replicas(&self, rel: &Relation, replicas: &[Replica]) -> Result<()> {
        let gated = self.gates.is_gated(&rel.name);
        for r in replicas {
            if let Some(previous) = self.store.note_replica(&rel.name, r.id(), Path::new(r.pattern.text()))? {
                self.emit(Event::Relocated { relation: rel.name.clone(), replica: r.id().to_owned(), from: previous.clone(), to: r.pattern.text().to_owned() });
                if !gated {
                    self.store.forget_replica(&rel.name, r.id())?;
                } else {
                    // Under a gate the old pattern stays recorded, so the move is reported again once the gate opens.
                    self.store.note_replica(&rel.name, r.id(), Path::new(&previous))?;
                }
            }
        }
        Ok(())
    }

    /// Collect the keys of a relation: files found under each replica plus recorded keys, less the recorded keys the constraints or the ignore list no longer admit.
    fn discover(&self, rel: &Relation, replicas: &[Replica], records: &RecordMap) -> Result<Discovery> {
        let mut keys = BTreeSet::new();
        let mut failures = Vec::new();
        for r in replicas {
            let found = discover_keys(r, &rel.identity, &rel.ignore, self.medium.as_ref())?;
            failures.extend(found.failures);
            keys.extend(found.keys);
        }
        // A key is admitted only when each replica can locate it within its constraints and no replica's tail is ignored.
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

    /// The first replica whose root is missing although it has recorded snapshots, meaning the volume may be unmounted.
    fn unavailable_root(&self, replicas: &[Replica], records: &RecordMap) -> Option<String> {
        replicas.iter().find(|r| {
            !self.medium.is_dir(r.root()) && records.values().any(|by| matches!(by.get(r.id()), Some(Basis::Snapshot(_))))
        }).map(|r| format!("{} has records but the folder is missing; treating it as unavailable, not deleted", r.root().display()))
    }

    /// Read the observation of each replica for a key against the records.
    fn inspect(&self, replicas: &[Replica], key: &Key, records: &RecordMap, unresolved: &BTreeSet<String>, full: bool) -> Result<Inspection> {
        let text = key.canonical();
        let by_replica = records.get(&text);
        let mut slots = Vec::with_capacity(replicas.len());
        let mut all_recorded = true;
        for r in replicas {
            let basis = by_replica.and_then(|m| m.get(r.id())).cloned().unwrap_or(Basis::None);
            // A replica with no record and no file is at rest; one with no record and a file is a newcomer the policy must see.
            let path = r.locate(key)?;
            let observation = observe(self.medium.as_ref(), &path, basis.snapshot());
            all_recorded &= basis.is_recorded() || observation == Observation::Missing;
            slots.push(Slot { id: r.id().to_owned(), site: r.site.clone(), path, basis, observation });
        }
        let all_missing = slots.iter().all(|s| s.observation == Observation::Missing);
        let settled = !full && all_recorded && slots.iter().all(|s| s.transition() == Transition::Unchanged) && !unresolved.contains(&text);
        Ok(Inspection { situation: Situation { slots }, all_missing, settled })
    }

    /// Run one relation: note replicas, guard the roots, discover, then resolve each key.
    fn run_relation(&self, rel: &Relation, replicas: &[Replica]) -> Result<CycleSummary> {
        let mut summary = CycleSummary::default();
        self.note_replicas(rel, replicas)?;
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
        let full = self.stale_policy.lock().unwrap().contains(&rel.name);
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
        if full && !gated {
            self.store.set_fingerprint(&rel.name, &rel.fingerprint())?;
            self.stale_policy.lock().unwrap().remove(&rel.name);
        }
        if changed_any {
            self.fire(rel, Trigger::Changed, &[("ops", &summary.ops.to_string())]);
        }
        Ok(summary)
    }

    /// Run an action of the relation for a trigger, with the relation name among the variables.
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

    /// Decide one key and apply or hold the plan. Returns the number of operations applied.
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
            // Report the held plan once per change of plan or of the content it would copy.
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

    /// Compile the report for each relation, exactly as a cycle would see it, without changing anything.
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

    /// The report of one relation.
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
        let full = self.stale_policy.lock().unwrap().contains(&rel.name);
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

/// Expand a relation's sites into replicas through a medium.
pub fn expand(rel: &Relation, medium: &dyn Medium) -> Result<Vec<Replica>> {
    let mut out = Vec::new();
    for site in &rel.sites {
        out.extend(site.expand(&rel.identity, medium)?);
    }
    Ok(out)
}

/// Read the observation of a path, reusing the recorded hash when size and modification time (to the millisecond) match the record.
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

/// The transition of a slot, exposed for the dashboard.
pub fn transition_of(basis: &Basis, observation: &Observation) -> Transition {
    classify(basis, observation)
}

/// Two footprints that overlap, or a footprint that contains or lies inside a reserved path.
pub struct Conflict {
    /// Indices into the relation list of the relations involved.
    pub relations: Vec<usize>,
    pub message: String,
    /// True when a reserved path is involved, which is a configuration error rather than two roots meeting.
    pub reserved: bool,
}

/// Find each pair of footprints where one lies inside the other, across all replicas, plus footprints meeting a reserved path.
pub fn footprint_conflicts(relations: &[Relation], expanded: &[(usize, Vec<Replica>)], reserved: &[PathBuf]) -> Vec<Conflict> {
    let mut items: Vec<(PathBuf, usize, &Replica)> = expanded.iter().flat_map(|(i, rs)| rs.iter().map(move |r| (r.pattern.footprint(), *i, r))).collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (i, (fa, ra, a)) in items.iter().enumerate() {
        // Footprints starting with fa sort directly after it, so the scan stops at the first that does not.
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

/// A problem found by `check`: its message, and whether a reserved path is involved.
pub struct Problem {
    pub message: String,
    pub reserved: bool,
}

/// Expand each relation and report expansion failures and footprint conflicts, for the check subcommand and startup.
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

/// Report across all relations.
pub struct Report {
    pub relations: Vec<RelationReport>,
    pub globally_gated: bool,
}

/// One state per relation, ordered worst first so the overall state is the minimum.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RelationState {
    Error,
    /// A root with records is missing; nothing runs until it returns.
    Unavailable,
    /// Some keys await a sync.
    Awaiting(usize),
    /// Some keys have plans a gate holds back.
    Held(usize),
    /// Part of a tree could not be read.
    Degraded,
    /// A configured folder does not exist yet.
    FolderMissing,
    InSync,
}

impl RelationState {
    /// One word for JSON consumers.
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

/// The report of one relation.
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
    /// Rows awaiting a sync and rows held by a gate. Under a gate only rows with a plan are held; a conflict is awaiting either way.
    pub fn counts(&self, globally_gated: bool) -> (usize, usize) {
        let gated = globally_gated || self.gated;
        let planned = self.rows.iter().filter(|r| !r.plan.is_empty()).count();
        let noted = self.rows.iter().filter(|r| r.plan.is_empty() && r.note.is_some()).count();
        if gated { (noted, planned) } else { (planned + noted, 0) }
    }

    /// The relation's state from its rows and notes.
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

/// A replica column.
pub struct ReplicaView {
    pub site: String,
    pub id: String,
    pub name: String,
    pub root: PathBuf,
    pub root_exists: bool,
    pub pattern: String,
}

/// A key row: one cell per replica, the plan the next cycle applies, the note explaining a hold.
pub struct RowView {
    pub key: String,
    pub cells: Vec<CellView>,
    pub plan: Vec<Op>,
    pub plan_text: String,
    pub note: Option<String>,
}

impl RowView {
    /// A row needs attention when the next cycle would act on it or a hold keeps it waiting.
    pub fn needs_attention(&self) -> bool {
        !self.plan.is_empty() || self.note.is_some()
    }
}

/// A cell: what was observed and how it moved since the basis.
pub struct CellView {
    pub observation: Observation,
    pub transition: Transition,
}

/// Which kinds the flow carries into a site, for the explain command.
pub fn incoming_kinds(policy: &Policy, site: &str) -> Vec<Kind> {
    Kind::ALL.into_iter().filter(|k| policy.flow.has_incoming(*k, site)).collect()
}

/// Reject an empty relation list, which a configuration with no relations produces.
pub fn ensure_relations(relations: &[Relation]) -> Result<()> {
    if relations.is_empty() {
        bail!("the configuration defines no relations");
    }
    Ok(())
}
