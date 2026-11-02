//! The executor: turn an assignment for one key into filesystem changes and committed records.
//!
//! Stages run in order, each able to fail before the next starts: stage the content the plan needs into the blob store, archive what will be overwritten or deleted, journal the intent and place markers, write and delete through the medium, commit the records in one transaction.
//! A failure removes the markers and the intent row and leaves the records alone, so the next cycle plans again from the last consistent basis.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result, bail};

use crate::action::{Shell, transform};
use crate::blobs::Blobs;
use crate::decide::{Assignment, Op, Situation, Slot, describe_ops};
use crate::medium::{Medium, marker_path};
use crate::observe::{Basis, Hash, Observation, Snapshot, hex};
use crate::site::{Key, Replica};
use crate::store::Store;

/// Everything the executor needs about the relation and key it applies to.
pub struct Context<'a> {
    pub relation: &'a str,
    pub key: &'a Key,
    pub replicas: &'a [Replica],
    pub situation: &'a Situation,
    /// Copy the proposing replica's modification time onto written files.
    pub sync_times: bool,
    /// Commands rewriting content on the way from one site to another, keyed by (from site, to site).
    pub transforms: &'a BTreeMap<(String, String), Shell>,
    /// Working directory for transform commands.
    pub workdir: &'a Path,
}

/// What a plan did.
#[derive(Debug, Default)]
pub struct Applied {
    pub ops: Vec<Op>,
    /// Paths written or deleted, for actions.
    pub paths: Vec<PathBuf>,
    /// Non-fatal problems, such as a permission bit that could not be set.
    pub warnings: Vec<String>,
}

/// The executor over one store, blob store, medium.
pub struct Executor<'a> {
    pub store: &'a Store,
    pub blobs: &'a Blobs,
    pub medium: &'a dyn Medium,
}

/// Bytes a write takes: the blob hash plus the mode and mtime of the replica the content came from.
struct Source {
    hash: Hash,
    size: u64,
    mtime: SystemTime,
    mode: Option<u32>,
}

/// Markers placed for the duration of a plan, removed on success and on failure alike.
struct Markers<'a> {
    medium: &'a dyn Medium,
    paths: Vec<PathBuf>,
}

impl Drop for Markers<'_> {
    fn drop(&mut self) {
        for marker in &self.paths {
            let _ = self.medium.remove(marker);
        }
    }
}

