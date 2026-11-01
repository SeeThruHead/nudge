# nudge

A scheduler for coding agents. nudge sends a prompt to an agent on a schedule (every 30 minutes, at 09:00,
or any cron pattern), and when the machine stays overloaded it tells every agent to pause heavy work and has
one of them find out why.

It reaches agents where they already run:

| Host     | Finds agents by                                                          | Sends with                                 |
| -------- | ------------------------------------------------------------------------ | ------------------------------------------ |
| `herdr`  | `herdr agent list` (status, folder, name, session)                       | `herdr agent prompt`                       |
| `cmux`   | each workspace's sidebar state (Claude Code, Codex, OpenCode, Gemini)    | a bracketed paste, then Enter              |
| `orca`   | `orca terminal list --json` (terminals with an agent identity)           | `orca terminal send --enter`               |
| `tmux`   | panes running a known agent (`claude`, `codex`, `opencode`, `gemini`, `pi`, `amp`, `aider`, `cursor-agent`, plus `NUDGE_AGENT_COMMANDS`) | a bracketed paste, then Enter |
| `claude` | no multiplexer: `claude -p` in a session (`--resume`) or folder (`--continue`) | a headless Claude Code turn            |

`host = "auto"` (the default) uses the first of herdr, cmux, orca and tmux that answers. A job with
`via = "claude"` skips the host and runs Claude Code directly.

A busy agent is waited for, never interrupted: the message goes out once it is idle. tmux cannot tell whether
an agent is busy, so there the message is pasted right away and the agent queues it. A target that matches
no agent, or more than one, is logged and retried; nothing is guessed.

## Install

```sh
brew install seethruhead/tap/nudge
brew services start nudge
```

Or build it: `cargo install --locked --path .`, then run `nudge run` under launchd or systemd (see below).

## Configure

`~/.config/nudge/config.toml` (or `$NUDGE_CONFIG`). nudge rereads it every few seconds, so edits apply
without a restart.

```toml
host = "auto"                                  # or herdr, tmux, cmux, orca

[[job]]
name = "ci-check"
every = "30m"                                  # at least 1m
target = { cwd = "/Users/me/code/app" }        # or { agent = "name" }, or { session = "id" }
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
via = "claude"                                 # run claude -p --resume <session>, no multiplexer
args = ["--permission-mode", "acceptEdits"]    # extra claude arguments
message = "Review today's commits for anything risky."

[[watch]]
name = "overload"
cpu = 85                                       # percent of the whole machine
memory = 92                                    # percent in use
load = 12                                      # 1-minute load average; about 1.2 x cores
for = "1m"                                     # how long the average must stay over (default 2m)
clear_for = "5m"                               # how long under before it clears (default 5m)
investigator = { cwd = "/Users/me/code/ops" }  # who finds out why; any target works
broadcast = true                               # tell every agent too (default true)
message = "Extra instructions for the investigator."
```

Each message starts with `[nudge: <job>, <schedule>]` so the agent knows where it came from. A `via = "claude"`
job waits while its previous run is still going, and appends Claude's output to
`~/.local/state/nudge/<job>.log`.

A watch averages the machine's CPU, memory and load over `for` and trips when any limit is over: every agent
is told what is using the machine and to pause heavy work, and the investigator is told to find out why and
fix it. Load matters most for "I can't click anything": a burst of test runs can queue far more work than
there are cores while CPU averages just under 85%. Memory is macOS's own figure (`kern.memorystatus_level`)
on a Mac and the share of RAM not available on Linux.

## Commands

```text
nudge run           read the config, send each job when it is due, check each watch
nudge send <job>    send one job's message now
nudge check         show the host, where each job goes and when it runs next, and how each watch stands
nudge --version
```

Binaries are found on `PATH`, `~/.local/bin`, `/opt/homebrew/bin` and `/usr/local/bin`; set `NUDGE_HERDR`,
`NUDGE_TMUX`, `NUDGE_CMUX`, `NUDGE_ORCA` or `NUDGE_CLAUDE` to choose one.

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
