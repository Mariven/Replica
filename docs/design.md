# Replica, redesigned

Replica keeps related files in agreed states across locations on the local machine. This document describes the second design of the daemon from the ground up: the primitives it is built on, how a cycle moves data through them, how a person configures, observes, controls it. The present code under `src/` at the start of the `redesign` branch is the first design; where this document refers to it, the reference is historical.


## Purpose

A person declares relations. Each relation says which locations hold copies of the same files, in which directions edits, new files, and deletions may travel between those locations, and how to settle a disagreement. On a schedule, or sooner when the filesystem reports a change, the daemon reads the locations and compares what it finds against the state it recorded after the previous agreement. From that comparison it decides what each location should hold now, then writes, deletes, archives, records, and reports. Correctness comes from the scan itself. A notification merely brings the next scan sooner.

The redesign keeps that purpose. It changes the primitives so that each idea appears once in the code. A person assembles a configuration from shared parts instead of repeating the same fields in each relation. A watcher, a command line, or a dashboard can drive the daemon from outside. The daemon in turn can run a compiler or copy files to a host through the event stream it already emits.


## Vocabulary

A **relation** is a named declaration relating several sites.

A **site** is a named role in a relation, such as `source`, `dest`, `backup`. The flow statements of the relation refer to sites by these names.

A **pattern** is the location of a site: an absolute path with variables in braces. A site may list several patterns.

A **variable** inside a pattern stands for one path segment, or, written `{name...}`, for one or more segments. A variable appearing in the pattern of each site of the relation is an **identity variable**. The relation lists its **fan-out variables** under `fanout`. Such a variable may appear on some sites only. When a variable appears on some sites only without being listed there, the loader treats it as a mistake, most often a typo, and rejects the relation.

A **key** is a binding of the identity variables. Two files at two sites belong together exactly when they bind the identity variables to the same values. A relation with no identity variables has exactly one key, the empty binding.

A **replica** is a concrete location: a site, one of its patterns, a binding of that pattern's fan-out variables. A site whose patterns have no fan-out variables has one replica per pattern. The daemon discovers the replicas of a fan-out variable by reading the filesystem at the depth of the variable.

A **basis** is the state the daemon recorded for a key at a replica after the last successful cycle: a snapshot (modification time, size, content hash), or a tombstone meaning the daemon saw the file deleted there, or nothing at all when the daemon has never recorded the key at that replica.

An **observation** is what the daemon finds on disk for a key at a replica during a cycle: a snapshot, or missing, or unreadable with a reason.

A **transition** is the comparison of a basis against an observation: unchanged, edited, appeared, recreated over a tombstone, disappeared, unreadable.

A **flow** is the directed graph over the sites of a relation. Each edge carries the kinds of transition it transports: `edit`, `create`, `delete`, in any subset.

A **resolver** is an ordered list of rules for settling a key whose replicas receive differing proposals.

An **assignment** is the daemon's decision for one key: for each replica, keep what is there, take a particular content hash, or become absent, together with the replicas held back and the reasons.

A **plan** is the list of filesystem operations an assignment implies against the current observations, phrased by content hash rather than by donor, so the executor may take the bytes from any replica holding them or from the blob store.

The **blob store** is a content-addressed directory beside the state database. It holds archived revisions of overwritten and deleted files, along with donor content staged during a plan and the output of transforms.

A **medium** is the trait through which the engine lists, reads, writes, removes files. The local filesystem is the only implementation in this design; the trait exists so that a remote one can be added later without changes to the engine.

An **action** is a shell command the daemon runs when an event of a chosen kind occurs for a relation.

A **transform** is a shell command through which content passes while traveling along one edge, so that the receiving site holds the command's output rather than the sender's bytes.


## Patterns

A pattern is written as an absolute path, or a path starting with `~`, in which some segments contain variables.

    ~/Desktop/mylibrary/{path...}
    ~/scripts/skills/{name}.md
    ~/.claude/skills/{name}/SKILL.md
    ~/Desktop/Projects/{project}/docs/style/main_style.md
    /etc/nginx/conf.d/default.conf

