//! End-to-end scenarios for the second design. The first design had no way to express any of these:
//!     - declared fan-out under a constraint
//!     - named locations with stable identities
//!     - a file recreated over a tombstone
//!     - transforms along an edge
//!     - actions on resolved keys and changed cycles
//!     - relations stamped out with `each`
//!     - a relocated replica
//!     - a policy change forcing re-evaluation
//!     - gates persisted across restarts
//!     - ignore lists

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use replica::blobs::Blobs;
use replica::config;
use replica::engine::{Engine, RelationState};
use replica::event::{Event, Hook};
use replica::medium::Local;
use replica::store::Store;
use tempfile::TempDir;

/// The events recorded from an engine, each formatted as a `kind relation key: text` line.
type Events = Arc<Mutex<Vec<String>>>;

/// Build an engine from the relations section of a configuration, keeping state under `tmp` and recording its events.
fn engine_with_events(tmp: &TempDir, relations_yaml: &str) -> (Engine, Events) {
    engine_from(tmp, &format!("state: {}\nrelations:\n{relations_yaml}", tmp.path().join("state").display()))
}

/// Build an engine from a full configuration text and collect its events.
fn engine_from(tmp: &TempDir, yaml: &str) -> (Engine, Events) {
    let loaded = config::load(yaml, Path::new("/cfg/config.yaml")).unwrap();
    let state = loaded.settings.state.clone().unwrap();
    let store = Store::open(&state.join("state.db")).unwrap();
    let blobs = Blobs { dir: state.join("blobs"), cap: 1 << 20 };
    let seen: Events = Arc::default();
    let sink = seen.clone();
    let hook: Hook = Box::new(move |e: &Event| {
        if !matches!(e, Event::Cycle { .. }) {
            sink.lock().unwrap().push(format!("{} {} {}: {}", e.kind(), e.relation(), e.key(), e.text()));
        }
    });
    let eng = Engine::new(store, blobs, Box::new(Local), loaded.relations, vec![hook], tmp.path().to_path_buf());
    if loaded.settings.gate {
        eng.set_global_gate(true);
    }
    (eng, seen)
}

/// Build an engine from the relations section alone, discarding the events.
fn engine(tmp: &TempDir, relations_yaml: &str) -> Engine {
    engine_with_events(tmp, relations_yaml).0
}

/// Write a file, creating its parents.
fn write(path: impl AsRef<Path>, content: &str) {
    fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Read a file as text, or None when it is missing.
fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Count the recorded event lines beginning with `prefix`.
fn count(events: &Events, prefix: &str) -> usize {
    events.lock().unwrap().iter().filter(|l| l.starts_with(prefix)).count()
}

#[test]
fn declared_fanout_with_a_constraint_skips_unwanted_folders() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("Projects");
    write(tmp.path().join("rules/main.md"), "v1");
    for p in ["alpha", "beta", ".git", "node_modules"] {
        fs::create_dir_all(projects.join(p)).unwrap();
    }
    let eng = engine(&tmp, &format!(
        "    style:\n      sites:\n        source: {}/rules/main.md\n        dest: \"{}/{{project: ~[a-z]+~}}/docs/main.md\"\n      fanout: [project]\n      flow: \"source -> dest [edit create]\"\n",
        tmp.path().display(), projects.display()
    ));
    eng.cycle(None).unwrap();
    assert_eq!(read(projects.join("alpha/docs/main.md")).unwrap(), "v1");
    assert_eq!(read(projects.join("beta/docs/main.md")).unwrap(), "v1");
    assert!(!projects.join(".git/docs").exists());
    assert!(!projects.join("node_modules/docs").exists());
    let report = eng.report();
    let ids: Vec<&str> = report.relations[0].replicas.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["source", "dest[project=alpha]", "dest[project=beta]"]);
    let names: Vec<&str> = report.relations[0].replicas.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["source", "dest (alpha)", "dest (beta)"]);
    assert_eq!(report.relations[0].rows[0].key, "(the file)");
}

