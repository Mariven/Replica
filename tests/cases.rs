//! Execute end-to-end integration scenarios against temporary directories.
//! Load relation specifications directly from YAML configurations to test parser logic alongside synchronization lifecycles, template expansions, conflict resolutions, and crash recovery routines.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use replica::config;
use replica::engine::{Engine, Event, RelationState};
use replica::store::Store;
use tempfile::TempDir;

/// Build an engine instance from a YAML snippet with paths rooted in the temporary directory.
fn engine(tmp: &TempDir, relations_yaml: &str) -> Engine {
    engine_with_events(tmp, relations_yaml).0
}

/// Events an engine emitted, as `kind relation identifier: text` lines.
type Events = Arc<Mutex<Vec<String>>>;

/// Build an engine that records every event it emits.
fn engine_with_events(tmp: &TempDir, relations_yaml: &str) -> (Engine, Events) {
    let yaml = format!("entries:\n{relations_yaml}");
    let (_, relations, _) = config::load(&yaml).unwrap();
    let store = Store::open(&tmp.path().join("state.db")).unwrap();
    let seen: Events = Arc::default();
    let sink = seen.clone();
    let hook: replica::engine::Hook = Box::new(move |e: &Event| {
        sink.lock().unwrap().push(format!("{} {} {}: {}", e.kind(), e.relation(), e.identifier(), e.text()));
    });
    (Engine::new(store, relations, vec![hook]), seen)
}

/// YAML for one entry relating `source` at `a` to `dest` at `b` under the given propagate block.
fn pair(name: &str, a: &Path, b: &Path, propagate: &str) -> String {
    format!(
        "    {name}:\n      labels: {{ source: \"{}\", dest: \"{}\" }}\n      propagate: {{ {propagate} }}\n",
        a.display(), b.display()
    )
}

/// Write string content to a destination path, creating parent directories when necessary.
fn write(path: impl AsRef<Path>, content: &str) {
    fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Read file contents into a string, returning None when the path is absent.
fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Set the modification timestamp of a path to the specified seconds past the Unix epoch.
/// Explicit timestamps establish deterministic update sequences independently of system clock elapsed time.
fn set_mtime(path: impl AsRef<Path>, secs: u64) {
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

/// Count the recorded events whose line starts with `prefix`.
fn count(events: &Events, prefix: &str) -> usize {
    events.lock().unwrap().iter().filter(|l| l.starts_with(prefix)).count()
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
        "    mylibrary:\n      labels: {{ source: {{ root: {} }}, dest: {{ root: {} }} }}\n      pattern: \"*.js\"\n      propagate: {{ change: \"source <-> dest\" }}\n",
        a.display(), b.display()
    ));

    // Run the initial cycle to index both directories, then apply modifications to verify bidirectional synchronization.
    eng.cycle().unwrap();
    write(a.join("app.js"), "console.log(2)");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("app.js")).unwrap(), "console.log(2)");

    write(b.join("util.js"), "util v2");
    eng.cycle().unwrap();
    assert_eq!(read(a.join("util.js")).unwrap(), "util v2");

    // Restrict synchronization to JavaScript files, keeping text files absent from the destination directory.
    assert_eq!(read(b.join("notes.txt")), None);
}

