# Replica

Replica is a local file sync service.

The basic unit of synchronization is a *relation*: a statement that certain files share identical content. Replica periodically verifies that these relations hold, and, when they don't, it changes files so that they do.


## Writing Relations

There are five primitive notions to a relation: its identifier, its labels, the referents of those labels, its optional filtering pattern, and its policies.
- Identifier: To construct a relation, you first specify the *identifier*, or the relative path held in common by all related files.
- Label/Ref: Individual files can be referred to by symbolic *labels* dependent on their location, and are interfaced with implicitly, as the referents (*ref*s) of these labels.
- Pattern: a glob pattern that filters which relative paths matter. Paths that don't match are ignored.
- Status: the current appearance of a file (present, missing, or unreadable), classified against the record into: unchanged, edited, appeared, or disappeared.
- Policy: a function from the sync context to a plan. A plan names copy and delete operations on labels for the engine to perform in order to resolve differences.

To detect changes, Replica maintains a *record*, or a database of what each referent looked like directly after the last sync, and checks whether they've changed since then.
Steps in a cycle:
- enumerate all folders that labels may match to files in,
- walk each folder and attempt to find a referent for each label by comparing its name to the label's identifier
- then, per identifier: 
  * observe status of each referent of the identifier's labels, continue if everything matches the record,
  * otherwise use the policy to determine how to proceed, filling out its symbolic labels with the actual referent files

## Policies

Each identifier has policies for handling the following four cases:
- `content`: `bidirectional` (default) or `mirror(label)`. Bidirectional is transition-based: whichever label's referent changed against its record gets mapped onto the others; if multiple have changed, a conflict arises, and must be handled by a conflict policy.
- `conflict`: `skip` (default), `newer` (latest edit wins), `prefer(label)`.
- `on_new`: `restrict` (default: never create files at labels with nonexistent referents) or `replicate`.
- `on_delete`: `restore` (default: repair deletions from a surviving copy), `propagate`, or `skip`.

If not specified, the default policies are `bidirectional` transfer of edited content, `skip` on edit conflicts, `restrict` creation on new files, `restore` deleted files from others if available.

## Configuration

See [config.example.yaml](config.example.yaml) for the full surface. The shape:

```yaml
state_db: "~/scripts/replica/live/state.db"
interval: "1m"
dashboard:
    port: 18516

entries:
    _:
        content: "bidirectional"
        log: "~/scripts/replica/live/logs/{name}.log"

    mylibrary:
        source: "~/Desktop/mylibrary"
        dest: "~/scripts/desktop-library"
        pattern: "*.js"

    nginx-default:
        source: "/etc/nginx/conf.d"
        dest: { root: "~/Desktop", rename: "nginx-conf.txt" }

    skills:
        source: "~/scripts/skills/{name}.md"
        dest: "~/.claude/skills/{name}/SKILL.md"
        on_new: "replicate"

    style-guide-all:
        source: "~/scripts/rules"
        dest:
            scan:
                template: "~/Desktop/Projects/{project}/docs/style"
                hole: "project"
                dir: "~/Desktop/Projects"
        pattern: "main_style.md"
        content: "mirror(source)"
        on_new: "replicate"
```

Each key of `entries` names one relation; any field of an entry that is not a reserved word (`pattern`, `content`, `conflict`, `on_new`, `on_delete`, `sync_times`, `log`) declares a label. `source`/`dest` are conventions, not keywords.

An endpoint is one of:
- a bare path string — a plain root folder;
- a path string with `{holes}` — a whole-path template. Each hole matches one path component, and the hole values are the file's shared identity, so two templates with the same holes form an invertible rewrite between naming schemes: `~/scripts/skills/prose.md` and `~/.claude/skills/prose/SKILL.md` are the same file. All labels of such an entry must be templates with the same holes; non-matching files are ignored, so no `pattern:` is needed;
- a mapping, for the longer forms: `root:` (optionally with `rename:`), `many:` (a list of roots), or `scan:` (roots discovered per directory entry, re-listed each cycle).

The special entry `_` holds defaults, merged into every other entry where the entry has no value of its own. Default keys and string values may contain `{placeholders}`, resolved against the entry's own scalar string fields plus `{name}`, the entry's key — so the `log:` default above lands each entry's events in its own file. Expansion is recursive (substituted text can assemble further placeholders); an unresolvable placeholder stays literal, and a reference cycle is a load error. Entry values themselves are never rewritten.

`sync_times: true` (default) propagates the donor's mtime on copy.

## Running

Directly, for development:

```sh
cargo run --release -- --config config.yaml  # start periodic cycling at set interval
cargo run --release -- --config config.yaml --once  # single cycle
cargo test  # unit tests
```

As a service: see [docs/service-mac.md](docs/service-mac.md) for installing, controlling, maintaining, and removing the launchd agent. In brief, the dashboard (`http://localhost:<dashboard.port>`) shows every relation's state and has pause / resume / restart / halt controls, all of which are plain POSTs to the loopback server; `GET /status.json` serves the same state machine-readably.

## Layout

- `src/identifier.rs`: relative paths and glob patterns
- `src/label.rs`: labels and their refs (folders), path/name mappings
- `src/status.rs`: file snapshots and change classification
- `src/policy.rs`: decisions; pure, no filesystem
- `src/store.rs`: record database and intent journal
- `src/engine.rs`: the cycle: discover, read, decide, apply
- `src/config.rs`: the YAML surface
- `src/main.rs`: the poll-loop daemon and dashboard server
- `src/style.css`, `src/favicon.svg`: dashboard assets, compiled into the binary and served at `/style.css` and `/favicon.svg`
- `tests/cases.rs`: end-to-end cases: the prespec cases, templates, defaults, lifecycle, conflicts, crash recovery, safety guards
- `docs/`: [todo.md](docs/todo.md), [service-mac.md](docs/service-mac.md); `docs/old/` holds the prespecs (untracked)
- `live/` (untracked): the state db and logs of a running installation

## Safety behaviors

- Every write goes through a same-directory temp file and an atomic rename; an intent row is journaled before mutating and cleared after commit, so a crash leaves evidence rather than corruption, and the affected files are simply re-resolved on the next start.
- Creates and deletes additionally leave a plain-text marker file beside the change (`<file>.replica.tmp`), removed when the operation commits: a marker you can read means the operation is in progress or died partway.
- A root folder that has records but is suddenly missing (an unmounted volume, a renamed folder) is treated as unavailable, not as a mass deletion: the relation waits instead of propagating deletions or writing into a dead mount point.