#[test]
fn named_locations_keep_their_identity_when_siblings_come_and_go() {
    let tmp = TempDir::new().unwrap();
    let (src, x, y) = (tmp.path().join("src"), tmp.path().join("x"), tmp.path().join("y"));
    write(src.join("a.mdc"), "v1");
    let one = format!("    rules:\n      sites:\n        source: {}/\n        dest: {{ x: {}/ }}\n      flow: \"source -> dest [edit create]\"\n", src.display(), x.display());
    let eng = engine(&tmp, &one);
    eng.cycle(None).unwrap();
    assert_eq!(read(x.join("a.mdc")).unwrap(), "v1");
    assert_eq!(eng.store.replicas_with_records("rules").unwrap(), BTreeSet::from(["dest[x]".to_string(), "source".to_string()]));

    // With a second location added, the records for x stay in place, so only y gets seeded from source.
    let two = format!("    rules:\n      sites:\n        source: {}/\n        dest: {{ x: {}/, y: {}/ }}\n      flow: \"source -> dest [edit create]\"\n", src.display(), x.display(), y.display());
    let (eng2, events) = engine_with_events(&tmp, &two);
    eng2.cycle(None).unwrap();
    assert_eq!(read(y.join("a.mdc")).unwrap(), "v1");
    let resolved: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("resolved")).cloned().collect();
    assert_eq!(resolved, vec!["resolved rules a.mdc: resolved: copy source -> dest (y)"]);
    let ids: BTreeSet<String> = eng2.store.replicas_with_records("rules").unwrap();
    assert_eq!(ids, BTreeSet::from(["dest[x]".to_string(), "dest[y]".to_string(), "source".to_string()]));
}

#[test]
fn a_file_recreated_after_a_deletion_wins_over_the_old_copy() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "old");
    write(b.join("f.txt"), "old");
    let eng = engine(&tmp, &format!("    g:\n      sites: {{ source: {}/, dest: {}/ }}\n      flow: \"source <-> dest [edit]\"\n", a.display(), b.display()));
    eng.cycle(None).unwrap();
    // The flow has no delete edge, so the deletion stays at source as a tombstone.
    fs::remove_file(a.join("f.txt")).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "old");
    // The engine reads the recreated file as an edit at source, so the new content travels to dest.
    write(a.join("f.txt"), "new");
    eng.cycle(None).unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "new");
    assert_eq!(read(b.join("f.txt")).unwrap(), "new");
}

#[test]
fn a_transform_rewrites_content_along_its_edge_and_the_receiver_stays_quiet() {
    let tmp = TempDir::new().unwrap();
    let (src, out) = (tmp.path().join("src"), tmp.path().join("out"));
    write(src.join("a.txt"), "shout");
    let (eng, events) = engine_with_events(&tmp, &format!(
        "    up:\n      sites: {{ src: {}/, out: {}/ }}\n      flow: \"src -> out [edit create delete]\"\n      transform: {{ \"src -> out\": \"tr a-z A-Z < ${{in}} > ${{out}}\" }}\n",
        src.display(), out.display()
    ));
    eng.cycle(None).unwrap();
    assert_eq!(read(out.join("a.txt")).unwrap(), "SHOUT");
    let before = count(&events, "resolved");
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(count(&events, "resolved"), before, "the transformed output reads as unchanged on quiet cycles");
    write(src.join("a.txt"), "again");
    eng.cycle(None).unwrap();
    assert_eq!(read(out.join("a.txt")).unwrap(), "AGAIN");
    fs::remove_file(src.join("a.txt")).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(out.join("a.txt")), None);
}

#[test]
fn actions_fire_once_per_resolved_key_and_once_per_changed_cycle() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    let log = tmp.path().join("actions.log");
    write(a.join("f.txt"), "1");
    write(a.join("g.txt"), "2");
    let (eng, events) = engine_with_events(&tmp, &format!(
        "    g:\n      sites: {{ source: {}/, dest: {}/ }}\n      flow: \"source -> dest [edit create]\"\n      on:\n        resolved: \"echo resolved ${{key}} ${{op}} ${{site}} >> {log}\"\n        changed: \"echo changed ${{relation}} >> {log}\"\n",
        a.display(), b.display(), log = log.display()
    ));
    eng.cycle(None).unwrap();
    let lines: Vec<String> = read(&log).unwrap().lines().map(str::to_owned).collect();
    assert_eq!(lines, vec!["resolved f.txt write dest", "resolved g.txt write dest", "changed g"]);
    assert_eq!(count(&events, "action g"), 3);
    // A quiet cycle fires no actions.
    eng.cycle(None).unwrap();
    assert_eq!(read(&log).unwrap().lines().count(), 3);
}

