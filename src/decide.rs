//! The decision for a key: what each of its replicas should hold, given their transitions and the relation's policy.
//!
//! Each rule in docs/design.md is marked below by its name, either on a function or on a comment inside `decide`: Voice, Admit, Hold, Contest, Stand, Conform, Resolve, Operation.
//! A replica is decided from the bases and observations of the key alone, so replicas can be visited in any order.
//! The result is an assignment, which says whether each replica keeps its current state, receives new content, or has its file deleted. Held replicas are listed with the reason.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::flow::{Flow, Kind, Policy, Property, Rule};
use crate::observe::{Basis, Hash, Observation, Snapshot, Transition, classify};

/// A replica of a key, with its recorded basis and what this cycle observed there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub id: String,
    pub site: String,
    pub path: PathBuf,
    pub basis: Basis,
    pub observation: Observation,
}

impl Slot {
    /// How the file changed since its basis was recorded.
    pub fn transition(&self) -> Transition {
        classify(&self.basis, &self.observation)
    }

    /// The observed snapshot, if the file is present.
    fn snapshot(&self) -> Option<&Snapshot> {
        self.observation.snapshot()
    }

    /// True if the file is present and still matches its recorded snapshot, so its content is the last synced version.
    fn settled(&self) -> bool {
        self.basis.snapshot().is_some() && self.transition() == Transition::Unchanged && self.snapshot().is_some()
    }

    /// True if the cycle could not read the file.
    fn unreadable(&self) -> bool {
        matches!(self.observation, Observation::Unreadable(_))
    }

    /// The file name, which the `name` resolver rule compares.
    fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// All replicas of a key.
#[derive(Clone, Debug)]
pub struct Situation {
    pub slots: Vec<Slot>,
}

/// A changed replica's request to its peers: take its content, or delete the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proposal {
    /// The content with this hash, as held by replica `from`.
    Content { from: String, hash: Hash },
    Absent,
}

impl Proposal {
    /// The content hash, or None for absence. Candidates with the same value are on the same side of a contest.
    fn value(&self) -> Option<Hash> {
        match self {
            Proposal::Content { hash, .. } => Some(*hash),
            Proposal::Absent => None,
        }
    }
}

/// The state a replica should be in after the plan runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Desired {
    Keep,
    /// The content with this hash, taken from replica `from`. When `sync_times` is on, the write also copies the mtime of `from`.
    Content { from: String, hash: Hash },
    Absent,
}

/// A write or delete that brings a replica to its desired state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Write the content with this hash to replica `to`. The mtime, if copied, comes from replica `from`.
    Write { to: String, hash: Hash, from: String },
    Delete { at: String },
}

/// The decision for a key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Assignment {
    /// The desired state of each replica, by id. Held replicas are left out.
    pub desired: BTreeMap<String, Desired>,
    /// The held replicas and the reason for each, in slot order.
    pub held: Vec<(String, String)>,
}

impl Assignment {
    /// The writes and deletes that take each replica from what was observed to its desired state.
    pub fn ops(&self, situation: &Situation) -> Vec<Op> {
        situation.slots.iter().filter_map(|slot| operation(slot, self.desired.get(&slot.id)?)).collect()
    }

    /// True if the replica with this id is held.
    pub fn is_held(&self, id: &str) -> bool {
        self.held.iter().any(|(h, _)| h == id)
    }
}

