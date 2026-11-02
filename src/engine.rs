//! Reconcile storage roots against policy rules and apply filesystem updates.
//!
//! Stage file writes through temporary files in target directories, then rename them into final positions once writing finishes. Record intent rows in the database before modifying files, clearing them when commits succeed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use anyhow::{Context as AnyhowContext, Result, bail};

use crate::archive::Archive;
use crate::identifier::{Identifier, Pattern, identifier_from_string, identifier_to_string};
use crate::policy::{Decision, Instance, Op, Policy, Slot};
use crate::label::{Label, Mapping, MappingKind, Ref, Shape, display_names};
use crate::status::{Change, Snapshot, Status};
use crate::store::{RecordMap, Store, to_epoch_ms};

/// Set by a signal handler or a dashboard command. The engine checks it between identifiers and returns from the cycle early.
pub static STOP: AtomicBool = AtomicBool::new(false);

// Outcome keys for relation-level conditions. A leading NUL keeps them apart from identifier strings, which are paths and cannot contain one.
const KEY_EXPAND: &str = "\0expand";
const KEY_OVERLAP: &str = "\0overlap";
const KEY_ROOT: &str = "\0root";
const KEY_WALK: &str = "\0walk";
const KEY_ERROR: &str = "\0error";

/// Target configuration pairing storage roots with path patterns and conflict policy.
pub struct Relation {
    pub name: String,
    pub labels: Vec<Label>,
    pub pattern: Pattern,
    pub policy: Policy,
    pub sync_times: bool,
    /// Default gate status loaded during startup and managed by [`Gates`].
    pub gate_default: bool,
}

/// Write barriers blocking mutations while allowing path discovery and planning to continue.
///
/// When a gate is active, mutations and database updates pause until an operator clears the flag. Load initial states from relation defaults at startup, then toggle flags globally or per relation through runtime commands.
#[derive(Default)]
pub struct Gates {
    pub global: AtomicBool,
    pub relations: Mutex<BTreeMap<String, bool>>,
}

impl Gates {
    /// Check whether the global write gate blocks mutations.
    pub fn globally_gated(&self) -> bool {
        self.global.load(Ordering::Relaxed)
    }

    /// Read the gate flag for a relation directly, bypassing the global gate setting.
    pub fn relation_flag(&self, relation: &str) -> bool {
        *self.relations.lock().unwrap().get(relation).unwrap_or(&false)
    }

    /// Check whether either local or global gates block writes for a relation.
    pub fn is_gated(&self, relation: &str) -> bool {
        self.globally_gated() || self.relation_flag(relation)
    }

    /// Set the global write gate flag across all relations.
    pub fn set_global(&self, gated: bool) {
        self.global.store(gated, Ordering::Relaxed);
    }

    /// Set the write gate flag for an individual relation.
    pub fn set_relation(&self, relation: &str, gated: bool) {
        self.relations.lock().unwrap().insert(relation.into(), gated);
    }
}

/// Progress notification emitted to registered hooks during synchronization.
#[derive(Debug)]
pub enum Event {
    /// A plan was applied. `detail` names the operations; `ops` counts them.
    Resolved { relation: String, identifier: String, ops: usize, detail: String },
    Held { relation: String, identifier: String, detail: String },
    Skipped { relation: String, identifier: String, reason: String },
    Pending { relation: String, identifier: String, reason: String },
    /// Something was skipped or degraded without failing the relation, such as an unreadable subfolder or a permission bit that could not be copied.
    Warned { relation: String, identifier: String, detail: String },
    Errored { relation: String, identifier: String, error: String },
    StaleIntent { relation: String, identifier: String, detail: String },
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
    /// The relation this event belongs to.
    pub fn relation(&self) -> &str {
        match self {
            Event::Resolved { relation, .. }
            | Event::Held { relation, .. }
            | Event::Skipped { relation, .. }
            | Event::Pending { relation, .. }
            | Event::Warned { relation, .. }
            | Event::Errored { relation, .. }
            | Event::StaleIntent { relation, .. } => relation,
        }
    }

    /// The identifier this event concerns, empty for relation-level events.
    pub fn identifier(&self) -> &str {
        match self {
            Event::Resolved { identifier, .. }
            | Event::Held { identifier, .. }
            | Event::Skipped { identifier, .. }
            | Event::Pending { identifier, .. }
            | Event::Warned { identifier, .. }
            | Event::Errored { identifier, .. }
            | Event::StaleIntent { identifier, .. } => identifier,
        }
    }

    /// One word naming the kind of event.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Resolved { .. } => "resolved",
            Event::Held { .. } => "held",
            Event::Skipped { .. } => "skipped",
            Event::Pending { .. } => "pending",
            Event::Warned { .. } => "warning",
            Event::Errored { .. } => "error",
            Event::StaleIntent { .. } => "stale intent",
        }
    }

    /// The display tone of the event.
    pub fn tone(&self) -> Tone {
        match self {
            Event::Resolved { .. } => Tone::Ok,
            Event::Held { .. } => Tone::Neutral,
            Event::Skipped { .. } | Event::Pending { .. } | Event::Warned { .. } => Tone::Warn,
            Event::Errored { .. } | Event::StaleIntent { .. } => Tone::Bad,
        }
    }

    /// The free text of the event, without relation or identifier.
    pub fn text(&self) -> String {
        match self {
            Event::Resolved { detail, .. } => format!("resolved: {detail}"),
            Event::Held { detail, .. } => format!("held by gate: would {detail}"),
            Event::Skipped { reason, .. } => format!("skipped: {reason}"),
            Event::Pending { reason, .. } => format!("pending: {reason}"),
            Event::Warned { detail, .. } => format!("warning: {detail}"),
            Event::Errored { error, .. } => format!("error: {error}"),
            Event::StaleIntent { detail, .. } => format!("stale intent (previous run died mid-operation): {detail}"),
        }
    }
}

/// Callback invoked on emitted lifecycle events.
pub type Hook = Box<dyn Fn(&Event) + Send>;

/// Engine executing synchronization cycles across configured relations.
pub struct Engine {
    pub store: Store,
    pub relations: Vec<Relation>,
    pub hooks: Vec<Hook>,
    pub gates: std::sync::Arc<Gates>,
    pub archive: Option<Archive>,
    /// Paths the daemon itself writes (state database, archive directory, relation logs). A root that contains or lies inside one is refused.
    pub reserved: Vec<PathBuf>,
    /// Last emitted outcome per (relation, key), hashed, so repeating conditions are reported once instead of every cycle.
    outcomes: Mutex<BTreeMap<(String, String), u64>>,
}

/// Slots of one identifier read against the records, with the derived facts both the engine and the dashboard need.
struct Inspection {
    slots: Vec<Slot>,
    /// No ref has the file on disk.
    all_missing: bool,
    /// Every ref has a record row (present or tombstone), nothing changed since, and no conflict is open, so policy need not run.
    settled: bool,
}

