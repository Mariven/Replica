//! Determine copy and deletion actions from inspected location slots.
//!
//! Propagate changes along directed label edges with transitive reachability: under `a -> b; b -> c`, edits move from `a` to `c`. Evaluate each slot independently. Gather proposals from reachable modified slots, applying conflict rules to select content for replication.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::status::{Change, Snapshot, Status, classify};

/// Inspected filesystem status and synchronization records for a replica location.
#[derive(Clone, Debug)]
pub struct Slot {
    /// Reference identifier for this replica location.
    pub ref_id: String,
    /// Configuration label grouping replicas in the topology graph.
    pub label: String,
    /// Absolute filesystem path to the replica location.
    pub path: PathBuf,
    /// Last recorded snapshot from synchronization metadata.
    pub record: Option<Snapshot>,
    /// Inspected filesystem state currently found on disk.
    pub status: Status,
}

impl Slot {
    /// Compare current filesystem status against recorded metadata.
    pub fn change(&self) -> Change {
        classify(self.record.as_ref(), &self.status)
    }

    /// Retrieve current snapshot metadata when present on disk.
    fn snapshot(&self) -> Option<&Snapshot> {
        match &self.status {
            Status::Present(s) => Some(s),
            _ => None,
        }
    }

    /// Return true when recorded metadata matches an unchanged file present on disk.
    fn settled(&self) -> bool {
        self.record.is_some() && self.change() == Change::Unchanged && self.snapshot().is_some()
    }

    /// Extract the file name from this slot's path for name comparisons.
    fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// Group evaluated replica slots under a shared file identifier.
#[derive(Clone, Debug)]
pub struct Instance {
    /// Evaluated replica slots for this identifier.
    pub slots: Vec<Slot>,
}

/// Filesystem operation planned across replica locations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Copy file content from one replica location to another.
    Copy { from: String, to: String },
    /// Delete file at the specified replica location.
    Delete { at: String },
}

/// Outcome of evaluating an instance across configured replica locations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Scheduled operations along with slot references omitted from record updates.
    Plan { ops: Vec<Op>, skipped: Vec<(String, String)> },
    /// Omission of operations to preserve all existing records, holding an explanation reason.
    Skip(String),
}

/// Directed graph of label dependencies with precomputed transitive reachability.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Graph {
    /// Explicit directed edges declared between replica labels.
    edges: BTreeSet<(String, String)>,
    /// Precomputed transitive reachability map for each source label.
    reach: BTreeMap<String, BTreeSet<String>>,
}

impl Graph {
    /// Parse directed edge statements separated by `;`, expanding braced label sets and arrow directions.
    pub fn parse(text: &str) -> Result<Graph> {
        let mut g = Graph::default();
        for stmt in statements(text) {
            // Split input statement into tokens.
            let toks = tokenize(stmt);
            let mut pos = 0;
            // Parse left-hand label set.
            let lhs = parse_set(&toks, &mut pos).map_err(|e| anyhow::anyhow!("{e} in {stmt:?}"))?;
            let op = match toks.get(pos) {
                Some(Tok::Right) | Some(Tok::Left) | Some(Tok::Both) => toks[pos].clone(),
                _ => bail!("expected ->, <- or <-> in {stmt:?}"),
            };
            pos += 1;
            // Parse right-hand label set.
            let rhs = parse_set(&toks, &mut pos).map_err(|e| anyhow::anyhow!("{e} in {stmt:?}"))?;
            if pos != toks.len() {
                bail!("unexpected trailing text in {stmt:?}");
            }
            // Populate directed edges according to arrow operator orientation.
            for l in &lhs {
                for r in &rhs {
                    if matches!(op, Tok::Right | Tok::Both) {
                        g.edges.insert((l.clone(), r.clone()));
                    }
                    if matches!(op, Tok::Left | Tok::Both) {
                        g.edges.insert((r.clone(), l.clone()));
                    }
                }
            }
        }
        g.close();
        Ok(g)
    }

    /// Compute transitive reachability from every source label.
    fn close(&mut self) {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (from, _) in &self.edges {
            // Traverse outgoing edges with a depth-first stack to find all reachable labels.
            let mut seen = BTreeSet::new();
            let mut stack = vec![from.clone()];
            while let Some(cur) = stack.pop() {
                for (a, b) in &self.edges {
                    if *a == cur && seen.insert(b.clone()) {
                        stack.push(b.clone());
                    }
                }
            }
            out.insert(from.clone(), seen);
        }
        self.reach = out;
    }