#[test]
fn each_stamps_out_independent_relations_with_their_own_gates() {
    let tmp = TempDir::new().unwrap();
    // Each stamped copy relates its own pair of folders, since the checker reports a folder shared by two relations as an overlap.
    for p in ["alpha", "beta"] {
        write(tmp.path().join(p).join("rules/x.mdc"), "v1");
    }
    let yaml = format!(
        "state: {state}\nvars: {{ root: {root} }}\nrelations:\n  rules:\n    each: {{ project: [alpha, beta] }}\n    sites: {{ source: \"${{root}}/${{project}}/rules/\", dest: \"${{root}}/${{project}}/.cursor/\" }}\n    flow: \"source -> dest [edit create]\"\n",
        state = tmp.path().join("state").display(), root = tmp.path().display()
    );
    let (eng, _) = engine_from(&tmp, &yaml);
    let names: Vec<&str> = eng.relations.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, vec!["rules-alpha", "rules-beta"]);
    eng.set_relation_gate("rules-beta", true);
    eng.cycle(None).unwrap();
    assert_eq!(read(tmp.path().join("alpha/.cursor/x.mdc")).unwrap(), "v1");
    assert_eq!(read(tmp.path().join("beta/.cursor/x.mdc")), None);
    let report = eng.report();
    assert_eq!(report.relations[1].state(false), RelationState::Held(1));
}

#[test]
fn a_relocated_replica_starts_over_and_is_reported() {
    let tmp = TempDir::new().unwrap();
    let (a, b, c) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    let pair = |dest: &PathBuf| format!("    g:\n      sites: {{ source: {}/, dest: {}/ }}\n      flow: \"source <-> dest [edit create]\"\n", a.display(), dest.display());
    let eng = engine(&tmp, &pair(&b));
    eng.cycle(None).unwrap();
    // dest now points at c, with different content in it. The old records for dest no longer apply, so the engine treats this as dest's first run.
    write(c.join("f.txt"), "theirs");
    let (eng2, events) = engine_with_events(&tmp, &pair(&c));
    eng2.cycle(None).unwrap();
    assert_eq!(count(&events, "relocated g"), 1, "{:?}", events.lock().unwrap());
    // A replica on its first run conforms to a peer with a recorded basis, so c takes the content at source.
    assert_eq!(read(c.join("f.txt")).unwrap(), "v1");
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1", "the old location is left alone");
}

#[test]
fn a_policy_change_re_evaluates_settled_keys() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    let eng = engine(&tmp, &format!("    g:\n      sites: {{ source: {}/, dest: {}/ }}\n      flow: \"source <-> dest [edit]\"\n", a.display(), b.display()));
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")), None, "no create edge, so nothing is copied");
    // The create edge changes the policy fingerprint, so the engine evaluates the settled key again and copies the file over.
    let eng2 = engine(&tmp, &format!("    g:\n      sites: {{ source: {}/, dest: {}/ }}\n      flow: \"source <-> dest [edit create]\"\n", a.display(), b.display()));
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
}

#[test]
fn gates_persist_across_restarts_and_the_config_can_insist_on_closed() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    let yaml = format!("    g:\n      sites: {{ source: {}/, dest: {}/ }}\n      flow: \"source -> dest [edit create]\"\n", a.display(), b.display());
    let eng = engine(&tmp, &yaml);
    eng.set_global_gate(true);
    eng.set_relation_gate("g", true);
    drop(eng);
    let eng2 = engine(&tmp, &yaml);
    assert!(eng2.gates.globally_gated() && eng2.gates.relation_flag("g"), "both gates come back closed");
    eng2.set_global_gate(false);
    eng2.set_relation_gate("g", false);
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    // With gate: true in its config, a relation starts closed on each restart, whatever the dashboard set before.
    let gated = format!("    g:\n      sites: {{ source: {}/, dest: {}/ }}\n      flow: \"source -> dest [edit create]\"\n      gate: true\n", a.display(), b.display());
    let eng3 = engine(&tmp, &gated);
    assert!(eng3.gates.relation_flag("g"));
}