A variable `{name}` matches exactly one segment, so `~/scripts/skills/{name}.md` matches `prose.md` but not `a/b.md`. A variable `{name...}` matches one or more segments and stands for a relative path. A pattern may contain at most one such variable, placed after any other variables in the pattern. A segment may mix literal text with a variable, as in `{name}.md`, though it may hold at most one variable. The same variable may not appear twice in one pattern.

The loader reads a pattern ending in `/` as the same pattern followed by `{path...}`, so `~/Desktop/mylibrary/` relates whole trees. A pattern with no variables locates exactly one file. Since a relation with no identity variables has exactly one key, the renamed-file case needs no special mapping: two literal file paths at two sites relate the two files, whatever their names.

A variable may carry a constraint after a colon: `{file: *.js}` matches one segment ending in `.js`; `{path...: docs/**/*.md}` matches relative paths under `docs` ending in `.md`. Constraints are globs over the bound text, where `*` and `?` stay within one segment and `**` crosses segments. A constraint written `~re~` is a regular expression anchored at both ends, for cases such as `{date: ~\d{4}-\d\d-\d\d~}`. Constraints may also be written in the relation's `where` block, keyed by variable, which keeps long patterns readable; a variable constrained in both places must satisfy both.

To match a path against a pattern, the daemon binds each variable to the text in its position and checks the constraints; the result is a binding. To substitute, it fills a full binding into the pattern and gets a path back. Under the restrictions above, when the daemon substitutes a binding into a pattern and then matches the resulting path, it gets the same binding back. The two operations undo each other on any path that fits the pattern. The loader enforces the restrictions, so this holds for each pattern in a running configuration.

From a pattern the daemon derives:

- The **root**: the longest literal directory prefix before the first variable. The walk for keys starts there.
- The **depth**: how many segments below the root a matching file lies. A pattern without a `{path...}` variable has a fixed depth. With that variable the depth is unbounded in general. When its constraint has no `**`, the depth becomes fixed again: the constraint's segment count added to the fixed part.
- The **footprint**: the root, or the single file when the pattern has no variables. Two footprints overlap when one lies within the other. When a footprint overlaps another relation's footprint, or one of the daemon's own files such as the database, the blob store, or a log, the daemon refuses the relation. Since a fan-out can produce a new overlap at any time, the dashboard shows the overlap on each cycle until it is gone.
- The **tail**: the part of a matched path below the root. Ignore globs test the tail.

A site may list several patterns, or a map from replica names to patterns. A list item becomes a replica named by its pattern text; a map item becomes a replica named by its key. Either way the replica's identity is independent of how many siblings it has.


## Keys and replicas

At the start of a cycle, the daemon expands each site of a relation into replicas. A pattern without fan-out variables is one replica. For a pattern with fan-out variables, the daemon walks down from the root; at the segment holding a fan-out variable it lists the directory and binds the variable to each entry that satisfies the constraints, then continues below each of them. Each complete binding becomes one replica. A fan-out variable name may appear on several sites. Its bindings stay local to the replicas of each site; the daemon pairs files across sites through the identity variables alone.

A replica's identity is its site name followed by its fan-out bindings in brackets, such as `dest`, `dest[project=alpha]`, `dest[~/scripts/actors/.cursor/rules]`. The identity is the key under which the store records bases, so adding or removing a sibling replica leaves the others' records alone. The store also remembers the pattern behind each identity. When the pattern changes between runs, the replica is a different location under the same name: the daemon reports the change, drops the old records (or holds them while the gate is closed), and lets the replica start as a newcomer, so identical content aligns and differing content is held for a decision.

