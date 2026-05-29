//! aqua-matrix-claude-p — the reference example backend.
//!
//! Each inbound DM (without a `#shell` prefix — that belongs to the ops
//! channel) becomes a `claude -p <prompt>` invocation; stdout is DM'd back.
//!
//! ## Chat confirmations (Phase A)
//!
//! A prompt that looks **destructive** (`rm`, `git push --force`, …; see
//! [`destructive`]) is not run blindly. Instead it follows a
//! **plan → approve → execute** flow:
//!   1. run `claude -p --permission-mode plan <prompt>` — this investigates
//!      with read-only tools and emits a *plan* but **executes no destructive
//!      tool** (the headless `ExitPlanMode` is auto-denied; verified empirically
//!      — see `docs/plans/chat-confirmations.md`);
//!   2. stream the plan back, then [`PendingMap::ask`] the authenticated user
//!      "Approve this plan? yes/no";
//!   3. on **yes** → resume the same session with `--resume <id>
//!      --permission-mode acceptEdits` to actually execute; on **no**/timeout →
//!      abort, touching nothing.
//!
//! Non-destructive prompts keep the original direct-stream behaviour. The
//! [`pending`] router (the shared `ask_user` primitive) is reused by Phases B/C.
//!
//! This is the canonical "drop in a backend" example for [`aqua_matrix_relay`].
mod destructive;
mod pending;

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use aqua_matrix_relay::{async_trait, run_daemon, AgentClient, AgentConfig, MessageHandler, ReplyStream};
use clap::Parser;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use pending::PendingMap;

const CLAUDE_TIMEOUT: Duration = Duration::from_secs(180);
/// How long to wait for the user's `yes`/`no` to a plan. Bounded by
/// [`CLAUDE_TIMEOUT`] per the plan doc (default-DENY on timeout); a separate,
/// shorter window keeps a forgotten question from holding a run open for the
/// full claude budget.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);
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

/// The claude-channel backend. Holds the shared [`PendingMap`] (the `ask_user`
/// primitive) plus a per-target run lock so only one `claude` run — and thus at
/// most one open question — exists per user at a time.
#[derive(Default)]
struct ClaudePHandler {
    pending: PendingMap,
    /// One async mutex per `target`, gating its active run. `claude` is
    /// expensive and an open confirmation must not race a second prompt; this
    /// serialises runs per user without blocking other users.
    run_locks: std::sync::Mutex<std::collections::HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl ClaudePHandler {
    /// Get-or-create the per-target run lock.
    fn run_lock(&self, target: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .run_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        locks
            .entry(target.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
}

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
            "[hello] aqua-matrix-claude-channel online (identity: {}). DM me any text (without `#shell` prefix) and I will run `claude -p <your message>` and reply with the output. {}s timeout per invocation. Destructive requests (rm, force-push, …) are shown as a plan and only run after you reply `yes`.",
            agent.user_id(),
            CLAUDE_TIMEOUT.as_secs(),
        ))
    }

    async fn handle_message(&self, agent: &AgentClient, target: &str, body: &str) {
        // `#shell` belongs to the ops/heartbeat channel, not the LLM channel.
        if body.to_lowercase().starts_with("#shell") {
            return;
        }

        // FIRST: if a run is waiting on this user's answer, this DM IS the
        // answer — resolve the pending question instead of starting a fresh
        // run. This is the pending-reply inversion at the heart of `ask_user`.
        if self.pending.try_resolve(target, body) {
            tracing::info!("claude-channel: DM from {target} resolved a pending question");
            return;
        }

        tracing::info!("claude-channel prompt from {}: {} chars", target, body.len());

        // Run claude in its own task so the sync stream keeps flowing while the
        // (potentially long) invocation runs.
        let agent = agent.clone();
        let target = target.to_string();
        let prompt = body.to_string();
        let pending = self.pending.clone();
        let run_lock = self.run_lock(&target);
        tokio::spawn(async move {
            // Serialise: hold the per-target lock for the whole run so a second
            // prompt can't start (or open a second question) until this one
            // finishes. `try_lock` would drop the prompt; instead we await —
            // the relay watermark already prevents duplicate delivery.
            let _guard = run_lock.lock().await;
            let res = if destructive::looks_destructive(&prompt) {
                run_gated(&agent, &pending, &target, &prompt).await
            } else {
                // Non-destructive: original direct-stream behaviour, no session
                // continuity needed.
                stream_claude(&agent, &target, &ClaudeRun::fresh(&prompt))
                    .await
                    .map(|_| ())
            };
            if let Err(e) = res {
                tracing::warn!("claude-channel run failed: {e:#}");
                let _ = agent
                    .send_dm(&target, &format!("[claude-channel error] {e:#}"))
                    .await;
            }
        });
    }
}