#[test]
fn case_2_rename_via_fixed_mapping() {
    let tmp = TempDir::new().unwrap();
    let (conf_dir, desk) = (tmp.path().join("nginx/conf.d"), tmp.path().join("Desktop"));
    write(conf_dir.join("default.conf"), "server {}");

    let eng = engine(&tmp, &format!(
        "    nginx-default:\n      labels: {{ source: {{ root: {} }}, dest: {{ root: {}, rename: nginx-conf.txt }} }}\n      pattern: default.conf\n      propagate: {{ change: \"source <-> dest\" }}\n",
        conf_dir.display(), desk.display()
    ));

    // Index the source file first, then introduce an empty destination file to populate it with source content.
    eng.cycle().unwrap();
    write(desk.join("nginx-conf.txt"), "");
    eng.cycle().unwrap();
    assert_eq!(read(desk.join("nginx-conf.txt")).unwrap(), "server {}");

    // Modify the renamed destination file to verify reverse synchronization back into the source configuration.
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

    let many: Vec<String> = projects.iter().map(|p| format!("          - {}", p.display())).collect();
    let eng = engine(&tmp, &format!(
        "    style-guide:\n      labels:\n        source: {{ root: {} }}\n        dest:\n{}\n      pattern: main_style.md\n      propagate: {{ change: \"source -> dest\" }}\n",
        rules.display(), many.join("\n")
    ));

    eng.cycle().unwrap();
    write(rules.join("main_style.md"), "v2");
    eng.cycle().unwrap();
    for p in &projects {
        assert_eq!(read(p.join("main_style.md")).unwrap(), "v2");
    }

    // Modify a destination copy directly to confirm the one-way edge reverts destination divergence to the source content.
    write(projects[1].join("main_style.md"), "local heresy");
    eng.cycle().unwrap();
    assert_eq!(read(projects[1].join("main_style.md")).unwrap(), "v2");

    // Refs sharing a label are named by their shortest unique path suffix.
    let report = eng.report();
    let names: Vec<&str> = report.relations[0].refs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["source", "dest (bouncer/docs/style)", "dest (duckmaster/docs/style)", "dest (screamio/docs/style)"]);
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
        "    style-guide-all:\n      labels:\n        source: {{ root: {} }}\n        dest:\n          scan:\n            template: \"{}/{{project}}/docs/style\"\n            hole: project\n            dir: {}\n      pattern: main_style.md\n      propagate: {{ change: \"source -> dest\", create: \"source -> dest\" }}\n",
        rules.display(), projects_dir.display(), projects_dir.display()
    ));

    eng.cycle().unwrap();
    for p in ["bouncer", "duckmaster"] {
        assert_eq!(read(projects_dir.join(p).join("docs/style/main_style.md")).unwrap(), "v1");
    }

    // Add a new project directory at runtime, allowing the scan template to discover the destination path and replicate the style guide.
    fs::create_dir_all(projects_dir.join("screamio")).unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(projects_dir.join("screamio/docs/style/main_style.md")).unwrap(), "v1");

    // Scan refs are named by their hole value.
    let report = eng.report();
    let names: Vec<&str> = report.relations[0].refs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["source", "dest (bouncer)", "dest (duckmaster)", "dest (screamio)"]);
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
        "    skills:\n      labels: {{ source: {{ path: \"{}/{{name}}.md\" }}, dest: {{ path: \"{}/{{name}}/SKILL.md\" }} }}\n      propagate: {{ change: \"source <-> dest\", create: \"source <-> dest\" }}\n",
        flat.display(), nested.display()
    ));

    // Match files across path templates, rewriting each source filename into the nested destination path structure.
    eng.cycle().unwrap();
    assert_eq!(read(nested.join("humanizer/SKILL.md")).unwrap(), "no em dashes");
    // Confirm notes.txt remains absent from the destination because its suffix deviates from the .md template.
    assert_eq!(read(nested.join("notes.txt")), None);

    // Update files on each side of the template mapping to verify bidirectional propagation through path rewrites.
    write(flat.join("prose.md"), "write plainly, v2");
    eng.cycle().unwrap();
    assert_eq!(read(nested.join("prose/SKILL.md")).unwrap(), "write plainly, v2");
    write(nested.join("prose/SKILL.md"), "write plainly, v3");
    eng.cycle().unwrap();
    assert_eq!(read(flat.join("prose.md")).unwrap(), "write plainly, v3");

    // Reject configuration entries defining mismatched template variable names between endpoints.
    let yaml = "entries:\n    bad:\n      labels: { a: { path: \"/x/{name}.md\" }, b: { path: \"/y/{other}/f.md\" } }\n      propagate: { change: \"a <-> b\" }\n";
    let err = match config::load(yaml) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("mismatched holes were accepted"),
    };
    assert!(err.contains("disagree on holes"), "unexpected error: {err}");

    // Reject configuration entries combining a templated path with a static root directory.
    let yaml = "entries:\n    bad2:\n      labels: { a: { path: \"/x/{name}.md\" }, b: { root: /y } }\n      propagate: { change: \"a <-> b\" }\n";
    assert!(config::load(yaml).is_err());
}