To discover the keys of a relation, the daemon walks each replica from its root down to its depth and matches each regular file against the pattern. A file becomes a key when the constraints admit it and the ignore list does not exclude its tail. The daemon adds the keys it has records for, so a file deleted everywhere since the last cycle still comes up, is seen to be gone, and has its records cleared. When the current constraints no longer admit a recorded key, the daemon drops the key and its records from the relation. If the constraints are widened later, the file then starts from its present state instead of a stale basis.

Walks skip symbolic links, the daemon's own temporary and marker files, and anything the ignore list excludes. Matching is case-sensitive. On a volume that ignores case, two keys differing only in case would land on one file; detecting that collision from the directory listing is listed as open work in todo.md.


## Observation and transitions

For each key the daemon reads one observation per replica. When the recorded modification time (to the millisecond) and size equal what the filesystem reports, the recorded hash is reused; otherwise the file is hashed. A path that is a directory, a link, or unreadable yields an unreadable observation with the reason.

The transition at a replica follows from basis and observation:

- no record and missing: unchanged
- no record and present: appeared
- tombstone and missing: unchanged
- tombstone and present: recreated
- snapshot and missing: disappeared
- snapshot and present with the same hash: unchanged
- snapshot and present with a different hash: edited
- anything and unreadable: unreadable

The daemon writes a tombstone only where a recorded snapshot went missing or where it deleted the file itself. A replica that lacked the file from the start keeps no record at all. So a tombstone marks a deletion at that replica.

The first design had no rule for a file deleted and later recreated. Here the appeared and recreated transitions supply one. A file appearing where the daemon never recorded one conforms to its peers when any peer has a basis, since it is most likely a copy made by hand to bootstrap the location. A file appearing over a tombstone is treated as an edit, since the person deleted it there on purpose and then wrote a new one.


## Flow and resolution

The flow of a relation is written as statements separated by semicolons. Each statement relates a site or a braced set of sites to another through an arrow, with the kinds carried written in brackets:

    source <-> dest [edit create]
    source -> dest [edit create delete]
    {a, b} -> c [edit]
    backup <- source [all]

An arrow without brackets carries `edit` and `create`. The keyword `all` stands for all three kinds. Edges compose transitively within a kind: under `a -> b [edit]; b -> c [edit]`, an edit made at `a` travels through `b` to `c`. A relation whose flow carries nothing is rejected at load, as is a statement mentioning an undeclared site.

The daemon decides each key one replica at a time, applying the rules below. The code and the tests refer to the rules by these names.

**Proposal.** An edited or recreated replica proposes its content. A replica whose file disappeared proposes a deletion. A replica where a file appeared proposes its content only if no replica of that key has a basis yet; if some peer already has one, the newcomer conforms to its peers instead. Unchanged and unreadable replicas propose nothing.

**Carriage.** A proposal travels along the edges of the kind that fits the target. New content travels along `edit` edges to a replica that has the file and along `create` edges to a replica that lacks it. A deletion travels along `delete` edges to a replica that has the file; nothing travels to a replica that lacks the file, since there is nothing to delete. When the target is unreadable, its state is unknown, so content may travel along either `edit` or `create` edges and a deletion along `delete` edges. A proposal may pass through intermediate sites, because the daemon uses the transitive closure of each kind's edges.

**Candidates.** The candidates for a target are the proposals that travel to it from other replicas. If the target has a proposal of its own, and that proposal could travel back to at least one of the candidates' replicas, the target's proposal joins the candidates. This is how a two-way edge produces a conflict when both ends change, while a one-way edge lets the upstream content overwrite a downstream edit: the downstream proposal has no edge back upstream, so it drops out of the competition.

**Reversion.** Suppose no proposal travels to the target. If the target has a proposal of its own that can travel somewhere else, the daemon leaves the target alone and lets the proposal go. Otherwise the daemon looks at the target's settled peers, meaning the replicas that still match their recorded snapshot and still have the file. Each settled peer whose content could travel to the target under the carriage rule becomes a candidate. In practice this restores a deleted file over an incoming `create` edge, fills an empty replica from an unchanged one, and undoes a downstream edit that has no edge to leave by.

