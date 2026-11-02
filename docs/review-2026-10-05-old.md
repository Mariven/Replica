# Replica review, 2026-10-05

## Do these first

1. The example config in the repo fails on its first cycle. `config.example.yaml:39` gives the nginx rename target `root: ~/Desktop`, a folder that contains `~/Desktop/mylibrary` plus the scanned `~/Desktop/Projects/*/docs/style`. The daemon prints `overlapping roots`, then exits with status 1. `README.md:74` carries the same example. The rule against nested roots appears in no document. Reproduced with the release binary.
2. Labels with exactly two paths in a list are read as `{root, rename}`. At `src/config.rs:174` the `LabelSpec` enum lists `One(EndpointSpec)` before `Many`, and serde deserializes a two-element sequence positionally into `EndpointConfig { root, rename }`. The second path becomes a file name under the first. The `cursor-rules` entry at `config.example.yaml:48` has exactly two paths. Put `Many` before `One`, or write a short manual `Deserialize` that dispatches on the YAML value type to keep the unknown-key errors. Reproduced with the release binary.
3. The `hermes-soul` relation walks about 140k files every minute, plus once more on every dashboard refresh. Its source is `~/.hermes` with `pattern: "SOUL.md"`. `walk_identifiers` at `src/engine.rs:617` descends the whole tree and builds a `Vec<Vec<String>>` for every file before applying the pattern. The live daemon, pid 81718, has used 330 CPU-minutes in 5 days. Derive a maximum depth from the pattern or template, given that a glob without `**` or `/` matches at depth 1 alone, then stop descending past it. The `scan:` endpoints and the `*.mdc` relation have the same structure. Reproduced with the release binary, then confirmed with `ps`.
4. The `evict` function at `src/archive.rs:44` can delete the revision that `preserve` just inserted. Its guard keeps at least one row in the whole table instead of protecting the new row. When the new revision belongs to the relation with the largest total and is that relation's oldest row, `evict` removes it along with its blob, `preserve` returns Ok, and the overwrite at `src/engine.rs:333` proceeds with no backup. Have `archive_insert` return the row id, then exclude that id from both selection queries. Reproduced by the review agents.
5. `write_atomic` at `src/engine.rs:579` resets permission bits. It creates the temp file with the default umask mode instead of the donor's mode. As a result 0600 and 0755 both become 0644 on every copy, and in a two-way relation the change propagates back to the origin. Read the donor's `permissions()` together with its bytes, mask them to 0o777, and call `set_permissions` on the temp file before the rename. Treat a chmod failure as a warning instead of an error, since exFAT or SMB destinations reject chmod. Keep `fs::write` for the temp file. Leftover temp files stay on disk after a crash, where `create_new` would fail on every later cycle. Whether an overwrite keeps the destination's mode or takes the donor's mode is a design decision. `cp` keeps the destination's. Reproduced by the review agents.
6. Web pages from any origin can post to the control endpoints. `src/main.rs:226` reads the request line alone, then handles POST `/halt`, `/restart`, `/pause`, and `/gate?to=open` without a look at `Host`, `Origin`, or `Sec-Fetch-Site`. A cross-site form post succeeds. At `src/main.rs:246` a missing or misspelled `to` parameter opens the gate as well. Require a loopback `Host`, reject POSTs that carry a foreign `Origin`, and match `to` explicitly with a 400 for anything else. Reproduced by the review agents, including in a real browser.
7. The overlap check in `validate_no_overlaps` has three defects. Reproduced with the release binary.
    - Its error propagates through `cycle()?` at `src/main.rs:130` and exits the process. A `mkdir` in a scanned tree can therefore kill a running daemon, then launchd relaunches it into the same failure.
    - The comparison at `src/engine.rs:658` treats a `rename:` endpoint as claiming its whole root, the defect behind the example config failure.
    - The check at `src/engine.rs:647` ignores the daemon's `state_db`, archive directory, and relation logs. A log inside a root therefore re-syncs itself forever. A `dest > source` rule can even replace the live SQLite file with a stale copy.
    Compare footprints, meaning `root/leaf` for Fixed refs. Report an overlapping pair as an `Errored` event, then skip those relations instead of exiting. Reject reserved paths inside roots at load time and again per cycle.
