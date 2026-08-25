# Running Replica as a macOS service

Replica runs as a per-user LaunchAgent. launchd starts it at login, relaunches it after a crash, and leaves it down after a clean exit.

## Install

Build, then write the agent plist and load it:

```sh
cargo build --release
```

`~/Library/LaunchAgents/com.mariven.replica.plist` (adjust paths):

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
    <key>StandardOutPath</key>
    <string>/Users/YOU/scripts/replica/live/logs/daemon.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/YOU/scripts/replica/live/logs/daemon.err</string>
</dict>
</plist>
```

```sh
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.mariven.replica.plist
```

`KeepAlive` with `SuccessfulExit: false` means: relaunch on any nonzero exit, stay down after a clean one. The dashboard's restart button exits nonzero on purpose. Its halt button exits cleanly on purpose.

## Control

The dashboard (`dashboard.port` in the config) is the main interface. Its controls are ordinary POSTs to the loopback server, so they are also scriptable:

```sh
curl -X POST localhost:18516/pause    # stop cycling; keep serving the dashboard
curl -X POST localhost:18516/resume   # cycle again immediately
curl -X POST localhost:18516/restart  # exit nonzero; launchd relaunches and rereads the config
curl -X POST localhost:18516/halt     # exit cleanly; launchd leaves it stopped
```

`GET /status.json` returns machine-readable state (running/paused, pid, uptime, last and next sync, cycle/op/error counts, files awaiting sync) for scripts and menubar widgets.

Use `launchctl` for things the process cannot do from inside:

```sh
launchctl kickstart gui/$UID/com.mariven.replica     # start (for example after halt)
launchctl kickstart -k gui/$UID/com.mariven.replica  # kill and restart
launchctl print gui/$UID/com.mariven.replica         # detailed status
```

## Maintain

Change the config: edit `config.yaml`, then `POST /restart`. The config is read once at startup; nothing watches it.

Update the code: `cargo build --release`, then `POST /restart`. The agent runs the binary from `target/release` in place.

Edit the plist: launchd does not reread a plist on kickstart. Reload it:

```sh
launchctl bootout gui/$UID com.mariven.replica
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.mariven.replica.plist
```

If bootout reports an I/O error while the job is crash-looping, the older `launchctl unload ~/Library/LaunchAgents/com.mariven.replica.plist` still dislodges it.

Read the logs: `live/logs/daemon.err` is the event stream (one line per resolution, skip, or error). `daemon.log` is stdout. Entries with a `log:` path also append their own events there.

Risky edits: `POST /pause` first, move files around, check that the dashboard shows what you expect, then `POST /resume`.

Do not run `--once` against the live state database while the service is up: the two processes would race on the same record. Pause or halt first.

## Remove

```sh
curl -X POST localhost:18516/halt
launchctl bootout gui/$UID com.mariven.replica
rm ~/Library/LaunchAgents/com.mariven.replica.plist
```

The state database and logs live in `live/`. Delete that folder to forget everything on record. Removal does not touch the synced files themselves.
