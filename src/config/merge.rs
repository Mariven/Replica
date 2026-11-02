//! Compose the configuration. This module expands `${name}` variables, deep-merges profiles into relation bodies, resolves and merges included files, and stamps out a relation per combination of `each` values.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_norway::{Mapping, Value};

/// Expand the `${name}` references in a string against a scope. A variable's value may hold references of its own, which are expanded in turn. A cycle or an unknown name is an error.
pub fn expand_vars(text: &str, scope: &BTreeMap<String, String>) -> Result<String> {
    expand_with_stack(text, scope, false, &mut Vec::new())
}

/// Expand the known `${name}` references and leave unknown ones as written. Command strings are expanded this way, since their remaining placeholders are filled in when the command runs.
pub fn expand_vars_lenient(text: &str, scope: &BTreeMap<String, String>) -> Result<String> {
    expand_with_stack(text, scope, true, &mut Vec::new())
}

/// The recursive worker behind expand_vars. `stack` lists the variables currently being expanded, so meeting one of them again means a cycle.
fn expand_with_stack(text: &str, scope: &BTreeMap<String, String>, lenient: bool, stack: &mut Vec<String>) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else { bail!("unclosed ${{ in {text:?}") };
        let name = &after[..end];
        let Some(value) = scope.get(name) else {
            if lenient {
                out.push_str(&rest[start..start + 3 + end]);
                rest = &after[end + 1..];
                continue;
            }
            bail!("undefined variable ${{{name}}} in {text:?}")
        };
        if stack.iter().any(|n| n == name) {
            bail!("circular variable reference through ${{{name}}}");
        }
        stack.push(name.to_owned());
        let expanded = expand_with_stack(value, scope, lenient, stack)?;
        stack.pop();
        out.push_str(&expanded);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The relation fields holding commands. Unknown placeholders under these are left in place for run time.
const COMMAND_FIELDS: [&str; 2] = ["on", "transform"];

/// Expand variables in each string of a YAML value, including mapping keys. Strings under `on` and `transform` keep unknown placeholders.
pub fn expand_value(v: &Value, scope: &BTreeMap<String, String>) -> Result<Value> {
    expand_inner(v, scope, false)
}

/// The recursive worker behind expand_value.
fn expand_inner(v: &Value, scope: &BTreeMap<String, String>, lenient: bool) -> Result<Value> {
    let expand = |s: &str| if lenient { expand_vars_lenient(s, scope) } else { expand_vars(s, scope) };
    Ok(match v {
        Value::String(s) => Value::String(expand(s)?),
        Value::Mapping(m) => {
            let mut out = Mapping::new();
            for (k, val) in m {
                let key_text = k.as_str();
                let k = match key_text {
                    Some(s) => Value::String(expand(s)?),
                    None => k.clone(),
                };
                let inner_lenient = lenient || key_text.is_some_and(|s| COMMAND_FIELDS.contains(&s));
                out.insert(k, expand_inner(val, scope, inner_lenient)?);
            }
            Value::Mapping(out)
        }
        Value::Sequence(seq) => Value::Sequence(seq.iter().map(|v| expand_inner(v, scope, lenient)).collect::<Result<_>>()?),
        other => other.clone(),
    })
}

/// Deep-merge `over` into `base`. When both are mappings, each key of `over` is added to `base` or merged into the matching key there. Any other kind of value in `over` replaces the value in `base` outright.
pub fn deep_merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Mapping(b), Value::Mapping(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(existing) => deep_merge(existing, v),
                    None => { b.insert(k.clone(), v.clone()); }
                }
            }
        }
        (base, over) => *base = over.clone(),
    }
}

/// Assemble a relation body from its profiles: the profile `_` when present, then each listed profile in order, then the body itself.
/// A profile may list profiles of its own, which are merged before it. The chain is refused when it comes back around to a profile already on it.
pub fn assemble(body: &Mapping, profiles: &Mapping) -> Result<Mapping> {
    let mut out = Value::Mapping(Mapping::new());
    if let Some(defaults) = profiles.get("_") {
        merge_profile("_", defaults, profiles, &mut out, &mut vec!["_".to_owned()])?;
    }
    for name in listed_profiles(body)? {
        let profile = profiles.get(name.as_str()).with_context(|| format!("profile {name:?} is not defined"))?;
        merge_profile(&name, profile, profiles, &mut out, &mut vec![name.clone()])?;
    }
    let mut own = body.clone();
    own.remove("profile");
    deep_merge(&mut out, &Value::Mapping(own));
    match out {
        Value::Mapping(m) => Ok(m),
        _ => unreachable!("merging mappings yields a mapping"),
    }
}

