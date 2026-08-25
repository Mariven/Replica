//! Discover files, decide what to do, and apply the plan.
//!
//! We write to a temp file in the same directory, then rename. We write an intent row first and clear it after commit.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as AnyhowContext, Result, bail};

use crate::archive::Archive;
use crate::identifier::{Identifier, Pattern, identifier_to_string};
use crate::policy::{Decision, Instance, Op, Policy, Slot};
use crate::label::{Label, Mapping, MappingKind, Ref, Shape};
use crate::status::{Change, Snapshot, Status, classify};
use crate::store::Store;

/// The labels, glob, and policy that make up a relation. `gate_default` is whether writes start gated.
pub struct Relation {
    pub name: String,
    pub labels: Vec<Label>,
    pub pattern: Pattern,
    pub policy: Policy,
    pub sync_times: bool,
    /// The config asked this relation to start gated. The live flag lives in [`Gates`].
    pub gate_default: bool,
}

/// While a gate is closed we still observe and plan. File writes and record updates resume when the gate opens.
/// The config chooses the starting value. The dashboard can change it later. The global gate applies to every relation.
#[derive(Default)]
pub struct Gates {
    pub global: AtomicBool,
    pub relations: Mutex<BTreeMap<String, bool>>,
}

impl Gates {
    /// True when the global gate is closed.
    pub fn globally_gated(&self) -> bool {
        self.global.load(Ordering::Relaxed)
    }

    /// This relation's own flag, without looking at the global gate.
    pub fn relation_flag(&self, relation: &str) -> bool {
        *self.relations.lock().unwrap().get(relation).unwrap_or(&false)
    }

    /// This relation's writes are held: either its own flag is set, or the global gate is closed.
    pub fn is_gated(&self, relation: &str) -> bool {
        self.globally_gated() || self.relation_flag(relation)
    }

    /// Close or open the global gate.
    pub fn set_global(&self, gated: bool) {
        self.global.store(gated, Ordering::Relaxed);
    }

    /// Close or open this relation's gate.
    pub fn set_relation(&self, relation: &str, gated: bool) {
        self.relations.lock().unwrap().insert(relation.into(), gated);
    }
}

/// After we finish a file we send this to every hook, including the log writers.
#[derive(Debug)]
pub enum Event {
    Resolved { relation: String, identifier: String, ops: usize },
    Held { relation: String, identifier: String, detail: String },
    Skipped { relation: String, identifier: String, reason: String },
    Pending { relation: String, identifier: String, reason: String },
    Errored { relation: String, identifier: String, error: String },
    StaleIntent { relation: String, identifier: String, detail: String },
}

/// A callback we run for every event.
pub type Hook = Box<dyn Fn(&Event) + Send>;

/// Run a sync cycle against the store and the configured relations.
pub struct Engine {
    pub store: Store,
    pub relations: Vec<Relation>,
    pub hooks: Vec<Hook>,
    pub gates: std::sync::Arc<Gates>,
    pub archive: Option<Archive>,
}

/// Stop this relation. The other relations in the cycle keep going.
struct RelationAbort(String);

impl Engine {
    /// Build an engine and set each relation's starting gate from the config.
    pub fn new(store: Store, relations: Vec<Relation>, hooks: Vec<Hook>) -> Engine {
        let gates = Gates::default();
        for rel in &relations {
            if rel.gate_default {
                gates.set_relation(&rel.name, true);
            }
        }
        Engine { store, relations, hooks, gates: std::sync::Arc::new(gates), archive: None }
    }

    /// Run every hook with this event.
    fn emit(&self, event: Event) {
        for hook in &self.hooks {
            hook(&event);
        }
    }

    /// If the last run left intent rows, it died mid-write. We log them, clear them, and let this cycle resolve those files from scratch.
    pub fn recover(&self) -> Result<()> {
        for (relation, ident, detail) in self.store.stale_intents()? {
            self.emit(Event::StaleIntent { relation: relation.clone(), identifier: identifier_to_string(&ident), detail });
            self.store.clear_intent(&relation, &ident)?;
        }
        Ok(())
    }