/// Decide what each replica of a key should hold under the relation's policy.
pub fn decide(policy: &Policy, situation: &Situation) -> Assignment {
    let slots = &situation.slots;
    let established = slots.iter().any(|s| s.basis.is_recorded());
    let proposals: Vec<Option<Proposal>> = slots.iter().map(|s| voice(s, established)).collect();
    let unreadable: Vec<&Slot> = slots.iter().filter(|s| s.unreadable()).collect();
    let mut out = Assignment::default();
    for (ti, target) in slots.iter().enumerate() {
        if let Observation::Unreadable(e) = &target.observation {
            out.held.push((target.id.clone(), format!("unreadable: {e}")));
            continue;
        }
        let own = proposals[ti].as_ref();
        // Hold: if the flow carries the target's proposal to an unreadable peer, the target is held as well. Its basis is left alone, so the change is still pending when the peer can be read again.
        if let Some(p) = own {
            if let Some(u) = unreadable.iter().find(|u| travels(&policy.flow, target, p, u)) {
                out.held.push((target.id.clone(), format!("waiting on {}: unreadable", u.id)));
                continue;
            }
        }
        let peers = || slots.iter().zip(&proposals).filter(|(s, _)| s.id != target.id);
        // Contest: the candidates are the proposals of readable peers that the flow carries to the target.
        let mut candidates: Vec<(&Slot, Proposal)> = peers()
            .filter(|(s, _)| !s.unreadable())
            .filter_map(|(s, p)| p.as_ref().filter(|p| travels(&policy.flow, s, p, target)).map(|p| (s, p.clone())))
            .collect();
        if !candidates.is_empty() {
            // The target's own proposal joins them if the flow carries it back to one of those peers.
            if let Some(p) = own {
                if candidates.iter().any(|(c, _)| travels(&policy.flow, target, p, c)) {
                    candidates.push((target, p.clone()));
                }
            }
        } else {
            // Stand: if the flow carries the target's proposal to some peer, the target keeps its state.
            // Conform: otherwise the candidates are the settled peers whose content the flow carries to the target.
            let heard = own.is_some_and(|p| peers().any(|(s, _)| travels(&policy.flow, target, p, s)));
            if !heard {
                candidates = peers()
                    .filter(|(s, _)| s.settled())
                    .filter_map(|(s, _)| {
                        let p = Proposal::Content { from: s.id.clone(), hash: s.snapshot().expect("settled slots are present").hash };
                        travels(&policy.flow, s, &p, target).then_some((s, p))
                    })
                    .collect();
            }
        }
        if candidates.is_empty() {
            out.desired.insert(target.id.clone(), Desired::Keep);
            continue;
        }
        match resolve(&policy.resolve, &candidates) {
            Ok(Proposal::Content { from, hash }) => { out.desired.insert(target.id.clone(), Desired::Content { from, hash }); }
            Ok(Proposal::Absent) => { out.desired.insert(target.id.clone(), Desired::Absent); }
            Err(reason) => out.held.push((target.id.clone(), reason)),
        }
    }
    out
}

/// Voice: the proposal a replica makes from its transition. A replica where a file appeared proposes its content only if no replica of the key has a record yet. Otherwise it conforms to its peers.
fn voice(slot: &Slot, established: bool) -> Option<Proposal> {
    let content = || slot.snapshot().map(|s| Proposal::Content { from: slot.id.clone(), hash: s.hash });
    match slot.transition() {
        Transition::Edited | Transition::Recreated => content(),
        Transition::Appeared if !established => content(),
        Transition::Disappeared => Some(Proposal::Absent),
        _ => None,
    }
}

/// Admit: the edge kinds along which a proposal can reach a target, given what was observed at the target. An unreadable target admits the kinds for a present file and for a missing one, since it is unknown whether the file exists.
fn admitting(proposal: &Proposal, target: &Observation) -> &'static [Kind] {
    match (proposal, target) {
        (Proposal::Content { .. }, Observation::Present(_)) => &[Kind::Edit],
        (Proposal::Content { .. }, Observation::Missing) => &[Kind::Create],
        (Proposal::Content { .. }, Observation::Unreadable(_)) => &[Kind::Edit, Kind::Create],
        (Proposal::Absent, Observation::Present(_)) | (Proposal::Absent, Observation::Unreadable(_)) => &[Kind::Delete],
        (Proposal::Absent, Observation::Missing) => &[],
    }
}