8. The dashboard and the engine disagree. `report_relation` at `src/engine.rs:401` is a second copy of `resolve()` that has drifted in four places. Reproduced with the release binary.
    - The report omits the unavailable-root guard from `src/engine.rs:176`. An unmounted drive therefore renders as a red `deleted` cell with a `delete at dest` plan the engine will refuse to run.
    - The report computes `all_recorded` after flattening the tombstone at `src/engine.rs:447`. It therefore promises copies the engine skips.
    - `row_attention` at `src/main.rs:597` counts any one-sided file as `awaiting sync` even when no edge covers it. While such a file exists the global state stays on `N awaiting sync`, with `/status.json` reporting the same inflated `awaiting_sync`.
    - Relations whose roots are both missing show a green `in sync` tag at `src/main.rs:737`.
    Extract one `inspect()` helper for both paths, carry an `unavailable` reason on `RelationReport`, and derive attention from the row's plan plus note instead of from presence.
9. `resolve` at `src/engine.rs:235` re-emits `Held` and `Skipped` events for every gated, unrecorded, or unresolved identifier on every cycle. Each conflict produces two lines per cycle in stderr, in the relation log, and in the 200-entry Activity ring. In my run the Activity list consisted of conflict repeats within a minute, with the real events pushed out. With a gate closed on thousands of files the volume reaches hundreds of MB per day. Hash each identifier's outcome in a map held by the engine, then emit on change alone. Reproduced with the release binary.
10. Failed writes get reported as crashes, with the cause dropped. `apply()` at `src/engine.rs:263` writes the intent row plus the marker files first, then every later `?` returns before clearing them or committing the operations that already succeeded. The next start therefore reports `previous run died mid-operation`. The error text comes from `e.to_string()` at `src/engine.rs:198`, a call that drops the anyhow chain, while the fs calls carry no context. The log line reads `Permission denied (os error 13)` without a path or a step. SIGTERM and SIGINT kill the process wherever it is. Every `launchctl bootout`, logout, or Ctrl-C during a cycle therefore ends the same way. Prepare first, meaning read donors then archive, before writing the journal. Clean up on error, format errors with `{e:#}`, add `with_context` naming the operation plus the path, and install a stop flag checked between identifiers. Reproduced by the review agents.

## Correctness and safety

Correctness items came up on the way to the requested categories. Each one carries its severity at present scale.