#[test]
fn lifecycle_create_restore_and_propagate() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "hello");
    write(a.join("only-here.txt"), "solo");
    write(b.join("f.txt"), "hello");

    let eng = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\""));

    eng.cycle().unwrap();
    assert_eq!(read(b.join("only-here.txt")), None);
    // A file present on one side with no edge to carry it is not awaiting anything.
    let report = eng.report();
    assert_eq!(report.relations[0].state(false), RelationState::InSync);
    assert!(!report.relations[0].rows.iter().find(|r| r.identifier == "only-here.txt").unwrap().needs_attention());

    fs::remove_file(b.join("f.txt")).unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")), None);

    // Create edges into dest fill the empty spot from the source copy and bring in only-here.txt.
    let eng2 = engine(&tmp, &pair("g2", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\""));
    eng2.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");
    assert_eq!(read(b.join("only-here.txt")).unwrap(), "solo");
    fs::remove_file(b.join("f.txt")).unwrap();
    eng2.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");

    // Propagate the removal of source f.txt to destination b along a delete edge.
    let eng3 = engine(&tmp, &pair("g3", &a, &b, "change: \"source <-> dest\", delete: \"source -> dest\""));
    eng3.cycle().unwrap();
    fs::remove_file(a.join("f.txt")).unwrap();
    eng3.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    eng3.cycle().unwrap();  // Purge the inert record from state and leave both endpoints empty.
    assert_eq!(read(a.join("f.txt")), None);
}

#[test]
fn conflict_skips_without_a_rule_and_mtime_breaks_the_tie() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "base");
    write(b.join("f.txt"), "base");

    let eng = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\""));
    eng.cycle().unwrap();

    write(a.join("f.txt"), "from a");
    write(b.join("f.txt"), "from b");
    eng.cycle().unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "from a");
    assert_eq!(read(b.join("f.txt")).unwrap(), "from b");

    // Resolve divergent edits by selecting the payload with the newer modification timestamp.
    let eng2 = engine(&tmp, &pair("g2", &a, &b, "change: \"source <-> dest\", conflict: \"source mtime dest\""));
    set_mtime(a.join("f.txt"), 1_000);
    set_mtime(b.join("f.txt"), 2_000);
    eng2.cycle().unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "from b");
    assert_eq!(read(b.join("f.txt")).unwrap(), "from b");
}

#[test]
fn unsettled_conflict_persists_while_the_rest_syncs() {
    let tmp = TempDir::new().unwrap();
    let dirs: Vec<_> = ["a", "b", "c", "d"].iter().map(|n| tmp.path().join(n)).collect();
    for d in &dirs {
        write(d.join("f.txt"), "base");
    }
    let (a, b, c, d) = (&dirs[0], &dirs[1], &dirs[2], &dirs[3]);

    let (eng, events) = engine_with_events(&tmp, &format!(
        "    quad:\n      labels: {{ a: \"{}\", b: \"{}\", c: \"{}\", d: \"{}\" }}\n      propagate: {{ change: \"a -> b; {{a, c}} -> d\" }}\n",
        a.display(), b.display(), c.display(), d.display()
    ));
    eng.cycle().unwrap();

    write(a.join("f.txt"), "from a");
    write(c.join("f.txt"), "from c");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "from a");
    assert_eq!(read(d.join("f.txt")).unwrap(), "base");

    // Conflict at d stays visible on later cycles even though every file is unchanged since the last one.
    eng.cycle().unwrap();
    eng.cycle().unwrap();
    let report = eng.report();
    let row = report.relations[0].rows.iter().find(|r| r.identifier == "f.txt").unwrap();
    assert!(row.note.as_deref().unwrap_or("").contains("conflict"), "note: {:?}", row.note);
    assert!(row.needs_attention());
    assert_eq!(report.relations[0].state(false), RelationState::Awaiting(1));
    assert_eq!(read(d.join("f.txt")).unwrap(), "base");
    // The unchanged conflict is reported once, not on every cycle.
    assert_eq!(count(&events, "skipped quad f.txt"), 1, "{:?}", events.lock().unwrap());

    // Once a and c agree, d conforms and the conflict clears.
    write(c.join("f.txt"), "from a");
    eng.cycle().unwrap();
    assert_eq!(read(d.join("f.txt")).unwrap(), "from a");
    let report = eng.report();
    let row = report.relations[0].rows.iter().find(|r| r.identifier == "f.txt").unwrap();
    assert_eq!(row.note, None);
    assert_eq!(report.relations[0].state(false), RelationState::InSync);
}

