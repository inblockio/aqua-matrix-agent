//! Generic Matrix agent daemon.
//!
//! This crate owns the *transport lifecycle* — authenticate via siwx-oidc,
//! connect to Matrix, stream-sync, rotate the client ahead of token expiry,
//! deduplicate inbound messages by timestamp watermark, and exit cleanly after
//! repeated connect failures so systemd can self-heal. It knows nothing about
//! *what an agent does* with a message.
//!
//! To build a new agent you implement [`MessageHandler`] and call
//! [`run_daemon`]. That's the whole surface — no Matrix types appear in the
//! trait, so a handler author never touches matrix-sdk directly. See
//! `examples/echo_agent.rs` for a complete ~30-line agent.
//!
//! matrix-sdk 0.17's deeply-nested async types overflow rustc's default
//! Send-bound recursion budget when we spawn `client.sync(...)` in a tokio
//! task (the same reason the agent lib raises it).
#![recursion_limit = "256"]
//!
//! The transport (siwx-oidc + Matrix) is intentionally NOT abstracted: this
//! crate's identity is "DID-authenticated, E2EE Matrix agent template," and a
//! `Transport` trait would dilute that. The seam is at the message boundary,
//! not the wire.
//!
//! ## Lifecycle (per client cycle)
//!
//! The outer loop in [`run_daemon`] builds a fresh [`AgentClient`] (which hits
//! the refresh-grant path, preserving `device_id` and the crypto store), then:
//!   1. joins any pending invites,
//!   2. sends the handler's one-time `hello()` on the first cycle only,
//!   3. upserts the fleet-registry entry,
//!   4. runs a sync stream + optional periodic tick until the token nears
//!      expiry, then drops the client and loops to rotate.
//!
//! This mirrors the matrix-sdk reality that the `Client` has no public API to
//! swap an access token in place; rotating the whole client ~30 s before expiry
//! is what avoids the `M_UNKNOWN_TOKEN` sync wedge.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use matrix_sdk::{
    config::SyncSettings,
    room::Room,
    ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType,
        events::{
            receipt::ReceiptThread,
            room::message::{MessageType, OriginalSyncRoomMessageEvent},
        },
    },
};

// Re-export everything a handler crate needs so it can depend on ONLY this
// crate (plus tokio for its `main`). A new agent never imports matrix-sdk.
// ReplyStream / TypingGuard let handlers stream answers and show "typing…".
pub use aqua_matrix_agent::{AgentClient, AgentConfig, ReplyStream, TypingGuard};
pub use async_trait::async_trait;

/// Rotate the client `REFRESH_GUARD_SECS` before the access token expires. 30 s
/// gives the refresh-grant round trip + a fresh initial sync ample headroom
/// inside the (typically 300 s) token TTL.
const REFRESH_GUARD_SECS: u64 = 30;
/// Minimum sleep between rotations, so a token that arrived near expiry still
/// gets one full cycle instead of hammering siwx-oidc.
const MIN_CYCLE_SECS: u64 = 15;
/// After this many consecutive [`AgentClient::connect`] failures, exit so
/// systemd's `Restart=always` brings up a fresh process. The connect path can
/// accumulate matrix-sdk / SQLite resources on partial failures; a fresh
/// process resets that. `StartLimitBurst` still guards against runaway
/// restarts.
const MAX_CONNECT_FAILURES: u32 = 3;

/// A concrete agent backend. The relay owns the Matrix lifecycle; the handler
/// owns "what to do with a message" and "what to do on each tick".
///
/// All methods are `&self`; the handler is shared (`Arc`) across the sync
/// stream and every spawned task, so keep interior state behind `Mutex`/atomics
/// if you need it. `handle_message` should return quickly — spawn a task for
/// slow work (an LLM call, a subprocess) so the sync stream keeps flowing.
#[async_trait]
pub trait MessageHandler: Send + Sync + 'static {
    /// Logical role for the fleet registry, e.g. `"heartbeat"`, `"claude-channel"`.
    fn role(&self) -> &str;

    /// systemd unit supervising this agent, recorded in the registry entry.
    /// `None` for ad-hoc runs with no supervising unit.
    fn systemd_unit(&self) -> Option<&str> {
        None
    }

    /// One-time greeting DM'd to `target` on the first successful connect.
    /// `None` stays silent.
    fn hello(&self, _agent: &AgentClient) -> Option<String> {
        None
    }

    /// Periodic tick interval. `None` (the default) disables the timer; the
    /// daemon then only reacts to inbound messages.
    fn tick_interval(&self) -> Option<Duration> {
        None
    }

    /// Called every [`tick_interval`](Self::tick_interval) (only when it is
    /// `Some`). The default is a no-op.
    async fn on_tick(&self, _agent: &AgentClient, _target: &str) {}

    /// Handle one inbound text message from `target`. The relay has already
    /// confirmed the sender and deduplicated by timestamp watermark, so this
    /// fires at most once per message. Errors are the handler's to log; the
    /// relay does not unwind on them.
    async fn handle_message(&self, agent: &AgentClient, target: &str, body: &str);
}

