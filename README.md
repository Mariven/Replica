# Replica

Replica is a local file sync service. It keeps related files in agreed states across locations on the same machine, under rules you declare, and shows you what it is doing.

You declare relations in a YAML file. Each relation says which locations hold copies of the same files, in which directions edits, new files, and deletions may travel between them, and how to settle a disagreement. The daemon scans those locations on a schedule, or sooner when the filesystem reports a change, compares what it finds with the state it recorded after the last agreement, decides what each location should hold, then writes, deletes, archives, records, and reports. The design is described in full in [docs/design.md](docs/design.md).


## Concepts

A **relation** is a named declaration relating several **sites**, the roles in the relation, such as `source` and `dest`.

Each site is a **pattern**: an absolute path with variables in braces. `~/Desktop/mylibrary/` is the whole tree below that folder (a trailing slash stands for `{path...}`, one or more path segments). `~/scripts/skills/{name}.md` matches one file per name. `/etc/nginx/conf.d/default.conf` is a single file. A variable may carry a constraint: `{file: *.js}` or `{path...: docs/**/*.md}`, or the constraint goes in the relation's `where` block.

A variable that appears on each site of the relation is an **identity variable**; its binding is the **key** of a file, which is how the daemon pairs files across sites. `~/scripts/skills/{name}.md` and `~/.claude/skills/{name}/SKILL.md` pair `prose.md` with `prose/SKILL.md` through `{name}`. A relation whose sites have no variables has exactly one key, so two literal file paths relate two files whatever their names.

A variable listed under `fanout` appears on some sites only and fans that site out into one **replica** per directory found at its position, discovered on each cycle. `~/Projects/{project}/docs/style/main_style.md` with `fanout: [project]` gives one replica per project folder. A site may also list several locations, or map names to them; each becomes a replica with a stable identity such as `dest[alpha]`.

For each key at each replica the daemon keeps a **basis**: the file as it stood after the last agreement, or a tombstone where the file was deleted. Comparing the basis with what is on disk gives a **transition**: unchanged, edited, appeared, recreated, disappeared, or unreadable.

The **flow** of a relation is a graph over its sites whose edges carry kinds of change: `edit`, `create`, `delete`. `source <-> dest [edit create]` lets edits and new files travel both ways and restores a file deleted on one side from the other; adding `source -> dest [delete]` lets deletions at source delete at dest. An arrow without brackets carries `edit` and `create`. A **resolver** such as `source mtime dest` or `source > dest` settles keys whose replicas disagree; with no rule, a disagreement waits and is shown on the dashboard.


## A cycle

For each relation the daemon expands the sites into replicas and refuses the relation if any replica's root lies inside another's or contains one of the daemon's own files. It walks each replica for files, adds the keys it has records for, and reads one observation per replica for each key, reusing the recorded hash when size and modification time match. A key whose replicas all match their bases is skipped.

Otherwise the daemon decides the key. A changed replica proposes its content, or its absence. Proposals travel along the flow to the replicas the edges admit, with the kind chosen by the target's state: content to a replica with the file along `edit` edges, content to a replica lacking it along `create` edges, absence along `delete` edges. A replica with no incoming proposal and no outgoing proposal of its own takes the content of its settled peers, which is how a downstream edit under a one-way edge reverts and how a deleted file is restored. When several proposals meet at one replica with differing content, the resolver decides or the replica waits.

The decision is applied in stages: the needed content is staged in a content-addressed blob store, anything about to be overwritten or deleted is archived there, an intent row and marker files record what is about to happen, files are written through a temporary sibling and renamed into place, and the records commit in one transaction. A failure at any stage leaves the records untouched so the next cycle plans again. An intent row found at startup means a previous run died mid-plan; it is reported and the key reconciled fresh.

A relation's **gate**, or the global one, holds all writes while planning and reporting continue. The held plan is visible on the dashboard and in `replica plan`.


## Configuration

The example in [config.example.yaml](config.example.yaml) shows each feature with comments. In brief:

```yaml
state: ~/scripts/replica/live
interval: 1m
watch: true
ignore: ["**/.DS_Store"]
dashboard: { port: 18516 }

vars: { projects: ~/Desktop/Projects }

profiles:
    _: { log: "${state}/logs/${relation}.log" }
    two-way: { flow: "source <-> dest [edit create]" }

relations:
    mylibrary:
        profile: two-way
        sites: { source: ~/Desktop/mylibrary/, dest: ~/scripts/desktop-library/ }
        where: { path: "*.js" }
        resolve: "source mtime dest"

    style-guide-all:
        sites:
            source: ~/scripts/rules/main_style.md
            dest: "${projects}/{project}/docs/style/main_style.md"
        fanout: [project]
        flow: "source -> dest [edit create delete]"
```

