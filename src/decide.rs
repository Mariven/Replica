//! Decide the desired state of each replica of a key, given the transitions of all its replicas and the policy of the relation.
//!
//! The rules from docs/design.md each appear below as a small function under the same name: Voice, Admit, Contest, Stand, Conform, Resolve, Hold, Operation.
//! The decision for a target replica depends only on the observations of the key, so the outcome is the same in whatever order the replicas are visited.
//! The result is an assignment: for each replica, keep its content, take a content hash, or become absent. Replicas held back are listed with their reasons.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::flow::{Flow, Kind, Policy, Property, Rule};
use crate::observe::{Basis, Hash, Observation, Snapshot, Transition, classify};

/// The inputs about a replica of a key: its identity, its site, its path, the basis from the store, and the observation from this cycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub id: String,
    pub site: String,
    pub path: PathBuf,
    pub basis: Basis,
    pub observation: Observation,
}

impl Slot {
    /// How the replica moved since its basis.
    pub fn transition(&self) -> Transition {
        classify(&self.basis, &self.observation)
    }

    /// The observed snapshot, when the file is present.
    fn snapshot(&self) -> Option<&Snapshot> {
        self.observation.snapshot()
    }

    /// True when the replica matches its recorded snapshot and the file is present: its content is the agreed baseline.
    fn settled(&self) -> bool {
        self.basis.snapshot().is_some() && self.transition() == Transition::Unchanged && self.snapshot().is_some()
    }

    /// True when the cycle could not read the replica.
    fn unreadable(&self) -> bool {
        matches!(self.observation, Observation::Unreadable(_))
    }

    /// The file name, for the `name` property.
    fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// All replicas of a key.
#[derive(Clone, Debug)]
pub struct Situation {
    pub slots: Vec<Slot>,
}

/// A changed replica's request to its peers: take this content, or drop the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proposal {
    /// This content, as held by the replica `from`.
    Content { from: String, hash: Hash },
    Absent,
}

impl Proposal {
    /// The content hash, or None for absence. Candidates with equal values stand on the same side of a contest.
    fn value(&self) -> Option<Hash> {
        match self {
            Proposal::Content { hash, .. } => Some(*hash),
            Proposal::Absent => None,
        }
    }
}

/// The target state of a replica once the plan runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Desired {
    Keep,
    /// This hash, as proposed or held by the replica `from`. When times are synced, the write copies the modification time of `from` as well.
    Content { from: String, hash: Hash },
    Absent,
}

/// A filesystem operation on a replica, implied by the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Write the content of this hash to the replica, taking the modification time from `from`.
    Write { to: String, hash: Hash, from: String },
    Delete { at: String },
}

/// The decision for a key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Assignment {
    /// Desired state by replica id. Held replicas are absent from this map.
    pub desired: BTreeMap<String, Desired>,
    /// Replicas held back, with the reason, in slot order.
    pub held: Vec<(String, String)>,
}

impl Assignment {
    /// The operations needed to move each replica from its observed state to its desired one.
    pub fn ops(&self, situation: &Situation) -> Vec<Op> {
        situation.slots.iter().filter_map(|slot| operation(slot, self.desired.get(&slot.id)?)).collect()
    }

    /// True when a replica is held.
    pub fn is_held(&self, id: &str) -> bool {
        self.held.iter().any(|(h, _)| h == id)
    }
}

