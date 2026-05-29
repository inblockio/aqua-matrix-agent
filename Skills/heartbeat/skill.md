---
name: heartbeat
description: Run aqua-matrix-agent as a heartbeat + Matrix command channel (deterministic, no LLM)
---

# Heartbeat + Command Channel

The `aqua-matrix-heartbeat` binary is a dual-purpose daemon — see [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) for the full design:

1. **Heartbeat (outbound)**: every N seconds (default 600 = 10 min) it sends a status DM to `--target` containing agent, host, and Claude Code session facts.
2. **Command channel (inbound)**: stream sync (matrix-sdk `client.sync()` in a background tokio task) plus an event handler delivers messages from `--target` within ~1s of arrival. Messages starting with `#shell` are parsed as commands and answered deterministically — no LLM involved.

## Command list

Sent as plain Matrix DMs from the configured `--target` to the heartbeat's identity. Prefix is `#shell` (chosen instead of `/` to avoid collisions with messengers that have their own `/command` palettes). Matching is case-insensitive on the prefix.

| Command | Reply |
|---|---|
| `#shell help` *(or just `#shell`)* | List of supported commands |
| `#shell status` | Same payload as a scheduled heartbeat — sent immediately |
| `#shell ping` | `pong @ <UTC timestamp>` |
| `#shell uptime` | Agent loop uptime + host uptime |
| `#shell restart` | Acks, then spawns `systemctl --user restart aqua-matrix-heartbeat`. systemd kills the running daemon and starts a fresh one. |
| `#shell respawn` | Restart the LOCAL interactive Claude in tmux: `systemctl --user restart claude-bridge`. For local `tmux attach`, not Matrix replies. |
| `#shell respawn-channel` | Restart the Matrix LLM channel daemon: `systemctl --user restart aqua-matrix-claude-channel`. |
| `#shell logs [N]` | Last N lines from `journalctl --user -u aqua-matrix-heartbeat` (default 10, max 50) |

**Security**: only messages whose sender matches `--target` are honored. Anyone else who DMs the heartbeat identity is ignored. Anything not matching a known subcommand yields `unknown command: ...` plus the help text.

**Watermark**: at startup the daemon sets a high-water timestamp to "now", so commands sent before the daemon came online are not replayed. After every restart, the watermark resets — there is no on-disk command queue.

**`#shell restart` safety**: because the watermark is initialized after startup, the freshly-started daemon will NOT see the original `#shell restart` message and will not loop.

**Restart reliability**: handled in code, not via systemd. `AgentClient::connect` caches the access token + device_id in `~/.aqua-matrix-heartbeat/config.toml` and reuses them on restart if still valid (~5 min window). If the cached token has expired, fresh auth mints a new device_id; if that triggers `account in the store doesn't match`, the connect code wipes `matrix-sdk-*.sqlite3*` and retries `restore_session` once. No more `ExecStartPre` wipe. See `docs/ARCHITECTURE.md` "Identity and device-id persistence".

**Sync model**: stream sync, not polling. `client.sync(...)` runs forever in a background tokio task; a registered event handler dispatches incoming `#shell` commands within ~1 second of receipt (not a 30s tick).

## Quick start (foreground)

```bash
cd ~/aqua-matrix-hello
./target/debug/aqua-matrix-heartbeat
```

Stops on Ctrl+C / SIGTERM. Send failures are logged and retried next tick — the daemon does not crash on transient errors.

## Recipient and interval

```bash
# Different recipient
./target/debug/aqua-matrix-heartbeat --target "@user:matrix.inblock.io"

# 5-minute interval instead of 10
./target/debug/aqua-matrix-heartbeat --interval 300
```

## Auto-start with WSL

When WSL starts, this chain brings the heartbeat online automatically:

1. `/etc/wsl.conf` has `[boot] systemd=true` → WSL boots systemd inside the distro
2. `loginctl enable-linger <user>` → the user systemd manager starts at boot, before any interactive login
3. `systemctl --user enable aqua-matrix-heartbeat` → the unit starts when the user manager comes up

Verify the chain on the host:

```bash
cat /etc/wsl.conf | grep -A1 boot       # expect systemd=true
loginctl show-user "$USER" | grep Linger # expect Linger=yes
systemctl --user is-enabled aqua-matrix-heartbeat  # expect enabled
```

WSL itself does NOT start automatically when Windows boots — open any WSL terminal once or set up a Windows scheduled task running `wsl --distribution <name> --exec true` at logon if you need that. After WSL is running, the unit comes up without further user action.

## Persistent install (systemd user unit)

The unit ships in the repo at `systemd/aqua-matrix-heartbeat.service`. Install and enable:

```bash
mkdir -p ~/.config/systemd/user
cp ~/aqua-matrix-hello/systemd/aqua-matrix-heartbeat.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now aqua-matrix-heartbeat
loginctl enable-linger "$USER"   # so it keeps running after logout
```

Check on it:

```bash
systemctl --user status aqua-matrix-heartbeat
journalctl --user -u aqua-matrix-heartbeat -f
```

Disable / remove:

```bash
systemctl --user disable --now aqua-matrix-heartbeat
rm ~/.config/systemd/user/aqua-matrix-heartbeat.service
```

The unit uses a dedicated identity (`heartbeat.pem` + `~/.aqua-matrix-heartbeat/` store) so it doesn't collide with the chat identity at `agent.pem`. `Environment=CONTEXT_WINDOW=1000000` matches the Opus 4.7 1M-context window — adjust if you switch models.

If you ever re-auth the heartbeat identity and the unit fails with `account in the store doesn't match the account in the constructor`, wipe `~/.aqua-matrix-heartbeat/matrix-sdk-*.sqlite3*` (keep `config.toml`) and restart the unit. The siwx-oidc flow issues a new `device_id` on each auth, and the SQLite crypto store binds to the previous one.

## Status payload format

Each heartbeat is plaintext with three rows after the timestamp:

```
aqua-matrix-agent heartbeat @ 2026-05-23 09:00:00Z
----------------------------------------
agent : up 1h23m, sent 8
host  : my-host | up 2d3h | load 0.34 0.42 0.45 | mem 12.3/16.0GB free (23% used) | disk 234G free (12% used)
claude: -home-user-aqua-matrix-hello | ctx ~38% of 1M (claude-opus-4-7) | session b1865bef | last_tool: Bash | last_user: "build the binary"
```

| Row | Source |
|---|---|
| agent | `HeartbeatStats` struct: loop start time, count of successful sends, last error |
| host | `/proc/sys/kernel/hostname`, `/proc/uptime`, `/proc/loadavg`, `/proc/meminfo`, `df -BG /` |
| claude | Most recently modified `~/.claude/projects/*/*.jsonl` — extracts model, latest `usage.input_tokens`, most recent `tool_use.name`, and last user message |

The `claude` row may be omitted if no transcript with usage data is found (e.g. on a fresh machine).

## Tuning

- **Threshold for "ctx ~X%"**: derived from the Opus 4.7 1M variant (`CONTEXT_WINDOW=1000000`). Transcripts log the model as `claude-opus-4-7` without the `[1m]` suffix, so the env var override is the only reliable signal for the larger window. The systemd unit sets this — for foreground runs, export it yourself if needed.
- **Interval**: pass `--interval <seconds>`. The systemd unit doesn't override it (uses the binary default 600).
- **Recipient**: pass `--target` or run multiple agent identities via `--key-file` + `--store-dir`.

## Troubleshooting

- **No `claude:` row**: No `*.jsonl` under `~/.claude/projects/` has usage info yet, or the home dir differs (set `HOME` correctly in the systemd unit).
- **`host: ... | disk ?`**: `df` is missing or `/` not mounted normally. The other host fields fall back individually.
- **`heartbeat send failed`** logs but no message arrives: check `journalctl --user -u aqua-matrix-heartbeat -n 50`, often it's `siwx-oidc` token expiry or sync trouble; the loop will keep trying.
- **High send latency**: each tick does a `sync_once()` before sending. If your Matrix homeserver is slow this stretches the cadence. The interval is "sleep between ticks", not "exact wall-clock cadence".
