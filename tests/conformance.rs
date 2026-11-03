//! End-to-end tests of the core sync scenarios, run over temporary folders. The tests named case_1 to case_5 cover the use cases the project was built for: two folders both ways, a renamed file, a mirror into fixed folders, discovered fan-out, and a path rewrite.
//! Each test loads relations from YAML and builds an engine over a fresh state database. It then runs cycles and checks the files, records, report, and events.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use replica::blobs::Blobs;
use replica::config::{self, Loaded};
use replica::engine::{Engine, RelationState};
use replica::event::{Event, Hook};
use replica::medium::Local;
use replica::observe::hex;
use replica::site::Key;
use replica::store::Store;
use tempfile::TempDir;

/// Load configuration text as the daemon does, with a fixed file path for error messages.
fn load(yaml: &str) -> anyhow::Result<Loaded> {
    config::load(yaml, Path::new("/cfg/config.yaml"))
}

/// The error text from loading a configuration. Panics if the loader accepts it.
fn load_error(yaml: &str) -> String {
    match load(yaml) {
        Err(e) => format!("{e:#}"),
        Ok(_) => panic!("config was accepted:\n{yaml}"),
    }
}

/// Build an engine over relations given as YAML, with the state database and blob store inside the temporary directory.
fn engine(tmp: &TempDir, relations_yaml: &str) -> Engine {
    engine_with_events(tmp, relations_yaml).0
}

/// Events recorded from an engine, each as a `kind relation key: text` line.
type Events = Arc<Mutex<Vec<String>>>;

/// Build an engine over the relations, with a hook that records each event it emits.
fn engine_with_events(tmp: &TempDir, relations_yaml: &str) -> (Engine, Events) {
    let loaded = load(&format!("relations:\n{relations_yaml}")).unwrap();
    let seen: Events = Arc::default();
    let sink = seen.clone();
    let hook: Hook = Box::new(move |e: &Event| {
        sink.lock().unwrap().push(format!("{} {} {}: {}", e.kind(), e.relation(), e.key(), e.text()));
    });
    (build(tmp, &tmp.path().join("state.db"), loaded, vec![hook]), seen)
}

/// Build an engine on the local filesystem from loaded relations, with its store at `db`, its blob store inside the temporary directory, and the temporary directory as its working directory.
fn build(tmp: &TempDir, db: &Path, loaded: Loaded, hooks: Vec<Hook>) -> Engine {
    let blobs = Blobs { dir: tmp.path().join("blobs"), cap: 1 << 20 };
    Engine::new(Store::open(db).unwrap(), blobs, Box::new(Local), loaded.relations, hooks, tmp.path().to_path_buf())
}

/// YAML for a relation with folder `a` as its `source` site, folder `b` as its `dest` site, and the given flow. Callers can append more fields at the same indentation.
fn pair(name: &str, a: &Path, b: &Path, flow: &str) -> String {
    format!("    {name}:\n      sites: {{ source: \"{}/\", dest: \"{}/\" }}\n      flow: \"{flow}\"\n", a.display(), b.display())
}

/// Write text to a path, creating parent directories as needed.
fn write(path: impl AsRef<Path>, content: &str) {
    fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// The text of a file, or None if it is missing.
fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Set a file's modification time to `secs` seconds after the Unix epoch, so tests can order files by mtime without depending on how fast they run.
fn set_mtime(path: impl AsRef<Path>, secs: u64) {
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

/// Count the recorded event lines starting with `prefix`.
fn count(events: &Events, prefix: &str) -> usize {
    events.lock().unwrap().iter().filter(|l| l.starts_with(prefix)).count()
}

/// The display names of a relation's replicas, in the order the report lists them.
fn replica_names(eng: &Engine, relation: usize) -> Vec<String> {
    eng.report().relations[relation].replicas.iter().map(|r| r.name.clone()).collect()
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
        "    mylibrary:\n      sites: {{ source: \"{}/\", dest: \"{}/\" }}\n      where: {{ path: \"*.js\" }}\n      flow: \"source <-> dest [edit]\"\n",
        a.display(), b.display()
    ));

    // The first cycle records both folders. After that, an edit on either side travels to the other.
    eng.cycle(None).unwrap();
    write(a.join("app.js"), "console.log(2)");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("app.js")).unwrap(), "console.log(2)");

    write(b.join("util.js"), "util v2");
    eng.cycle(None).unwrap();
    assert_eq!(read(a.join("util.js")).unwrap(), "util v2");

    // The constraint matches only .js files, so notes.txt is not copied.
    assert_eq!(read(b.join("notes.txt")), None);
}

