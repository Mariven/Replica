# Replica

Replica is a local file sync service.

Replica synchronizes files across local directories according to configured relations. During periodic sync cycles, Replica inspects these locations, copying or deleting files to reconcile differences.


## Relations

Replica processes files according to relation configurations:

- Identifier: Common relative path for paired files. Replica uses this path to match corresponding files across endpoints.
- Label and ref: User-assigned names such as `source` or `dest` for relation endpoints. Replica resolves each label into concrete refs, one per directory or path template the label expands to. A label listing several folders, or a `scan:` endpoint, yields one ref per folder; the dashboard names them by the scan hole value or by the shortest path suffix that tells them apart.
- Pattern: Optional glob filter for relative paths. `*` and `?` stay within one path segment, so `*.js` matches top-level files alone; `**` crosses segments and `{a,b}` lists alternatives. Matching is case-sensitive even on case-insensitive volumes. Symbolic links are skipped.
- Status: File condition relative to the previous sync record. Replica records status as unchanged, edited, appeared, disappeared, or unreadable. The dashboard shows these as `in sync`, `edited`, `new`, `deleted` (recorded, now missing), `absent` (never recorded there), and `unreadable`, and tags each pending row with its plan as `next sync`, `held` under a gate, or `waiting` on a conflict.
- Propagation: Directed graphs define how edits, additions, deletions move between labels, accompanied by rules to settle conflicts.
- Donor and settled peer: When a ref needs new content, the donor is the ref whose file is copied to it. A settled peer is a ref whose file is unchanged since the last record, so its content is the agreed baseline.

After each successful sync, Replica stores file metadata in the state record. On later cycles, Replica compares filesystem entries against this baseline to identify modifications. A file whose size and modification time (to the millisecond) match its record is taken as unchanged without re-reading it; an edit that keeps the size and lands within the same millisecond, or a restore made with a tool that preserves timestamps, is therefore not noticed until the file changes again.

Replica executes each sync cycle in sequential stages:

1. Expand configured labels into directory paths, and skip any relation whose roots overlap another root or one of Replica's own files.
2. Traverse those directories to collect relative path identifiers, descending only as deep as the pattern or template allows.
3. For each identifier:
    - Inspect filesystem status across associated refs.
    - Proceed immediately when collected statuses match the recorded baseline.
    - Otherwise evaluate relation policies to build and execute an action plan.

## Propagation

Define directional file propagation within `propagate`:

- `change`: Directed edges for modified files. The syntax `a -> b` routes edits from label a to label b, `a <- b` routes from b to a, `a <-> b` routes bidirectionally, and `{a, b} -> c` routes from both a and b to c. Semicolons separate statements.
- `create`: Directed edges for new files targeting empty locations. A create edge targeting an empty path also restores files deleted at that destination.
- `delete`: Directed edges for deletions targeting existing files.
- `conflict`: Precedence rules for conflicting changes across refs. Syntax `a > b` prefers changes from a over b, `a < b` prefers b, and `a mtime b` selects the newer modification timestamp. Criterion `size` selects the larger file, while `name` selects the first file name alphabetically. A standalone criterion like `mtime` applies across all candidate pairs.

Reachability is transitive. In the configuration `a -> b; b -> c`, edits at a propagate through to c. A field left blank means no edges of that kind.

Replica evaluates each ref independently. Modified refs propose content along outgoing reachability edges. When multiple connected refs present different modifications, Replica resolves target content using the declared conflict rules. For example, under `a -> b` with edits on both sides, edits from a overwrite b. Under `a <-> b`, differing edits require conflict resolution.

When a ref has incoming edges without local modifications, it accepts updates from settled peer refs. Deleted files restore across incoming create edges, empty endpoints populate from peers, and local changes lacking return paths revert to upstream versions.

Differing file contents across competing sources produce a conflict. Replica applies resolution rules in order until a rule selects a clear candidate; a rule whose property ties, such as two files with equal size under `size`, leaves the decision to the next rule. If no rule decides, Replica leaves the destination ref unmodified, marks the conflict in dashboard reports, and reevaluates the ref during subsequent cycles. Unrelated refs for the file continue syncing normally. An unreadable ref, such as a path that is a directory or a symbolic link, is marked unresolved on its own; refs whose edit or deletion would travel to it wait with their baseline unchanged, so the change is still delivered, or still detected as a conflict, once the ref can be read, while refs it does not touch keep syncing.

