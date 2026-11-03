# Running Replica as a macOS service

Replica runs as a per-user LaunchAgent. launchd starts it at login and restarts it if it crashes.


## Install

Build the release binary, create the log folder (launchd does not create it), and check the configuration:

```sh
cargo build --release
mkdir -p ~/scripts/replica/live/logs
./target/release/replica check --config config.yaml
```

Write this to `~/Library/LaunchAgents/com.mariven.replica.plist`, with `YOU` replaced by your user name:

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

Then load it:

```sh
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.mariven.replica.plist
```

With `SuccessfulExit` set to false under `KeepAlive`, launchd restarts the daemon after a crash or a nonzero exit and leaves it stopped after a clean exit. Replica's restart and halt depend on this: `replica restart` exits with status 1, so launchd starts a fresh process, while `replica halt` exits with status 0, so the service stays stopped. `ProcessType` and `LowPriorityIO` keep the folder walks from competing with foreground work.


## Day to day

The dashboard and the `replica` commands control the running daemon, as described in the README. From the shell, launchd can also start a halted daemon or restart a running one:

```sh
launchctl kickstart gui/$UID/com.mariven.replica
launchctl kickstart -k gui/$UID/com.mariven.replica
```

The daemon reads its configuration only at startup. After editing `config.yaml`, run `replica check` and then `replica restart`. If the new configuration fails to load, the daemon refuses the restart and keeps running on the old one. To run new code, build the release binary and restart the same way, since the agent runs the binary straight from `target/release`.

After editing the plist itself, launchd has to load it again:

```sh
launchctl bootout gui/$UID/com.mariven.replica
launchctl bootstrap gui/$UID ~/Library/LaunchAgents/com.mariven.replica.plist
```

Before rearranging synced folders by hand, pause the daemon, and resume it once the dashboard shows what you expect. Halt the service before running `replica run --once` against the same state folder, since only one daemon can hold the lock.

Events and errors go to `daemon.err`, one line each, stamped with the local time. A relation with a `log` setting also writes its own events to that file.


## Remove

```sh
launchctl bootout gui/$UID/com.mariven.replica
rm ~/Library/LaunchAgents/com.mariven.replica.plist
```

Deleting the state folder leaves the synced files in place but loses the records and the archive, and the next run starts as a first run. If you move or back up the database, keep `state.db` together with its `-wal` and `-shm` files.
