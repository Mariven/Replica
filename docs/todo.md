# To do

## Open questions

- Should `changed` actions also run for plans held by a gate, so that a notification can say work is waiting?
- A guard against mass deletion: when a cycle plans to delete more than some fraction of a relation's files, hold the relation until someone opens the gate. The missing-root check catches an unmounted drive, but a mounted drive emptied by mistake gets through.
- When a proposal is held at some replica, the proposing replica's basis still advances. Should it stay as it was until the conflict is resolved? The conflict stays open either way, because the settled peers still disagree.

## Mechanisms

- Let a fan-out variable take its values from a list as well as from the directory listing, so that a listed folder that does not exist yet shows up as a missing replica.
- Read presence from the directory listing instead of a stat per key. On a case-insensitive volume, a rename that only changes case then shows up as a delete and a create, instead of as two files sharing an inode.
- Prune the records of a replica id that has been gone from both the configuration and the disk for a long time.
- Keep the previous hash in a tombstone, so that a merge command has a base to work from.
- A merge resolver: `a merge b` runs a three-way merge command on the two contents, with the basis or the tombstone as the base.
- Skip `.git`, `node_modules`, and similar folders in fan-out by default, or document the `[a-z]*` constraint the example uses for this.

## Interface

- A richer `replica status` that lists the waiting and held keys of each relation.
- A menu bar app that polls `/status.json` and posts to the control routes.
- A Windows port: forward slashes in patterns, Task Scheduler or the `windows-service` crate in place of launchd, and a tray companion.
