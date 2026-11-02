# To do

Open work on the second design. The design itself is in [design.md](design.md).

## Decisions left open

- Whether `changed` actions should also fire for plans held by a gate, so that a notification can announce work waiting for the gate.
- Whether to add a delete-ratio guard. When a cycle plans to delete more than a configured fraction of a relation's keys, the guard holds the relation until an operator opens the gate. The missing-root guard already covers an unmounted volume; the ratio guard is for a mounted volume emptied by mistake.
- Whether a proposer should keep its basis frozen while its proposal stays unresolved at some target. At present the basis advances anyway. The conflict stays open because the settled peers still disagree, as in the first design.

## Mechanisms

- Enumerate a fan-out variable from an explicit list of values as well as from the directory listing. A listed folder then shows up as a replica in the folder-missing state when it is absent, instead of vanishing.
- Read presence from the directory listing rather than from a stat per key. On a case-insensitive volume, a rename differing only in case then reads as a delete plus a create, rather than as two present files sharing an inode.
- Prune the records and unresolved marks of a replica identity once it has been absent from both the configuration and the enumeration for a long time.
- Store the previous content hash in a tombstone, so that a three-way merge command has a base to work from.
- A merge resolver. `a merge b` runs a configured three-way merge command over the two contents, with the tombstone's or the basis's content as the base.
- Give a fan-out variable a default constraint so that `.git`, `node_modules`, and similar folders are skipped, or at least document the `[a-z]*` idiom from the example.

## Interface

- A richer `replica status`, listing the awaiting and held keys per relation from the report rather than from the counters.
- A menubar companion in its own process, polling `/status.json` and posting to the control endpoints.
- A Windows port, with forward slashes in pattern text, Task Scheduler or the `windows-service` crate for the service, and a companion process for the tray.
