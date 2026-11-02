//! Path patterns: absolute paths with variables in braces, such as `~/.claude/skills/{name}/SKILL.md` or `~/Projects/{project}/docs/{path...: **/*.md}`.
//!
//! The parser splits a pattern into segments once. To match a path, the engine reads a value for each variable out of the path and checks it against the variable's constraints; to substitute, it writes the values of a binding back into the pattern and gets a path.
//! The round trip is exact: when a path matches, substituting the resulting binding yields that same path. This is guaranteed because a pattern holds at most one multi-segment variable, a segment holds at most one variable, and the constraints are checked in both directions.
//! From the parsed form the engine also derives the directory where the walk starts, how deep the walk goes, the footprint for overlap checks, and the tail of a matched path for the ignore globs.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;

/// Bound variable values, by variable name.
pub type Binding = BTreeMap<String, String>;

/// A constraint on the value of a variable: either a glob, where `*` stays within a segment and `**` crosses segments, or an anchored regular expression.
#[derive(Clone, Debug)]
pub enum Constraint {
    Glob { text: String, matcher: GlobMatcher },
    Regex { text: String, regex: Regex },
    /// A constraint written with a leading `!`. It accepts a value exactly when the inner constraint rejects it.
    Not { text: String, inner: Box<Constraint> },
}

impl Constraint {
    /// Parse constraint text: `~re~` is a regular expression, anything else a glob.
    pub fn parse(text: &str) -> Result<Constraint> {
        let text = text.trim();
        if text.is_empty() {
            bail!("empty constraint");
        }
        if let Some(negated) = text.strip_prefix('!') {
            return Ok(Constraint::Not { text: text.to_owned(), inner: Box::new(Constraint::parse(negated)?) });
        }
        if let Some(inner) = text.strip_prefix('~').and_then(|t| t.strip_suffix('~')) {
            let regex = Regex::new(&format!("^(?:{inner})$")).with_context(|| format!("bad regular expression {inner:?}"))?;
            return Ok(Constraint::Regex { text: text.to_owned(), regex });
        }
        let matcher = GlobBuilder::new(text).literal_separator(true).build().with_context(|| format!("bad glob {text:?}"))?.compile_matcher();
        Ok(Constraint::Glob { text: text.to_owned(), matcher })
    }

    /// Check a bound value against the constraint.
    pub fn admits(&self, value: &str) -> bool {
        match self {
            Constraint::Glob { matcher, .. } => matcher.is_match(value),
            Constraint::Regex { regex, .. } => regex.is_match(value),
            Constraint::Not { inner, .. } => !inner.admits(value),
        }
    }

    /// The constraint as written.
    pub fn text(&self) -> &str {
        match self {
            Constraint::Glob { text, .. } | Constraint::Regex { text, .. } | Constraint::Not { text, .. } => text,
        }
    }

    /// The number of path segments spanned by a glob without `**` or a character class, so the walk can stop at that depth. None for other constraints, since they leave the depth unbounded.
    fn segment_count(&self) -> Option<usize> {
        match self {
            Constraint::Glob { text, .. } if !text.contains("**") && !text.contains('[') => Some(text.matches('/').count() + 1),
            _ => None,
        }
    }
}

impl PartialEq for Constraint {
    fn eq(&self, other: &Self) -> bool {
        self.text() == other.text()
    }
}
impl Eq for Constraint {}

/// A variable declared in a pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Var {
    pub name: String,
    /// True for `{name...}`, which binds one or more segments.
    pub multi: bool,
    /// Constraints from the pattern text and from a `where` block, all of which the value must satisfy.
    pub constraints: Vec<Constraint>,
}

impl Var {
    /// Check a value against each constraint of the variable.
    fn admits(&self, value: &str) -> bool {
        self.constraints.iter().all(|c| c.admits(value))
    }
}

/// A segment of a pattern: literal text around at most one variable.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    before: String,
    var: Option<usize>,
    after: String,
}

impl Segment {
    /// A segment with no variable.
    fn literal(text: &str) -> Segment {
        Segment { before: text.to_owned(), var: None, after: String::new() }
    }
}