    /// Test whether changes from source label reach target label directly or transitively.
    pub fn reaches(&self, from: &str, to: &str) -> bool {
        self.reach.get(from).is_some_and(|set| set.contains(to))
    }

    /// Return true when the graph contains no configured edges.
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Collect all unique labels appearing across graph edges.
    pub fn labels(&self) -> BTreeSet<&str> {
        self.edges.iter().flat_map(|(a, b)| [a.as_str(), b.as_str()]).collect()
    }
}

/// File metadata attribute inspected to resolve concurrent changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    /// Modification timestamp, favoring newer files.
    Mtime,
    /// File byte length, favoring larger files.
    Size,
    /// File path name, favoring alphabetically earlier names.
    Name,
}

impl Property {
    /// Map token string to the matching property variant.
    fn parse(word: &str) -> Option<Property> {
        match word {
            "mtime" => Some(Property::Mtime),
            "size" => Some(Property::Size),
            "name" => Some(Property::Name),
            _ => None,
        }
    }
}

/// Conflict resolution rule prioritizing specific labels or comparing file properties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Fixed label precedence prioritizing winner over loser.
    Beats { winner: String, loser: String },
    /// Property comparison across all labels or between a designated label pair.
    Compare { pair: Option<(String, String)>, by: Property },
}

/// Parse semicolon-separated conflict rules into priority rankings and property comparisons.
pub fn parse_rules(text: &str) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    for stmt in statements(text) {
        let toks = tokenize(stmt);
        // Match global property comparison when a statement contains only a property name.
        if let [Tok::Word(w)] = toks.as_slice() {
            match Property::parse(w) {
                Some(by) => rules.push(Rule::Compare { pair: None, by }),
                None => bail!("expected a property (mtime, size, name) or a comparison in {stmt:?}"),
            }
            continue;
        }
        let mut pos = 0;
        // Parse left-hand label set.
        let lhs = parse_set(&toks, &mut pos).map_err(|e| anyhow::anyhow!("{e} in {stmt:?}"))?;
        // Validate the operator before reading the right-hand side, so a missing or misspelled operator is reported as such.
        let op = match toks.get(pos) {
            Some(Tok::Gt) => RuleOp::Beats(true),
            Some(Tok::Lt) => RuleOp::Beats(false),
            Some(Tok::Word(w)) => match Property::parse(w) {
                Some(by) => RuleOp::Compare(by),
                None => bail!("unknown property {w:?} in {stmt:?} (use mtime, size or name)"),
            },
            _ => bail!("expected >, < or a property (mtime, size, name) between labels in {stmt:?}"),
        };
        pos += 1;
        // Parse right-hand label set.
        let rhs = parse_set(&toks, &mut pos).map_err(|e| anyhow::anyhow!("{e} in {stmt:?}"))?;
        if pos != toks.len() {
            bail!("unexpected trailing text in {stmt:?}");
        }
        // Construct pairwise rules for each combination of left and right labels.
        for l in &lhs {
            for r in &rhs {
                let rule = match op {
                    RuleOp::Beats(left_wins) => {
                        if l == r {
                            bail!("label {l:?} is ordered against itself in {stmt:?}: use a property rule for refs of one label");
                        }
                        let (winner, loser) = if left_wins { (l, r) } else { (r, l) };
                        Rule::Beats { winner: winner.clone(), loser: loser.clone() }
                    }
                    RuleOp::Compare(by) => Rule::Compare { pair: Some((l.clone(), r.clone())), by },
                };
                rules.push(rule);
            }
        }
    }
    Ok(rules)
}

/// Operator of a two-sided conflict statement, resolved before the right-hand side is read.
#[derive(Clone, Copy)]
enum RuleOp {
    /// `>` when true, `<` when false.
    Beats(bool),
    Compare(Property),
}

/// Collect replica labels referenced across conflict rules.
pub fn rule_labels(rules: &[Rule]) -> BTreeSet<&str> {
    rules.iter().flat_map(|r| match r {
        Rule::Beats { winner, loser } => vec![winner.as_str(), loser.as_str()],
        Rule::Compare { pair: Some((a, b)), .. } => vec![a.as_str(), b.as_str()],
        Rule::Compare { pair: None, .. } => vec![],
    }).collect()
}

/// Split input text by semicolons, filtering out empty entries.
fn statements(text: &str) -> impl Iterator<Item = &str> {
    text.split(';').map(str::trim).filter(|s| !s.is_empty())
}

/// Lexical tokens parsed from policy statements.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Open,
    Close,
    Comma,
    Right,
    Left,
    Both,
    Gt,
    Lt,
    Word(String),
}

