//! Claude-channel daemon: separate Matrix identity that forwards inbound DMs
//! from `--target` to `claude -p <prompt>` and DMs back the stdout.
//!
//! Each incoming Matrix message becomes a fresh `claude -p` invocation —
//! stateless per message. No conversation continuity for now (could be added
//! later via `claude -c <session-id>` keyed by room or user).
//!
//! Resilience: an outer loop owns AgentClient lifecycle and rotates it ~30 s
//! before the siwx-oidc access token expires, matching `heartbeat.rs`.
//!
//! See docs/ARCHITECTURE.md for the full design.
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use matrix_sdk::{
    config::SyncSettings,
    room::Room,
    ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent},
};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{AgentClient, AgentConfig};

const CLAUDE_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_REPLY_BYTES: usize = 16_000; // Matrix can take more, but be polite
const REFRESH_GUARD_SECS: u64 = 30;
const MIN_CYCLE_SECS: u64 = 15;
/// After this many consecutive `AgentClient::connect` failures, exit so
/// systemd's `Restart=always` brings up a fresh process. The connect path
/// can leak matrix-sdk / SQLite resources on partial-build failures; a
/// fresh process resets that state. `StartLimitBurst` still guards against
/// runaway restarts.
const MAX_CONNECT_FAILURES: u32 = 3;

