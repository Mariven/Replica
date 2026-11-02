# Replica

Replica is a daemon that keeps files in sync between folders on one machine. You declare which folders hold copies of the same files, and in which directions edits, new files, and deletions may travel. The daemon watches the folders, applies changes as they happen, archives anything it overwrites or deletes, and shows what it is doing on a local dashboard.


## Example

```yaml
state: ~/scripts/replica/live
dashboard: { port: 18516 }

relations:
    mylibrary:
        sites:
            source: ~/Desktop/mylibrary/
            dest: ~/scripts/desktop-library/
        flow: "source <-> dest [edit create]"
        resolve: "source mtime dest"

    skills:
        sites:
            source: ~/scripts/skills/{name}.md
            dest: ~/.claude/skills/{name}/SKILL.md
        flow: "source -> dest [all]"
```

The first relation keeps two folders in sync both ways. Edits and new files travel in either direction, a file deleted on one side is restored from the other, and when both sides change, the newer file wins. The second copies each `skills/NAME.md` to `NAME/SKILL.md` in one direction, deletions included, and reverts any edit made on the receiving side.

[config.example.yaml](config.example.yaml) shows the rest: variables, profiles, fan-out across project folders, ignore lists, gates, and shell commands that run after a sync or rewrite content on the way. [docs/design.md](docs/design.md) explains how the daemon decides what to do.


## Running it

```sh
cargo build
./target/debug/replica check --config config.yaml
./target/debug/replica plan --config config.yaml
./target/debug/replica run --config config.yaml
```

`check` validates the configuration, `plan` shows what the daemon would do without doing it, and `run` starts the daemon. The dashboard is then at `http://localhost:18516`. While the daemon runs, `replica status`, `sync`, `pause`, `resume`, `gate open|closed`, `restart`, and `halt` control it from the command line. `replica --help` lists everything.

To run it as a login service, see [docs/service-mac.md](docs/service-mac.md).


## Safety

Before the daemon overwrites or deletes a file, it copies the file into an archive in the state folder, and the dashboard's history page lets you download old versions. Writes go to a temporary file that is renamed into place, so a crash leaves either the old file or the new one. If a folder that used to have files disappears, as when a drive is unmounted, the daemon pauses that relation instead of treating the files as deleted.

A closed gate lets the daemon plan and report without writing anything. When trying a new configuration, start with `gate: true`, look at the dashboard, and then open the gate from there or with `replica gate open`.


## Upgrading from 1.x

The configuration format has changed, and the daemon will not open a 1.x state database. Rewrite the configuration with config.example.yaml as a guide, point `state` at a new folder, and start under a closed gate. The first cycle then shows which files already match across sites, which differ, and what the daemon would do about each. [config.redesign.yaml](config.redesign.yaml) is the current live configuration in the new format, with its own state folder and dashboard port, so the old and new versions can run side by side.
