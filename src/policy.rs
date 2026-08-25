//! Decide what to copy or delete, using only the slots we already read.

use crate::identifier::Identifier;
use crate::status::{Change, Snapshot, Status, classify};

/// The last-sync record and current status of a file at a given location.
#[derive(Clone, Debug)]
pub struct Slot {
    pub ref_id: String,
    pub label: String,
    pub record: Option<Snapshot>,
    pub status: Status,
}

impl Slot {
    /// Compare the live status to the record.
    pub fn change(&self) -> Change {
        classify(self.record.as_ref(), &self.status)
    }

    /// The live snapshot, if the file is there.
    fn snapshot(&self) -> Option<&Snapshot> {
        match &self.status {
            Status::Present(s) => Some(s),
            _ => None,
        }
    }
}

/// The slots for a file, after we've read every location.
#[derive(Clone, Debug)]
pub struct Instance {
    pub identifier: Identifier,
    pub slots: Vec<Slot>,
}

/// Copy bytes from a ref onto another, or delete a file. The policy only names the refs. The engine reads the bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Copy { from: String, to: String },
    Delete { at: String },
}

/// After looking at a file, `decide` returns a plan, a skip, a wait, or an abort.
/// A `Plan` (empty or not) means we update the record to match what we just saw.
/// `Skip` and `Pending` keep the record as it is, so the next cycle sees the same situation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Plan(Vec<Op>),
    NoDecision,
    Skip(String),
    Pending(String),
    Abort(String),
}

/// Where we take edited content from.
#[derive(Clone, Debug)]
pub enum ContentPolicy {
    /// Copy from this label's ref.
    Mirror(String),
    Bidirectional,
}

/// Two sides changed in different ways. We can take the newer edit, prefer a named label, or skip.
#[derive(Clone, Debug)]
pub enum ConflictPolicy {
    Newer,
    /// Take this label's version.
    Prefer(String),
    Skip,
}

/// We can create the file at a location that has never had it, or only sync where it already exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewPolicy {
    /// Create the file at refs that have never had it.
    Replicate,
    /// Sync only where the file already exists.
    Restrict,
}

/// Put a deleted file back, spread the deletion, or skip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeletePolicy {
    Propagate,
    Restore,
    Skip,
}

/// A relation's rules for edits, conflicts, new files, and deletions.
#[derive(Clone, Debug)]
pub struct Policy {
    pub content: ContentPolicy,
    pub conflict: ConflictPolicy,
    pub on_new: NewPolicy,
    pub on_delete: DeletePolicy,
}

impl Default for Policy {
    /// The defaults are bidirectional edits, skip conflicts, restrict new files, and restore deletions.
    fn default() -> Self {
        Policy {
            content: ContentPolicy::Bidirectional,
            conflict: ConflictPolicy::Skip,
            on_new: NewPolicy::Restrict,
            on_delete: DeletePolicy::Restore,
        }
    }
}

/// The content we want at every location: a copy from this donor, or deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    /// Copy from the ref with this id.
    Content(String),
    Absent,
}

impl Policy {
    /// Decide what to do with this file, using only the slots already on the instance.
    pub fn decide(&self, inst: &Instance) -> Decision {
        for s in &inst.slots {
            if let Status::Unreadable(e) = &s.status {
                return Decision::Skip(format!("{} unreadable: {e}", s.ref_id));
            }
        }

        let target = match self.target(inst) {
            Ok(t) => t,
            Err(d) => return d,
        };

        // Restore: put the deleted file back from any slot that is still present and unchanged.
        let target = if target == Target::Absent {
            match self.on_delete {
                DeletePolicy::Propagate => Target::Absent,
                DeletePolicy::Skip => return Decision::Skip("deletion observed, on_delete=skip".into()),
                DeletePolicy::Restore => {
                    match inst.slots.iter().find(|s| s.snapshot().is_some() && s.change() == Change::Unchanged) {
                        Some(donor) => Target::Content(donor.ref_id.clone()),
                        None => return Decision::Skip("deletion observed, no surviving copy to restore from".into()),
                    }
                }
            }
        } else {
            target
        };

        Decision::Plan(self.ops(inst, &target))
    }