**Resolution.** When the candidates agree on one content (or on absence), the target takes it. Otherwise the resolver rules are tried in order. A rule `a > b` prefers a candidate from site `a` over one from site `b`. A rule of the form `a mtime b`, `a size b`, or `a name b` compares the two properties and prefers the greater (newer, larger, alphabetically earlier name); a tie falls through to the next rule. A bare property applies to all pairs. Under a property rule a present file beats an absence. A candidate beating all candidates of differing content wins. When no rule decides, the daemon holds the target with a conflict reason, keeps its basis as it was, and reports the conflict.

**Unreadable peers.** An unreadable replica is held, with the read error as its reason. Any other replica whose proposal could travel to the unreadable one is held as well, because the daemon cannot tell whether the unreadable peer holds a competing change. A held replica keeps its recorded basis, so its edit or deletion is still pending when the peer becomes readable again.

**Operation.** Once a target's desired state is known, the operation follows: a write when the target has the file with different content, a create when it lacks the file, a delete when it has the file and the decision was absence, nothing when it already matches.

**Commitment.** After the plan applies, the daemon records a new basis for each replica it did not hold: the snapshot it wrote, a tombstone where it deleted the file or where a recorded snapshot was found missing, or the observed snapshot where nothing changed. A replica that had no record and still has no file keeps no record. Held replicas keep their old basis and are marked unresolved with their reason. While a key has an unresolved mark, the daemon evaluates it fully on each cycle even when nothing on disk changes, so a standing conflict stays visible.

**Forgetting.** When a key is missing at all of its replicas, the daemon clears its records, so a later file with the same name starts from nothing.

Under these rules the scenarios from the first design's tests come out the same way:

- A unilateral edit wins regardless of timestamps.
- Concurrent differing edits are held when no rule applies, or settled by `mtime` or precedence when a rule does.
- A one-way edge overwrites downstream edits silently.
- Three sites conflict only where their paths meet.
- Chains propagate transitively.
- Convergent edits commit with no operation.
- A late joiner conforms.
- A first run seeds identical content and contests differing content.
- Creation needs a `create` edge.
- A deletion propagates along `delete` edges or is restored through `create` edges.
- An edit against a deletion is held unless a rule decides; under `mtime` the file beats the absence.
- A mirror overwrites downstream changes and fans out.
- Replicas of a single site sync through a cycle in the flow.

Because each target's candidates are computed from the observations alone, the order in which the daemon visits the replicas does not affect the outcome.


## Plans and execution

An assignment says, for each replica, which content hash it should hold or that it should be absent. The executor turns that into writes in stages; a failure at any stage stops the later ones from running:

1. **Staging.** For each hash the plan needs, the executor finds a replica that was observed with that hash, reads the file once through the medium, hashes it as it goes, and stores it in the blob store under its hash. If the recomputed hash differs from the observed one, the file changed under the daemon and the plan aborts. If no replica holds the hash (because it is a transform output, or because the replica changed during the cycle), the executor looks in the blob store; if the blob store lacks it too, the plan aborts. Since the bytes come out of the blob store rather than from a donor path, a plan may safely overwrite a replica even when its previous content is on its way to another replica.
2. **Archiving.** The executor reads each present target about to receive different content or absence into the blob store, then writes an archive row recording which relation, key, replica, and path the content came from, along with its hash, its size, the reason for archiving, and the time. A failure here aborts the plan, so no content is destroyed unarchived.
3. **Journaling.** An intent row records the plan under its relation and key. Beside each file about to be created or deleted, the executor places a marker file named after the target plus `.replica.tmp`. The marker states the process id, relation, key, replica, operation, and start time, together with a note that it disappears once the operation commits.
4. **Writing.** Each write streams the blob to a temporary file in the target directory, applies the mode and modification time, syncs, renames over the target, syncs the directory. When times are synced, the modification time comes from the replica that proposed the content. A new file takes that replica's permission bits; an overwritten file keeps its own. Each delete removes the target, treating an already-absent target as done.
5. **Committing.** In a single transaction the executor replaces the records of unheld replicas with their outcomes, marks held replicas unresolved, and removes the intent row. It then removes the markers and any staged blob without an archive row.