- Deleted files that get recreated lose their new content. Under `change: source <-> dest` without a delete edge, a deletion at source leaves a tombstone. When the user recreates the file, `src/policy.rs:394` treats the appeared slot as a newcomer to peers with history. The stale peer copy therefore overwrites the new file while the log says `resolved (1 ops)`. The README documents this alignment rule. It is therefore a surprising design decision more than a plain bug. The verifier's fix, treating tombstoned slots as proposals, would break the documented feature. The decision needed is whether a tombstone means unknown to this ref or deleted at this ref. Reproduced by the review agents. Severity medium.
- Each ref's id depends on the sibling count and on the label name. `Ref::id` at `src/label.rs:220` adds `?root=` only when a label has more than one endpoint. Adding or removing a sibling folder therefore re-keys the unchanged refs. Those refs have no record under the new id, get treated as newcomers, and lose any unsynced edit to the peer copy. Repointing a single-endpoint label at a different folder has the same effect, since the old baseline then applies to the new folder. The skeptic found that position-based ids would make this worse. The fix needs a design decision, probably a stored ref-to-root mapping checked at the start of each cycle. Reproduced by the review agents. Severity medium. Partly listed in `docs/todo.md`.
- `src/engine.rs:167` merges every known identifier from the store without applying the pattern. After a user narrows `pattern`, files that already have records keep syncing. Filter the merge with `rel.pattern.matches`. Reproduced by the review agents. Severity medium.
- `walk_identifiers` at `src/engine.rs:625` returns an error for any `read_dir` failure below the root, macOS TCC-protected folders included, while an unreadable file skips that file alone. The relation then shows `0 files` with the global state still green. Record the failure, continue below the root, and keep the hard error for the root alone. Reproduced by the review agents. Severity medium.
- `src/policy.rs:383` skips the whole identifier when any slot is `Unreadable`, and `read_status` marks every symlink or directory as unreadable. Edits between the healthy replicas stop as well. Skip the unreadable targets alone and mark them unresolved. Reproduced by the review agents. Severity low.
- `Policy::beats` at `src/policy.rs:494` returns the first covering `Compare` result even when the property ties. In a `size` rule followed by an `mtime` rule the second rule is therefore unreachable, contrary to `README.md:44`. Make `property_beats` return an `Option<bool>` and fall through on a tie. Confirmed by reading the code. Severity low.
- `decide` at `src/policy.rs:406` lets a peer that the same plan overwrites serve as a donor for another target. With `a -> b` plus `b -> c` and edits on both `a` and `b`, the result can leave `c` with content that neither side holds. `decide` needs dependency-ordered or fixpoint evaluation. Reproduced by the review agents. Severity low.
- Extra daemons run freely against the same `state_db`. The bind failure at `src/main.rs:217` is logged while the process continues, then its `recover()` wipes the live daemon's intent rows. Take a `File::try_lock` on `<state_db>.lock` before `Store::open`. Reproduced by the review agents. Severity low.
- `write_atomic` at `src/engine.rs:579` plus `preserve` at `src/archive.rs:32` call `fs::write` then `rename` with no fsync. After a power cut a zero-length destination reads as edited and, under a two-way edge, propagates. Call `sync_all` on the file, then on its directory. Confirmed by reading the code. Severity low.
- `src/engine.rs:337` records `donor_snap.hash` for bytes it read later. A donor edited between the stat and the copy therefore gets a wrong hash in the record. Hash the bytes actually written. Confirmed by reading the code. Severity low.
- The unavailable-root guard at `src/engine.rs:177` fires only when the folder stops being a directory. A mount point left as an empty directory reads as the user deleting everything, and `README.md:139` promises more than this. Reword the README now, with the deletion threshold from the todo as the real fix. Confirmed by reading the code. Severity low. Listed in `docs/todo.md`.
- File names of 241 to 255 bytes fail to sync. The staging and marker suffixes at `src/engine.rs:578` and `:595` push the temp name past NAME_MAX. Fall back to a hashed short name when the suffix would overflow. Reproduced by the review agents. Severity low.
- Gate state plus pause state live in memory alone. `Gates` and `paused` reset on every relaunch, and every config edit requires a relaunch. A gate closed as a safety step therefore reopens silently. Persist both in a `setting` table. Confirmed by reading the code. Severity low.
- `parse_duration` at `src/config.rs:215` accepts `interval: 0`, and the loop at `src/main.rs:139` then runs cycles back to back at full CPU. Reject zero. Reproduced by the review agents. Severity low.

## Dashboard and command line