    /// Expand every label, fail if any roots overlap, then resolve each relation.
    pub fn cycle(&self) -> Result<()> {
        let mut expanded: Vec<(usize, Vec<Ref>)> = Vec::new();
        for (i, rel) in self.relations.iter().enumerate() {
            let refs: Result<Vec<Ref>> = rel.labels.iter().map(|r| r.refs(&list_subdirs)).collect::<Result<Vec<_>>>().map(|v| v.into_iter().flatten().collect());
            match refs {
                Ok(refs) => expanded.push((i, refs)),
                Err(e) => self.emit(Event::Errored { relation: rel.name.clone(), identifier: String::new(), error: e.to_string() }),
            }
        }

        validate_no_overlaps(&self.relations, &expanded)?;

        for (i, refs) in &expanded {
            let rel = &self.relations[*i];
            if let Err(e) = self.resolve_relation(rel, refs) {
                self.emit(Event::Errored { relation: rel.name.clone(), identifier: String::new(), error: e.to_string() });
            }
        }
        Ok(())
    }

    /// Walk each ref for files, and also include every identifier already in the record (so a file deleted everywhere is still seen).
    /// Fixed refs cannot identify files. Template refs only match paths that fit the template.
    fn identifiers(&self, rel: &Relation, refs: &[Ref]) -> Result<BTreeSet<Identifier>> {
        let mut idents: BTreeSet<Identifier> = BTreeSet::new();
        for rf in refs {
            if matches!(rf.mapping_kind, MappingKind::Fixed(_)) {
                continue;
            }
            for found in walk_identifiers(&rf.root)? {
                let path = found.iter().fold(rf.root.clone(), |p, c| p.join(c));
                if let Some(ident) = rf.identify(&path) {
                    if rel.pattern.matches(&ident) {
                        idents.insert(ident);
                    }
                }
            }
        }
        for ident in self.store.known_identifiers(&rel.name)? {
            idents.insert(ident);
        }
        Ok(idents)
    }

    /// If a root has records but the folder is gone, wait: that looks like an unmount, not a mass delete.
    /// Then resolve each identifier.
    fn resolve_relation(&self, rel: &Relation, refs: &[Ref]) -> Result<()> {
        // Records exist but the folder is gone: almost certainly an unmount or rename, not a mass delete. Wait.
        for rf in refs {
            if !rf.root.is_dir() && self.store.ref_has_records(&rel.name, &rf.id())? {
                self.emit(Event::Pending {
                    relation: rel.name.clone(),
                    identifier: String::new(),
                    reason: format!("{} has records but the folder is missing; treating as unavailable, not deleted", rf.root.display()),
                });
                return Ok(());
            }
        }

        let idents = self.identifiers(rel, refs)?;

        // Fixed maps every identifier to one path, so it is only coherent for single-identifier relations.
        let has_fixed = refs.iter().any(|s| matches!(s.mapping_kind, MappingKind::Fixed(_)));
        if has_fixed && idents.len() > 1 {
            bail!("relation {:?} has a fixed mapping but {} identifiers; fixed requires exactly one", rel.name, idents.len());
        }

        for ident in idents {
            match self.resolve(rel, refs, &ident) {
                Ok(()) => {}
                Err(e) if e.is::<RelationAbort>() => {
                    let RelationAbort(reason) = e.downcast::<RelationAbort>().unwrap();
                    bail!("relation {:?} aborted: {reason}", rel.name);
                }
                Err(e) => self.emit(Event::Errored { relation: rel.name.clone(), identifier: identifier_to_string(&ident), error: e.to_string() }),
            }
        }
        Ok(())
    }

