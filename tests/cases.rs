//! End-to-end cases against temp directories: the original scenarios, templates, defaults, lifecycle, conflicts, crash recovery, and safety guards.
//! Each test builds relations through the YAML config layer, so the config format is exercised too.

use std::fs;
use std::path::Path;

use replica::config;
use replica::engine::Engine;
use replica::store::Store;
use tempfile::TempDir;

/// Build an engine from a YAML relations snippet, with paths rooted in the temp dir.
fn engine(tmp: &TempDir, relations_yaml: &str) -> Engine {
    let yaml = format!("entries:\n{relations_yaml}");
    let (_, relations, _) = config::load(&yaml).unwrap();
    let store = Store::open(&tmp.path().join("state.db")).unwrap();
    Engine::new(store, relations, vec![])
}

/// Write `content` to `path`, creating parent directories as needed.
fn write(path: impl AsRef<Path>, content: &str) {
    fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Read a file as a string, or `None` if it is missing.
fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Set a file's mtime to `secs` seconds after the Unix epoch.
/// Poll cycles carry no wall-clock delay in tests; mtime-based policies are not used here except where this makes the ordering explicit.
fn set_mtime(path: impl AsRef<Path>, secs: u64) {
    filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(secs as i64, 0)).unwrap();
}

#[test]
fn case_1_two_folders_bidirectional() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("mylibrary"), tmp.path().join("desktop-library"));
    write(a.join("app.js"), "console.log(1)");
    write(a.join("util.js"), "util");
    write(a.join("notes.txt"), "not js");
    write(b.join("app.js"), "console.log(1)");
    write(b.join("util.js"), "util");

    let eng = engine(&tmp, &format!(
        "    mylibrary:\n      source: {{ root: {} }}\n      dest: {{ root: {} }}\n      pattern: \"*.js\"\n",
        a.display(), b.display()
    ));

    // First cycle seeds the record. An edit on either side then propagates to the other.
    eng.cycle().unwrap();
    write(a.join("app.js"), "console.log(2)");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("app.js")).unwrap(), "console.log(2)");

    write(b.join("util.js"), "util v2");
    eng.cycle().unwrap();
    assert_eq!(read(a.join("util.js")).unwrap(), "util v2");

    // Pattern boundary: the txt file never syncs.
    assert_eq!(read(b.join("notes.txt")), None);
}

#[test]
fn case_2_rename_via_fixed_mapping() {
    let tmp = TempDir::new().unwrap();
    let (conf_dir, desk) = (tmp.path().join("nginx/conf.d"), tmp.path().join("Desktop"));
    write(conf_dir.join("default.conf"), "server {}");

    let eng = engine(&tmp, &format!(
        "    nginx-default:\n      source: {{ root: {} }}\n      dest: {{ root: {}, rename: nginx-conf.txt }}\n",
        conf_dir.display(), desk.display()
    ));

    // Establish the source, then touch-to-bootstrap: the empty touched file is a late joiner that conforms to the source, and does not overwrite it.
    eng.cycle().unwrap();
    write(desk.join("nginx-conf.txt"), "");
    eng.cycle().unwrap();
    assert_eq!(read(desk.join("nginx-conf.txt")).unwrap(), "server {}");

    // The mapping works in both directions through the rename.
    write(desk.join("nginx-conf.txt"), "server { listen 80; }");
    eng.cycle().unwrap();
    assert_eq!(read(conf_dir.join("default.conf")).unwrap(), "server { listen 80; }");
}

#[test]
fn case_3_explicit_fan_out_mirror() {
    let tmp = TempDir::new().unwrap();
    let rules = tmp.path().join("rules");
    let projects: Vec<_> = ["bouncer", "duckmaster", "screamio"].iter()
        .map(|p| tmp.path().join(p).join("docs/style")).collect();
    write(rules.join("main_style.md"), "v1");
    for p in &projects {
        write(p.join("main_style.md"), "v1");
    }

    let many: Vec<String> = projects.iter().map(|p| format!("        - {}", p.display())).collect();
    let eng = engine(&tmp, &format!(
        "    style-guide:\n      source: {{ root: {} }}\n      dest:\n        many:\n{}\n      pattern: main_style.md\n      content: mirror(source)\n      on_delete: skip\n",
        rules.display(), many.join("\n")
    ));

    eng.cycle().unwrap();
    write(rules.join("main_style.md"), "v2");
    eng.cycle().unwrap();
    for p in &projects {
        assert_eq!(read(p.join("main_style.md")).unwrap(), "v2");
    }

    // A project-side edit is overwritten on the next cycle: mirror is one-way.
    write(projects[1].join("main_style.md"), "local heresy");
    eng.cycle().unwrap();
    assert_eq!(read(projects[1].join("main_style.md")).unwrap(), "v2");
}

