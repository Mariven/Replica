# Replica

Replica is a daemon that keeps copies of files in sync across several folders on the same machine. You describe the copies and the rules in a YAML file. The daemon scans the folders, works out what each copy should contain, and writes, deletes, archives, and reports accordingly.

This document is the reference manual for the design. It defines the terms, states the rules the daemon follows, describes the data it stores, and documents the configuration file and the interfaces. [README.md](../README.md) is the short introduction. [todo.md](todo.md) lists open work.

## Terms

A few words are used throughout and are defined in full in later sections:

- A relation is a named group of folders or files that the daemon keeps in sync. Relations are declared under `relations` in the configuration file.
- A site is a named role inside a relation, such as `source` or `dest`. A replica is a concrete folder or file playing that role. A site with a fan-out variable can have several replicas.
- A key identifies one file across the replicas of a relation.
- The basis is the record the database holds for a key at a replica from the last time the daemon committed that key.
- The engine is the part of the daemon that runs cycles. The executor is the part that applies a plan to disk.

## The cycle

The daemon works in cycles. At the start of a cycle it expands the sites of all relations into replicas and checks that no two replicas overlap on disk. Then it handles the relations one at a time. For each relation it:

1. Records the pattern of each replica in the database. If a replica's pattern has changed since the last run, the daemon reports a relocation.
2. Checks that each replica's root folder exists. If a root is missing but the database holds snapshots for that replica, the daemon reports the relation as unavailable and skips the rest of these steps.
3. Walks each replica to discover the keys of the relation.
4. Observes each key at each replica and compares what it finds with the basis. If nothing changed at any replica, the daemon skips the key.
5. Decides, for each changed key, what each replica should hold. The flow and the resolver of the relation control this decision.
6. Applies the plan: stage, archive, journal, write, commit.
7. Reports what happened through events, the relation's log, and the dashboard. Then it runs the shell commands configured under `on` (see Actions and transforms).

## Gates

Each relation has a gate, and there is a global gate as well. A gate is a flag. While the global gate or a relation's own gate is closed, the daemon still runs steps 1 through 5 for that relation and shows the resulting plan, but it writes nothing to the files and nothing to the database. You open and close gates from the dashboard or the command line.


## Patterns

A pattern is a path with variables in it. Variables are written in braces. Each site of a relation is written as one or more patterns.

    ~/Desktop/mylibrary/{path...}
    ~/scripts/skills/{name}.md
    ~/.claude/skills/{name}/SKILL.md
    ~/Desktop/Projects/{project}/docs/style/main_style.md
    /etc/nginx/conf.d/default.conf

### Syntax

- A pattern must be an absolute path, or start with `~/`. The loader replaces `~` with the home directory.
- `{name}` matches a single path segment, or part of a segment. A segment may combine literal text with a variable, as in `{name}.md`, but a segment may hold only one variable. A variable always matches at least one character, so `{name}.md` does not match a file named `.md`.
- `{name...}` matches one or more segments, in other words a relative path. A pattern may hold only one such variable, and it must come after the other variables of the pattern.
- A variable name is made of ASCII letters, digits, and underscores, and starts with a letter or an underscore. A name may appear once in a pattern.
- A pattern that ends in `/` means the whole tree below that folder. The loader rewrites it by appending `{path...}`. The bare `/` is an error, and so is a pattern with an empty segment, such as `/a//b`.
- A pattern with no variables names a single file.

### Constraints

A constraint limits the values a variable may take. Write it after the variable name, separated by a colon:

- A glob, as in `{file: *.js}` or `{path...: docs/**/*.md}`. `*` and `?` match within one segment. `**` matches across segments.
- A regular expression between tildes, as in `{date: ~\d{4}-\d\d-\d\d~}`. The loader anchors it at both ends.
- Either of the above with a leading `!`, meaning the opposite. `{project: !node_modules}` matches any single segment other than `node_modules`.

A relation may also hold constraints in its `where` block, keyed by variable name. Each entry is one constraint or a list of constraints. The loader attaches them to each pattern that uses that variable. When a variable has constraints in both places, a value must satisfy all of them.

### Matching and substitution

Matching takes a path and produces a binding, that is, a value for each variable. The daemon reads each variable's value out of the path and checks the constraints. Matching is case-sensitive.

Substitution takes a binding and produces a path. The daemon writes the values into the pattern. Substitution fails if the binding lacks one of the pattern's variables, if a value is empty, if the value of a single-segment variable contains `/`, or if a constraint rejects a value.

The two operations are inverses. Matching a path and substituting the resulting binding gives the path back. Substituting a binding and matching the resulting path gives the binding back. Those rules are what guarantee that matching and substitution reverse each other, and the loader enforces them on each pattern.

### Derived properties

From each pattern the daemon derives four things:

- The root. This is the folder named by the leading segments that hold no variable. For `/a/pre{x}/f` the root is `/a`, since the segment `pre{x}` contains a variable. A pattern with no variables has its parent folder as its root. Walks start at the root.
- The depth. This is how many segments a matching file has below the root, counting the file's own segment. Without a `{name...}` variable the depth is fixed. With a `{name...}` variable the depth is unbounded, with one exception: a glob constraint on that variable that contains no `**` and no bracket class such as `[abc]` spans a fixed number of segments, and the depth becomes fixed again. If several such globs constrain the same variable, the daemon uses the one that spans the fewest segments. A negated glob or a regular expression leaves the depth unbounded.
- The footprint. This is the root, or the file itself when the pattern has no variables. The overlap check compares footprints. Two footprints overlap when they are equal or when one lies inside the other.
- The tail. This is the part of a matched path below the root. The ignore globs from the configuration (see `ignore` under Top-level fields) are matched against the tail.


## Sites, keys, and replicas

A relation declares its sites under `sites`. A site is a named role in the relation, such as `source` or `dest`. Flow statements refer to sites by these names. A relation must have at least two sites.

### Locations

A site is written in one of three forms:

