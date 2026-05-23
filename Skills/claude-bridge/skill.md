---
name: claude-bridge
description: Persistent claude-ws (Claude Code) session in a tmux session managed by systemd, respawnable via #shell respawn
---

# Claude Bridge (local interactive Claude in tmux)

A long-lived `claude --dangerously-skip-permissions` process running inside a detached tmux session named `claude-bridge`, supervised by the `claude-bridge.service` systemd user unit. The heartbeat's `#shell respawn` command restarts it remotely. For Matrix-routed Claude conversations see the separate `/claude-channel` skill — this skill is about the LOCAL interactive session you can `tmux attach` to. See [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the full layout.

## Quick check

```bash
systemctl --user is-active claude-bridge   # expect: active
tmux ls                                    # expect: claude-bridge: 1 windows (created ...)
```

Attach interactively (use `Ctrl+b d` to detach without killing):

```bash
tmux attach -t claude-bridge
```

## Install

```bash
cp ~/aqua-matrix-hello/systemd/claude-bridge.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now claude-bridge
```

The same wsl.conf-systemd + linger chain that auto-starts the heartbeat covers this unit too — verified by `systemctl --user is-enabled claude-bridge` returning `enabled`.

## Respawn

```bash
# From any local shell
systemctl --user restart claude-bridge

# Remotely via Matrix DM from the configured --target identity
#shell respawn
```

Both paths run `ExecStop` (`tmux kill-session -t claude-bridge`) followed by `ExecStart` (`tmux new-session -d -s claude-bridge -- ~/.local/bin/claude --dangerously-skip-permissions`), giving a clean Claude Code process.

## Unit anatomy

`systemd/claude-bridge.service`:

| Directive | Value | Why |
|---|---|---|
| `Type=oneshot` + `RemainAfterExit=yes` | | `tmux new-session -d` exits as soon as the session is detached; oneshot reports success and stays active. |
| `WorkingDirectory=%h/aqua-matrix-hello` | | Claude reads `.claude/` from cwd up; this puts it in the project. Change to `%h` if you want a workspace-agnostic bridge. |
| `ExecStart=/usr/bin/tmux new-session -d -s claude-bridge -- %h/.local/bin/claude --dangerously-skip-permissions` | | Absolute path to claude — systemd's PATH does not include `~/.local/bin`. |
| `ExecStop=/usr/bin/tmux kill-session -t claude-bridge` | | Lets `systemctl restart` cycle cleanly. |

## Failure modes

- **Bridge `active` but tmux session missing**: should no longer happen — the unit now uses a `Type=simple` supervisor that polls `tmux has-session` every 10s and re-runs `tmux new-session` if missing (so the inner claude exiting / `/quit` / crash respawns on its own within ~10s). If you see this anyway, check `journalctl --user -u claude-bridge -n 30` for `spawn failed` lines and the cause (PATH, claude binary missing, etc).
- **`tmux: failed to connect to server`**: tmux server died. Restart the unit; it spawns a new server transparently.
- **`claude: command not found` in journal**: PATH issue. Verify `which claude` resolves and update the unit's `ExecStart` if the binary moved.