#[test]
fn case_4_discovered_fan_out() {
    let tmp = TempDir::new().unwrap();
    let rules = tmp.path().join("rules");
    let projects_dir = tmp.path().join("Projects");
    write(rules.join("main_style.md"), "v1");
    for p in ["bouncer", "duckmaster"] {
        write(projects_dir.join(p).join("docs/style/main_style.md"), "old");
    }

    let eng = engine(&tmp, &format!(
        "    style-guide-all:\n      source: {{ root: {} }}\n      dest:\n        scan:\n          template: \"{}/{{project}}/docs/style\"\n          hole: project\n          dir: {}\n      pattern: main_style.md\n      content: mirror(source)\n      on_new: replicate\n",
        rules.display(), projects_dir.display(), projects_dir.display()
    ));

    eng.cycle().unwrap();
    for p in ["bouncer", "duckmaster"] {
        assert_eq!(read(projects_dir.join(p).join("docs/style/main_style.md")).unwrap(), "v1");
    }

    // A project added later is discovered on the next cycle, with no relation change.
    fs::create_dir_all(projects_dir.join("screamio")).unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(projects_dir.join("screamio/docs/style/main_style.md")).unwrap(), "v1");
}

#[test]
fn case_5_path_template_rewrite() {
    let tmp = TempDir::new().unwrap();
    let (flat, nested) = (tmp.path().join("scripts/skills"), tmp.path().join("claude/skills"));
    write(flat.join("prose.md"), "write plainly");
    write(nested.join("prose/SKILL.md"), "write plainly");
    write(flat.join("humanizer.md"), "no em dashes");
    write(flat.join("notes.txt"), "not a skill");

    let eng = engine(&tmp, &format!(
        "    skills:\n      source: {{ path: \"{}/{{name}}.md\" }}\n      dest: {{ path: \"{}/{{name}}/SKILL.md\" }}\n      on_new: replicate\n",
        flat.display(), nested.display()
    ));

    // Discovery through both templates. A source-only file replicates into its rewritten home.
    eng.cycle().unwrap();
    assert_eq!(read(nested.join("humanizer/SKILL.md")).unwrap(), "no em dashes");
    // Non-matching files are outside the relation entirely.
    assert_eq!(read(nested.join("notes.txt")), None);

    // Edits propagate through the rewrite in both directions.
    write(flat.join("prose.md"), "write plainly, v2");
    eng.cycle().unwrap();
    assert_eq!(read(nested.join("prose/SKILL.md")).unwrap(), "write plainly, v2");
    write(nested.join("prose/SKILL.md"), "write plainly, v3");
    eng.cycle().unwrap();
    assert_eq!(read(flat.join("prose.md")).unwrap(), "write plainly, v3");

    // Mismatched hole sets are rejected at load.
    let yaml = "entries:\n    bad:\n      a: { path: \"/x/{name}.md\" }\n      b: { path: \"/y/{other}/f.md\" }\n";
    let err = match config::load(yaml) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("mismatched holes were accepted"),
    };
    assert!(err.contains("disagree on holes"), "unexpected error: {err}");

    // A template label cannot relate to a plain root.
    let yaml = "entries:\n    bad2:\n      a: { path: \"/x/{name}.md\" }\n      b: { root: /y }\n";
    assert!(config::load(yaml).is_err());
}

#[test]
fn lifecycle_restrict_restore_and_propagate() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "hello");
    write(a.join("only-here.txt"), "solo");
    write(b.join("f.txt"), "hello");

    let eng = engine(&tmp, &format!(
        "    g:\n      source: {{ root: {} }}\n      dest: {{ root: {} }}\n",
        a.display(), b.display()
    ));

    // on_new: restrict (default): only-here.txt is never created at b.
    eng.cycle().unwrap();
    assert_eq!(read(b.join("only-here.txt")), None);

    // on_delete: restore (default): a deletion is repaired from the surviving copy.
    fs::remove_file(b.join("f.txt")).unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");

    // With on_delete: propagate, the same deletion spreads and the identifier is then forgotten.
    let eng2 = engine(&tmp, &format!(
        "    g2:\n      source: {{ root: {} }}\n      dest: {{ root: {} }}\n      on_delete: propagate\n",
        a.display(), b.display()
    ));
    eng2.cycle().unwrap();
    fs::remove_file(a.join("f.txt")).unwrap();
    eng2.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    eng2.cycle().unwrap(); // inert identifier collected; nothing resurrects
    assert_eq!(read(a.join("f.txt")), None);
}