impl Engine {
    /// Create a synchronization engine and set initial gate flags from relation defaults.
    pub fn new(store: Store, relations: Vec<Relation>, hooks: Vec<Hook>) -> Engine {
        let gates = Gates::default();
        // Set initial gate flags using configured relation defaults.
        for rel in &relations {
            if rel.gate_default {
                gates.set_relation(&rel.name, true);
            }
        }
        Engine { store, relations, hooks, gates: std::sync::Arc::new(gates), archive: None, reserved: Vec::new(), outcomes: Mutex::default() }
    }

    /// Send an event to each registered hook in sequence.
    fn emit(&self, event: Event) {
        for hook in &self.hooks {
            hook(&event);
        }
    }

    /// Return true when `value` differs from what was last recorded under (relation, key), recording it. The first sighting counts as a change.
    fn changed(&self, relation: &str, key: &str, value: &str) -> bool {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut h);
        let digest = h.finish();
        let mut map = self.outcomes.lock().unwrap();
        map.insert((relation.to_owned(), key.to_owned()), digest) != Some(digest)
    }

    /// Forget the recorded outcome under (relation, key), so the next occurrence is reported again.
    fn clear_outcome(&self, relation: &str, key: &str) {
        self.outcomes.lock().unwrap().remove(&(relation.to_owned(), key.to_owned()));
    }

    /// Clear interrupted writes: notify hooks of stale intent records, remove those rows, and return them for display.
    pub fn recover(&self) -> Result<Vec<(String, Identifier, String)>> {
        let stale = self.store.stale_intents()?;
        for (relation, ident, detail) in &stale {
            self.emit(Event::StaleIntent { relation: relation.clone(), identifier: identifier_to_string(ident), detail: detail.clone() });
            self.store.clear_intent(relation, ident)?;
        }
        Ok(stale)
    }

    /// Expand every relation's labels into refs. Returns the expansions and, for relations that failed, the error text.
    fn expand_all(&self) -> (Vec<(usize, Vec<Ref>)>, Vec<(usize, String)>) {
        let mut expanded = Vec::new();
        let mut failed = Vec::new();
        for (i, rel) in self.relations.iter().enumerate() {
            match expand(rel) {
                Ok(refs) => expanded.push((i, refs)),
                Err(e) => failed.push((i, format!("{e:#}"))),
            }
        }
        (expanded, failed)
    }

    /// Run a synchronization cycle across all relations.
    /// Relations whose roots overlap another root or a reserved path are reported as errored and skipped; the rest still sync.
    pub fn cycle(&self) -> Result<()> {
        let (expanded, failed) = self.expand_all();
        for (i, error) in &failed {
            let rel = &self.relations[*i].name;
            if self.changed(rel, KEY_EXPAND, error) {
                self.emit(Event::Errored { relation: rel.clone(), identifier: String::new(), error: error.clone() });
            }
        }
        for (i, _) in &expanded {
            self.clear_outcome(&self.relations[*i].name, KEY_EXPAND);
        }
        let conflicts = footprint_conflicts(&self.relations, &expanded, &self.reserved);
        let mut skip: BTreeSet<usize> = BTreeSet::new();
        for c in &conflicts {
            skip.extend(c.relations.iter().copied());
        }
        // Report each relation's conflicts once per change of that relation's own set.
        for i in &skip {
            let rel = &self.relations[*i].name;
            let own: Vec<&str> = conflicts.iter().filter(|c| c.relations.contains(i)).map(|c| c.message.as_str()).collect();
            if self.changed(rel, KEY_OVERLAP, &own.join("\n")) {
                for message in own {
                    self.emit(Event::Errored { relation: rel.clone(), identifier: String::new(), error: message.to_owned() });
                }
            }
        }
        for (i, _) in &expanded {
            if !skip.contains(i) {
                self.clear_outcome(&self.relations[*i].name, KEY_OVERLAP);
            }
        }

        // Reconcile files for each relation in sequence.
        for (i, refs) in &expanded {
            if STOP.load(Ordering::Relaxed) {
                return Ok(());
            }
            if skip.contains(i) {
                continue;
            }
            let rel = &self.relations[*i];
            match self.resolve_relation(rel, refs) {
                Ok(()) => self.clear_outcome(&rel.name, KEY_ERROR),
                Err(e) => {
                    let error = format!("{e:#}");
                    if self.changed(&rel.name, KEY_ERROR, &error) {
                        self.emit(Event::Errored { relation: rel.name.clone(), identifier: String::new(), error });
                    }
                }
            }
        }
        Ok(())
    }

    /// Collect identifiers across filesystem references and merge recorded identifiers that still match the pattern, to detect deleted files.
    /// Walks stop at the depth the pattern or template allows. Fixed references map directly to static paths without scanning.
    /// A relation with a fixed mapping must resolve to exactly one identifier, since the fixed path cannot name more.
    fn identifiers(&self, rel: &Relation, refs: &[Ref], records: &RecordMap) -> Result<Discovery> {
        let mut idents: BTreeSet<Identifier> = BTreeSet::new();
        let mut failures = Vec::new();
        let pattern_depth = rel.pattern.max_depth();
        for rf in refs {
            // Skip fixed mappings targeting static paths.
            if matches!(rf.mapping_kind, MappingKind::Fixed(_)) {
                continue;
            }
            // Walk directory trees and record identifiers matching the relation pattern.
            let walk = walk_identifiers(&rf.root, rf.walk_depth(pattern_depth))?;
            failures.extend(walk.failures);
            for found in walk.idents {
                let path = found.iter().fold(rf.root.clone(), |p, c| p.join(c));
                if let Some(ident) = rf.identify(&path) {
                    if rel.pattern.matches(&ident) {
                        idents.insert(ident);
                    }
                }
            }
        }
        // Merge recorded identifiers to detect deleted files. Those a narrowed pattern no longer covers are returned separately,
        // so the cycle can forget them: a record outside the pattern is not a baseline the relation may act on later.
        let mut dropped = Vec::new();
        for ident in records.keys() {
            let ident = identifier_from_string(ident);
            if rel.pattern.matches(&ident) {
                idents.insert(ident);
            } else {
                dropped.push(ident);
            }
        }
        // Reject executions with multiple identifiers whenever a reference uses fixed mapping.
        let has_fixed = refs.iter().any(|s| matches!(s.mapping_kind, MappingKind::Fixed(_)));
        if has_fixed && idents.len() > 1 {
            bail!("relation {:?} has a fixed mapping but {} identifiers; fixed requires exactly one", rel.name, idents.len());
        }
        Ok(Discovery { idents, failures, dropped })
    }

    /// Name the first ref whose root is missing although it has records, meaning the drive may simply be unmounted.
    fn unavailable_root(&self, refs: &[Ref], records: &RecordMap) -> Option<String> {
        for rf in refs {
            if !rf.root.is_dir() {
                let has_records = records.values().any(|by_ref| matches!(by_ref.get(rf.id()), Some(Some(_))));
                if has_records {
                    return Some(format!("{} has records but the folder is missing; treating it as unavailable, not deleted", rf.root.display()));
                }
            }
        }
        None
    }

    /// Read disk status and records into slots for one identifier.
    fn inspect(&self, refs: &[Ref], ident: &Identifier, records: &RecordMap, unresolved: &BTreeSet<String>) -> Inspection {
        let ident_str = identifier_to_string(ident);
        let by_ref = records.get(&ident_str);
        let mut slots = Vec::with_capacity(refs.len());
        let mut all_recorded = true;
        for rf in refs {
            let row = by_ref.and_then(|m| m.get(rf.id()));
            all_recorded &= row.is_some();
            let record = row.cloned().flatten();
            let path = rf.locate(ident);
            let status = read_status(&path, record.as_ref());
            slots.push(Slot { ref_id: rf.id().to_owned(), label: rf.label.clone(), path, record, status });
        }
        let all_missing = slots.iter().all(|s| s.status == Status::Missing);
        let settled = all_recorded && slots.iter().all(|s| s.change() == Change::Unchanged) && !unresolved.contains(&ident_str);
        Inspection { slots, all_missing, settled }
    }

    /// Verify storage root directories exist for recorded references, then resolve each identifier found across the relation.
    fn resolve_relation(&self, rel: &Relation, refs: &[Ref]) -> Result<()> {
        let records = self.store.records_for(&rel.name)?;
        // Halt synchronization if a reference with stored records lacks its root folder on disk, because the underlying drive may simply be unmounted.
        if let Some(reason) = self.unavailable_root(refs, &records) {
            if self.changed(&rel.name, KEY_ROOT, &reason) {
                self.emit(Event::Pending { relation: rel.name.clone(), identifier: String::new(), reason });
            }
            return Ok(());
        }
        self.clear_outcome(&rel.name, KEY_ROOT);

        // Gather identifiers present on the filesystem alongside any identifiers already stored in the database.
        let found = self.identifiers(rel, refs, &records)?;
        if self.changed(&rel.name, KEY_WALK, &found.failures.join("\n")) {
            for f in &found.failures {
                self.emit(Event::Warned { relation: rel.name.clone(), identifier: String::new(), detail: f.clone() });
            }
        }
        // Records the pattern no longer covers leave the relation, so a later widening starts those files from scratch instead of from a stale baseline.
        if !self.gates.is_gated(&rel.name) {
            for ident in &found.dropped {
                self.store.forget(&rel.name, ident).with_context(|| format!("forgetting {} after it left the pattern", identifier_to_string(ident)))?;
            }
        }

        let unresolved = self.store.unresolved_for(&rel.name)?;
        for ident in found.idents {
            if STOP.load(Ordering::Relaxed) {
                return Ok(());
            }
            if let Err(e) = self.resolve(rel, refs, &ident, &records, &unresolved) {
                let ident_str = identifier_to_string(&ident);
                let error = format!("{e:#}");
                // A persistent failure is reported when it appears or changes, like a held plan; the identifier's digest is replaced so a later hold is reported again.
                if self.changed(&rel.name, &ident_str, &format!("error {error}")) {
                    self.emit(Event::Errored { relation: rel.name.clone(), identifier: ident_str, error });
                }
            }
        }
        Ok(())
    }

    /// Compare reference slot statuses against stored records, evaluate policy, and apply planned mutations or hold them under active gates.
    /// Held and skipped outcomes are reported when they first appear or change, not on every cycle.
    fn resolve(&self, rel: &Relation, refs: &[Ref], ident: &Identifier, records: &RecordMap, unresolved: &BTreeSet<String>) -> Result<()> {
        let gated = self.gates.is_gated(&rel.name);
        let ident_str = identifier_to_string(ident);
        let insp = self.inspect(refs, ident, records, unresolved);

        // Purge stored records when all slots report the identifier missing, provided the relation is not gated.
        if insp.all_missing {
            if !gated {
                self.store.forget(&rel.name, ident)?;
            }
            self.clear_outcome(&rel.name, &ident_str);
            return Ok(());
        }

        // Exit early if disk states match stored records and no unresolved conflicts remain.
        if insp.settled {
            self.clear_outcome(&rel.name, &ident_str);
            return Ok(());
        }

        let names = display_names(refs);
        // Skip reasons name the column the way the table does, not by raw ref id.
        let name_of = |id: &str| refs.iter().position(|r| r.id() == id).map(|i| names[i].clone()).unwrap_or_else(|| id.to_owned());
        let skip_text = |skipped: &[(String, String)]| skipped.iter().map(|(rf, reason)| format!("{}: {reason}", name_of(rf))).collect::<Vec<_>>();
        match rel.policy.decide(&Instance { slots: insp.slots.clone() }) {
            Decision::Plan { ops, skipped } if gated => {
                // Report the held plan without altering files or database entries, once per change of plan or of the content it would copy.
                let held = describe_plan(refs, &names, &ops);
                let reasons = skip_text(&skipped);
                let content: Vec<String> = insp.slots.iter().map(|s| match &s.status {
                    Status::Present(sn) => format!("{:02x?}", &sn.hash[..8]),
                    other => format!("{other:?}"),
                }).collect();
                if self.changed(&rel.name, &ident_str, &format!("held {held}\n{}\n{}", reasons.join("\n"), content.join(" "))) {
                    if !ops.is_empty() {
                        self.emit(Event::Held { relation: rel.name.clone(), identifier: ident_str.clone(), detail: held });
                    }
                    for reason in reasons {
                        self.emit(Event::Skipped { relation: rel.name.clone(), identifier: ident_str.clone(), reason });
                    }
                }
            }
            Decision::Plan { ops, skipped } => {
                let n = ops.len();
                let detail = describe_plan(refs, &names, &ops);
                let warnings = self.apply(rel, refs, &insp.slots, ident, ops, &skipped)?;
                for w in warnings {
                    self.emit(Event::Warned { relation: rel.name.clone(), identifier: ident_str.clone(), detail: w });
                }
                if n > 0 {
                    self.clear_outcome(&rel.name, &ident_str);
                    self.emit(Event::Resolved { relation: rel.name.clone(), identifier: ident_str.clone(), ops: n, detail });
                } else if skipped.is_empty() {
                    self.clear_outcome(&rel.name, &ident_str);
                    self.emit(Event::Resolved { relation: rel.name.clone(), identifier: ident_str.clone(), ops: 0, detail: "records aligned, no files changed".into() });
                }
                let reasons = skip_text(&skipped);
                if !reasons.is_empty() && self.changed(&rel.name, &ident_str, &format!("skipped\n{}", reasons.join("\n"))) {
                    for reason in reasons {
                        self.emit(Event::Skipped { relation: rel.name.clone(), identifier: ident_str.clone(), reason });
                    }
                }
            }
            Decision::Skip(reason) => {
                if self.changed(&rel.name, &ident_str, &format!("skip {reason}")) {
                    self.emit(Event::Skipped { relation: rel.name.clone(), identifier: ident_str, reason });
                }
            }
        }
        Ok(())
    }

    /// Apply planned file mutations in three stages: prepare (read donors, archive what will be lost), journal (intent row plus markers), execute (write, then commit records in one transaction).
    /// A failure in any stage removes the markers and the intent row, leaves the records untouched, and returns the error with its operation and path.
    /// For skipped references, retain existing database entries and mark them unresolved. Returns warnings that did not stop the operation.
    fn apply(&self, rel: &Relation, refs: &[Ref], slots: &[Slot], ident: &Identifier, ops: Vec<Op>, skipped: &[(String, String)]) -> Result<Vec<String>> {
        let by_id: BTreeMap<&str, (&Ref, &Slot)> = refs.iter().zip(slots).map(|(r, s)| (r.id(), (r, s))).collect();
        let find = |id: &str| by_id.get(id).copied().ok_or_else(|| anyhow::anyhow!("plan references unknown ref {id:?}"));

        // Stage 1: read every donor once, hashing the bytes that will be written, and archive each target about to be overwritten or deleted.
        let mut donors: BTreeMap<&str, Donor> = BTreeMap::new();
        for op in &ops {
            if let Op::Copy { from, .. } = op {
                if !donors.contains_key(from.as_str()) {
                    let (rf, slot) = find(from)?;
                    let path = rf.locate(ident);
                    let bytes = fs::read(&path).with_context(|| format!("reading donor {}", path.display()))?;
                    let mode = fs::metadata(&path).ok().map(|m| mode_bits(&m));
                    let mtime = match &slot.status {
                        Status::Present(sn) => sn.mtime,
                        _ => bail!("donor {from:?} has no present snapshot"),
                    };
                    let hash = *blake3::hash(&bytes).as_bytes();
                    donors.insert(from, Donor { bytes, hash, mode, mtime });
                }
            }
        }
        for op in &ops {
            match op {
                Op::Copy { from, to } => {
                    let (dest, slot) = find(to)?;
                    let donor = &donors[from.as_str()];
                    if matches!(&slot.status, Status::Present(sn) if sn.hash != donor.hash) {
                        self.preserve(rel, ident, to, &dest.locate(ident), "overwritten")?;
                    }
                }
                Op::Delete { at } => {
                    let (rf, _) = find(at)?;
                    self.preserve(rel, ident, at, &rf.locate(ident), "deleted")?;
                }
            }
        }

        // Stage 2: journal the intent, then place marker files beside creation and deletion targets.
        // Markers left on disk after an interrupted run signal incomplete file additions or removals during recovery.
        if !ops.is_empty() {
            self.store.write_intent(&rel.name, ident, &describe(&ops)).context("writing intent row")?;
        }
        let mut markers = Markers::default();
        for op in &ops {
            let (word, target_id) = match op {
                Op::Copy { to, .. } => {
                    let (_, slot) = find(to)?;
                    if slot.status != Status::Missing {
                        continue;
                    }
                    ("create", to)
                }
                Op::Delete { at } => ("delete", at),
            };
            let target = find(target_id)?.0.locate(ident);
            match write_marker(&target, &rel.name, ident, target_id, word) {
                Ok(marker) => markers.paths.push(marker),
                Err(e) => {
                    let _ = self.store.clear_intent(&rel.name, ident);
                    return Err(e);
                }
            }
        }

        // Stage 3: execute, then commit. Outcomes start as the current slot states so copies and deletes can record their new digests.
        let mut outcomes: BTreeMap<&str, Option<Snapshot>> = slots.iter().map(|s| {
            let snap = match &s.status {
                Status::Present(snap) => Some(snap.clone()),
                _ => None,
            };
            (s.ref_id.as_str(), snap)
        }).collect();
        let skip_reason: BTreeMap<&str, &str> = skipped.iter().map(|(id, reason)| (id.as_str(), reason.as_str())).collect();
        let mut warnings = Vec::new();
        let executed: Result<()> = (|| {
            for op in &ops {
                match op {
                    Op::Copy { from, to } => {
                        let (dest, slot) = find(to)?;
                        let donor = &donors[from.as_str()];
                        let dest_path = dest.locate(ident);
                        // A new file takes the donor's permission bits; an overwrite keeps the destination's own, as cp does.
                        let mode = match &slot.status {
                            Status::Missing => donor.mode,
                            _ => fs::metadata(&dest_path).ok().map(|m| mode_bits(&m)).or(donor.mode),
                        };
                        let mtime = rel.sync_times.then_some(donor.mtime);
                        let written = write_atomic(&dest_path, &donor.bytes, mtime, mode)?;
                        warnings.extend(written.warning);
                        let snap = Snapshot { mtime: written.mtime, size: donor.bytes.len() as u64, hash: donor.hash };
                        outcomes.insert(to.as_str(), Some(snap));
                    }
                    Op::Delete { at } => {
                        let (rf, _) = find(at)?;
                        let path = rf.locate(ident);
                        match fs::remove_file(&path) {
                            Ok(()) => {}
                            Err(e) if e.kind() == ErrorKind::NotFound => {}
                            Err(e) => return Err(e).with_context(|| format!("deleting {}", path.display())),
                        }
                        outcomes.insert(at.as_str(), None);
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = executed {
            // Leave the records as they were: the next cycle re-plans from the last consistent baseline.
            let _ = self.store.clear_intent(&rel.name, ident);
            drop(markers);
            return Err(e);
        }

        // Commit outcomes and skip reasons in one transaction, then drop the intent row with them.
        let committed = self.store.transaction(|store| {
            for (ref_id, snap) in &outcomes {
                match skip_reason.get(ref_id) {
                    Some(reason) => store.mark_unresolved(&rel.name, ident, ref_id, reason)?,
                    None => {
                        store.commit_record(&rel.name, ident, ref_id, snap.as_ref())?;
                        store.clear_unresolved(&rel.name, ident, ref_id)?;
                    }
                }
            }
            store.clear_intent(&rel.name, ident)
        }).context("committing records");
        if let Err(e) = committed {
            // The files were written and the error is reported with its cause, so the journal row must not read as a crash at the next start.
            let _ = self.store.clear_intent(&rel.name, ident);
            drop(markers);
            return Err(e);
        }
        drop(markers);
        Ok(warnings)
    }

    /// Save a target file to the archive before it is modified or removed. A missing file needs no backup.
    /// Failing to archive aborts the plan so unarchived contents are never destroyed.
    fn preserve(&self, rel: &Relation, ident: &Identifier, target_id: &str, path: &Path, reason: &str) -> Result<()> {
        let Some(arch) = &self.archive else { return Ok(()) };
        match fs::read(path) {
            Ok(bytes) => arch.preserve(&self.store, &rel.name, ident, target_id, path, &bytes, reason)
                .with_context(|| format!("archiving {} before it is {reason}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("reading {} for the archive", path.display())),
        }
    }

    /// Compile status reports for each configured relation, capturing errors into failed relation entries.
    /// Relations involved in a root overlap are reported with that error, as the cycle skips them.
    pub fn report(&self) -> Report {
        let (expanded, failed_expansions) = self.expand_all();
        let conflicts = footprint_conflicts(&self.relations, &expanded, &self.reserved);
        let mut relations = Vec::new();
        for (i, rel) in self.relations.iter().enumerate() {
            let failed = |error: String| RelationReport {
                name: rel.name.clone(),
                labels: Vec::new(),
                refs: Vec::new(),
                rows: Vec::new(),
                gated: self.gates.relation_flag(&rel.name),
                unavailable: None,
                warnings: Vec::new(),
                error: Some(error),
            };
            let overlaps: Vec<&str> = conflicts.iter().filter(|c| c.relations.contains(&i)).map(|c| c.message.as_str()).collect();
            if !overlaps.is_empty() {
                relations.push(failed(overlaps.join("; ")));
                continue;
            }
            let Some((_, refs)) = expanded.iter().find(|(j, _)| *j == i) else {
                let error = failed_expansions.iter().find(|(j, _)| *j == i).map(|(_, e)| e.clone()).unwrap_or_else(|| "could not expand labels".into());
                relations.push(failed(error));
                continue;
            };
            match self.report_relation(rel, refs) {
                Ok(r) => relations.push(r),
                Err(e) => relations.push(failed(format!("{e:#}"))),
            }
        }
        Report { relations, globally_gated: self.gates.globally_gated() }
    }

    /// Compile the status report for a relation, reading records and statuses exactly as a cycle would.
    fn report_relation(&self, rel: &Relation, expanded: &[Ref]) -> Result<RelationReport> {
        let labels = rel.labels.iter().map(|l| {
            let mut notes = Vec::new();
            if l.endpoints.len() > 1 {
                notes.push(format!("{} roots", l.endpoints.len()));
            }
            for ep in &l.endpoints {
                if let Mapping::Fixed(leaf) = &ep.mapping {
                    notes.push(format!("stored as {leaf}"));
                }
                if let Shape::Scan { template, hole, dir } = &ep.shape {
                    notes.push(format!("scan {template} per {{{hole}}} under {}", dir.display()));
                }
            }
            LabelView { name: l.name.clone(), notes }
        }).collect();

        let names = display_names(expanded);
        let refs: Vec<RefView> = expanded.iter().zip(&names).map(|(rf, name)| RefView {
            label: rf.label.clone(),
            id: rf.id().to_owned(),
            name: name.clone(),
            root: rf.root.clone(),
            root_exists: rf.root.is_dir(),
            pattern: match &rf.mapping_kind {
                MappingKind::Template(t) => Some(t.text.clone()),
                _ => None,
            },
        }).collect();

        let records = self.store.records_for(&rel.name)?;
        let gated = self.gates.relation_flag(&rel.name);
        // An unavailable root stops the cycle before any plan, so the report carries the reason instead of plans the engine would refuse.
        if let Some(reason) = self.unavailable_root(expanded, &records) {
            return Ok(RelationReport { name: rel.name.clone(), labels, refs, rows: Vec::new(), gated, unavailable: Some(reason), warnings: Vec::new(), error: None });
        }

        // Collect identifiers across all expanded references, failing the relation exactly where a cycle would.
        let found = self.identifiers(rel, expanded, &records)?;
        let warnings = found.failures;
        let unresolved = self.store.unresolved_for(&rel.name)?;

        // Compile status rows and evaluate policy plans per identifier.
        let mut rows = Vec::new();
        for ident in &found.idents {
            let insp = self.inspect(expanded, ident, &records, &unresolved);
            let cells = insp.slots.iter().map(|s| CellView { status: s.status.clone(), change: s.change() }).collect();
            let (plan, note) = if insp.settled || insp.all_missing {
                (Vec::new(), None)
            } else {
                match rel.policy.decide(&Instance { slots: insp.slots }) {
                    Decision::Plan { ops, skipped } => {
                        let name_of = |id: &str| expanded.iter().position(|r| r.id() == id).map(|i| names[i].clone()).unwrap_or_else(|| id.to_owned());
                        let note = (!skipped.is_empty()).then(|| {
                            skipped.iter().map(|(rf, reason)| format!("{}: {reason}", name_of(rf))).collect::<Vec<_>>().join("; ")
                        });
                        (ops, note)
                    }
                    Decision::Skip(reason) => (Vec::new(), Some(reason)),
                }
            };
            let plan_text = describe_plan(expanded, &names, &plan);
            rows.push(RowView { identifier: identifier_to_string(ident), cells, plan, plan_text, note });
        }

        Ok(RelationReport { name: rel.name.clone(), labels, refs, rows, gated, unavailable: None, warnings, error: None })
    }
}

/// Identifiers found for a relation, the subfolders that could not be read, and recorded identifiers the pattern no longer covers.
struct Discovery {
    idents: BTreeSet<Identifier>,
    failures: Vec<String>,
    dropped: Vec<Identifier>,
}

/// Bytes and metadata of a donor file, read once per plan.
struct Donor {
    bytes: Vec<u8>,
    hash: [u8; 32],
    /// Permission bits (masked to 0o777) on Unix, None where unavailable.
    mode: Option<u32>,
    mtime: SystemTime,
}

/// Marker files placed for the duration of a plan; removed when dropped, whether the plan succeeded or failed.
#[derive(Default)]
struct Markers {
    paths: Vec<PathBuf>,
}

impl Drop for Markers {
    fn drop(&mut self) {
        for marker in &self.paths {
            let _ = fs::remove_file(marker);
        }
    }
}

/// Read permission bits masked to the classic rwx triplet.
fn mode_bits(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

/// Aggregated status report across all configured relations.
pub struct Report {
    pub relations: Vec<RelationReport>,
    pub globally_gated: bool,
}

/// One state per relation, ordered worst first so the global state is the minimum.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RelationState {
    /// The relation could not be evaluated.
    Error,
    /// A root with records is missing; nothing runs until it returns.
    Unavailable,
    /// Some files await a sync.
    Awaiting(usize),
    /// Some files have plans the gate holds back.
    Held(usize),
    /// Part of a tree could not be read.
    Degraded,
    /// A configured folder does not exist yet.
    FolderMissing,
    InSync,
}

/// Status report for a relation, detailing resolved references and evaluated identifier rows.
pub struct RelationReport {
    pub name: String,
    pub labels: Vec<LabelView>,
    pub refs: Vec<RefView>,
    pub rows: Vec<RowView>,
    /// Gate flag for this relation.
    pub gated: bool,
    /// Why the engine refuses to evaluate the relation this cycle: a root with records is missing.
    pub unavailable: Option<String>,
    /// Subfolders the walk could not read.
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

impl RelationReport {
    /// Count rows awaiting a sync and rows held by a gate. Under a gate only rows with a plan are held; a conflict is awaiting either way.
    pub fn counts(&self, globally_gated: bool) -> (usize, usize) {
        let gated = globally_gated || self.gated;
        let planned = self.rows.iter().filter(|r| !r.plan.is_empty()).count();
        let noted = self.rows.iter().filter(|r| r.plan.is_empty() && r.note.is_some()).count();
        if gated { (noted, planned) } else { (planned + noted, 0) }
    }

    /// Derive the relation's state from its rows and notes, given whether the global gate is closed.
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
        if self.refs.iter().any(|r| !r.root_exists) {
            return RelationState::FolderMissing;
        }
        RelationState::InSync
    }
}

/// Display metadata for a relation label and its endpoint configurations.
pub struct LabelView {
    pub name: String,
    pub notes: Vec<String>,
}

/// Expanded reference column in relation report tables.
pub struct RefView {
    pub label: String,
    pub id: String,
    /// Display name: the label, qualified by the scan hole value or the shortest unique path suffix when the label has several refs.
    pub name: String,
    pub root: PathBuf,
    pub root_exists: bool,
    /// Template path pattern for template-mapped references.
    pub pattern: Option<String>,
}

/// File status entry across reference locations, pairing evaluated cells with planned operations and decision notes.
pub struct RowView {
    pub identifier: String,
    pub cells: Vec<CellView>,
    /// Operations scheduled for synchronization after resolving policy rules.
    pub plan: Vec<Op>,
    /// The plan phrased with display names, as the dashboard and the log show it.
    pub plan_text: String,
    pub note: Option<String>,
}

impl RowView {
    /// A row needs attention when the next cycle would act on it or a conflict holds it.
    pub fn needs_attention(&self) -> bool {
        !self.plan.is_empty() || self.note.is_some()
    }
}

/// File status at a reference location, pairing current filesystem status with change history.
pub struct CellView {
    pub status: Status,
    pub change: Change,
}

/// Expand a relation's labels into refs.
fn expand(rel: &Relation) -> Result<Vec<Ref>> {
    Ok(rel.labels.iter().map(|l| l.refs(&list_subdirs)).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect())
}

/// Format operations into text such as `copy a -> b; delete c` by ref id, for the intent journal.
/// The journal keeps ref ids rather than display names, since ids are the stable key a recovery tool would need.
fn describe(ops: &[Op]) -> String {
    ops.iter().map(|op| match op {
        Op::Copy { from, to } => format!("copy {from} -> {to}"),
        Op::Delete { at } => format!("delete {at}"),
    }).collect::<Vec<_>>().join("; ")
}

/// Phrase a plan with display names, grouping copies by donor: `copy source -> dest (a), dest (b); delete at dest (c)`.
/// The ASCII arrow matches the intent journal and the log lines written before this formatter existed.
pub fn describe_plan(refs: &[Ref], names: &[String], ops: &[Op]) -> String {
    let name = |id: &str| refs.iter().position(|r| r.id() == id).map(|i| names[i].clone()).unwrap_or_else(|| id.to_owned());
    let mut copies: Vec<(String, Vec<String>)> = Vec::new();
    let mut deletes: Vec<String> = Vec::new();
    for op in ops {
        match op {
            Op::Copy { from, to } => {
                let donor = name(from);
                let dest = name(to);
                match copies.iter_mut().find(|(d, _)| *d == donor) {
                    Some((_, dests)) => dests.push(dest),
                    None => copies.push((donor, vec![dest])),
                }
            }
            Op::Delete { at } => deletes.push(name(at)),
        }
    }
    let mut parts: Vec<String> = copies.into_iter()
        .map(|(donor, dests)| format!("copy {donor} -> {}", dests.join(", ")))
        .collect();
    if !deletes.is_empty() {
        parts.push(format!("delete at {}", deletes.join(", ")));
    }
    parts.join("; ")
}

/// Collect sorted names of immediate subdirectories under `dir` for root expansion.
fn list_subdirs(dir: &Path) -> Result<Vec<String>> {
    let mut entries = Vec::new();
    for e in fs::read_dir(dir).with_context(|| format!("scanning {}", dir.display()))? {
        let e = e?;
        if e.file_type()?.is_dir() {
            entries.push(e.file_name().to_string_lossy().into_owned());
        }
    }
    entries.sort();
    Ok(entries)
}

/// Read filesystem metadata and content hash for the entry at `path`.
/// Reuse the recorded hash when modification time and size match the prior snapshot, streaming the file through BLAKE3 only when metadata changes or no record exists.
fn read_status(path: &Path, record: Option<&Snapshot>) -> Status {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == ErrorKind::NotFound => return Status::Missing,
        Err(e) => return Status::Unreadable(e.to_string()),
    };
    if !meta.is_file() {
        return Status::Unreadable(format!("{} is not a regular file", path.display()));
    }
    let mtime = match meta.modified() {
        Ok(t) => t,
        Err(e) => return Status::Unreadable(e.to_string()),
    };
    let size = meta.len();
    if let Some(r) = record {
        // Skip reading file contents when modification time (to the millisecond the store keeps) and size match the recorded snapshot.
        if to_epoch_ms(r.mtime) == to_epoch_ms(mtime) && r.size == size {
            return Status::Present(Snapshot { mtime, size, hash: r.hash });
        }
    }
    let hashed = fs::File::open(path).and_then(|f| {
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(f)?;
        Ok(*hasher.finalize().as_bytes())
    });
    match hashed {
        Ok(hash) => Status::Present(Snapshot { mtime, size, hash }),
        Err(e) => Status::Unreadable(e.to_string()),
    }
}

/// Result of an atomic write: the mtime the filesystem recorded, plus a warning when a non-fatal step such as chmod failed.
struct Written {
    mtime: SystemTime,
    warning: Option<String>,
}

/// Longest file name most filesystems accept, in bytes.
const NAME_MAX: usize = 255;

/// Build a sibling name `<prefix><name><suffix>` for `path`, falling back to a hash of the name when the suffix would push it past NAME_MAX.
fn sibling_name(path: &Path, prefix: &str, suffix: &str) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
    if prefix.len() + name.len() + suffix.len() <= NAME_MAX {
        format!("{prefix}{name}{suffix}")
    } else {
        format!("{prefix}{}{suffix}", &blake3::hash(name.as_bytes()).to_hex()[..16])
    }
}

/// Stage byte writes into a sibling temporary file, apply the mode and modification time, flush to disk, then atomically rename over `path`.
/// Return the modification time recorded by the filesystem after renaming. A chmod failure is reported as a warning, since some filesystems refuse it.
fn write_atomic(path: &Path, bytes: &[u8], mtime: Option<SystemTime>, mode: Option<u32>) -> Result<Written> {
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let tmp = parent.join(sibling_name(path, ".", TEMP_SUFFIX));
    let mut warning = None;
    {
        let mut f = fs::File::create(&tmp).with_context(|| format!("creating temp file {}", tmp.display()))?;
        // Set the mode before any byte lands, so a private donor's contents are never world-readable in the temp file.
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = f.set_permissions(fs::Permissions::from_mode(m)) {
                warning = Some(format!("could not set mode {m:o} on {}: {e}", path.display()));
            }
        }
        f.write_all(bytes).with_context(|| format!("writing {}", tmp.display()))?;
        if let Some(t) = mtime {
            f.set_modified(t).with_context(|| format!("setting mtime on {}", tmp.display()))?;
        }
        f.sync_all().with_context(|| format!("syncing {}", tmp.display()))?;
    }
    fs::rename(&tmp, path).with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    // A directory sync makes the rename durable; filesystems that refuse it still hold the synced file contents.
    if let Ok(d) = fs::File::open(parent) {
        let _ = d.sync_all();
    }
    let mtime = fs::symlink_metadata(path).and_then(|m| m.modified()).with_context(|| format!("reading mtime of {}", path.display()))?;
    Ok(Written { mtime, warning })
}

// Temporary file suffix for staging atomic writes before renaming.
const TEMP_SUFFIX: &str = ".replica-write";
// Diagnostic marker file suffix placed alongside in-progress operations.
const MARKER_SUFFIX: &str = ".replica.tmp";

/// Write an informational marker file adjacent to `target` during file modifications.
/// Record the process id, relation, identifier, and operation type to diagnose in-progress or interrupted writes before the marker is removed on commit.
fn write_marker(target: &Path, relation: &str, ident: &Identifier, ref_id: &str, op: &str) -> Result<PathBuf> {
    let parent = target.parent().ok_or_else(|| anyhow::anyhow!("{} has no parent directory", target.display()))?;
    let marker = parent.join(sibling_name(target, "", MARKER_SUFFIX));
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let started = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    fs::write(&marker, format!(
        "service:    replica\n\
         pid:        {}\n\
         relation:   {relation}\n\
         identifier: {}\n\
         ref:        {ref_id}\n\
         op:         {op}\n\
         started:    {started}\n\
         note:       this marker is removed when the operation commits. if you are reading it, the operation is still running or died partway.\n",
        std::process::id(),
        identifier_to_string(ident),
    )).with_context(|| format!("writing marker {}", marker.display()))?;
    Ok(marker)
}

/// Identifiers found under a root, plus the subfolders that could not be read.
struct Walk {
    idents: Vec<Identifier>,
    failures: Vec<String>,
}

/// Traverse `root` recursively to collect relative identifier paths for regular files, descending at most `max_depth` levels (None for unbounded).
/// Yield an empty list when the root directory does not exist on disk, ignoring symbolic links and temporary files from earlier runs.
/// A root that cannot be read is an error; a subfolder that cannot be read is recorded as a failure and skipped.
fn walk_identifiers(root: &Path, max_depth: Option<usize>) -> Result<Walk> {
    let mut walk = Walk { idents: Vec::new(), failures: Vec::new() };
    if max_depth == Some(0) {
        return Ok(walk);
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        let depth = rel.components().count();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound && depth == 0 => return Ok(walk),
            Err(e) if depth == 0 => return Err(e).with_context(|| format!("walking {}", dir.display())),
            Err(e) => {
                walk.failures.push(format!("skipped {}: {e}", dir.display()));
                continue;
            }
        };
        for entry in entries {
            let (entry, ty) = match entry.and_then(|entry| entry.file_type().map(|ty| (entry, ty))) {
                Ok(pair) => pair,
                Err(e) => {
                    walk.failures.push(format!("skipped an entry in {}: {e}", dir.display()));
                    continue;
                }
            };
            let child = rel.join(entry.file_name());
            if ty.is_dir() {
                // Files at depth d+1 are allowed when d+1 <= max_depth; descend only while that holds.
                if max_depth.is_none_or(|max| depth + 1 < max) {
                    stack.push(child);
                }
            } else if ty.is_file() && {
                // Skip temporary write and marker files during traversal.
                let name = entry.file_name().to_string_lossy().into_owned();
                !name.ends_with(TEMP_SUFFIX) && !name.ends_with(MARKER_SUFFIX)
            } {
                walk.idents.push(child.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect());
            }
        }
    }
    Ok(walk)
}

/// A pair of footprints that overlap, or a footprint that contains or lies inside a reserved path.
pub struct Conflict {
    /// Indices into the relation list of every relation involved.
    pub relations: Vec<usize>,
    pub message: String,
    /// True when a reserved path is involved: a static configuration error rather than two roots meeting.
    pub reserved: bool,
}

/// Find every root that nests inside another root across all refs, and every root that contains or lies inside a reserved path.
/// A fixed ref claims one file, so it conflicts only with a root above that file. Footprints are sorted so each prefix relation is found in a forward scan.
pub fn footprint_conflicts(relations: &[Relation], expanded: &[(usize, Vec<Ref>)], reserved: &[PathBuf]) -> Vec<Conflict> {
    let mut items: Vec<(PathBuf, usize, &Ref)> = expanded.iter()
        .flat_map(|(i, refs)| refs.iter().map(move |rf| (rf.footprint(), *i, rf)))
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (i, (fa, ra, a)) in items.iter().enumerate() {
        // Every footprint that starts with fa sorts directly after it, so stop at the first that does not.
        for (fb, rb, b) in items[i + 1..].iter().take_while(|(fb, _, _)| fb.starts_with(fa)) {
            out.push(Conflict {
                relations: if ra == rb { vec![*ra] } else { vec![(*ra).min(*rb), (*ra).max(*rb)] },
                message: format!(
                    "overlapping roots: {} ({}/{}) and {} ({}/{})",
                    fa.display(), relations[*ra].name, a.id(), fb.display(), relations[*rb].name, b.id()
                ),
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

/// A problem `check` found: its message, and whether it involves a reserved path (a configuration error to refuse rather than skip).
pub struct Problem {
    pub message: String,
    pub reserved: bool,
}

/// Expand every relation and report every footprint conflict, for `--check` and startup. Expansion errors are reported as problems too.
pub fn check(relations: &[Relation], reserved: &[PathBuf]) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut expanded = Vec::new();
    for (i, rel) in relations.iter().enumerate() {
        match expand(rel) {
            Ok(refs) => expanded.push((i, refs)),
            Err(e) => problems.push(Problem { message: format!("{}: {e:#}", rel.name), reserved: false }),
        }
    }
    problems.extend(footprint_conflicts(relations, &expanded, reserved).into_iter().map(|c| Problem { message: c.message, reserved: c.reserved }));
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn sibling_names_fall_back_to_a_hash_past_name_max() {
        let short = Path::new("/d/notes.txt");
        assert_eq!(sibling_name(short, ".", TEMP_SUFFIX), ".notes.txt.replica-write");
        assert_eq!(sibling_name(short, "", MARKER_SUFFIX), "notes.txt.replica.tmp");
        let long = PathBuf::from("/d").join("x".repeat(250));
        let tmp = sibling_name(&long, ".", TEMP_SUFFIX);
        assert!(tmp.len() <= NAME_MAX && tmp.starts_with('.') && tmp.ends_with(TEMP_SUFFIX), "{tmp}");
        let marker = sibling_name(&long, "", MARKER_SUFFIX);
        assert!(marker.len() <= NAME_MAX && marker.ends_with(MARKER_SUFFIX), "{marker}");
    }

    #[test]
    fn walk_stops_at_the_allowed_depth_and_skips_unreadable_subfolders() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("top.md"), "").unwrap();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/mid.md"), "").unwrap();
        fs::write(root.join("a/b/deep.md"), "").unwrap();
        let names = |w: &Walk| { let mut v: Vec<String> = w.idents.iter().map(identifier_to_string).collect(); v.sort(); v };
        assert_eq!(names(&walk_identifiers(root, Some(1)).unwrap()), vec!["top.md"]);
        assert_eq!(names(&walk_identifiers(root, Some(2)).unwrap()), vec!["a/mid.md", "top.md"]);
        assert_eq!(names(&walk_identifiers(root, None).unwrap()), vec!["a/b/deep.md", "a/mid.md", "top.md"]);
        assert!(walk_identifiers(root, Some(0)).unwrap().idents.is_empty());
        assert!(walk_identifiers(&root.join("absent"), None).unwrap().idents.is_empty());

        // An unreadable subfolder is skipped and reported; the rest of the tree is still walked.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root.join("a/b"), fs::Permissions::from_mode(0o000)).unwrap();
        let w = walk_identifiers(root, None).unwrap();
        fs::set_permissions(root.join("a/b"), fs::Permissions::from_mode(0o755)).unwrap();
        if nix_is_root() {
            return;
        }
        assert_eq!(names(&w), vec!["a/mid.md", "top.md"]);
        assert_eq!(w.failures.len(), 1, "{:?}", w.failures);
        assert!(w.failures[0].contains("a/b"));
        // An unreadable root is still a hard error.
        fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o000)).unwrap();
        let err = walk_identifiers(&root.join("a"), None).is_err();
        fs::set_permissions(root.join("a"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err);
    }

    /// Root ignores permission bits, so the unreadable-folder assertions only hold for ordinary users.
    fn nix_is_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn mtime_comparison_tolerates_sub_millisecond_precision() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f.txt");
        fs::write(&path, "hello").unwrap();
        // Pin an mtime with a sub-millisecond remainder, so the test does not depend on the filesystem's clock granularity.
        let pinned = SystemTime::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_789);
        fs::File::options().write(true).open(&path).unwrap().set_modified(pinned).unwrap();
        let live = fs::metadata(&path).unwrap().modified().unwrap();
        assert_ne!(to_epoch_ms(live) as u128 * 1_000_000, live.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos(), "the filesystem keeps sub-millisecond precision");
        let hash = *blake3::hash(b"hello").as_bytes();
        // A record round-tripped through the store keeps only milliseconds.
        let stored = crate::store::from_epoch_ms(crate::store::to_epoch_ms(live));
        let record = Snapshot { mtime: stored, size: 5, hash: [9; 32] };
        // The fast path trusts the recorded hash when mtime (to the millisecond) and size match.
        assert_eq!(read_status(&path, Some(&record)), Status::Present(Snapshot { mtime: live, size: 5, hash: [9; 32] }));
        // A size change forces a re-hash.
        let record = Snapshot { mtime: stored, size: 4, hash: [9; 32] };
        assert_eq!(read_status(&path, Some(&record)), Status::Present(Snapshot { mtime: live, size: 5, hash }));
    }

    #[test]
    fn describe_plan_groups_copies_by_donor_and_names_deletes() {
        let mk = |label: &str, root: &str| Ref::new(label.into(), BTreeMap::new(), root.into(), MappingKind::Direct);
        let refs = vec![mk("source", "/s"), mk("dest", "/p/a/docs"), mk("dest", "/p/b/docs"), mk("dest", "/p/c/docs")];
        let names = display_names(&refs);
        let ops = vec![
            Op::Copy { from: "source".into(), to: "dest?root=/p/a/docs".into() },
            Op::Copy { from: "source".into(), to: "dest?root=/p/b/docs".into() },
            Op::Delete { at: "dest?root=/p/c/docs".into() },
        ];
        // These refs carry no root tag (single endpoint each in this synthetic list), so unknown ids fall back to the id text.
        assert_eq!(describe_plan(&refs, &names, &ops), "copy source -> dest?root=/p/a/docs, dest?root=/p/b/docs; delete at dest?root=/p/c/docs");
        let ops = vec![Op::Copy { from: "source".into(), to: "dest".into() }];
        let two = vec![mk("source", "/s"), mk("dest", "/d")];
        assert_eq!(describe_plan(&two, &display_names(&two), &ops), "copy source -> dest");
        assert_eq!(describe_plan(&two, &display_names(&two), &[]), "");
    }

    #[test]
    fn attention_follows_the_plan_and_the_note_not_presence() {
        let row = |plan: Vec<Op>, note: Option<&str>| RowView {
            identifier: "f".into(),
            cells: vec![CellView { status: Status::Present(Snapshot { mtime: SystemTime::UNIX_EPOCH, size: 1, hash: [0; 32] }), change: Change::Appeared }, CellView { status: Status::Missing, change: Change::Unchanged }],
            plan_text: String::new(),
            plan,
            note: note.map(str::to_owned),
        };
        // One-sided presence alone is not attention: nothing will happen to this row.
        assert!(!row(vec![], None).needs_attention());
        assert!(row(vec![Op::Copy { from: "a".into(), to: "b".into() }], None).needs_attention());
        assert!(row(vec![], Some("conflict")).needs_attention());
    }

    /// The chmod-failure warning path needs a filesystem that refuses chmod (exFAT, SMB) and is not exercised here.
    #[test]
    fn write_atomic_applies_the_requested_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("secret.txt");
        let w = write_atomic(&path, b"x", None, Some(0o600)).unwrap();
        assert!(w.warning.is_none());
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let w = write_atomic(&path, b"y", None, None).unwrap();
        assert!(w.warning.is_none());
        assert_eq!(fs::read(&path).unwrap(), b"y");
    }

    fn rel(name: &str, roots: &[(&str, &str)]) -> Relation {
        Relation {
            name: name.into(),
            labels: roots.iter().map(|(label, root)| Label {
                name: (*label).into(),
                endpoints: vec![crate::label::Endpoint { shape: Shape::Literal { root: (*root).into() }, mapping: Mapping::Direct }],
            }).collect(),
            pattern: Pattern::All,
            policy: Policy::default(),
            sync_times: true,
            gate_default: false,
        }
    }

    #[test]
    fn footprint_conflicts_use_file_footprints_for_renames_and_catch_reserved_paths() {
        let mut fixed = rel("nginx", &[("source", "/etc/nginx/conf.d")]);
        fixed.labels.push(Label {
            name: "dest".into(),
            endpoints: vec![crate::label::Endpoint { shape: Shape::Literal { root: "/u/Desktop".into() }, mapping: Mapping::Fixed("nginx-conf.txt".into()) }],
        });
        let relations = vec![
            rel("lib", &[("source", "/u/Desktop/mylibrary"), ("dest", "/u/scripts/desktop-library")]),
            fixed,
            rel("nested", &[("source", "/u/scripts"), ("dest", "/elsewhere")]),
        ];
        let expanded: Vec<(usize, Vec<Ref>)> = relations.iter().enumerate().map(|(i, r)| (i, expand(r).unwrap())).collect();
        // The rename endpoint claims one file under /u/Desktop, so it does not overlap /u/Desktop/mylibrary.
        let conflicts = footprint_conflicts(&relations[..2], &expanded[..2], &[]);
        assert!(conflicts.is_empty(), "{:?}", conflicts.iter().map(|c| &c.message).collect::<Vec<_>>());
        // /u/scripts contains /u/scripts/desktop-library: both relations are named.
        let conflicts = footprint_conflicts(&relations, &expanded, &[]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].relations, vec![0, 2]);
        assert!(conflicts[0].message.contains("overlapping roots"));
        // A reserved path inside a root is a conflict for that relation alone.
        let reserved = vec![PathBuf::from("/u/scripts/replica/live/state.db")];
        let conflicts = footprint_conflicts(&relations, &expanded, &reserved);
        assert_eq!(conflicts.len(), 2);
        assert!(conflicts.iter().any(|c| c.relations == vec![2] && c.message.contains("state.db")), "{:?}", conflicts.iter().map(|c| &c.message).collect::<Vec<_>>());
    }
}