- The listener is one blocking thread with no timeouts. At `src/main.rs:224` an idle connection blocks every later request, `/halt` included. A `%` followed by a multibyte character in any query string panics `url_decode` at `src/main.rs:468`, then the listener is gone for the life of the process while syncing continues. Set read and write timeouts, handle each connection on its own thread, and decode on bytes. Reproduced by the review agents. Severity low.
- `GET /`, the 10-second refresh, and every `/status.json` poll call `engine.report()`. That call re-walks every root, runs one SQLite point query per ref per identifier, and re-runs policy, all under the mutex the main loop holds for a whole cycle at `src/main.rs:130`. The verifier measured 0.28 s per poll at 6,000 files, 10 to 14 s during a cycle, and a queued `/pause` waiting 14 s. Publish a snapshot after each cycle and serve pages plus status from it. Reproduced with the release binary. Severity medium.
- `/halt` plus `/restart` answer `exited cleanly` at `src/main.rs:336`, then block on `engine.lock()` until the running cycle ends. The process therefore keeps writing after the page says it stopped. Both act on one click with no confirmation, while halt removes the page that could undo it. Set a stop flag checked between identifiers, answer `stopping after the current sync`, and add a `confirm()` on halt. Reproduced by the review agents. Severity low.
- The main loop at `src/main.rs:139` polls a flag in 500 ms slices with no way to request a cycle. Opening a gate, resuming, or fixing a problem waits up to a full interval, and `/pause` leaves `next_cycle` set, with the page showing a stale schedule. Replace the poll with an `mpsc` command channel plus `recv_timeout`, then add `POST /sync`. Confirmed by reading the code. Severity low.
- The `_ =>` arm at `src/main.rs:344` answers every unknown path or method with 200 plus the full page. `curl /pause` without `-X POST`, `GET /favicon.ico`, and `HEAD /` all trigger a full report while looking successful. Add an explicit `GET /` arm plus a 404 or 405 fallback. Reproduced with the release binary. Severity low.
- `src/main.rs:795` caps in-sync rows alone. A first sync or a gated relation therefore renders every pending row, 900 KB for 3,000 files, re-fetched every 10 s. The line counting the hidden in-sync rows has no link. Cap both kinds of row and add `?rel=&all=1`. Reproduced by the review agents. Severity low.
- The history page at `src/main.rs:396` renders every archived version with one `stat` per row and no paging or filter. Its verifier failed, then I confirmed it from the code. Severity low.
- `ref_short` at `src/main.rs:556` and the header loop at `:790` both name refs from the last two path components. Every `{project}/docs/style` column therefore reads `dest docs/style`, with the plan leaving the project unnamed. Name refs by the hole value or the shortest unique suffix, in one helper. Reproduced by the review agents. Severity medium.
- Failed relations get no state tag and count for nothing in the global state. The page therefore still says `everything in sync`. Compute one state per relation, then show the worst one globally. Reproduced with the release binary. Severity low.
- `recover()` at `src/main.rs:122` clears the intent rows before any page renders. The `Unfinished operations` panel at `src/main.rs:716` is therefore reachable through a live apply failure alone. Keep the cleared rows on `Runtime` for the process lifetime. Confirmed by reading the code. Severity low.
- The refresh script at `src/main.rs:873` assigns `document.body.innerHTML` every 10 s with an empty `catch`. It wipes selection plus focus, keeps polling in hidden tabs, and leaves a stale green page when the daemon dies. Skip hidden tabs and active selections, then show a `daemon unreachable` note on fetch failure. Reproduced by the review agents. Severity low.
- `unwrap_or_default()` at `src/main.rs:255` and `:349` turns a locked or broken database into `Nothing archived yet` plus `0 records`. Pass the `Result` to the view. Reproduced by the review agents. Severity low.
- `/status.json` misleads in four ways. Confirmed by reading the code. Severity low.
    - `errors` at `src/main.rs:98` is a lifetime count of re-emitted events that resets on restart alone.
    - The body has no gate field, with a gated daemon reporting `running`.
    - `last_sync_ms` is a duration next to `last_sync`, a timestamp.
    - The body also lacks per-relation state. A menubar companion built on this, as the todo plans, would show a permanent warning.
- At 375 px the header controls overflow off-screen, and `src/style.css` has no width media query although `docs/design-style.md:195` specifies one. In the light theme the plain `.warn` text at `src/style.css:101` is 1.96:1 against white, used for the folder-missing warning plus held plans. Reproduced in a browser. Severity low.
- The history page tells the user to download a version, then place it at the path shown, but that path is the downstream side. In a one-way relation the next cycle therefore reverts the restored file. State where to restore based on the relation's edges, or restore in place. Confirmed by reading the code. Severity low.
- The command line accepts exactly three flags, `--config <path>`, `--once`, and `--gate`. `--help`, `-h`, `--version`, and `--config=PATH` all produce `Error: unknown argument`. The usage line appears as an error alone. `src/main.rs:4` is the only place that documents `--gate`. `--once` exits 0 after relations errored. `POST /restart` at `src/main.rs:325` exits without validating the config, then a typo crash-loops the daemon under launchd with the dashboard gone. The daemon prints nothing at startup, on gate or pause changes, or at cycle end. Add `--help`, `--version`, `--config=PATH`, and `--check`, validate before restart, and log lifecycle events. Reproduced with the release binary. Severity low.
- `src/main.rs:610` loads Roboto from Google Fonts, the one external dependency of a loopback tool. Delete the link, since the CSS already falls back to Helvetica. Confirmed by reading the code. Severity low.

## Config, errors, logs, docs

The YAML config is the main human interface, where most of the friction is.