- As a single pattern. The site then has one location, with no label.
- As a list of patterns. Each item is one location. Its label is the item text as written in the file, before `~` expansion.
- As a map from labels to patterns. Each entry is one location, labeled with its key.

An empty list or an empty map is an error. Two patterns under the same site that expand to the same text are an error as well, for example `~/x/` and `/Users/me/x/{path...}`.

### Identity and fan-out variables

The loader sorts the variables of a relation into two kinds.

An identity variable appears in the pattern of each location of each site. The values of the identity variables form the key of a file. Two files at different sites belong together when their keys are equal. If a relation has no identity variables, it has a single key, the empty one. That is how a relation made of two plain paths, such as `/etc/nginx/conf.d/default.conf` and `~/backup/nginx.conf`, keeps two files with different names in sync as a pair.

A fan-out variable is one you list under `fanout`. It may appear at some sites and not at others. At each location that uses it, the daemon creates a replica for each matching entry on disk.

The loader reports an error in these cases:

- A variable appears at some sites but not all, and it is not listed under `fanout`. The message suggests both repairs: add the variable to the other sites, or list it under `fanout`. If the variable is `path`, the message also suggests adding a trailing slash to the sites without it.
- A `fanout` entry names a variable that no site uses.
- A `fanout` entry names a variable that all sites use. Such a variable is part of the file identity.
- A site has several locations, and one of them lacks an identity variable that the others have.
- A fan-out variable is a `{name...}` variable, or comes after an identity variable in its pattern. Fan-out variables must match a single segment and come first, so that each replica has a fixed root.

### Replicas

A replica is a concrete location: a site, one of its locations, and a value for each fan-out variable in that location's pattern. The daemon expands the sites into replicas at the start of each cycle.

A location without fan-out variables is a single replica. For a location with fan-out variables, the daemon walks down from the root. When it reaches the segment of a fan-out variable, it lists the folder and binds the variable to each entry whose name fits the segment and its constraints. If more segments follow, the entry must be a folder. Symbolic links and other special entries are skipped. The daemon continues below each binding in the same way. Each complete binding becomes a replica.

If a folder is missing on the way to a fan-out variable, that location simply produces no replicas. If a folder exists but cannot be listed, the relation fails for this cycle and the daemon reports the error.

The daemon pays no attention to fan-out values when it pairs files across sites. A file under `source[alpha]` and a file under `dest[beta]` belong together when their keys are equal, whatever the fan-out values are.

### Replica identity

Each replica has an identity string: the site name, then the label in brackets if there is one, then the fan-out values in brackets if there are any. Examples: `dest`, `dest[alpha]`, `dest[~/scripts/actors/.cursor/rules]`, `dest[project=alpha]`. The database stores all records for a replica under this identity. Adding or removing a sibling replica leaves the other replicas' records untouched.

On the dashboard, in logs, and in hold reasons, the daemon shows a replica by its display name. When a site has a single replica, the display name is the site name alone. Otherwise it is the site name followed by the label or the fan-out values in parentheses, such as `dest (alpha)`.

The database also stores the pattern behind each identity. If a replica shows up under a known identity but with a different pattern, the daemon reports a relocation and deletes the records stored under that identity. The replica then starts over with no records, and its files count as new. On that cycle the daemon evaluates each key of the relation, including the keys a normal cycle would skip (see Skipping settled keys). If a gate is closed, the database keeps the old pattern, the daemon reports the relocation on each cycle, and the records are deleted on the first cycle after the gate opens.

A location in list form is labeled by its own pattern text. Editing that pattern creates a new identity, and the daemon sees a new replica instead of a relocation. The records under the old identity stay in the database until you remove them with `replica forget`.

A replica whose identity is new to the database has no records. On the cycle where it first appears, the daemon evaluates each key of the relation. If a gate is closed on that cycle, the daemon does not record the identity, so the replica still counts as new on the next cycle.

### Discovering keys

For each replica, the daemon walks from the root down to the pattern's depth and matches each regular file against the pattern. A file becomes a key if it matches, its constraints hold, and no ignore glob matches its tail.

During the walk the daemon skips folders beyond the depth, entries other than regular files and folders, and its own `.replica.tmp` marker files and `.replica-write` temporary files (both described under Applying a plan). Ignore globs apply to files only. A glob that matches a folder's tail does not stop the daemon from descending into that folder. If a subfolder cannot be read, the daemon reports a warning and skips it. If the root itself exists but cannot be read, the relation fails for this cycle.

A missing root is handled in one of two ways. If the database holds snapshots for that replica, the daemon reports the relation as unavailable and leaves all replicas of the relation alone until the root is back. If the database holds no records for that replica, the daemon treats the root as a folder that has not been created yet, and the walk yields no keys.

The database may hold records for keys the walk did not find. This happens when a file was deleted at all replicas since the last cycle. The daemon adds these keys to the keys found by the walk, so that it observes the deletion at each replica and clears the records.

Finally, the daemon checks each key against each replica. It substitutes the key into the replica's pattern, then checks that the resulting path still matches the pattern and is not excluded by an ignore glob. If the check fails at any replica, the daemon drops the key and deletes the key's records. If you later loosen the constraints so that the file matches again, the daemon treats the file as if it had just appeared, with no record behind it.


## Observing a key

### Basis

The basis is the record that the last commit stored in the database for a key at a replica. Commits are described under Applying a plan. The basis is one of:

- A snapshot: the modification time, the size, and the BLAKE3 hash of the file's content.
- A tombstone, meaning the file was deleted at this replica. The daemon writes a tombstone when it finds a recorded file missing, or when it deletes the file itself.
- Nothing, when the database has no row for this key at this replica.

### Observation

For each key, the daemon looks at each replica and records one of:

- Present, with a snapshot. If the basis is a snapshot with the same modification time (to the millisecond) and the same size as the file on disk, the daemon reuses the recorded hash. Otherwise it hashes the file. The record is rewritten only when the executor commits the key. Take a file whose modification time changed but whose content did not. Its hash still matches the record, so its transition is unchanged, and the daemon does not commit an unchanged key. The record keeps the old modification time, so the daemon hashes this file again on each cycle until some other change at the key leads to a commit.
- Missing.
- Unreadable, with the reason. Anything other than a regular file at the path counts as unreadable, and so does a failure to stat or read the path.

### Transition

Comparing the basis with the observation gives the transition at that replica:

- no record, missing: unchanged
- no record, present: appeared
- tombstone, missing: unchanged
- tombstone, present: recreated
- snapshot, missing: disappeared
- snapshot, present, same hash: unchanged
- snapshot, present, different hash: edited
- any basis, unreadable: unreadable


## Flow and resolver

### Flow statements

The `flow` field holds one or more statements separated by semicolons. A statement has a site, or a braced set of sites, on each side of an arrow. After the right-hand side, a bracketed list names the kinds of change that pass along the arrow. The loader creates one edge for each pair of a left-hand site and a right-hand site.

    source <-> dest [edit create]
    source -> dest [edit create delete]
    {a, b} -> c [edit]
    backup <- source [all]

The kinds are `edit`, `create`, and `delete`. Separate them with spaces or commas. Empty brackets are an error. The loader also accepts `edits`, `change`, `changes`, `creates`, `creation`, `deletes`, and `deletion`. `all` means all three kinds. An arrow with no brackets carries `edit` and `create`. If two statements connect the same pair of sites in the same direction, the loader merges them into one edge that carries both sets of kinds.

Each kind forms its own graph over the sites, and the daemon uses the transitive closure of that graph. Under `a -> b [edit]; b -> c [edit]`, an edit at `a` reaches `c`. Two replicas of the same site exchange changes only if the closure leads from the site back to itself, as it does under `a <-> b` or `a -> b; b -> a`. Under a one-way flow, the replicas of the upstream site are independent of each other.

A flow with no edges is an error at load time. So is a flow statement or resolver rule that names a site the relation does not declare.

### Resolver rules

The `resolve` field holds rules separated by semicolons. When the changes arriving at a replica disagree, the daemon tries the rules in order (see the Resolve rule under Deciding a key):

- `a > b` prefers content from site `a` over content from site `b`. `a < b` prefers `b`. Either side may be a braced set of sites. A rule with the same site on both sides, such as `a > a` or `{a, b} > {a, c}`, is an error.
- `a mtime b` prefers the newer file. `a size b` prefers the larger file. `a name b` prefers the file whose name sorts first in byte order, so uppercase letters sort before lowercase ones. `newer` and `larger` are synonyms for `mtime` and `size`. On a tie, the daemon moves to the next rule.
- A bare property, such as `size`, compares any pair of sites. A property rule that names the same site twice, such as `a mtime a`, compares only the replicas of that site. Use either form to decide between replicas of one site.

Under a property rule, a present file beats an absent one. The `resolve` field is optional. If it is missing, or if no rule decides a pair, the daemon holds the replica with a conflict reason and waits for a person to resolve it.


## Deciding a key

The daemon decides each replica of a key from the bases and observations of all replicas of that key, together with the relation's flow and resolver. The decision made for one replica plays no part in the decision for another, so the order in which the daemon visits the replicas does not matter. The rules below carry the names used in `src/decide.rs`.

1. Voice. Each replica makes at most one proposal, based on its transition. An edited or recreated replica proposes its content. A disappeared replica proposes absence. An appeared replica proposes its content only if no replica of the key has any record, neither snapshot nor tombstone. Otherwise the newcomer proposes nothing and conforms to its peers under the Conform rule below. Unchanged and unreadable replicas propose nothing.

2. Admit. A proposal travels from a source replica to a target replica when the transitive closure of the flow, for a suitable kind, has an edge from the source site to the target site. The suitable kind depends on the target's observation:
    - content to a present target: `edit`
    - content to a missing target: `create`
    - absence to a present target: `delete`
    - content to an unreadable target: `edit` or `create`
    - absence to an unreadable target: `delete`
    - absence to a missing target: nothing travels

3. Hold. An unreadable replica is held with the reason `unreadable: <error>`. A replica whose proposal travels to an unreadable peer is held too, with the reason `waiting on <replica>: unreadable`, since that peer might hold a competing change. A held replica keeps its basis, so its change stays pending until the peer becomes readable.

4. Contest. The candidates for a target are the proposals that travel to it from readable peers. Suppose at least one candidate has arrived. If the flow also lets the target's own proposal reach at least one of the peers that sent a candidate, the daemon adds the target's proposal to the candidates. On a two-way edge, each end's proposal is a candidate at the other end. If the two contents differ, the resolver rules pick one. If the two contents are identical, both ends already agree, and no operation results. On a one-way edge, the downstream proposal has no edge back, so the upstream content overwrites the downstream edit.

5. Stand. If no candidate arrives and the target's own proposal travels to some peer, the target keeps its state. Its proposal is handled at the peers that receive it.

6. Conform. If no candidate arrives and the target has no proposal that travels anywhere, the daemon looks at the target's settled peers, meaning the replicas that are present on disk and unchanged against a recorded snapshot. Each settled peer whose content can travel to the target under the Admit rule becomes a candidate. This is how a deleted file is restored through an incoming `create` edge, how a newcomer is filled from an unchanged peer, and how a downstream edit with no outgoing edge is reverted. If there are still no candidates, the target keeps its state.

7. Resolve. If all candidates agree on one content, or all propose absence, the target takes that. Otherwise the daemon applies the resolver rules to each pair of disagreeing candidates. A candidate that beats each candidate with different content wins. If none wins, the daemon holds the target with the reason `conflict: <replica> vs <replica> (no rule decides)`, naming all candidates. The target keeps its basis.

8. Operation. Each unheld replica now has a desired state: keep, a content hash taken from a named replica, or absence. The operation follows from the desired state and the observation. A missing target, or a present target with different content, gets a write. A present target whose desired state is absence gets a delete. Anything else is left alone.