    /// Read every slot, decide, and apply. If the gate is closed we report the plan and leave the files and the record as they are.
    fn resolve(&self, rel: &Relation, refs: &[Ref], ident: &Identifier) -> Result<()> {
        let gated = self.gates.is_gated(&rel.name);

        // Recorded-as-absent is not the same as never recorded. Only "never recorded" blocks the early exit below.
        let mut slots = Vec::new();
        let mut all_recorded = true;
        for rf in refs {
            let row = self.store.record(&rel.name, ident, &rf.id())?;
            all_recorded &= row.is_some();
            let record = row.flatten();
            let status = read_status(&rf.locate(ident), record.as_ref());
            slots.push((rf, Slot { ref_id: rf.id(), label: rf.label.clone(), record, status }));
        }

        // Missing everywhere: drop the rows and move on.
        // Forgetting writes the record, so a closed gate skips that too.
        if slots.iter().all(|(_, s)| s.status == Status::Missing) {
            if !gated {
                self.store.forget(&rel.name, ident)?;
            }
            return Ok(());
        }

        // Everything matches the record: nothing to do.
        // A brand-new scan root has no record yet, so it still goes through decide (policy may want to create files there).
        if all_recorded && slots.iter().all(|(_, s)| s.change() == Change::Unchanged) {
            return Ok(());
        }

        let instance = Instance { identifier: ident.clone(), slots: slots.iter().map(|(_, s)| s.clone()).collect() };
        let ident_str = identifier_to_string(ident);
        match rel.policy.decide(&instance) {
            Decision::Plan(ops) if gated => {
                // Gate closed: say what we would do, but do not write files or the record, so the same plan comes back next cycle.
                if !ops.is_empty() {
                    let detail: Vec<String> = ops.iter().map(|op| match op {
                        Op::Copy { from, to } => format!("copy {from} -> {to}"),
                        Op::Delete { at } => format!("delete {at}"),
                    }).collect();
                    self.emit(Event::Held { relation: rel.name.clone(), identifier: ident_str, detail: detail.join("; ") });
                }
            }
            Decision::Plan(ops) => {
                let n = ops.len();
                self.apply(rel, &slots, ident, ops)?;
                self.emit(Event::Resolved { relation: rel.name.clone(), identifier: ident_str, ops: n });
            }
            Decision::Skip(reason) => {
                self.emit(Event::Skipped { relation: rel.name.clone(), identifier: ident_str, reason });
            }
            Decision::NoDecision => {
                self.emit(Event::Skipped { relation: rel.name.clone(), identifier: ident_str, reason: "no decision".into() });
            }
            Decision::Pending(reason) => {
                self.emit(Event::Pending { relation: rel.name.clone(), identifier: ident_str, reason });
            }
            Decision::Abort(reason) => return Err(anyhow::Error::new(RelationAbort(reason))),
        }
        Ok(())
    }

    /// Write the intent, apply the copies and deletes (temp file + rename), update the record, then clear the intent.
    /// If we crash, the leftover intent is the evidence.
    fn apply(&self, rel: &Relation, slots: &[(&Ref, Slot)], ident: &Identifier, ops: Vec<Op>) -> Result<()> {
        let find = |id: &str| slots.iter().find(|(s, _)| s.id() == id).map(|(s, _)| *s)
            .ok_or_else(|| anyhow::anyhow!("plan references unknown rf {id:?}"));

        if !ops.is_empty() {
            let detail: Vec<String> = ops.iter().map(|op| match op {
                Op::Copy { from, to } => format!("copy {from} -> {to}"),
                Op::Delete { at } => format!("delete {at}"),
            }).collect();
            self.store.write_intent(&rel.name, ident, &detail.join("; "))?;
        }

        // Before a create or delete, drop a marker next to the file. Remove it after commit.
        // A leftover marker means the operation did not finish.
        // Only creates and deletes: they are rare and hard to undo. Marking every edit would teach people to ignore the markers.
        let mut markers: Vec<PathBuf> = Vec::new();
        for op in &ops {
            let (word, target_id) = match op {
                Op::Copy { to, .. } => {
                    let creating = slots.iter().any(|(rf, s)| rf.id() == *to && s.status == Status::Missing);
                    if !creating {
                        continue;
                    }
                    ("create", to)
                }
                Op::Delete { at } => ("delete", at),
            };
            let target = find(target_id)?.locate(ident);
            markers.push(write_marker(&target, &rel.name, ident, target_id, word)?);
        }

        // Start from the status we just observed; update as each op lands.
        let mut outcomes: Vec<(String, Option<Snapshot>)> = slots.iter().map(|(rf, s)| {
            let snap = match &s.status {
                Status::Present(snap) => Some(snap.clone()),
                _ => None,
            };
            (rf.id(), snap)
        }).collect();

        // Archive the current bytes before we overwrite or delete them.
        // If the archive write fails, abort: we would otherwise lose the only copy.
        let preserve = |target_id: &str, path: &Path, reason: &str| -> Result<()> {
            let Some(arch) = &self.archive else { return Ok(()) };
            match fs::read(path) {
                Ok(bytes) => arch.preserve(&self.store, &rel.name, ident, target_id, path, &bytes, reason),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.into()),
            }
        };