If a file appears at an endpoint where peer refs already record existing history, Replica aligns the new file with those peers rather than treating it as a new edit.

## Configuration

Consult [config.example.yaml](config.example.yaml) for every configuration option. Example configuration:

```yaml
state_db: "~/scripts/replica/live/state.db"
interval: "1m"
dashboard:
    port: 18516

entries:
    _:
        propagate:
            change: "source <-> dest"
            create: "source -> dest"
        log: "~/scripts/replica/live/logs/{name}.log"

    mylibrary:
        labels:
            source: "~/Desktop/mylibrary"
            dest: "~/scripts/desktop-library"
        pattern: "*.js"

    nginx-default:
        labels:
            source: "/etc/nginx/conf.d"
            dest: { root: "~/Documents", rename: "nginx-conf.txt" }
        pattern: default.conf

    skills:
        labels:
            source: "~/scripts/skills/{name}.md"
            dest: "~/.claude/skills/{name}/SKILL.md"

    style-guide-all:
        labels:
            source: "~/scripts/rules"
            dest:
                scan:
                    template: "~/Desktop/Projects/{project}/docs/style"
                    hole: "project"
                    dir: "~/Desktop/Projects"
        pattern: "main_style.md"
        propagate:
            change: "source -> dest"
            create: "source -> dest"
```

Top-level keys are `state_db` (required), `interval` (default `30s`; units `ms`, `s`, `m`, `h`; zero is rejected), `gate` (start with every write held), `archive` (`dir`, default an `archive` folder beside the database, and `cap`, default `100MB`), and `dashboard` (`port`, `recent_events`, `refresh`).

Define relation rules as entries under the top-level `entries` map. Each entry specifies file targets under `labels` and synchronization rules under `propagate`, with optional keys for `pattern`, `sync_times`, `gate`, and `log`. Label keys such as `source` and `dest` represent arbitrary user-selected names; the dashboard lists them in the order written. Wrap propagate expressions in quotation marks because unquoted `{a, b}` constructs parse as YAML mappings. Every propagate section requires at least one directional edge. YAML merge keys (`<<: *anchor`) are honored.

Each label maps to an endpoint or a list of endpoints. Configurations support three endpoint formats:

- Path strings referencing a root directory.
- Path templates with `{token}` holes, written as a string or as `{ path: "..." }`. Each bracketed token matches a single path segment to determine file identity. Matching templates with identical hole variables align files between differing directory trees, for example `~/scripts/skills/{name}.md` and `~/.claude/skills/{name}/SKILL.md`. Every endpoint in such an entry must include matching hole variable names. Files that do not match the template layout skip evaluation automatically.
- Mapping objects: `root:` with an optional `rename:` target, or `scan:` to dynamically discover matching directory trees on every sync cycle. A `rename` endpoint holds exactly one file, so its relation must carry a `pattern` naming that file (the loader rejects a rename without one, and the engine refuses the relation whenever the pattern matches more than one file), and at least one endpoint of the relation must be a plain root, scan, or template.

Every path must be absolute or start with `~`. Roots must be disjoint: no root may lie inside another root, in any entry, and no root may contain the state database, the archive directory, or a log file. A root that contains one of Replica's own files is refused at startup, by `--check`, and by a restart request. Roots that nest inside each other are reported on the dashboard and skipped each cycle while the other relations keep syncing, since a scan can produce such a pair at any time. Narrowing `pattern` drops the records of files it no longer covers, so widening it later starts those files afresh rather than from a stale baseline.

The `_` entry defines shared fallback values across relation configurations. Defaults merge by top-level section key. Defining a `propagate` block on a relation replaces the default propagation settings completely. Variable placeholders inside default values expand against top-level string attributes of the relation alongside `{name}` for the entry key. Replica recursively expands nested variable references, preserving unresolvable placeholders as literal text. Circular variable references cause configuration load errors. Explicit relation settings override default values.

Setting `sync_times: true` (enabled by default) copies donor modification timestamps to destination files during file replication. A file created by a copy takes the donor's permission bits; an overwrite keeps the destination's own bits.

