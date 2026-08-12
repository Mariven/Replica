use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context as AnyhowContext, Result, bail};

use crate::identifier::{Identifier, Pattern, identifier_to_string};
use crate::policy::{Decision, Instance, Op, Policy, Slot};
use crate::label::{Label, Mapping, MappingKind, Ref, Shape};
use crate::status::{Change, Snapshot, Status, classify};
use crate::store::Store;

pub struct Relation {
    pub name: String,
    pub labels: Vec<Label>,
    pub pattern: Pattern,
    pub policy: Policy,
    pub sync_times: bool,
}

// What happened to one instance during a cycle, for hooks and logs.
#[derive(Debug)]
pub enum Event {
    Resolved { relation: String, identifier: String, ops: usize },
    Skipped { relation: String, identifier: String, reason: String },
    Pending { relation: String, identifier: String, reason: String },
    Errored { relation: String, identifier: String, error: String },
    StaleIntent { relation: String, identifier: String, detail: String },
}

pub type Hook = Box<dyn Fn(&Event) + Send>;

pub struct Engine {
    pub store: Store,
    pub relations: Vec<Relation>,
    pub hooks: Vec<Hook>,
}

// A relation aborts its cycle on Decision::Abort; other relations continue.
struct RelationAbort(String);

impl Engine {
    pub fn new(store: Store, relations: Vec<Relation>, hooks: Vec<Hook>) -> Engine {
        Engine { store, relations, hooks }
    }

    fn emit(&self, event: Event) {
        for hook in &self.hooks {
            hook(&event);
        }
    }

    // Surviving intents mean a previous run died mid-operation. Stale plans are not replayed; the affected instances are simply resolved fresh this cycle, which repairs any half-applied state.
    pub fn recover(&self) -> Result<()> {
        for (relation, ident, detail) in self.store.stale_intents()? {
            self.emit(Event::StaleIntent { relation: relation.clone(), identifier: identifier_to_string(&ident), detail });
            self.store.clear_intent(&relation, &ident)?;
        }
        Ok(())
    }

    // One full pass: expand labels to refs, validate, discover identifiers, resolve each instance.
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

    // Identifiers come from asking each ref to identify the files under its root
    // (Fixed cannot; Template only claims paths matching its pattern), plus every
    // identifier on record (so a file deleted everywhere visible is still noticed
    // and its rows collected).
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