        for op in &ops {
            match op {
                Op::Copy { from, to } => {
                    let donor = find(from)?;
                    let dest = find(to)?;
                    let bytes = fs::read(donor.locate(ident))?;
                    let dest_path = dest.locate(ident);
                    let donor_snap = slots.iter().find(|(s, _)| s.id() == *from)
                        .and_then(|(_, s)| match &s.status { Status::Present(sn) => Some(sn.clone()), _ => None })
                        .ok_or_else(|| anyhow::anyhow!("donor {from:?} has no present snapshot"))?;
                    let overwriting = slots.iter().any(|(rf, s)| rf.id() == *to
                        && matches!(&s.status, Status::Present(sn) if sn.hash != donor_snap.hash));
                    if overwriting {
                        preserve(to, &dest_path, "overwritten")?;
                    }
                    let mtime = rel.sync_times.then_some(donor_snap.mtime);
                    let written = write_atomic(&dest_path, &bytes, mtime)?;
                    let snap = Snapshot { mtime: written, size: bytes.len() as u64, hash: donor_snap.hash };
                    if let Some(o) = outcomes.iter_mut().find(|(id, _)| id == to) {
                        o.1 = Some(snap);
                    }
                }
                Op::Delete { at } => {
                    let rf = find(at)?;
                    let path = rf.locate(ident);
                    preserve(at, &path, "deleted")?;
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                    if let Some(o) = outcomes.iter_mut().find(|(id, _)| id == at) {
                        o.1 = None;
                    }
                }
            }
        }