impl Executor<'_> {
    /// Apply an assignment. Operations are derived from the assignment against the situation's observations.
    pub fn apply(&self, ctx: &Context, assignment: &Assignment) -> Result<Applied> {
        let ops = assignment.ops(ctx.situation);
        let replicas: BTreeMap<&str, &Replica> = ctx.replicas.iter().map(|r| (r.id(), r)).collect();
        let slots: BTreeMap<&str, &Slot> = ctx.situation.slots.iter().map(|s| (s.id.as_str(), s)).collect();
        let replica = |id: &str| replicas.get(id).copied().with_context(|| format!("plan refers to unknown replica {id:?}"));
        let slot = |id: &str| slots.get(id).copied().with_context(|| format!("plan refers to unknown replica {id:?}"));

        // Stage 1: bring the content of each write into the blob store, from a replica holding it or from the store itself, applying transforms along the way.
        let mut staged: BTreeSet<Hash> = BTreeSet::new();
        let mut sources: BTreeMap<String, Source> = BTreeMap::new();
        for op in &ops {
            let Op::Write { to, hash, from } = op else { continue };
            let donor = slot(from)?;
            let donor_path = replica(from)?.locate(ctx.key)?;
            let mtime = donor.observation.snapshot().map(|s| s.mtime).with_context(|| format!("replica {from:?} proposed content it does not hold"))?;
            let mode = self.medium.stat(&donor_path).ok().flatten().map(|m| m.mode);
            let size = self.stage_hash(hash, from, &donor_path, &slots, &replicas, ctx.key, &mut staged)?;
            let edge = (donor.site.clone(), slot(to)?.site.clone());
            let source = match ctx.transforms.get(&edge) {
                Some(shell) => self.transform(shell, hash, ctx.workdir, &mut staged)?,
                None => Source { hash: *hash, size, mtime, mode },
            };
            sources.insert(to.clone(), Source { mtime, mode, ..source });
        }

        // Stage 2: archive each target about to lose content.
        for op in &ops {
            let (id, reason) = match op {
                Op::Write { to, .. } => (to, "overwritten"),
                Op::Delete { at } => (at, "deleted"),
            };
            let target = slot(id)?;
            let Some(snap) = target.observation.snapshot() else { continue };
            if let Some(src) = sources.get(id.as_str()) {
                if src.hash == snap.hash {
                    continue;
                }
            }
            let path = replica(id)?.locate(ctx.key)?;
            self.archive(ctx.relation, ctx.key, id, &path, snap, reason)?;
        }

        // Stage 3: journal the intent, then mark each file about to be created or deleted.
        if !ops.is_empty() {
            self.store.write_intent(ctx.relation, ctx.key, &describe_ops(&ops)).context("writing intent row")?;
        }
        let mut markers = Markers { medium: self.medium, paths: Vec::new() };
        for op in &ops {
            let (word, id) = match op {
                Op::Write { to, .. } if slot(to)?.observation == Observation::Missing => ("create", to),
                Op::Write { .. } => continue,
                Op::Delete { at } => ("delete", at),
            };
            let target = replica(id)?.locate(ctx.key)?;
            if let Err(e) = self.place_marker(&target, ctx.relation, ctx.key, id, word) {
                let _ = self.store.clear_intent(ctx.relation, ctx.key);
                return Err(e);
            }
            markers.paths.push(marker_path(&target));
        }

        // Stage 4: write and delete. Outcomes start from the observations so untouched replicas commit what was seen.
        let mut outcomes: BTreeMap<&str, Basis> = ctx.situation.slots.iter().map(|s| (s.id.as_str(), outcome_of(s))).collect();
        let mut applied = Applied::default();
        let executed: Result<()> = (|| {
            for op in &ops {
                match op {
                    Op::Write { to, .. } => {
                        let target = slot(to)?;
                        let path = replica(to)?.locate(ctx.key)?;
                        let src = &sources[to.as_str()];
                        // A new file takes the source's permission bits; an overwrite keeps the target's own, as cp does.
                        let mode = match &target.observation {
                            Observation::Missing => src.mode,
                            _ => self.medium.stat(&path).ok().flatten().map(|m| m.mode).or(src.mode),
                        };
                        let mut reader = self.blobs.open(&src.hash)?;
                        let written = self.medium.write(&path, &mut reader, ctx.sync_times.then_some(src.mtime), mode)?;
                        applied.warnings.extend(written.warning);
                        outcomes.insert(to.as_str(), Basis::Snapshot(Snapshot { mtime: written.mtime, size: src.size, hash: src.hash }));
                        applied.paths.push(path);
                    }
                    Op::Delete { at } => {
                        let path = replica(at)?.locate(ctx.key)?;
                        self.medium.remove(&path).with_context(|| format!("deleting {}", path.display()))?;
                        outcomes.insert(at.as_str(), Basis::Tombstone);
                        applied.paths.push(path);
                    }
                }
            }
            Ok(())
        })();
        if let Err(e) = executed {
            let _ = self.store.clear_intent(ctx.relation, ctx.key);
            drop(markers);
            self.sweep(&staged);
            return Err(e);
        }

        // Stage 5: commit records and unresolved marks together, then drop the intent row with them.
        let held: BTreeMap<&str, &str> = assignment.held.iter().map(|(id, reason)| (id.as_str(), reason.as_str())).collect();
        let committed = self.store.transaction(|store| {
            for (id, basis) in &outcomes {
                match held.get(id) {
                    Some(reason) => store.mark_unresolved(ctx.relation, ctx.key, id, reason)?,
                    None => {
                        store.commit_record(ctx.relation, ctx.key, id, basis)?;
                        store.clear_unresolved(ctx.relation, ctx.key, id)?;
                    }
                }
            }
            store.clear_intent(ctx.relation, ctx.key)
        }).context("committing records");
        drop(markers);
        self.sweep(&staged);
        if let Err(e) = committed {
            // The files are written and the error is reported with its cause, so the journal row must not read as a crash at the next start.
            let _ = self.store.clear_intent(ctx.relation, ctx.key);
            return Err(e);
        }
        applied.ops = ops;
        Ok(applied)
    }

    /// Make sure a hash is in the blob store: read it from the proposing replica, else from any replica holding it, else trust the store.
    fn stage_hash(&self, hash: &Hash, from: &str, donor_path: &Path, slots: &BTreeMap<&str, &Slot>, replicas: &BTreeMap<&str, &Replica>, key: &Key, staged: &mut BTreeSet<Hash>) -> Result<u64> {
        if staged.contains(hash) || self.blobs.has(hash) {
            return Ok(self.medium.stat(donor_path).ok().flatten().map(|m| m.size).unwrap_or_else(|| slots.get(from).and_then(|s| s.observation.snapshot()).map(|s| s.size).unwrap_or(0)));
        }
        let mut holders: Vec<(&str, PathBuf)> = vec![(from, donor_path.to_path_buf())];
        for (id, slot) in slots {
            if *id != from && slot.observation.hash() == Some(*hash) {
                if let Some(r) = replicas.get(id) {
                    holders.push((id, r.locate(key)?));
                }
            }
        }
        let mut last_error = None;
        for (id, path) in holders {
            let mut reader = match self.medium.open(&path) {
                Ok(r) => r,
                Err(e) => { last_error = Some(anyhow::anyhow!("reading {} at replica {id}: {e}", path.display())); continue; }
            };
            let (got, size) = self.blobs.stage(&mut reader).with_context(|| format!("staging {}", path.display()))?;
            if got == *hash {
                staged.insert(got);
                return Ok(size);
            }
            // The file changed between observation and read; another holder may still carry the agreed content.
            staged.insert(got);
            last_error = Some(anyhow::anyhow!("{} changed while the plan was being applied (expected {}, read {})", path.display(), hex(hash), hex(&got)));
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no replica holds content {}", hex(hash))))
    }

    /// Run a transform over a staged blob, staging its output, and describe the result as the write's source.
    fn transform(&self, shell: &Shell, input: &Hash, workdir: &Path, staged: &mut BTreeSet<Hash>) -> Result<Source> {
        let output = self.blobs.dir.join(format!(".transform-{}-{}", std::process::id(), hex(input)));
        let result = transform(shell, &self.blobs.path(input), &output, workdir).and_then(|()| {
            let mut f = std::fs::File::open(&output)?;
            self.blobs.stage(&mut f)
        });
        let _ = std::fs::remove_file(&output);
        let (hash, size) = result?;
        staged.insert(hash);
        Ok(Source { hash, size, mtime: SystemTime::now(), mode: None })
    }

    /// Save a file about to be overwritten or deleted into the blob store with an archive row, then evict past the cap.
    fn archive(&self, relation: &str, key: &Key, id: &str, path: &Path, snap: &Snapshot, reason: &str) -> Result<()> {
        let mut reader = match self.medium.open(path) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {} for the archive", path.display())),
        };
        let (hash, size) = self.blobs.stage(&mut reader).with_context(|| format!("archiving {} before it is {reason}", path.display()))?;
        if hash != snap.hash {
            bail!("{} changed while the plan was being applied (expected {}, read {})", path.display(), hex(&snap.hash), hex(&hash));
        }
        let row = self.store.archive_insert(relation, key, id, path, &hash, size, reason)?;
        self.blobs.evict(self.store, row)
    }

    /// Write the marker beside a target about to be created or deleted.
    fn place_marker(&self, target: &Path, relation: &str, key: &Key, id: &str, op: &str) -> Result<()> {
        let started = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let text = format!(
            "service:  replica\npid:      {}\nrelation: {relation}\nkey:      {key}\nreplica:  {id}\nop:       {op}\nstarted:  {started}\nnote:     this marker is removed when the operation commits. if you are reading it, the operation is still running or died partway.\n",
            std::process::id(),
        );
        self.medium.put(&marker_path(target), text.as_bytes()).with_context(|| format!("writing marker beside {}", target.display()))
    }

    /// Remove staged blobs no archive row refers to.
    fn sweep(&self, staged: &BTreeSet<Hash>) {
        for hash in staged {
            let _ = self.blobs.sweep(self.store, hash);
        }
    }
}