- The config loader accepts several mistakes without complaint. Reproduced by the review agents.
    - `propagate: { change: ... }` with no `create` loads, then every new file stays uncopied while the log says `resolved (0 ops)`, per `src/config.rs:302`.
    - Entry-level `propagate` blocks replace the whole default block, since defaults merge by top-level key at `src/config.rs:423`. One overridden sub-key therefore discards every inherited edge.
    - `pattern:` on a path-template relation matches the hole values instead of the path, per `src/engine.rs:160`. `*.md` therefore syncs nothing.
    - `ScanConfig` at `src/config.rs:202` lacks `deny_unknown_fields`, alone among the config structs.
    - Relative paths pass for `root:`, `scan:`, `state_db`, `log`, and `archive.dir`, then resolve against the launchd working directory.
    - Entries with two `rename` endpoints load, then sync nothing.
    - Placeholder typos such as `{nmae}` become literal file names.
- Error messages omit where the problem is. Reproduced by the review agents.
    - Endpoint errors name the label but omit the entry.
    - The two `untagged` enums at `src/config.rs:172` turn a misspelled `rooot` into a generic untagged-enum mismatch error with no field name.
    - Typos under `entries._` get reported against whichever entry merged first.
    - `interval` and `archive.cap` errors omit the key.
    - Blank `delete:` values, YAML null, fail with `invalid type: unit value`, although `config.example.yaml:31` says to leave the field blank.
    - `<<: *base` merge keys fail as `unknown field <<`, since the loader skips `apply_merge`.
- The pattern rules are undocumented and surprising. Reproduced with the release binary.
    - `*` stays within one path segment. The headline `pattern: "*.js"` therefore syncs top-level files alone.
    - `**` and `{a,b}` are undocumented.
    - The glob matcher is case-sensitive on a case-insensitive filesystem. `*.jpg` therefore skips `IMG.JPG` with no trace.
    - The config offers no exclude key. `.DS_Store` therefore gets synced, then conflicts forever.
    - `scan:` fans out into `.git` and `node_modules`.
    - The walk skips symlinked files silently, as does `scan` for symlinked directories.
- `render_line` at `src/main.rs:201` prints epoch seconds, `Resolved` carries an op count alone, and relation-level errors leave a blank identifier slot, as in `basic  error:`. A copy and a delete therefore log identically. Give `Resolved` the `describe(&ops)` text, then print local time. Reproduced with the release binary.
- Each conflict produces two or three differently ordered sentences with raw ref ids such as `dest?project=alpha`. The text omits what each side did, along with the fact that a `propagate.conflict` rule would settle it. Make the skip reason a typed value, report once per identifier, and render with the column names. Reproduced with the release binary.
- The docs disagree with the code in these places. Reproduced with the release binary where behavior is involved.
    - `README.md:50` calls `config.example.yaml` complete, yet top-level `gate`, `archive`, per-entry `gate`, the `path:` endpoint key, `--gate`, `POST /gate`, `GET /history`, and `GET /blob` are undocumented.
    - `README.md:138` describes marker scanning that the code lacks.
    - `docs/service-mac.md:79` gives a `launchctl bootout` form that always fails, then explains the resulting I/O error as a timing problem.
    - `docs/service-mac.md:99` calls deleting `live/` harmless, yet that folder holds the archive and the sync baseline.
    - The `rename` example at `README.md:71` maps a directory with no pattern. A second file in it therefore errors every cycle.
    - `docs/design-style.md` describes a general design system whose numbers disagree with `src/style.css` in six places, with seven tokens unused.
    - README vocabulary (`appeared`, `disappeared`) differs from the dashboard words (`new`, `deleted`, `absent`, `held`, `waiting`), and `ref`, `donor`, and `settled peer` are undefined.
    - `Cargo.toml` still says 1.0.0 across an incompatible config format change, with no migration note.

## Replicated code and drift