/// Tokenize statement text into punctuation marks, directional arrows, and word tokens.
/// Word tokens absorb remaining characters like hyphens or dollar signs. Every input tokenizes.
fn tokenize(s: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let c = rest.chars().next().unwrap();
        let (tok, len) = if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
            continue;
        } else if let Some(len) = arrow_len(rest) {
            (match len { 3 => Tok::Both, 2 if rest.starts_with("->") => Tok::Right, _ => Tok::Left }, len)
        } else {
            match c {
                '{' => (Tok::Open, 1),
                '}' => (Tok::Close, 1),
                ',' => (Tok::Comma, 1),
                '>' => (Tok::Gt, 1),
                '<' => (Tok::Lt, 1),
                _ => {
                    // Consume characters until encountering whitespace, delimiters, or arrows.
                    let mut end = 0;
                    for (i, ch) in rest.char_indices() {
                        if ch.is_whitespace() || matches!(ch, '{' | '}' | ',' | '>' | '<') || arrow_len(&rest[i..]).is_some() {
                            break;
                        }
                        end = i + ch.len_utf8();
                    }
                    (Tok::Word(rest[..end].to_string()), end)
                }
            }
        };
        toks.push(tok);
        rest = &rest[len..];
    }
    toks
}

/// Measure the prefix byte length when the string begins with a directional arrow.
fn arrow_len(s: &str) -> Option<usize> {
    if s.starts_with("<->") {
        Some(3)
    } else if s.starts_with("->") || s.starts_with("<-") {
        Some(2)
    } else {
        None
    }
}

/// Parse a single label or a comma-delimited braced label set from the token stream.
fn parse_set(toks: &[Tok], pos: &mut usize) -> std::result::Result<Vec<String>, String> {
    match toks.get(*pos) {
        Some(Tok::Word(w)) => {
            *pos += 1;
            Ok(vec![w.clone()])
        }
        // Parse a comma-separated label group enclosed in curly braces.
        Some(Tok::Open) => {
            *pos += 1;
            let mut out = Vec::new();
            loop {
                match toks.get(*pos) {
                    Some(Tok::Word(w)) => out.push(w.clone()),
                    _ => return Err("expected a label inside braces".into()),
                }
                *pos += 1;
                match toks.get(*pos) {
                    Some(Tok::Comma) => *pos += 1,
                    Some(Tok::Close) => {
                        *pos += 1;
                        return Ok(out);
                    }
                    _ => return Err("expected , or } inside braces".into()),
                }
            }
        }
        _ => Err("expected a label or {labels}".into()),
    }
}

/// Replication policy controlling state propagation graphs and conflict resolution rules.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    pub change: Graph,
    pub create: Graph,
    pub delete: Graph,
    pub conflict: Vec<Rule>,
}

/// Target state assigned to a replica slot, representing content updates or absence.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Content { from: String, hash: [u8; 32] },
    Absent,
}

impl Target {
    /// Yield the content hash for comparison, returning None for absent targets.
    fn key(&self) -> Option<[u8; 32]> {
        match self {
            Target::Content { hash, .. } => Some(*hash),
            Target::Absent => None,
        }
    }
}

/// Candidate proposal pairing a source slot reference with its target state.
struct Candidate<'a> {
    slot: &'a Slot,
    target: Target,
}