#[test]
fn case_2_rename_via_two_literal_files() {
    let tmp = TempDir::new().unwrap();
    let (conf_dir, desk) = (tmp.path().join("nginx/conf.d"), tmp.path().join("Desktop"));
    write(conf_dir.join("default.conf"), "server {}");

    // Each site is a single file, so two files with different names are related.
    let eng = engine(&tmp, &format!(
        "    nginx-default:\n      sites: {{ source: \"{}/default.conf\", dest: \"{}/nginx-conf.txt\" }}\n      flow: \"source <-> dest [edit]\"\n",
        conf_dir.display(), desk.display()
    ));

    // The first cycle records the source file. An empty file that then appears at dest is new, so it takes the source content.
    eng.cycle(None).unwrap();
    write(desk.join("nginx-conf.txt"), "");
    eng.cycle(None).unwrap();
    assert_eq!(read(desk.join("nginx-conf.txt")).unwrap(), "server {}");

    // An edit to the Desktop copy travels back to default.conf.
    write(desk.join("nginx-conf.txt"), "server { listen 80; }");
    eng.cycle(None).unwrap();
    assert_eq!(read(conf_dir.join("default.conf")).unwrap(), "server { listen 80; }");
}

#[test]
fn case_3_explicit_fan_out_mirror() {
    let tmp = TempDir::new().unwrap();
    let rules = tmp.path().join("rules");
    let projects: Vec<PathBuf> = ["bouncer", "duckmaster", "screamio"].iter().map(|p| tmp.path().join(p).join("docs/style")).collect();
    write(rules.join("main_style.md"), "v1");
    for p in &projects {
        write(p.join("main_style.md"), "v1");
    }

    // The dest site lists several patterns, and each becomes a replica labeled with its pattern text.
    let listed: Vec<String> = projects.iter().map(|p| format!("          - \"{}/main_style.md\"", p.display())).collect();
    let eng = engine(&tmp, &format!(
        "    style-guide:\n      sites:\n        source: \"{}/main_style.md\"\n        dest:\n{}\n      flow: \"source -> dest [edit]\"\n",
        rules.display(), listed.join("\n")
    ));

    eng.cycle(None).unwrap();
    write(rules.join("main_style.md"), "v2");
    eng.cycle(None).unwrap();
    for p in &projects {
        assert_eq!(read(p.join("main_style.md")).unwrap(), "v2");
    }

    // An edit at a dest replica has no edge to leave by, so it is reverted to the source content.
    write(projects[1].join("main_style.md"), "local heresy");
    eng.cycle(None).unwrap();
    assert_eq!(read(projects[1].join("main_style.md")).unwrap(), "v2");

    // Replicas of the same site are named by their label, which here is the pattern as written in the configuration.
    let mut expected = vec!["source".to_string()];
    expected.extend(projects.iter().map(|p| format!("dest ({}/main_style.md)", p.display())));
    assert_eq!(replica_names(&eng, 0), expected);
}

/// YAML for the case 4 relation, which copies a single source file into each project folder under `projects_dir`.
fn style_guide_fan_out(rules: &Path, projects_dir: &Path) -> String {
    format!(
        "    style-guide-all:\n      sites:\n        source: \"{}/main_style.md\"\n        dest: \"{}/{{project}}/docs/style/main_style.md\"\n      fanout: [project]\n      flow: \"source -> dest [edit create]\"\n",
        rules.display(), projects_dir.display()
    )
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

    // The fanout variable matches each folder under Projects, giving a replica per project.
    let eng = engine(&tmp, &style_guide_fan_out(&rules, &projects_dir));

    eng.cycle(None).unwrap();
    for p in ["bouncer", "duckmaster"] {
        assert_eq!(read(projects_dir.join(p).join("docs/style/main_style.md")).unwrap(), "v1");
    }

    // A project folder added later is found on the next cycle and gets the style guide over the create edge.
    fs::create_dir_all(projects_dir.join("screamio")).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(projects_dir.join("screamio/docs/style/main_style.md")).unwrap(), "v1");

    // Fanout replicas are named by the value of the fanout variable.
    assert_eq!(replica_names(&eng, 0), vec!["source", "dest (bouncer)", "dest (duckmaster)", "dest (screamio)"]);
}