/// Phase A plan → approve → execute flow for a destructive prompt.
///
/// 1. Run plan mode (no destructive tool executes — verified) and stream the
///    plan, capturing the session id.
/// 2. Ask the authenticated user to approve.
/// 3. On `yes` → resume that session in `acceptEdits` mode to execute.
///    On `no`/timeout/anything else → abort, fail closed (do nothing).
async fn run_gated(
    agent: &AgentClient,
    pending: &PendingMap,
    target: &str,
    prompt: &str,
) -> anyhow::Result<()> {
    let class = destructive::classify(prompt).unwrap_or("destructive action");
    tracing::info!("claude-channel: gating {class:?} prompt from {target}");

    // 1. Plan mode. The streamed message is the plan; we also need its session
    //    id to resume on approval.
    let plan = stream_claude(agent, target, &ClaudeRun::plan(prompt)).await?;
    let Some(session_id) = plan.session_id else {
        // No session id → we cannot resume to execute. Fail closed.
        anyhow::bail!("plan produced no session id to resume; aborting (fail closed)");
    };

    // 2. Ask for approval. Default-DENY on timeout / send failure.
    let question = format!(
        "[confirm] The request above involves a **{class}**. I have planned it but not executed anything. Reply `yes` to proceed, or `no` to abort.",
    );
    let answer = pending
        .ask(agent, target, &question, APPROVAL_TIMEOUT)
        .await;

    let approved = matches!(
        answer.as_deref().map(str::trim).map(str::to_lowercase).as_deref(),
        Some("yes" | "y" | "approve" | "approved" | "ok" | "proceed")
    );
    if !approved {
        let note = match answer {
            Some(a) => format!("[aborted] Not approved (you said: {:?}). Nothing was executed.", a.trim()),
            None => "[aborted] No approval within the timeout. Nothing was executed.".to_string(),
        };
        tracing::info!("claude-channel: {class:?} from {target} NOT approved; aborting");
        let _ = agent.send_dm(target, &note).await;
        return Ok(());
    }

    // 3. Approved → resume the SAME session in execution mode.
    tracing::info!("claude-channel: {class:?} from {target} approved; resuming {session_id}");
    stream_claude(
        agent,
        target,
        &ClaudeRun::resume(&session_id, "Approved. Proceed with the plan."),
    )
    .await?;
    Ok(())
}

/// One `claude -p` invocation's mode/arguments. Built via [`ClaudeRun::fresh`],
/// [`ClaudeRun::plan`], or [`ClaudeRun::resume`].
struct ClaudeRun<'a> {
    /// The prompt / message to send.
    prompt: &'a str,
    /// `--permission-mode <mode>` when `Some`. `plan` previews without
    /// executing; `acceptEdits` lets a resumed, already-approved run execute.
    permission_mode: Option<&'a str>,
    /// `--resume <session_id>` when `Some`, to continue a prior plan session.
    resume_session: Option<&'a str>,
}

impl<'a> ClaudeRun<'a> {
    /// A normal one-shot run (original behaviour) — no plan gate.
    fn fresh(prompt: &'a str) -> Self {
        Self { prompt, permission_mode: None, resume_session: None }
    }

    /// Plan mode: investigate + emit a plan, execute no destructive tool.
    fn plan(prompt: &'a str) -> Self {
        Self { prompt, permission_mode: Some("plan"), resume_session: None }
    }

    /// Resume a plan session in execution mode (post-approval).
    fn resume(session_id: &'a str, prompt: &'a str) -> Self {
        Self {
            prompt,
            permission_mode: Some("acceptEdits"),
            resume_session: Some(session_id),
        }
    }
}

/// Outcome of a streamed run that callers may need afterwards.
#[derive(Default)]
struct RunOutcome {
    /// The `claude` session id (from the `init` event), used to resume a plan.
    session_id: Option<String>,
}

/// Run `claude -p` in streaming mode and pipe its output into a single Matrix
/// message that is edited in place as tokens arrive. A typing indicator covers
/// the wait for the first token. Bounded by [`CLAUDE_TIMEOUT`]; uses whatever
/// `claude` is on PATH plus the absolute fallback matching the systemd unit's
/// `Environment=PATH`. Returns the run's [`RunOutcome`] (session id) so a plan
/// can be resumed for execution.
async fn stream_claude(
    agent: &AgentClient,
    target: &str,
    run: &ClaudeRun<'_>,
) -> anyhow::Result<RunOutcome> {
    let claude_bin = find_claude_bin();
    tracing::debug!(
        "invoking {} -p (stream-json, mode={:?}, resume={:?})",
        claude_bin,
        run.permission_mode,
        run.resume_session,
    );

    let mut cmd = Command::new(&claude_bin);
    cmd.arg("-p").arg(run.prompt);
    if let Some(session) = run.resume_session {
        cmd.arg("--resume").arg(session);
    }
    if let Some(mode) = run.permission_mode {
        cmd.arg("--permission-mode").arg(mode);
    }
    let mut child = cmd
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
    let mut outcome = RunOutcome::default();

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
            // {"type":"system","subtype":"init","session_id":"…"} — capture the
            // session id so a plan run can be resumed for execution.
            Some("system") => {
                if v.get("subtype").and_then(|s| s.as_str()) == Some("init") {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        outcome.session_id = Some(sid.to_string());
                    }
                }
            }
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
            // Terminal: {"type":"result","is_error":bool,"result":"<full
            // text>","session_id":"…"}. The session id also rides here — keep
            // it as a fallback if the init event was missed.
            Some("result") => {
                if outcome.session_id.is_none() {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        outcome.session_id = Some(sid.to_string());
                    }
                }
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
    Ok(outcome)
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

    run_daemon(config, &args.target, ClaudePHandler::default()).await;
}
