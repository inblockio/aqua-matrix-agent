# aqua-matrix-agent

A Rust library and CLI for building Matrix agents that authenticate via [siwx-oidc](https://github.com/inblockio/siwx-oidc) using decentralized identifiers (DIDs).

Each agent gets a deterministic DID derived from an Ed25519 key. Authentication goes through a CAIP-122 OIDC flow, giving the agent a Matrix account without passwords or manual registration.

## Features

- **DID-based identity**: agents authenticate with Ed25519 keys, no passwords needed
- **Library + CLI**: use `aqua_matrix_agent` as a crate in your own agent, or run the CLI directly
- **Direct messaging**: send and read messages in DM rooms
- **Auto-join**: automatically accepts room invitations
- **SQLite session store**: persists sync state across restarts

## Prerequisites

- Rust 1.75+
- A running [siwx-oidc](https://github.com/inblockio/siwx-oidc) instance connected to a Matrix homeserver
- An OIDC client registered with the siwx-oidc provider (you need the `client_id` and `redirect_uri`)

## Quick start

```bash
# Build
cargo build --release

# Print the agent's DID (generates a key if none exists)
./target/release/aqua-matrix-agent --print-did

# Send a message
./target/release/aqua-matrix-agent \
  --client-id <YOUR_CLIENT_ID> \
  --redirect-uri <YOUR_REDIRECT_URI> \
  --target "@user:matrix.example.com" \
  --message "Hello from my agent"

# Read recent messages
./target/release/aqua-matrix-agent \
  --client-id <YOUR_CLIENT_ID> \
  --redirect-uri <YOUR_REDIRECT_URI> \
  --target "@user:matrix.example.com" \
  --read
```

## CLI options

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--key-file` | `AGENT_KEY_FILE` | `agent.pem` | Path to Ed25519 PEM key (created if missing) |
| `--siwx-url` | `SIWX_URL` | `https://siwx-oidc.inblock.io` | siwx-oidc provider URL |
| `--matrix-url` | `MATRIX_URL` | `https://matrix.inblock.io` | Matrix homeserver URL |
| `--client-id` | `OIDC_CLIENT_ID` | (required) | OIDC client ID |
| `--redirect-uri` | `OIDC_REDIRECT_URI` | (required) | OIDC redirect URI |
| `--target` | | | Matrix user ID to message |
| `--store-dir` | `AGENT_STORE_DIR` | `~/.aqua-matrix-agent` | SQLite session store directory |
| `--message` | | | Message text to send |
| `--read` | | | Read recent messages from the DM room |
| `--read-limit` | | `20` | Number of messages to fetch |
| `--print-did` | | | Print agent DID and exit |
| `--heartbeat` | | | Run as a heartbeat daemon (10-min status DMs by default) |
| `--heartbeat-interval` | | `600` | Heartbeat interval in seconds |

## Heartbeat daemon

`--heartbeat` puts the binary into a loop that sends a Matrix DM every 10 minutes with agent, host, and (if present) active Claude Code session status. A ready-to-install systemd user unit ships at `systemd/aqua-matrix-heartbeat.service`. See `Skills/heartbeat/skill.md` for setup, payload format, and tuning.

## Using as a library

```rust
use aqua_matrix_agent::{AgentClient, AgentConfig};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = AgentConfig {
        key_file: PathBuf::from("my-agent.pem"),
        siwx_url: "https://siwx-oidc.inblock.io".into(),
        matrix_url: "https://matrix.inblock.io".into(),
        client_id: "my-client-id".into(),
        redirect_uri: "http://localhost:0/callback".into(),
        store_dir: PathBuf::from(".agent-store"),
    };

    let agent = AgentClient::connect(config).await?;
    println!("Connected as {} ({})", agent.user_id(), agent.did());

    agent.send_dm("@someone:matrix.inblock.io", "Hello!").await?;
    Ok(())
}
```

## How it works

1. The agent loads (or generates) an Ed25519 key from a PEM file
2. It derives a `did:pkh:eip155:1:0x...` DID from the key
3. It authenticates against siwx-oidc using a CAIP-122 signed message, receiving an OAuth2 access token
4. The access token is used to restore a Matrix client session
5. The agent can then send/receive messages like any Matrix client

## License

Apache-2.0
