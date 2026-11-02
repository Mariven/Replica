//! Run the scenarios of the first design against the redesigned engine, end to end, over temporary directories.
//! Each test loads a relation from YAML in the new form, builds an engine over a fresh state database, then drives cycles and checks the files, the records, the report, the events.

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

/// The error text from loading a configuration; panics when the loader accepts it.
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

/// The events recorded from an engine, one `kind relation key: text` line each.
type Events = Arc<Mutex<Vec<String>>>;

/// Build an engine over the relations, recording each emitted event through a hook.
fn engine_with_events(tmp: &TempDir, relations_yaml: &str) -> (Engine, Events) {
    let loaded = load(&format!("relations:\n{relations_yaml}")).unwrap();
    let seen: Events = Arc::default();
    let sink = seen.clone();
    let hook: Hook = Box::new(move |e: &Event| {
        sink.lock().unwrap().push(format!("{} {} {}: {}", e.kind(), e.relation(), e.key(), e.text()));
    });
    (build(tmp, &tmp.path().join("state.db"), loaded, vec![hook]), seen)
}

/// Assemble an engine from loaded relations: a store at `db`, a blob store in the temporary directory, the local filesystem, the temporary directory as the working directory.
fn build(tmp: &TempDir, db: &Path, loaded: Loaded, hooks: Vec<Hook>) -> Engine {
    let blobs = Blobs { dir: tmp.path().join("blobs"), cap: 1 << 20 };
    Engine::new(Store::open(db).unwrap(), blobs, Box::new(Local), loaded.relations, hooks, tmp.path().to_path_buf())
}

/// YAML for a relation with `source` over folder `a` and `dest` over folder `b` under the given flow. Callers append further fields at the same indentation.
fn pair(name: &str, a: &Path, b: &Path, flow: &str) -> String {
    format!("    {name}:\n      sites: {{ source: \"{}/\", dest: \"{}/\" }}\n      flow: \"{flow}\"\n", a.display(), b.display())
}