/// Merge a profile into the body under assembly. Any profiles listed inside it are merged first.
fn merge_profile(name: &str, profile: &Value, profiles: &Mapping, out: &mut Value, chain: &mut Vec<String>) -> Result<()> {
    let Value::Mapping(map) = profile else { bail!("profile {name:?} must be a mapping of relation fields") };
    for sub in listed_profiles(map)? {
        if chain.contains(&sub) {
            bail!("profile {sub:?} is referenced in a cycle: {}", chain.join(" -> "));
        }
        let inner = profiles.get(sub.as_str()).with_context(|| format!("profile {name:?} lists profile {sub:?}, which is not defined"))?;
        chain.push(sub.clone());
        merge_profile(&sub, inner, profiles, out, chain)?;
        chain.pop();
    }
    let mut own = map.clone();
    own.remove("profile");
    deep_merge(out, &Value::Mapping(own));
    Ok(())
}

/// Read the `profile` field of a body, which may be a string or a list of strings.
fn listed_profiles(body: &Mapping) -> Result<Vec<String>> {
    Ok(match body.get("profile") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Sequence(items)) => items.iter().map(|v| v.as_str().map(str::to_owned).with_context(|| format!("profile names must be strings, not {v:?}"))).collect::<Result<_>>()?,
        Some(other) => bail!("profile must be a name or a list of names, not {}", super::text::yaml_kind(other)),
    })
}

/// Stamp a relation out once per combination of its `each` values. A copy's name is the relation name with its values appended, as in `rules-alpha-x`. The copy also gets those values as variables.
pub fn stamp(name: &str, body: &Mapping) -> Result<Vec<(String, Mapping, BTreeMap<String, String>)>> {
    let each: BTreeMap<String, Vec<String>> = match body.get("each") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(v) => serde_norway::from_value(v.clone()).with_context(|| format!("relation {name:?}: each must map variable names to lists of values"))?,
    };
    if each.is_empty() {
        return Ok(vec![(name.to_owned(), body.clone(), BTreeMap::new())]);
    }
    let mut combos: Vec<BTreeMap<String, String>> = vec![BTreeMap::new()];
    for (var, values) in &each {
        if values.is_empty() {
            bail!("relation {name:?}: each.{var} lists no values");
        }
        combos = combos.into_iter().flat_map(|c| values.iter().map(move |v| { let mut c = c.clone(); c.insert(var.clone(), v.clone()); c })).collect();
    }
    let mut body = body.clone();
    body.remove("each");
    Ok(combos.into_iter().map(|c| {
        let suffix: Vec<&str> = c.values().map(String::as_str).collect();
        (format!("{name}-{}", suffix.join("-")), body.clone(), c)
    }).collect())
}

