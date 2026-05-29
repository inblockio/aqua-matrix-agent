//! aqua-matrix-claude-p — the reference example backend.
//!
//! Each inbound DM (without a `#shell` prefix — that belongs to the ops
//! channel) becomes a fresh `claude -p <prompt>` invocation; stdout is DM'd
//! back. Stateless per message: no conversation continuity (could be added
//! later via `claude -c <session-id>` keyed by room or user).
//!
//! This is the canonical "drop in a backend" example for [`aqua_matrix_relay`].
//! `claude -p` is a *placeholder* used to validate the siwx-oidc + Matrix bridge
//! against the live homeserver — replace [`ClaudePHandler::handle_message`] with
//! a call into any agent and you have a new agent on the same transport.
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use aqua_matrix_relay::{async_trait, run_daemon, AgentClient, AgentConfig, MessageHandler};
use clap::Parser;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const CLAUDE_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_REPLY_BYTES: usize = 16_000; // Matrix can take more, but be polite.
const ROLE: &str = "claude-channel";
const UNIT: &str = "aqua-matrix-claude-channel";

#[derive(Parser)]
#[command(
    name = "aqua-matrix-claude-p",
    about = "LLM bridge: forward inbound DMs through `claude -p` and reply with stdout"
)]
struct Args {
    #[arg(long, env = "AGENT_KEY_FILE", default_value = "claude-channel.pem")]
    key_file: PathBuf,

    #[arg(long, env = "SIWX_URL", default_value = "https://siwx-oidc.inblock.io")]
    siwx_url: String,

    #[arg(long, env = "MATRIX_URL", default_value = "https://matrix.inblock.io")]
    matrix_url: String,

    #[arg(long, env = "OIDC_CLIENT_ID", help = "OIDC client ID (auto-registered if omitted)")]
    client_id: Option<String>,

    #[arg(long, env = "OIDC_REDIRECT_URI", help = "OIDC redirect URI (defaults to http://localhost:0/callback)")]
    redirect_uri: Option<String>,

    #[arg(
        long,
        default_value = "@did-pkh-eip155-1-0x0000000000000000000000000000000000000000:matrix.inblock.io",
        help = "Matrix user ID whose DMs are forwarded to claude -p"
    )]
    target: String,

    #[arg(long, env = "AGENT_STORE_DIR")]
    store_dir: Option<PathBuf>,
}

fn default_store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".aqua-matrix-claude-channel")
}

struct ClaudePHandler;

#[async_trait]
impl MessageHandler for ClaudePHandler {
    fn role(&self) -> &str {
        ROLE
    }

    fn systemd_unit(&self) -> Option<&str> {
        Some(UNIT)
    }

    fn hello(&self, agent: &AgentClient) -> Option<String> {
        Some(format!(
            "[hello] aqua-matrix-claude-channel online (identity: {}). DM me any text (without `#shell` prefix) and I will run `claude -p <your message>` and reply with the output. {}s timeout per invocation, stateless per message.",
            agent.user_id(),
            CLAUDE_TIMEOUT.as_secs(),
        ))
    }

    async fn handle_message(&self, agent: &AgentClient, target: &str, body: &str) {
        // `#shell` belongs to the ops/heartbeat channel, not the LLM channel.
        if body.to_lowercase().starts_with("#shell") {
            return;
        }

        tracing::info!("claude-channel prompt from {}: {} chars", target, body.len());

        // Run claude in its own task so the sync stream keeps flowing while the
        // (potentially long) invocation runs.
        let agent = agent.clone();
        let target = target.to_string();
        let prompt = body.to_string();
        tokio::spawn(async move {
            let reply = match invoke_claude(&prompt).await {
                Ok(out) => out,
                Err(e) => format!("[claude-channel error] {e:#}"),
            };
            let reply = if reply.trim().is_empty() {
                "[claude-channel] (no output)".to_string()
            } else {
                truncate(&reply, MAX_REPLY_BYTES)
            };
            if let Err(e) = agent.send_dm(&target, &reply).await {
                tracing::warn!("claude-channel reply send failed: {e:#}");
            }
        });
    }
}

/// Run `claude -p <prompt>` with stdin closed, capturing stdout. Bounded by
/// [`CLAUDE_TIMEOUT`]. Uses whatever `claude` is on PATH plus the absolute
/// fallback that matches the systemd unit's `Environment=PATH`.
async fn invoke_claude(prompt: &str) -> anyhow::Result<String> {
    let claude_bin = find_claude_bin();
    tracing::debug!("invoking {} -p", claude_bin);

    let mut child = Command::new(&claude_bin)
        .arg("-p")
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {claude_bin} -p"))?;

    // No stdin needed; close it explicitly anyway.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.shutdown().await;
    }

    let with_timeout = tokio::time::timeout(CLAUDE_TIMEOUT, child.wait_with_output()).await;
    let output = match with_timeout {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => anyhow::bail!("claude wait failed: {e}"),
        Err(_) => anyhow::bail!("claude -p timed out after {}s", CLAUDE_TIMEOUT.as_secs()),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        anyhow::bail!(
            "claude -p exited with status {}: {}",
            output.status,
            stderr.trim()
        );
    }
    Ok(stdout)
}

fn find_claude_bin() -> String {
    // Try absolute path first (matches the systemd unit Environment).
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = format!("{home}/.local/bin/claude");
    if std::path::Path::new(&candidate).exists() {
        return candidate;
    }
    // Fall back to PATH lookup.
    "claude".to_string()
}

fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Cut on a char boundary to avoid splitting UTF-8.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n[...truncated]");
    out
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,aqua_matrix_agent=info,aqua_matrix_relay=info,aqua_matrix_claude_p=info".into()),
        )
        .init();

    let args = Args::parse();
    let config = AgentConfig {
        key_file: args.key_file,
        siwx_url: args.siwx_url,
        matrix_url: args.matrix_url,
        client_id: args.client_id,
        redirect_uri: args.redirect_uri,
        store_dir: args.store_dir.unwrap_or_else(default_store_dir),
    };

    run_daemon(config, &args.target, ClaudePHandler).await;
}