Configuration variables are written `${name}` and expand in paths, patterns, flow statements, commands, and log paths; `${relation}` is the relation's name and `${state}` the state directory. Path variables in braces belong to patterns and are left alone, so the two never collide. Profiles are bundles of relation fields, deep-merged in order: the profile `_`, then each listed profile, then the relation's own fields. `include` pulls in further files or globs; `each` stamps a relation out once per listed value. Unknown fields are errors at every level, with the error pointing at the profile or relation holding them.

A relation may run shell commands on its events under `on` (`changed`, `resolved`, `held`, `conflict`, `error`), with `${key}`, `${path}`, `${replica}`, `${site}`, and `${op}` filled in at run time, and may rewrite content on an edge under `transform`, where the command sees `${in}` and `${out}`.


## Commands

```sh
cargo build
./target/debug/replica check --config config.yaml  # load, expand replicas, report overlaps
./target/debug/replica explain --config config.yaml  # how each relation resolves: profiles, variables, patterns, replicas, flow
./target/debug/replica plan --config config.yaml  # what the next cycle would do, without writing
./target/debug/replica run --config config.yaml  # run cycles at the configured interval
./target/debug/replica run --config config.yaml --once  # one cycle, then exit (status 1 on errors)
cargo test  # unit tests and the end-to-end suites
```

With a daemon running, `status`, `sync`, `pause`, `resume`, `restart`, `halt`, `gate open|closed [--relation NAME]`, and `history RELATION` talk to it over the dashboard port, given as `--port N` or read from `--config PATH`. `forget --config PATH RELATION REPLICA` drops the records of one replica, for a location that was replaced rather than moved. Only one daemon may run against a state directory; a lock file inside it refuses a second.

The dashboard at `http://localhost:<port>` lists each relation with its replicas, the files and their state at each replica, the plan the next sync applies or the reason a file waits, and the recent activity. Its buttons post to `/sync`, `/pause`, `/resume`, `/gate?to=open|closed[&rel=NAME]`, `/restart`, and `/halt` from the loopback interface alone. `GET /status.json` serves the process state, counters, watch state, and one entry per relation; `GET /history?rel=NAME` lists archived versions with downloads. For launchd setup see [docs/service-mac.md](docs/service-mac.md).


## Safety

Before a plan touches disk, the daemon stages the content it needs and archives each file it will overwrite or delete, so nothing is destroyed unarchived; the archive is capped and evicts the oldest versions of the largest relation. Writes go through a temporary sibling and an atomic rename. Marker files named `<file>.replica.tmp` sit beside files being created or deleted for the duration of the plan; one left behind means a run died partway and says which relation, key, and operation. SIGTERM, SIGINT, halt, and restart stop the daemon between keys, never inside a write.

A folder that has records but is missing, as with an unmounted volume, makes its relation unavailable rather than a mass deletion. A file that appears where the daemon never recorded one conforms to its peers, so copying files by hand to bootstrap a location is safe; a file recreated where the daemon recorded a deletion is treated as an edit. Roots that overlap are refused, and a root containing the state directory or a log file is refused at startup.


## Layout

- `src/pattern.rs`: patterns, variables, constraints, matching and substitution
- `src/site.rs`: sites, keys, replicas, discovery of files under a replica
- `src/observe.rs`: snapshots, observations, bases, transitions
- `src/flow.rs`: flow statements, kinds, closure, resolver rules
- `src/decide.rs`: the rules deciding what each replica of a key should hold
- `src/apply.rs`: the executor: staging, archiving, journaling, writing, committing, transforms
- `src/engine.rs`: cycles, discovery, reports, gates
- `src/store.rs`, `src/blobs.rs`: the SQLite state database and the content-addressed blob store
- `src/medium.rs`: file operations behind one trait, with the local filesystem implementation
- `src/event.rs`, `src/action.rs`, `src/watch.rs`: events and hooks, shell actions and transforms, filesystem notification
- `src/config/`: the YAML form, the merge algebra (variables, profiles, includes, each), the loader
- `src/dashboard/`: the HTTP layer, status JSON, page rendering, the server, with the stylesheet and favicon
- `src/cli.rs`, `src/main.rs`: subcommands and the daemon loop
- `tests/`: end-to-end suites over temporary directories
- `docs/`: [design.md](docs/design.md), [service-mac.md](docs/service-mac.md), [todo.md](docs/todo.md), [design-style.md](docs/design-style.md)


## Coming from the first design

The configuration form changed entirely: `entries` became `relations`, `labels` became `sites`, `pattern` became `where`, the three `propagate` graphs became one `flow` with kinds in brackets, `scan` and `rename` disappeared into patterns, and placeholders became `${name}`. The state database changed schema, and the daemon refuses an old one; start a fresh state directory. The first cycle behaves as a first run, aligning identical content and holding differing content for a decision, so run it under a closed gate (`gate: true` or `--gate`) and read the dashboard before opening the gate. [config.redesign.yaml](config.redesign.yaml) is the live configuration translated to the new form, against a fresh state directory and a second dashboard port, so both daemons can run side by side while you compare.