/// The basis a replica commits when no operation touches it: what was observed, a tombstone where a recorded snapshot is gone, no record where there never was one.
fn outcome_of(slot: &Slot) -> Basis {
    match (&slot.basis, &slot.observation) {
        (_, Observation::Present(s)) => Basis::Snapshot(s.clone()),
        (Basis::Snapshot(_), Observation::Missing) => Basis::Tombstone,
        (basis, _) => basis.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::Desired;
    use crate::flow::Policy;
    use crate::medium::Local;
    use crate::pattern::{Binding, Pattern};
    use std::fs;
    use tempfile::TempDir;

    /// Observe a path for a test: hash present files, mark missing ones.
    fn observe(path: &Path) -> Observation {
        match fs::read(path) {
            Ok(bytes) => Observation::Present(Snapshot { mtime: fs::metadata(path).unwrap().modified().unwrap(), size: bytes.len() as u64, hash: *blake3::hash(&bytes).as_bytes() }),
            Err(_) => Observation::Missing,
        }
    }

    fn setup(tmp: &TempDir) -> (Store, Blobs, Vec<Replica>) {
        let store = Store::open(&tmp.path().join("state.db")).unwrap();
        let blobs = Blobs { dir: tmp.path().join("blobs"), cap: 1 << 20 };
        let replicas = ["a", "b", "c"].iter().map(|s| Replica::new(s, None, Binding::new(), Pattern::parse(&format!("{}/{s}/", tmp.path().display())).unwrap())).collect();
        (store, blobs, replicas)
    }

    fn situation(replicas: &[Replica], key: &Key, store: &Store) -> Situation {
        let records = store.records_for("r").unwrap();
        Situation { slots: replicas.iter().map(|r| {
            let path = r.locate(key).unwrap();
            let basis = records.get(&key.canonical()).and_then(|m| m.get(r.id())).cloned().unwrap_or(Basis::None);
            Slot { id: r.id().to_owned(), site: r.site.clone(), path: path.clone(), basis, observation: observe(&path) }
        }).collect() }
    }

    #[test]
    fn a_plan_can_overwrite_a_replica_whose_old_content_another_replica_receives() {
        let tmp = TempDir::new().unwrap();
        let (store, blobs, replicas) = setup(&tmp);
        let key = Key::parse("path=f.txt");
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::create_dir_all(tmp.path().join("b")).unwrap();
        fs::create_dir_all(tmp.path().join("c")).unwrap();
        fs::write(tmp.path().join("a/f.txt"), "from a").unwrap();
        fs::write(tmp.path().join("b/f.txt"), "from b").unwrap();
        fs::write(tmp.path().join("c/f.txt"), "old").unwrap();
        let sit = situation(&replicas, &key, &store);
        // b takes a's content while c takes b's old content: the plan names hashes, so the order of writes does not matter.
        let policy = Policy::parse("a -> b [edit]; {a, b} -> c [edit]", "b > a").unwrap();
        let assignment = crate::decide::decide(&policy, &sit);
        let exec = Executor { store: &store, blobs: &blobs, medium: &Local };
        let ctx = Context { relation: "r", key: &key, replicas: &replicas, situation: &sit, sync_times: true, transforms: &BTreeMap::new(), workdir: tmp.path() };
        let applied = exec.apply(&ctx, &assignment).unwrap();
        assert_eq!(applied.ops.len(), 2);
        assert_eq!(fs::read_to_string(tmp.path().join("b/f.txt")).unwrap(), "from a");
        assert_eq!(fs::read_to_string(tmp.path().join("c/f.txt")).unwrap(), "from b");
        // Overwritten content is archived; records hold the new hashes; no intent or marker remains.
        let archived = store.archive_entries("r").unwrap();
        assert_eq!(archived.len(), 2);
        assert!(store.stale_intents().unwrap().is_empty());
        let records = store.records_for("r").unwrap();
        assert_eq!(records[&key.canonical()]["c"].snapshot().unwrap().hash, *blake3::hash(b"from b").as_bytes());
        assert!(fs::read_dir(tmp.path().join("b")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("replica")));
    }

    #[test]
    fn deletes_leave_tombstones_and_missing_untouched_replicas_leave_no_record() {
        let tmp = TempDir::new().unwrap();
        let (store, blobs, replicas) = setup(&tmp);
        let key = Key::parse("path=f.txt");
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::create_dir_all(tmp.path().join("b")).unwrap();
        fs::write(tmp.path().join("b/f.txt"), "doomed").unwrap();
        store.commit_record("r", &key, "a", &Basis::Snapshot(Snapshot { mtime: SystemTime::UNIX_EPOCH, size: 6, hash: [1; 32] })).unwrap();
        let sit = situation(&replicas, &key, &store);
        let mut assignment = Assignment::default();
        assignment.desired.insert("a".into(), Desired::Keep);
        assignment.desired.insert("b".into(), Desired::Absent);
        assignment.desired.insert("c".into(), Desired::Keep);
        let exec = Executor { store: &store, blobs: &blobs, medium: &Local };
        let ctx = Context { relation: "r", key: &key, replicas: &replicas, situation: &sit, sync_times: true, transforms: &BTreeMap::new(), workdir: tmp.path() };
        exec.apply(&ctx, &assignment).unwrap();
        assert!(!tmp.path().join("b/f.txt").exists());
        let records = store.records_for("r").unwrap();
        let row = &records[&key.canonical()];
        assert_eq!(row["a"], Basis::Tombstone, "a recorded snapshot found missing becomes a tombstone");
        assert_eq!(row["b"], Basis::Tombstone, "a deleted file becomes a tombstone");
        assert!(!row.contains_key("c"), "a replica that never had the file keeps no record");
        assert_eq!(store.archive_entries("r").unwrap()[0].reason, "deleted");
    }

    #[test]
    fn a_transform_rewrites_content_on_the_way() {
        let tmp = TempDir::new().unwrap();
        let (store, blobs, replicas) = setup(&tmp);
        let key = Key::parse("path=f.txt");
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::write(tmp.path().join("a/f.txt"), "shout").unwrap();
        let sit = situation(&replicas, &key, &store);
        let mut assignment = Assignment::default();
        assignment.desired.insert("b".into(), Desired::Content { from: "a".into(), hash: *blake3::hash(b"shout").as_bytes() });
        let transforms: BTreeMap<(String, String), Shell> = [(("a".to_string(), "b".to_string()), Shell { text: "tr a-z A-Z < ${in} > ${out}".into(), timeout: crate::action::DEFAULT_TIMEOUT })].into_iter().collect();
        let exec = Executor { store: &store, blobs: &blobs, medium: &Local };
        let ctx = Context { relation: "r", key: &key, replicas: &replicas, situation: &sit, sync_times: false, transforms: &transforms, workdir: tmp.path() };
        exec.apply(&ctx, &assignment).unwrap();
        assert_eq!(fs::read_to_string(tmp.path().join("b/f.txt")).unwrap(), "SHOUT");
        let records = store.records_for("r").unwrap();
        assert_eq!(records[&key.canonical()]["b"].snapshot().unwrap().hash, *blake3::hash(b"SHOUT").as_bytes(), "the receiver records the output hash");
    }

    #[test]
    fn a_failed_write_leaves_records_intent_and_markers_clean() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let (store, blobs, replicas) = setup(&tmp);
        let key = Key::parse("path=f.txt");
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::create_dir_all(tmp.path().join("b")).unwrap();
        fs::write(tmp.path().join("a/f.txt"), "v2").unwrap();
        fs::write(tmp.path().join("b/f.txt"), "v1").unwrap();
        let sit = situation(&replicas, &key, &store);
        let mut assignment = Assignment::default();
        assignment.desired.insert("b".into(), Desired::Content { from: "a".into(), hash: *blake3::hash(b"v2").as_bytes() });
        fs::set_permissions(tmp.path().join("b"), fs::Permissions::from_mode(0o500)).unwrap();
        let exec = Executor { store: &store, blobs: &blobs, medium: &Local };
        let ctx = Context { relation: "r", key: &key, replicas: &replicas, situation: &sit, sync_times: true, transforms: &BTreeMap::new(), workdir: tmp.path() };
        let err = exec.apply(&ctx, &assignment).unwrap_err().to_string();
        fs::set_permissions(tmp.path().join("b"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.contains("creating temp file"), "{err}");
        assert!(store.stale_intents().unwrap().is_empty());
        assert!(store.records_for("r").unwrap().is_empty());
        assert_eq!(fs::read_to_string(tmp.path().join("b/f.txt")).unwrap(), "v1");
    }
}