When a stage fails, the executor removes the markers and the intent row, leaves the records as they were, and reports the error along with the operation and the path, so the next cycle plans again from the last consistent basis. An intent row found at startup means a previous run died mid-plan; the daemon reports it as an unfinished operation, shows it on the dashboard for the life of the process, and reconciles its key from scratch.

While the store exceeds its cap, the blob store evicts the oldest archived revisions of the relation holding the most bytes; the revision just written is exempt. Once the last archive row pointing at a blob is gone, the blob is removed from disk.


## State

The state database is SQLite in write-ahead mode, in the state directory. Its tables:

- `setting`: schema version, persisted gate flags (global and per relation), persisted pause flag.
- `relation`: name, policy fingerprint. When the fingerprint of a loaded relation differs from the stored one, the next cycle evaluates each key of the relation instead of skipping settled ones, then stores the new fingerprint.
- `replica`: relation, replica identity, path last seen, first seen time.
- `record`: relation, key, replica, state (absent or present), mtime, size, hash.
- `unresolved`: relation, key, replica, reason.
- `intent`: relation, key, plan text, started time.
- `archive`: id, relation, key, replica, path, hash, size, reason, time.

The schema version is 2. The daemon refuses a database written by the first design, since its replica identities and record states do not carry over; the message says which file was refused and which two versions are involved. The operator starts a fresh state directory. The first cycle then behaves as a first run: it aligns identical content and holds differing content for a decision.

Gate state is read from the database at startup and combined with the configuration so that the gate is closed when either says closed. When the operator opens a gate from the dashboard, the daemon stores the open state, so it survives a restart unless the configuration says to start closed.


## Configuration

The configuration is YAML. Its top level:

    state: ~/scripts/replica/live
    interval: 1m
    watch: true
    gate: false
    ignore: ["**/.DS_Store"]
    archive: { cap: 100MB }
    dashboard: { port: 18516, recent_events: 200, refresh: 10 }
    include: ["relations/*.yaml"]
    vars: { ... }
    profiles: { ... }
    relations: { ... }

`state` is the directory holding the database, the blob store, and the logs. Each path must be absolute or start with `~`, since the daemon's working directory under launchd is `/`.

**Variables** are written `${name}` and are expanded in each string of the file after includes are merged: paths, patterns, flow statements, action commands, log paths. This expansion leaves path variables in braces alone, so `{name}` and `${name}` can share a name without interfering. Inside a relation, `${relation}` is bound to the relation's name, then the top-level `vars`, then the relation's own `vars`, each layer overriding the one before. The loader rejects a reference to an undefined variable, quoting the string and the variable name in the message. A cycle among variables is an error.

**Profiles** are named bundles of relation fields. A relation lists one or more profiles under `profile`. The loader assembles the relation by deep-merging, in order, the profile named `_` if there is one, then each listed profile, then the relation's own body. In a deep merge, a later map adds or replaces keys inside an earlier map, while a later list or scalar replaces the earlier value outright. A profile may itself list profiles. The loader rejects a reference to an undefined profile. An unknown field is an error wherever it appears, including inside a profile; the message says which profile or relation holds it.

**Includes** are files or globs, resolved relative to the including file. An included file has the same top-level form as the main file, except that it may not include further files or set `state`, `interval`, or `dashboard`. Its vars, profiles, and relations merge into the main file's. A name defined in two files is an error whose message quotes both file names.

A **relation** has these fields:

    sites:
        source: ~/Desktop/mylibrary/
        dest: ~/scripts/desktop-library/
    where: { path: "*.js" }
    flow: "source <-> dest [edit create]"
    resolve: "source mtime dest"
    fanout: [project]
    ignore: ["*.tmp"]
    sync_times: true
    gate: false
    watch: true
    log: ${state}/logs/${relation}.log
    on: { ... }
    transform: { ... }
    each: { ... }

