# To do

Ideas carried over from the README, plus items from the earlier notes in `docs/old/` that still need a design decision.

## Mechanisms

- Notify-based invalidation (FSEvents / inotify) feeding the same poll loop, for latency shorter than the interval. The earlier notes' rule: the scan is the source of truth, watch events are hints. Correctness must not depend on event delivery.
- Per-hole value constraints in `path:` templates (`{date:\d+}`-style, for dated logs).
- A conflict backup directory (write each conflicting variant to `dir/<ident>.<label>.bak` before resolving) and a three-way merge command. Open questions: backup naming and retention, and where the merge base comes from. The earlier notes suggest optionally storing last-synced content per relation, not just its hash.
- A sweep policy for record rows whose identifiers have been gone for a long time, beyond the current all-missing collection.
- Force a full re-resolution when a relation's policy changes. The per-instance early exit assumes the policy that committed the record is still in force.

## Safety (from the earlier notes)

- A big-delete threshold: the missing-root guard covers a vanished root, but a root that survives while most of its recorded files disappear still propagates under `on_delete: propagate`. A fraction-based abort (Unison's confirmbigdel) needs a threshold choice and a per-relation override.
- Bootstrap `seed` as a first-observation policy distinct from `on_new`. The earlier transition table separates them: ignore | seed | replicate. Current restrict / replicate covers the practical cases.
- Marker files carry a small field set. The earlier full format also listed the log path, database path, donor hash, and policy. Those would have to be passed into the engine's apply path.
- A DANGER.README dropped beside the state database when it is inconsistent, and an optional per-relation heartbeat file. The earlier notes already flag the heartbeat as risky: always-present markers get learned as noise.

## Interface

- Menubar sidecar: a SwiftBar plugin or a small Swift app that polls `/status.json` and posts `/pause`, `/resume`, `/restart`, with reveal-config and open-dashboard items. Kept out of the daemon: an in-process NSStatusItem needs the main thread's event loop, which would invert the process structure for a convenience.
- Windows port: the engine is portable except path-separator handling in template matching (normalize to forward slashes when turning paths into strings). The service story is Task Scheduler or the windows-service crate. A tray icon has the same event-loop question as the menubar. See docs/service-mac.md for what a service-windows.md would parallel.