When the daemon reports a hold reason, the report starts with the display name of the held replica, followed by the reason.

After the plan has been applied, the executor commits a new basis for each unheld replica. This happens even when the plan contained no operations. The new basis is:

- for a file the executor wrote: the new snapshot
- for a file the executor deleted, or a recorded file found missing: a tombstone
- for a present file that was left alone: the observed snapshot
- for a missing file that had no record or a tombstone: the old basis, unchanged

Each held replica is marked unresolved, together with its reason. While a key has an unresolved mark, the daemon evaluates it on each cycle, even when nothing on disk changes, so a standing conflict stays visible.

If a key is missing at all of its replicas, the daemon does not run the rules above for it. Unless a gate is closed, the daemon deletes the key's records right away. A file that later appears under the same key starts with no record.


## Applying a plan

The executor applies a plan in stages. If a stage fails, the later stages do not run.

1. Staging. For each write in the plan, the executor makes sure the blob store holds the content. If the store already has the blob, nothing is read. Otherwise the executor reads the file at the replica named as the source, hashing it as it reads, and stores the bytes under their hash. If the hash comes out different from the observed one, the file changed during the cycle. The executor then tries each other replica observed with the same hash, and aborts the plan only if none of them still holds that content. If a transform is configured for the edge (see Actions and transforms), it runs here against the staged blob. Writes read from the blob store, and from nowhere else. This is why the executor can overwrite a replica even when another replica is about to receive that replica's old content: the old content was staged as a blob first.

2. Archiving. For each present target that is about to receive different content or be deleted, the executor copies the current file into the blob store, hashing as it copies, and writes an archive row: relation, key, replica, path, hash, size, reason (`overwritten` or `deleted`), and time. If the hash differs from the observed snapshot, the target changed during the cycle and the plan aborts. A target that has vanished since the observation is skipped without a row. Because this stage runs before any write, nothing is destroyed without an archived copy.

3. Journaling. If the plan contains at least one operation, the executor writes an intent row for the key, describing the operations. For each file about to be created or deleted, the executor creates a marker file in the target's folder, named after the target with `.replica.tmp` appended. An overwrite of an existing file gets no marker, since the rename in the next stage is atomic. The marker records the process id, relation, key, replica, operation, and start time, plus a note saying that the marker disappears once the operation commits. A plan with no operations skips this stage and the next and goes straight to the commit.

4. Writing. For each write, the executor creates any missing parent folders, creates a temporary file named `.<file>.replica-write` in the target folder, sets its permission bits while it is still empty, streams the blob into it, sets the modification time, syncs the file, renames it over the target, and syncs the folder. Some filesystems refuse the folder sync, and the executor ignores that refusal. With `sync_times` on, the modification time is copied from the replica that supplied the content. A newly created file gets that replica's permission bits. An overwritten file keeps its own. If the bits cannot be set, the write still succeeds and the executor reports a warning. For each delete, the executor removes the target, and treats an already missing target as removed. If a file name plus suffix would exceed 255 bytes, the executor uses the first 16 hex characters of the BLAKE3 hash of the name in place of the name, both for the marker and for the temporary file.

5. Committing. In one transaction, the executor writes the new basis of each unheld replica, marks the held replicas unresolved, clears old unresolved marks on the others, and deletes the intent row. Then it removes the markers and any staged blob that no archive row refers to.

If a stage fails, the executor removes the markers and the intent row, leaves the records untouched, and returns the error along with the operation and the path. The next cycle plans the key again from the last consistent basis. If the commit stage itself fails, the files already written and deleted stay as they are, and the next cycle sees them as changes and plans the key from there.

An intent row found at startup means that a previous process died in the middle of a plan. The daemon reports it as a stale intent, shows it on the dashboard for the life of the process, deletes the row, and removes any `.staging-` files from the blob folder. Markers and `.replica-write` files left in the target folders stay where they are, and the key walk skips them. The daemon then handles the key like any other, from its observations.

### Archive eviction

After each archive write, the blob store checks its cap. While the archived blobs together exceed the cap, the store picks the relation with the most archived bytes and deletes that relation's oldest archive row. The row just written is exempt, so a single revision larger than the cap stays. A blob with no remaining archive rows is deleted from disk. Evicted revisions disappear from the history page along with their rows. A row whose blob is missing from disk shows `evicted` in place of its download link.


## State folder

The daemon creates the `state` folder if it is missing, creates `blobs/` the first time it stages content, and creates the parent folder of each log file at startup. The folder holds:

- `state.db`, the SQLite database, in write-ahead mode.
- `blobs/`, the blob store. Each blob is a file named by the hex of its BLAKE3 hash, readable only by the owner. The store writes bytes to a temporary file named `.staging-<pid>-<n>` and renames it into place, so any blob found under its final name is complete. Leftover `.staging-` files are removed at startup.
- `replica.lock`, the lock file. At startup, before opening the database or serving the dashboard, the daemon creates this file and takes an exclusive lock on it. A second daemon pointed at the same folder finds the lock taken, prints the state folder and the lock path, and exits with status 2.
- The log files, if the relations' `log` paths point here.

### Tables

- `setting`: `key` and `value`. The schema version is stored under `schema_version`. Gate flags are stored under `gate` and `gate.<relation>`, with the value `true` or `false`.
- `relation`: `name`, `fingerprint`.
- `replica`: `rel`, `id` (the replica identity), `path` (the pattern text), `seen` (when the daemon last noted the replica, rewritten on each cycle).
- `record`: `rel`, `key`, `replica`, `present`, `mtime`, `size`, `hash`. A row with `present` 0 is a tombstone, and its other columns are null. `key` holds the canonical key text. Times are milliseconds since the Unix epoch. Hashes are 32-byte blobs.
- `unresolved`: `rel`, `key`, `replica`, `reason`.
- `intent`: `rel`, `key`, `detail` (the plan text), `started`.
- `archive`: `id`, `rel`, `key`, `replica`, `path`, `hash`, `size`, `reason`, `at`.