/// Run the agent daemon forever: connect, serve, rotate, repeat. Only returns
/// by `std::process::exit` after [`MAX_CONNECT_FAILURES`] consecutive connect
/// failures (so systemd restarts a clean process).
pub async fn run_daemon<H: MessageHandler>(config: AgentConfig, target: &str, handler: H) {
    let handler = Arc::new(handler);
    let target = Arc::new(target.to_string());

    tracing::info!(
        "{} daemon starting (target: {}, sync: stream, refresh-guard: {}s)",
        handler.role(),
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
                    "{}: AgentClient::connect failed ({consecutive_failures}/{MAX_CONNECT_FAILURES}): {e:#}",
                    handler.role(),
                );
                if consecutive_failures >= MAX_CONNECT_FAILURES {
                    tracing::error!(
                        "{}: {MAX_CONNECT_FAILURES} consecutive connect failures; exiting for systemd Restart=always (avoids in-process resource accumulation)",
                        handler.role(),
                    );
                    std::process::exit(1);
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        if let Err(e) = agent.join_invited_rooms().await {
            tracing::warn!("{}: join_invited_rooms failed: {e:#}", handler.role());
        }

        let now = unix_now_secs();
        let ttl = agent
            .expires_at_unix()
            .saturating_sub(now)
            .saturating_sub(REFRESH_GUARD_SECS)
            .max(MIN_CYCLE_SECS);
        let refresh_deadline = tokio::time::Instant::now() + Duration::from_secs(ttl);
        tracing::info!(
            "{}: client cycle starting (token ttl {}s, rotating in {}s)",
            handler.role(),
            agent.expires_at_unix().saturating_sub(now),
            ttl,
        );

        if first_cycle {
            if let Some(hello) = handler.hello(&agent) {
                if let Err(e) = agent.send_dm(&target, &hello).await {
                    tracing::warn!("{}: hello send failed: {e:#}", handler.role());
                }
            }
            first_cycle = false;
        }

        // Upsert the fleet-registry entry on every cycle start so a freshly
        // rotated session re-announces itself promptly. Best-effort: never let a
        // registry failure perturb the daemon.
        if let Err(e) = agent
            .update_registry(handler.role(), handler.systemd_unit())
            .await
        {
            tracing::warn!("{}: registry update failed: {e:#}", handler.role());
        }

        let exit = run_cycle(&agent, &target, &handler, refresh_deadline).await;
        tracing::info!("{}: cycle ended ({exit}); reconnecting", handler.role());
    }
}

async fn run_cycle<H: MessageHandler>(
    agent: &AgentClient,
    target: &Arc<String>,
    handler: &Arc<H>,
    refresh_deadline: tokio::time::Instant,
) -> &'static str {
    // Watermark starts at "now": ignore backlog, only react to messages that
    // arrive during this cycle. Advanced monotonically as messages are seen.
    let watermark = Arc::new(AtomicU64::new(now_epoch_ms()));
    register_handler(agent.clone(), target.clone(), handler.clone(), watermark);

    let sync_client = agent.client().clone();
    let mut sync_task = tokio::spawn(async move { sync_client.sync(SyncSettings::default()).await });

    let exit = match handler.tick_interval() {
        Some(interval) => {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick — the hello (or the previous cycle's
            // last tick) just went out; let the operator see the real cadence.
            tick.tick().await;
            loop {
                tokio::select! {
                    biased;
                    _ = tokio::time::sleep_until(refresh_deadline) => break "refresh-deadline",
                    res = &mut sync_task => {
                        log_sync_end(res);
                        break "sync-ended";
                    }
                    _ = tick.tick() => {
                        handler.on_tick(agent, target).await;
                    }
                }
            }
        }
        None => {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(refresh_deadline) => "refresh-deadline",
                res = &mut sync_task => {
                    log_sync_end(res);
                    "sync-ended"
                }
            }
        }
    };

    sync_task.abort();
    let _ = sync_task.await;
    exit
}

fn log_sync_end(res: Result<matrix_sdk::Result<()>, tokio::task::JoinError>) {
    match res {
        Ok(Ok(_)) => tracing::warn!("matrix sync returned Ok (unexpected)"),
        Ok(Err(e)) => tracing::warn!("matrix sync error: {e:#}"),
        Err(e) => tracing::warn!("matrix sync task join error: {e:#}"),
    }
}

fn register_handler<H: MessageHandler>(
    agent: AgentClient,
    target: Arc<String>,
    handler: Arc<H>,
    watermark: Arc<AtomicU64>,
) {
    agent.client().add_event_handler({
        let agent = agent.clone();
        move |ev: OriginalSyncRoomMessageEvent, room: Room| {
            let agent = agent.clone();
            let target = target.clone();
            let handler = handler.clone();
            let watermark = watermark.clone();
            async move {
                dispatch(ev, room, &agent, &target, &handler, &watermark).await;
            }
        }
    });
}

async fn dispatch<H: MessageHandler>(
    ev: OriginalSyncRoomMessageEvent,
    room: Room,
    agent: &AgentClient,
    target: &str,
    handler: &Arc<H>,
    watermark: &AtomicU64,
) {
    // Only messages from the configured peer, and only ones newer than anything
    // we've already seen this cycle.
    if ev.sender.as_str() != target {
        return;
    }
    let ts_ms = u64::from(ev.origin_server_ts.0);
    if ts_ms <= watermark.load(Ordering::Relaxed) {
        return;
    }

    // Instant "seen" acknowledgement (fire-and-forget): the user gets feedback
    // that the message landed before any handler latency. Best-effort.
    {
        let room = room.clone();
        let event_id = ev.event_id.clone();
        tokio::spawn(async move {
            let _ = room
                .send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id)
                .await;
        });
    }

    let body = match &ev.content.msgtype {
        MessageType::Text(t) => t.body.trim().to_string(),
        // Non-text content carries nothing for a handler; ignore without
        // advancing the watermark (the ts check still prevents reprocessing).
        _ => return,
    };

    // Advance the watermark BEFORE dispatching: if the handler (or the process)
    // dies mid-work we must not re-trigger on the same message after restart.
    watermark
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if ts_ms > v {
                Some(ts_ms)
            } else {
                None
            }
        })
        .ok();

    if body.is_empty() {
        return;
    }

    handler.handle_message(agent, target, &body).await;
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