#[test]
fn case_4_discovered_fan_out_seeds_the_projects_present_at_start() {
    let tmp = TempDir::new().unwrap();
    let rules = tmp.path().join("rules");
    let projects_dir = tmp.path().join("Projects");
    write(rules.join("main_style.md"), "v1");
    for p in ["bouncer", "duckmaster"] {
        write(projects_dir.join(p).join("docs/style/main_style.md"), "old");
    }
    // A project folder without a docs folder still gets a replica, because the variable matches the folders directly under Projects.
    fs::create_dir_all(projects_dir.join("screamio")).unwrap();

    // The first cycle copies the source content into each project. screamio has no file yet, so it gets one over the create edge.
    let eng = engine(&tmp, &style_guide_fan_out(&rules, &projects_dir));
    eng.cycle(None).unwrap();
    for p in ["bouncer", "duckmaster", "screamio"] {
        assert_eq!(read(projects_dir.join(p).join("docs/style/main_style.md")).unwrap(), "v1");
    }

    // A later edit at the source reaches each project. An edit at a project has no edge to leave by, so it is reverted.
    write(rules.join("main_style.md"), "v2");
    eng.cycle(None).unwrap();
    write(projects_dir.join("bouncer/docs/style/main_style.md"), "local heresy");
    eng.cycle(None).unwrap();
    for p in ["bouncer", "duckmaster", "screamio"] {
        assert_eq!(read(projects_dir.join(p).join("docs/style/main_style.md")).unwrap(), "v2");
    }

    // Fanout replicas are named by the value of the fanout variable and listed in folder order.
    assert_eq!(replica_names(&eng, 0), vec!["source", "dest (bouncer)", "dest (duckmaster)", "dest (screamio)"]);
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
        "    skills:\n      sites: {{ source: \"{}/{{name}}.md\", dest: \"{}/{{name}}/SKILL.md\" }}\n      flow: \"source <-> dest [edit create]\"\n",
        flat.display(), nested.display()
    ));

    // The value of `{name}` from a source file name is substituted into the nested dest pattern.
    eng.cycle(None).unwrap();
    assert_eq!(read(nested.join("humanizer/SKILL.md")).unwrap(), "no em dashes");
    // The text file does not match `{name}.md`, so it is not a key of the relation.
    assert_eq!(read(nested.join("notes.txt")), None);

    // Edits travel between the flat and nested names in both directions.
    write(flat.join("prose.md"), "write plainly, v2");
    eng.cycle(None).unwrap();
    assert_eq!(read(nested.join("prose/SKILL.md")).unwrap(), "write plainly, v2");
    write(nested.join("prose/SKILL.md"), "write plainly, v3");
    eng.cycle(None).unwrap();
    assert_eq!(read(flat.join("prose.md")).unwrap(), "write plainly, v3");

    // A variable on only some sites is rejected with an error that names it.
    let err = load_error("relations:\n    bad:\n      sites: { a: \"/x/{name}.md\", b: \"/y/{other}/f.md\" }\n      flow: \"a <-> b\"\n");
    assert!(err.contains("{name}") || err.contains("{other}"), "unexpected error: {err}");

    // A `{name}` pattern paired with a whole folder is rejected for the same reason.
    assert!(load("relations:\n    bad2:\n      sites: { a: \"/x/{name}.md\", b: \"/y/\" }\n      flow: \"a <-> b\"\n").is_err());
}

#[test]
fn lifecycle_create_restore_and_propagate() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "hello");
    write(a.join("only-here.txt"), "solo");
    write(b.join("f.txt"), "hello");

    let eng = engine(&tmp, &pair("g", &a, &b, "source <-> dest [edit]"));

    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("only-here.txt")), None);
    // With no create edge, only-here.txt stays where it is and the relation still counts as in sync.
    let report = eng.report();
    assert_eq!(report.relations[0].state(false), RelationState::InSync);
    assert!(!report.relations[0].rows.iter().find(|r| r.key == "only-here.txt").unwrap().needs_attention());

    // Without a create edge, a deleted copy stays deleted.
    fs::remove_file(b.join("f.txt")).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")), None);

    // With a create edge into dest, the missing f.txt is restored from source and only-here.txt is copied over. A later deletion at dest is restored the same way.
    let eng2 = engine(&tmp, &pair("g2", &a, &b, "source <-> dest [edit]; source -> dest [create]"));
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");
    assert_eq!(read(b.join("only-here.txt")).unwrap(), "solo");
    fs::remove_file(b.join("f.txt")).unwrap();
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");

    // With a delete edge, deleting the file at source deletes it at dest too.
    let eng3 = engine(&tmp, &pair("g3", &a, &b, "source <-> dest [edit]; source -> dest [delete]"));
    eng3.cycle(None).unwrap();
    fs::remove_file(a.join("f.txt")).unwrap();
    eng3.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    eng3.cycle(None).unwrap();  // The key is missing everywhere now, so its records are cleared.
    assert_eq!(read(a.join("f.txt")), None);
}

#[test]
fn conflict_skips_without_a_rule_and_mtime_breaks_the_tie() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "base");
    write(b.join("f.txt"), "base");

    let eng = engine(&tmp, &pair("g", &a, &b, "source <-> dest [edit]"));
    eng.cycle(None).unwrap();

    // Different edits on both sides are a conflict, so the key is held and each file keeps its content.
    write(a.join("f.txt"), "from a");
    write(b.join("f.txt"), "from b");
    eng.cycle(None).unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "from a");
    assert_eq!(read(b.join("f.txt")).unwrap(), "from b");

    // With an mtime rule, the newer file wins.
    let eng2 = engine(&tmp, &format!("{}      resolve: \"source mtime dest\"\n", pair("g2", &a, &b, "source <-> dest [edit]")));
    set_mtime(a.join("f.txt"), 1_000);
    set_mtime(b.join("f.txt"), 2_000);
    eng2.cycle(None).unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "from b");
    assert_eq!(read(b.join("f.txt")).unwrap(), "from b");
}

