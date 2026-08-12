# To do

Carried over from the README, plus concrete suggestions mined from the prespec
(docs/old/) that need design judgement before implementing.

## Mechanisms

- notify-based invalidation (FSEvents/inotify) feeding the same poll loop, for
  sub-interval latency. The prespec's rule: scan is truth, watch events are hints;
  correctness must never depend on event delivery.
- per-hole value constraints in `path:` templates (`{date:\d+}`-style, for the
  dated-logs case).
- conflict backup dir (write each conflicting variant to `dir/<ident>.<label>.bak`
  before resolving) and a three-way merge command effect. Judgement needed: backup
  naming and retention, and where the merge base comes from (the prespec suggests
  optionally storing last-synced content per relation, not just its hash).
- record-row sweep policy for long-gone identifiers beyond the current all-missing
  collection.
- force a full re-resolution when a relation's policy changes (the per-instance early
  exit assumes the policy that committed the record is still in force).

## Safety (prespec-derived)

- big-delete threshold: the missing-root guard covers a vanished root, but a root that
  survives while most of its recorded files disappear still propagates under
  `on_delete: propagate`. A fraction-based abort (Unison's confirmbigdel) needs a
  threshold choice and a per-relation override.
- bootstrap `seed` as a first-observation policy distinct from `on_new` (the prespec's
  transition table separates them: ignore | seed | replicate). Current restrict /
  replicate covers the practical cases.
- marker files carry a minimal field set; the prespec's full format also names the log
  path, db path, donor hash and policy. Needs plumbing those into the engine's apply.
- a DANGER.README dropped beside the state db when it is inconsistent, and an optional
  per-relation heartbeat file. The prespec itself flags the heartbeat as risky
  (always-present markers get learned as noise).

## Interface

- menubar sidecar: a SwiftBar plugin or ~100-line Swift app polling /status.json and
  posting /pause /resume /restart, with reveal-config and open-dashboard items. Kept out
  of the daemon: an in-process NSStatusItem needs to own the main thread's event loop,
  which inverts the process structure for a nicety.
- Windows port: the engine is portable except path-separator handling in template
  matching (normalize to forward slashes when stringifying); service story is Task
  Scheduler or the windows-service crate; tray icon has the same event-loop question as
  the menubar. See docs/service-mac.md for what a service-windows.md would parallel.