/// True if the flow carries the proposal from the site of `from` to the site of `to` along an edge of an admitted kind.
fn travels(flow: &Flow, from: &Slot, proposal: &Proposal, to: &Slot) -> bool {
    admitting(proposal, &to.observation).iter().any(|k| flow.carries(*k, &from.site, &to.site))
}

/// Resolve: if the candidates all have the same value, the target takes it. Otherwise a candidate wins if it beats each candidate with a different value under the rules. If none does, the result is a conflict and the target is held.
fn resolve(rules: &[Rule], candidates: &[(&Slot, Proposal)]) -> Result<Proposal, String> {
    let distinct: BTreeSet<Option<Hash>> = candidates.iter().map(|(_, p)| p.value()).collect();
    if distinct.len() == 1 {
        return Ok(candidates[0].1.clone());
    }
    for (w, wp) in candidates {
        let wins = candidates.iter().filter(|(_, op)| op.value() != wp.value()).all(|(o, _)| beats(rules, w, o));
        if wins {
            return Ok(wp.clone());
        }
    }
    let names: Vec<&str> = candidates.iter().map(|(s, _)| s.id.as_str()).collect();
    Err(format!("conflict: {} (no rule decides)", names.join(" vs ")))
}

/// True if `w` beats `o` under the first rule that decides between them. If the files tie on a property, that rule is skipped. If no rule decides, `w` does not beat `o`.
fn beats(rules: &[Rule], w: &Slot, o: &Slot) -> bool {
    for rule in rules {
        match rule {
            Rule::Beats { winner, loser } => {
                if *winner == w.site && *loser == o.site {
                    return true;
                }
                if *winner == o.site && *loser == w.site {
                    return false;
                }
            }
            Rule::Compare { pair, by } => {
                let covers = match pair {
                    None => true,
                    Some((x, y)) => (*x == w.site && *y == o.site) || (*x == o.site && *y == w.site),
                };
                if covers {
                    if let Some(decided) = property_beats(*by, w, o) {
                        return decided;
                    }
                }
            }
        }
    }
    false
}

/// Whether `w` beats `o` on a property, or None on a tie so the caller can try the next rule. A present file beats a missing one.
fn property_beats(by: Property, w: &Slot, o: &Slot) -> Option<bool> {
    let order = match (w.snapshot(), o.snapshot()) {
        (Some(ws), Some(os)) => match by {
            Property::Mtime => ws.mtime.cmp(&os.mtime),
            Property::Size => ws.size.cmp(&os.size),
            Property::Name => o.file_name().cmp(&w.file_name()),
        },
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    };
    match order {
        Ordering::Greater => Some(true),
        Ordering::Less => Some(false),
        Ordering::Equal => None,
    }
}

/// Operation: the write or delete that brings a replica to its desired state, or None if it is already there.
fn operation(slot: &Slot, desired: &Desired) -> Option<Op> {
    match (desired, &slot.observation) {
        (Desired::Content { from, hash }, Observation::Present(snap)) => (snap.hash != *hash).then(|| Op::Write { to: slot.id.clone(), hash: *hash, from: from.clone() }),
        (Desired::Content { from, hash }, Observation::Missing) => Some(Op::Write { to: slot.id.clone(), hash: *hash, from: from.clone() }),
        (Desired::Absent, Observation::Present(_)) => Some(Op::Delete { at: slot.id.clone() }),
        _ => None,
    }
}

/// The operations as text with replica ids, in the form the intent row stores.
pub fn describe_ops(ops: &[Op]) -> String {
    ops.iter().map(|op| match op {
        Op::Write { to, hash, from } => format!("write {to} <- {from} ({})", crate::observe::short_hash(hash)),
        Op::Delete { at } => format!("delete {at}"),
    }).collect::<Vec<_>>().join("; ")
}