    fn resolve_relation(&self, rel: &Relation, refs: &[Ref]) -> Result<()> {
        // Empty-side guard: a root that has committed records but no directory reads as
        // everything-deleted at once. That is far more likely an unmounted volume or a
        // moved folder than a real mass deletion, so the relation waits instead of
        // propagating deletions (or writing into a dead mount point).
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

    fn resolve(&self, rel: &Relation, refs: &[Ref], ident: &Identifier) -> Result<()> {
        // Read every slot's status against its record. A row can be recorded-as-absent, which differs from nothing-on-record: only the latter blocks the early exit below.
        let mut slots = Vec::new();
        let mut all_recorded = true;
        for rf in refs {
            let row = self.store.record(&rel.name, ident, &rf.id())?;
            all_recorded &= row.is_some();
            let record = row.flatten();
            let status = read_status(&rf.locate(ident), record.as_ref());
            slots.push((rf, Slot { ref_id: rf.id(), label: rf.label.clone(), record, status }));
        }

        // An identifier absent everywhere is inert; drop its rows and move on.
        if slots.iter().all(|(_, s)| s.status == Status::Missing) {
            self.store.forget(&rel.name, ident)?;
            return Ok(());
        }

        // Self-write suppression and the common no-op: everything agrees with the record.
        // A slot with nothing on record (e.g. a freshly discovered scan root) still resolves, since lifecycle policy may want to populate it.
        if all_recorded && slots.iter().all(|(_, s)| s.change() == Change::Unchanged) {
            return Ok(());
        }

        let instance = Instance { identifier: ident.clone(), slots: slots.iter().map(|(_, s)| s.clone()).collect() };
        let ident_str = identifier_to_string(ident);
        match rel.policy.decide(&instance) {
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

    // Execute a validated plan: journal intent, mutate through same-dir temp files and atomic renames, put the outcome on record, clear intent. A crash leaves the intent row as evidence.
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

        // Creates and deletes leave a marker file beside the change before it happens,
        // removed after commit: a marker still present later means the operation did not
        // finish. Only creates and deletes — they are rare and hard to reverse; marking
        // every edit would just teach the reader to ignore markers.
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

        // Track what each rf ends up as, starting from its observed status.
        let mut outcomes: Vec<(String, Option<Snapshot>)> = slots.iter().map(|(rf, s)| {
            let snap = match &s.status {
                Status::Present(snap) => Some(snap.clone()),
                _ => None,
            };
            (rf.id(), snap)
        }).collect();

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
                    let mtime = rel.sync_times.then_some(donor_snap.mtime);
                    let written = write_atomic(&dest_path, &bytes, mtime)?;
                    let snap = Snapshot { mtime: written, size: bytes.len() as u64, hash: donor_snap.hash };
                    if let Some(o) = outcomes.iter_mut().find(|(id, _)| id == to) {
                        o.1 = Some(snap);
                    }
                }
                Op::Delete { at } => {
                    let rf = find(at)?;
                    match fs::remove_file(rf.locate(ident)) {
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

    // A read-only pass for the dashboard: every relation's labels, their current refs, and
    // each identifier's status and change at each ref. Reads the filesystem and the record,
    // mutates nothing. Unchanged files cost a stat, not a hash, via the record fastcheck.
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
                    error: Some(e.to_string()),
                }),
            }
        }
        Report { relations }
    }

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
            for rf in &expanded {
                let record = self.store.record(&rel.name, ident, &rf.id())?.flatten();
                let status = read_status(&rf.locate(ident), record.as_ref());
                let change = classify(record.as_ref(), &status);
                cells.push(CellView { status, change });
            }
            rows.push(RowView { identifier: identifier_to_string(ident), cells });
        }

        Ok(RelationReport { name: rel.name.clone(), labels, refs, rows, error: None })
    }
}

// The dashboard's view of the world; plain typed data, rendered elsewhere.
// Rows are identifier-major: one row per identifier, one cell per ref, cells in `refs` order.
pub struct Report {
    pub relations: Vec<RelationReport>,
}

pub struct RelationReport {
    pub name: String,
    pub labels: Vec<LabelView>,
    pub refs: Vec<RefView>,
    pub rows: Vec<RowView>,
    pub error: Option<String>,
}

pub struct LabelView {
    pub name: String,
    pub mapping: String,
    pub shape: String,
}

pub struct RefView {
    pub label: String,
    pub id: String,
    pub root: PathBuf,
    pub root_exists: bool,
    // For template refs, the whole-path pattern this ref matches and writes.
    pub pattern: Option<String>,
}

pub struct RowView {
    pub identifier: String,
    pub cells: Vec<CellView>,
}

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

// The directory listing that Scan shapes expand through: immediate subdirectories, sorted.
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

// Stat and hash one path. The mtime+size fastcheck reuses the recorded hash when both match; a tool that edits bytes while preserving both slips past this, same trade as Unison's fastcheck.
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

// Write via a temp file in the destination directory, then rename into place.
// Returns the resulting mtime.
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

// A plain-text breadcrumb at the change site, written before a create or delete and
// removed after the record commits. Plain text, not JSON: the likely reader is a person
// wondering what touched their file, not a parser.
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
         note:       this marker is removed when the operation commits; if you are\n\
         \x20           reading it, the operation is in progress or died partway.\n",
        std::process::id(),
        identifier_to_string(ident),
    ))?;
    Ok(marker)
}

// Identifiers of all regular files under a root. A missing root yields no identifiers (the rf simply has nothing yet). Symlinks are skipped: out of the v0 node model. Leftover temp files from a crashed write are never identifiers.
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

// Overlapping real roots across refs make ownership ambiguous and can feed one relation's writes into another; rejected outright in v0.
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