/// Resolve an include entry to a list of files. The entry is either a path or a glob over the entries of a single directory. A relative entry is resolved against the including file's directory.
pub fn included_files(entry: &str, base: &Path) -> Result<Vec<PathBuf>> {
    let expanded = shellexpand::tilde(entry).into_owned();
    let path = if Path::new(&expanded).is_absolute() { PathBuf::from(&expanded) } else { base.join(&expanded) };
    let text = path.to_string_lossy().into_owned();
    if !text.contains(['*', '?', '[']) {
        if !path.is_file() {
            bail!("included file {} does not exist", path.display());
        }
        return Ok(vec![path]);
    }
    let dir = path.parent().with_context(|| format!("include {entry:?} has no directory"))?;
    let name = path.file_name().with_context(|| format!("include {entry:?} has no file name"))?.to_string_lossy().into_owned();
    if dir.to_string_lossy().contains(['*', '?', '[']) {
        bail!("include {entry:?}: wildcards are allowed in the file name alone");
    }
    let matcher = globset::GlobBuilder::new(&name).literal_separator(true).build().with_context(|| format!("include {entry:?}"))?.compile_matcher();
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir).with_context(|| format!("include {entry:?}: reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false) && matcher.is_match(e.file_name().to_string_lossy().as_ref()))
        .map(|e| e.path())
        .collect();
    found.sort();
    Ok(found)
}

/// Merge the vars, profiles, relations, and ignore list of an included file into the main file. When both files define the same name, the merge fails.
pub fn merge_included(main: &mut super::text::FileText, other: super::text::FileText, main_path: &Path, other_path: &Path) -> Result<()> {
    if other.state.is_some() || other.interval.is_some() || other.dashboard.is_some() || !other.include.is_empty() {
        bail!("included file {} may only set vars, profiles, relations, and ignore", other_path.display());
    }
    for (k, v) in other.vars {
        if main.vars.contains_key(&k) {
            bail!("variable {k:?} is defined in both {} and {}", main_path.display(), other_path.display());
        }
        main.vars.insert(k, v);
    }
    for (k, v) in other.profiles {
        if main.profiles.contains_key(&k) {
            bail!("profile {} is defined in both {} and {}", k.as_str().unwrap_or("?"), main_path.display(), other_path.display());
        }
        main.profiles.insert(k, v);
    }
    for (k, v) in other.relations {
        if main.relations.contains_key(&k) {
            bail!("relation {} is defined in both {} and {}", k.as_str().unwrap_or("?"), main_path.display(), other_path.display());
        }
        main.relations.insert(k, v);
    }
    main.ignore.extend(other.ignore);
    Ok(())
}

/// The keys of a body that fall outside `allowed`. The profile check runs this on each profile before any relation merges it in.
pub fn unknown_fields(body: &Mapping, allowed: &[&str]) -> BTreeSet<String> {
    body.keys().filter_map(|k| k.as_str()).filter(|k| !allowed.contains(k)).map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn yaml(text: &str) -> Mapping {
        serde_norway::from_str(text).unwrap()
    }

    #[test]
    fn variables_expand_recursively_and_refuse_cycles_and_unknowns() {
        let sc = scope(&[("state", "/live"), ("logs", "${state}/logs"), ("relation", "r")]);
        assert_eq!(expand_vars("${logs}/${relation}.log", &sc).unwrap(), "/live/logs/r.log");
        assert_eq!(expand_vars("~/x/{name}.md", &sc).unwrap(), "~/x/{name}.md", "path holes pass through");
        assert!(expand_vars("${nope}", &sc).unwrap_err().to_string().contains("undefined variable"));
        let cyc = scope(&[("a", "${b}"), ("b", "${a}")]);
        assert!(expand_vars("${a}", &cyc).unwrap_err().to_string().contains("circular"));
        assert!(expand_vars("${open", &sc).unwrap_err().to_string().contains("unclosed"));
    }

    #[test]
    fn deep_merge_adds_inside_mappings_and_replaces_scalars_and_lists() {
        let mut base: Value = serde_norway::from_str("a: 1\nm: { x: 1, y: 2 }\nl: [1, 2]\n").unwrap();
        let over: Value = serde_norway::from_str("a: 2\nm: { y: 3, z: 4 }\nl: [9]\n").unwrap();
        deep_merge(&mut base, &over);
        let got: Value = serde_norway::from_str("a: 2\nm: { x: 1, y: 3, z: 4 }\nl: [9]\n").unwrap();
        assert_eq!(base, got);
    }

    #[test]
    fn profiles_assemble_in_order_with_defaults_first() {
        let profiles = yaml("_: { log: default.log, sync_times: false }\ntwo-way: { flow: \"a <-> b\", resolve: mtime }\nloud: { profile: two-way, resolve: \"a > b\" }\n");
        let body = yaml("profile: loud\nsites: { a: /a/, b: /b/ }\nsync_times: true\n");
        let out = assemble(&body, &profiles).unwrap();
        assert_eq!(out.get("flow").unwrap().as_str(), Some("a <-> b"));
        assert_eq!(out.get("resolve").unwrap().as_str(), Some("a > b"), "the listing profile overrides the one it lists");
        assert_eq!(out.get("log").unwrap().as_str(), Some("default.log"));
        assert_eq!(out.get("sync_times").unwrap().as_bool(), Some(true), "the body wins over all of its profiles");
        assert!(out.get("profile").is_none());
        let err = assemble(&yaml("profile: nope\n"), &profiles).unwrap_err().to_string();
        assert!(err.contains("profile \"nope\" is not defined"), "{err}");
        let cyc = yaml("a: { profile: b }\nb: { profile: a }\n");
        assert!(assemble(&yaml("profile: a\n"), &cyc).unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn each_stamps_out_combinations() {
        let body = yaml("each: { project: [alpha, beta], side: [x] }\nsites: { a: \"/p/${project}/${side}/\" }\n");
        let copies = stamp("rules", &body).unwrap();
        let names: Vec<&str> = copies.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["rules-alpha-x", "rules-beta-x"]);
        assert!(copies[0].1.get("each").is_none());
        assert_eq!(copies[1].2["project"], "beta");
        assert_eq!(stamp("plain", &yaml("sites: {}\n")).unwrap().len(), 1);
        assert!(stamp("bad", &yaml("each: { p: [] }\n")).unwrap_err().to_string().contains("lists no values"));
    }

    #[test]
    fn includes_resolve_files_and_globs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("relations");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.yaml"), "").unwrap();
        std::fs::write(dir.join("b.yaml"), "").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        let found = included_files("relations/*.yaml", tmp.path()).unwrap();
        assert_eq!(found, vec![dir.join("a.yaml"), dir.join("b.yaml")]);
        assert_eq!(included_files("relations/a.yaml", tmp.path()).unwrap(), vec![dir.join("a.yaml")]);
        assert!(included_files("relations/zzz.yaml", tmp.path()).unwrap_err().to_string().contains("does not exist"));
        assert!(included_files("*/x.yaml", tmp.path()).unwrap_err().to_string().contains("file name alone"));
    }
}