#[test]
fn unsettled_conflict_persists_while_the_rest_syncs() {
    let tmp = TempDir::new().unwrap();
    let dirs: Vec<PathBuf> = ["a", "b", "c", "d"].iter().map(|n| tmp.path().join(n)).collect();
    for d in &dirs {
        write(d.join("f.txt"), "base");
    }
    let (a, b, c, d) = (&dirs[0], &dirs[1], &dirs[2], &dirs[3]);

    let (eng, events) = engine_with_events(&tmp, &format!(
        "    quad:\n      sites: {{ a: \"{}/\", b: \"{}/\", c: \"{}/\", d: \"{}/\" }}\n      flow: \"a -> b [edit]; {{a, c}} -> d [edit]\"\n",
        a.display(), b.display(), c.display(), d.display()
    ));
    eng.cycle(None).unwrap();

    // Edits from a and c both reach d, so d is held while b takes the edit from a.
    write(a.join("f.txt"), "from a");
    write(c.join("f.txt"), "from c");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "from a");
    assert_eq!(read(d.join("f.txt")).unwrap(), "base");

    // The conflict at d stays on the report in later cycles, even though no file has changed.
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    let report = eng.report();
    let row = report.relations[0].rows.iter().find(|r| r.key == "f.txt").unwrap();
    assert!(row.note.as_deref().unwrap_or("").contains("conflict"), "note: {:?}", row.note);
    assert!(row.needs_attention());
    assert_eq!(report.relations[0].state(false), RelationState::Awaiting(1));
    assert_eq!(read(d.join("f.txt")).unwrap(), "base");
    // The conflict is reported once, when it first appears.
    assert_eq!(count(&events, "skipped quad f.txt"), 1, "{:?}", events.lock().unwrap());

    // Once a and c agree, d takes their content and the conflict clears.
    write(c.join("f.txt"), "from a");
    eng.cycle(None).unwrap();
    assert_eq!(read(d.join("f.txt")).unwrap(), "from a");
    let report = eng.report();
    let row = report.relations[0].rows.iter().find(|r| r.key == "f.txt").unwrap();
    assert_eq!(row.note, None);
    assert_eq!(report.relations[0].state(false), RelationState::InSync);
}

#[test]
fn self_writes_do_not_churn() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");

    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "source <-> dest [edit]"));
    eng.cycle(None).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    // Each resolved event carries its plan text. The first cycle found two identical files and only recorded them.
    let resolved: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("resolved")).cloned().collect();
    assert_eq!(resolved, vec![
        "resolved g f.txt: resolved: records aligned, no files changed",
        "resolved g f.txt: resolved: copy source -> dest",
    ]);

    // The copy at dest matches its record on later cycles, so it is left alone.
    let b_mtime = fs::metadata(b.join("f.txt")).unwrap().modified().unwrap();
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
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
    let yaml = format!("relations:\n{}", pair("g", &a, &b, "source <-> dest [edit]"));

    {
        let eng = build(&tmp, &db, load(&yaml).unwrap(), vec![]);
        eng.cycle(None).unwrap();
        // An intent row with no commit and a half-written file at dest simulate a process killed mid-plan.
        eng.store.write_intent("g", &Key::parse("path=f.txt"), "copy source -> dest").unwrap();
        write(b.join("f.txt"), "torn write");
    }

    // A new engine reports the stale intent through its hook and clears it. The next cycle then reconciles the key from the files on disk.
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen2 = seen.clone();
    let hook: Hook = Box::new(move |e: &Event| {
        if let Event::StaleIntent { key, .. } = e {
            seen2.lock().unwrap().push(key.clone());
        }
    });
    let eng = build(&tmp, &db, load(&yaml).unwrap(), vec![hook]);
    let cleared = eng.recover().unwrap();
    assert_eq!(*seen.lock().unwrap(), vec!["f.txt".to_string()]);
    assert_eq!(cleared.len(), 1);
    assert_eq!(cleared[0].detail, "copy source -> dest");
    eng.cycle(None).unwrap();
    // The torn write differs from its record, so it counts as an edit at dest and is copied to source.
    assert_eq!(read(a.join("f.txt")).unwrap(), "torn write");
}

#[test]
fn arbitrary_site_names_beyond_source_and_dest() {
    let tmp = TempDir::new().unwrap();
    let (a, b, c) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    write(c.join("f.txt"), "v1");

    // Sites can have any name, even one starting with a dollar sign. The flow lists two of them in braces.
    let eng = engine(&tmp, &format!(
        "    tri:\n      sites:\n        first: \"{}/\"\n        second: \"{}/\"\n        \"$backup\": \"{}/\"\n      flow: \"first -> {{second, $backup}} [edit]\"\n",
        a.display(), b.display(), c.display()
    ));
    eng.cycle(None).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    assert_eq!(read(c.join("f.txt")).unwrap(), "v2");

    // The loader rejects a flow that names an undeclared site, a relation with no flow, and an unknown field.
    let rejects = |yaml: &str, needle: &str| {
        let err = load_error(yaml);
        assert!(err.contains(needle), "unexpected error: {err}");
    };
    rejects("relations:\n    bad:\n      sites: { x: /tmp/x/, y: /tmp/y/ }\n      flow: \"x -> z\"\n", "undeclared");
    rejects("relations:\n    idle:\n      sites: { x: /tmp/x/, y: /tmp/y/ }\n      resolve: \"x > y\"\n", "carries nothing");
    rejects("relations:\n    old:\n      sites: { x: /tmp/x/, y: /tmp/y/ }\n      flow: \"x <-> y\"\n      content: bidirectional\n", "unknown field");
}

