# nudge

Sends messages to coding agents running in Herdr panes: on a schedule (jobs), and when this machine is
overloaded (watches), so a session can be told to check on something every so often, and every session
can be told to back off when the computer is struggling. One small Rust binary run by launchd or systemd; it
talks to Herdr through `herdr agent list` and `herdr agent prompt`.

- `nudge run` reads `~/.config/nudge/config.toml` every 30 seconds and sends each job's message when
  it is due. The first send is one interval after start. A busy agent (working or blocked) is waited
  for, never interrupted; the message goes out as soon as it is idle.
- Every 5 seconds it also checks each watch. When CPU or memory stays over a watch's limit for `for`, the
  watch trips: every agent on the machine is told what is using it and to pause heavy work, and the
  investigator (if set) is told to find out why and fix it. Once usage stays back under for `clear_for`,
  everyone is told it has cleared. These messages go to busy agents too (the busy one is usually the cause);
  one that is blocked gets it as soon as it can take input, or is given up on after 15 minutes. Memory is
  macOS's own figure (`kern.memorystatus_level`, as `memory_pressure` shows) on a Mac, and the share of RAM
  not available on Linux.
- `nudge send <job>` sends one job's message now. `nudge check` shows where each job would go and how each
  watch stands right now.

```toml
[[job]]
name = "encode-watch"
every = "30m"
target = { cwd = "/Users/sth/code/homelab" }   # or { agent = "name" }, or { session = "id" }
message = "Check that encoding is running."
```

```toml
[[watch]]
name = "overload"
cpu = 85                                           # percent of the whole machine; either limit is optional
memory = 92                                        # percent in use
for = "2m"                                         # how long it must stay over before tripping (default 2m)
clear_for = "5m"                                   # how long under before it clears (default 5m)
investigator = { cwd = "/Users/sth/code/homelab" } # who finds out why; any target works
broadcast = true                                   # tell every agent too (default true)
message = "Extra instructions for the investigator."
```

A target that matches no agent, or more than one, is logged and retried; nothing is guessed.

Install: `cargo build --release`, copy `target/release/nudge` to `~/.local/bin`, then
`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/vip.sthloc.nudge.plist`. Logs go to
`~/Library/Logs/nudge.log`.

On Linux, copy the binary to `~/.local/bin` and run it as a systemd user service:

```ini
# ~/.config/systemd/user/nudge.service
[Unit]
Description=nudge

[Service]
ExecStart=%h/.local/bin/nudge run
Restart=always

[Install]
WantedBy=default.target
```

then `systemctl --user enable --now nudge` (and `loginctl enable-linger $USER` to keep it running without a
login). `journalctl --user -u nudge` shows its log.
