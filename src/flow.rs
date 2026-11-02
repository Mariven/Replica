//! The flow of a relation: which kinds of transition travel between which sites, and how disagreements are settled.
//!
//! Flow statements are separated by `;`. Each relates a site or a braced set to another through an arrow with the carried kinds in brackets:
//!
//!     source <-> dest [edit create]
//!     source -> dest [all]
//!     {a, b} -> c [edit]
//!     backup <- source
//!
//! An arrow without brackets carries `edit` and `create`. Carriage is transitive per kind.
//! Resolver statements are `a > b` (a wins), `a < b`, `a mtime b` (newer wins), `a size b` (larger wins), `a name b` (alphabetically earlier file name wins), or a bare property for all pairs. Rules apply in order; a property tie falls through to the next rule.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use anyhow::{Result, bail};

/// The kinds of transition an edge can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Content to a replica that already holds the file.
    Edit,
    /// Content to a replica missing the file, including a restore after a deletion there.
    Create,
    /// Absence to a replica holding the file.
    Delete,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Edit, Kind::Create, Kind::Delete];

    /// The keyword as written in brackets.
    pub fn word(self) -> &'static str {
        match self {
            Kind::Edit => "edit",
            Kind::Create => "create",
            Kind::Delete => "delete",
        }
    }

    /// Parse a bracket keyword.
    fn parse(word: &str) -> Option<Kind> {
        match word {
            "edit" | "edits" | "change" | "changes" => Some(Kind::Edit),
            "create" | "creates" | "creation" => Some(Kind::Create),
            "delete" | "deletes" | "deletion" => Some(Kind::Delete),
            _ => None,
        }
    }
}

/// A directed edge between two sites with the kinds it carries.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kinds: BTreeSet<Kind>,
}

/// The flow graph with its transitive closure per kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Flow {
    edges: Vec<Edge>,
    /// For each kind, the sites each site can carry that kind to, through any number of edges.
    reach: BTreeMap<Kind, BTreeMap<String, BTreeSet<String>>>,
}

impl Flow {
    /// Parse flow statements.
    pub fn parse(text: &str) -> Result<Flow> {
        let mut edges: Vec<Edge> = Vec::new();
        for stmt in statements(text) {
            let toks = tokenize(stmt);
            let mut pos = 0;
            let lhs = parse_set(&toks, &mut pos).map_err(|e| anyhow::anyhow!("{e} in {stmt:?}"))?;
            let arrow = match toks.get(pos) {
                Some(t @ (Tok::Right | Tok::Left | Tok::Both)) => t.clone(),
                _ => bail!("expected ->, <- or <-> in {stmt:?}"),
            };
            pos += 1;
            let rhs = parse_set(&toks, &mut pos).map_err(|e| anyhow::anyhow!("{e} in {stmt:?}"))?;
            let kinds = parse_kinds(&toks, &mut pos).map_err(|e| anyhow::anyhow!("{e} in {stmt:?}"))?;
            if pos != toks.len() {
                bail!("unexpected trailing text in {stmt:?}");
            }
            for l in &lhs {
                for r in &rhs {
                    if matches!(arrow, Tok::Right | Tok::Both) {
                        add_edge(&mut edges, l, r, &kinds);
                    }
                    if matches!(arrow, Tok::Left | Tok::Both) {
                        add_edge(&mut edges, r, l, &kinds);
                    }
                }
            }
        }
        let mut flow = Flow { edges, reach: BTreeMap::new() };
        flow.close();
        Ok(flow)
    }

    /// Compute, per kind, which sites each site can carry that kind to.
    fn close(&mut self) {
        for kind in Kind::ALL {
            let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            let edges: Vec<(&str, &str)> = self.edges.iter().filter(|e| e.kinds.contains(&kind)).map(|e| (e.from.as_str(), e.to.as_str())).collect();
            for (from, _) in &edges {
                let mut seen = BTreeSet::new();
                let mut stack = vec![*from];
                while let Some(cur) = stack.pop() {
                    for (a, b) in &edges {
                        if *a == cur && seen.insert((*b).to_owned()) {
                            stack.push(b);
                        }
                    }
                }
                out.insert((*from).to_owned(), seen);
            }
            self.reach.insert(kind, out);
        }
    }

    /// True when the flow carries `kind` from one site to another, directly or through other sites.
    pub fn carries(&self, kind: Kind, from: &str, to: &str) -> bool {
        self.reach.get(&kind).and_then(|m| m.get(from)).is_some_and(|set| set.contains(to))
    }