#[test]
fn self_writes_do_not_churn() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");

    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\""));
    eng.cycle().unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle().unwrap();
    // The resolved event names the plan, not just an op count. The first cycle only aligned the records of two identical files.
    let resolved: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("resolved")).cloned().collect();
    assert_eq!(resolved, vec![
        "resolved g f.txt: resolved: records aligned, no files changed",
        "resolved g f.txt: resolved: copy source -> dest",
    ]);

    // Match recorded destination metadata across subsequent cycles to preserve the synchronized file unmodified.
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
    let yaml = format!("entries:\n{}", pair("g", &a, &b, "change: \"source <-> dest\""));
    let (_, relations, _) = config::load(&yaml).unwrap();

    {
        let eng = Engine::new(Store::open(&db).unwrap(), relations, vec![]);
        eng.cycle().unwrap();
        // Write an uncommitted intent to the journal alongside a partial destination write to reproduce an interrupted run.
        eng.store.write_intent("g", &vec!["f.txt".into()], "copy source -> dest").unwrap();
        write(b.join("f.txt"), "torn write");
    }

    // Recover the journaled intent through an event listener before running the next synchronization cycle.
    let (_, relations, _) = config::load(&yaml).unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen2 = seen.clone();
    let eng = Engine::new(Store::open(&db).unwrap(), relations, vec![Box::new(move |e| {
        if let Event::StaleIntent { identifier, .. } = e {
            seen2.lock().unwrap().push(identifier.clone());
        }
    })]);
    let cleared = eng.recover().unwrap();
    assert_eq!(*seen.lock().unwrap(), vec!["f.txt".to_string()]);
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].2, "copy source -> dest");
    eng.cycle().unwrap();
    // Synchronize the torn destination modification back into source path a against the store baseline.
    assert_eq!(read(a.join("f.txt")).unwrap(), "torn write");
}

#[test]
fn arbitrary_labels_beyond_source_and_dest() {
    let tmp = TempDir::new().unwrap();
    let (a, b, c) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    write(c.join("f.txt"), "v1");

    // Send file updates from label first to destination endpoints configured with arbitrary and sigiled labels.
    let eng = engine(&tmp, &format!(
        "    tri:\n      labels:\n        first: {{ root: {} }}\n        second: {{ root: {} }}\n        \"$backup\": {{ root: {} }}\n      propagate: {{ change: \"first -> {{second, $backup}}\" }}\n",
        a.display(), b.display(), c.display()
    ));
    eng.cycle().unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    assert_eq!(read(c.join("f.txt")).unwrap(), "v2");

    // Reject configurations whose propagate statements reference undeclared labels.
    let rejects = |yaml: &str, needle: &str| {
        let err = match config::load(yaml) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("config was accepted:\n{yaml}"),
        };
        assert!(err.contains(needle), "unexpected error: {err}");
    };
    rejects("entries:\n    bad:\n      labels: { x: { root: /tmp/x }, y: { root: /tmp/y } }\n      propagate: { change: \"x -> z\" }\n", "undeclared");
    rejects("entries:\n    idle:\n      labels: { x: { root: /tmp/x }, y: { root: /tmp/y } }\n      propagate: { conflict: \"x > y\" }\n", "no edges");
    // Reject the retired policy keys.
    rejects("entries:\n    old:\n      labels: { x: { root: /tmp/x }, y: { root: /tmp/y } }\n      content: bidirectional\n", "unknown field");
}

