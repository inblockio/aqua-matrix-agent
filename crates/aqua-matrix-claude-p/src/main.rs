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
use aqua_matrix_relay::{async_trait, run_daemon, AgentClient, AgentConfig, MessageHandler, ReplyStream};
use clap::Parser;
use tokio::io::AsyncBufReadExt;
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
        // (potentially long) invocation runs. The answer streams into a single
        // message edited in place; a typing indicator covers the pre-first-token
        // latency.
        let agent = agent.clone();
        let target = target.to_string();
        let prompt = body.to_string();
        tokio::spawn(async move {
            if let Err(e) = stream_claude(&agent, &target, &prompt).await {
                tracing::warn!("claude-channel stream failed: {e:#}");
                let _ = agent
                    .send_dm(&target, &format!("[claude-channel error] {e:#}"))
                    .await;
            }
        });
    }
}

/// Run `claude -p` in streaming mode and pipe its output into a single Matrix
/// message that is edited in place as tokens arrive. A typing indicator covers
/// the wait for the first token. Bounded by [`CLAUDE_TIMEOUT`]; uses whatever
/// `claude` is on PATH plus the absolute fallback matching the systemd unit's
/// `Environment=PATH`.
async fn stream_claude(agent: &AgentClient, target: &str, prompt: &str) -> anyhow::Result<()> {
    let claude_bin = find_claude_bin();
    tracing::debug!("invoking {} -p (stream-json)", claude_bin);

    let mut child = Command::new(&claude_bin)
        .arg("-p")
        .arg(prompt)
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-partial-messages")
        .arg("--verbose")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true) // reap claude if this task is dropped/errors early
        .spawn()
        .with_context(|| format!("failed to spawn {claude_bin} -p"))?;

    let stdout = child.stdout.take().context("claude produced no stdout pipe")?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();

    // "typing…" until the first visible token; after that the growing message
    // itself signals progress.
    let mut typing = agent.typing_guard(target).await;
    let mut stream: Option<ReplyStream> = None;
    let mut final_text: Option<String> = None;
    let mut err: Option<String> = None;

    let deadline = tokio::time::Instant::now() + CLAUDE_TIMEOUT;
    loop {
        let line = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(deadline) => {
                let _ = child.start_kill();
                err = Some(format!("timed out after {}s", CLAUDE_TIMEOUT.as_secs()));
                break;
            }
            l = lines.next_line() => l,
        };
        let line = match line {
            Ok(Some(l)) => l,
            Ok(None) => break, // EOF — claude exited
            Err(e) => {
                err = Some(format!("reading claude output: {e}"));
                break;
            }
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            // Incremental output: {"type":"stream_event","event":{"type":
            // "content_block_delta","delta":{"type":"text_delta","text":"…"}}}
            Some("stream_event") => {
                let event = v.get("event");
                let is_text_delta = event.and_then(|e| e.get("type")).and_then(|t| t.as_str())
                    == Some("content_block_delta")
                    && event
                        .and_then(|e| e.get("delta"))
                        .and_then(|d| d.get("type"))
                        .and_then(|t| t.as_str())
                        == Some("text_delta");
                if is_text_delta {
                    if let Some(text) = event
                        .and_then(|e| e.get("delta"))
                        .and_then(|d| d.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        if !text.is_empty() {
                            if stream.is_none() {
                                // First token: stop "typing", open the live message.
                                typing.take();
                                stream = Some(agent.reply_stream(target).await?);
                            }
                            if let Some(s) = stream.as_mut() {
                                s.push(text).await?;
                            }
                        }
                    }
                }
            }
            // Terminal: {"type":"result","is_error":bool,"result":"<full text>"}
            Some("result") => {
                if v.get("is_error").and_then(|b| b.as_bool()) == Some(true) {
                    err = Some(
                        v.get("result")
                            .and_then(|r| r.as_str())
                            .unwrap_or("claude reported an error")
                            .to_string(),
                    );
                } else {
                    final_text = v.get("result").and_then(|r| r.as_str()).map(String::from);
                }
                break;
            }
            _ => {}
        }
    }

    let _ = child.wait().await;
    typing.take(); // clear the indicator if no token ever streamed

    match (stream, err) {
        // Normal: finalize with the authoritative full result.
        (Some(s), None) => s.finish(final_text.as_deref()).await?,
        // Streamed some, then failed/timed out — finalize gracefully in place.
        (Some(s), Some(e)) => {
            let mut text = final_text.unwrap_or_default();
            text.push_str(&format!("\n\n[claude-channel error] {e}"));
            let _ = s.finish(Some(&text)).await;
        }
        // Nothing streamed but a final result arrived — send it as one message.
        (None, None) => {
            let text = final_text.unwrap_or_default();
            let text = if text.trim().is_empty() {
                "[claude-channel] (no output)".to_string()
            } else {
                truncate(&text, MAX_REPLY_BYTES)
            };
            agent.send_dm(target, &text).await?;
        }
        // Failed before any output — surface the error to the caller.
        (None, Some(e)) => anyhow::bail!("{e}"),
    }
    Ok(())
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