#[test]
fn ignore_globs_keep_files_out_of_every_relation() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(a.join(".DS_Store"), "junk");
    write(a.join("sub/.DS_Store"), "junk");
    write(a.join("sub/g.txt"), "v1");
    write(a.join("notes.tmp"), "scratch");
    let yaml = format!(
        "state: {}\nignore: [\".DS_Store\", \"**/.DS_Store\"]\nrelations:\n  g:\n    sites: {{ source: {}/, dest: {}/ }}\n    ignore: [\"*.tmp\"]\n    flow: \"source -> dest [edit create]\"\n",
        tmp.path().join("state").display(), a.display(), b.display()
    );
    let (eng, _) = engine_from(&tmp, &yaml);
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    assert_eq!(read(b.join("sub/g.txt")).unwrap(), "v1");
    assert!(!b.join(".DS_Store").exists() && !b.join("sub/.DS_Store").exists() && !b.join("notes.tmp").exists());
    assert!(eng.report().relations[0].rows.iter().all(|r| !r.key.contains("DS_Store") && !r.key.ends_with(".tmp")));
}

#[test]
fn where_constraints_can_exclude_with_a_leading_bang() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("app.js"), "1");
    write(a.join("app.min.js"), "2");
    write(a.join("readme.md"), "3");
    let eng = engine(&tmp, &format!(
        "    g:\n      sites: {{ source: {}/, dest: {}/ }}\n      where: {{ path: [\"*.js\", \"!*.min.js\"] }}\n      flow: \"source -> dest [edit create]\"\n", a.display(), b.display()
    ));
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("app.js")).unwrap(), "1");
    assert!(!b.join("app.min.js").exists() && !b.join("readme.md").exists());
}

#[test]
fn a_cycle_over_named_relations_leaves_the_others_alone() {
    let tmp = TempDir::new().unwrap();
    let (a, b, c, d) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"), tmp.path().join("d"));
    write(a.join("f.txt"), "1");
    write(c.join("f.txt"), "1");
    let eng = engine(&tmp, &format!(
        "    one:\n      sites: {{ source: {}/, dest: {}/ }}\n      flow: \"source -> dest [edit create]\"\n    two:\n      sites: {{ source: {}/, dest: {}/ }}\n      flow: \"source -> dest [edit create]\"\n",
        a.display(), b.display(), c.display(), d.display()
    ));
    eng.cycle(Some(&BTreeSet::from(["one".to_string()]))).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "1");
    assert_eq!(read(d.join("f.txt")), None);
    eng.cycle(None).unwrap();
    assert_eq!(read(d.join("f.txt")).unwrap(), "1");
}

#[test]
fn a_replica_discovered_under_a_closed_gate_is_filled_once_the_gate_opens() {
    let tmp = TempDir::new().unwrap();
    let projects = tmp.path().join("Projects");
    write(tmp.path().join("rules/main.md"), "v1");
    fs::create_dir_all(projects.join("alpha")).unwrap();
    let eng = engine(&tmp, &format!(
        "    style:\n      sites:\n        source: {}/rules/main.md\n        dest: \"{}/{{project}}/docs/main.md\"\n      fanout: [project]\n      flow: \"source -> dest [edit create]\"\n",
        tmp.path().display(), projects.display()
    ));
    eng.cycle(None).unwrap();
    assert_eq!(read(projects.join("alpha/docs/main.md")).unwrap(), "v1");
    // A new project appears while the gate is closed: the plan is held, and the replica must still count as new afterwards.
    eng.set_global_gate(true);
    fs::create_dir_all(projects.join("beta")).unwrap();
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(projects.join("beta/docs/main.md")), None);
    assert_eq!(eng.report().relations[0].state(true), RelationState::Held(1));
    eng.set_global_gate(false);
    eng.cycle(None).unwrap();
    assert_eq!(read(projects.join("beta/docs/main.md")).unwrap(), "v1");
}