#[test]
fn conflict_skips_by_default_and_newer_tiebreaks() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "base");
    write(b.join("f.txt"), "base");

    let eng = engine(&tmp, &format!(
        "    g:\n      source: {{ root: {} }}\n      dest: {{ root: {} }}\n",
        a.display(), b.display()
    ));
    eng.cycle().unwrap();

    // Concurrent divergent edits: the default conflict policy leaves both alone.
    write(a.join("f.txt"), "from a");
    write(b.join("f.txt"), "from b");
    eng.cycle().unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "from a");
    assert_eq!(read(b.join("f.txt")).unwrap(), "from b");

    // Same divergence under conflict: newer adopts the later mtime.
    let eng2 = engine(&tmp, &format!(
        "    g2:\n      source: {{ root: {} }}\n      dest: {{ root: {} }}\n      conflict: newer\n",
        a.display(), b.display()
    ));
    set_mtime(a.join("f.txt"), 1_000);
    set_mtime(b.join("f.txt"), 2_000);
    eng2.cycle().unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "from b");
    assert_eq!(read(b.join("f.txt")).unwrap(), "from b");
}

#[test]
fn self_writes_do_not_churn() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");

    let eng = engine(&tmp, &format!(
        "    g:\n      source: {{ root: {} }}\n      dest: {{ root: {} }}\n",
        a.display(), b.display()
    ));
    eng.cycle().unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle().unwrap();

    // The engine's own write to b must read as on-record, not as a fresh dest edit.
    let b_mtime = fs::metadata(b.join("f.txt")).unwrap().modified().unwrap();
    eng.cycle().unwrap();
    eng.cycle().unwrap();
    assert_eq!(fs::metadata(b.join("f.txt")).unwrap().modified().unwrap(), b_mtime);
    assert_eq!(read(a.join("f.txt")).unwrap(), "v2");
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
}

#[test]
fn stale_intent_is_reported_and_instance_resolves_fresh() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");

    let db = tmp.path().join("state.db");
    let yaml = format!(
        "entries:\n    g:\n      source: {{ root: {} }}\n      dest: {{ root: {} }}\n",
        a.display(), b.display()
    );
    let (_, relations, _) = config::load(&yaml).unwrap();

    {
        let eng = Engine::new(Store::open(&db).unwrap(), relations, vec![]);
        eng.cycle().unwrap();
        // Simulate a crash mid-operation: intent journaled, mutation half-applied, no commit.
        eng.store.write_intent("g", &vec!["f.txt".into()], "copy source -> dest").unwrap();
        write(b.join("f.txt"), "torn write");
    }

    // A new process finds the stale intent, reports it, and repairs the divergence normally.
    let (_, relations, _) = config::load(&yaml).unwrap();
    use std::sync::{Arc, Mutex};
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen2 = seen.clone();
    let eng = Engine::new(Store::open(&db).unwrap(), relations, vec![Box::new(move |e| {
        if let replica::engine::Event::StaleIntent { identifier, .. } = e {
            seen2.lock().unwrap().push(identifier.clone());
        }
    })]);
    eng.recover().unwrap();
    assert_eq!(*seen.lock().unwrap(), vec!["f.txt".to_string()]);
    eng.cycle().unwrap();
    // The torn write reads as a dest edit against the record and propagates back.
    assert_eq!(read(a.join("f.txt")).unwrap(), "torn write");
}

#[test]
fn arbitrary_labels_beyond_source_and_dest() {
    let tmp = TempDir::new().unwrap();
    let (a, b, c) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    write(c.join("f.txt"), "v1");

    // Three labels, none named source or dest; one sigiled.
    let eng = engine(&tmp, &format!(
        "    tri:\n      first: {{ root: {} }}\n      second: {{ root: {} }}\n      \"$backup\": {{ root: {} }}\n      content: mirror(first)\n",
        a.display(), b.display(), c.display()
    ));
    eng.cycle().unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    assert_eq!(read(c.join("f.txt")).unwrap(), "v2");

    // A policy naming an undeclared label is rejected at load.
    let yaml = "entries:\n    bad:\n      x: { root: /tmp/x }\n      y: { root: /tmp/y }\n      content: mirror(z)\n";
    let err = match config::load(yaml) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("undeclared policy label was accepted"),
    };
    assert!(err.contains("not declared"), "unexpected error: {err}");
}

