# Replica

Replica keeps files in sync between folders on one machine. A YAML file declares relations. Each relation names its sites, says which files at each site correspond, and gives a flow: the directions in which edits, new files, and deletions travel, and how to settle a disagreement.

The daemon runs a cycle once per interval, or sooner when the filesystem reports a change. In a cycle it lists the files at each site, compares each one against what it recorded after the last sync, decides what each site should hold, and then writes, deletes, and archives to make it so. A closed gate lets the daemon plan and report without writing anything.


## Patterns

A site is a pattern: an absolute path with variables in braces.

    ~/Desktop/mylibrary/{path...}
    ~/scripts/skills/{name}.md
    ~/.claude/skills/{name}/SKILL.md
    ~/Desktop/Projects/{project}/docs/style/main_style.md
    /etc/nginx/conf.d/default.conf

`{name}` matches one path segment and `{name...}` matches one or more. A pattern ending in `/` is short for `/{path...}`, so `~/Desktop/mylibrary/` means the whole tree. A pattern without variables is a single file. Because a pattern holds at most one `{name...}` variable and at most one variable per segment, a path matches in only one way, and substituting the values back gives the same path.

A constraint after a colon narrows what a variable may match: a glob like `{file: *.js}` or `{path...: docs/**/*.md}`, a regular expression like `{date: ~\d{4}-\d\d-\d\d~}`, or either one negated with a leading `!`. Constraints can also go in the relation's `where` block, keyed by variable, which keeps long patterns readable.

The literal prefix before the first variable is the pattern's root. The daemon walks the tree below the root looking for files that match. No two replicas may overlap on disk, and none may contain the state folder or a log file.


## Sites, keys, and replicas

A relation has at least two sites. A variable that appears on all sites is an identity variable, and the values of the identity variables are a file's key. Files at different sites with the same key are copies of each other, so `~/scripts/skills/{name}.md` and `~/.claude/skills/{name}/SKILL.md` pair `prose.md` with `prose/SKILL.md`. A relation with no variables has a single key, which is how two files with different names get related.

A variable listed under `fanout` may appear on some sites only. At those sites the daemon makes one replica per matching directory, so `${projects}/{project}/docs/style.md` with `fanout: [project]` gives a replica for each project folder. A site can also list several patterns, or a map from labels to patterns, and each becomes a replica. A variable that is on some sites but not others, and not declared under `fanout`, is rejected, since it is most often a typo.

Each replica has an id: the site name, followed by the label or the fan-out values in brackets, as in `dest[project=alpha]`. The database keeps records under this id and remembers the pattern behind it. If the pattern behind an id changes, the daemon reports the move, drops the old records, and lets the replica start fresh.

To find the keys of a relation, the daemon walks each replica from its root and matches the files it finds, leaving out any that match an `ignore` glob. It also includes keys it has records for but found nowhere, so that a deletion is noticed. A key is dropped along with its records when some replica can no longer place it within its pattern, as happens after a constraint is narrowed.


## Bases and transitions

For each key at each replica, the database holds a basis: the snapshot (mtime, size, BLAKE3 hash) recorded after the last sync, a tombstone if the file was deleted there, or nothing if the daemon has yet to see the file there. A tombstone is written only where a recorded file went missing or the daemon deleted it, so a replica where the file was absent all along has no record at all.

On each cycle the daemon observes the file, reusing the recorded hash when the mtime and size are unchanged, and classifies the transition:

- unchanged
- appeared: no record, and the file is present
- recreated: a tombstone, and the file is present
- edited: a snapshot, and the file is present with a different hash
- disappeared: a snapshot, and the file is missing
- unreadable: something is at the path but it cannot be read as a regular file

A key whose replicas are all unchanged is skipped.


## Flow and resolver

The flow is a graph over the sites. Each edge carries some of the kinds `edit`, `create`, and `delete`:

    source <-> dest [edit create]
    source -> dest [edit create delete]
    {a, b} -> c [edit]

An arrow without brackets carries `edit` and `create`, `all` carries all three, and semicolons separate statements. Edges of the same kind chain together, so under `a -> b; b -> c` an edit at `a` reaches `c`. Replicas of the same site exchange changes only when the flow leads from the site back to itself, as `a <-> b` does.

The resolver settles disagreements. `a > b` prefers content from `a`. `a mtime b` prefers the newer file, `size` the larger one, and `name` the alphabetically earlier name. The rules are tried in order, and a bare property such as `mtime` applies to any pair of sites. When no rule decides, the key is held and shown on the dashboard until someone resolves it.


## Deciding a key

Each key is decided from the transitions of its replicas and the flow. The rules carry the names used in `src/decide.rs`:

1. Voice. An edited or recreated replica proposes its content, and a disappeared one proposes absence. A replica where a file appeared proposes its content only if no replica has a record of the key yet. Otherwise it is a newcomer and conforms to its peers.

2. Admit. A proposal reaches a replica along edges of the fitting kind: content along `edit` edges to a replica that has the file and along `create` edges to one that lacks it, absence along `delete` edges. An unreadable replica admits both `edit` and `create`.