- `resolve()` at `src/engine.rs:205` plus `report_relation()` at `:436` each build slots, then compute settledness separately. Item 8 lists two drifts between them. One `inspect()` helper covers both.
- `Event` gets matched three times in `src/main.rs`, at `:176`, `:189`, and `:201`, with drifted wording. `Ev.kind` is a `&'static str` matched again at `:856`, and the Activity color comes from `text.contains("conflict")`. A file named `conflict-notes.txt` therefore renders as a warning. Give `Event` accessors `relation()` plus `kind()`, with a typed tone.
- Plans get formatted twice. `describe` at `src/engine.rs:525` yields `copy source -> dest?project=a` for logs, Activity, and the intent journal. `describe_plan` at `src/main.rs:571` yields the labeled form with an arrow character for the table. Move the labeled formatter into the engine and use it everywhere.
- `config.rs` converts endpoints twice, once at `:229` and once at `:251`, with different validation. Settings get parsed in two places as well. `config::load` returns raw strings, then `main.rs:48` finishes the parsing and repeats the 100 MB default.
- `parse_size` at `src/config.rs:101` and `parse_duration` at `:215` are two hand-rolled number-plus-unit parsers that already disagree on decimals, case, whitespace, and units, while `dashboard.refresh` bypasses both.
- `Graph::parse` at `src/policy.rs:93` and `parse_rules` at `:193` share the same left-operator-right skeleton with drifted error messages. A missing operator in a conflict rule reports `expected a label` instead of the operator error the change graph gives.
- `tests/cases.rs` hand-escapes the same two-label YAML 17 times, four of them byte-identical, and repeats the `match config::load` error block three times. A `pair()` helper plus an `engine_with_events()` helper would remove most of it.
- `main.rs` repeats the 303 response three times, the page skeleton three times, the pluralizer four times, and the last-two-components snippet twice. `tilde_str` at `:496` is also wrong, since it rewrites `/Users/home2` as `~2`.
- The storage formats are inconsistent. The mtime is stored as milliseconds, hashes are BLOB in `record` but hex text in `archive`, and the epoch conversion is written inline seven times.
- Inline copies bypass existing helpers, among them the `Status::Present` projection, `identifier_to_string`, `Gates::is_gated`, and the label expansion chain.

## Modularity, DRY, dependency inversion

- `main.rs` holds 882 untested lines of HTTP parsing, routing, state mutation, `process::exit`, JSON, and HTML, in one 147-line socket closure plus a 245-line, 9-argument `render_page`. Nothing in `tests/` can reach any of it. The cheapest first step is a `#[cfg(test)]` module for `url_decode`, `tilde_str`, `fmt_dur`, `fmt_size`, `describe_plan`, and `row_attention`, then a split of request handling from rendering.
- `apply()` is a 110-line function with direct `fs` calls, no abstraction over the filesystem, and no cleanup on the error path, as item 10 describes. A prepare and execute split, with markers held in a guard, fixes both.
- Strings stand in for types in many places. Skip reasons, archive reasons (`overwritten` and `deleted` compared by literal at `src/main.rs:406`), event kinds, ref ids, and the intents tuple `(String, Vec<String>, String)` all get parsed or matched later. `Identifier` is a bare `Vec<String>` alias.
- `src/label.rs` models one endpoint with three types, `Mapping` at `:13`, `Shape` at `:137`, and `MappingKind` at `:156`, bridged by `From`. `Fixed` plus `Template` is representable there and excluded by a runtime bail alone. One `enum Endpoint { Dir, Scan, Template }` would do.
- The archive is attached from outside. `Engine.archive` is an `Option` set by field mutation at `src/main.rs:111`, and 16 of 17 tests run with it off. The tests therefore cover preserve-before-write once. Make it a constructor argument.
- `config::load` returns an untyped tuple and leaves interval parsing, the `state_db` requirement, and archive resolution to `main`, where they fail after load succeeded, with different wording.
- The engine composes user-facing prose in `LabelView.notes` at `src/engine.rs:405`, while `main.rs` reaches through the public `Engine.store` to run queries and turns every error into an empty value.
- `Ref::id()` allocates on every call and is called in nested loops inside `apply()`. Compute it once at construction.

## Performance

At present scale the first item is the one that matters. The rest matter at the design scale or with large files.