`sites` maps each site name to a pattern, a list of patterns, or a map from replica names to patterns. `where` constrains variables. `fanout` lists the variables that fan a site out into replicas; a fan-out variable must match a single segment and must come before the identity variables of its pattern, so each replica has a fixed root. `flow` and `resolve` are described above. `each` stamps the relation out once per combination of the listed values. Each copy is called `name-value`, with the value available to it as a variable. This is for several independent relations that share a form but need their own gates, logs, and histories.

The loader checks the following, in order, and reports the first failure together with the relation, site, field, and offending text:

- each path is absolute;
- each pattern is well formed under the restrictions in the patterns section;
- each variable constraint is a valid glob or regular expression;
- each variable either appears on all sites or is listed under `fanout`; when a variable appears on some sites but not others, the error suggests both fixes, adding it to the remaining sites or listing it as fan-out;
- each fan-out variable matches a single segment and comes before the identity variables of its pattern;
- each site in `flow` and `resolve` is declared;
- `flow` has at least one edge;
- no two relations share a name after `each` expansion;
- no footprint overlaps another footprint or a reserved path.

The example configuration and the live configuration are rewritten in this form in `config.example.yaml` and in the operator's `config.yaml`.


## Triggers and actions

**Watching.** With `watch: true`, the daemon registers the root of each replica and the directory of each fan-out variable with the operating system's change notification facility through the `notify` crate. An event under a root marks the relation dirty. After a quiet period (`debounce`, default two seconds) the main loop runs a cycle over the dirty relations alone, then resumes the schedule. The daemon's own writes arrive as events too; the following cycle finds them matching the basis and does nothing. A relation may set `watch: false` to stay on the schedule alone. The scheduled cycle still runs at `interval` and still decides what is correct, so a lost or coalesced event delays a sync by at most one interval.

**Actions.** The `on` block of a relation maps event kinds to commands:

    on:
        changed: "make -C ~/scripts/site"
        resolved: "scp ${path} host:backup/${relation}/"
        conflict: "osascript -e 'display notification \"${key}\" with title \"replica conflict\"'"
        error: "..."

The kinds are `changed`, `resolved`, `held`, `conflict`, and `error`. A `changed` action runs once per cycle in which at least one plan of the relation applied. A `resolved` action runs once per key whose plan applied. For each replica that was written, the command has `${key}`, `${path}`, `${replica}`, `${site}`, and `${op}` filled in. The daemon runs a command through `sh -c` with the state directory as its working directory. The command inherits the daemon's environment with `REPLICA_RELATION`, `REPLICA_KEY`, and `REPLICA_EVENT` added. It is killed after a minute unless the action sets its own `timeout`. The last line of the command's output is carried in the action event, which the relation's log records. A nonzero exit or a timeout is reported as a failed action; the cycle's own outcome is unaffected. An action that writes into a replica of its own relation causes a transition at the next cycle like any outside edit; an idempotent action converges after one extra cycle, since the daemon finds the rewritten file unchanged by hash.

**Transforms.** The `transform` block maps an edge to a command:

    transform:
        "source -> site": "sassc ${in} ${out}"

While content travels along that edge, the executor stages the sender's blob, runs the command with `${in}` bound to the blob path and `${out}` to a temporary path, hashes the output, stores it as a blob, and assigns that hash to the receiver. The receiver's basis records the output hash, so it reads as unchanged next cycle. A site fed through a transform must not have outgoing edges, since there is no way to map its content back. The loader rejects a flow that gives it one. A failing or timed-out command fails the plan for that key; the error is reported and the key is planned again on the next cycle. A command whose output varies between runs causes no churn, since the transform runs only when the sender's transition calls for it.


## Interfaces

