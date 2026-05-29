# Reference implementation guide

This repo is a **reference implementation for building agents over Matrix +
[siwx-oidc](https://github.com/inblockio/siwx-oidc)**. The reusable asset is the
transport: DID-authenticated, end-to-end-encrypted Matrix messaging with durable
identity, automatic token rotation, and self-healing under systemd. Any agent
backend drops in behind a single trait.

`claude -p` (the `aqua-matrix-claude-p` crate) is **a placeholder backend** that
validates the bridge against the live homeserver and OIDC provider. It is not
the product — it is the worked example.

## Workspace layout

```
aqua-matrix-hello/                  # virtual Cargo workspace
├── Cargo.toml                      # [workspace] + shared [workspace.dependencies]
└── crates/
    ├── aqua-matrix-agent/          # LIBRARY: AgentClient, AgentConfig, OIDC,
    │                               #   session cache, recovery (SSSS), registry.
    │                               #   Also a one-shot CLI: send / read / print-did.
    ├── aqua-matrix-relay/          # GENERIC DAEMON: MessageHandler trait + run_daemon()
    │   └── examples/echo_agent.rs  #   a complete ~30-line agent
    ├── aqua-matrix-heartbeat/      # ops agent (status DMs + #shell) — host-specific
    └── aqua-matrix-claude-p/       # reference backend: forwards DMs to `claude -p`
```

Dependency direction is one-way:

```
aqua-matrix-heartbeat ─┐
                       ├─► aqua-matrix-relay ─► aqua-matrix-agent ─► siwx-oidc-auth + matrix-sdk
aqua-matrix-claude-p ──┘
```

`aqua-matrix-relay` re-exports everything a backend needs
(`AgentClient`, `AgentConfig`, `async_trait`), so a new agent crate depends on
**only `aqua-matrix-relay`** (plus `tokio` for its `main`) and never imports
matrix-sdk directly.

## The seam: `MessageHandler`

The relay owns the *transport lifecycle*; your handler owns *what an agent does*.
The trait carries no Matrix types — you receive the message body as a `&str`:

```rust
#[async_trait]
pub trait MessageHandler: Send + Sync + 'static {
    fn role(&self) -> &str;                              // fleet-registry role
    fn systemd_unit(&self) -> Option<&str> { None }      // supervising unit, for the registry
    fn hello(&self, _agent: &AgentClient) -> Option<String> { None }  // one-time greeting
    fn tick_interval(&self) -> Option<Duration> { None } // periodic timer; None = react-only
    async fn on_tick(&self, _agent: &AgentClient, _target: &str) {}
    async fn handle_message(&self, agent: &AgentClient, target: &str, body: &str);
}
```

Contract:

- `handle_message` fires **once per inbound message** from `target`. The relay
  has already confirmed the sender and deduplicated by a monotonic timestamp
  watermark (advanced *before* dispatch, so a crash mid-handler does not
  re-trigger on restart).
- Keep `handle_message` **fast**. For slow work (an LLM call, a subprocess),
  `tokio::spawn` a task and reply from there so the sync stream keeps flowing —
  see `aqua-matrix-claude-p`.
- All methods are `&self`; the handler is shared (`Arc`) across the sync stream
  and every spawned task. Put mutable state behind a `Mutex`/atomics (see
  `OpsHandler`'s stats in `aqua-matrix-heartbeat`).
- Errors are yours to log; the relay never unwinds on a handler error.

## What `run_daemon` does for you

```rust
run_daemon(config, target, handler).await   // never returns in steady state
```

Per client cycle it:

1. builds a fresh `AgentClient` (refresh-grant path preserves `device_id` and
   the crypto store — see ARCHITECTURE.md § "Identity and device-id persistence"),
2. joins pending invites,
3. sends `hello()` once, on the first cycle only,
4. upserts the fleet-registry entry (`role` / `systemd_unit`),
5. streams sync + an optional periodic tick until ~30 s before the access token
   expires, then drops the client and loops to rotate it.

After `MAX_CONNECT_FAILURES` (3) consecutive connect failures it
`std::process::exit(1)` so systemd's `Restart=always` brings up a clean process
(matrix-sdk has no public hook to swap a token in place; rotating the whole
client is what avoids the `M_UNKNOWN_TOKEN` sync wedge).

## Minimal agent (the whole contract)

From `crates/aqua-matrix-relay/examples/echo_agent.rs`:

```rust
use std::path::PathBuf;
use aqua_matrix_relay::{async_trait, run_daemon, AgentClient, AgentConfig, MessageHandler};

struct EchoHandler;

#[async_trait]
impl MessageHandler for EchoHandler {
    fn role(&self) -> &str { "echo" }
    fn hello(&self, agent: &AgentClient) -> Option<String> {
        Some(format!("[echo] online as {}. I echo whatever you DM me.", agent.user_id()))
    }
    async fn handle_message(&self, agent: &AgentClient, target: &str, body: &str) {
        if let Err(e) = agent.send_dm(target, &format!("echo: {body}")).await {
            eprintln!("echo reply failed: {e:#}");
        }
    }
}

#[tokio::main]
async fn main() {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let config = AgentConfig {
        key_file: PathBuf::from("echo.pem"),
        siwx_url: "https://siwx-oidc.inblock.io".into(),
        matrix_url: "https://matrix.inblock.io".into(),
        client_id: None,
        redirect_uri: None,
        store_dir: PathBuf::from(home).join(".aqua-matrix-echo"),
    };
    run_daemon(config, "@someone:matrix.inblock.io", EchoHandler).await;
}
```

Run it:

```bash
cargo run -p aqua-matrix-relay --example echo_agent
```

That is the entire surface: implement `MessageHandler`, build an `AgentConfig`,
call `run_daemon`. No auth code, no sync loop, no matrix-sdk.

## Building a production agent — checklist

1. **New crate** under `crates/` with a `[[bin]]`, depending on
   `aqua-matrix-relay.workspace = true` (+ `tokio`, `async-trait`). Add it to the
   root `Cargo.toml` `[workspace] members`.
2. **Implement `MessageHandler`.** Return a stable `role()` and, if supervised,
   `systemd_unit()` — these populate the fleet registry
   (`io.inblock.aqua.registry`, see ARCHITECTURE.md).
3. **Parse args / build `AgentConfig`** in a thin `main.rs` (copy
   `aqua-matrix-heartbeat/src/main.rs` as a template). Give the agent its own
   `--key-file` and `--store-dir` — **one identity per surface**, never
   multiplex onto an existing `.pem`.
4. **Ship a systemd user unit** (copy one from `systemd/`): `Restart=always`,
   `RestartSec=5s`, `StartLimitBurst` for the crash-loop guard, `WorkingDirectory`
   at the repo root, `ExecStart` pointing at `target/debug/<your-bin>`.
5. **Build & cut over:** `cargo build`, then `systemctl --user daemon-reload &&
   systemctl --user restart <unit>`; confirm `connected` + `client cycle
   starting` in `journalctl --user -u <unit>`.

## What is intentionally NOT abstracted

The transport — siwx-oidc + Matrix — is fixed. There is no `Transport` trait.
The repo's identity *is* "DID-authenticated, E2EE Matrix agent template"; a
pluggable transport would dilute that. The seam is at the message boundary, not
the wire. If you need a different transport, fork; if you need a different
agent, implement `MessageHandler`.

## See also

- `docs/ARCHITECTURE.md` — design rationale: two-identity model, identity/
  device-id persistence, stream-sync, the self-registry, auto-start chain.
- `docs/RECOVERY.md` — what auto-recovers vs needs a human; diagnostic decision
  tree.
- `Skills/heartbeat`, `Skills/claude-channel` — operating the two shipped agents.