        for (ref_id, snap) in &outcomes {
            self.store.commit_record(&rel.name, ident, ref_id, snap.as_ref())?;
        }
        self.store.clear_intent(&rel.name, ident)?;
        for marker in markers {
            let _ = fs::remove_file(marker);
        }
        Ok(())
    }

    /// Walk the filesystem and the last-sync record so the dashboard can show every relation.
    /// Unchanged files only need a stat. We reuse the recorded hash.
    pub fn report(&self) -> Report {
        let mut relations = Vec::new();
        for rel in &self.relations {
            match self.report_relation(rel) {
                Ok(r) => relations.push(r),
                Err(e) => relations.push(RelationReport {
                    name: rel.name.clone(),
                    labels: Vec::new(),
                    refs: Vec::new(),
                    rows: Vec::new(),
                    gated: self.gates.relation_flag(&rel.name),
                    error: Some(e.to_string()),
                }),
            }
        }
        Report { relations }
    }

    /// Build the dashboard view of this relation, including the plan the next open-gate cycle would apply.
    fn report_relation(&self, rel: &Relation) -> Result<RelationReport> {
        let labels = rel.labels.iter().map(|l| LabelView {
            name: l.name.clone(),
            mapping: match &l.mapping {
                Mapping::Direct => "direct".into(),
                Mapping::Fixed(leaf) => format!("fixed({leaf})"),
            },
            shape: match &l.shape {
                Shape::Literal { root } => format!("root {}", root.display()),
                Shape::Many { roots } => format!("many, {} roots", roots.len()),
                Shape::Scan { template, hole, dir } => format!("scan {template} per {{{hole}}} under {}", dir.display()),
                Shape::Template(t) => format!("pattern {}", t.text),
            },
        }).collect();

        let expanded: Vec<Ref> = rel.labels.iter().map(|l| l.refs(&list_subdirs)).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect();
        let refs: Vec<RefView> = expanded.iter().map(|rf| RefView {
            label: rf.label.clone(),
            id: rf.id(),
            root: rf.root.clone(),
            root_exists: rf.root.is_dir(),
            pattern: match &rf.mapping_kind {
                MappingKind::Template(t) => Some(t.text.clone()),
                _ => None,
            },
        }).collect();

        let idents = self.identifiers(rel, &expanded)?;

        let mut rows = Vec::new();
        for ident in &idents {
            let mut cells = Vec::new();
            let mut slots = Vec::new();
            for rf in &expanded {
                let record = self.store.record(&rel.name, ident, &rf.id())?.flatten();
                let status = read_status(&rf.locate(ident), record.as_ref());
                let change = classify(record.as_ref(), &status);
                slots.push(Slot { ref_id: rf.id(), label: rf.label.clone(), record, status: status.clone() });
                cells.push(CellView { status, change });
            }
            // Same decision the next open-gate cycle will make.
            let all_recorded = slots.iter().all(|s| s.record.is_some());
            let settled = all_recorded && cells.iter().all(|c| c.change == Change::Unchanged);
            let (plan, note) = if settled {
                (Vec::new(), None)
            } else {
                match rel.policy.decide(&Instance { identifier: ident.clone(), slots }) {
                    Decision::Plan(ops) => (ops, None),
                    Decision::Skip(reason) if reason == "no changes" => (Vec::new(), None),
                    Decision::Skip(reason) | Decision::Pending(reason) | Decision::Abort(reason) => (Vec::new(), Some(reason)),
                    Decision::NoDecision => (Vec::new(), None),
                }
            };
            rows.push(RowView { identifier: identifier_to_string(ident), cells, plan, note });
        }

        Ok(RelationReport {
            name: rel.name.clone(),
            labels,
            refs,
            rows,
            gated: self.gates.relation_flag(&rel.name),
            error: None,
        })
    }
}

/// The dashboard builds the page from this. Each row is a file. Each cell is a location, in the same order as `refs`.
pub struct Report {
    pub relations: Vec<RelationReport>,
}

/// The dashboard shows these labels, locations, and file rows for a relation.
pub struct RelationReport {
    pub name: String,
    pub labels: Vec<LabelView>,
    pub refs: Vec<RefView>,
    pub rows: Vec<RowView>,
    /// This relation's own flag. The global gate lives on [`Gates`].
    pub gated: bool,
    pub error: Option<String>,
}

/// The location line prints this name, mapping, and shape.
pub struct LabelView {
    pub name: String,
    pub mapping: String,
    pub shape: String,
}

/// Column headers and plan text use this. It has the location's id, its root, and whether the folder exists.
pub struct RefView {
    pub label: String,
    pub id: String,
    pub root: PathBuf,
    pub root_exists: bool,
    /// The whole-path pattern, when this is a template ref.
    pub pattern: Option<String>,
}

/// A file's status at each location, plus the plan for the next cycle (or the reason we have no plan).
pub struct RowView {
    pub identifier: String,
    pub cells: Vec<CellView>,
    /// The plan the next open-gate cycle would apply. `note` is set instead when the decision is a skip or a conflict.
    pub plan: Vec<Op>,
    pub note: Option<String>,
}