**Command line.** The binary takes a subcommand:

    replica run --config PATH [--once] [--gate]
    replica check --config PATH
    replica explain --config PATH [RELATION]
    replica plan --config PATH [RELATION]
    replica status [--port N] [--json]
    replica sync | pause | resume | restart | halt [--port N]
    replica gate open|closed [--relation NAME] [--port N]
    replica history RELATION [--port N] [--json]
    replica forget --config PATH RELATION REPLICA

`check` loads the configuration, expands the replicas, reports overlaps, and exits nonzero on any problem. `explain` prints, for each relation, the profile chain it was assembled from, the expanded variables, each site's patterns with their roots and depths, the discovered replicas, the flow closure per kind, and the resolver. `plan` runs the discovery and decision stages against the live state database without writing anything, then prints what the next cycle would do. The control subcommands post to the running daemon's dashboard port. `forget` clears the records of one replica, for the case where the path behind an identity was replaced rather than moved.

**Dashboard.** The loopback HTTP server keeps its protections: GET and POST answer only to loopback hosts, POST only with a matching origin. While the engine is idle, pages and `status.json` read live state. While a cycle runs, they read the report published after the previous cycle instead. Each region of the page has its own rendering function: the header, the status strip, the unfinished operations, a relation section, the activity. A small set of helpers for tags, tables, and links keeps each of those functions under a screen. The stylesheet and favicon are unchanged. `status.json` adds the watch state and the dirty relations.

**Logs and events.** The engine emits events through hooks as before: resolved, held, skipped, pending, warned, errored, stale intent, plus action started and finished, watch event received, cycle begun and finished. A condition persisting across cycles is reported once when it appears and again whenever it changes, with no report on the cycles between.


## Modules

- `src/lib.rs`: module list
- `src/pattern.rs`: patterns, variables, constraints, match and substitute, root, depth, footprint
- `src/site.rs`: sites, replicas, expansion through the medium
- `src/observe.rs`: snapshots, observations, bases, transitions
- `src/flow.rs`: flow statements, kind sets, closure, resolver rules, the policy
- `src/decide.rs`: situations, assignments, the named rules
- `src/medium.rs`: the medium trait and the local filesystem
- `src/blobs.rs`: the content-addressed blob store and eviction
- `src/store.rs`: SQLite schema and queries
- `src/apply.rs`: the executor: staging, archiving, journaling, writing, committing, transforms
- `src/engine.rs`: cycles, discovery, reports, gates, outcome deduplication
- `src/event.rs`: events, tones, hooks
- `src/action.rs`: action templating and running
- `src/watch.rs`: change notification into the main loop
- `src/config/mod.rs`: loading, validation, the typed result
- `src/config/text.rs`: the YAML form as typed structs
- `src/config/merge.rs`: variables, profiles, includes, deep merge, each
- `src/dashboard/mod.rs`: the server and routes
- `src/dashboard/http.rs`: requests, responses, loopback checks
- `src/dashboard/render.rs`: pages
- `src/dashboard/status.rs`: status.json
- `src/cli.rs`: argument parsing and the control subcommands
- `src/main.rs`: the run loop
- `tests/conformance.rs`: each scenario of the first design under the new form
- `tests/redesign.rs`: scenarios the first design could not express

The YAML parser is `serde_norway`, a maintained continuation of `serde_yaml` with the same interface, since `serde_yaml` is archived.


## Migration

The configuration form changes entirely. The operator rewrites `config.yaml` by hand with `config.example.yaml` as a guide, checks it with `replica check`, and inspects the result with `replica explain`. The state directory starts fresh, since the first design's record keys do not carry over. The first cycle behaves as a first run, aligning identical content and holding differing content for a decision, so the safe path is to run it under a closed gate and read the dashboard before opening the gate.


## Decisions left open

- `changed` actions could also fire for plans held under a gate, so that a notification goes out before the operator opens it.
- A delete-ratio guard could be added: a cycle planning to delete more than a configured fraction of a relation's keys holds the relation until an operator opens it. The missing-root guard covers the common case of an unmounted volume; the ratio guard covers a mounted volume emptied by mistake.