#[test]
fn gate_holds_writes_and_reports_the_plan() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");

    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "source <-> dest [edit]"));
    eng.cycle(None).unwrap();

    // While the global gate is closed, the plan is computed and reported, but dest keeps its old content.
    eng.gates.set_global(true);
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    // The held plan is reported once, even though three cycles held it.
    assert_eq!(count(&events, "held g f.txt"), 1, "{:?}", events.lock().unwrap());

    // The report shows the held plan on the row for f.txt.
    let report = eng.report();
    let row = report.relations[0].rows.iter().find(|r| r.key == "f.txt").unwrap();
    assert_eq!(row.plan.len(), 1);
    assert_eq!(row.plan_text, "copy source -> dest");
    assert_eq!(report.relations[0].state(true), RelationState::Held(1));

    // After the gate opens, the next cycle applies the held write.
    eng.gates.set_global(false);
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    // A new edit under a closed gate is reported as held again, because the earlier hold was resolved.
    eng.gates.set_global(true);
    write(a.join("f.txt"), "v2b");
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(count(&events, "held g f.txt"), 2, "{:?}", events.lock().unwrap());
    eng.gates.set_global(false);
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2b");

    // With `gate: true` in the configuration, the relation starts with its own gate closed.
    let loaded = load(&format!("relations:\n{}      gate: true\n", pair("g", &a, &b, "source <-> dest [edit]"))).unwrap();
    assert!(loaded.relations[0].gate_default);
    let eng2 = build(&tmp, &tmp.path().join("state2.db"), loaded, vec![]);
    eng2.cycle(None).unwrap();
    write(a.join("f.txt"), "v3");
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2b");
    // A gated cycle records nothing, so source is set back to match dest before the gate opens. The first ungated cycle then records matching files.
    write(a.join("f.txt"), "v2b");
    eng2.set_relation_gate("g", false);
    eng2.cycle(None).unwrap();
    write(a.join("f.txt"), "v3");
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v3");
}

#[test]
fn archive_keeps_overwritten_and_deleted_content() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "old content");
    write(b.join("f.txt"), "old content");
    write(a.join("gone.txt"), "delete me");
    write(b.join("gone.txt"), "delete me");

    let mut eng = engine(&tmp, &pair("g", &a, &b, "source <-> dest [edit]; source -> dest [delete]"));
    let dir = eng.blobs.dir.clone();
    eng.cycle(None).unwrap();

    // When a file at dest is overwritten or deleted, its old content goes into the blob store with an archive row.
    write(a.join("f.txt"), "new content");
    fs::remove_file(a.join("gone.txt")).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "new content");
    assert_eq!(read(b.join("gone.txt")), None);

    let entries = eng.store.archive_entries("g").unwrap();
    assert_eq!(entries.len(), 2);
    let overwritten = entries.iter().find(|e| e.reason == "overwritten").unwrap();
    assert_eq!(read(dir.join(hex(&overwritten.hash))).unwrap(), "old content");
    assert!(overwritten.path.contains("b/f.txt") || overwritten.path.ends_with("f.txt"));
    let deleted = entries.iter().find(|e| e.reason == "deleted").unwrap();
    assert_eq!(read(dir.join(hex(&deleted.hash))).unwrap(), "delete me");

    // Identical content archived from different paths is stored once.
    write(a.join("f.txt"), "delete me");
    eng.cycle(None).unwrap();  // b/f.txt is overwritten and "new content" is archived.
    write(a.join("f.txt"), "final");
    eng.cycle(None).unwrap();  // b/f.txt is overwritten again and "delete me" is archived, but its blob is already stored from gone.txt.
    let (rows, bytes) = eng.store.archive_stats().unwrap();
    assert_eq!(rows, 4);
    let unique: std::collections::BTreeSet<String> = eng.store.archive_entries("g").unwrap().into_iter().map(|e| hex(&e.hash)).collect();
    assert_eq!(unique.len(), 3, "'delete me' must be stored once");
    assert_eq!(bytes as usize, "old content".len() + "new content".len() + "delete me".len());

    // With a tiny cap, the older rows are evicted and only the newest row is kept.
    eng.blobs = Blobs { dir: dir.clone(), cap: 1 };
    write(a.join("f.txt"), "post-cap");
    eng.cycle(None).unwrap();
    let entries = eng.store.archive_entries("g").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(read(dir.join(hex(&entries[0].hash))).unwrap(), "final");
    // A blob is deleted from disk when its last archive row is evicted.
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

    let flow = "source <-> dest [edit]; source -> dest [create]";
    let (eng, events) = engine_with_events(&tmp, &format!("{}{}{}", pair("g1", &a, &c, flow), pair("g2", &nested, &d, flow), pair("g3", &e, &f, flow)));
    // The cycle succeeds. The two relations with overlapping roots are reported and skipped while g3 syncs.
    eng.cycle(None).unwrap();
    assert_eq!(read(c.join("f.txt")), None);
    assert_eq!(read(d.join("g.txt")), None);
    assert_eq!(read(f.join("h.txt")).unwrap(), "fine");
    let errors: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("error")).cloned().collect();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors.iter().all(|l| l.contains("overlapping roots")), "{errors:?}");
    assert!(errors.iter().any(|l| l.starts_with("error g1 ")) && errors.iter().any(|l| l.starts_with("error g2 ")), "{errors:?}");
    // The report shows the error on both relations. A second cycle emits no new error events.
    let report = eng.report();
    assert!(report.relations[0].error.as_deref().unwrap_or("").contains("overlapping roots"));
    assert!(report.relations[1].error.as_deref().unwrap_or("").contains("overlapping roots"));
    assert_eq!(report.relations[0].state(false), RelationState::Error);
    assert_eq!(report.relations[2].state(false), RelationState::InSync);
    eng.cycle(None).unwrap();
    assert_eq!(count(&events, "error"), 2, "{:?}", events.lock().unwrap());
}