/// A parsed path pattern.
#[derive(Clone, Debug)]
pub struct Pattern {
    text: String,
    segments: Vec<Segment>,
    vars: Vec<Var>,
    regex: Regex,
    root: PathBuf,
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.vars == other.vars
    }
}
impl Eq for Pattern {}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// Check that a variable name is an identifier: letters, digits, and underscores, with a letter or underscore first.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Pattern {
    /// Parse pattern text. A leading `~` is expanded to the home directory; the result must be absolute. A trailing `/` appends `{path...}`.
    pub fn parse(text: &str) -> Result<Pattern> {
        let expanded = shellexpand::tilde(text.trim()).into_owned();
        if !expanded.starts_with('/') {
            bail!("pattern {text:?} must be an absolute path or start with ~");
        }
        let body = match expanded.strip_suffix('/') {
            Some(dir) if !dir.is_empty() => format!("{dir}/{{path...}}"),
            _ => expanded.clone(),
        };
        let mut vars: Vec<Var> = Vec::new();
        let mut segments = Vec::new();
        for raw in split_segments(&body[1..]) {
            if raw.is_empty() {
                bail!("pattern {text:?} has an empty path segment");
            }
            let Some(open) = raw.find('{') else {
                segments.push(Segment::literal(raw));
                continue;
            };
            if raw[..open].contains('}') {
                bail!("stray }} in pattern {text:?}");
            }
            let close = matching_close(raw, open).with_context(|| format!("unclosed {{ in pattern {text:?}"))?;
            let after = &raw[close + 1..];
            if after.contains('{') {
                bail!("segment {raw:?} of pattern {text:?} holds two variables; a segment may hold one");
            }
            let inner = &raw[open + 1..close];
            let (spec, constraint) = match inner.find(':') {
                Some(i) => (&inner[..i], Some(&inner[i + 1..])),
                None => (inner, None),
            };
            let spec = spec.trim();
            let (name, multi) = match spec.strip_suffix("...") {
                Some(n) => (n.trim(), true),
                None => (spec, false),
            };
            if !valid_name(name) {
                bail!("bad variable name {name:?} in pattern {text:?}");
            }
            if vars.iter().any(|v| v.name == name) {
                bail!("variable {{{name}}} repeats in pattern {text:?}");
            }
            if vars.iter().any(|v| v.multi) {
                bail!("pattern {text:?} has a variable after its multi-segment variable; {{name...}} must be the last variable");
            }
            let constraints = match constraint {
                Some(c) => vec![Constraint::parse(c).with_context(|| format!("variable {{{name}}} in pattern {text:?}"))?],
                None => Vec::new(),
            };
            vars.push(Var { name: name.to_owned(), multi, constraints });
            segments.push(Segment { before: raw[..open].to_owned(), var: Some(vars.len() - 1), after: after.to_owned() });
        }
        if segments.is_empty() {
            bail!("pattern {text:?} is the root directory");
        }
        let regex = build_regex(&segments, &vars);
        let root = root_of(&segments);
        Ok(Pattern { text: body, segments, vars, regex, root })
    }

    /// The pattern text after `~` expansion, with a trailing `/` already rewritten as `/{path...}`.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The variables in order of appearance.
    pub fn vars(&self) -> &[Var] {
        &self.vars
    }

    /// The variable names as a set.
    pub fn var_names(&self) -> BTreeSet<&str> {
        self.vars.iter().map(|v| v.name.as_str()).collect()
    }

    /// Add a constraint to a variable, as a `where` block does. Fails when the pattern has no such variable.
    pub fn constrain(&mut self, name: &str, constraint: Constraint) -> Result<()> {
        let var = self.vars.iter_mut().find(|v| v.name == name).with_context(|| format!("pattern {} has no variable {{{name}}}", self.text))?;
        var.constraints.push(constraint);
        Ok(())
    }

    /// The directory where the walk for matching files begins. It is made of the literal segments before the first variable; a pattern without variables uses its parent directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The part of the filesystem covered by the pattern, for overlap checks: the root, or the file itself when the pattern has no variables.
    pub fn footprint(&self) -> PathBuf {
        if self.vars.is_empty() { PathBuf::from(&self.text) } else { self.root.clone() }
    }

    /// The depth of a matching file below the root, counted in segments. None when a multi-segment variable leaves the depth unbounded.
    pub fn depth(&self) -> Option<usize> {
        let fixed = self.segments.len() - self.root_segments();
        match self.vars.iter().find(|v| v.multi) {
            None => Some(fixed),
            Some(v) => {
                let spans: Vec<usize> = v.constraints.iter().filter_map(Constraint::segment_count).collect();
                spans.into_iter().min().map(|n| fixed - 1 + n)
            }
        }
    }

    /// Number of leading literal segments, which is the root's segment count below `/`.
    fn root_segments(&self) -> usize {
        if self.vars.is_empty() { self.segments.len() - 1 } else { self.segments.iter().take_while(|s| s.var.is_none()).count() }
    }

    /// Match an absolute path, binding each variable and checking its constraints. None when the path lies outside the pattern.
    pub fn matches(&self, path: &Path) -> Option<Binding> {
        let text = path.to_string_lossy();
        let caps = self.regex.captures(&text)?;
        let mut binding = Binding::new();
        for (i, var) in self.vars.iter().enumerate() {
            let value = caps.get(i + 1)?.as_str();
            if !var.admits(value) {
                return None;
            }
            binding.insert(var.name.clone(), value.to_owned());
        }
        Some(binding)
    }

    /// Fill each variable from the binding, checking its constraints, to produce the path.
    pub fn substitute(&self, binding: &Binding) -> Result<PathBuf> {
        Ok(PathBuf::from(self.bound_text(binding, true)?))
    }

    /// Bind some variables, yielding a pattern over the rest. The remaining text is parsed again so the root and depth follow the bound prefix.
    pub fn bind(&self, binding: &Binding) -> Result<Pattern> {
        let text = self.bound_text(binding, false)?;
        let mut out = Pattern::parse(&text)?;
        // A where block adds constraints to the variables without changing the text, so the reparse loses them; copy them over.
        for var in &self.vars {
            if let Some(target) = out.vars.iter_mut().find(|v| v.name == var.name) {
                target.constraints = var.constraints.clone();
            }
        }
        Ok(out)
    }

    /// Render the pattern, replacing each bound variable with its value. An unbound variable is written back as it appeared, or is an error when `complete` is set.
    fn bound_text(&self, binding: &Binding, complete: bool) -> Result<String> {
        let mut out = String::new();
        for seg in &self.segments {
            out.push('/');
            out.push_str(&seg.before);
            if let Some(i) = seg.var {
                let var = &self.vars[i];
                match binding.get(&var.name) {
                    Some(value) => {
                        if value.is_empty() || (!var.multi && value.contains('/')) || !var.admits(value) {
                            bail!("value {value:?} does not fit variable {{{}}} of pattern {}", var.name, self.text);
                        }
                        out.push_str(value);
                    }
                    None if complete => bail!("binding lacks variable {{{}}} of pattern {}", var.name, self.text),
                    None => {
                        out.push('{');
                        out.push_str(&var.name);
                        if var.multi {
                            out.push_str("...");
                        }
                        if let Some(c) = var.constraints.first() {
                            out.push_str(": ");
                            out.push_str(c.text());
                        }
                        out.push('}');
                    }
                }
            }
            out.push_str(&seg.after);
        }
        Ok(out)
    }

    /// The part of `path` beneath the root, joined with `/`. None when the path lies elsewhere.
    pub fn tail(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let text = rel.to_string_lossy().into_owned();
        if text.is_empty() { None } else { Some(text) }
    }

    /// Each variable paired with the depth of its segment below the root, in pattern order. The fan-out enumeration walks these.
    pub fn var_segments(&self) -> Vec<(usize, &Var)> {
        let root = self.root_segments();
        self.segments.iter().enumerate().filter_map(|(i, s)| s.var.map(|v| (i - root, &self.vars[v]))).collect()
    }

    /// Match a directory entry name against the segment at `depth` below the root. None when the name does not fit; Some(None) when it equals a literal segment; Some(Some((variable, value))) when the segment holds a variable and the name binds it.
    pub fn match_segment(&self, depth: usize, name: &str) -> Option<Option<(String, String)>> {
        let seg = self.segments.get(self.root_segments() + depth)?;
        match seg.var {
            None => (seg.before == name).then_some(None),
            Some(i) => {
                let var = &self.vars[i];
                let value = name.strip_prefix(seg.before.as_str())?.strip_suffix(seg.after.as_str())?;
                (!value.is_empty() && var.admits(value)).then(|| Some((var.name.clone(), value.to_owned())))
            }
        }
    }

    /// The literal text of the segment at `depth` below the root, or None when that segment holds a variable or lies past the end.
    pub fn segment_literal(&self, depth: usize) -> Option<&str> {
        let seg = self.segments.get(self.root_segments() + depth)?;
        seg.var.is_none().then_some(seg.before.as_str())
    }

    /// Total segment count below the root, which bounds fan-out enumeration.
    pub fn segments_below_root(&self) -> usize {
        self.segments.len() - self.root_segments()
    }
}

