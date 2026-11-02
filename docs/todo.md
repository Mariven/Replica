# To do

Open work on the second design. The design itself is in [design.md](design.md).

## Decisions left open

- Whether `changed` actions should also fire for plans a gate holds, so a notification can announce work waiting for the gate.
- Whether to add a delete-ratio guard: a cycle planning to delete more than a configured fraction of a relation's keys holds the relation until an operator opens the gate. The missing-root guard covers an unmounted volume; the ratio guard would cover a mounted volume emptied by mistake.
- Whether a proposer whose proposal was left unresolved at some target should keep its basis frozen until the conflict clears. At present its basis advances, and the standing conflict is kept alive by the disagreeing settled peers, which matches the first design.

## Mechanisms

- Enumerate a fan-out variable from an explicit list of values as well as from the directory, so a listed folder that is missing shows up as a replica in the folder-missing state instead of vanishing.
- Read presence from the directory listing rather than from a stat per key, which makes a rename differing only in case on a case-insensitive volume read as a delete plus a create instead of two present files on one inode.
- Prune records and unresolved marks of replica identities absent from both the configuration and the enumeration for a long time.
- Store the previous content hash in a tombstone, so a three-way merge command has a base to work from.
- A merge resolver: `a merge b` running a configured three-way merge command over the two contents and the tombstone's or the basis's content.
- Give a fan-out variable a constraint by default for `.git`, `node_modules`, and other folders nobody wants as a replica, or at least document the `[a-z]*` idiom from the example.

## Interface

- Richer `replica status`: the awaiting and held keys per relation, from the report rather than from the counters.
- A menubar companion polling `/status.json` and posting to the control endpoints, in its own process.
- A Windows port: forward slashes in pattern text, Task Scheduler or the `windows-service` crate for the service, a companion process for the tray.