    /// True when the flow carries nothing at all.
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// The edges as parsed and merged.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// The sites mentioned by any edge.
    pub fn sites(&self) -> BTreeSet<&str> {
        self.edges.iter().flat_map(|e| [e.from.as_str(), e.to.as_str()]).collect()
    }

    /// The sites each site can carry `kind` to, for the explain command.
    pub fn closure(&self, kind: Kind) -> &BTreeMap<String, BTreeSet<String>> {
        self.reach.get(&kind).expect("closure is computed for each kind")
    }

    /// True when any edge carries `kind` into `site`.
    pub fn has_incoming(&self, kind: Kind, site: &str) -> bool {
        self.edges.iter().any(|e| e.to == site && e.kinds.contains(&kind))
    }

    /// True when any edge carries `kind` out of `site`.
    pub fn has_outgoing(&self, kind: Kind, site: &str) -> bool {
        self.edges.iter().any(|e| e.from == site && e.kinds.contains(&kind))
    }
}

impl fmt::Display for Flow {
    /// Render one statement per edge, with the kinds in brackets.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.edges.iter().map(|e| {
            let kinds: Vec<&str> = e.kinds.iter().map(|k| k.word()).collect();
            format!("{} -> {} [{}]", e.from, e.to, kinds.join(" "))
        }).collect();
        f.write_str(&parts.join("; "))
    }
}

/// Merge an edge into the list, uniting kinds when the same pair already exists.
fn add_edge(edges: &mut Vec<Edge>, from: &str, to: &str, kinds: &BTreeSet<Kind>) {
    match edges.iter_mut().find(|e| e.from == from && e.to == to) {
        Some(e) => e.kinds.extend(kinds.iter().copied()),
        None => edges.push(Edge { from: from.to_owned(), to: to.to_owned(), kinds: kinds.clone() }),
    }
}

/// Parse the optional bracketed kind list after the right-hand side. Without brackets the edge carries edit and create.
fn parse_kinds(toks: &[Tok], pos: &mut usize) -> std::result::Result<BTreeSet<Kind>, String> {
    let Some(Tok::Bracket) = toks.get(*pos) else {
        return Ok(BTreeSet::from([Kind::Edit, Kind::Create]));
    };
    *pos += 1;
    let mut kinds = BTreeSet::new();
    loop {
        match toks.get(*pos) {
            Some(Tok::Word(w)) if w == "all" => kinds.extend(Kind::ALL),
            Some(Tok::Word(w)) => match Kind::parse(w) {
                Some(k) => { kinds.insert(k); }
                None => return Err(format!("unknown kind {w:?} (use edit, create, delete or all)")),
            },
            Some(Tok::Comma) => {}
            Some(Tok::EndBracket) => {
                *pos += 1;
                if kinds.is_empty() {
                    return Err("empty [] carries nothing; list edit, create, delete or all".into());
                }
                return Ok(kinds);
            }
            _ => return Err("expected a kind or ] inside brackets".into()),
        }
        *pos += 1;
    }
}

/// A property of a file compared by a resolver rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    /// Newer wins.
    Mtime,
    /// Larger wins.
    Size,
    /// Alphabetically earlier file name wins.
    Name,
}

impl Property {
    fn parse(word: &str) -> Option<Property> {
        match word {
            "mtime" | "newer" => Some(Property::Mtime),
            "size" | "larger" => Some(Property::Size),
            "name" => Some(Property::Name),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Property::Mtime => "mtime",
            Property::Size => "size",
            Property::Name => "name",
        }
    }
}

/// One resolver rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Content from `winner` beats content from `loser`.
    Beats { winner: String, loser: String },
    /// Compare a property between the sites of a pair, or between any pair when `pair` is None.
    Compare { pair: Option<(String, String)>, by: Property },
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rule::Beats { winner, loser } => write!(f, "{winner} > {loser}"),
            Rule::Compare { pair: Some((a, b)), by } => write!(f, "{a} {} {b}", by.word()),
            Rule::Compare { pair: None, by } => f.write_str(by.word()),
        }
    }
}