#[test]
fn gate_holds_writes_and_reports_the_plan() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");

    let eng = engine(&tmp, &format!(
        "    g:\n      source: \"{}\"\n      dest: \"{}\"\n",
        a.display(), b.display()
    ));
    eng.cycle().unwrap();

    // Gate closed: the edit is observed and planned but never applied, cycle after cycle.
    eng.gates.set_global(true);
    write(a.join("f.txt"), "v2");
    eng.cycle().unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");

    // The report carries the held plan: one copy op.
    let report = eng.report();
    let row = report.relations[0].rows.iter().find(|r| r.identifier == "f.txt").unwrap();
    assert_eq!(row.plan.len(), 1);

    // Opening the gate applies exactly that plan on the next cycle.
    eng.gates.set_global(false);
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");

    // A per-relation gate holds this relation only, and config can set it from the start.
    let yaml = format!(
        "entries:\n    g:\n      source: \"{}\"\n      dest: \"{}\"\n      gate: true\n",
        a.display(), b.display()
    );
    let (_, relations, _) = config::load(&yaml).unwrap();
    assert!(relations[0].gate_default);
    let eng2 = Engine::new(Store::open(&tmp.path().join("state2.db")).unwrap(), relations, vec![]);
    eng2.cycle().unwrap();
    write(a.join("f.txt"), "v3");
    eng2.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    // The gate held even baseline seeding, so undo the divergence, open, seed, then edit.
    write(a.join("f.txt"), "v2");
    eng2.gates.set_relation("g", false);
    eng2.cycle().unwrap();
    write(a.join("f.txt"), "v3");
    eng2.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v3");
}

#[test]
fn archive_keeps_overwritten_and_deleted_content() {
    use replica::archive::Archive;

    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "old content");
    write(b.join("f.txt"), "old content");
    write(a.join("gone.txt"), "delete me");
    write(b.join("gone.txt"), "delete me");

    let mut eng = engine(&tmp, &format!(
        "    g:\n      source: \"{}\"\n      dest: \"{}\"\n      on_delete: \"propagate\"\n",
        a.display(), b.display()
    ));
    let dir = tmp.path().join("archive");
    eng.archive = Some(Archive { dir: dir.clone(), cap: 1 << 20 });
    eng.cycle().unwrap();

    // An overwrite archives the losing content; a propagated deletion archives the deleted copy.
    write(a.join("f.txt"), "new content");
    fs::remove_file(a.join("gone.txt")).unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "new content");
    assert_eq!(read(b.join("gone.txt")), None);

    let entries = eng.store.archive_entries("g").unwrap();
    assert_eq!(entries.len(), 2);
    let overwritten = entries.iter().find(|e| e.reason == "overwritten").unwrap();
    assert_eq!(read(dir.join(&overwritten.hash)).unwrap(), "old content");
    assert!(overwritten.path.contains("b/f.txt") || overwritten.path.ends_with("f.txt"));
    let deleted = entries.iter().find(|e| e.reason == "deleted").unwrap();
    assert_eq!(read(dir.join(&deleted.hash)).unwrap(), "delete me");

    // Same content at two names is stored once.
    write(a.join("f.txt"), "delete me");
    eng.cycle().unwrap(); // overwrites b/f.txt ("new content" archived)
    write(a.join("f.txt"), "final");
    eng.cycle().unwrap(); // archives b/f.txt's "delete me", deduped against gone.txt's blob
    let (rows, bytes) = eng.store.archive_stats().unwrap();
    assert_eq!(rows, 4);
    let unique: std::collections::BTreeSet<String> =
        eng.store.archive_entries("g").unwrap().into_iter().map(|e| e.hash).collect();
    assert_eq!(unique.len(), 3, "'delete me' must be stored once");
    assert_eq!(bytes as usize, "old content".len() + "new content".len() + "delete me".len());

    // Eviction: a tiny cap drops the oldest rows but always keeps the newest.
    eng.archive = Some(Archive { dir: dir.clone(), cap: 1 });
    write(a.join("f.txt"), "post-cap");
    eng.cycle().unwrap();
    let entries = eng.store.archive_entries("g").unwrap();
    assert_eq!(entries.len(), 1);
    // The newest entry survives: "final" was archived by this very overwrite.
    assert_eq!(read(dir.join(&entries[0].hash)).unwrap(), "final");
    // Evicted blobs are gone from disk.
    assert!(!dir.join(blake3::hash(b"old content").to_hex().to_string()).exists());
}