/// Split pattern text on `/` outside braces, so a constraint such as `{path...: docs/*.md}` stays within its segment.
fn split_segments(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '/' if depth == 0 => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out
}

/// Find the `}` closing the brace at `open`, counting nested braces so a regular expression such as `\d{4}` stays inside its variable.
fn matching_close(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in text[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Compile the matching regular expression. Literal text is escaped; a single variable matches one segment and a multi variable matches one or more.
fn build_regex(segments: &[Segment], vars: &[Var]) -> Regex {
    let mut rx = String::from("^");
    for seg in segments {
        rx.push('/');
        rx.push_str(&regex::escape(&seg.before));
        if let Some(i) = seg.var {
            rx.push_str(if vars[i].multi { "(.+)" } else { "([^/]+)" });
        }
        rx.push_str(&regex::escape(&seg.after));
    }
    rx.push('$');
    Regex::new(&rx).expect("escaped literals and fixed groups form a valid regex")
}

/// The literal directory prefix: segments before the first variable, or the parent directory when there is no variable.
fn root_of(segments: &[Segment]) -> PathBuf {
    let literal: Vec<&str> = segments.iter().take_while(|s| s.var.is_none()).map(|s| s.before.as_str()).collect();
    let take = if literal.len() == segments.len() { literal.len() - 1 } else { literal.len() };
    let mut root = PathBuf::from("/");
    for seg in &literal[..take] {
        root.push(seg);
    }
    root
}

/// Render a binding as `name=value, name=value` in name order, or the lone value when it has one entry.
pub fn describe_binding(binding: &Binding) -> String {
    match binding.len() {
        1 => binding.values().next().cloned().unwrap_or_default(),
        _ => binding.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(pairs: &[(&str, &str)]) -> Binding {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn literal_file_has_no_vars_and_claims_itself() {
        let p = Pattern::parse("/etc/nginx/conf.d/default.conf").unwrap();
        assert!(p.vars().is_empty());
        assert_eq!(p.root(), Path::new("/etc/nginx/conf.d"));
        assert_eq!(p.footprint(), PathBuf::from("/etc/nginx/conf.d/default.conf"));
        assert_eq!(p.depth(), Some(1));
        assert_eq!(p.matches(Path::new("/etc/nginx/conf.d/default.conf")), Some(Binding::new()));
        assert_eq!(p.matches(Path::new("/etc/nginx/conf.d/other.conf")), None);
        assert_eq!(p.substitute(&Binding::new()).unwrap(), PathBuf::from("/etc/nginx/conf.d/default.conf"));
    }

    #[test]
    fn trailing_slash_means_whole_tree() {
        let p = Pattern::parse("/u/Desktop/mylibrary/").unwrap();
        assert_eq!(p.text(), "/u/Desktop/mylibrary/{path...}");
        assert_eq!(p.root(), Path::new("/u/Desktop/mylibrary"));
        assert_eq!(p.footprint(), PathBuf::from("/u/Desktop/mylibrary"));
        assert_eq!(p.depth(), None);
        let b = p.matches(Path::new("/u/Desktop/mylibrary/sub/app.js")).unwrap();
        assert_eq!(b, bind(&[("path", "sub/app.js")]));
        assert_eq!(p.substitute(&b).unwrap(), PathBuf::from("/u/Desktop/mylibrary/sub/app.js"));
        assert_eq!(p.tail(Path::new("/u/Desktop/mylibrary/sub/app.js")), Some("sub/app.js".into()));
        assert_eq!(p.matches(Path::new("/u/Desktop/other/app.js")), None);
    }

    #[test]
    fn constraints_bound_depth_and_filter_matches() {
        let mut p = Pattern::parse("/u/lib/{path...: *.js}").unwrap();
        assert_eq!(p.depth(), Some(1));
        assert!(p.matches(Path::new("/u/lib/app.js")).is_some());
        assert!(p.matches(Path::new("/u/lib/sub/app.js")).is_none(), "a single star stays within one segment");
        assert!(p.matches(Path::new("/u/lib/notes.txt")).is_none());
        // A where-block constraint stacks on the inline one.
        p.constrain("path", Constraint::parse("a*").unwrap()).unwrap();
        assert!(p.matches(Path::new("/u/lib/app.js")).is_some());
        assert!(p.matches(Path::new("/u/lib/util.js")).is_none());
        assert!(p.constrain("nope", Constraint::parse("*").unwrap()).is_err());

        let deep = Pattern::parse("/u/lib/{path...: docs/*.md}").unwrap();
        assert_eq!(deep.depth(), Some(2));
        assert!(deep.matches(Path::new("/u/lib/docs/a.md")).is_some());
        assert!(deep.matches(Path::new("/u/lib/docs/sub/a.md")).is_none());
        let any = Pattern::parse("/u/lib/{path...: **/*.md}").unwrap();
        assert_eq!(any.depth(), None);
        assert!(any.matches(Path::new("/u/lib/docs/sub/a.md")).is_some());

        let re = Pattern::parse("/logs/{date: ~\\d{4}-\\d\\d-\\d\\d~}.log").unwrap();
        assert!(re.matches(Path::new("/logs/2026-11-01.log")).is_some());
        assert!(re.matches(Path::new("/logs/yesterday.log")).is_none());
        assert!(re.substitute(&bind(&[("date", "nope")])).is_err(), "substitution checks constraints too");
    }

    #[test]
    fn single_variables_round_trip_in_any_order() {
        let flat = Pattern::parse("/u/scripts/skills/{name}.md").unwrap();
        let nested = Pattern::parse("/u/.claude/skills/{name}/SKILL.md").unwrap();
        let b = flat.matches(Path::new("/u/scripts/skills/prose.md")).unwrap();
        assert_eq!(b, bind(&[("name", "prose")]));
        assert_eq!(nested.substitute(&b).unwrap(), PathBuf::from("/u/.claude/skills/prose/SKILL.md"));
        assert_eq!(nested.matches(Path::new("/u/.claude/skills/prose/SKILL.md")), Some(b));
        assert_eq!(nested.matches(Path::new("/u/.claude/skills/a/b/SKILL.md")), None);
        assert_eq!(nested.depth(), Some(2));
        assert_eq!(flat.depth(), Some(1));

        let two = Pattern::parse("/logs/{year}/{app}.log").unwrap();
        let b = two.matches(Path::new("/logs/2026/replica.log")).unwrap();
        assert_eq!(b, bind(&[("app", "replica"), ("year", "2026")]));
        assert_eq!(two.substitute(&b).unwrap(), PathBuf::from("/logs/2026/replica.log"));
    }

    #[test]
    fn binding_a_prefix_variable_yields_a_deeper_pattern() {
        let p = Pattern::parse("/u/Projects/{project}/docs/style/{path...}").unwrap();
        assert_eq!(p.root(), Path::new("/u/Projects"));
        let segs: Vec<(usize, &str)> = p.var_segments().iter().map(|(d, v)| (*d, v.name.as_str())).collect();
        assert_eq!(segs, vec![(0, "project"), (3, "path")]);
        assert_eq!(p.match_segment(0, "alpha"), Some(Some(("project".into(), "alpha".into()))));
        assert_eq!(p.match_segment(1, "docs"), Some(None));
        assert_eq!(p.match_segment(1, "src"), None);
        let bound = p.bind(&bind(&[("project", "alpha")])).unwrap();
        assert_eq!(bound.text(), "/u/Projects/alpha/docs/style/{path...}");
        assert_eq!(bound.root(), Path::new("/u/Projects/alpha/docs/style"));
        assert_eq!(bound.var_names(), BTreeSet::from(["path"]));
        // Bound text keeps constraints of the remaining variables.
        let c = Pattern::parse("/u/Projects/{project: [a-z]*}/docs/{path...: *.md}").unwrap();
        let bound = c.bind(&bind(&[("project", "alpha")])).unwrap();
        assert_eq!(bound.text(), "/u/Projects/alpha/docs/{path...: *.md}");
        assert!(bound.matches(Path::new("/u/Projects/alpha/docs/x.md")).is_some());
        assert!(bound.matches(Path::new("/u/Projects/alpha/docs/x.txt")).is_none());
    }

    #[test]
    fn malformed_patterns_are_rejected() {
        assert!(Pattern::parse("relative/{x}").is_err());
        assert!(Pattern::parse("/a/{x}/{x}.md").is_err());
        assert!(Pattern::parse("/a/{bad name}.md").is_err());
        assert!(Pattern::parse("/a/{oops.md").is_err());
        assert!(Pattern::parse("/a/{x}{y}.md").is_err());
        assert!(Pattern::parse("/a/{rest...}/{x}.md").is_err(), "a multi variable must be the last variable");
        assert!(Pattern::parse("/a//b").is_err());
        assert!(Pattern::parse("/").is_err());
        assert!(Pattern::parse("/a/{x: [}").is_err());
    }

    #[test]
    fn describe_binding_shows_the_lone_value_or_pairs() {
        assert_eq!(describe_binding(&bind(&[("path", "docs/a.md")])), "docs/a.md");
        assert_eq!(describe_binding(&bind(&[("year", "2026"), ("app", "x")])), "app=x, year=2026");
        assert_eq!(describe_binding(&Binding::new()), "");
    }
}
