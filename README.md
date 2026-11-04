# nudge

A scheduler for coding agents. nudge sends a prompt to an agent on a schedule (every 30 minutes, at 09:00,
or any cron pattern), and when the machine stays overloaded it tells every busy agent to pause heavy work and has
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
broadcast = true                               # tell every busy agent too (default true)
message = "Extra instructions for the investigator."

[park]                                         # Herdr only; leave the table out to never park
after = "30m"                                  # idle or done this long (default 30m)
skip = [{ cwd = "/Users/me/code/ops" }]        # never park these; investigators are never parked either

[background]                                   # macOS; on unless enabled = false
enabled = true

[freeze]                                       # macOS; on unless enabled = false; defaults shown
active_within = "2m"                           # keyboard or mouse used this recently: someone is at the Mac
load = 10                                      # 1-minute load average; defaults to this machine's core count
memory_free = 20                               # percent
thaw_after = "1m"                              # under both limits this long before thawing
also = ["ffmpeg"]                              # added to the heavy list; heavy = [...] replaces it
```

Each message starts with `[nudge: <job>, <schedule>]` so the agent knows where it came from. A `via = "claude"`
job waits while its previous run is still going, and appends Claude's output to
`~/.local/state/nudge/<job>.log`.

A watch averages the machine's CPU, memory and load over `for` and trips when any limit is over: every busy
agent is told what is using the machine and to pause heavy work, and the investigator is told to find out why
and fix it. Idle agents are left alone, since they are not running anything, and only the agents that were
told to pause hear that it has cleared. Load matters most for "I can't click anything": a burst of test runs can queue far more work than
there are cores while CPU averages just under 85%. Memory is macOS's own figure (`kern.memorystatus_level`)
on a Mac and the share of RAM not available on Linux.

## Park idle Claude agents

An idle Claude Code session still holds a few hundred MB. With `[park]`, an agent in Herdr that has been
`idle` or `done` for `after` is parked: nudge records its pane, name, folder and session in
`~/.local/state/nudge/park.json`, sends `/exit`, waits for the shell, and runs a small placeholder in the pane
(a copy of nudge named `claude` in `~/.local/share/nudge/parked`, a few MB). Herdr keeps listing it as an idle
claude agent with the same name and session, so targets still match it.

The placeholder shows `✳ <name> (parked)` and a prompt box. Enter resumes the session (`claude --resume <id>`
through your interactive zsh, so a `claude` shell function still adds its flags); a message typed or sent with
`herdr agent prompt` resumes it with that message as the first prompt. So a job, another agent or the overload
investigator wakes a parked agent just by prompting it. When Claude exits, the pane is back at its shell.
Ctrl-C leaves the placeholder and prints the `claude --resume` line.

An agent is never parked while it is working or blocked, while its Claude process still runs a shell (a
background command, a dev server, a test run, which Claude always starts through a shell), while text is in
its prompt box, or when `skip` or a watch's
investigator names it. Other children of Claude, such as the MCP and language servers it starts from
`.mcp.json` and plugins, do not count; any still running once Claude has exited are closed. Overload warnings and all-clears skip parked agents, since they are not using the machine.

## Keep agents' work in the background (macOS)

A Herdr pane shell can put itself in the background (`taskpolicy -b -p $$` in `.zshrc`), so agents and what
they start yield the CPU to whoever is using the machine. Some launchers drop that policy for what they spawn
(a vitest run under bun came out at normal priority, with its Chrome). `nudge run` checks every process under
each Herdr server every 5 seconds and puts any that is not in the background back in it
(`setpriority(PRIO_DARWIN_PROCESS, pid, PRIO_DARWIN_BG)`, what `taskpolicy -b -p` does), logging what it moved.
`nudge check` shows how many processes are under Herdr and which are above background base priority now.
It checks each task's base priority: `ps` shows a process's busiest thread, and a thread in a background
process can run higher for a moment while the kernel boosts it to serve a higher-priority waiter, which is
not an escape. A Herdr
server is a `herdr` process whose parent is launchd; the server itself and anything outside it are left alone.

## Freeze heavy work while someone is using the Mac (macOS)

Background priority is not always enough to keep the Mac responsive. While someone is at the Mac (keyboard
or mouse used within `active_within`, from `HIDIdleTime`) and the 1-minute load is over `load` or free memory
is under `memory_free`, `nudge run` stops (SIGSTOP) the heavy processes under each Herdr server and everything
they started: vitest, tsc, tsgo, tsgolint, oxlint, Playwright, Chrome, Chromium and chrome-headless-shell,
cargo, rustc, clippy-driver, `bun test` and moon. It continues them (SIGCONT) once nobody has used the Mac for
`active_within`, or load and memory have stayed under the limits for `thaw_after`. Claude, herdr, shells and
nudge itself are never stopped, nor anything outside Herdr. Frozen work just waits, but a command with a
wall-clock timeout (a Claude Bash call, `timeout`) can run out while frozen.

Each freeze and thaw is logged and shown in `nudge check`. What is frozen is kept in
`~/.local/state/nudge/freeze.json`: `nudge run` thaws it when it stops (TERM, INT or HUP) and when it starts,
so a crash leaves nothing stopped for long, and `nudge thaw` continues everything by hand.

## Commands

```text
nudge run           read the config, send each job when it is due, check each watch
nudge send <job>    send one job's message now
nudge check         show the host, where each job goes and when it runs next, how each watch stands,
                    which agents are parked and when each resting agent parks,
                    and which processes under Herdr are not in the background
nudge wake <agent>  resume a parked agent (by name or pane) with no message
nudge thaw          continue every process nudge froze
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