pub async fn run(config: AgentConfig, target: &str) {
    let target = Arc::new(target.to_string());

    tracing::info!(
        "claude-channel daemon starting (target: {}, sync: stream, refresh-guard: {}s)",
        target,
        REFRESH_GUARD_SECS,
    );

    let mut first_cycle = true;
    let mut consecutive_failures: u32 = 0;
    loop {
        let agent = match AgentClient::connect(config.clone()).await {
            Ok(a) => {
                consecutive_failures = 0;
                a
            }
            Err(e) => {
                consecutive_failures += 1;
                tracing::error!(
                    "claude-channel: AgentClient::connect failed ({consecutive_failures}/{MAX_CONNECT_FAILURES}): {e:#}"
                );
                if consecutive_failures >= MAX_CONNECT_FAILURES {
                    tracing::error!(
                        "claude-channel: {MAX_CONNECT_FAILURES} consecutive connect failures; exiting for systemd Restart=always (avoids in-process resource accumulation)"
                    );
                    std::process::exit(1);
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        if let Err(e) = agent.join_invited_rooms().await {
            tracing::warn!("claude-channel: join_invited_rooms failed: {e:#}");
        }

        let now = unix_now_secs();
        let ttl = agent
            .expires_at_unix()
            .saturating_sub(now)
            .saturating_sub(REFRESH_GUARD_SECS)
            .max(MIN_CYCLE_SECS);
        let refresh_deadline = tokio::time::Instant::now() + Duration::from_secs(ttl);
        tracing::info!(
            "claude-channel: client cycle starting (token ttl {}s, rotating in {}s)",
            agent.expires_at_unix().saturating_sub(now),
            ttl,
        );

        if first_cycle {
            let hello = format!(
                "[hello] aqua-matrix-claude-channel online (identity: {}). DM me any text (without `#shell` prefix) and I will run `claude -p <your message>` and reply with the output. {}s timeout per invocation, stateless per message.",
                agent.user_id(),
                CLAUDE_TIMEOUT.as_secs(),
            );
            if let Err(e) = agent.send_dm(&target, &hello).await {
                tracing::warn!("hello send failed: {e:#}");
            }
            first_cycle = false;
        }

        // Upsert the fleet-registry entry on every client cycle start. The
        // channel has no periodic tick of its own, but the outer loop rotates
        // the client ~every token TTL (≈270s), so `last_online` is refreshed at
        // that cadence. Best-effort: never let a registry failure perturb the
        // daemon.
        if let Err(e) = agent.update_registry("claude-channel", Some("aqua-matrix-claude-channel")).await {
            tracing::warn!("registry update failed: {e:#}");
        }

        let exit = run_cycle(&agent, &target, refresh_deadline).await;
        tracing::info!("claude-channel: cycle ended ({exit}); reconnecting");
    }
}

async fn run_cycle(
    agent: &AgentClient,
    target: &Arc<String>,
    refresh_deadline: tokio::time::Instant,
) -> &'static str {
    let watermark = Arc::new(AtomicU64::new(now_epoch_ms()));
    register_handler(agent.clone(), target.clone(), watermark);

    let sync_client = agent.client().clone();
    let mut sync_task = tokio::spawn(async move {
        sync_client.sync(SyncSettings::default()).await
    });

    let exit = tokio::select! {
        biased;
        _ = tokio::time::sleep_until(refresh_deadline) => "refresh-deadline",
        res = &mut sync_task => {
            match res {
                Ok(Ok(_)) => tracing::warn!("matrix sync returned Ok (unexpected)"),
                Ok(Err(e)) => tracing::warn!("matrix sync error: {e:#}"),
                Err(e) => tracing::warn!("matrix sync task join error: {e:#}"),
            }
            "sync-ended"
        }
    };

    sync_task.abort();
    let _ = sync_task.await;
    exit
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn register_handler(agent: AgentClient, target: Arc<String>, watermark: Arc<AtomicU64>) {
    agent.client().add_event_handler({
        let agent = agent.clone();
        move |ev: OriginalSyncRoomMessageEvent, _room: Room| {
            let agent = agent.clone();
            let target = target.clone();
            let watermark = watermark.clone();
            async move {
                if let Err(e) = handle_event(ev, &agent, &target, &watermark).await {
                    tracing::warn!("claude-channel handler error: {e:#}");
                }
            }
        }
    });
}

async fn handle_event(
    ev: OriginalSyncRoomMessageEvent,
    agent: &AgentClient,
    target: &str,
    watermark: &AtomicU64,
) -> anyhow::Result<()> {
    if ev.sender.as_str() != target {
        return Ok(());
    }
    let ts_ms = u64::from(ev.origin_server_ts.0);
    if ts_ms <= watermark.load(Ordering::Relaxed) {
        return Ok(());
    }

    let body = match &ev.content.msgtype {
        MessageType::Text(t) => t.body.trim().to_string(),
        _ => return Ok(()),
    };

    if body.is_empty() {
        return Ok(());
    }

    // Skip #shell — that belongs to the heartbeat channel, not the LLM channel.
    if body.to_lowercase().starts_with("#shell") {
        watermark
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                if ts_ms > v {
                    Some(ts_ms)
                } else {
                    None
                }
            })
            .ok();
        return Ok(());
    }

    // Advance watermark BEFORE spawning claude — if we crash mid-claude we don't
    // want to re-trigger on restart.
    watermark
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if ts_ms > v {
                Some(ts_ms)
            } else {
                None
            }
        })
        .ok();

    tracing::info!("claude-channel prompt from {}: {} chars", target, body.len());

    // Spawn claude in its own task so sync keeps flowing.
    let agent = agent.clone();
    let target_owned = target.to_string();
    tokio::spawn(async move {
        let reply = match invoke_claude(&body).await {
            Ok(out) => out,
            Err(e) => format!("[claude-channel error] {e:#}"),
        };
        let reply = if reply.trim().is_empty() {
            "[claude-channel] (no output)".to_string()
        } else {
            truncate(&reply, MAX_REPLY_BYTES)
        };
        if let Err(e) = agent.send_dm(&target_owned, &reply).await {
            tracing::warn!("claude-channel reply send failed: {e:#}");
        }
    });

    Ok(())
}

/// Run `claude -p <prompt>` with stdin closed, capturing stdout. Bounded by
/// CLAUDE_TIMEOUT. Spawns headlessly — uses whatever `claude` is on PATH plus
/// the absolute fallback that matches the systemd unit's `Environment=PATH`.
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

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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

// Re-export anyhow::Context locally so the file is self-contained.
use anyhow::Context;