/// Write text to a path, creating parent directories as needed.
fn write(path: impl AsRef<Path>, content: &str) {
    fs::create_dir_all(path.as_ref().parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// Read a file as text, None when it is absent.
fn read(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Set the modification time of a file to a count of seconds past the Unix epoch, so an ordering between files does not depend on how fast the test runs.
fn set_mtime(path: impl AsRef<Path>, secs: u64) {
    let f = fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

/// Count the recorded event lines starting with `prefix`.
fn count(events: &Events, prefix: &str) -> usize {
    events.lock().unwrap().iter().filter(|l| l.starts_with(prefix)).count()
}

/// The display names of a relation's replicas as the report lists them.
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

    // The first cycle records both folders; edits on either side then travel to the other.
    eng.cycle(None).unwrap();
    write(a.join("app.js"), "console.log(2)");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("app.js")).unwrap(), "console.log(2)");

    write(b.join("util.js"), "util v2");
    eng.cycle(None).unwrap();
    assert_eq!(read(a.join("util.js")).unwrap(), "util v2");

    // The constraint admits JavaScript alone, so the text file stays out of the destination.
    assert_eq!(read(b.join("notes.txt")), None);
}

#[test]
fn case_2_rename_via_two_literal_files() {
    let tmp = TempDir::new().unwrap();
    let (conf_dir, desk) = (tmp.path().join("nginx/conf.d"), tmp.path().join("Desktop"));
    write(conf_dir.join("default.conf"), "server {}");

    // Two patterns without variables relate two files under different names.
    let eng = engine(&tmp, &format!(
        "    nginx-default:\n      sites: {{ source: \"{}/default.conf\", dest: \"{}/nginx-conf.txt\" }}\n      flow: \"source <-> dest [edit]\"\n",
        conf_dir.display(), desk.display()
    ));

    // The first cycle records the source file; an empty destination file appearing afterwards conforms to it.
    eng.cycle(None).unwrap();
    write(desk.join("nginx-conf.txt"), "");
    eng.cycle(None).unwrap();
    assert_eq!(read(desk.join("nginx-conf.txt")).unwrap(), "server {}");

    // An edit of the renamed file travels back into the configuration directory.
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

    // The destination site lists its locations, so each becomes a replica labeled by the pattern text as written.
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

    // An edit at a destination has no edge to leave by, so the one-way edge reverts it to the source content.
    write(projects[1].join("main_style.md"), "local heresy");
    eng.cycle(None).unwrap();
    assert_eq!(read(projects[1].join("main_style.md")).unwrap(), "v2");

    // Replicas sharing a site are named by their location label, here the path as written in the configuration.
    let mut expected = vec!["source".to_string()];
    expected.extend(projects.iter().map(|p| format!("dest ({}/main_style.md)", p.display())));
    assert_eq!(replica_names(&eng, 0), expected);
}

/// YAML for the style-guide relation of case 4: a literal source file mirrored into each project found under `projects_dir`.
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

    // The fan-out variable is bound to each directory found under Projects, one replica per project.
    let eng = engine(&tmp, &style_guide_fan_out(&rules, &projects_dir));

    eng.cycle(None).unwrap();
    for p in ["bouncer", "duckmaster"] {
        assert_eq!(read(projects_dir.join(p).join("docs/style/main_style.md")).unwrap(), "v1");
    }

    // A project directory added later is discovered on the next cycle and receives the style guide through the create edge.
    fs::create_dir_all(projects_dir.join("screamio")).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(projects_dir.join("screamio/docs/style/main_style.md")).unwrap(), "v1");

    // Fan-out replicas are named by the value bound to the variable.
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
    // A project folder with no docs yet is still a replica, since the variable is bound at the Projects level.
    fs::create_dir_all(projects_dir.join("screamio")).unwrap();

    // On the first cycle the source content is copied to each project found under Projects, to the empty one through the create edge.
    let eng = engine(&tmp, &style_guide_fan_out(&rules, &projects_dir));
    eng.cycle(None).unwrap();
    for p in ["bouncer", "duckmaster", "screamio"] {
        assert_eq!(read(projects_dir.join(p).join("docs/style/main_style.md")).unwrap(), "v1");
    }

    // A later edit at the source fans out to each replica; an edit at a project has no edge to leave by and is reverted.
    write(rules.join("main_style.md"), "v2");
    eng.cycle(None).unwrap();
    write(projects_dir.join("bouncer/docs/style/main_style.md"), "local heresy");
    eng.cycle(None).unwrap();
    for p in ["bouncer", "duckmaster", "screamio"] {
        assert_eq!(read(projects_dir.join(p).join("docs/style/main_style.md")).unwrap(), "v2");
    }

    // Fan-out replicas are named by the value bound to the variable, in directory order.
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

    // The variable bound from a source file name is substituted into the nested destination pattern.
    eng.cycle(None).unwrap();
    assert_eq!(read(nested.join("humanizer/SKILL.md")).unwrap(), "no em dashes");
    // The text file does not match `{name}.md`, so it is not a key of the relation.
    assert_eq!(read(nested.join("notes.txt")), None);

    // Edits on either side travel through the rewrite in both directions.
    write(flat.join("prose.md"), "write plainly, v2");
    eng.cycle(None).unwrap();
    assert_eq!(read(nested.join("prose/SKILL.md")).unwrap(), "write plainly, v2");
    write(nested.join("prose/SKILL.md"), "write plainly, v3");
    eng.cycle(None).unwrap();
    assert_eq!(read(flat.join("prose.md")).unwrap(), "write plainly, v3");

    // A variable appearing on some sites only is refused, with the variable itself quoted in the message.
    let err = load_error("relations:\n    bad:\n      sites: { a: \"/x/{name}.md\", b: \"/y/{other}/f.md\" }\n      flow: \"a <-> b\"\n");
    assert!(err.contains("{name}") || err.contains("{other}"), "unexpected error: {err}");

    // A pattern with a variable on one side and a whole folder on the other is refused for the same reason.
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
    // A file present on one side with no edge to carry it is not awaiting anything.
    let report = eng.report();
    assert_eq!(report.relations[0].state(false), RelationState::InSync);
    assert!(!report.relations[0].rows.iter().find(|r| r.key == "only-here.txt").unwrap().needs_attention());

    // Without a create edge a deleted copy stays deleted.
    fs::remove_file(b.join("f.txt")).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")), None);

    // A create edge into dest fills the empty spot from the source copy and brings in only-here.txt; a later deletion is restored the same way.
    let eng2 = engine(&tmp, &pair("g2", &a, &b, "source <-> dest [edit]; source -> dest [create]"));
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");
    assert_eq!(read(b.join("only-here.txt")).unwrap(), "solo");
    fs::remove_file(b.join("f.txt")).unwrap();
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");

    // A delete edge carries the removal of the source copy to dest.
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

    // Differing edits on both sides are held, so each file keeps its own content.
    write(a.join("f.txt"), "from a");
    write(b.join("f.txt"), "from b");
    eng.cycle(None).unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "from a");
    assert_eq!(read(b.join("f.txt")).unwrap(), "from b");

    // Under an mtime rule the newer file wins the contest.
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

    // The paths from a and c meet at d, so d is held while b takes the edit from a.
    write(a.join("f.txt"), "from a");
    write(c.join("f.txt"), "from c");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "from a");
    assert_eq!(read(d.join("f.txt")).unwrap(), "base");

    // The conflict at d stays visible on later cycles although no file changed since the last one.
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    let report = eng.report();
    let row = report.relations[0].rows.iter().find(|r| r.key == "f.txt").unwrap();
    assert!(row.note.as_deref().unwrap_or("").contains("conflict"), "note: {:?}", row.note);
    assert!(row.needs_attention());
    assert_eq!(report.relations[0].state(false), RelationState::Awaiting(1));
    assert_eq!(read(d.join("f.txt")).unwrap(), "base");
    // A conflict unchanged since the last cycle is not reported again.
    assert_eq!(count(&events, "skipped quad f.txt"), 1, "{:?}", events.lock().unwrap());

    // Once a and c agree, d conforms and the conflict clears.
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
    // The resolved event carries the plan text. The first cycle only aligned the records of two identical files.
    let resolved: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("resolved")).cloned().collect();
    assert_eq!(resolved, vec![
        "resolved g f.txt: resolved: records aligned, no files changed",
        "resolved g f.txt: resolved: copy source -> dest",
    ]);

    // The written copy matches its record on later cycles, so nothing rewrites it.
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
        // A journal row without a commit plus a half-written destination stands in for a run killed mid-plan.
        eng.store.write_intent("g", &Key::parse("path=f.txt"), "copy source -> dest").unwrap();
        write(b.join("f.txt"), "torn write");
    }

    // The next instance reports the stale row through its hook, clears it, then reconciles the key from the files as they stand.
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
    // The torn write differs from the recorded basis, so it reads as an edit at dest and travels to source.
    assert_eq!(read(a.join("f.txt")).unwrap(), "torn write");
}