#[test]
fn gate_holds_writes_and_reports_the_plan() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");

    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\""));
    eng.cycle().unwrap();

    // Close the global gate to hold write operations in the plan while keeping destination files at earlier versions.
    eng.gates.set_global(true);
    write(a.join("f.txt"), "v2");
    eng.cycle().unwrap();
    eng.cycle().unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    // The held plan is reported once while it stays the same, not on every cycle.
    assert_eq!(count(&events, "held g f.txt"), 1, "{:?}", events.lock().unwrap());

    // Inspect the generated relation report for the held plan entry corresponding to f.txt.
    let report = eng.report();
    let row = report.relations[0].rows.iter().find(|r| r.identifier == "f.txt").unwrap();
    assert_eq!(row.plan.len(), 1);
    assert_eq!(row.plan_text, "copy source -> dest");
    assert_eq!(report.relations[0].state(true), RelationState::Held(1));

    // Open the global gate and run a cycle to commit the deferred write to destination b.
    eng.gates.set_global(false);
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    // A new edit under a closed gate is reported again, since the previous hold was resolved.
    eng.gates.set_global(true);
    write(a.join("f.txt"), "v2b");
    eng.cycle().unwrap();
    eng.cycle().unwrap();
    assert_eq!(count(&events, "held g f.txt"), 2, "{:?}", events.lock().unwrap());
    eng.gates.set_global(false);
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2b");

    // Configure an initial relation gate to block synchronization from startup.
    let yaml = format!("entries:\n{}      gate: true\n", pair("g", &a, &b, "change: \"source <-> dest\""));
    let (_, relations, _) = config::load(&yaml).unwrap();
    assert!(relations[0].gate_default);
    let eng2 = Engine::new(Store::open(&tmp.path().join("state2.db")).unwrap(), relations, vec![]);
    eng2.cycle().unwrap();
    write(a.join("f.txt"), "v3");
    eng2.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2b");
    // Realign source content with destination, clear the relation gate to establish baseline state, then apply edits.
    write(a.join("f.txt"), "v2b");
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

    let mut eng = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", delete: \"source -> dest\""));
    let dir = tmp.path().join("archive");
    eng.archive = Some(Archive { dir: dir.clone(), cap: 1 << 20 });
    eng.cycle().unwrap();

    // Overwrite one file and delete another to record replaced contents and removed files in the archive.
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

    // Deduplicate identical content across different file paths in archive storage.
    write(a.join("f.txt"), "delete me");
    eng.cycle().unwrap();  // Overwrite b/f.txt and archive the previous "new content".
    write(a.join("f.txt"), "final");
    eng.cycle().unwrap();  // Archive "delete me" from b/f.txt, deduplicating against the existing gone.txt blob.
    let (rows, bytes) = eng.store.archive_stats().unwrap();
    assert_eq!(rows, 4);
    let unique: std::collections::BTreeSet<String> =
        eng.store.archive_entries("g").unwrap().into_iter().map(|e| e.hash).collect();
    assert_eq!(unique.len(), 3, "'delete me' must be stored once");
    assert_eq!(bytes as usize, "old content".len() + "new content".len() + "delete me".len());

    // Enforce a small archive size cap to evict older rows while retaining the newest entry.
    eng.archive = Some(Archive { dir: dir.clone(), cap: 1 });
    write(a.join("f.txt"), "post-cap");
    eng.cycle().unwrap();
    let entries = eng.store.archive_entries("g").unwrap();
    assert_eq!(entries.len(), 1);
    // Confirm the surviving archive entry preserves the "final" content from the overwrite.
    assert_eq!(read(dir.join(&entries[0].hash)).unwrap(), "final");
    // Verify disk cleanup for evicted blob files.
    assert!(!dir.join(blake3::hash(b"old content").to_hex().to_string()).exists());
}

#[test]
fn overlapping_roots_are_reported_and_skipped_while_the_rest_syncs() {
    let tmp = TempDir::new().unwrap();
    let (a, nested) = (tmp.path().join("a"), tmp.path().join("a/nested"));
    let (c, d) = (tmp.path().join("c"), tmp.path().join("d"));
    let (e, f) = (tmp.path().join("e"), tmp.path().join("f"));
    write(a.join("f.txt"), "x");
    write(nested.join("g.txt"), "y");
    write(e.join("h.txt"), "fine");

    let (eng, events) = engine_with_events(&tmp, &format!(
        "{}{}{}",
        pair("g1", &a, &c, "change: \"source <-> dest\", create: \"source -> dest\""),
        pair("g2", &nested, &d, "change: \"source <-> dest\", create: \"source -> dest\""),
        pair("g3", &e, &f, "change: \"source <-> dest\", create: \"source -> dest\""),
    ));
    // The cycle no longer fails as a whole: the overlapping pair is reported and skipped, the third relation syncs.
    eng.cycle().unwrap();
    assert_eq!(read(c.join("f.txt")), None);
    assert_eq!(read(d.join("g.txt")), None);
    assert_eq!(read(f.join("h.txt")).unwrap(), "fine");
    let errors: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("error")).cloned().collect();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors.iter().all(|l| l.contains("overlapping roots")), "{errors:?}");
    assert!(errors.iter().any(|l| l.starts_with("error g1 ")) && errors.iter().any(|l| l.starts_with("error g2 ")), "{errors:?}");
    // The report shows the same error on both relations, and the second cycle does not repeat it.
    let report = eng.report();
    assert!(report.relations[0].error.as_deref().unwrap_or("").contains("overlapping roots"));
    assert!(report.relations[1].error.as_deref().unwrap_or("").contains("overlapping roots"));
    assert_eq!(report.relations[0].state(false), RelationState::Error);
    assert_eq!(report.relations[2].state(false), RelationState::InSync);
    eng.cycle().unwrap();
    assert_eq!(count(&events, "error"), 2, "{:?}", events.lock().unwrap());
}