/// Decide the assignment for a key under a policy.
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
        // Hold: when the flow carries the target's proposal toward an unreadable peer, the target waits. Its basis stays as it is, so the change is still pending once the peer is readable again.
        if let Some(p) = own {
            if let Some(u) = unreadable.iter().find(|u| travels(&policy.flow, target, p, u)) {
                out.held.push((target.id.clone(), format!("waiting on {}: unreadable", u.id)));
                continue;
            }
        }
        let peers = || slots.iter().zip(&proposals).filter(|(s, _)| s.id != target.id);
        // Contest: gather proposals from readable peers. A proposal counts only when the flow carries it to the target.
        let mut candidates: Vec<(&Slot, Proposal)> = peers()
            .filter(|(s, _)| !s.unreadable())
            .filter_map(|(s, p)| p.as_ref().filter(|p| travels(&policy.flow, s, p, target)).map(|p| (s, p.clone())))
            .collect();
        if !candidates.is_empty() {
            // The target's own proposal joins the contest only when the flow carries it back to at least one challenger.
            if let Some(p) = own {
                if candidates.iter().any(|(c, _)| travels(&policy.flow, target, p, c)) {
                    candidates.push((target, p.clone()));
                }
            }
        } else {
            // Stand: when the flow carries the target's proposal to some peer, the target keeps its state. Conform: with no such proposal, settled peers supply the content.
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

/// Voice: derive a proposal from the replica's transition. A newcomer with no record makes no proposal when some peer already has a basis; it conforms instead.
fn voice(slot: &Slot, established: bool) -> Option<Proposal> {
    let content = || slot.snapshot().map(|s| Proposal::Content { from: slot.id.clone(), hash: s.hash });
    match slot.transition() {
        Transition::Edited | Transition::Recreated => content(),
        Transition::Appeared if !established => content(),
        Transition::Disappeared => Some(Proposal::Absent),
        _ => None,
    }
}

/// Admit: the kinds of edge along which a proposal may travel to a target, given the target's observed state. An unreadable target admits the kinds for both a present and a missing file, since its state is unknown.
fn admitting(proposal: &Proposal, target: &Observation) -> &'static [Kind] {
    match (proposal, target) {
        (Proposal::Content { .. }, Observation::Present(_)) => &[Kind::Edit],
        (Proposal::Content { .. }, Observation::Missing) => &[Kind::Create],
        (Proposal::Content { .. }, Observation::Unreadable(_)) => &[Kind::Edit, Kind::Create],
        (Proposal::Absent, Observation::Present(_)) | (Proposal::Absent, Observation::Unreadable(_)) => &[Kind::Delete],
        (Proposal::Absent, Observation::Missing) => &[],
    }
}

/// True when the flow carries the proposal from one slot's site to another's along an admitted kind.
fn travels(flow: &Flow, from: &Slot, proposal: &Proposal, to: &Slot) -> bool {
    admitting(proposal, &to.observation).iter().any(|k| flow.carries(*k, &from.site, &to.site))
}

/// Resolve: candidates with equal values stand together. A candidate wins by beating all candidates of other values under the rules. With no such candidate, the contest stays unresolved and the target is held.
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

/// Try the rules in order until one decides between the two slots. A property tie passes to the next rule. When the rules run out, `w` does not beat `o`.
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

/// Compare two slots by a property. A present file beats an absence. On a tie this returns None so the caller can move to the next rule.
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

/// Operation: the write or delete needed to bring a slot to its desired state, or None when it is already there.
fn operation(slot: &Slot, desired: &Desired) -> Option<Op> {
    match (desired, &slot.observation) {
        (Desired::Content { from, hash }, Observation::Present(snap)) => (snap.hash != *hash).then(|| Op::Write { to: slot.id.clone(), hash: *hash, from: from.clone() }),
        (Desired::Content { from, hash }, Observation::Missing) => Some(Op::Write { to: slot.id.clone(), hash: *hash, from: from.clone() }),
        (Desired::Absent, Observation::Present(_)) => Some(Op::Delete { at: slot.id.clone() }),
        _ => None,
    }
}

/// Describe operations by replica id, as the intent journal records them.
pub fn describe_ops(ops: &[Op]) -> String {
    ops.iter().map(|op| match op {
        Op::Write { to, hash, from } => format!("write {to} <- {from} ({})", crate::observe::short_hash(hash)),
        Op::Delete { at } => format!("delete {at}"),
    }).collect::<Vec<_>>().join("; ")
}

/// Phrase a plan with display names, grouping writes by source: `copy source -> dest (a), dest (b); delete at dest (c)`.
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

    /// Operations and held ids of a decision.
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
        // Both files have size 1, so size ties and mtime decides for b.
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
        // With only a one-way edge from c to a, the flow has no path from a to b, so a is free to proceed and c overwrites its stray edit.
        let a = decide(&policy("c -> a [edit]", ""), &s);
        assert_eq!(a.ops(&s), vec![copy("c", "a", 1)]);
        assert_eq!(a.held, vec![("b".to_string(), "unreadable: is a directory".to_string())]);
        // An unreadable slot contributes no content. A settled slot has no proposal, so nothing waits on the unreadable peer.
        let a = decide(&policy("a <-> b [edit]", ""), &sit(vec![
            slot("a", "a", rec(1, 100), present(1, 100)),
            slot("b", "b", rec(1, 100), Observation::Unreadable("denied".into())),
        ]));
        assert!(a.ops(&sit(vec![])).is_empty());
        assert_eq!(a.held, vec![("b".to_string(), "unreadable: denied".to_string())]);
        // A deletion waits in the same way when a delete edge leads to the unreadable peer.
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