#[test]
fn arbitrary_site_names_beyond_source_and_dest() {
    let tmp = TempDir::new().unwrap();
    let (a, b, c) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"));
    write(a.join("f.txt"), "v1");
    write(b.join("f.txt"), "v1");
    write(c.join("f.txt"), "v1");

    // Site names are free, a leading dollar sign included; the flow refers to them in a braced set.
    let eng = engine(&tmp, &format!(
        "    tri:\n      sites:\n        first: \"{}/\"\n        second: \"{}/\"\n        \"$backup\": \"{}/\"\n      flow: \"first -> {{second, $backup}} [edit]\"\n",
        a.display(), b.display(), c.display()
    ));
    eng.cycle(None).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    assert_eq!(read(c.join("f.txt")).unwrap(), "v2");

    // The loader refuses an undeclared site, a flow carrying nothing, a field of the first design.
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

    // Under the closed global gate the plan is computed and reported while the destination keeps its old content.
    eng.gates.set_global(true);
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    // A held plan unchanged since the last cycle is not reported again.
    assert_eq!(count(&events, "held g f.txt"), 1, "{:?}", events.lock().unwrap());

    // The report shows the held plan on the row for f.txt.
    let report = eng.report();
    let row = report.relations[0].rows.iter().find(|r| r.key == "f.txt").unwrap();
    assert_eq!(row.plan.len(), 1);
    assert_eq!(row.plan_text, "copy source -> dest");
    assert_eq!(report.relations[0].state(true), RelationState::Held(1));

    // Opening the gate lets the next cycle apply the deferred write.
    eng.gates.set_global(false);
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
    // A new edit under a closed gate is reported again, since the previous hold was resolved.
    eng.gates.set_global(true);
    write(a.join("f.txt"), "v2b");
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(count(&events, "held g f.txt"), 2, "{:?}", events.lock().unwrap());
    eng.gates.set_global(false);
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2b");

    // A relation may start with its own gate closed from the configuration.
    let loaded = load(&format!("relations:\n{}      gate: true\n", pair("g", &a, &b, "source <-> dest [edit]"))).unwrap();
    assert!(loaded.relations[0].gate_default);
    let eng2 = build(&tmp, &tmp.path().join("state2.db"), loaded, vec![]);
    eng2.cycle(None).unwrap();
    write(a.join("f.txt"), "v3");
    eng2.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2b");
    // A gated cycle records nothing, so the sides are realigned by hand before the gate opens and the baseline is taken.
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

    // An overwrite and a deletion at dest each leave the displaced content in the blob store with an archive row.
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
    eng.cycle(None).unwrap();  // b/f.txt is overwritten, archiving "new content".
    write(a.join("f.txt"), "final");
    eng.cycle(None).unwrap();  // b/f.txt is overwritten again, so "delete me" is archived; its blob is already there from gone.txt.
    let (rows, bytes) = eng.store.archive_stats().unwrap();
    assert_eq!(rows, 4);
    let unique: std::collections::BTreeSet<String> = eng.store.archive_entries("g").unwrap().into_iter().map(|e| hex(&e.hash)).collect();
    assert_eq!(unique.len(), 3, "'delete me' must be stored once");
    assert_eq!(bytes as usize, "old content".len() + "new content".len() + "delete me".len());

    // A tiny cap evicts the older rows while the row just written survives.
    eng.blobs = Blobs { dir: dir.clone(), cap: 1 };
    write(a.join("f.txt"), "post-cap");
    eng.cycle(None).unwrap();
    let entries = eng.store.archive_entries("g").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(read(dir.join(hex(&entries[0].hash))).unwrap(), "final");
    // An evicted blob leaves the disk with its last row.
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
    // The cycle as a whole succeeds: the overlapping pair is reported and skipped while the third relation syncs.
    eng.cycle(None).unwrap();
    assert_eq!(read(c.join("f.txt")), None);
    assert_eq!(read(d.join("g.txt")), None);
    assert_eq!(read(f.join("h.txt")).unwrap(), "fine");
    let errors: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("error")).cloned().collect();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(errors.iter().all(|l| l.contains("overlapping roots")), "{errors:?}");
    assert!(errors.iter().any(|l| l.starts_with("error g1 ")) && errors.iter().any(|l| l.starts_with("error g2 ")), "{errors:?}");
    // The report shows the same error on both relations; the second cycle does not repeat it.
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

    // The renamed file is written into Desktop, which also contains the mylibrary root; the footprints do not overlap, so both relations run.
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
    // A relation log inside a root is refused, since the daemon's own writes there turn into endless edits.
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

    // The relation takes its flow from the default profile; the log path expands ${state} and ${relation}.
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
    // The create edge inherited from the profile brings the file to dest.
    assert_eq!(read(b.join("f.txt")).unwrap(), "hello");

    // A field written on the relation replaces the same field of the profile.
    let yaml2 = "profiles:\n    _:\n      flow: \"source <-> dest [edit]\"\n      where: { path: \"*.md\" }\nrelations:\n    h:\n      sites: { source: /tmp/x/, dest: /tmp/y/ }\n      where: { path: \"*.js\" }\n";
    let loaded = load(yaml2).unwrap();
    let pattern = &loaded.relations[0].sites[0].locations[0].pattern;
    assert!(pattern.matches(Path::new("/tmp/x/a.js")).is_some());
    assert!(pattern.matches(Path::new("/tmp/x/a.md")).is_none());

    // Patterns with variables are accepted as plain strings.
    let yaml3 = "relations:\n    t:\n      sites: { source: \"/x/{name}.md\", dest: \"/y/{name}/SKILL.md\" }\n      flow: \"source <-> dest [edit]\"\n";
    assert!(load(yaml3).is_ok());

    // Variables referring to each other in a cycle are refused.
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

    // The whole destination folder vanishes, as happens with an unmounted volume or a renamed root.
    // A root with records but no folder is treated as unavailable, so the deletion does not travel to source.
    fs::remove_dir_all(&b).unwrap();
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(a.join("f.txt")).unwrap(), "precious");
    // The report applies the same guard: no rows, no delete plan, an unavailable reason, reported once.
    let report = eng.report();
    assert!(report.relations[0].unavailable.as_deref().unwrap_or("").contains("folder is missing"));
    assert!(report.relations[0].rows.is_empty());
    assert_eq!(report.relations[0].state(false), RelationState::Unavailable);
    assert_eq!(count(&events, "pending g"), 1, "{:?}", events.lock().unwrap());

    // Once the folder is back, cycles resume as before.
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

    // A create and a delete each place a marker beside the target and remove it when the operation commits.
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    assert_eq!(read(b.join("f.txt.replica.tmp")), None);

    fs::remove_file(a.join("f.txt")).unwrap();
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")), None);
    assert_eq!(read(b.join("f.txt.replica.tmp")), None);

    // A stray marker left by a crash is skipped by the walk, so no key is made from it.
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

    // The same relation narrowed to *.js: notes.md has records, but the constraint no longer admits it.
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

    // A constraint with a fixed segment count bounds the walk, so files above or below that depth are left alone.
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

    // dest/f.txt becomes a directory, so dest is unreadable; the edit at source waits with its basis unchanged.
    fs::remove_file(b.join("f.txt")).unwrap();
    fs::create_dir_all(b.join("f.txt")).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    eng.cycle(None).unwrap();
    assert!(events.lock().unwrap().iter().any(|l| l.contains("waiting on dest: unreadable")), "{:?}", events.lock().unwrap());
    assert_eq!(count(&events, "resolved"), seeded, "nothing resolves while the peer is unreadable: {:?}", events.lock().unwrap());

    // The directory goes away and the old file comes back: the pending edit is delivered to it.
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
    // The peer returns with an edit of its own: both sides moved since the baseline, so the key is held as a conflict.
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
    // The record at source still says the file existed, so the deletion is still pending when the peer is readable again.
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

    // Narrowed to *.js, notes.md leaves the relation and its records go with it.
    let eng2 = engine(&tmp, &format!("{}      where: {{ path: \"*.js\" }}\n", pair("g", &a, &b, flow)));
    eng2.cycle(None).unwrap();
    assert!(eng2.store.records_for("g").unwrap().get(&Key::parse("path=notes.md").canonical()).is_none());
    // While narrowed, the person replaces notes.md on both sides; widening again must not read that as a deletion against the old baseline.
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
    // The path variable appears on source alone, since dest is a single file, so the loader asks for a fanout entry or a matching variable.
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

    // A read-only destination folder keeps the temporary file from being created.
    fs::set_permissions(&b, fs::Permissions::from_mode(0o500)).unwrap();
    write(a.join("f.txt"), "v2");
    eng.cycle(None).unwrap();
    fs::set_permissions(&b, fs::Permissions::from_mode(0o755)).unwrap();
    let errors: Vec<String> = events.lock().unwrap().iter().filter(|l| l.starts_with("error g f.txt")).cloned().collect();
    assert_eq!(errors.len(), 1, "{:?}", events.lock().unwrap());
    assert!(errors[0].contains("creating temp file") && errors[0].contains("f.txt"), "{}", errors[0]);
    // The intent row is cleared, no marker is left, the records are as they were, so the next cycle retries.
    assert!(eng.store.stale_intents().unwrap().is_empty());
    assert!(fs::read_dir(&b).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().contains("replica")));
    assert_eq!(read(b.join("f.txt")).unwrap(), "v1");
    eng.cycle(None).unwrap();
    assert_eq!(read(b.join("f.txt")).unwrap(), "v2");
}