#[test]
fn literal_file_patterns_claim_one_file_not_their_folder() {
    let tmp = TempDir::new().unwrap();
    let (conf, desk, lib, lib_copy) = (tmp.path().join("conf.d"), tmp.path().join("Desktop"), tmp.path().join("Desktop/mylibrary"), tmp.path().join("lib-copy"));
    write(conf.join("default.conf"), "server {}");
    write(lib.join("app.js"), "1");

    // nginx-conf.txt goes into Desktop, which also holds the mylibrary root. A single-file site covers only its file, so the roots do not overlap and both relations run.
    let flow = "source <-> dest [edit]; source -> dest [create]";
    let eng = engine(&tmp, &format!(
        "    nginx:\n      sites: {{ source: \"{}/default.conf\", dest: \"{}/nginx-conf.txt\" }}\n      flow: \"{flow}\"\n{}",
        conf.display(), desk.display(), pair("lib", &lib, &lib_copy, flow)
    ));
    eng.cycle(None).unwrap();
    assert_eq!(read(desk.join("nginx-conf.txt")).unwrap(), "server {}");
    assert_eq!(read(lib_copy.join("app.js")).unwrap(), "1");
    assert!(eng.report().relations.iter().all(|r| r.error.is_none()));
}

#[test]
fn reserved_paths_inside_a_root_are_refused() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "x");
    let (mut eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "source <-> dest [edit]; source -> dest [create]"));
    // A relation log inside a root is rejected, since each write to the log shows up as an edit on the next cycle.
    eng.reserved = vec![a.join("logs/g.log")];
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    assert_eq!(count(&events, "error g"), 1, "{:?}", events.lock().unwrap());
    assert!(events.lock().unwrap()[0].contains("which replica itself writes"));
    let report = eng.report();
    assert_eq!(report.relations[0].state(false), RelationState::Error);
}

