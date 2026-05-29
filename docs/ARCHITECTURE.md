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

## Workspace layout

The repo is a Cargo workspace (virtual root manifest) and a **reference implementation** for any agent backend over Matrix + siwx-oidc — the `claude -p` daemon is just a placeholder backend. Four crates live under `crates/`:

| Crate | Role |
|---|---|
| `aqua-matrix-agent` | The library (`AgentClient`, `AgentConfig`, OIDC, recovery, registry) plus the one-shot `aqua-matrix-agent` CLI binary (`--message` / `--read` / `--print-did`). |
| `aqua-matrix-relay` | Generic daemon framework: the `MessageHandler` trait + `run_daemon()` lifecycle (connect-rotate-sync-watermark). Ships `examples/echo_agent.rs`. |
| `aqua-matrix-heartbeat` | Binary `aqua-matrix-heartbeat` — the ops/heartbeat agent (periodic status DM + `#shell` commands). |
| `aqua-matrix-claude-p` | Binary `aqua-matrix-claude-p` — the reference example backend that forwards DMs to `claude -p`. |

A single `cargo build` builds the whole workspace at once.

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

## DM rooms as server-side state (`m.direct`)

When the agent sends a DM it also marks the room as direct in the `m.direct`
global account data map (`{ user_id: [room_id, ...] }`). This means the DM
relationship lives in server-side account data, not only in local crypto/sync
state:

- The DM room resolves from the homeserver on a cold start — even after a full
  crypto-store wipe (tier-3 fresh auth, see "Identity and device-id
  persistence") — instead of the agent creating a fresh, duplicate DM room.
- Element shows the room under "People" / direct chats rather than as a generic
  room.

`m.direct` is written best-effort and idempotently (the existing entry is read,
the room id added if absent, the map written back); it does not gate message
sending.

## Agent self-registry (`io.inblock.aqua.registry`)

Each daemon advertises itself in its own global account data under the custom
type `io.inblock.aqua.registry`, so the fleet can be enumerated by reading
account data rather than scraping logs or systemd. The payload:

```jsonc
{
  "did":          "did:...",                    // the agent's decentralized identifier
  "user_id":      "@...:matrix.inblock.io",      // server-assigned Matrix user id
  "role":         "heartbeat" | "claude-channel",// which surface this identity serves
  "systemd_unit": "aqua-matrix-heartbeat",       // managing unit (None for ad-hoc runs)
  "last_online":  1700000000,                    // unix seconds, refreshed on each upsert
  "version":      "..."                          // crate version
}
```

`AgentClient::update_registry(role, systemd_unit)` performs the upsert. Refresh
cadence:

- **heartbeat**: every tick (and on each client-cycle start), so `last_online`
  tracks the heartbeat interval.
- **claude-channel**: on each client-cycle start. The channel has no periodic
  tick of its own, so `last_online` is refreshed at the token-rotation cadence
  (≈270s).

The upsert is always best-effort at the call site — a registry write failure is
logged at `warn` and never crashes or skips the daemon's primary job.

## Benign `404 Account data not found` log lines on a fresh account

On a brand-new account the matrix-sdk `http_client` logs `ERROR` lines like
`404 ... Account data not found` for `m.direct` and
`m.secret_storage.default_key` during initial sync. These are harmless: the SDK
is probing for account data that has not been written yet.

- The `m.direct` 404 stops once the agent marks its first DM room (see "DM rooms
  as server-side state").
- The `m.secret_storage.default_key` 404 stops once SSSS is enabled (see
  RECOVERY.md § "Secure Secret Storage").

We intentionally do **not** filter the SDK's `http_client` logging to suppress
these, because that same logger surfaces genuine errors — silencing it would
hide real problems to cosmetically remove two expected first-run probes.

## Auto-respawn

All three units are designed to be "always alive" via `Restart=always` (Matrix daemons) or a bash supervisor loop (claude-bridge). Per-unit mechanism, recovery times, manual procedures, and the diagnostic decision tree all live in [`RECOVERY.md`](RECOVERY.md) — start there when something is broken.

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
| `claude-channel.pem` | Claude-channel daemon identity. | Auto-generated on first `aqua-matrix-claude-p` run. |
| `~/.aqua-matrix-heartbeat/` | Heartbeat sync state + config + session cache. | Heartbeat daemon. |
| `~/.aqua-matrix-claude-channel/` | Claude-channel sync state + config + session cache. | Claude-channel daemon. |
| `~/.config/systemd/user/aqua-matrix-heartbeat.service` | Heartbeat unit. | Operator install (`cp` from `systemd/`). |
| `~/.config/systemd/user/aqua-matrix-claude-channel.service` | Claude-channel unit. | Operator install. |
| `~/.config/systemd/user/claude-bridge.service` | Interactive Claude tmux unit. | Operator install. |

All `.pem` files are gitignored (`*.pem` in `.gitignore`) — they ARE the identities, must not be checked in.

## Troubleshooting

Moved to [`RECOVERY.md`](RECOVERY.md) — its "Diagnostic decision tree" section covers the canonical symptoms (silent ops channel, claude channel not responding, device_id rotating, `#shell` commands ignored, bridge tmux missing). RECOVERY.md is the runbook; this document is the design rationale.

## Adding a fourth surface

To add another Matrix-visible agent surface (e.g. a calendar bot, a code-review bot): implement the `MessageHandler` trait in a new crate (or as an example) and call `run_daemon()` from `aqua-matrix-relay` — that gives you the connect-rotate-sync-watermark lifecycle for free, so you only write the per-message logic. Then generate a fresh `.pem`, register a fresh OIDC client (auto on first run), ship a new systemd unit, and enable it. See [`docs/REFERENCE.md`](REFERENCE.md) and `crates/aqua-matrix-relay/examples/echo_agent.rs` for a minimal working handler. Do NOT multiplex onto an existing identity — the whole point of this architecture is that each surface owns its channel.