impl Policy {
    /// Evaluate instance slots against propagation graphs and conflict rules to determine replica operations.
    pub fn decide(&self, inst: &Instance) -> Decision {
        let established = inst.slots.iter().any(|s| s.record.is_some());
        // Derive target proposals from local slot changes.
        let proposal = |s: &Slot| -> Option<Target> {
            let content = || s.snapshot().map(|snap| Target::Content { from: s.ref_id.clone(), hash: snap.hash });
            match s.change() {
                Change::Edited => content(),
                Change::Appeared if !established => content(),
                Change::Disappeared => Some(Target::Absent),
                _ => None,
            }
        };

        let unreadable: Vec<&Slot> = inst.slots.iter().filter(|s| matches!(s.status, Status::Unreadable(_))).collect();
        let mut ops = Vec::new();
        let mut skipped = Vec::new();
        for t in &inst.slots {
            // An unreadable slot neither donates nor receives. Mark it unresolved and let the readable slots sync among themselves.
            if let Status::Unreadable(e) = &t.status {
                skipped.push((t.ref_id.clone(), format!("unreadable: {e}")));
                continue;
            }
            let own = proposal(t);
            // A proposal that would reach an unreadable peer cannot be delivered yet. Hold the proposing slot too, so its record keeps the
            // old baseline and the edit (or deletion) is still pending, and still a conflict if the peer comes back edited.
            if let Some(p) = &own {
                if let Some(u) = unreadable.iter().find(|u| self.could_reach(t, p, u)) {
                    skipped.push((t.ref_id.clone(), format!("waiting on {}: unreadable", u.ref_id)));
                    continue;
                }
            }
            let others = || inst.slots.iter().filter(|s| s.ref_id != t.ref_id);
            // Gather candidates reaching this target slot from peer replicas.
            let mut candidates: Vec<Candidate> = others()
                .filter_map(|s| {
                    let p = proposal(s)?;
                    self.reaches(s, &p, t).then_some(Candidate { slot: s, target: p })
                })
                .collect();
            if !candidates.is_empty() {
                if let Some(p) = &own {
                    // Include the target slot itself when propagation runs symmetrically.
                    if candidates.iter().any(|c| self.reaches(t, p, c.slot)) {
                        candidates.push(Candidate { slot: t, target: p.clone() });
                    }
                }
            } else {
                let heard = own.as_ref().is_some_and(|p| others().any(|s| self.reaches(t, p, s)));
                if !heard {
                    // Fall back to settled peer slots when local changes remain unpropagated.
                    candidates = others()
                        .filter(|d| d.settled())
                        .filter_map(|d| {
                            let p = proposal_from(d);
                            self.reaches(d, &p, t).then_some(Candidate { slot: d, target: p })
                        })
                        .collect();
                }
            }
            if candidates.is_empty() {
                continue;
            }
            // Resolve competing proposals using conflict rules.
            match self.choose(&candidates) {
                Ok(target) => ops.extend(op_for(t, &target)),
                Err(reason) => skipped.push((t.ref_id.clone(), reason)),
            }
        }
        Decision::Plan { ops, skipped }
    }

    /// Check whether a proposal would propagate to a slot whose presence is unknown because it is unreadable.
    /// Content may travel along change or create edges; absence travels along delete edges.
    fn could_reach(&self, from: &Slot, proposal: &Target, to: &Slot) -> bool {
        match proposal {
            Target::Absent => self.delete.reaches(&from.label, &to.label),
            Target::Content { .. } => self.change.reaches(&from.label, &to.label) || self.create.reaches(&from.label, &to.label),
        }
    }

    /// Check whether a proposal propagates from a source slot to a target slot in the corresponding policy graph.
    fn reaches(&self, from: &Slot, proposal: &Target, to: &Slot) -> bool {
        // Select the graph corresponding to proposal state and destination presence.
        let graph = match (proposal, &to.status) {
            (Target::Absent, Status::Present(_)) => &self.delete,
            (Target::Absent, _) => return false,
            (Target::Content { .. }, Status::Present(_)) => &self.change,
            (Target::Content { .. }, _) => &self.create,
        };
        graph.reaches(&from.label, &to.label)
    }

    /// Select the winning target among candidates or report an unresolved conflict.
    fn choose(&self, candidates: &[Candidate]) -> std::result::Result<Target, String> {
        let distinct: BTreeSet<Option<[u8; 32]>> = candidates.iter().map(|c| c.target.key()).collect();
        if distinct.len() == 1 {
            return Ok(candidates[0].target.clone());
        }
        // Accept a candidate beating every conflicting peer in pairwise evaluation.
        for w in candidates {
            let wins = candidates.iter()
                .filter(|o| o.target.key() != w.target.key())
                .all(|o| self.beats(w.slot, o.slot));
            if wins {
                return Ok(w.target.clone());
            }
        }
        let names: Vec<&str> = candidates.iter().map(|c| c.slot.ref_id.as_str()).collect();
        Err(format!("conflict: {} (no rule decides)", names.join(" vs ")))
    }