### Schema version and first run

The schema version is 2. If the database has a different version, the daemon refuses to open it and names the file and both versions. Start a fresh state folder in that case.

The first cycle against a fresh state folder behaves as a first run. No replica has a record, so each present replica proposes its content under the Voice rule. Where the copies are identical, the daemon stores a snapshot of each copy as its basis and changes no file. Where they differ, the resolver rules decide, and the daemon holds the key with a conflict reason only if no rule applies.

### Policy fingerprint

The fingerprint of a relation covers its site names, the pattern text and constraints of each location, the flow and resolver text, the combined ignore list (the top-level globs followed by the relation's own), and the transform commands. When the fingerprint of a loaded relation differs from the stored one, the daemon evaluates each key of that relation on each cycle instead of skipping settled keys. It does this under a closed gate too. The first cycle with the gate open stores the new fingerprint.


## Cycles and triggers

### Schedule and watch

The daemon runs a full cycle over all relations as soon as it starts. After that, each full cycle begins `interval` after the previous one ended.

With `watch` on, the daemon asks the operating system (through the `notify` crate) to report changes under the root of each replica and under the root of each fan-out pattern. When a change is reported, the daemon marks dirty each relation whose registered root contains the changed path. Once `debounce` has passed with no further reports, the daemon runs an early cycle, that is, a cycle over the dirty relations only. The scheduled full cycle still runs at `interval`, so a lost or merged report delays a sync by at most one interval. The daemon's own writes are reported too, but the next cycle finds those files matching their bases and does nothing. A relation with `watch: false` relies on the schedule alone.

If a root does not exist at startup, the watcher does not register it, and nothing registers it later in the run. Until the daemon restarts, it learns of changes under that root from the scheduled cycle only. If the watcher cannot start at all, the daemon prints `watch: disabled` with the reason and runs on the schedule. If the operating system refuses one root, the daemon prints a message and leaves that root unregistered.

### Skipping settled keys

Within a cycle, the daemon skips a key when all of its replicas match their bases. It still evaluates such a key if the key has an unresolved mark, if the relation's fingerprint has changed, or if a replica of the relation is new to the database or has relocated since the last cycle.

### Reporting persisting conditions

A condition that persists across cycles, such as a conflict, is reported when it first appears and again whenever it changes. The cycles in between stay quiet. Once the condition clears, its next appearance is reported again. The daemon keeps this bookkeeping in memory, so after a restart it reports each standing condition once more.

### Control requests

The dashboard and the command line send control requests to the daemon:

- `pause` stops both the scheduled cycles and the early cycles that change reports trigger.
- `resume` runs a cycle at once and restores the schedule.
- `sync` runs a full cycle over all relations at once, even while paused. A paused daemon stays paused afterwards.
- `restart` first checks that the configuration file still loads. If it does not, the daemon refuses the restart and keeps running with its current configuration. Otherwise the daemon finishes the key it is applying, exits with status 1, and launchd starts it again.
- `halt` makes the daemon finish the key it is applying and exit with status 0. SIGTERM and SIGINT have the same effect.

### Gate persistence

Each gate is stored as a flag in the `setting` table. A flag of `true` means closed. At startup, a gate starts closed if either its stored flag or the configuration says closed. An open flag in the database does not open a gate that the configuration closes.

Opening or closing a gate from the dashboard or the command line writes the flag, so the gate keeps its state across restarts. `gate: true` at the top level, or the `--gate` option, does more than close the global gate at startup: it also writes `true` to the global flag, so later runs start closed too, until someone opens the gate. A relation's own `gate: true` closes that relation's gate at startup without writing its flag. If you remove it from the configuration, that gate opens on the next start, unless someone closed the gate by hand in the meantime.

A gate changed from the dashboard takes effect immediately for cycles that have not started yet. A cycle already running reads each relation's gate once, just before visiting that relation's keys, so it picks up the change at the next relation. The flag is written to the database once the running cycle ends.


## Configuration

The configuration is a YAML file. The loader honors YAML merge keys (`<<`). It rejects any field outside the lists below, whether at the top level, inside `archive` or `dashboard`, inside a relation or a profile, or inside a command map.

All paths must be absolute or start with `~`, because the daemon's working directory under launchd is `/`. The one exception is an `include` entry, which the loader resolves relative to the configuration file.

### Top-level fields

- `state`: the state folder. The `run`, `plan`, and `forget` subcommands need it. `check` and `explain` work without it.
- `interval`: time between scheduled cycles. Default `30s`. Units are `ms`, `s`, `m`, and `h`. A bare number means seconds. Zero is an error. `debounce` and each command `timeout` use the same format.
- `watch`: whether to register for filesystem change reports. Default `true`.
- `debounce`: how long to wait after a change report before the early cycle. Default `2s`.
- `gate`: whether the global gate starts closed. Default `false`. The `--gate` option of `run` has the same effect.
- `ignore`: globs matched against the tail of a path. A matching file is left out of all relations. `*` matches within one segment.
- `archive`: a map with `cap`, the size limit of the archive. Default `100MB`. Units are `B`, `KB`, `MB`, `GB`, `TB`, or their first letters, in any letter case, as binary multiples. Decimals are allowed. A bare number means bytes.
- `dashboard`: a map with `port` (required), `recent_events` (default 200), and `refresh`, the seconds between page refreshes (default 10). Without this field there is no dashboard, and the control subcommands have nothing to connect to.
- `include`: a list of files or globs. A relative entry is resolved against the folder of the configuration file. An absolute entry, or one starting with `~`, is used as is. A wildcard may appear only in the file name. A glob picks up the regular files of its folder in name order and may match nothing. A literal path to a missing file is an error. An included file has the same form as the main file. The loader takes `vars`, `profiles`, `relations`, and `ignore` from it. The loader rejects the file if it sets `state`, `interval`, `dashboard`, or `include`. The loader ignores `watch`, `debounce`, `gate`, and `archive` if the file sets them. The included entries merge into the main file's. If a variable, profile, or relation is defined twice, the loader reports an error naming the main file and the included file with the later definition.
- `vars`: configuration variables.
- `profiles`: bundles of relation fields.
- `relations`: the relations, by name.

### Profiles

A profile is a named map of relation fields. A relation lists its profiles under `profile`, as a name or a list of names. The loader builds the relation by deep-merging, in this order: the profile named `_` if there is one, then each listed profile, then the relation's own body. In a deep merge, a later mapping adds its new keys to the earlier mapping. Where both mappings have the same key, the loader merges the two values by this same rule. A later list or scalar replaces the earlier value outright. A profile may itself list profiles, and the loader merges those first. A profile chain that loops back on itself is an error. An unknown field inside a profile is an error, and the message names the profile.

### Variables

A configuration variable is written `${name}`. After the loader has merged a relation's profiles, it expands variable references in each string of that relation, in mapping keys as well as values. This covers site names, location labels, paths, patterns, flow and resolver text, commands, transform edges, and log paths. Path variables in plain braces, such as `{name}`, belong to patterns and are left alone, so a path variable and a configuration variable may share a name.

The loader builds the scope of a relation in layers, each layer overriding the one before:

1. The top-level `vars`. `${state}` is set to the state folder unless `vars` defines it.
2. `${relation}`, set to the relation's name. For a relation stamped out by `each`, this is the copy's name.
3. The `each` values of this copy.
4. The relation's own `vars`, read after the profiles are merged, so a profile may supply them.

A value may refer to other variables. An undefined reference, a circular reference, or an unclosed `${` is an error, and the message quotes the string and the variable. Inside `on` and `transform` commands, unknown references are left as written, because the daemon fills them in when the command runs.

### Relation fields

    sites:
        source: ~/Desktop/mylibrary/
        dest: ~/scripts/desktop-library/
    where: { path: "*.js" }
    fanout: [project]
    flow: "source <-> dest [edit create]"
    resolve: "source mtime dest"
    ignore: ["*.tmp"]
    sync_times: true
    gate: false
    watch: true
    log: ${state}/logs/${relation}.log
    profile: two-way
    vars: { ... }
    each: { ... }
    on: { ... }
    transform: { ... }

- `sites`: see Sites, keys, and replicas.
- `where`: constraints by variable, see Patterns.
- `fanout`: the fan-out variables.
- `flow`, `resolve`: see Flow and resolver. `resolve` is optional. A relation without rules holds each disagreement as a conflict.
- `ignore`: globs added to the top-level list for this relation.
- `sync_times`: copy the modification time along with the content. Default `true`.
- `gate`: start this relation's gate closed. Default `false`.
- `watch`: register this relation's roots for change reports. Default `true`.
- `log`: a file that receives the relation's events, one per line. Its path is reserved, so no root may contain it.
- `profile`, `vars`: see above.
- `each`: a map from variable names to non-empty lists of values. The loader reads it from the relation's own body and ignores it inside a profile. It creates a copy of the relation for each combination of values, named `<relation>-<value>...` with the values in variable-name order, and gives each copy its values as variables. Use this for several independent relations that share a form but need their own gates, logs, and histories.
- `on`, `transform`: see Actions and transforms.

### Validation

The loader checks each relation and reports the first problem it finds, together with the relation, site, field, and offending text. It checks that:

- each path is absolute or starts with `~`
- each pattern follows the syntax rules
- each constraint is a valid glob or regular expression, and each `where` entry names a variable that some site uses
- each variable is either an identity variable or a declared fan-out variable
- each fan-out variable matches a single segment and comes before the identity variables of its pattern
- at least two sites are declared
- `flow` has at least one edge, and `flow`, `resolve`, and `transform` name only declared sites
- the receiving site of a transform has no outgoing edges
- each command is non-empty, and its timeout parses when given
- each `on` key is one of `changed`, `resolved`, `held`, `conflict`, `error`
- each `transform` key has the form `from -> to`
- no two relations share a name after `each` expansion

At startup, the daemon also refuses to run if any footprint overlaps the state folder or a log file. At the start of each cycle, the daemon checks the footprints of all replicas against each other and against those reserved paths. Any relation involved in an overlap is skipped for that cycle and reported. This per-cycle check is what catches a fan-out replica that appears after startup.


## Actions and transforms

### Actions

The `on` block maps event kinds to shell commands:

    on:
        changed: "make -C ~/scripts/site"
        resolved: "scp ${path} host:backup/${relation}/"
        conflict: "osascript -e 'display notification \"${key}\" with title \"replica conflict\"'"
        error: { run: "...", timeout: 5m }

A command is either a string or a map with `run` and `timeout`. `timeout` uses the same duration format as `interval` and defaults to one minute.

The event kinds, and the variables each one provides:

- `changed` runs once per cycle in which the relation applied at least one operation, after that cycle's `resolved` actions. Variables: `ops`, the number of operations the relation applied in the cycle.
- `resolved` runs once for each operation applied. Variables: `key`, `path`, `replica`, `site`, `op` (`write` or `delete`), and `plan`.
- `held` runs while a gate is holding a plan that has at least one operation. It runs only when something differs from the last report: the plan itself, the hold reasons, or the observed content at some replica. Variables: `key` and `plan`.
- `conflict` runs after a cycle with the gate open leaves a key with held replicas, provided the hold reasons differ from those in the last report. It does not run while the gate is closed. Variables: `key`, and `reason`, a list of the held replicas, each written `<replica>: <reason>` and separated by `; `.
- `error` runs when inspecting, deciding, or applying one key fails, and the error differs from the last report. A failure of the whole relation, such as an expansion failure, an overlap, or an unreadable root, produces an error event but runs no action. Variables: `key` and `error`.

`${relation}` is available in each kind. The daemon substitutes the variables into the command text and runs it with `sh -c`, with the state folder as the working directory and standard input closed. The environment is the daemon's own, plus `REPLICA_<NAME>` for each variable (upper case) and `REPLICA_EVENT` holding the kind. The daemon waits for the command, so the cycle pauses for as long as the command runs, up to the timeout, at which point the daemon kills it. The resulting action event records the expanded command, its exit status (or a note that it timed out), and the last line of its output, where the output is standard output followed by standard error. The relation's log receives this event. A nonzero exit or a timeout is reported as a failed action, but it does not change the outcome of the cycle.

An action that writes into a replica of its own relation causes a transition on the next cycle, like any outside edit. If the action is idempotent, the daemon finds the rewritten file unchanged by hash and the relation settles after one extra cycle.

### Transforms

The `transform` block maps an edge, written `from -> to`, to a command. The command takes the same string-or-map form as an action and has the same default timeout.

    transform:
        "src -> out": "sassc ${in} ${out}"

The executor applies a transform to each write whose content comes from a replica of the `from` site and goes to a replica of the `to` site. The `from` site is the site of the replica that supplied the winning content. As a result, a transform on `b -> c` does not apply to content that `a` sends to `c` through the closure, and a fan-in such as `{a, b} -> out` needs one entry per supplying site.

The executor runs the command during staging, with `sh -c` and the state folder as the working directory. `${in}` is the path of the sender's blob in the blob store. `${out}` is a path inside the blob folder that does not exist when the command starts. The environment is the daemon's own plus `REPLICA_IN` and `REPLICA_OUT`. After the command finishes, the executor hashes the file the command wrote to `${out}`, stores a copy in the blob store under that hash, deletes the file at `${out}`, and uses the hash as the receiver's content. The receiver's basis records that hash, so the file reads as unchanged on the next cycle. If the command exits nonzero, times out, or writes nothing to `${out}`, the plan for that key fails, and the daemon plans the key again on the next cycle.

The loader rejects a flow in which the receiving site of a transform has an outgoing edge, because transformed content cannot be mapped back to the sender's form.


## Interfaces

### Command line

    replica run --config PATH [--once] [--gate]
    replica check --config PATH
    replica explain --config PATH [RELATION]
    replica plan --config PATH [RELATION]
    replica status (--port N | --config PATH) [--json]
    replica sync | pause | resume | restart | halt (--port N | --config PATH)
    replica gate open|closed [--relation NAME] (--port N | --config PATH)
    replica history RELATION (--port N | --config PATH) [--json]
    replica forget --config PATH RELATION REPLICA

`replica --help` prints the usage and `replica --version` prints the version. Each option may also be written as `--option=VALUE`. On a usage or runtime error, the command prints `replica: <error>` to standard error and exits with status 2.

- `run` runs cycles at the configured interval. With `--once`, it runs one cycle and exits, with status 1 if that cycle reported an error and 0 otherwise. With `--gate`, it closes the global gate at startup and writes the flag to the database.
- `check` loads the configuration and expands the replicas. If the configuration does not load, or a footprint overlaps the state folder or a log file, it exits with status 2. If there are other expansion failures or root overlaps, it prints them to standard error and exits with status 1. Otherwise it prints `ok: N relations from PATH` and exits with status 0.
- `explain` prints the configuration path, the state folder, `interval`, `watch`, `debounce`, the gate default, and the top-level ignore list. Then, for each relation, it prints the profiles it was built from, the expanded variables, the key and fan-out variables, each location with its root, depth, and constraints, the discovered replicas (with `(folder missing)` where the root is absent), the flow closure per kind, the resolver, the relation's ignore list, its transforms, actions, and log path, and its `sync_times`, `watch`, and gate settings.
- `plan` opens the state database without taking the lock (creating the folder and the database if they are absent), runs discovery and decision, and changes no record and no file. For each relation it prints the relation's state (see the list of states under Dashboard) and any error, unavailable reason, or warning. Then, for each key that needs attention, it prints one line. A key with a plan prints the plan, labeled `next:` while the gate is open and `held:` while the gate is closed. A held key prints its hold reason, labeled `waiting:`. A relation with nothing pending prints `N files in sync`.
- `status`, `sync`, `pause`, `resume`, `restart`, `halt`, `gate`, and `history` connect to the daemon at 127.0.0.1 on the dashboard port, given as `--port N` or read from `--config PATH`. They give up after three seconds with no answer. A control command prints `ok: /PATH` whenever the daemon answers with a status below 400, which includes a refused restart. `status` prints a summary (version, pid, state, gate, counts, each relation's state), or the raw document with `--json`. `history` prints one line per archived version, marking evicted ones, or the raw document with `--json`. `gate` accepts `open`, `closed`, or `close`.
- `forget` takes a replica identity as described under Replica identity, such as `dest` or `dest[alpha]`, and deletes its records, its unresolved marks, and its stored pattern. If the identity has no records, it refuses and lists the identities that do. It writes to the state database without taking the lock, so halt the daemon first.

### Dashboard

The dashboard is an HTTP server bound to 127.0.0.1 on the configured `port`. If the port cannot be bound, the daemon prints the error and runs without a dashboard. Any request whose `Host` header is present but is not `localhost`, `127.0.0.1`, or `::1` (with or without a port) is refused with 403. A POST must carry a loopback `Host`. If the browser sends an `Origin` header, it must carry the dashboard's own address and port. If the browser sends `Sec-Fetch-Site`, the request must come from the page itself or from the address bar.

Routes:

- `GET /`: the main page. It shows a status strip (process, configuration path, database, last and next sync, watch state, this run's operation and error counts, archive use, and overall state), then the unfinished operations (intent rows cleared at startup and any still in the database), then each relation with its replicas and keys, and finally the recent events. For each key it shows the state at each replica and either the planned operation or the hold reason. Each relation's table is cut at 50 pending and 50 in-sync rows unless the request is `/?rel=NAME&all=1`. A relation is tagged `failed`, `unavailable`, `N awaiting sync`, `N held by gate`, `partly unreadable`, `folder missing`, or `in sync`. The page reloads itself after each `refresh` interval, skips the reload while the tab is hidden or text is selected, and shows a banner when the daemon stops answering.
- `GET /status.json`: the status document, described below.
- `GET /history?rel=NAME`: the archived versions of a relation, newest first, cut at 100 unless the query has `all=1`. Each version has a download link if its blob is still on disk, and an `evicted` mark otherwise. With `json=1`, the daemon returns the same list as JSON, with `at`, `key`, `replica`, `path`, `reason`, `size`, `hash`, and `available` per version.
- `GET /blob?h=HASH[&as=NAME]`: downloads an archived blob under the name NAME, or `HASH.bin` if `as` is absent. An unknown hash gives 404.
- `POST /sync`, `POST /pause`, `POST /resume`, `POST /restart`, `POST /halt`: the control requests described under Cycles and triggers. `/sync`, `/pause`, and `/resume` answer with a 303 redirect to `/`. `/restart` and `/halt` answer with a notice page and status 200, even when the restart was refused. A control route reached with any other method answers 405.
- `POST /gate?to=open|closed[&rel=NAME]`: opens or closes the global gate, or one relation's gate. It answers with a 303 redirect to `/`, or 400 if `to` is neither `open` nor `closed`.

While the engine is idle, the page and `status.json` read live state. While a cycle is running, they read the report the daemon published after the previous cycle. The page then notes that a sync is running and shows the time of that report, and the status document sets `report_stale` to true. If no cycle has finished yet, a request made during a cycle waits for the engine instead.

The status document contains:

- `state`: `running` or `paused`.
- `gate`: `open` or `closed`, for the global gate.
- `pid`, `version`, `uptime_secs`.
- `last_sync`, `next_sync`, `report_at`: seconds since the Unix epoch. `last_sync` and `last_sync_took_ms` are null before the first cycle. `next_sync` is null before the first cycle and while paused.
- `report_stale`: true while the document comes from the previous cycle's report.
- `cycles`: cycles run so far. `ops_applied`: file operations applied. `errors`: error and stale-intent events, so a persisting error counts once when it appears and again each time it changes. `awaiting_sync` and `held`: sums of the per-relation `awaiting` and `held` counts.
- `watch`: `enabled`, `roots`, and `dirty`.
- `relations`: one entry per relation with `name`, `state`, `awaiting`, `held`, `gated`, `files`, and `replicas`. `awaiting` counts the keys with a plan while the gate is open, plus the keys waiting on a conflict or an unreadable peer regardless of the gate. `held` counts the keys with a plan while the global or the relation's gate is closed, and is 0 otherwise. `gated` is true when either gate is closed. `files` is the number of keys and `replicas` the number of replicas.

A relation's `state` is the first of these that applies:

- `error`: expansion or the cycle failed, or a footprint overlaps.
- `unavailable`: a root with recorded snapshots is missing.
- `awaiting`: the `awaiting` count is above zero.
- `held`: the `held` count is above zero.
- `degraded`: a subfolder could not be read.
- `folder missing`: a replica's root is absent and nothing is recorded for it.
- `in sync`: none of the above.

### Events and logs

The engine reports what it does as events. At startup the `run` command attaches handlers, called hooks, that write each event to standard error, to the relation logs, and to the dashboard's event list. The event kinds, as the daemon prints them, are `resolved`, `held`, `skipped`, `pending`, `warning`, `error`, `stale intent`, `relocated`, `action`, and `cycle`. A filesystem change report produces no event. It shows up only in the `dirty` list of the status document and in the watch cell of the page.

The daemon writes each event to standard error, except a cycle event with no operations and no errors. At startup it creates the parent folders of each `log` path and opens the file for appending. From then on it writes each event of that relation as one line: the local time as `YYYY-MM-DD HH:MM:SS`, the relation name, the key if the event has one, and the event text. Cycle events stay out of relation logs. The dashboard keeps the most recent `recent_events` events in memory, excluding cycle events, and lists them newest first.


## Source layout

- `src/pattern.rs`: patterns, variables, constraints, matching and substitution, and the derived properties.
- `src/site.rs`: sites, keys, replicas, fan-out enumeration, and the key walk.
- `src/observe.rs`: the snapshot, basis, observation, and transition types, and the function that turns a basis and an observation into a transition.
- `src/flow.rs`: flow statements, kinds, closure, resolver rules, and the policy.
- `src/decide.rs`: the decision procedure.
- `src/medium.rs`: the file operation trait and its local filesystem implementation.
- `src/blobs.rs`: the blob store and eviction.
- `src/store.rs`: the database schema and queries.
- `src/apply.rs`: the executor.
- `src/engine.rs`: cycles, site expansion, reading observations, discovery, the footprint overlap check, reports and relation states, gates, and the suppression of repeated reports.
- `src/event.rs`: events, tones, and hooks.
- `src/action.rs`: command templating and running, for actions and transforms.
- `src/watch.rs`: filesystem change reports into the main loop.
- `src/config/mod.rs`, `src/config/text.rs`, `src/config/merge.rs`: the loader and validation, the YAML form as typed structures, and variable expansion, profile merging, include resolution, and `each` stamping.
- `src/dashboard/mod.rs`, `src/dashboard/http.rs`, `src/dashboard/render.rs`, `src/dashboard/status.rs`: the server and routes, the HTTP layer, the pages, and the status document.
- `src/dashboard/style.css`, `src/dashboard/favicon.svg`: the stylesheet and icon, compiled into the binary and served at `/style.css` and `/favicon.svg`.
- `src/cli.rs`: command-line parsing and each subcommand except `run`.
- `src/main.rs`: the `run` subcommand. It sets up the lock file, the signal handlers, the hooks that write events to standard error, the relation logs, and the dashboard, then runs the main loop.
- `tests/conformance.rs`, `tests/redesign.rs`: end-to-end test suites over temporary folders.

The YAML parser is the `serde_norway` crate. It has the same interface as `serde_yaml`.