3. Hold. An unreadable replica is held, and so is any replica whose proposal reaches it, since the unreadable file might hold a competing change.

4. Contest. A replica's candidates are the proposals that reach it. Its own proposal joins them if it can travel back to one of the proposers. This is why a two-way edge produces a conflict when both ends change, while on a one-way edge the upstream change simply overwrites the downstream one.

5. Stand. A replica with no candidates, whose own proposal reaches some peer, keeps its state. The peers deal with its proposal.

6. Conform. A replica with no candidates and no outgoing proposal takes the content of a settled peer, one that is unchanged against its snapshot, if the flow carries that content to it. This restores a deleted file over a `create` edge, fills a newcomer, and reverts a downstream edit that has no edge to leave by.

7. Resolve. If the candidates agree, the replica takes their content. Otherwise the resolver rules pick a winner, or the replica is held with a conflict.

8. Operation. A replica whose decided content differs from what it has gets a write. One that should be absent gets a delete.

After the plan is applied, the executor records a new basis for each unheld replica: the snapshot it wrote, a tombstone where it deleted a file or found a recorded file missing, and otherwise what it observed. Held replicas keep their basis and are marked unresolved, which makes the daemon look at the key again on each cycle so the conflict stays visible. A key missing everywhere has its records cleared.


## Applying a plan

The executor applies a plan in stages, and a failure in one stage stops the rest.

1. Stage. The content for each write is copied into the blob store under its hash, read from a replica that was observed with that hash. If the file has changed in the meantime, the plan aborts. Transforms run here, on the staged blob. Because writes read from the blob store, not from another replica, the order of writes within a plan does not matter.

2. Archive. Each file about to be overwritten or deleted is copied into the blob store, and an archive row records where it came from and why. Nothing is destroyed without a copy.

3. Journal. An intent row records the plan, and a marker named `<file>.replica.tmp` is placed beside each file about to be created or deleted.

4. Write. Each write goes to a temporary file in the target folder and is renamed into place, carrying the source's mtime when `sync_times` is on. Deletes remove the file.

5. Commit. One transaction replaces the records, marks held replicas unresolved, and removes the intent row. The markers are removed with it.

If a stage fails, the markers and the intent row are removed, the records are left as they were, and the next cycle plans the key again. An intent row found at startup means a previous process died mid-plan. The daemon reports it and reconciles the key from what is on disk.

The archive is capped. When it grows past `archive.cap`, the oldest revisions of the relation using the most space are evicted.


## State

The state folder holds `state.db`, the `blobs/` folder, a lock file, and by default the logs. The database tables:

- `setting`: the schema version and the persisted gate flags
- `relation`: name and policy fingerprint
- `replica`: id, pattern, last seen
- `record`: the basis for each relation, key, and replica
- `unresolved`: held replicas and their reasons
- `intent`: plans in progress
- `archive`: archived revisions, pointing into the blob store

The schema is version 2, and the daemon refuses a database of any other version. Start a fresh state folder in that case. The first cycle then behaves as a first run, aligning identical files and resolving or holding the ones that differ.

The fingerprint covers whatever in a relation affects decisions: patterns, constraints, flow, resolver, ignore list, transforms. When it changes, the next cycle looks at each key instead of skipping the settled ones.

Gates persist. Opening or closing a gate from the dashboard or the command line writes a flag, and a gate starts closed if either the flag or the configuration says so. `gate: true` in the configuration, or `--gate`, writes the flag as well, so the daemon stays gated across restarts until someone opens it.


## Running

The daemon runs a full cycle at startup and once per `interval` after that. With `watch` on, it also registers each root with the operating system through the `notify` crate, marks a relation dirty when a change is reported under one of its roots, and after `debounce` of quiet runs a cycle over the dirty relations alone. The scheduled cycle remains the source of truth, so a missed notification costs at most one interval. The daemon's own writes come back as notifications too and are found unchanged on the next cycle.

A condition that persists, such as a conflict, is reported when it appears and again when it changes, and stays quiet in between.

Pause, resume, sync, restart, and halt arrive from the dashboard or the command line. Restart checks that the configuration still loads, then exits with status 1 so launchd relaunches the daemon. Halt and SIGTERM exit with status 0. In both cases the daemon finishes the key it is applying first. A gate change takes effect at the next relation of a running cycle.


## Configuration

    state: ~/scripts/replica/live
    interval: 1m  # default 30s
    watch: true
    debounce: 2s
    gate: false
    ignore: ["**/.DS_Store"]
    archive: { cap: 100MB }
    dashboard: { port: 18516, recent_events: 200, refresh: 10 }
    include: ["relations/*.yaml"]
    vars: { projects: ~/Desktop/Projects }
    profiles: { ... }
    relations: { ... }

Paths must be absolute or start with `~`, since launchd runs the daemon from `/`. Without `dashboard` there is no dashboard, and the control commands have nothing to talk to.