#[test]
fn rename_endpoints_claim_one_file_not_their_folder() {
    let tmp = TempDir::new().unwrap();
    let (conf, desk, lib, lib_copy) = (tmp.path().join("conf.d"), tmp.path().join("Desktop"), tmp.path().join("Desktop/mylibrary"), tmp.path().join("lib-copy"));
    write(conf.join("default.conf"), "server {}");
    write(lib.join("app.js"), "1");

    // The renamed file lands in Desktop, which also contains the mylibrary root: no overlap, both relations run.
    let eng = engine(&tmp, &format!(
        "    nginx:\n      labels: {{ source: \"{}\", dest: {{ root: \"{}\", rename: nginx-conf.txt }} }}\n      pattern: default.conf\n      propagate: {{ change: \"source <-> dest\", create: \"source -> dest\" }}\n{}",
        conf.display(), desk.display(),
        pair("lib", &lib, &lib_copy, "change: \"source <-> dest\", create: \"source -> dest\""),
    ));
    eng.cycle().unwrap();
    assert_eq!(read(desk.join("nginx-conf.txt")).unwrap(), "server {}");
    assert_eq!(read(lib_copy.join("app.js")).unwrap(), "1");
    assert!(eng.report().relations.iter().all(|r| r.error.is_none()));
}

#[test]
fn reserved_paths_inside_a_root_are_refused() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "x");
    let (mut eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\""));
    // A relation log inside a root would sync itself forever; the engine refuses the relation instead.
    eng.reserved = vec![a.join("logs/g.log")];
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    assert_eq!(count(&events, "error g"), 1, "{:?}", events.lock().unwrap());
    assert!(events.lock().unwrap()[0].contains("which replica itself writes"));
    let report = eng.report();
    assert_eq!(report.relations[0].state(false), RelationState::Error);
}

#[test]
fn string_shorthand_defaults_and_expansion() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "hello");

    // Define bare-string endpoints while inheriting the propagate defaults and expanding {name} in log paths.
    let yaml = format!(
        "entries:\n    _:\n      propagate: {{ change: \"source <-> dest\", create: \"source -> dest\" }}\n      log: \"{}/logs/{{name}}.log\"\n    g:\n      labels: {{ source: \"{}\", dest: \"{}\" }}\n",
        tmp.path().display(), a.display(), b.display()
    );
    let (_, relations, logs) = config::load(&format!("state_db: \"{}\"\n{yaml}", tmp.path().join("state.db").display())).unwrap();
    assert_eq!(logs, vec![("g".to_string(), tmp.path().join("logs/g.log"))]);

    let store = Store::open(&tmp.path().join("state.db")).unwrap();
    let eng = Engine::new(store, relations, vec![]);
    eng.cycle().unwrap();
    // Verify destination file creation through the inherited create edge.
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");

    // Override default table settings with explicit per-relation parameters.
    let yaml2 = "entries:\n    _:\n      propagate: { change: \"source <-> dest\" }\n      pattern: \"*.md\"\n    h:\n      labels: { source: \"/tmp/x\", dest: \"/tmp/y\" }\n      pattern: \"*.js\"\n";
    let (_, relations, _) = config::load(yaml2).unwrap();
    assert!(relations[0].pattern.matches(&vec!["a.js".to_string()]));
    assert!(!relations[0].pattern.matches(&vec!["a.md".to_string()]));

    // Load string shorthand paths containing placeholder holes as template configurations.
    let yaml3 = "entries:\n    t:\n      labels: { source: \"/x/{name}.md\", dest: \"/y/{name}/SKILL.md\" }\n      propagate: { change: \"source <-> dest\" }\n";
    assert!(config::load(yaml3).is_ok());

    // Reject circular placeholder references during configuration loading.
    let yaml4 = "entries:\n    _:\n      log: \"{a}\"\n    g:\n      a: \"{b}\"\n      b: \"{a}\"\n      labels: { source: \"/tmp/x\", dest: \"/tmp/y\" }\n";
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

    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", delete: \"source <-> dest\""));
    eng.cycle().unwrap();

    // Remove the entire destination directory to simulate an unmounted filesystem or renamed root.
    // Guard against missing endpoints by marking the root unavailable, preventing source deletions during delete propagation.
    fs::remove_dir_all(&b).unwrap();
    eng.cycle().unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "precious");
    // The dashboard sees the same guard: no rows, no delete plan, an unavailable reason, reported once.
    let report = eng.report();
    assert!(report.relations[0].unavailable.as_deref().unwrap_or("").contains("folder is missing"));
    assert!(report.relations[0].rows.is_empty());
    assert_eq!(report.relations[0].state(false), RelationState::Unavailable);
    assert_eq!(count(&events, "pending g"), 1, "{:?}", events.lock().unwrap());

    // Recreate the destination root to resume normal synchronization cycles.
    write(b.join("f.txt"), "precious");
    eng.cycle().unwrap();
    write(a.join("f.txt"), "precious v2");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "precious v2");
    assert!(eng.report().relations[0].unavailable.is_none());
}

