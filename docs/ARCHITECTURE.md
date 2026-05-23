# Architecture

This document is the single source of truth for how the agent infrastructure on this host is wired. Skills cover *operations*; this doc covers *design*. When something is broken and the obvious answer is not in a skill, start here.

## Components

There are three components running on the host. Each does one thing, has its own failure domain, and can be restarted independently.

```
┌────────────────────────────────────────────────────────────────────────┐
│ Tim's Element (matrix.inblock.io)                                      │
└────────┬───────────────────────────────────────────┬───────────────────┘
         │ DM with heartbeat-identity                │ DM with claude-channel-identity
         │ (ops + #shell commands)                   │ (free-form prose → LLM)
         ▼                                           ▼
┌────────────────────────────────┐       ┌────────────────────────────────┐
│ aqua-matrix-heartbeat          │       │ aqua-matrix-claude-channel     │
│   ident: heartbeat.pem         │       │   ident: claude-channel.pem    │
│   store: ~/.aqua-matrix-       │       │   store: ~/.aqua-matrix-       │
│          heartbeat/            │       │          claude-channel/       │
│                                │       │                                │
│   sends: 10-min status DM      │       │   on incoming DM from target:  │
│   reads (event-driven sync):   │       │     spawn `claude -p <body>`   │
│     #shell <cmd>               │       │     stream stdout back as DM   │
│       → dispatcher             │       │                                │
│       → systemd unit op        │       │   stateless: each msg is a     │
│                                │       │   fresh claude invocation      │
│   #shell respawn ──────────────┼───────┤── targets claude-bridge OR     │
│                                │       │   aqua-matrix-claude-channel   │
└────────────────────────────────┘       └────────────────────────────────┘
                                                       │
                                                       │ (#shell respawn target)
                                                       ▼
                                         ┌────────────────────────────────┐
                                         │ claude-bridge.service          │
                                         │   tmux session `claude-bridge` │
                                         │   running:                     │
                                         │     claude --dangerously-      │
                                         │            skip-permissions    │
                                         │                                │
                                         │   for LOCAL interactive use:   │
                                         │     tmux attach -t claude-     │
                                         │            bridge              │
                                         │                                │
                                         │   NOT connected to Matrix.     │
                                         └────────────────────────────────┘
```

### Component roles

| Component | Purpose | Identity | Failure mode |
|---|---|---|---|
| `aqua-matrix-heartbeat` | Ops channel. Periodic status DMs + `#shell` commands. | `heartbeat.pem` | Heartbeat stops; commands stop. Claude side unaffected. |
| `aqua-matrix-claude-channel` | LLM channel. Free-form prose in, Claude output out. | `claude-channel.pem` | Conversational replies stop. Ops channel unaffected. |
| `claude-bridge.service` | Persistent interactive Claude in tmux, for local human attachment via `tmux attach`. | none (host-local process) | Local interactive Claude unavailable. Both Matrix daemons unaffected. |

## Why two Matrix identities (not one)

See the comparison in the chat history that led to this design. Summary:

- **State**: each daemon is stateless w.r.t. the other. No "mode" to track, no command/prose dispatcher.
- **Restart safety**: restarting one does not perturb the other. `#shell restart` of the heartbeat does not disrupt an in-flight `claude -p` invocation.
- **Heartbeat noise stays out of the LLM conversation**: 10-minute status payloads do not interleave with Claude replies.
- **Misroute risk**: zero. A DM to identity-A cannot be misread as a command for identity-B.
- **Element UX**: two distinct DM rooms in Tim's Element — mute, star, or notify each independently.
- **Cost**: one extra `.pem`, one extra OIDC client, one extra systemd unit. ~20 minutes of setup, paid once.

## Synchronization model (stream sync, not polling)

Both Matrix daemons use **matrix-sdk's continuous sync stream** rather than polling:

```rust
// at startup:
client.add_event_handler(|ev: OriginalSyncRoomMessageEvent, room: Room| async move { ... });
tokio::spawn(async move { client.sync(SyncSettings::default()).await });
```

Implications:

- Incoming DMs arrive within ~1 second (matrix-sdk's sync long-poll cadence), not after a 30s tick.
- Event handlers run on the sync task; long-running work (e.g. `claude -p`) is spawned as a separate tokio task so it does not block sync.
- The heartbeat timer is a third tokio task — independent of sync state.

Tradeoff: the daemon holds an open HTTP connection to the homeserver continuously. If the homeserver is unreachable, sync reconnects internally; the rest of the daemon stays alive.

## Identity and device-id persistence

Matrix `device_id` is server-assigned on every fresh OAuth login. matrix-sdk's SQLite crypto store binds to the `device_id` it was created with, so a `device_id` change against an existing store errors with `account in the store doesn't match`. We preserve `device_id` (and therefore the crypto store) across restarts using siwx-oidc's refresh-token grant.

**Three-tier resolution in `AgentClient::connect`** (`<store_dir>/config.toml` holds the cache):

1. **Cached access token still within TTL** → validate via `/whoami`, reuse. No network beyond the cheap whoami call.
2. **Access token expired but refresh_token present** → call `siwx_oidc_auth::refresh(server, client_id, refresh_token, did)` to mint a new access token. `device_id` is preserved across this exchange because the refresh grant is bound to the original login session. Persist the new access token (and rotated refresh token, if the server rotated) back to config.toml.
3. **No cached session, refresh failed, or refresh_token missing** → fresh `siwx_oidc_auth::authenticate(...)`. This is the only path that mints a new `device_id`. The fresh AuthTokens come with a new refresh_token (24h TTL on siwx-oidc standalone) which is persisted for next time.

If step 3 minted a new `device_id` and the existing SQLite store rejects it with "account in the store doesn't match", the connect code wipes `<store_dir>/matrix-sdk-*.sqlite3*` (NOT `config.toml`) and retries `restore_session` once. The previous `ExecStartPre=rm -f ...sqlite3*` workaround in the systemd unit is removed; the in-code path replaces it.

**Resulting behavior:**

| Time since last activity | `device_id` after restart? | Network calls |
|---|---|---|
| Within access-token TTL (~5 min) | Same | 1 `/whoami` |
| Within refresh-token TTL (24h) | Same | 1 `/token` (refresh grant) |
| Beyond refresh-token TTL | New (crypto store wiped + rebuilt) | Full OAuth code flow + cross-signing rebootstrap |

So practical operation — restarts within a single day, or even after a long weekend if the daemon was kept alive enough to refresh — preserves identity completely. Only multi-day downtime forces a fresh device. The daemon recovers automatically from any of these cases without manual intervention.

**Persisted fields in `SessionCache`** (`<store_dir>/config.toml` under `[session]`):

| Field | Purpose |
|---|---|
| `access_token` | Bearer token for the Matrix homeserver. Validated via `/whoami`. |
| `expires_at_unix` | When the access token expires. We trigger refresh 30s early. |
| `refresh_token` | siwx-oidc refresh-grant token (opaque `mcr_*`). 24h TTL on standalone. |
| `did` | Required by the refresh-grant call (siwx-oidc signature: `refresh(server, client_id, refresh_token, did)`). |
| `user_id` / `device_id` | Server-assigned; checked against `/whoami` to detect drift; required for `restore_session`. |

**Upstream dependency:** this design relies on `siwx-oidc-auth` shipping `AuthTokens.refresh_token` and `siwx_oidc_auth::refresh(...)`. Since commit `ab3ad3f` of the siwx-oidc repo these are available; we depend via `path = "../siwx-oidc/siwx-oidc-auth"` (sibling checkout) rather than a pinned git rev, because the newer commit's workspace also requires the `aqua-auth` crate at `../../aqua-auth` which cargo cannot fetch through a single git dep. To set up a fresh dev host: `git clone https://github.com/inblockio/siwx-oidc.git && git clone https://github.com/inblockio/aqua-auth.git` alongside this repo.

## Auto-respawn (always alive)

All three systemd units use `Restart=always` (Matrix daemons) or a supervisor loop (claude-bridge) so that any kill, crash, or clean exit triggers an automatic respawn:

| Unit | Mechanism | Crash → up in |
|---|---|---|
| `aqua-matrix-heartbeat` | `Restart=always`, `RestartSec=5s` + `StartLimitBurst=10 / IntervalSec=300` | ~5s |
| `aqua-matrix-claude-channel` | same | ~5s |
| `claude-bridge` | `Type=simple` bash supervisor that polls `tmux has-session` every 10s and re-runs `tmux new-session` if missing; plus `Restart=always` on the supervisor itself | ~10s (inner claude respawn) or ~5s (supervisor respawn) |

Acid-tested on this host: `kill -9 $(pid_of_heartbeat)` → systemd respawned in <5s, NRestarts=1, cached session reused (no auth storm). `tmux kill-session -t claude-bridge` → supervisor respawned the session on its next 10s tick.

Crash-loop guard: `StartLimitBurst=10` over `StartLimitIntervalSec=300` means systemd will give up after 10 restarts in 5 minutes — covers the case where the daemon is fundamentally broken (e.g. siwx-oidc unreachable) without burning through retries forever. After the limit, `systemctl --user reset-failed <unit>` re-enables restarts once the root cause is fixed.

## Auto-start chain (WSL boot → daemons up)

```
WSL distro starts
    └─ /etc/wsl.conf [boot] systemd=true
            └─ systemd PID 1 boots
                    └─ user@1000.service starts (linger=yes in loginctl)
                            └─ user systemd manager spawns
                                    ├─ aqua-matrix-heartbeat.service (enabled)
                                    ├─ aqua-matrix-claude-channel.service (enabled)
                                    └─ claude-bridge.service (enabled)
```

Verify on a fresh boot:

```bash
cat /etc/wsl.conf | grep -A1 boot           # systemd=true
loginctl show-user "$USER" | grep Linger    # Linger=yes
systemctl --user is-enabled aqua-matrix-heartbeat aqua-matrix-claude-channel claude-bridge
```

WSL itself does **not** start on Windows boot unless triggered (open a WSL terminal, or set up a Windows scheduled task running `wsl --distribution <name> --exec true` at logon). After WSL starts, everything above is automatic.

## File and identity inventory

| Path | Purpose | Created by |
|---|---|---|
| `agent.pem` | Free chat identity for ad-hoc `aqua-matrix-agent` runs. | Binary, on first chat run. |
| `agent-b.pem` | Second identity for `/e2e-test`. | E2E test setup. |
| `heartbeat.pem` | Heartbeat daemon identity. | Renamed from auto-generated `agent.pem` during initial setup. |
| `claude-channel.pem` | Claude-channel daemon identity. | Auto-generated on first `--claude-channel` run. |
| `~/.aqua-matrix-heartbeat/` | Heartbeat sync state + config + session cache. | Heartbeat daemon. |
| `~/.aqua-matrix-claude-channel/` | Claude-channel sync state + config + session cache. | Claude-channel daemon. |
| `~/.config/systemd/user/aqua-matrix-heartbeat.service` | Heartbeat unit. | Operator install (`cp` from `systemd/`). |
| `~/.config/systemd/user/aqua-matrix-claude-channel.service` | Claude-channel unit. | Operator install. |
| `~/.config/systemd/user/claude-bridge.service` | Interactive Claude tmux unit. | Operator install. |

All `.pem` files are gitignored (`*.pem` in `.gitignore`) — they ARE the identities, must not be checked in.

## Troubleshooting decision tree

**The ops channel went silent / no heartbeat in 10+ min**

1. `systemctl --user is-active aqua-matrix-heartbeat` — expect `active`. If `failed`, check `journalctl --user -u aqua-matrix-heartbeat -n 50`.
2. If `activating` indefinitely: cached session has expired AND the store-wipe-retry path is failing for some other reason. Manual remediation: `rm ~/.aqua-matrix-heartbeat/matrix-sdk-*.sqlite3* && rm ~/.aqua-matrix-heartbeat/config.toml && systemctl --user restart aqua-matrix-heartbeat`. (Removing `config.toml` forces a fully fresh start including OIDC re-registration.)
3. If `active` but no heartbeat: matrix sync may be stuck. `journalctl ... -f` will show sync errors. Send `#shell ping` from Tim's account — if no reply, the daemon is wedged. `systemctl --user restart aqua-matrix-heartbeat` forces a fresh stream sync.

**The Claude channel does not respond**

1. `systemctl --user is-active aqua-matrix-claude-channel` — same triage as heartbeat.
2. Test `claude -p hi` locally: if claude itself is broken, daemon cannot reply either.
3. If daemon is active but messages do not get answers: check `journalctl --user -u aqua-matrix-claude-channel -n 30`. Look for `claude -p` spawn failures or sync errors.
4. From Tim's account, send `#shell respawn` to the **heartbeat** identity. The `respawn` verb restarts `claude-bridge.service` (the local tmux), NOT the claude-channel daemon. To restart the Matrix-side claude channel: `systemctl --user restart aqua-matrix-claude-channel` locally, or extend the `#shell` dispatcher with a new verb.

**`device_id` keeps rotating, store keeps wiping**

This should NOT happen routinely — the refresh-token grant covers up to 24h of downtime while preserving `device_id`. If it is happening on every restart:

1. Check the session cache has a refresh_token: `grep refresh_token <store_dir>/config.toml`. If missing, the daemon was authenticated against an older siwx-oidc that didn't issue them — restart will fix on the next fresh auth.
2. Check `did` is also persisted (`grep '^did ' <store_dir>/config.toml`). Both `refresh_token` AND `did` are needed for the refresh-grant call.
3. Check the journal for `refresh grant failed`. If the refresh token expired (>24h since issue), fresh auth is the only path and will mint a new device.

For genuine multi-day downtime, expect a single fresh-auth + cross-signing rebootstrap. That is the design boundary, not a bug.

**`#shell` commands do nothing**

1. Verify the sender matches `--target` of the heartbeat unit (defaults to Tim). The dispatcher only honors commands from the configured target — DMs from other accounts are silently ignored by design.
2. Verify the prefix is exactly `#shell` (case-insensitive on the prefix). `/help`, `!help`, etc. are NOT recognized — that was the whole point of the rename.
3. `#shell logs 30` from Tim's account dumps the journal — useful for diagnosing the daemon from inside Matrix without local shell access.

**Local interactive `claude-bridge` died but Matrix daemons fine**

`systemctl --user restart claude-bridge` or `#shell respawn` from Tim. The interactive tmux session is decoupled from the Matrix daemons; their lifecycles do not affect each other.

## Adding a fourth surface

To add another Matrix-visible agent surface (e.g. a calendar bot, a code-review bot): generate a fresh `.pem`, register a fresh OIDC client (auto on first run), write a new mode in `src/`, ship a new systemd unit, enable it. Reuse the session-cache + event-handler patterns. Do NOT multiplex onto an existing identity — the whole point of this architecture is that each surface owns its channel.
