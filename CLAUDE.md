# aqua-matrix-agent

Rust library and CLI for Matrix agent communication with E2E encryption.
Agents authenticate via [siwx-oidc](https://github.com/inblockio/siwx-oidc) using decentralized identifiers (DIDs), not passwords.

**Architecture:** [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — design rationale for the host's three agent components (heartbeat ops channel, claude-channel LLM channel, local claude-bridge tmux), identity/device-id persistence model, stream-sync design, file/identity inventory.

**Recovery / when things break:** [`docs/RECOVERY.md`](docs/RECOVERY.md) — what auto-recovers vs needs human intervention, per-unit restart policies, crash-loop guard explained, manual recovery procedures, and a diagnostic decision tree keyed by symptom. Start here when something is broken.

## If you are an AI agent: how to send and receive messages

This repo is your messaging tool. Use `/matrix-message` for the full skill, or follow the quick start below.

### Send a message

```bash
~/aqua-matrix-hello/target/debug/aqua-matrix-agent --message "your message here"
```

No flags needed beyond `--message`. The agent auto-registers OIDC credentials on first run, caches them in `~/.aqua-matrix-agent/config.toml`, and defaults to messaging Tim.

### Read messages

```bash
~/aqua-matrix-hello/target/debug/aqua-matrix-agent --read --read-limit 20
```

### Send and read in one call

```bash
~/aqua-matrix-hello/target/debug/aqua-matrix-agent --message "ping" --read
```

### Message a specific user

```bash
~/aqua-matrix-hello/target/debug/aqua-matrix-agent --message "hello" --target "@user:matrix.inblock.io"
```

### Use a different agent identity

```bash
~/aqua-matrix-hello/target/debug/aqua-matrix-agent --key-file other.pem --store-dir ~/.other-agent --message "hi"
```

Each key file produces a unique DID and separate Matrix account. Convention on this host:

- `agent.pem` — chat identity (re-created on first chat run if absent)
- `agent-b.pem` — second test identity for `/e2e-test`
- `heartbeat.pem` — ops identity (heartbeat + `#shell` command channel). Store at `~/.aqua-matrix-heartbeat/`. Managed by `aqua-matrix-heartbeat.service`.
- `claude-channel.pem` — LLM channel identity (forwards prose to `claude -p`). Store at `~/.aqua-matrix-claude-channel/`. Managed by `aqua-matrix-claude-channel.service`.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the rationale (one daemon per surface, separate failure domains, no stateful mode-switching).

### Build if binary is missing

```bash
cd ~/aqua-matrix-hello && cargo build
```

Default to debug builds (release is much slower for iteration). The systemd units intentionally point at `target/debug/`.

## Architecture

This is a Cargo workspace (virtual root manifest) and a reference implementation for any agent backend over Matrix + siwx-oidc — implement `MessageHandler` and call `run_daemon()` from `aqua-matrix-relay`. The four crates under `crates/`:

```
aqua-matrix-hello (virtual workspace)
  |
  +-- crates/aqua-matrix-agent     -- library (AgentClient, AgentConfig, OIDC, recovery,
  |                                   registry) + one-shot CLI binary `aqua-matrix-agent`
  |                                   (--message / --read / --print-did only)
  +-- crates/aqua-matrix-relay     -- generic daemon: MessageHandler trait + run_daemon()
  |                                   lifecycle (connect-rotate-sync-watermark); examples/echo_agent.rs
  +-- crates/aqua-matrix-heartbeat -- binary `aqua-matrix-heartbeat` (ops agent: periodic
  |                                   status DM + #shell commands)
  +-- crates/aqua-matrix-claude-p  -- binary `aqua-matrix-claude-p` (reference backend:
                                      forwards DMs to `claude -p`)
  |
  +-- siwx-oidc-auth -- Headless OIDC client (CAIP-122 signature auth)
  +-- matrix-sdk     -- Matrix client with E2E encryption (Megolm/Vodozemac)
```

**Authentication flow:** Ed25519 key -> derive DID -> CAIP-122 challenge-response against siwx-oidc -> siwx-oidc verifies signature, provisions user in Synapse via MSC3861 endpoints, issues opaque `mat_*`/`mcr_*` tokens -> Synapse validates tokens via `/oauth2/introspect` (RFC 7662) -> Matrix session restored.

**siwx-oidc is NOT a fork of MAS.** It is a fully independent Rust OIDC provider (Axum + Redis) that implements MSC3861 compatibility so Synapse can delegate authentication to it. The CAIP-122 signature verification happens server-side in siwx-core. A shared `MAS_SHARED_SECRET` secures the introspection channel between Synapse and siwx-oidc.

**Encryption:** All DMs are E2E encrypted (Megolm via Vodozemac). The agent bootstraps cross-signing on first connect so its device appears as "verified" in Element. Crypto state persists in SQLite at the store directory.

**OIDC auto-registration:** If no `--client-id` is provided, the agent registers a new client via the siwx-oidc `/register` endpoint and caches credentials in `{store_dir}/config.toml`.

## Building and testing

```bash
cargo build                                  # debug build for iteration (DEFAULT — fast)
cargo build --release                        # only when shipping; do not use during dev
cargo test                                   # run unit tests (config roundtrip, partial loading)
cargo test --test e2e --features e2e         # run E2E test (requires live matrix.inblock.io)
```

A single `cargo build` builds the whole workspace, producing `target/debug/aqua-matrix-agent` (one-shot CLI), `target/debug/aqua-matrix-heartbeat`, and `target/debug/aqua-matrix-claude-p`. The systemd units point at the debug paths on purpose — keeps rebuild cycles tight.

## CLI flags

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--key-file` | `AGENT_KEY_FILE` | `agent.pem` | Ed25519 PEM key (created if missing) |
| `--siwx-url` | `SIWX_URL` | `https://siwx-oidc.inblock.io` | siwx-oidc provider URL |
| `--matrix-url` | `MATRIX_URL` | `https://matrix.inblock.io` | Matrix homeserver URL |
| `--client-id` | `OIDC_CLIENT_ID` | auto-registered | OIDC client ID |
| `--redirect-uri` | `OIDC_REDIRECT_URI` | `http://localhost:0/callback` | OIDC redirect URI |
| `--target` | | Tim's account | Matrix user ID to message |
| `--store-dir` | `AGENT_STORE_DIR` | `~/.aqua-matrix-agent` | SQLite + config directory |
| `--message` | | | Message text to send |
| `--read` | | | Read recent messages |
| `--read-limit` | | `20` | Number of messages to fetch |
| `--print-did` | | | Print agent DID and exit |

These are the only flags on the `aqua-matrix-agent` binary — it is one-shot. The long-running daemons are now separate workspace binaries built from the same `cargo build`: `aqua-matrix-heartbeat` (status-DM + `#shell`; interval via `--interval <secs>`, see `/heartbeat` skill) and `aqua-matrix-claude-p` (forwards inbound DMs from `--target` through `claude -p`, see `/claude-channel` skill). See [`docs/REFERENCE.md`](docs/REFERENCE.md).

## Wrapped-harness configuration

This repo runs under `claude-ws` (alias for `claude --dangerously-skip-permissions`, defined at `~/.bashrc:122`). `~/.claude/settings.json` is configured to:

- pin `"model": "claude-opus-4-7"` so every session uses the most capable model
- expose `CONTEXT_WINDOW=1000000` so the Stop hook + heartbeat report the right context % for the 1M variant
- register a `Stop` hook at `~/.claude/hooks/compact-at-50.py` that blocks stop with `decision: "block"` and instructs Claude to run `/compact` whenever context usage crosses 50% (`COMPACT_THRESHOLD` env var to tune)

The hook reads the latest `usage.input_tokens` from the active transcript, so token accounting matches whatever the model itself reported.

## Skills

| Skill | Purpose |
|---|---|
| `/matrix-message` | Full reference for sending and receiving E2E encrypted messages |
| `/e2e-test` | Run and verify E2EE integration tests between two agent identities |
| `/heartbeat` | Run the `aqua-matrix-heartbeat` binary — DMs status every 10min AND honors `#shell help`, `#shell status`, `#shell ping`, `#shell uptime`, `#shell restart`, `#shell respawn`, `#shell logs` commands sent from `--target` |
| `/claude-bridge` | Persistent `claude --dangerously-skip-permissions` in tmux, supervised by systemd; respawnable via `#shell respawn` |
| `/claude-channel` | Matrix LLM channel daemon (`aqua-matrix-claude-p` binary) — separate identity, forwards DMs from `--target` to `claude -p` and replies with stdout; respawnable via `#shell respawn-channel` |

**Skill layout.** Skill source-of-truth lives at the repo root in `Skills/<name>/skill.md`. The Claude Code discovery directory `.claude/skills/<name>` is a symlink into `Skills/`:

```
Skills/
  matrix-message/skill.md   <-- canonical content (edit here)
  e2e-test/skill.md
.claude/skills/
  matrix-message -> ../../Skills/matrix-message
  e2e-test      -> ../../Skills/e2e-test
```

Edit skills in `Skills/`. Do not duplicate content into `.claude/skills/`. When adding a new skill: create `Skills/<name>/skill.md`, then `ln -s ../../Skills/<name> .claude/skills/<name>`. If `.claude/skills/<name>/skill.md` ever resolves to a regular file instead of a symlink target, the layout has drifted — re-create the symlink.

## Companion repos

| Repo | Purpose |
|---|---|
| `../siwx-oidc/` | CAIP-122 OIDC provider. Path-dep source of `siwx-oidc-auth` (see Cargo.toml). |
| `../aqua-auth/` | Workspace member of `../siwx-oidc/`. Must be checked out as a sibling for `cargo build` to resolve `siwx-oidc-auth`'s `path = "../../aqua-auth"` dep. |
| `../siwx-oidc-matrix-server/` | Docker Compose stack (Synapse + siwx-oidc + Element Web) |

Fresh dev setup needs `git clone https://github.com/inblockio/siwx-oidc.git` and `git clone https://github.com/inblockio/aqua-auth.git` alongside this repo.