/// Parse resolver statements.
pub fn parse_rules(text: &str) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    for stmt in statements(text) {
        let toks = tokenize(stmt);
        if let [Tok::Word(w)] = toks.as_slice() {
            match Property::parse(w) {
                Some(by) => rules.push(Rule::Compare { pair: None, by }),
                None => bail!("expected a property (mtime, size, name) or a comparison in {stmt:?}"),
            }
            continue;
        }
        let mut pos = 0;
        let lhs = parse_set(&toks, &mut pos).map_err(|e| anyhow::anyhow!("{e} in {stmt:?}"))?;
        let op = match toks.get(pos) {
            Some(Tok::Gt) => RuleOp::Beats(true),
            Some(Tok::Lt) => RuleOp::Beats(false),
            Some(Tok::Word(w)) => match Property::parse(w) {
                Some(by) => RuleOp::Compare(by),
                None => bail!("unknown property {w:?} in {stmt:?} (use mtime, size or name)"),
            },
            _ => bail!("expected >, < or a property (mtime, size, name) between sites in {stmt:?}"),
        };
        pos += 1;
        let rhs = parse_set(&toks, &mut pos).map_err(|e| anyhow::anyhow!("{e} in {stmt:?}"))?;
        if pos != toks.len() {
            bail!("unexpected trailing text in {stmt:?}");
        }
        for l in &lhs {
            for r in &rhs {
                rules.push(match op {
                    RuleOp::Beats(left_wins) => {
                        if l == r {
                            bail!("site {l:?} is ordered against itself in {stmt:?}: use a property rule for replicas of one site");
                        }
                        let (winner, loser) = if left_wins { (l, r) } else { (r, l) };
                        Rule::Beats { winner: winner.clone(), loser: loser.clone() }
                    }
                    RuleOp::Compare(by) => Rule::Compare { pair: Some((l.clone(), r.clone())), by },
                });
            }
        }
    }
    Ok(rules)
}

/// The operator of a two-sided resolver statement.
#[derive(Clone, Copy)]
enum RuleOp {
    Beats(bool),
    Compare(Property),
}

/// The sites mentioned by resolver rules.
pub fn rule_sites(rules: &[Rule]) -> BTreeSet<&str> {
    rules.iter().flat_map(|r| match r {
        Rule::Beats { winner, loser } => vec![winner.as_str(), loser.as_str()],
        Rule::Compare { pair: Some((a, b)), .. } => vec![a.as_str(), b.as_str()],
        Rule::Compare { pair: None, .. } => vec![],
    }).collect()
}

/// The flow and resolver of a relation together.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    pub flow: Flow,
    pub resolve: Vec<Rule>,
}

impl Policy {
    /// Parse both texts.
    pub fn parse(flow: &str, resolve: &str) -> Result<Policy> {
        Ok(Policy { flow: Flow::parse(flow)?, resolve: parse_rules(resolve)? })
    }

    /// A stable text of the policy, hashed into the relation fingerprint so a policy change forces full re-evaluation.
    pub fn fingerprint_text(&self) -> String {
        let rules: Vec<String> = self.resolve.iter().map(|r| r.to_string()).collect();
        format!("{} ; resolve: {}", self.flow, rules.join("; "))
    }
}

/// Split text on semicolons, dropping blanks.
fn statements(text: &str) -> impl Iterator<Item = &str> {
    text.split(';').map(str::trim).filter(|s| !s.is_empty())
}

/// Tokens of flow and resolver statements.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Open,
    Close,
    Bracket,
    EndBracket,
    Comma,
    Right,
    Left,
    Both,
    Gt,
    Lt,
    Word(String),
}