    /// Decide what content every location should have. On conflict this returns `Err` with a finished decision (usually skip).
    fn target(&self, inst: &Instance) -> Result<Target, Decision> {
        match &self.content {
            ContentPolicy::Mirror(label) => {
                let auth = inst.slots.iter().find(|s| &s.label == label)
                    .ok_or_else(|| Decision::Abort(format!("mirror label {label:?} has no ref")))?;
                match (&auth.status, auth.change()) {
                    (Status::Present(_), _) => Ok(Target::Content(auth.ref_id.clone())),
                    (Status::Missing, Change::Disappeared) => Ok(Target::Absent),
                    // The mirror source has no file to copy. Leave the destinations alone.
                    (Status::Missing, _) => Err(Decision::Skip("mirror source absent".into())),
                    (Status::Unreadable(_), _) => unreachable!("filtered above"),
                }
            }
            ContentPolicy::Bidirectional => {
                let established = inst.slots.iter().any(|s| s.record.is_some());
                // A slot proposes if it changed against its own record.
                // A file that appears where peers already have a record is a late joiner: it takes their content instead of proposing its own. That is why touching an empty dest is safe.
                let proposals: Vec<&Slot> = inst.slots.iter().filter(|s| match s.change() {
                    Change::Edited | Change::Disappeared => true,
                    Change::Appeared => !established,
                    _ => false,
                }).collect();

                // Same content on two sides is one proposal (they edited to the same thing). Deletions key on None.
                let mut seen: Vec<Option<[u8; 32]>> = Vec::new();
                let mut targets: Vec<Target> = Vec::new();
                for s in &proposals {
                    let hash = s.snapshot().map(|snap| snap.hash);
                    if !seen.contains(&hash) {
                        seen.push(hash);
                        targets.push(match s.snapshot() {
                            Some(_) => Target::Content(s.ref_id.clone()),
                            None => Target::Absent,
                        });
                    }
                }

                match targets.len() {
                    0 => {
                        // Nobody proposed. If a late joiner appeared, copy from any slot that still matches its record.
                        let donor = inst.slots.iter().find(|s| s.record.is_some() && s.change() == Change::Unchanged && s.snapshot().is_some());
                        let joiner = inst.slots.iter().any(|s| s.change() == Change::Appeared);
                        match (donor, joiner) {
                            (Some(d), true) => Ok(Target::Content(d.ref_id.clone())),
                            _ => Err(Decision::Skip("no changes".into())),
                        }
                    }
                    1 => Ok(targets.remove(0)),
                    _ => self.tiebreak(inst, &proposals),
                }
            }
        }
    }

    /// Two sides proposed different things. Apply the conflict policy.
    /// With `Newer`, an edit wins over a deletion because a deletion has no mtime to compare.
    fn tiebreak(&self, inst: &Instance, proposals: &[&Slot]) -> Result<Target, Decision> {
        match &self.conflict {
            ConflictPolicy::Skip => Err(Decision::Skip("conflict: concurrent differing changes".into())),
            ConflictPolicy::Prefer(label) => {
                let s = inst.slots.iter().find(|s| &s.label == label)
                    .ok_or_else(|| Decision::Abort(format!("prefer label {label:?} has no ref")))?;
                Ok(match s.snapshot() {
                    Some(_) => Target::Content(s.ref_id.clone()),
                    None => Target::Absent,
                })
            }
            ConflictPolicy::Newer => {
                let newest = proposals.iter()
                    .filter_map(|s| s.snapshot().map(|snap| (snap.mtime, &s.ref_id)))
                    .max_by_key(|(mtime, _)| *mtime);
                Ok(match newest {
                    Some((_, id)) => Target::Content(id.clone()),
                    None => Target::Absent,
                })
            }
        }
    }