#[test]
fn overlapping_roots_are_rejected() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("a/nested"));
    write(a.join("f.txt"), "x");
    write(b.join("g.txt"), "y");

    let eng = engine(&tmp, &format!(
        "    g1:\n      source: {{ root: {} }}\n      dest: {{ root: {} }}\n    g2:\n      source: {{ root: {} }}\n      dest: {{ root: {} }}\n",
        a.display(), tmp.path().join("c").display(), b.display(), tmp.path().join("d").display()
    ));
    let err = eng.cycle().unwrap_err().to_string();
    assert!(err.contains("overlapping roots"), "unexpected error: {err}");
}

#[test]
fn string_shorthand_defaults_and_expansion() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "hello");

    // Bare-string endpoints. `_` supplies on_new and a `{name}`-expanded log path.
    let yaml = format!(
        "entries:\n    _:\n      on_new: \"replicate\"\n      log: \"{}/logs/{{name}}.log\"\n    g:\n      source: \"{}\"\n      dest: \"{}\"\n",
        tmp.path().display(), a.display(), b.display()
    );
    let (_, relations, logs) = config::load(&format!("state_db: \"{}\"\n{yaml}", tmp.path().join("state.db").display())).unwrap();
    assert_eq!(logs, vec![("g".to_string(), tmp.path().join("logs/g.log"))]);

    let store = Store::open(&tmp.path().join("state.db")).unwrap();
    let eng = Engine::new(store, relations, vec![]);
    eng.cycle().unwrap();
    // on_new came from the defaults: the dest copy was created.
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");

    // A local value beats the default.
    let yaml2 = "entries:\n    _:\n      on_new: \"replicate\"\n      pattern: \"*.md\"\n    h:\n      source: \"/tmp/x\"\n      dest: \"/tmp/y\"\n      pattern: \"*.js\"\n";
    let (_, relations, _) = config::load(yaml2).unwrap();
    assert!(relations[0].pattern.matches(&vec!["a.js".to_string()]));
    assert!(!relations[0].pattern.matches(&vec!["a.md".to_string()]));

    // A string shorthand containing `{holes}` is a path template.
    let yaml3 = "entries:\n    t:\n      source: \"/x/{name}.md\"\n      dest: \"/y/{name}/SKILL.md\"\n";
    assert!(config::load(yaml3).is_ok());

    // Circular placeholder references error at load.
    let yaml4 = "entries:\n    _:\n      log: \"{a}\"\n    g:\n      a: \"{b}\"\n      b: \"{a}\"\n      source: \"/tmp/x\"\n      dest: \"/tmp/y\"\n";
    let err = match config::load(yaml4) {
        Err(e) => e.root_cause().to_string(),
        Ok(_) => panic!("circular reference was accepted"),
    };
    assert!(err.contains("circular"), "unexpected error: {err}");
}

#[test]
fn missing_recorded_root_is_not_a_mass_deletion() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "precious");
    write(b.join("f.txt"), "precious");

    let eng = engine(&tmp, &format!(
        "    g:\n      source: \"{}\"\n      dest: \"{}\"\n      on_delete: \"propagate\"\n",
        a.display(), b.display()
    ));
    eng.cycle().unwrap();

    // The dest root vanishes wholesale (unmount, rename).
    // With on_delete: propagate a naive read would delete the source copy; the guard treats the root as unavailable.
    fs::remove_dir_all(&b).unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "precious");

    // When the root comes back, sync resumes normally.
    write(b.join("f.txt"), "precious");
    eng.cycle().unwrap();
    write(a.join("f.txt"), "precious v2");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "precious v2");
}

#[test]
fn markers_bracket_creates_and_deletes() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");

    let eng = engine(&tmp, &format!(
        "    g:\n      source: \"{}\"\n      dest: \"{}\"\n      on_new: \"replicate\"\n      on_delete: \"propagate\"\n",
        a.display(), b.display()
    ));

    // A create and a delete both complete: the marker is written and removed, and the marker never becomes an identifier of the relation.
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    assert_eq!(read(b.join("f.txt.replica.tmp")), None);

    fs::remove_file(a.join("f.txt")).unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    assert_eq!(read(b.join("f.txt.replica.tmp")), None);

    // A stray marker left by a crash is ignored by discovery, not synced.
    write(b.join("g.txt.replica.tmp"), "service: replica\nop: create\n");
    eng.cycle().unwrap();
    assert_eq!(read(a.join("g.txt.replica.tmp")), None);
}
