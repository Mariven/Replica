# Replica

Replica is a local file sync service.

A *relation* is a statement that certain files share the same content. Replica checks those files on a timer. When they no longer match, it copies or deletes so they do.


## Relations

A relation has an identifier, labels (and the files those labels point to), an optional glob, and policies.

- Identifier: the relative path that related files share. Two files with the same identifier are treated as the same file.
- Label and ref: each side of a relation has a *label*, a name you pick (`source`, `dest`, or anything else). A label expands to one or more *refs*: the actual folders or path templates where the files live.
- Pattern: a glob that picks which relative paths belong to the relation. Non-matching paths are ignored. `*` stops at `/`.
- Status: whether a file is present, missing, or unreadable right now, compared to the *record* of the last sync: unchanged, edited, appeared, or disappeared.
- Policy: given that picture, decide which copies and deletes to run.

The *record* remembers how each ref looked just after the last successful sync. Replica detects changes by comparing the live files to that record.

A cycle does this:

1. Expand every label into the folders it currently matches.
2. Walk those folders and collect identifiers (relative paths that match a label).
3. For each identifier:
    - Read the status of each ref.
    - If everything matches the record, move on.
    - Otherwise ask the policy for a plan, then apply it.

## Policies

Each relation has four policy fields:

- `content`: `bidirectional` (default) or `mirror(label)`. Bidirectional copies from whichever side changed. If two sides changed in different ways, that is a conflict. `mirror(label)` always copies from the named label.
- `conflict`: `skip` (default), `newer` (the latest mtime wins), or `prefer(label)`.
- `on_new`: `restrict` (default: do not create a file at a ref that has never had one) or `replicate`.
- `on_delete`: `restore` (default: put a deleted file back from a surviving copy), `propagate`, or `skip`.

If you omit them, the defaults are bidirectional edits, skip conflicts, restrict new files, and restore deletions.

## Configuration

[config.example.yaml](config.example.yaml) shows every field. A typical file looks like:

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

Each key under `entries` is a relation. Any field that is not a reserved word (`pattern`, `content`, `conflict`, `on_new`, `on_delete`, `sync_times`, `log`) is a label. `source` and `dest` are ordinary names, not keywords.

An endpoint (the value of a label) can be:

- A bare path string: a single root folder.
- A path string with `{holes}`: a whole-path template. Each hole matches one path component, and the hole values are the file's identity. Two templates with the same holes describe the same files under different layouts, for example `~/scripts/skills/prose.md` and `~/.claude/skills/prose/SKILL.md`. Every label on such an entry must be a template with the same holes. Files that do not match are ignored, so you usually do not need `pattern:`.
- A mapping, for the longer forms: `root:` (optionally with `rename:`), `many:` (a list of roots), or `scan:` (one root per directory entry, rediscovered each cycle).

The special entry `_` holds defaults. They are copied into every other entry that does not set the same field. Default keys and string values may contain `{placeholders}`, filled from the entry's own string fields plus `{name}` (the entry's key). That is how the `log:` default above gives each entry its own log file. Expansion is recursive: substituted text can introduce more placeholders. An unknown placeholder is left as written. A reference cycle is a load error. Values you write on the entry itself are left alone.

`sync_times: true` (the default) copies the donor's mtime onto the destination after a copy.

## Running

For development:

```sh
cargo run --release -- --config config.yaml  # periodic cycles at the configured interval
cargo run --release -- --config config.yaml --once  # one cycle, then exit
cargo test  # unit and end-to-end tests
```

As a service: [docs/service-mac.md](docs/service-mac.md) covers installing, controlling, and removing the launchd agent. The dashboard (`http://localhost:<dashboard.port>`) shows every relation and has pause / resume / restart / halt controls. Those controls are ordinary POSTs to the loopback server. `GET /status.json` returns the same state for scripts.

## Layout

- `src/identifier.rs`: relative paths and glob patterns
- `src/label.rs`: labels, refs, and path templates
- `src/status.rs`: file snapshots and change classification
- `src/policy.rs`: decisions (no filesystem access)
- `src/store.rs`: the record database and the intent journal
- `src/archive.rs`: preserved copies of overwritten and deleted content
- `src/engine.rs`: discover, read, decide, apply
- `src/config.rs`: YAML loading
- `src/main.rs`: the poll-loop daemon and the dashboard server
- `src/style.css`, `src/favicon.svg`: dashboard assets, compiled into the binary and served at `/style.css` and `/favicon.svg`
- `tests/cases.rs`: end-to-end cases covering the original scenarios, templates, defaults, lifecycle, conflicts, crash recovery, and safety guards
- `docs/`: [todo.md](docs/todo.md), [service-mac.md](docs/service-mac.md). `docs/old/` holds earlier design notes (untracked)
- `live/` (untracked): the state database and logs of a running installation

## Safety

- Every write goes through a temp file in the same directory, then an atomic rename. An intent row is written before the mutation and cleared after commit. If the process dies, the leftover intent is the evidence, and the next start resolves those files again.
- Creates and deletes also drop a plain-text marker (`<file>.replica.tmp`) next to the change, removed when the operation commits. If you can still read the marker, the operation is running or died partway.
- If a root folder has records but has suddenly disappeared (unmounted volume, renamed folder), Replica treats it as unavailable and waits. It does not treat the files as a mass deletion, and it does not write into a missing mount.
