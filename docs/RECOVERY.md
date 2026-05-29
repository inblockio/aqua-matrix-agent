# Recovery Behavior

How the agent infrastructure on this host survives crashes, restarts, network blips, server outages, and identity drift. Read this when something is broken or before changing recovery-critical config. For the *design* (why three daemons, why stream sync, identity model) see [`ARCHITECTURE.md`](ARCHITECTURE.md).

## At a glance — what recovers without human intervention

| Scenario | Recovery | Time |
|---|---|---|
| `kill -9` on a Matrix daemon | systemd respawns (`Restart=always`) | ~5s |
| Matrix daemon panics / OOM | same | ~5s |
| Matrix daemon exits cleanly (shouldn't, defensive) | same | ~5s |
| Claude inside `claude-bridge` tmux exits (`/quit`, crash, OOM) | Supervisor's 10s tick re-runs `tmux new-session` | ≤10s |
| `claude-bridge` supervisor itself dies | systemd respawns the supervisor, which respawns tmux | ~5s |
| Matrix sync stream errors (homeserver reachable but transient) | matrix-sdk-internal retry | seconds |
| Matrix sync stream errors (homeserver/network down for a while) | inner cycle's sync future returns Err → outer loop reconnects (tier-2 refresh) | seconds after server is back |
| Access token nears expiry while daemon is running | daemon proactively rotates the matrix-sdk Client `REFRESH_GUARD_SECS` (30s) before `expires_at_unix`. Tier-2 refresh-grant in `AgentClient::connect` mints a new access token, `device_id` preserved, crypto store untouched. No 401 reaches the server | TTL − 30s, no operator-visible disruption |
| WSL reboots | wsl.conf `systemd=true` + linger + units `enabled` brings all three up | seconds after WSL itself starts |
| Crypto store wiped but `recovery.key` present | Cold start restores cross-signing/secrets from the persisted recovery key via SSSS — no new identity, no fresh cross-signing bootstrap | next start |

## At a glance — what requires human intervention

| Scenario | Why | What to do |
|---|---|---|
| Restart-loop guard exceeded (`StartLimitBurst=10` in `StartLimitIntervalSec=300`) | Daemon failed >10 times in 5 minutes — usually means a real upstream problem | Fix root cause, then `systemctl --user reset-failed <unit>` to re-enable restarts |
| Refresh token expired (>24h since last refresh-grant) | Standalone siwx-oidc TTL boundary | Nothing — next start does a fresh OAuth code flow, mints a new `device_id`, and the in-code store-mismatch handler wipes + rebuilds the SQLite crypto store. Tim sees one cross-signing rebootstrap. |
| siwx-oidc server permanently down | Cannot authenticate at all | Fix siwx-oidc, then `systemctl --user reset-failed` if guard tripped |
| Disk full | SQLite writes fail | Free space, then `systemctl --user restart <unit>` |
| All `.pem` files lost | Identities derive from these | Accept the new identity (binary auto-generates), notify counterparties (they see a new Matrix user) |
| Crypto store AND `recovery.key` both lost | SSSS has nothing to restore from | A brand-new cross-signing identity is bootstrapped; Tim sees one re-verification. Back up `recovery.key` to avoid this. |

## Component-by-component

### `aqua-matrix-heartbeat.service`

- `Restart=always`, `RestartSec=5s`, crash-loop guard `StartLimitBurst=10 / IntervalSec=300` (in `[Unit]`, not `[Service]`)
- **Outer loop owns AgentClient lifecycle.** Each iteration:
  1. `AgentClient::connect()` runs the three-tier session resolution (see below)
  2. Inner cycle (`run_cycle`) registers the message handler, spawns `client.sync()`, runs heartbeat ticks. Returns when **either** the refresh deadline (`expires_at_unix − REFRESH_GUARD_SECS`) is reached **or** the sync future ends (network blip, fatal error)
  3. Outer loop drops the AgentClient and reconnects. matrix-sdk has no public API to swap an access token in place, so we rotate the whole Client instead — `device_id` is preserved through the rebuild via tier-2.
- Three-tier session resolution inside `connect()`:
  1. cached access token still valid → reuse (one `/whoami` call, ~50 ms)
  2. cached refresh_token + did → `siwx_oidc_auth::refresh()` → new access token, **same `device_id`**, crypto store untouched (one `/token` call)
  3. neither → fresh OAuth code flow → **new `device_id`**, store-mismatch handler wipes `matrix-sdk-*.sqlite3*` and retries `restore_session` once, cross-signing re-bootstraps
- Steady state on a 300 s siwx-oidc token TTL: tier-2 refresh-grant fires roughly every 270 s, no `M_UNKNOWN_TOKEN` ever reaches the server.
- Heartbeat stats (`sent`, `commands_handled`, `start`) survive rotations — they live outside `run_cycle`. Watermark is reset per cycle because event handlers are re-registered on the new Client; the matrix-sdk initial sync after each reconnect fetches anything missed.

### `aqua-matrix-claude-channel.service`

Identical recovery profile to heartbeat — same outer-loop rotation, same three-tier auth, same `Restart=always`. No periodic tick of its own; the inner cycle parks on `select!` until refresh deadline or sync exit.

Differences in failure modes specific to claude-channel:
- Per-message `claude -p` invocation has a 180s timeout; on timeout the daemon replies with `[claude-channel error] claude -p timed out after 180s` and stays running.
- `claude` binary missing: every prompt returns `[claude-channel error] failed to spawn ... claude -p`. Daemon does not crash.

### `claude-bridge.service`

- `Type=simple` with a bash supervisor as the main process. Supervisor:
  1. Loops every 10s
  2. Checks `tmux has-session -t claude-bridge`
  3. If missing, runs `tmux new-session -d -s claude-bridge -- ~/.local/bin/claude --dangerously-skip-permissions`
  4. Logs `[claude-bridge supervisor] spawned tmux session` to the journal
- If the inner `claude` process exits (any cause), the tmux session ends and is recreated on the next tick (≤10s downtime)
- If the supervisor itself is killed, `Restart=always` brings it back in ~5s, then it immediately respawns the tmux session
- `ExecStop=/usr/bin/tmux kill-session -t claude-bridge` makes `systemctl --user stop` clean

## Crash-loop guard explained

Each unit declares:

```
StartLimitIntervalSec=300
StartLimitBurst=10
```

systemd refuses to restart the unit if it has already restarted 10 times in the last 300 seconds. The unit transitions to `failed`. This prevents log spam / CPU burn when a daemon is fundamentally broken (siwx-oidc unreachable, disk full, etc.).

To re-enable restarts after fixing the root cause:

```bash
systemctl --user reset-failed aqua-matrix-heartbeat
systemctl --user start aqua-matrix-heartbeat
```

## Identity / device_id state recovery

Detailed in [`ARCHITECTURE.md` § "Identity and device-id persistence"](ARCHITECTURE.md#identity-and-device-id-persistence). One-paragraph summary for operators:

`<store_dir>/config.toml` persists `access_token`, `refresh_token`, `did`, `user_id`, `device_id`, and `expires_at_unix`. On startup the daemon picks the cheapest valid tier (cached access token → refresh grant → fresh OAuth). The first two tiers preserve `device_id` and therefore the SQLite crypto store. Tier three is the only path that triggers a crypto-store wipe + cross-signing rebootstrap. This means the only restart that causes visible disruption to your Element timeline is one that happens >24h after the last successful refresh.

## Secure Secret Storage (SSSS) — surviving a crypto-store wipe

Cross-signing normally persists across **ordinary** restarts via the SQLite
crypto store plus the preserved `device_id` (tiers 1–2 of session resolution).
SSSS specifically covers the **store-loss** case — a tier-3 fresh auth, a manual
`rm -f matrix-sdk-*.sqlite3*`, or a corrupted store — where the local crypto
state is gone but the Matrix account is the same.

What the agent does:

- **First run**: enables Secure Secret Storage on the homeserver and persists
  the recovery key to `<store_dir>/recovery.key` with mode `0600`. Enabling SSSS
  also stops the recurring `404 ... m.secret_storage.default_key` probe (see
  ARCHITECTURE.md § "Benign 404 ... log lines").
- **Cold start after a store wipe**: if `<store_dir>/recovery.key` is present,
  the agent restores cross-signing and secrets from SSSS using that key instead
  of bootstrapping a brand-new cross-signing identity. To Tim this looks like
  the same verified device, not a re-verification prompt.

What this means per scenario:

| State on cold start | Outcome |
|---|---|
| Store intact (tier 1/2) | Nothing to do — cross-signing already in the store |
| Store wiped, `recovery.key` present | SSSS restore — same cross-signing identity, no re-verify |
| Store wiped, `recovery.key` absent | Fresh cross-signing bootstrap — Tim sees one re-verification |

**SECURITY NOTE.** The recovery key sits at rest, unencrypted, on the host at
`<store_dir>/recovery.key` (mode `0600`). It is the master secret for this
identity's cross-signing. Protect and back it up. Anyone who can read that file
plus the account can impersonate the verified device. Losing the key **and** the
store means the next start mints a brand-new cross-signing identity.

## Manual recovery procedures

### "Daemon stuck in `failed` state"

```bash
systemctl --user status <unit>          # look at the last error
systemctl --user reset-failed <unit>    # clear the failed flag
systemctl --user start <unit>           # try again
```

### "Restart loop, want to debug live"

```bash
systemctl --user stop <unit>            # halt the auto-restart cycle
~/aqua-matrix-hello/target/debug/aqua-matrix-heartbeat   # or aqua-matrix-claude-p, whichever daemon
# ... iterate. Ctrl+C when done. systemctl --user start <unit> to resume normal supervision.
```

### "Wipe crypto store but keep OIDC creds"

```bash
systemctl --user stop <unit>
rm -f ~/.aqua-matrix-<unit-name>/matrix-sdk-*.sqlite3*
systemctl --user start <unit>
```

### "Wipe everything for this identity, start fresh"

```bash
systemctl --user stop <unit>
rm -rf ~/.aqua-matrix-<unit-name>/       # everything: store + config + cached session
# Optionally also: rm ~/aqua-matrix-hello/<unit>.pem  # changes the underlying DID/Matrix account
systemctl --user start <unit>
```

(Removing `config.toml` forces fresh OIDC client registration. Removing the `.pem` forces a brand-new identity — counterparties will see a new Matrix user. Note that `rm -rf` of the store dir also deletes `recovery.key`, so SSSS cannot restore the prior cross-signing identity — the next start bootstraps a fresh one. Back up `recovery.key` first if you want to keep it.)

### "Tail logs"

```bash
journalctl --user -u aqua-matrix-heartbeat -f
journalctl --user -u aqua-matrix-claude-channel -f
journalctl --user -u claude-bridge -f
```

### "Restart a daemon over Matrix when local shell is unavailable"

From the configured `--target` account (Tim), DM the heartbeat identity:

- `#shell restart` → restarts `aqua-matrix-heartbeat`
- `#shell respawn-channel` → restarts `aqua-matrix-claude-channel`
- `#shell respawn` → restarts `claude-bridge`

If the heartbeat itself is the wedged one, this won't work — you need local shell or another path in.

## Diagnostic decision tree

**No heartbeat status payload in 10+ min**

1. `systemctl --user is-active aqua-matrix-heartbeat` — `active` / `failed` / `activating`?
   - `failed` → `journalctl --user -u aqua-matrix-heartbeat -n 50`. Common: crash-loop guard tripped → `reset-failed` + fix root cause.
   - `activating` (indefinitely) → fresh-auth path stuck. Check siwx-oidc connectivity: `curl https://siwx-oidc.inblock.io/.well-known/openid-configuration`.
   - `active` but silent → sync stream might be wedged. Send `#shell ping` from Tim. No reply → daemon is alive but its sync is stuck → `systemctl --user restart aqua-matrix-heartbeat`.

**Claude channel does not respond to a DM**

1. `systemctl --user is-active aqua-matrix-claude-channel` — must be `active`.
2. Locally test `~/.local/bin/claude -p hi`. If claude itself is broken, the daemon cannot reply.
3. Check journal for `claude -p exited with status` or `claude -p timed out` — these are reported back to Element as `[claude-channel error]`. If Element shows the prompt was received but no error message arrived either, the reply send itself failed (look for `claude-channel reply send failed` in the journal).
4. Last resort: `systemctl --user restart aqua-matrix-claude-channel` (or `#shell respawn-channel` from heartbeat).

**Bridge tmux session is gone but unit is `active`**

Should NOT happen with the Type=simple supervisor design. If it does:
- `journalctl --user -u claude-bridge -n 30` — look for `spawn failed` lines and their reason
- Most likely: `claude` binary missing or PATH wrong. Verify `~/.local/bin/claude --version` works.

**`device_id` rotates on every restart, store keeps wiping**

Should NOT happen if refresh tokens are working. Check:
1. `grep refresh_token ~/.aqua-matrix-heartbeat/config.toml` — if missing, the daemon authenticated against an old siwx-oidc that did not issue refresh tokens. Next fresh auth (against the new server) fixes it.
2. `grep '^did' ~/.aqua-matrix-heartbeat/config.toml` — must also be present for the refresh-grant call.
3. Journal: `refresh grant failed` → most likely refresh token expired (>24h since last refresh). Fresh auth runs and mints a new device. One-time event, not a recurring problem unless restarts happen with >24h gaps.

**"All three are running but Tim still does not see them in Element"**

Less of a recovery issue, more of an onboarding one. Both Matrix daemons send a one-shot `[hello]` message on every start. If Tim sees no hello:
- Verify the daemons did NOT crash on hello: `journalctl --user -u <unit> -n 30 | grep hello`
- Verify the DM rooms exist on the server: `~/aqua-matrix-hello/target/debug/aqua-matrix-agent --read --key-file heartbeat.pem --store-dir ~/.aqua-matrix-heartbeat --target <tim>` (but stop the unit first to avoid SQLite contention)
- Tim's Element may have an unaccepted room invite from the claude-channel identity if this is the first time he is seeing it

## What this document does not cover

- The design rationale for two-identity architecture, sync model, identity persistence: [`ARCHITECTURE.md`](ARCHITECTURE.md).
- Day-to-day operations (sending a message, running the e2e test, attaching to the bridge): per-skill docs under [`../Skills/`](../Skills/).
- siwx-oidc / Synapse / Element server-side recovery: out of scope, see the companion repos in CLAUDE.md.