Setting `gate: true` on an entry, or at the top level for every entry, holds writes from startup while Replica keeps planning and reporting. Open a gate from the dashboard. The `--gate` flag closes the global gate for one run.

## Running

For development:

```sh
cargo run --release -- --config config.yaml  # periodic cycles at the configured interval
cargo run --release -- --config config.yaml --once  # one cycle, then exit
cargo run --release -- --config config.yaml --check  # load the config, expand refs, report overlapping roots, exit
cargo test  # unit and end-to-end tests
```

`--config=PATH` is accepted as well; `--help` and `--version` print and exit. Only one daemon may run against a database: a lock file beside it refuses a second instance.

For launchd service setup and maintenance on macOS, follow [docs/service-mac.md](docs/service-mac.md). The web dashboard at `http://localhost:<dashboard.port>` lists configured relations and their state, and offers these controls, all as HTTP POST requests from the loopback interface:

- `/sync` runs a cycle now, `/pause` and `/resume` hold and release the schedule.
- `/gate?to=closed` and `/gate?to=open` set the global gate; add `rel=<entry>` for one entry's gate.
- `/restart` validates the config and exits non-zero so launchd relaunches; `/halt` exits zero. Both finish the file operation in progress first.

`GET /status.json` reports process state, gate, counters, and one state per relation. `GET /history?rel=<entry>` lists archived revisions of a relation, with `GET /blob` serving one for download. A `?rel=<entry>&all=1` query on the main page shows every row of a relation instead of the first fifty of each kind. While a sync runs, pages and status are served from the report published after the previous sync, marked as such; between syncs they read live state. The daemon keeps `state.db-wal`, `state.db-shm`, and `state.db.lock` beside the database.

`--once` exits with status 1 when any relation reported an error during its cycle.

## Layout

- `src/identifier.rs`: relative paths and glob pattern parsing
- `src/label.rs`: label definitions, target refs, and path templates
- `src/status.rs`: file snapshots and change classification
- `src/policy.rs`: sync decisions detached from filesystem operations
- `src/store.rs`: SQLite persistence for sync records and intent journals
- `src/archive.rs`: archived revisions of overwritten and deleted files
- `src/engine.rs`: synchronization engine coordinating discovery, status checks, policy evaluation, filesystem mutation
- `src/config.rs`: YAML configuration parsing and validation
- `src/main.rs`: poll loop daemon and HTTP dashboard server
- `src/style.css`, `src/favicon.svg`: dashboard assets embedded into the binary and served at `/style.css` and `/favicon.svg`
- `tests/cases.rs`: integration tests covering sync workflows, templates, lifecycle events, conflicts, crash recovery
- `docs/`: documentation guides [todo.md](docs/todo.md), [service-mac.md](docs/service-mac.md), and [design-style.md](docs/design-style.md) (the dashboard's visual style), with untracked historical notes in `docs/old/`
- `live/` (untracked): state database, archive, and activity logs for running daemon instances

## Safety

- Before a plan touches disk, Replica reads every donor file and archives every file the plan will overwrite or delete. Only then does it record the intended changes in the SQLite journal and place a marker file (`<file>.replica.tmp`) beside each path it is about to create or delete. Each write goes to a temporary file in the target directory, is flushed to disk, and is renamed into place. When every operation succeeds, the records are committed in one transaction and the journal row and markers are removed. If an operation fails, the markers and the journal row are removed, the records stay as they were so the next cycle re-plans from the last consistent baseline, and the error names the operation and the path.
- A journal row that survives a crash is reported at the next start as an unfinished operation and stays listed on the dashboard for that run. Markers left behind by a crash are diagnostic: they name the relation, file, and operation, and walks skip them.
- SIGTERM, SIGINT, and the dashboard's halt and restart stop Replica between files, never in the middle of one.
- When a configured root folder is missing, as with an unmounted drive or renamed folder, and that root has records, Replica flags the relation as unavailable and skips it until the folder returns. This guard applies when the folder itself is gone. A mount point that remains as an empty directory looks like a folder whose files were all deleted, and delete edges then propagate those deletions; unmount cleanly or close the gate before such changes.