/// Tokenize a statement. Words absorb any character outside whitespace, braces, brackets, commas, arrows, comparison signs.
fn tokenize(s: &str) -> Vec<Tok> {
    let mut toks = Vec::new();
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let (tok, len) = if let Some(len) = arrow_len(rest) {
            (match len { 3 => Tok::Both, 2 if rest.starts_with("->") => Tok::Right, _ => Tok::Left }, len)
        } else {
            match c {
                '{' => (Tok::Open, 1),
                '}' => (Tok::Close, 1),
                '[' => (Tok::Bracket, 1),
                ']' => (Tok::EndBracket, 1),
                ',' => (Tok::Comma, 1),
                '>' => (Tok::Gt, 1),
                '<' => (Tok::Lt, 1),
                _ => {
                    let mut end = 0;
                    for (i, ch) in rest.char_indices() {
                        if ch.is_whitespace() || matches!(ch, '{' | '}' | '[' | ']' | ',' | '>' | '<') || arrow_len(&rest[i..]).is_some() {
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

/// Byte length of an arrow at the start of the text, if any.
fn arrow_len(s: &str) -> Option<usize> {
    if s.starts_with("<->") {
        Some(3)
    } else if s.starts_with("->") || s.starts_with("<-") {
        Some(2)
    } else {
        None
    }
}

/// Parse a site name or a braced, comma-separated set of names.
fn parse_set(toks: &[Tok], pos: &mut usize) -> std::result::Result<Vec<String>, String> {
    match toks.get(*pos) {
        Some(Tok::Word(w)) => {
            *pos += 1;
            Ok(vec![w.clone()])
        }
        Some(Tok::Open) => {
            *pos += 1;
            let mut out = Vec::new();
            loop {
                match toks.get(*pos) {
                    Some(Tok::Word(w)) => out.push(w.clone()),
                    _ => return Err("expected a site inside braces".into()),
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
        _ => Err("expected a site or {sites}".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_parses_arrows_sets_kinds_and_closure() {
        let f = Flow::parse("a -> b; {a, b} -> c [edit]").unwrap();
        assert!(f.carries(Kind::Edit, "a", "b") && f.carries(Kind::Edit, "a", "c") && f.carries(Kind::Edit, "b", "c"));
        assert!(f.carries(Kind::Create, "a", "b"), "an arrow without brackets carries edit and create");
        assert!(!f.carries(Kind::Create, "b", "c"), "the bracketed edge carries edit alone");
        assert!(!f.carries(Kind::Delete, "a", "b"));
        assert!(!f.carries(Kind::Edit, "b", "a") && !f.carries(Kind::Edit, "c", "a"));
        let chain = Flow::parse("a -> b [edit]; b -> c [edit]").unwrap();
        assert!(chain.carries(Kind::Edit, "a", "c"), "carriage is transitive");
        let both = Flow::parse("x<->y[all]").unwrap();
        assert!(both.carries(Kind::Delete, "x", "y") && both.carries(Kind::Delete, "y", "x"));
        let rev = Flow::parse("x <- {y, z} [create, delete]").unwrap();
        assert!(rev.carries(Kind::Create, "y", "x") && rev.carries(Kind::Delete, "z", "x") && !rev.carries(Kind::Edit, "y", "x"));
        let cyc = Flow::parse("a -> b; b -> a").unwrap();
        assert!(cyc.carries(Kind::Edit, "a", "a"), "a cycle lets a site carry to its own other replicas");
        assert!(Flow::parse("").unwrap().is_empty());
        assert_eq!(Flow::parse("a -> b [edit]; a -> b [delete]").unwrap().edges().len(), 1, "repeated pairs merge their kinds");
        assert_eq!(Flow::parse("my-dir -> other_dir").unwrap().sites(), BTreeSet::from(["my-dir", "other_dir"]));
    }

    #[test]
    fn flow_errors_describe_the_missing_piece() {
        let err = |t: &str| Flow::parse(t).unwrap_err().to_string();
        assert!(err("a b").contains("expected ->, <- or <->"));
        assert!(err("a ->").contains("expected a site"));
        assert!(err("a -> {b,}").contains("inside braces"));
        assert!(err("a -> b [move]").contains("unknown kind"));
        assert!(err("a -> b []").contains("carries nothing"));
        assert!(err("a -> b [edit").contains("expected a kind or ]"));
        assert!(err("a -> b c").contains("trailing"));
    }

    #[test]
    fn rules_parse_orderings_and_properties() {
        let rules = parse_rules("a > b; c < d; {a, b} mtime c; size").unwrap();
        assert_eq!(rules[0], Rule::Beats { winner: "a".into(), loser: "b".into() });
        assert_eq!(rules[1], Rule::Beats { winner: "d".into(), loser: "c".into() });
        assert_eq!(rules[2], Rule::Compare { pair: Some(("a".into(), "c".into())), by: Property::Mtime });
        assert_eq!(rules[3], Rule::Compare { pair: Some(("b".into(), "c".into())), by: Property::Mtime });
        assert_eq!(rules[4], Rule::Compare { pair: None, by: Property::Size });
        assert!(parse_rules("a > a").is_err());
        assert!(parse_rules("a older b").is_err());
        assert!(parse_rules("a -> b").is_err());
        assert_eq!(rule_sites(&rules), BTreeSet::from(["a", "b", "c", "d"]));
        let err = parse_rules("{a, b}").unwrap_err().to_string();
        assert!(err.contains("expected >, < or a property"), "{err}");
    }

    #[test]
    fn display_round_trips_through_parse() {
        let f = Flow::parse("source <-> dest [edit create]; source -> dest [delete]").unwrap();
        let text = f.to_string();
        assert_eq!(text, "source -> dest [edit create delete]; dest -> source [edit create]");
        assert_eq!(Flow::parse(&text).unwrap(), f);
        let p = Policy::parse("a -> b", "a > b; mtime").unwrap();
        assert_eq!(p.fingerprint_text(), "a -> b [edit create] ; resolve: a > b; mtime");
    }
}