    /// Compare `w` against `o` across configured conflict rules.
    fn beats(&self, w: &Slot, o: &Slot) -> bool {
        for rule in &self.conflict {
            match rule {
                Rule::Beats { winner, loser } => {
                    // Match explicit winner and loser labels.
                    if *winner == w.label && *loser == o.label {
                        return true;
                    }
                    if *winner == o.label && *loser == w.label {
                        return false;
                    }
                }
                Rule::Compare { pair, by } => {
                    // Match property comparisons for this label pair or global rules.
                    let covers = match pair {
                        None => true,
                        Some((x, y)) => (*x == w.label && *y == o.label) || (*x == o.label && *y == w.label),
                    };
                    // A tie on the property leaves the question to the next rule.
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
}

/// Build a content target referencing the settled slot snapshot.
fn proposal_from(d: &Slot) -> Target {
    Target::Content { from: d.ref_id.clone(), hash: d.snapshot().expect("settled slots are present").hash }
}

/// Compare two slots by the selected property, preferring present files over missing files.
/// Return None when the property ties, so the caller can fall through to the next rule.
fn property_beats(by: Property, w: &Slot, o: &Slot) -> Option<bool> {
    use std::cmp::Ordering;
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

/// Determine the synchronization operation needed to update slot `t` to match `target`.
fn op_for(t: &Slot, target: &Target) -> Option<Op> {
    match (target, &t.status) {
        (Target::Content { from, hash }, Status::Present(snap)) => {
            (snap.hash != *hash).then(|| Op::Copy { from: from.clone(), to: t.ref_id.clone() })
        }
        (Target::Content { from, .. }, Status::Missing) => Some(Op::Copy { from: from.clone(), to: t.ref_id.clone() }),
        (Target::Absent, Status::Present(_)) => Some(Op::Delete { at: t.ref_id.clone() }),
        (Target::Absent, Status::Missing) => None,
        (_, Status::Unreadable(_)) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// Build a snapshot with digest bytes set to `hash` and timestamp at `mtime_s` seconds past unix epoch.
    fn snap(hash: u8, mtime_s: u64) -> Snapshot {
        Snapshot {
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_s),
            size: 1,
            hash: [hash; 32],
        }
    }

    /// Build a test slot with label, snapshot record, and status under `ref_id`.
    fn slot(ref_id: &str, label: &str, record: Option<Snapshot>, status: Status) -> Slot {
        Slot { ref_id: ref_id.into(), label: label.into(), path: PathBuf::from(format!("/{ref_id}/f.txt")), record, status }
    }

    /// Create a test instance across the given slots.
    fn inst(slots: Vec<Slot>) -> Instance {
        Instance { slots }
    }

    /// Parse propagation graph strings and conflict rules into a policy.
    fn policy(change: &str, create: &str, delete: &str, conflict: &str) -> Policy {
        Policy {
            change: Graph::parse(change).unwrap(),
            create: Graph::parse(create).unwrap(),
            delete: Graph::parse(delete).unwrap(),
            conflict: parse_rules(conflict).unwrap(),
        }
    }

    /// Wrap planned operations in an unskipped decision.
    fn plan(ops: Vec<Op>) -> Decision {
        Decision::Plan { ops, skipped: vec![] }
    }

    /// Construct a copy operation between two slot references.
    fn copy(from: &str, to: &str) -> Op {
        Op::Copy { from: from.into(), to: to.into() }
    }

    /// Extract skipped slot identifiers from a planned decision.
    fn skipped_at(d: &Decision) -> Vec<String> {
        match d {
            Decision::Plan { skipped, .. } => skipped.iter().map(|(id, _)| id.clone()).collect(),
            Decision::Skip(_) => panic!("whole-instance skip"),
        }
    }

    /// Test reachability closure, bidirectional edges, set expansions, and parsing rules.
    #[test]
    fn graph_parses_sets_arrows_and_closure() {
        let g = Graph::parse("a -> b; {a, b} -> c").unwrap();
        assert!(g.reaches("a", "b") && g.reaches("a", "c") && g.reaches("b", "c"));
        assert!(!g.reaches("b", "a") && !g.reaches("c", "a"));
        let chain = Graph::parse("a -> b; b -> c").unwrap();
        assert!(chain.reaches("a", "c"), "reachability is transitive");
        let both = Graph::parse("x<->y").unwrap();
        assert!(both.reaches("x", "y") && both.reaches("y", "x"));
        let rev = Graph::parse("x <- {y, z}").unwrap();
        assert!(rev.reaches("y", "x") && rev.reaches("z", "x") && !rev.reaches("x", "y"));
        let cyc = Graph::parse("a -> b; b -> a").unwrap();
        assert!(cyc.reaches("a", "a"), "a cycle lets a label reach its own other refs");
        assert!(Graph::parse("").unwrap().is_empty());
        assert!(Graph::parse("a b").is_err());
        assert!(Graph::parse("a -> {b,}").is_err());
        assert_eq!(Graph::parse("$backup -> my-dir").unwrap().labels(), BTreeSet::from(["$backup", "my-dir"]));
    }

    /// Parse precedence inequalities, property comparisons, and global fallbacks.
    #[test]
    fn rules_parse_orderings_and_properties() {
        let rules = parse_rules("a > b; c < d; {a, b} mtime c; size").unwrap();
        assert_eq!(rules[0], Rule::Beats { winner: "a".into(), loser: "b".into() });
        assert_eq!(rules[1], Rule::Beats { winner: "d".into(), loser: "c".into() });
        assert_eq!(rules[2], Rule::Compare { pair: Some(("a".into(), "c".into())), by: Property::Mtime });
        assert_eq!(rules[3], Rule::Compare { pair: Some(("b".into(), "c".into())), by: Property::Mtime });
        assert_eq!(rules[4], Rule::Compare { pair: None, by: Property::Size });
        assert!(parse_rules("a > a").is_err());
        assert!(parse_rules("a newer b").is_err());
        assert!(parse_rules("a -> b").is_err());
    }

    /// Parser errors name the thing that is wrong: the operator, the property, or the label.
    #[test]
    fn parser_errors_name_the_missing_piece() {
        let err = parse_rules("{a, b}").unwrap_err().to_string();
        assert!(err.contains("expected >, < or a property"), "{err}");
        let err = parse_rules("a newer b").unwrap_err().to_string();
        assert!(err.contains("unknown property \"newer\""), "{err}");
        let err = parse_rules("a >").unwrap_err().to_string();
        assert!(err.contains("expected a label"), "{err}");
        let err = parse_rules("a").unwrap_err().to_string();
        assert!(err.contains("expected a property"), "{err}");
        let err = Graph::parse("a b").unwrap_err().to_string();
        assert!(err.contains("expected ->, <- or <->"), "{err}");
        let err = Graph::parse("a ->").unwrap_err().to_string();
        assert!(err.contains("expected a label"), "{err}");
    }

    /// A tie on one property falls through to the next rule instead of settling the conflict as a loss.
    #[test]
    fn property_tie_falls_through_to_the_next_rule() {
        let i = inst(vec![
            slot("a", "a", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("b", "b", Some(snap(1, 100)), Status::Present(snap(3, 300))),
        ]);
        // Both files have size 1, so `size` ties and `mtime` decides for b.
        assert_eq!(policy("a <-> b", "", "", "size; mtime").decide(&i), plan(vec![copy("b", "a")]));
        assert_eq!(policy("a <-> b", "", "", "size; a > b").decide(&i), plan(vec![copy("a", "b")]));
        // With nothing after the tie, the conflict stays open.
        assert_eq!(skipped_at(&policy("a <-> b", "", "", "size").decide(&i)), vec!["a", "b"]);
    }

    /// An unreadable slot is marked unresolved on its own while the readable slots keep syncing,
    /// and a slot whose proposal cannot reach the unreadable peer is held so its baseline does not move.
    #[test]
    fn unreadable_slot_is_skipped_alone_and_undelivered_proposals_are_held() {
        let i = inst(vec![
            slot("a", "a", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("b", "b", Some(snap(1, 100)), Status::Unreadable("is a directory".into())),
            slot("c", "c", Some(snap(1, 100)), Status::Present(snap(1, 100))),
        ]);
        // a's edit reaches b, so a is held; c still receives the copy.
        let d = policy("a -> {b, c}", "", "", "").decide(&i);
        assert_eq!(d, Decision::Plan { ops: vec![copy("a", "c")], skipped: vec![
            ("a".into(), "waiting on b: unreadable".into()),
            ("b".into(), "unreadable: is a directory".into()),
        ] });
        // Under a one-way edge from c to a, a's edit never goes to b, so a is not held and its stray edit reverts to c as usual.
        let d = policy("c -> a", "", "", "").decide(&i);
        assert_eq!(d, Decision::Plan { ops: vec![copy("c", "a")], skipped: vec![("b".into(), "unreadable: is a directory".into())] });
        // An unreadable slot never donates, even under a two-way edge, and a settled slot has nothing to hold.
        let d = policy("a <-> b", "", "", "").decide(&inst(vec![
            slot("a", "a", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("b", "b", Some(snap(1, 100)), Status::Unreadable("denied".into())),
        ]));
        assert_eq!(d, Decision::Plan { ops: vec![], skipped: vec![("b".into(), "unreadable: denied".into())] });
        // A deletion that would travel to the unreadable peer along a delete edge is held as well.
        let d = policy("a <-> b", "", "a -> b", "").decide(&inst(vec![
            slot("a", "a", Some(snap(1, 100)), Status::Missing),
            slot("b", "b", Some(snap(1, 100)), Status::Unreadable("denied".into())),
        ]));
        assert_eq!(d, Decision::Plan { ops: vec![], skipped: vec![
            ("a".into(), "waiting on b: unreadable".into()),
            ("b".into(), "unreadable: denied".into()),
        ] });
    }

    /// Propagate a unilateral change across peers regardless of timestamps.
    #[test]
    fn unilateral_edit_wins_regardless_of_mtime() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(2, 50))),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(1, 100))),
        ]);
        let d = policy("source <-> dest", "", "", "").decide(&i);
        assert_eq!(d, plan(vec![copy("source", "dest")]));
    }

    /// Skip concurrent edits lacking rules, resolving them once mtime or precedence rules apply.
    #[test]
    fn concurrent_differing_edits_skip_without_a_rule_and_mtime_breaks_tie() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(3, 300))),
        ]);
        let d = policy("source <-> dest", "", "", "").decide(&i);
        assert_eq!(skipped_at(&d), vec!["source", "dest"]);
        let d = policy("source <-> dest", "", "", "source mtime dest").decide(&i);
        assert_eq!(d, plan(vec![copy("dest", "source")]));
        let d = policy("source <-> dest", "", "", "source > dest").decide(&i);
        assert_eq!(d, plan(vec![copy("source", "dest")]));
    }

    /// Overwrite downstream slots along directed edges without flagging conflicts.
    #[test]
    fn one_way_edge_overwrites_silently() {
        let both = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(3, 300))),
        ]);
        assert_eq!(policy("source -> dest", "", "", "").decide(&both), plan(vec![copy("source", "dest")]));
        let lone = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(3, 300))),
        ]);
        assert_eq!(policy("source -> dest", "", "", "").decide(&lone), plan(vec![copy("source", "dest")]));
    }

    #[test]
    fn three_nodes_conflict_only_where_paths_meet() {
        let i = inst(vec![
            slot("a", "a", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("b", "b", Some(snap(1, 100)), Status::Present(snap(3, 300))),
            slot("c", "c", Some(snap(1, 100)), Status::Present(snap(1, 100))),
        ]);
        let d = policy("a -> b; {a, b} -> c", "", "", "").decide(&i);
        assert_eq!(d, Decision::Plan { ops: vec![copy("a", "b")], skipped: vec![("c".into(), "conflict: a vs b (no rule decides)".into())] });
        let d = policy("a -> b; {a, b} -> c", "", "", "a > b").decide(&i);
        assert_eq!(d, plan(vec![copy("a", "b"), copy("a", "c")]));
        let d = policy("a -> b; {a, b} -> c", "", "", "b > a").decide(&i);
        assert_eq!(d, plan(vec![copy("a", "b"), copy("b", "c")]));
    }

    #[test]
    fn chain_propagates_transitively() {
        let i = inst(vec![
            slot("a", "a", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("b", "b", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("c", "c", Some(snap(1, 100)), Status::Present(snap(1, 100))),
        ]);
        assert_eq!(policy("a -> b; b -> c", "", "", "").decide(&i), plan(vec![copy("a", "b"), copy("a", "c")]));
    }

    #[test]
    fn convergent_concurrent_edits_commit_with_no_ops() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(2, 300))),
        ]);
        assert_eq!(policy("source <-> dest", "", "", "").decide(&i), plan(vec![]));
    }

    #[test]
    fn late_join_conforms_instead_of_proposing() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("dest", "dest", None, Status::Present(snap(9, 999))),
        ]);
        assert_eq!(policy("source <-> dest", "", "", "").decide(&i), plan(vec![copy("source", "dest")]));
        assert_eq!(policy("dest -> source", "", "", "").decide(&i), plan(vec![]));
    }

    #[test]
    fn first_run_seeds_identical_content_and_contests_differing() {
        let same = inst(vec![
            slot("source", "source", None, Status::Present(snap(1, 100))),
            slot("dest", "dest", None, Status::Present(snap(1, 200))),
        ]);
        assert_eq!(policy("source <-> dest", "", "", "").decide(&same), plan(vec![]));

        let differ = inst(vec![
            slot("source", "source", None, Status::Present(snap(1, 100))),
            slot("dest", "dest", None, Status::Present(snap(2, 200))),
        ]);
        assert_eq!(skipped_at(&policy("source <-> dest", "", "", "").decide(&differ)), vec!["source", "dest"]);
        let d = policy("source <-> dest", "", "", "mtime").decide(&differ);
        assert_eq!(d, plan(vec![copy("dest", "source")]));
    }

    #[test]
    fn creation_needs_a_create_edge() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("dest", "dest", None, Status::Missing),
        ]);
        assert_eq!(policy("source <-> dest", "", "", "").decide(&i), plan(vec![]));
        assert_eq!(policy("source <-> dest", "source -> dest", "", "").decide(&i), plan(vec![copy("source", "dest")]));
        let settled = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("dest", "dest", None, Status::Missing),
        ]);
        assert_eq!(policy("", "source -> dest", "", "").decide(&settled), plan(vec![copy("source", "dest")]));
    }

    #[test]
    fn deletion_propagates_along_delete_edges_or_is_restored_through_create_edges() {
        let deleted = || inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Missing),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(1, 100))),
        ]);
        let restore = policy("source <-> dest", "dest -> source", "", "");
        assert_eq!(restore.decide(&deleted()), plan(vec![copy("dest", "source")]));

        let propagate = policy("source <-> dest", "", "source -> dest", "");
        assert_eq!(propagate.decide(&deleted()), plan(vec![Op::Delete { at: "dest".into() }]));

        let leave = policy("source <-> dest", "", "", "");
        assert_eq!(leave.decide(&deleted()), plan(vec![]));
    }

    #[test]
    fn edit_against_deletion_is_a_conflict_where_the_file_beats_the_absence_under_mtime() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Missing),
            slot("dest", "dest", Some(snap(1, 100)), Status::Present(snap(2, 200))),
        ]);
        let contested = policy("source <-> dest", "dest -> source", "dest <-> source", "");
        assert_eq!(skipped_at(&contested.decide(&i)), vec!["source", "dest"]);
        let newer = policy("source <-> dest", "dest -> source", "dest <-> source", "mtime");
        assert_eq!(newer.decide(&i), plan(vec![copy("dest", "source")]));
        let one_way = policy("source <-> dest", "", "source -> dest", "");
        assert_eq!(one_way.decide(&i), plan(vec![Op::Delete { at: "dest".into() }]));
    }

    #[test]
    fn mirror_overwrites_dest_changes_and_fans_out() {
        let policy = policy("source -> dest", "source -> dest", "", "");
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("dest?project=a", "dest", Some(snap(1, 100)), Status::Present(snap(2, 999))),
            slot("dest?project=b", "dest", None, Status::Missing),
        ]);
        assert_eq!(policy.decide(&i), plan(vec![copy("source", "dest?project=a"), copy("source", "dest?project=b")]));
    }

    #[test]
    fn refs_of_one_label_sync_through_a_cycle() {
        let i = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("dest?root=/x", "dest", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("dest?root=/y", "dest", Some(snap(1, 100)), Status::Present(snap(1, 100))),
        ]);
        let d = policy("source <-> dest", "", "", "").decide(&i);
        assert_eq!(d, plan(vec![copy("dest?root=/x", "source"), copy("dest?root=/x", "dest?root=/y")]));
        let both = inst(vec![
            slot("source", "source", Some(snap(1, 100)), Status::Present(snap(1, 100))),
            slot("dest?root=/x", "dest", Some(snap(1, 100)), Status::Present(snap(2, 200))),
            slot("dest?root=/y", "dest", Some(snap(1, 100)), Status::Present(snap(3, 300))),
        ]);
        assert_eq!(skipped_at(&policy("source <-> dest", "", "", "").decide(&both)), vec!["source", "dest?root=/x", "dest?root=/y"]);
        let d = policy("source <-> dest", "", "", "dest mtime dest").decide(&both);
        assert_eq!(d, plan(vec![copy("dest?root=/y", "source"), copy("dest?root=/y", "dest?root=/x")]));
    }

    #[test]
    fn size_and_name_properties() {
        let big = Snapshot { size: 10, ..snap(2, 100) };
        let i = inst(vec![
            slot("a", "a", Some(snap(1, 100)), Status::Present(big)),
            slot("b", "b", Some(snap(1, 100)), Status::Present(snap(3, 100))),
        ]);
        assert_eq!(policy("a <-> b", "", "", "size").decide(&i), plan(vec![copy("a", "b")]));
        let mut named = i.clone();
        named.slots[0].path = "/a/zeta.txt".into();
        named.slots[1].path = "/b/alpha.txt".into();
        assert_eq!(policy("a <-> b", "", "", "a name b").decide(&named), plan(vec![copy("b", "a")]));
        assert_eq!(skipped_at(&policy("a <-> b", "", "", "mtime").decide(&i)), vec!["a", "b"]);
    }
}