- `walk_identifiers` from item 3 costs O(all files under the root) per cycle and per dashboard request, independent of the pattern. On `~/.hermes` that is about 0.3 s per walk.
- The mtime fast path fails on every file. `store.rs:93` saves milliseconds, while `src/engine.rs:563` compares against the nanosecond filesystem mtime. Every cycle and every dashboard render therefore re-reads and re-hashes every file. A 300 MB file costs 0.3 s plus 308 MB resident per no-op cycle. For the current files the cost is close to zero. Store nanoseconds, or truncate the live mtime to milliseconds before comparing. Reproduced with the release binary. Severity medium at the design scale.
- Files are read whole into memory up to three times, for the hash, the donor buffer, and the archive, then re-hashed although the hash is known. Peak RSS is twice the largest file, and `/blob` loads the whole revision too. Stream the hash with `Hasher::update_reader`, then `io::copy` the blob.
- The first sync pays six autocommits per file. `src/engine.rs:360` commits each record separately against a rollback-journal, `synchronous=FULL` database. 3,000 small files took 4.2 s, mostly system time. `PRAGMA journal_mode=WAL` plus one transaction per identifier recovers most of it while keeping the intent durable.
- `resolve` and `report_relation` issue one parsed point query per ref per identifier at `src/engine.rs:212`, `:228`, `:440`, and `:448`, re-parsed each time. Load the relation's records plus its unresolved set in two bulk queries.
- The evict loop scans the whole archive table several times per preserved file, with no indexes. 1,000 overwrites took 107 s at 100k rows versus 3 s with indexes.
- The overlap check compares every pair of refs every cycle, and `apply()` does linear `id()` scans per op. Scan fan-out is therefore quadratic, with a no-op cycle at 0.77 s for 2,000 refs.
- The repeated events from item 9 also cost disk writes, 340 KB per cycle to each of three sinks with 5,000 gated files.
- The launchd plist in `docs/service-mac.md` runs the walker at default priority. `ProcessType Background` and `LowPriorityIO` belong there.

## Low-hanging fruit

- Add `--help`, `--version`, `--config=PATH`, and `--check`, where `--check` loads the config, expands refs, runs the overlap check, and exits.
- Reorder the `LabelSpec` variants, as item 2 describes. One line.
- Filter `known_identifiers` by pattern. One line.
- Set `PRAGMA journal_mode=WAL`. One line.
- Add `deny_unknown_fields` to `ScanConfig`. One line.
- Delete the Google Fonts link. One line.
- Format errors with `{e:#}` at the four emit sites to keep their cause.
- Reject an `interval` of zero, accept `h`, and accept null for propagate fields.
- Use `entries.shift_remove("_")` at `src/config.rs:436`, since `remove` hoists the last entry to the front. Switch `labels` to an order-preserving map, since the columns currently come out alphabetical.
- Add a `confirm()` to the halt button.
- Fix `tilde_str` with `strip_prefix`.
- Drop the dead `Runtime.interval` field, the one compiler warning, along with `Instance.identifier`, `ArchiveEntry.ref_id`, and the `?` on `tokenize`, a function that always succeeds. Replace the `filetime` crate with `File::set_modified`. Note that `serde_yaml` is archived upstream.
- Add covering indexes on `archive(hash, size)`, `archive(rel, size)`, and `archive(rel, at, id)`.
- Add a schema version row to the state database before the mtime fix or any todo item needs a migration.
- Add `mkdir -p ~/scripts/replica/live/logs` to the install steps, since launchd skips creating log directories.
- Add unit tests for the pure functions that have none, among them `classify`, the `Unreadable` skip, rule ties, parser error wording, and `url_decode`.
- Fix the example config with `~/Documents` for the nginx destination plus `pattern: default.conf` on it, to make a new user's first run work.

## Coverage and caveats

- In round 1 all 13 lenses ran, 8 cross-cutting plus 5 per file cluster, with every finding verified. In round 2, 7 lenses reran with the round 1 list marked as already found. The dashboard lens and the user-journey lens failed to complete in round 2. The user journeys were therefore covered in round 1 alone.
- The uncapped history page and the missing SIGTERM handler lost their verifier to a transient failure. Both appear above as confirmed from the code by me instead of by an agent.
- Agents read the `config.yaml` to calibrate severity. The live config, the live database, and port 18516 stayed out of every agent run.
- Severities are the skeptic-adjusted ones. Where the verifier and the skeptic disagreed on a fix, the report says a decision is needed instead of picking one.
