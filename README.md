# nudge

Sends a message to a coding agent running in a Herdr pane on a schedule, so a session can be told to
check on something every so often. One small Rust binary run by launchd; it talks to Herdr through
`herdr agent list` and `herdr agent prompt`.

- `nudge run` reads `~/.config/nudge/config.toml` every 30 seconds and sends each job's message when
  it is due. The first send is one interval after start. A busy agent (working or blocked) is waited
  for, never interrupted; the message goes out as soon as it is idle.
- `nudge send <job>` sends one job's message now. `nudge check` shows where each job would go.

```toml
[[job]]
name = "encode-watch"
every = "30m"
target = { cwd = "/Users/sth/code/homelab" }   # or { agent = "name" }, or { session = "id" }
message = "Check that encoding is running."
```

A target that matches no agent, or more than one, is logged and retried; nothing is guessed.

Install: `cargo build --release`, copy `target/release/nudge` to `~/.local/bin`, then
`launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/vip.sthloc.nudge.plist`. Logs go to
`~/Library/Logs/nudge.log`.
