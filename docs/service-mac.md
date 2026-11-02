# Running Replica as a macOS service

Replica runs as a per-user LaunchAgent. launchd starts the daemon at login and restarts it after a crash. After a clean exit the daemon stays stopped.

## Install

Build the release binary, create the log directory (launchd does not create the directories for its log paths), and check the config before registering the agent:

```sh
cargo build --release
mkdir -p ~/scripts/replica/live/logs
./target/release/replica check --config config.yaml
```

Write this to `~/Library/LaunchAgents/com.mariven.replica.plist`, with the paths changed to match your user account:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.mariven.replica</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Users/YOU/scripts/replica/target/release/replica</string>
        <string>run</string>
        <string>--config</string>
        <string>/Users/YOU/scripts/replica/config.yaml</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/Users/YOU/scripts/replica/live/logs/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/YOU/scripts/replica/live/logs/daemon.err</string>
</dict>
</plist>
```

`ProcessType Background` and `LowPriorityIO` keep the directory walks from competing with foreground work.

```sh
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.mariven.replica.plist
```

`SuccessfulExit: false` under `KeepAlive` has launchd restart replica after a crash or a non-zero exit and leave it stopped after an exit with status zero. The dashboard's restart button relies on this: the daemon finishes the file operation in progress and exits non-zero, so launchd starts a fresh instance. The halt button, or a SIGTERM from launchd, makes the daemon finish the operation in progress and exit with status zero, so the service stays stopped.

## Control

The dashboard at the port in `dashboard.port` is the main interface. Each of its buttons sends an HTTP POST to the local server, so the same actions can be scripted:

```sh
curl -X POST localhost:18516/sync  # run a sync now instead of waiting for the next scheduled one
curl -X POST localhost:18516/pause  # pause sync cycles; the dashboard keeps serving
curl -X POST localhost:18516/resume  # resume, starting with an immediate pass
curl -X POST 'localhost:18516/gate?to=closed'  # hold all writes; to=open releases them (quote the URL, since zsh treats ? as a glob)
curl -X POST 'localhost:18516/gate?to=open&rel=mylibrary'  # the gate of a single relation
curl -X POST localhost:18516/restart  # validate the config, then exit non-zero so launchd restarts with it
curl -X POST localhost:18516/halt  # exit with status zero so launchd leaves the process stopped
```

The server accepts a POST only when its `Host` header is a loopback address and, for a request from a browser, when its `Origin` is this dashboard. A page from another site is refused.

Scripts and status bar widgets read `GET /status.json`. The body holds:

- the process state (`running` or `paused`) and the global gate (`open` or `closed`)
- the pid, the version, and the uptime
- the start time and duration of the last sync, and the time of the next one
- the time the report was taken, and whether it is a snapshot from before a sync still in progress
- counters since start: cycles, applied operations, and distinct errors
- the number of files awaiting sync and the number held by a gate
- an entry per relation with its name, state, gated flag, and its own awaiting and held counts

While a sync runs, the dashboard and `/status.json` serve the report published at the end of the previous sync, marked as such, instead of waiting for the engine. Between syncs they read live state.

Start, restart, and inspect the service from the shell with `launchctl`:

```sh
launchctl kickstart gui/$UID/com.mariven.replica  # start a halted instance
launchctl kickstart -k gui/$UID/com.mariven.replica  # kill and restart the running process
launchctl print gui/$UID/com.mariven.replica  # show the registration and the running state
```

## Maintain

To change the configuration, edit `config.yaml`. Run `replica check --config config.yaml` to catch typos and overlapping roots, then read `replica explain --config config.yaml` to see how the file resolves. Send `POST /restart` (or run `replica restart --config config.yaml`) to pick the change up, since the daemon reads the configuration only at startup. The restart handler loads the config itself before exiting and refuses to exit when the file fails to load, so after a typo the agent keeps running on the old configuration.

To run new code, build with `cargo build --release` and send `POST /restart`. The LaunchAgent runs the binary straight from `target/release`, so the new build is picked up.

After editing the plist, register the agent again, because `kickstart` keeps using the definition it loaded before:

```sh
launchctl bootout gui/$UID/com.mariven.replica
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.mariven.replica.plist
```

The logs are in `live/logs/`. Sync events and errors go to `daemon.err`, each line stamped with the local time. Standard output goes to `daemon.log`. A relation with a `log:` setting writes its events to that file as well. A condition lasting across cycles, such as a held plan, an open conflict, or a failing operation, is logged once when it appears and again when it changes.

When a root contains the state directory or a log file, `check` fails and the daemon refuses to start, because the daemon must not sync its own files. A root nested inside another root is instead reported on the dashboard and skipped on each cycle, since a fan-out can produce a new overlap at any time.

Before moving files around, send `POST /pause` to hold the sync passes while you work. Check the planned changes in the dashboard, then send `POST /resume`.

Halt the service before running `run --once` against the production state directory. A second instance refuses to start while the first holds the lock file inside it.

## Remove

```sh
curl -X POST localhost:18516/halt
launchctl bootout gui/$UID/com.mariven.replica
rm ~/Library/LaunchAgents/com.mariven.replica.plist
```

The state directory holds:

- the database with its write-ahead log and shared-memory files (`state.db`, `state.db-wal`, `state.db-shm`); these belong together, so move or back them up as one
- the lock file (`replica.lock`), held by the running daemon to keep a second one out
- the blob store of archived revisions (`blobs/`)
- the log files

Deleting the directory leaves the synced files in place but discards the sync baseline and the archived revisions. On the next run each file counts as newly seen: identical copies are aligned, differing copies are held for a decision, and there is nothing to restore from.
