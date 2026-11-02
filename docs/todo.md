# To do

Pending design decisions and operational mechanisms for the sync daemon.

## Mechanisms

- Dispatch filesystem notifications (FSEvents on macOS, inotify on Linux) into the poll loop to evaluate changes before scheduled intervals:
    - The daemon scans directories directly to determine authoritative state.
    - Incoming notification events trigger evaluation passes early without relying on event delivery guarantees.
- Support regex parameter constraints in `path:` templates, such as `{date:\d+}` for dated log files.
- Conflict backup directory and three-way merge resolution:
    - Write conflicting variants to `dir/<ident>.<label>.bak` before resolving divergences.
    - Specify retention limits and naming schemes for backup files.
    - Store baseline file contents per relation alongside recorded hashes to supply merge bases during three-way merges.
- Prune database records for identifiers missing across extended sync windows, expanding cleanup beyond records with all replicas absent.
- Prune record and unresolved rows for refs removed from the configuration. A stale unresolved row keeps its identifier under full evaluation every cycle.
- Trigger full re-resolution across managed records upon modifying relation policies. Evaluation routines currently skip matching file hashes, bypassing updated policy rules on established records.

## Safety

- Abort batch deletions when the proportion of removed files exceeds a configured limit:
    - The engine halts propagation when an entire root directory disappears, while deleting most files within an intact root still propagates removals along delete edges.
    - Define a deletion ratio threshold with per-relation overrides.
- Distinguish initial replica seeding from ongoing discovery along create edges, defining explicit actions for files encountered during initial scans: ignore, seed, or replicate.
- Include extended diagnostic metadata in conflict marker files, passing the log path, database path, donor hash, and applied policy through the engine apply pipeline.
- Health indicators and failure alerts:
    - Write a `DANGER.README` alert file beside the state database upon detecting state inconsistency.
    - Evaluate optional per-relation heartbeat files for monitoring sync activity, weighing operational visibility against the noise of persistent marker files.

## Configuration

- Replace the endpoint notation: `scan` with `template`, `hole`, and `dir`, plus `root` with `rename`. Find a form as direct as the `propagate` statements.

## Interface

- Menubar companion:
    - Poll `/status.json` and post commands to `/pause`, `/resume`, and `/restart` via a SwiftBar plugin or a dedicated Swift application, with shortcuts to reveal configuration files and launch dashboards.
    - Run the status item in an external process, avoiding main-thread GUI event loop constraints inside the sync daemon.
- Windows port:
    - Normalize path separators to forward slashes when converting paths to strings during template matching.
    - Manage daemon execution through Task Scheduler or the `windows-service` crate, matching the architecture in `docs/service-mac.md`.
    - Isolate system tray integration into a companion process to avoid coupling GUI event loops with daemon execution.
