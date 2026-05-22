---
name: heartbeat
description: Run aqua-matrix-agent as a heartbeat daemon that DMs system status every 10 minutes
---

# Heartbeat Daemon

`--heartbeat` puts the agent into a loop: every N seconds (default 600 = 10min) it syncs Matrix and sends a status DM to `--target` containing agent, host, and Claude Code session facts.

## Quick start (foreground)

```bash
cd ~/aqua-matrix-hello
./target/release/aqua-matrix-agent --heartbeat
```

Stops on Ctrl+C / SIGTERM. Send failures are logged and retried next tick — the daemon does not crash on transient errors.

## Recipient and interval

```bash
# Different recipient
./target/release/aqua-matrix-agent --heartbeat --target "@user:matrix.inblock.io"

# 5-minute interval instead of 10
./target/release/aqua-matrix-agent --heartbeat --heartbeat-interval 300
```

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
- **Interval**: pass `--heartbeat-interval <seconds>`. The systemd unit doesn't override it (uses the binary default 600).
- **Recipient**: pass `--target` or run multiple agent identities via `--key-file` + `--store-dir`.

## Troubleshooting

- **No `claude:` row**: No `*.jsonl` under `~/.claude/projects/` has usage info yet, or the home dir differs (set `HOME` correctly in the systemd unit).
- **`host: ... | disk ?`**: `df` is missing or `/` not mounted normally. The other host fields fall back individually.
- **`heartbeat send failed`** logs but no message arrives: check `journalctl --user -u aqua-matrix-heartbeat -n 50`, often it's `siwx-oidc` token expiry or sync trouble; the loop will keep trying.
- **High send latency**: each tick does a `sync_once()` before sending. If your Matrix homeserver is slow this stretches the cadence. The interval is "sleep between ticks", not "exact wall-clock cadence".
