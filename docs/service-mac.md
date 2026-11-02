# Running Replica as a macOS service

Replica runs as a per-user LaunchAgent under launchd control. At user login, launchd starts the daemon, reviving the process after unexpected crashes, while leaving the process offline after clean termination.

## Install

Build the release binary, create the log directory (launchd does not create the directories its log paths point into), and check the config before registering the agent:

```sh
cargo build --release
mkdir -p ~/scripts/replica/live/logs
./target/release/replica check --config config.yaml
```

Populate `~/Library/LaunchAgents/com.mariven.replica.plist`, adjusting paths to match your user account:

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

With `SuccessfulExit: false` configured under `KeepAlive`, launchd restarts replica after unexpected terminations, while leaving the process stopped after clean exits. When you click restart in the dashboard, the daemon finishes the file operation in progress, then exits with non-zero status so launchd starts a new instance. When you click halt, or launchd sends SIGTERM, the daemon finishes the operation in progress and exits with status zero, leaving the service stopped.

## Control

The dashboard, running at the port in `dashboard.port`, provides the primary user interface. Its buttons issue HTTP POST requests to the local server, so you can script every action directly:

```sh
curl -X POST localhost:18516/sync  # run a sync now instead of waiting for the next scheduled one
curl -X POST localhost:18516/pause  # pause sync cycles while continuing dashboard service
curl -X POST localhost:18516/resume  # resume, starting with an immediate pass
curl -X POST 'localhost:18516/gate?to=closed'  # hold every write; to=open releases it (quote the URL: zsh treats ? as a glob)
curl -X POST 'localhost:18516/gate?to=open&rel=mylibrary'  # one entry's gate alone
curl -X POST localhost:18516/restart  # validate the config, then exit non-zero so launchd restarts with it
curl -X POST localhost:18516/halt  # exit with status zero so launchd leaves the process stopped
```

The server accepts POSTs only with a loopback `Host` header and, when a browser sends one, an `Origin` of this dashboard, so a web page from another site cannot drive it.

External scripts and status bar widgets query `GET /status.json` for telemetry. The body carries the process state (`running` or `paused`), the global gate (`open` or `closed`), the pid, version, and uptime, the last sync's start time and duration, the next sync time, when the report was taken and whether it is a snapshot from before a sync that is running now, counters for cycles, applied operations, and distinct errors since start, the number of files awaiting sync and held by a gate, and one entry per relation with its name, state, gated flag, and counts of files awaiting sync and held.

While a sync runs, the dashboard and `/status.json` serve the report published at the end of the previous sync, marked as such, instead of waiting for the engine. Between syncs they read live state.

Manage service lifecycle actions externally through `launchctl`:

```sh
launchctl kickstart gui/$UID/com.mariven.replica  # start a halted instance
launchctl kickstart -k gui/$UID/com.mariven.replica  # terminate and restart the running process
launchctl print gui/$UID/com.mariven.replica  # inspect service registration and runtime state
```

## Maintain

To update configuration settings, edit `config.yaml`, run `replica check --config config.yaml` to catch typos and overlapping roots, read `replica explain --config config.yaml` to see how it resolves, and send `POST /restart` (or `replica restart --config config.yaml`). Replica loads configuration values during startup, requiring a restart to apply subsequent edits. A restart request first loads the config itself and refuses to exit if the file does not load, so a typo cannot crash-loop the agent.

To run updated code, compile with `cargo build --release` and send `POST /restart`. The LaunchAgent executes the newly compiled binary directly from `target/release`.

Reload modified plists by restarting the agent registration, as kickstart commands retain previously cached definitions:

```sh
launchctl bootout gui/$UID/com.mariven.replica
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.mariven.replica.plist
```

Monitor service logs in `live/logs/`. Synchronization events and errors stream sequentially to `daemon.err`, each line stamped with the local time. Standard output routes to `daemon.log`. Paths configured with a `log:` directive write their events into their dedicated files as well. A condition that persists across cycles, such as a held plan, an open conflict, or a failing operation, is logged when it appears or changes, not on every cycle.

A root that contains the state directory or a log file is refused at startup and by `check`, since the daemon would otherwise sync its own files. Roots that merely nest inside each other are reported on the dashboard and skipped each cycle, since a scan can produce them at any time.

Before reorganizing files, send `POST /pause` to hold synchronization passes. Verify planned changes in the dashboard, then send `POST /resume` to process the updates.

Halt the service before running `run --once` against the production state directory. A second instance refuses to start while the first holds the lock file inside it.

## Remove

```sh
curl -X POST localhost:18516/halt
launchctl bootout gui/$UID/com.mariven.replica
rm ~/Library/LaunchAgents/com.mariven.replica.plist
```

The `live/` directory stores the state database with its write-ahead log and shared-memory files (`state.db-wal`, `state.db-shm`), the lock file that keeps a second daemon off the database (`state.db.lock`), the archive of overwritten and deleted revisions, and the log files. The three database files belong together; move or back them up as one. Deleting it leaves the synchronized files in place but discards the sync baseline and every archived revision: the next run treats every file as newly seen and aligns differing copies according to the propagate rules, with nothing to restore from.
