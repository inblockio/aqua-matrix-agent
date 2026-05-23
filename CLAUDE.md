# aqua-matrix-agent

Rust library and CLI for Matrix agent communication with E2E encryption.
Agents authenticate via [siwx-oidc](https://github.com/inblockio/siwx-oidc) using decentralized identifiers (DIDs), not passwords.

## If you are an AI agent: how to send and receive messages

This repo is your messaging tool. Use `/matrix-message` for the full skill, or follow the quick start below.

### Send a message

```bash
~/aqua-matrix-hello/target/release/aqua-matrix-agent --message "your message here"
```

No flags needed beyond `--message`. The agent auto-registers OIDC credentials on first run, caches them in `~/.aqua-matrix-agent/config.toml`, and defaults to messaging Tim.

### Read messages

```bash
~/aqua-matrix-hello/target/release/aqua-matrix-agent --read --read-limit 20
```

### Send and read in one call

```bash
~/aqua-matrix-hello/target/release/aqua-matrix-agent --message "ping" --read
```

### Message a specific user

```bash
~/aqua-matrix-hello/target/release/aqua-matrix-agent --message "hello" --target "@user:matrix.inblock.io"
```

### Use a different agent identity

```bash
~/aqua-matrix-hello/target/release/aqua-matrix-agent --key-file other.pem --store-dir ~/.other-agent --message "hi"
```

Each key file produces a unique DID and separate Matrix account. Convention on this host:

- `agent.pem` — chat identity (re-created on first chat run if absent)
- `agent-b.pem` — second test identity for `/e2e-test`
- `heartbeat.pem` — heartbeat-only identity used by the systemd unit. Do not reuse for chat — its crypto store lives separately at `~/.aqua-matrix-heartbeat/`.

### Build if binary is missing

```bash
cd ~/aqua-matrix-hello && cargo build --release
```

## Architecture

```
aqua-matrix-agent (binary)
  |
  +-- src/lib.rs     -- AgentClient: connect, send_dm, messages, cross-signing bootstrap
  +-- src/main.rs    -- CLI: clap args, delegates to AgentClient
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

The binary lands at `target/debug/aqua-matrix-agent` (debug) or `target/release/aqua-matrix-agent` (release). The systemd heartbeat unit points at the debug path on purpose — keeps rebuild cycles tight.

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
| `--heartbeat` | | | Run as a status-DM daemon (see `/heartbeat` skill) |
| `--heartbeat-interval` | | `600` | Heartbeat tick in seconds |

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
| `/heartbeat` | Run aqua-matrix-agent as a daemon DMing status every 10min AND honoring `/help`, `/status`, `/ping`, `/uptime`, `/restart`, `/logs` commands sent from `--target` |

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
| `../siwx-oidc/` | CAIP-122 OIDC provider (authentication backend) |
| `../siwx-oidc-matrix-server/` | Docker Compose stack (Synapse + siwx-oidc + Element Web) |