    /// The copies and deletes that bring every location in line with the target, after applying `on_new` and `on_delete`.
    fn ops(&self, inst: &Instance, target: &Target) -> Vec<Op> {
        let donor_hash = match target {
            Target::Absent => None,
            Target::Content(id) => inst.slots.iter().find(|s| &s.ref_id == id).and_then(|s| s.snapshot()).map(|s| s.hash),
        };
        inst.slots.iter().filter_map(|s| match (target, &s.status) {
            (Target::Content(donor), Status::Present(snap)) => {
                if Some(snap.hash) == donor_hash { None } else { Some(Op::Copy { from: donor.clone(), to: s.ref_id.clone() }) }
            }
            (Target::Content(donor), Status::Missing) => {
                // Putting a deleted file back is a restore. `on_new` only applies to files that have never existed here.
                let create_ok = s.record.is_some() || self.on_new == NewPolicy::Replicate;
                create_ok.then(|| Op::Copy { from: donor.clone(), to: s.ref_id.clone() })
            }
            (Target::Absent, Status::Present(_)) => Some(Op::Delete { at: s.ref_id.clone() }),
            (Target::Absent, Status::Missing) => None,
            (_, Status::Unreadable(_)) => None,
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// A snapshot whose hash is this byte repeated, and whose mtime is `mtime_s` seconds after the epoch.
    fn snap(hash: u8, mtime_s: u64) -> Snapshot {
        Snapshot {
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_s),
            size: 1,
            hash: [hash; 32],
        }
    }

    /// Build a slot for tests.
    fn slot(ref_id: &str, label: &str, record: Option<Snapshot>, status: Status) -> Slot {
        Slot { ref_id: ref_id.into(), label: label.into(), record, status }
    }

    /// An instance of `f.txt` for tests.
    fn inst(slots: Vec<Slot>) -> Instance {
        Instance { identifier: vec!["f.txt".into()], slots }
    }

    /// Bidirectional policy with this conflict setting. The other fields stay at their defaults.
    fn bidirectional(conflict: ConflictPolicy) -> Policy {
        Policy { content: ContentPolicy::Bidirectional, conflict, ..Policy::default() }
    }

    #[test]
    fn unilateral_edit_wins_regardless_of_mtime() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(2, 50))),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(1, 100))),
        ]);
        let d = bidirectional(ConflictPolicy::Skip).decide(&i);
        assert_eq!(d, Decision::Plan(vec![Op::Copy { from: "source".into(), to: "dest".into() }]));
    }

    #[test]
    fn concurrent_differing_edits_skip_by_default_newer_breaks_tie() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(3, 300))),
        ]);
        assert!(matches!(bidirectional(ConflictPolicy::Skip).decide(&i), Decision::Skip(_)));
        let d = bidirectional(ConflictPolicy::Newer).decide(&i);
        assert_eq!(d, Decision::Plan(vec![Op::Copy { from: "dest".into(), to: "source".into() }]));
    }

    #[test]
    fn convergent_concurrent_edits_commit_with_no_ops() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(2, 300))),
        ]);
        assert_eq!(bidirectional(ConflictPolicy::Skip).decide(&i), Decision::Plan(vec![]));
    }

    #[test]
    fn late_join_conforms_instead_of_proposing() {
        // Touch-to-bootstrap: an empty file appears at dest while source has an established record.
        // The new file must not win; it gets filled from source.
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("dest", "dest", None, Status::Present(snap(9, 999))),
        ]);
        let d = bidirectional(ConflictPolicy::Newer).decide(&i);
        assert_eq!(d, Decision::Plan(vec![Op::Copy { from: "source".into(), to: "dest".into() }]));
    }

    #[test]
    fn first_run_seeds_identical_content_and_tiebreaks_differing() {
        let same = inst(vec![
            slot("source", "source", None, Status::Present(snap(1, 100))),
            slot("dest", "dest", None, Status::Present(snap(1, 200))),
        ]);
        assert_eq!(bidirectional(ConflictPolicy::Skip).decide(&same), Decision::Plan(vec![]));

        let differ = inst(vec![
            slot("source", "source", None, Status::Present(snap(1, 100))),
            slot("dest", "dest", None, Status::Present(snap(2, 200))),
        ]);
        assert!(matches!(bidirectional(ConflictPolicy::Skip).decide(&differ), Decision::Skip(_)));
        let d = bidirectional(ConflictPolicy::Newer).decide(&differ);
        assert_eq!(d, Decision::Plan(vec![Op::Copy { from: "dest".into(), to: "source".into() }]));
    }

    #[test]
    fn missing_without_record_is_gated_by_on_new() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("dest", "dest", None, Status::Missing),
        ]);
        // Restrict: the edit goes on record but no file is created at dest.
        assert_eq!(bidirectional(ConflictPolicy::Skip).decide(&i), Decision::Plan(vec![]));
        let replicate = Policy { on_new: NewPolicy::Replicate, ..bidirectional(ConflictPolicy::Skip) };
        assert_eq!(replicate.decide(&i), Decision::Plan(vec![Op::Copy { from: "source".into(), to: "dest".into() }]));
    }

    #[test]
    fn deletion_restores_propagates_or_skips() {
        let deleted = || inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Missing),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(1, 100))),
        ]);
        let restore = bidirectional(ConflictPolicy::Skip);
        assert_eq!(restore.decide(&deleted()), Decision::Plan(vec![Op::Copy { from: "dest".into(), to: "source".into() }]));

        let propagate = Policy { on_delete: DeletePolicy::Propagate, ..bidirectional(ConflictPolicy::Skip) };
        assert_eq!(propagate.decide(&deleted()), Decision::Plan(vec![Op::Delete { at: "dest".into() }]));

        let skip = Policy { on_delete: DeletePolicy::Skip, ..bidirectional(ConflictPolicy::Skip) };
        assert!(matches!(skip.decide(&deleted()), Decision::Skip(_)));
    }

    #[test]
    fn edit_against_deletion_is_a_conflict_where_edit_beats_deletion_under_newer() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Missing),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(2, 200))),
        ]);
        assert!(matches!(bidirectional(ConflictPolicy::Skip).decide(&i), Decision::Skip(_)));
        let d = bidirectional(ConflictPolicy::Newer).decide(&i);
        assert_eq!(d, Decision::Plan(vec![Op::Copy { from: "dest".into(), to: "source".into() }]));
    }

    #[test]
    fn mirror_overwrites_dest_changes_and_fans_out() {
        let policy = Policy {
            content: ContentPolicy::Mirror("source".into()),
            on_new: NewPolicy::Replicate,
            ..Policy::default()
        };
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("dest?project=a", "dest", Some(snap(1, 100)), Status::Present(snap(2, 999))),
            slot("dest?project=b", "dest", None, Status::Missing),
        ]);
        let d = policy.decide(&i);
        assert_eq!(d, Decision::Plan(vec![
            Op::Copy { from: "source".into(), to: "dest?project=a".into() },
            Op::Copy { from: "source".into(), to: "dest?project=b".into() },
        ]));
    }

    #[test]
    fn mirror_deletion_respects_on_delete() {
        let i = || inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Missing),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(1, 100))),
        ]);
        let base = Policy { content: ContentPolicy::Mirror("source".into()), ..Policy::default() };
        // Restore: the surviving dest copy repairs the source.
        assert_eq!(base.decide(&i()), Decision::Plan(vec![Op::Copy { from: "dest".into(), to: "source".into() }]));
        let propagate = Policy { on_delete: DeletePolicy::Propagate, ..base };
        assert_eq!(propagate.decide(&i()), Decision::Plan(vec![Op::Delete { at: "dest".into() }]));
    }
}