#[test]
fn markers_bracket_creates_and_deletes() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");

    let eng = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\", delete: \"source -> dest\""));

    // Run creation and deletion cycles, verifying that the engine writes and removes temporary markers.
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    assert_eq!(read(b.join("f.txt.replica.tmp")), None);

    fs::remove_file(a.join("f.txt")).unwrap();
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    assert_eq!(read(b.join("f.txt.replica.tmp")), None);

    // Skip stray markers left by crashes during directory discovery.
    write(b.join("g.txt.replica.tmp"), "service: replica\nop: create\n");
    eng.cycle().unwrap();
    assert_eq!(read(a.join("g.txt.replica.tmp")), None);
}

#[test]
fn narrowed_pattern_drops_recorded_files_it_no_longer_covers() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("app.js"), "js");
    write(a.join("notes.md"), "md v1");

    let eng = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\""));
    eng.cycle().unwrap();
    assert_eq!(read(b.join("notes.md")).unwrap(), "md v1");

    // The same relation narrowed to *.js: notes.md has records, but it is no longer part of the relation.
    let eng2 = engine(&tmp, &format!("{}      pattern: \"*.js\"\n", pair("g", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\"")));
    write(a.join("notes.md"), "md v2");
    write(a.join("app.js"), "js v2");
    eng2.cycle().unwrap();
    assert_eq!(read(b.join("app.js")).unwrap(), "js v2");
    assert_eq!(read(b.join("notes.md")).unwrap(), "md v1");
    assert!(eng2.report().relations[0].rows.iter().all(|r| r.identifier != "notes.md"));
}

#[test]
fn pattern_depth_still_finds_files_in_named_subfolders() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("docs/guide.md"), "deep");
    write(a.join("docs/sub/deeper.md"), "deeper");
    write(a.join("top.md"), "top");

    let eng = engine(&tmp, &format!("{}      pattern: \"docs/*.md\"\n", pair("g", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\"")));
    eng.cycle().unwrap();
    assert_eq!(read(b.join("docs/guide.md")).unwrap(), "deep");
    assert_eq!(read(b.join("docs/sub/deeper.md")), None);
    assert_eq!(read(b.join("top.md")), None);
}

#[test]
fn new_files_take_the_donor_mode_and_overwrites_keep_the_destination_mode() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("secret.txt"), "v1");
    fs::set_permissions(a.join("secret.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    write(a.join("run.sh"), "#!/bin/sh");
    fs::set_permissions(a.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();

    let eng = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\""));
    eng.cycle().unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&b.join("secret.txt")), 0o600);
    assert_eq!(mode(&b.join("run.sh")), 0o755);

    // An overwrite keeps whatever mode the destination had, as cp does.
    fs::set_permissions(b.join("run.sh"), fs::Permissions::from_mode(0o640)).unwrap();
    write(a.join("run.sh"), "#!/bin/sh\necho v2");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("run.sh")).unwrap(), "#!/bin/sh\necho v2");
    assert_eq!(mode(&b.join("run.sh")), 0o640);
}

#[test]
fn an_edit_made_while_the_peer_is_unreadable_is_delivered_once_the_peer_returns() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", create: \"source <-> dest\""));
    eng.cycle().unwrap();
    let seeded = count(&events, "resolved");

    // dest/f.txt becomes a directory, so dest is unreadable; the edit at source must wait, not be recorded as delivered.
    fs::remove_file(b.join("f.txt")).unwrap();
    fs::create_dir_all(b.join("f.txt")).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle().unwrap();
    eng.cycle().unwrap();
    assert!(events.lock().unwrap().iter().any(|l| l.contains("waiting on dest: unreadable")), "{:?}", events.lock().unwrap());
    assert_eq!(count(&events, "resolved"), seeded, "nothing resolves while the peer is unreadable: {:?}", events.lock().unwrap());

    // The directory goes away and the old file comes back: the pending edit is delivered, not swapped.
    fs::remove_dir(b.join("f.txt")).unwrap();
    write(b.join("f.txt"), "v1");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    assert_eq!(read(a.join("f.txt")).unwrap(), "v2");
}