/// The plan as text with display names and with writes grouped by source, as in `copy source -> dest (a), dest (b); delete at dest (c)`.
pub fn describe_plan(names: &BTreeMap<&str, &str>, ops: &[Op]) -> String {
    let name = |id: &str| names.get(id).map(|n| (*n).to_owned()).unwrap_or_else(|| id.to_owned());
    let mut copies: Vec<(String, Vec<String>)> = Vec::new();
    let mut deletes: Vec<String> = Vec::new();
    for op in ops {
        match op {
            Op::Write { to, from, .. } => {
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
    let mut parts: Vec<String> = copies.into_iter().map(|(donor, dests)| format!("copy {donor} -> {}", dests.join(", "))).collect();
    if !deletes.is_empty() {
        parts.push(format!("delete at {}", deletes.join(", ")));
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn snap(hash: u8, mtime_s: u64) -> Snapshot {
        Snapshot { mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_s), size: 1, hash: [hash; 32] }
    }

    fn slot(id: &str, site: &str, basis: Basis, observation: Observation) -> Slot {
        Slot { id: id.into(), site: site.into(), path: PathBuf::from(format!("/{id}/f.txt")), basis, observation }
    }

    fn rec(hash: u8, mtime_s: u64) -> Basis {
        Basis::Snapshot(snap(hash, mtime_s))
    }

    fn present(hash: u8, mtime_s: u64) -> Observation {
        Observation::Present(snap(hash, mtime_s))
    }

    fn sit(slots: Vec<Slot>) -> Situation {
        Situation { slots }
    }

    fn policy(flow: &str, resolve: &str) -> Policy {
        Policy::parse(flow, resolve).unwrap()
    }

    fn copy(from: &str, to: &str, hash: u8) -> Op {
        Op::Write { to: to.into(), hash: [hash; 32], from: from.into() }
    }

    fn delete(at: &str) -> Op {
        Op::Delete { at: at.into() }
    }

    /// Decide a key and return its operations and the ids of the held replicas.
    fn run(p: &Policy, s: &Situation) -> (Vec<Op>, Vec<String>) {
        let a = decide(p, s);
        (a.ops(s), a.held.iter().map(|(id, _)| id.clone()).collect())
    }

    #[test]
    fn property_tie_falls_through_to_the_next_rule() {
        let s = sit(vec![
            slot("a", "a", rec(1, 100), present(2, 200)),
            slot("b", "b", rec(1, 100), present(3, 300)),
        ]);
        // Both files have size 1, so the size rule ties and mtime picks b.
        assert_eq!(run(&policy("a <-> b [edit]", "size; mtime"), &s).0, vec![copy("b", "a", 3)]);
        assert_eq!(run(&policy("a <-> b [edit]", "size; a > b"), &s).0, vec![copy("a", "b", 2)]);
        assert_eq!(run(&policy("a <-> b [edit]", "size"), &s).1, vec!["a", "b"]);
    }

    #[test]
    fn unreadable_slot_is_held_alone_and_undelivered_proposals_wait() {
        let s = sit(vec![
            slot("a", "a", rec(1, 100), present(2, 200)),
            slot("b", "b", rec(1, 100), Observation::Unreadable("is a directory".into())),
            slot("c", "c", rec(1, 100), present(1, 100)),
        ]);
        let a = decide(&policy("a -> {b, c} [edit]", ""), &s);
        assert_eq!(a.ops(&s), vec![copy("a", "c", 2)]);
        assert_eq!(a.held, vec![("a".to_string(), "waiting on b: unreadable".to_string()), ("b".to_string(), "unreadable: is a directory".to_string())]);
        // Here the only edge runs from c to a. Since a's edit cannot reach b, a is not held. Instead c overwrites the edit.
        let a = decide(&policy("c -> a [edit]", ""), &s);
        assert_eq!(a.ops(&s), vec![copy("c", "a", 1)]);
        assert_eq!(a.held, vec![("b".to_string(), "unreadable: is a directory".to_string())]);
        // b is unreadable, so it offers no content. a is settled and makes no proposal, so a is not held either.
        let a = decide(&policy("a <-> b [edit]", ""), &sit(vec![
            slot("a", "a", rec(1, 100), present(1, 100)),
            slot("b", "b", rec(1, 100), Observation::Unreadable("denied".into())),
        ]));
        assert!(a.ops(&sit(vec![])).is_empty());
        assert_eq!(a.held, vec![("b".to_string(), "unreadable: denied".to_string())]);
        // A deletion at a is held the same way when a delete edge leads to the unreadable peer.
        let a = decide(&policy("a <-> b [edit]; a -> b [delete]", ""), &sit(vec![
            slot("a", "a", rec(1, 100), Observation::Missing),
            slot("b", "b", rec(1, 100), Observation::Unreadable("denied".into())),
        ]));
        assert_eq!(a.held, vec![("a".to_string(), "waiting on b: unreadable".to_string()), ("b".to_string(), "unreadable: denied".to_string())]);
    }

    #[test]
    fn unilateral_edit_wins_regardless_of_mtime() {
        let s = sit(vec![
            slot("source", "source", rec(1, 100), present(2, 50)),
            slot("dest", "dest", rec(1, 100), present(1, 100)),
        ]);
        assert_eq!(run(&policy("source <-> dest [edit]", ""), &s).0, vec![copy("source", "dest", 2)]);
    }

    #[test]
    fn concurrent_differing_edits_hold_without_a_rule_and_mtime_breaks_the_tie() {
        let s = sit(vec![
            slot("source", "source", rec(1, 100), present(2, 200)),
            slot("dest", "dest", rec(1, 100), present(3, 300)),
        ]);
        assert_eq!(run(&policy("source <-> dest [edit]", ""), &s).1, vec!["source", "dest"]);
        assert_eq!(run(&policy("source <-> dest [edit]", "source mtime dest"), &s).0, vec![copy("dest", "source", 3)]);
        assert_eq!(run(&policy("source <-> dest [edit]", "source > dest"), &s).0, vec![copy("source", "dest", 2)]);
    }

    #[test]
    fn one_way_edge_overwrites_silently() {
        let both = sit(vec![
            slot("source", "source", rec(1, 100), present(2, 200)),
            slot("dest", "dest", rec(1, 100), present(3, 300)),
        ]);
        assert_eq!(run(&policy("source -> dest [edit]", ""), &both).0, vec![copy("source", "dest", 2)]);
        let lone = sit(vec![
            slot("source", "source", rec(1, 100), present(1, 100)),
            slot("dest", "dest", rec(1, 100), present(3, 300)),
        ]);
        assert_eq!(run(&policy("source -> dest [edit]", ""), &lone).0, vec![copy("source", "dest", 1)]);
    }

    #[test]
    fn three_sites_conflict_only_where_paths_meet() {
        let s = sit(vec![
            slot("a", "a", rec(1, 100), present(2, 200)),
            slot("b", "b", rec(1, 100), present(3, 300)),
            slot("c", "c", rec(1, 100), present(1, 100)),
        ]);
        let a = decide(&policy("a -> b [edit]; {a, b} -> c [edit]", ""), &s);
        assert_eq!(a.ops(&s), vec![copy("a", "b", 2)]);
        assert_eq!(a.held, vec![("c".to_string(), "conflict: a vs b (no rule decides)".to_string())]);
        assert_eq!(run(&policy("a -> b [edit]; {a, b} -> c [edit]", "a > b"), &s).0, vec![copy("a", "b", 2), copy("a", "c", 2)]);
        assert_eq!(run(&policy("a -> b [edit]; {a, b} -> c [edit]", "b > a"), &s).0, vec![copy("a", "b", 2), copy("b", "c", 3)]);
    }

    #[test]
    fn chain_propagates_transitively() {
        let s = sit(vec![
            slot("a", "a", rec(1, 100), present(2, 200)),
            slot("b", "b", rec(1, 100), present(1, 100)),
            slot("c", "c", rec(1, 100), present(1, 100)),
        ]);
        assert_eq!(run(&policy("a -> b [edit]; b -> c [edit]", ""), &s).0, vec![copy("a", "b", 2), copy("a", "c", 2)]);
    }

    #[test]
    fn convergent_concurrent_edits_commit_with_no_ops() {
        let s = sit(vec![
            slot("source", "source", rec(1, 100), present(2, 200)),
            slot("dest", "dest", rec(1, 100), present(2, 300)),
        ]);
        let a = decide(&policy("source <-> dest [edit]", ""), &s);
        assert!(a.ops(&s).is_empty() && a.held.is_empty());
    }

    #[test]
    fn late_join_conforms_instead_of_proposing() {
        let s = sit(vec![
            slot("source", "source", rec(1, 100), present(1, 100)),
            slot("dest", "dest", Basis::None, present(9, 999)),
        ]);
        assert_eq!(run(&policy("source <-> dest [edit]", ""), &s).0, vec![copy("source", "dest", 1)]);
        assert!(run(&policy("dest -> source [edit]", ""), &s).0.is_empty());
    }

    #[test]
    fn a_file_recreated_over_a_tombstone_proposes_like_an_edit() {
        let s = sit(vec![
            slot("source", "source", Basis::Tombstone, present(5, 500)),
            slot("dest", "dest", rec(1, 100), present(1, 100)),
        ]);
        assert_eq!(run(&policy("source <-> dest [edit]", ""), &s).0, vec![copy("source", "dest", 5)]);
    }

    #[test]
    fn first_run_seeds_identical_content_and_contests_differing() {
        let same = sit(vec![
            slot("source", "source", Basis::None, present(1, 100)),
            slot("dest", "dest", Basis::None, present(1, 200)),
        ]);
        assert!(run(&policy("source <-> dest [edit]", ""), &same).0.is_empty());
        let differ = sit(vec![
            slot("source", "source", Basis::None, present(1, 100)),
            slot("dest", "dest", Basis::None, present(2, 200)),
        ]);
        assert_eq!(run(&policy("source <-> dest [edit]", ""), &differ).1, vec!["source", "dest"]);
        assert_eq!(run(&policy("source <-> dest [edit]", "mtime"), &differ).0, vec![copy("dest", "source", 2)]);
    }

    #[test]
    fn creation_needs_a_create_edge() {
        let s = sit(vec![
            slot("source", "source", rec(1, 100), present(2, 200)),
            slot("dest", "dest", Basis::None, Observation::Missing),
        ]);
        assert!(run(&policy("source <-> dest [edit]", ""), &s).0.is_empty());
        assert_eq!(run(&policy("source <-> dest [edit]; source -> dest [create]", ""), &s).0, vec![copy("source", "dest", 2)]);
        let settled = sit(vec![
            slot("source", "source", rec(1, 100), present(1, 100)),
            slot("dest", "dest", Basis::None, Observation::Missing),
        ]);
        assert_eq!(run(&policy("source -> dest [create]", ""), &settled).0, vec![copy("source", "dest", 1)]);
    }

    #[test]
    fn deletion_propagates_along_delete_edges_or_is_restored_through_create_edges() {
        let deleted = || sit(vec![
            slot("source", "source", rec(1, 100), Observation::Missing),
            slot("dest", "dest", rec(1, 100), present(1, 100)),
        ]);
        assert_eq!(run(&policy("source <-> dest [edit]; dest -> source [create]", ""), &deleted()).0, vec![copy("dest", "source", 1)]);
        assert_eq!(run(&policy("source <-> dest [edit]; source -> dest [delete]", ""), &deleted()).0, vec![delete("dest")]);
        assert!(run(&policy("source <-> dest [edit]", ""), &deleted()).0.is_empty());
    }

    #[test]
    fn edit_against_deletion_is_a_conflict_where_the_file_beats_the_absence_under_mtime() {
        let s = sit(vec![
            slot("source", "source", rec(1, 100), Observation::Missing),
            slot("dest", "dest", rec(1, 100), present(2, 200)),
        ]);
        let contested = policy("source <-> dest [edit delete]; dest -> source [create]", "");
        assert_eq!(run(&contested, &s).1, vec!["source", "dest"]);
        let newer = policy("source <-> dest [edit delete]; dest -> source [create]", "mtime");
        assert_eq!(run(&newer, &s).0, vec![copy("dest", "source", 2)]);
        let one_way = policy("source <-> dest [edit]; source -> dest [delete]", "");
        assert_eq!(run(&one_way, &s).0, vec![delete("dest")]);
    }

    #[test]
    fn mirror_overwrites_dest_changes_and_fans_out() {
        let s = sit(vec![
            slot("source", "source", rec(1, 100), present(1, 100)),
            slot("dest[project=a]", "dest", rec(1, 100), present(2, 999)),
            slot("dest[project=b]", "dest", Basis::None, Observation::Missing),
        ]);
        assert_eq!(run(&policy("source -> dest [edit create]", ""), &s).0, vec![copy("source", "dest[project=a]", 1), copy("source", "dest[project=b]", 1)]);
    }

    #[test]
    fn replicas_of_one_site_sync_through_a_cycle() {
        let s = sit(vec![
            slot("source", "source", rec(1, 100), present(1, 100)),
            slot("dest[x]", "dest", rec(1, 100), present(2, 200)),
            slot("dest[y]", "dest", rec(1, 100), present(1, 100)),
        ]);
        assert_eq!(run(&policy("source <-> dest [edit]", ""), &s).0, vec![copy("dest[x]", "source", 2), copy("dest[x]", "dest[y]", 2)]);
        let both = sit(vec![
            slot("source", "source", rec(1, 100), present(1, 100)),
            slot("dest[x]", "dest", rec(1, 100), present(2, 200)),
            slot("dest[y]", "dest", rec(1, 100), present(3, 300)),
        ]);
        assert_eq!(run(&policy("source <-> dest [edit]", ""), &both).1, vec!["source", "dest[x]", "dest[y]"]);
        assert_eq!(run(&policy("source <-> dest [edit]", "dest mtime dest"), &both).0, vec![copy("dest[y]", "source", 3), copy("dest[y]", "dest[x]", 3)]);
    }

    #[test]
    fn size_and_name_properties() {
        let big = Snapshot { size: 10, ..snap(2, 100) };
        let s = sit(vec![
            slot("a", "a", rec(1, 100), Observation::Present(big)),
            slot("b", "b", rec(1, 100), present(3, 100)),
        ]);
        assert_eq!(run(&policy("a <-> b [edit]", "size"), &s).0, vec![copy("a", "b", 2)]);
        let mut named = s.clone();
        named.slots[0].path = "/a/zeta.txt".into();
        named.slots[1].path = "/b/alpha.txt".into();
        assert_eq!(run(&policy("a <-> b [edit]", "a name b"), &named).0, vec![copy("b", "a", 3)]);
        assert_eq!(run(&policy("a <-> b [edit]", "mtime"), &s).1, vec!["a", "b"]);
    }

    #[test]
    fn plans_are_described_with_display_names() {
        let names: BTreeMap<&str, &str> = [("source", "source"), ("dest[a]", "dest (a)"), ("dest[b]", "dest (b)"), ("dest[c]", "dest (c)")].into_iter().collect();
        let ops = vec![copy("source", "dest[a]", 1), copy("source", "dest[b]", 1), delete("dest[c]")];
        assert_eq!(describe_plan(&names, &ops), "copy source -> dest (a), dest (b); delete at dest (c)");
        assert_eq!(describe_plan(&names, &[]), "");
        assert_eq!(describe_ops(&ops[..1]), "write dest[a] <- source (01010101)");
    }
}