Variables are written `${name}` and expand in each string of a relation after its profiles are merged. `${relation}` is the relation's name and `${state}` the state folder. Path variables in plain braces belong to patterns and are left alone. An undefined variable is an error.

A profile is a bundle of relation fields. A relation names its profiles under `profile`, and the loader deep-merges the profile `_`, then each named profile, then the relation itself, with later layers winning. A profile may name profiles of its own.

`include` pulls in files or globs relative to the configuration file. An included file may define `vars`, `profiles`, `relations`, and `ignore`. A name defined twice is an error.

A relation looks like this:

    sites:
        source: ~/Desktop/mylibrary/
        dest: ~/scripts/desktop-library/
    where: { path: "*.js" }
    fanout: [project]
    flow: "source <-> dest [edit create]"
    resolve: "source mtime dest"
    ignore: ["*.tmp"]
    sync_times: true
    gate: false
    watch: true
    log: ${state}/logs/${relation}.log
    each: { ... }
    on: { ... }
    transform: { ... }

`sites` takes a pattern, a list of patterns, or a map from labels to patterns. `each` maps variable names to lists of values and stamps out a copy of the relation for each combination, named `relation-value`, for relations that share a form but need their own gates, logs, and history.

The loader checks the patterns and constraints, that variables are consistent across sites, that the flow and resolver parse and name real sites, and that no roots overlap. It reports the first problem with the relation and field it came from. `replica check` runs the same checks without starting the daemon.


## Actions and transforms

`on` maps event kinds to shell commands:

    on:
        changed: "make -C ~/scripts/site"
        resolved: "scp ${path} host:backup/${relation}/"
        conflict: "osascript -e 'display notification \"${key}\" with title \"replica conflict\"'"

`changed` runs once per cycle in which the relation applied something. `resolved` runs once per operation, with `${key}`, `${path}`, `${replica}`, `${site}`, and `${op}` filled in. `held`, `conflict`, and `error` run when those conditions appear or change. Commands run through `sh -c` from the state folder, with the same variables in the environment as `REPLICA_KEY` and so on, and are killed after a minute unless given a `timeout`. A failed command is logged and has no effect on the sync.

`transform` maps an edge to a command that rewrites content on the way:

    transform:
        "src -> out": "sassc ${in} ${out}"

The executor runs the command on the staged blob, stores the output as a new blob, and writes that to the receiving site, whose basis then records the output hash. The receiving site may have no outgoing edges, since its content cannot be mapped back.


## Interfaces

    replica run --config PATH [--once] [--gate]
    replica check --config PATH
    replica explain --config PATH [RELATION]
    replica plan --config PATH [RELATION]
    replica status (--port N | --config PATH) [--json]
    replica sync | pause | resume | restart | halt (--port N | --config PATH)
    replica gate open|closed [--relation NAME] (--port N | --config PATH)
    replica history RELATION (--port N | --config PATH) [--json]
    replica forget --config PATH RELATION REPLICA

`check` loads the configuration and reports problems. `explain` shows how each relation resolved: profiles, variables, patterns with their roots, replicas, flow closure, resolver. `plan` shows what the next cycle would do without doing it. The control commands talk to the running daemon over the dashboard port, taken from `--port` or from the configuration. `forget` drops the records of a replica, by id, for when a location was replaced instead of moved. Halt the daemon first.

The dashboard listens on loopback and refuses requests from any other host or origin. `GET /` shows each relation's replicas, the state of each key, the pending plan or hold reason, and the recent events. `GET /status.json` gives the process state, the gate, counters, the watch state, and a line per relation with its state: `error`, `unavailable`, `awaiting`, `held`, `degraded`, `folder missing`, or `in sync`. `GET /history?rel=NAME` lists archived revisions with download links. The POST routes `/sync`, `/pause`, `/resume`, `/restart`, `/halt`, and `/gate?to=open|closed[&rel=NAME]` are the controls. While a cycle is running, the page shows the report from the previous cycle and says so.

Each event goes to standard error and to the relation's `log` if it has one, and the dashboard keeps the last `recent_events` of them in memory.


## Source layout

- `src/pattern.rs`: patterns, constraints, matching and substitution
- `src/site.rs`: sites, keys, replicas, the key walk
- `src/observe.rs`: snapshots, bases, transitions
- `src/flow.rs`: flow statements and resolver rules
- `src/decide.rs`: the decision rules
- `src/apply.rs`: the executor
- `src/medium.rs`: file operations behind a trait, with the local filesystem implementation
- `src/blobs.rs`, `src/store.rs`: the blob store and the SQLite database
- `src/engine.rs`: cycles, discovery, reports, gates
- `src/event.rs`, `src/action.rs`, `src/watch.rs`: events, shell commands, filesystem notifications
- `src/config/`: the YAML form, variables, profiles, includes, `each`, validation
- `src/dashboard/`: the HTTP server, pages, and `status.json`
- `src/cli.rs`, `src/main.rs`: the subcommands and the daemon loop
- `tests/`: end-to-end suites over temporary folders
