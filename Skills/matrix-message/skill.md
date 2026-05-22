---
name: matrix-message
description: Send and receive E2E encrypted Matrix messages via aqua-matrix-agent
---

# Matrix Messaging

Send and receive end-to-end encrypted Matrix messages using `aqua-matrix-agent`.

## Prerequisites

Binary at `~/aqua-matrix-hello/target/release/aqua-matrix-agent`. If missing:

```bash
cd ~/aqua-matrix-hello && cargo build --release
```

## Quick Reference

| Operation | Command |
|---|---|
| Send message | `~/aqua-matrix-hello/target/release/aqua-matrix-agent --message "text" --target "@user:matrix.inblock.io"` |
| Read messages | `~/aqua-matrix-hello/target/release/aqua-matrix-agent --read --read-limit 20 --target "@user:matrix.inblock.io"` |
| Send + read | `~/aqua-matrix-hello/target/release/aqua-matrix-agent --message "text" --read --target "@user:matrix.inblock.io"` |
| Print agent DID | `~/aqua-matrix-hello/target/release/aqua-matrix-agent --print-did` |
| Use different identity | Add `--key-file path/to/other.pem` |

## Setup

**Zero-config:** The agent auto-registers an OIDC client on first run and caches credentials in `~/.aqua-matrix-agent/config.toml`. No manual setup needed.

**Default servers:**
- Matrix: `https://matrix.inblock.io`
- SIWX-OIDC: `https://siwx-oidc.inblock.io`
- Override with `--matrix-url` / `--siwx-url` if using a different server

**Default target:** Tim (`@did-pkh-eip155-1-0x0000000000000000000000000000000000000000:matrix.inblock.io`). Override with `--target`.

**Agent identity:** Ed25519 key at `agent.pem` in CWD (auto-generated if missing). Each key file produces a unique DID and Matrix account.

## Encryption and Verification

- All DMs are E2E encrypted by default (Megolm via matrix-sdk)
- The agent bootstraps cross-signing on first connect, so its device appears as "verified" in Element
- Crypto state (identity keys, session keys) persists in SQLite at `~/.aqua-matrix-agent/`
- Messages that cannot be decrypted show as `[unable to decrypt]`

## Agent-to-Agent Communication

Two agents can communicate by using different key files:

```bash
# Agent A (default key)
aqua-matrix-agent --message "ping" --target "@agent-b-user:matrix.inblock.io"

# Agent B (separate key)
aqua-matrix-agent --key-file agent-b.pem --store-dir ~/.aqua-matrix-agent-b \
  --read --target "@agent-a-user:matrix.inblock.io"
```

Pre-existing keys: `~/aqua-matrix-hello/agent.pem` (Agent A) and `~/aqua-matrix-hello/agent-b.pem` (Agent B).

## Environment Variables

| Var | Purpose | Default |
|---|---|---|
| `AGENT_KEY_FILE` | Path to Ed25519 PEM key | `agent.pem` |
| `SIWX_URL` | siwx-oidc server URL | `https://siwx-oidc.inblock.io` |
| `MATRIX_URL` | Matrix homeserver URL | `https://matrix.inblock.io` |
| `OIDC_CLIENT_ID` | Override OIDC client (skips auto-registration) | auto |
| `OIDC_REDIRECT_URI` | Override redirect URI | `http://localhost:0/callback` |
| `AGENT_STORE_DIR` | SQLite + config directory | `~/.aqua-matrix-agent` |

## Troubleshooting

- **"siwx-oidc authentication failed"**: Server may be down. Check `curl https://siwx-oidc.inblock.io/.well-known/openid-configuration`
- **"[unable to decrypt]"**: Crypto store mismatch. The sender encrypted before this agent's device existed. New messages will decrypt fine.
- **"initial sync failed"**: Network issue or expired token. Retry.
- **Cross-signing bootstrap warning**: Non-fatal. Agent works without cross-signing; device just shows as "unverified" in Element.