/// Status of a file at a location, and whether it matches the record.
pub struct CellView {
    pub status: Status,
    pub change: Change,
}

impl std::fmt::Display for RelationAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "abort: {}", self.0)
    }
}
impl std::fmt::Debug for RelationAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
impl std::error::Error for RelationAbort {}

/// Immediate subdirectories of `dir`, sorted. Scan uses this when expanding roots.
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

/// Stat and hash this path.
/// If mtime and size still match the record, we reuse the recorded hash (Unison's fastcheck).
/// A tool that changes bytes without touching mtime or size will slip through.
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
        if r.mtime == mtime && r.size == size {
            return Status::Present(Snapshot { mtime, size, hash: r.hash });
        }
    }
    match fs::read(path) {
        Ok(bytes) => Status::Present(Snapshot { mtime, size, hash: *blake3::hash(&bytes).as_bytes() }),
        Err(e) => Status::Unreadable(e.to_string()),
    }
}

/// Write to a temp file in the same directory, then rename over `path`.
/// The return value is the mtime after the rename.
fn write_atomic(path: &Path, bytes: &[u8], mtime: Option<std::time::SystemTime>) -> Result<std::time::SystemTime> {
    let parent = path.parent().ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}{TEMP_SUFFIX}", path.file_name().unwrap_or_default().to_string_lossy()));
    fs::write(&tmp, bytes)?;
    if let Some(t) = mtime {
        filetime::set_file_mtime(&tmp, filetime::FileTime::from_system_time(t))?;
    }
    fs::rename(&tmp, path)?;
    Ok(fs::symlink_metadata(path)?.modified()?)
}

const TEMP_SUFFIX: &str = ".replica-write";
const MARKER_SUFFIX: &str = ".replica.tmp";

/// Write a small text file next to the create or delete, and remove it after commit.
/// Plain text on purpose: the reader is someone wondering what happened to their file.
fn write_marker(target: &Path, relation: &str, ident: &Identifier, ref_id: &str, op: &str) -> Result<PathBuf> {
    let marker = PathBuf::from(format!("{}{MARKER_SUFFIX}", target.display()));
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
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
    ))?;
    Ok(marker)
}

/// Relative paths of every regular file under `root`.
/// A missing root counts as empty (the ref has nothing yet).
/// We skip symlinks, leftover `.replica-write` files, and leftover `.replica.tmp` files.
fn walk_identifiers(root: &Path) -> Result<Vec<Identifier>> {
    let mut idents = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound && rel.as_os_str().is_empty() => return Ok(idents),
            Err(e) => return Err(e).with_context(|| format!("walking {}", dir.display())),
        };
        for entry in entries {
            let entry = entry?;
            let ty = entry.file_type()?;
            let child = rel.join(entry.file_name());
            if ty.is_dir() {
                stack.push(child);
            } else if ty.is_file() && {
                let name = entry.file_name().to_string_lossy().into_owned();
                !name.ends_with(TEMP_SUFFIX) && !name.ends_with(MARKER_SUFFIX)
            } {
                idents.push(child.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect());
            }
        }
    }
    Ok(idents)
}

/// Fail if two refs share a root or nest inside each other. Otherwise a relation can write into another's files.
fn validate_no_overlaps(relations: &[Relation], expanded: &[(usize, Vec<Ref>)]) -> Result<()> {
    let mut roots: Vec<(&str, &Ref)> = Vec::new();
    for (i, refs) in expanded {
        for rf in refs {
            roots.push((&relations[*i].name, rf));
        }
    }
    for (i, (ra, a)) in roots.iter().enumerate() {
        for (rb, b) in roots.iter().skip(i + 1) {
            if a.root.starts_with(&b.root) || b.root.starts_with(&a.root) {
                bail!(
                    "overlapping roots: {} ({ra}/{}) and {} ({rb}/{})",
                    a.root.display(), a.id(), b.root.display(), b.id()
                );
            }
        }
    }
    Ok(())
}