#[test]
fn an_edit_at_the_recovered_peer_conflicts_with_the_edit_made_while_it_was_unreadable() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    let eng = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\""));
    eng.cycle().unwrap();

    fs::remove_file(b.join("f.txt")).unwrap();
    fs::create_dir_all(b.join("f.txt")).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle().unwrap();
    // The peer returns with its own edit: both sides changed since the baseline, which is a conflict, not an overwrite.
    fs::remove_dir(b.join("f.txt")).unwrap();
    write(b.join("f.txt"), "v1-edited");
    eng.cycle().unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "v2");
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1-edited");
    let report = eng.report();
    assert!(report.relations[0].rows[0].note.as_deref().unwrap_or("").contains("conflict"));
}

#[test]
fn a_deletion_made_while_the_peer_is_unreadable_still_propagates_later() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    let eng = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", delete: \"source -> dest\""));
    eng.cycle().unwrap();

    fs::remove_file(b.join("f.txt")).unwrap();
    fs::create_dir_all(b.join("f.txt")).unwrap();
    fs::remove_file(a.join("f.txt")).unwrap();
    eng.cycle().unwrap();
    // The record at source still says the file existed, so the deletion is still pending.
    fs::remove_dir(b.join("f.txt")).unwrap();
    write(b.join("f.txt"), "v1");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    assert_eq!(read(a.join("f.txt")), None);
}

#[test]
fn records_outside_a_narrowed_pattern_are_forgotten() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("app.js"), "js");
    write(a.join("notes.md"), "md v1");
    let eng = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\", delete: \"source <-> dest\""));
    eng.cycle().unwrap();
    assert_eq!(read(b.join("notes.md")).unwrap(), "md v1");

    // Narrowed to *.js, notes.md leaves the relation and its records go with it.
    let eng2 = engine(&tmp, &format!("{}      pattern: \"*.js\"\n", pair("g", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\", delete: \"source <-> dest\"")));
    eng2.cycle().unwrap();
    assert!(eng2.store.records_for("g").unwrap().get("notes.md").is_none());
    // While narrowed, the user replaces notes.md on both sides; widening again must not read that as a deletion against the old baseline.
    fs::remove_file(a.join("notes.md")).unwrap();
    fs::remove_file(b.join("notes.md")).unwrap();
    write(a.join("notes.md"), "md v2");
    eng2.cycle().unwrap();
    let eng3 = engine(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\", create: \"source -> dest\", delete: \"source <-> dest\""));
    eng3.cycle().unwrap();
    assert_eq!(read(a.join("notes.md")).unwrap(), "md v2");
    assert_eq!(read(b.join("notes.md")).unwrap(), "md v2");
}

#[test]
fn the_report_refuses_a_fixed_mapping_with_two_files_just_like_the_cycle() {
    let tmp = TempDir::new().unwrap();
    let (conf, desk) = (tmp.path().join("conf.d"), tmp.path().join("Desktop"));
    write(conf.join("a.conf"), "a");
    write(conf.join("b.conf"), "b");
    let (eng, events) = engine_with_events(&tmp, &format!(
        "    fx:\n      labels: {{ source: \"{}\", dest: {{ root: \"{}\", rename: x.txt }} }}\n      pattern: \"*.conf\"\n      propagate: {{ change: \"source <-> dest\", create: \"source -> dest\" }}\n",
        conf.display(), desk.display()
    ));
    eng.cycle().unwrap();
    eng.cycle().unwrap();
    assert_eq!(count(&events, "error fx"), 1, "{:?}", events.lock().unwrap());
    let report = eng.report();
    assert_eq!(report.relations[0].state(false), RelationState::Error);
    assert!(report.relations[0].error.as_deref().unwrap_or("").contains("fixed requires exactly one"));
    assert!(report.relations[0].rows.is_empty());
}

#[test]
fn a_failed_write_is_an_error_with_context_and_leaves_no_journal_behind() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return;  // root ignores directory permissions
    }
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");

    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "change: \"source <-> dest\""));
    eng.cycle().unwrap();

    // Make the destination folder read-only so the staged write cannot be created.
    fs::set_permissions(&b, fs::Permissions::from_mode(0o500)).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle().unwrap();
    fs::set_permissions(&b, fs::Permissions::from_mode(0o755)).unwrap();
    let errors: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("error g f.txt")).cloned().collect();
    assert_eq!(errors.len(), 1, "{:?}", events.lock().unwrap());
    assert!(errors[0].contains("creating temp file") && errors[0].contains("f.txt"), "{}", errors[0]);
    // The intent row is cleared, no marker is left, and the records are untouched, so the next cycle retries.
    assert!(eng.store.stale_intents().unwrap().is_empty());
    assert!(fs::read_dir(&b).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("replica")));
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    eng.cycle().unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
}