#[test]
fn profile_defaults_and_variable_expansion() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "hello");

    // The relation gets its flow and log from the default profile. The log path expands `${state}` and `${relation}`.
    let state = tmp.path().join("live");
    let yaml = format!(
        "state: \"{}\"\nprofiles:\n    _:\n      flow: \"source <-> dest [edit]; source -> dest [create]\"\n      log: \"${{state}}/logs/${{relation}}.log\"\nrelations:\n    g:\n      sites: {{ source: \"{}/\", dest: \"{}/\" }}\n",
        state.display(), a.display(), b.display()
    );
    let loaded = load(&yaml).unwrap();
    assert_eq!(loaded.relations[0].log.as_deref(), Some(state.join("logs/g.log").as_path()));
    assert_eq!(config::reserved_paths(&loaded.settings, &loaded.relations), vec![state.clone(), state.join("logs/g.log")]);

    let eng = build(&tmp, &loaded.settings.db_path().unwrap(), loaded, vec![]);
    eng.cycle(None).unwrap();
    // The create edge from the profile copies the file to dest.
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");

    // A field set on the relation overrides the same field in the profile.
    let yaml2 = "profiles:\n    _:\n      flow: \"source <-> dest [edit]\"\n      where: { path: \"*.md\" }\nrelations:\n    h:\n      sites: { source: /tmp/x/, dest: /tmp/y/ }\n      where: { path: \"*.js\" }\n";
    let loaded = load(yaml2).unwrap();
    let pattern = &loaded.relations[0].sites[0].locations[0].pattern;
    assert!(pattern.matches(Path::new("/tmp/x/a.js")).is_some());
    assert!(pattern.matches(Path::new("/tmp/x/a.md")).is_none());

    // Patterns with variables load as plain YAML strings.
    let yaml3 = "relations:\n    t:\n      sites: { source: \"/x/{name}.md\", dest: \"/y/{name}/SKILL.md\" }\n      flow: \"source <-> dest [edit]\"\n";
    assert!(load(yaml3).is_ok());

    // Variables that refer to each other in a loop are rejected.
    let yaml4 = "relations:\n    g:\n      vars: { a: \"${b}\", b: \"${a}\" }\n      sites: { source: /tmp/x/, dest: /tmp/y/ }\n      flow: \"source <-> dest [edit]\"\n      log: \"${a}\"\n";
    let err = match load(yaml4) {
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

    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "source <-> dest [edit delete]"));
    eng.cycle(None).unwrap();

    // The whole dest folder disappears, as it does when a volume is unmounted or a root is renamed.
    // A missing root that has records is treated as unavailable, so the deletions do not travel to source.
    fs::remove_dir_all(&b).unwrap();
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "precious");
    // The report marks the relation unavailable, with the reason and no rows. The pending event is emitted once.
    let report = eng.report();
    assert!(report.relations[0].unavailable.as_deref().unwrap_or("").contains("folder is missing"));
    assert!(report.relations[0].rows.is_empty());
    assert_eq!(report.relations[0].state(false), RelationState::Unavailable);
    assert_eq!(count(&events, "pending g"), 1, "{:?}", events.lock().unwrap());

    // Once the folder is back, syncing resumes.
    write(b.join("f.txt"), "precious");
    eng.cycle(None).unwrap();
    write(a.join("f.txt"), "precious v2");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "precious v2");
    assert!(eng.report().relations[0].unavailable.is_none());
}

#[test]
fn markers_bracket_creates_and_deletes() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");

    let eng = engine(&tmp, &pair("g", &a, &b, "source <-> dest [edit]; source -> dest [create delete]"));

    // A create and a delete each put a marker beside the target file and remove it when the plan commits.
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    assert_eq!(read(b.join("f.txt.replica.tmp")), None);

    fs::remove_file(a.join("f.txt")).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    assert_eq!(read(b.join("f.txt.replica.tmp")), None);

    // The walk skips a marker left behind by a crash, so the marker does not become a key.
    write(b.join("g.txt.replica.tmp"), "service: replica\nop: create\n");
    eng.cycle(None).unwrap();
    assert_eq!(read(a.join("g.txt.replica.tmp")), None);
    assert!(eng.report().relations[0].rows.iter().all(|r| !r.key.contains("replica.tmp")));
}

#[test]
fn narrowed_pattern_drops_recorded_files_it_no_longer_covers() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("app.js"), "js");
    write(a.join("notes.md"), "md v1");

    let flow = "source <-> dest [edit]; source -> dest [create]";
    let eng = engine(&tmp, &pair("g", &a, &b, flow));
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("notes.md")).unwrap(), "md v1");

    // The same relation is narrowed to *.js. notes.md still has records, but it no longer matches.
    let eng2 = engine(&tmp, &format!("{}      where: {{ path: \"*.js\" }}\n", pair("g", &a, &b, flow)));
    write(a.join("notes.md"), "md v2");
    write(a.join("app.js"), "js v2");
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("app.js")).unwrap(), "js v2");
    assert_eq!(read(b.join("notes.md")).unwrap(), "md v1");
    assert!(eng2.report().relations[0].rows.iter().all(|r| r.key != "notes.md"));
}

#[test]
fn pattern_depth_still_finds_files_in_named_subfolders() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("docs/guide.md"), "deep");
    write(a.join("docs/sub/deeper.md"), "deeper");
    write(a.join("top.md"), "top");

    // The constraint `docs/*.md` matches only files directly inside docs, so top.md and docs/sub/deeper.md are not copied.
    let eng = engine(&tmp, &format!("{}      where: {{ path: \"docs/*.md\" }}\n", pair("g", &a, &b, "source <-> dest [edit]; source -> dest [create]")));
    eng.cycle(None).unwrap();
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

    let eng = engine(&tmp, &pair("g", &a, &b, "source <-> dest [edit]; source -> dest [create]"));
    eng.cycle(None).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&b.join("secret.txt")), 0o600);
    assert_eq!(mode(&b.join("run.sh")), 0o755);

    // An overwrite keeps whatever mode the destination had, as cp does.
    fs::set_permissions(b.join("run.sh"), fs::Permissions::from_mode(0o640)).unwrap();
    write(a.join("run.sh"), "#!/bin/sh\necho v2");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("run.sh")).unwrap(), "#!/bin/sh\necho v2");
    assert_eq!(mode(&b.join("run.sh")), 0o640);
}

