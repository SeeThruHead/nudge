# nudge

nudge sends messages to coding agents in Herdr, on a schedule or on demand; that is its only job.

A job names an agent (by its address, folder, name or session), a schedule (every 30 minutes, at 09:00, or
any cron pattern) and a message. nudge waits while the agent is busy and sends the message once it is idle,
never interrupting it. It reads nothing about the machine and touches no processes.

## Targets

`{ address = "machine/space/name" }` is the one to use when you can. nudge resolves it with
[reach](https://github.com/SeeThruHead/reach), which gives every agent a stable address (in the Herdr app, the
Threads mod pins one to each agent; `reach ls` lists them). The message goes to exactly that agent: an asleep
agent is woken with it, a busy one is waited for. When the address does not resolve (nothing there, more than
one agent there, the agent ended, or reach is not installed), the job is not sent, nudge logs why and posts one
notification through `notify`, and tries again a minute later. It never falls back to another agent.

Without reach, `{ cwd }`, `{ agent }` (the terminal name) and `{ session }` still work through the host. A
target that matches more than one agent is refused the same way, never guessed.

| Host        | Finds agents by                                                                  | Sends with           |
| ----------- | -------------------------------------------------------------------------------- | -------------------- |
| `herdr`     | `herdr agent list` (status, folder, name, session)                               | `herdr agent prompt` |
| `herdr-app` | `hc ls --json` (live terminals running an agent: status, folder, name; the Claude session from `hc get` and `~/.claude/sessions`) | `hc send` |

`host = "auto"` (the default) uses the Herdr TUI and the Herdr desktop app together: agents from both, each
message sent through the one that runs the agent (TUI panes look like `w1:p2`, app terminals like `t7`).

Only terminals running an agent count. A shell split beside an agent shares its tab and name but is never
picked, so `{ agent = "name" }` always reaches the agent itself. A target that matches no agent is logged and
retried; one that matches more than one is refused and notified.

## Install

```sh
brew install seethruhead/tap/nudge
brew services start seethruhead/tap/nudge
```

Or build it: `cargo install --locked --path .`, then run `nudge run` under launchd or systemd (see below).

## Configure

`~/.config/nudge/config.toml` (or `$NUDGE_CONFIG`). nudge rereads it every few seconds, so edits apply
without a restart.

```toml
host = "auto"                                  # or herdr, herdr-app
notify = ["sth-seat", "notify", "--title", "nudge"]   # optional: a command; the message is added last

[[job]]
name = "ci-check"
every = "30m"                                  # at least 1m
target = { address = "laptop/app/app" }        # or { cwd = "..." }, { agent = "name" }, { session = "id" }
message = "Check CI on main and fix what is red."

[[job]]
name = "standup"
cron = "0 9 * * 1-5"                           # local time; 5 fields, or 6 with seconds
target = { agent = "api" }
message = "Summarise what changed since yesterday."

[[job]]
name = "evening-review"
at = ["12:30", "17:30"]                        # daily, local time
target = { session = "4f1c2e9a-8a7d-4b55-9d0e-0c1f7f0f2c11" }
message = "Review today's commits for anything risky."
```

Each message starts with `[nudge: <job>, <schedule>]` so the agent knows where it came from.

Configs written for older versions still load: `[[watch]]`, `[park]`, `[freeze]` and `[background]` tables
are ignored.

## Commands

```text
nudge run           every 5s: read the config and send each job's message when it is due
nudge send <job>    send one job's message now
nudge check         show the host, where each job goes and when it runs next
nudge --version
```

Binaries are found on `PATH`, `~/.local/bin`, `/opt/homebrew/bin` and `/usr/local/bin`; set `NUDGE_HERDR`,
`NUDGE_HC` or `NUDGE_REACH` to choose one.

## Run it as a service without Homebrew

macOS, `~/Library/LaunchAgents/nudge.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>nudge</string>
  <key>ProgramArguments</key><array><string>/Users/me/.cargo/bin/nudge</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/Users/me/Library/Logs/nudge.log</string>
</dict></plist>
```

then `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/nudge.plist`.

Linux, `~/.config/systemd/user/nudge.service`:

```ini
[Unit]
Description=nudge

[Service]
ExecStart=%h/.cargo/bin/nudge run
Restart=always

[Install]
WantedBy=default.target
```

then `systemctl --user enable --now nudge`.

## License

MIT