#[test]
fn an_edit_made_while_the_peer_is_unreadable_is_delivered_once_the_peer_returns() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "source <-> dest [edit create]"));
    eng.cycle(None).unwrap();
    let seeded = count(&events, "resolved");

    // dest/f.txt is replaced by a directory, so dest is unreadable. The edit at source waits and keeps its old basis.
    fs::remove_file(b.join("f.txt")).unwrap();
    fs::create_dir_all(b.join("f.txt")).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert!(events.lock().unwrap().iter().any(|l| l.contains("waiting on dest: unreadable")), "{:?}", events.lock().unwrap());
    assert_eq!(count(&events, "resolved"), seeded, "nothing resolves while the peer is unreadable: {:?}", events.lock().unwrap());

    // When the directory is replaced by the old file, the waiting edit is copied to it.
    fs::remove_dir(b.join("f.txt")).unwrap();
    write(b.join("f.txt"), "v1");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    assert_eq!(read(a.join("f.txt")).unwrap(), "v2");
}

#[test]
fn an_edit_at_the_recovered_peer_conflicts_with_the_edit_made_while_it_was_unreadable() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    let eng = engine(&tmp, &pair("g", &a, &b, "source <-> dest [edit]"));
    eng.cycle(None).unwrap();

    fs::remove_file(b.join("f.txt")).unwrap();
    fs::create_dir_all(b.join("f.txt")).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    // dest comes back with its own edit. Both sides changed since their records, so the key is held as a conflict.
    fs::remove_dir(b.join("f.txt")).unwrap();
    write(b.join("f.txt"), "v1-edited");
    eng.cycle(None).unwrap();
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
    let eng = engine(&tmp, &pair("g", &a, &b, "source <-> dest [edit]; source -> dest [delete]"));
    eng.cycle(None).unwrap();

    fs::remove_file(b.join("f.txt")).unwrap();
    fs::create_dir_all(b.join("f.txt")).unwrap();
    fs::remove_file(a.join("f.txt")).unwrap();
    eng.cycle(None).unwrap();
    // The record at source still has the file, so the deletion is still pending when dest is readable again.
    fs::remove_dir(b.join("f.txt")).unwrap();
    write(b.join("f.txt"), "v1");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    assert_eq!(read(a.join("f.txt")), None);
}

#[test]
fn records_outside_a_narrowed_pattern_are_forgotten() {
    let tmp = TempDir::new().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    write(a.join("app.js"), "js");
    write(a.join("notes.md"), "md v1");
    let flow = "source <-> dest [edit delete]; source -> dest [create]";
    let eng = engine(&tmp, &pair("g", &a, &b, flow));
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("notes.md")).unwrap(), "md v1");

    // Once the relation is narrowed to *.js, notes.md is no longer a key and its records are dropped.
    let eng2 = engine(&tmp, &format!("{}      where: {{ path: \"*.js\" }}\n", pair("g", &a, &b, flow)));
    eng2.cycle(None).unwrap();
    assert!(eng2.store.records_for("g").unwrap().get(&Key::parse("path=notes.md").canonical()).is_none());
    // While the relation is narrowed, notes.md is deleted on both sides and recreated at source. The old records are gone, so after the relation is widened again, the missing copy at dest does not count as a deletion.
    fs::remove_file(a.join("notes.md")).unwrap();
    fs::remove_file(b.join("notes.md")).unwrap();
    write(a.join("notes.md"), "md v2");
    eng2.cycle(None).unwrap();
    let eng3 = engine(&tmp, &pair("g", &a, &b, flow));
    eng3.cycle(None).unwrap();
    assert_eq!(read(a.join("notes.md")).unwrap(), "md v2");
    assert_eq!(read(b.join("notes.md")).unwrap(), "md v2");
}

#[test]
fn a_variable_on_one_side_against_a_literal_file_is_rejected_at_load() {
    // The `{path}` variable appears only on source, because dest is a single file. The error suggests a fanout entry or a matching variable.
    let err = load_error("relations:\n    fx:\n      sites: { source: /conf.d/, dest: /Desktop/x.txt }\n      where: { path: \"*.conf\" }\n      flow: \"source <-> dest [edit]; source -> dest [create]\"\n");
    assert!(err.contains("fanout"), "unexpected error: {err}");
    assert!(err.contains("{path}"), "unexpected error: {err}");
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

    let (eng, events) = engine_with_events(&tmp, &pair("g", &a, &b, "source <-> dest [edit]"));
    eng.cycle(None).unwrap();

    // The dest folder is read-only, so the temporary file cannot be created.
    fs::set_permissions(&b, fs::Permissions::from_mode(0o500)).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    fs::set_permissions(&b, fs::Permissions::from_mode(0o755)).unwrap();
    let errors: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("error g f.txt")).cloned().collect();
    assert_eq!(errors.len(), 1, "{:?}", events.lock().unwrap());
    assert!(errors[0].contains("creating temp file") && errors[0].contains("f.txt"), "{}", errors[0]);
    // After the failure, the intent row is cleared, no marker is left, and the records are unchanged. The next cycle then retries the write.
    assert!(eng.store.stale_intents().unwrap().is_empty());
    assert!(fs::read_dir(&b).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("replica")));
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
}
