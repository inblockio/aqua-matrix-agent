//! Heartbeat daemon: 10-min status DM + `#shell` command channel, driven by
//! matrix-sdk's continuous sync stream (near-realtime, not polling).
//!
//! Three concurrent tasks per client lifecycle:
//!   1. background sync (`client.sync()` looping forever) — pulls events
//!   2. message event handler (registered before sync starts) — dispatches commands
//!   3. heartbeat timer (this function's main loop) — sends status every `interval`
//!
//! Resilience: an outer loop owns AgentClient lifecycle. ~30 s before the
//! siwx-oidc access token expires, the inner cycle returns, the AgentClient
//! is dropped, and a fresh one is built via `AgentClient::connect` — which
//! hits the tier-2 refresh-grant path, preserves device_id, and leaves the
//! crypto store untouched. This avoids the M_UNKNOWN_TOKEN sync-loop wedge
//! that matrix-sdk has no public hook to recover from on its own.
//!
//! See docs/ARCHITECTURE.md for the full design and rationale.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use matrix_sdk::{
    config::SyncSettings,
    room::Room,
    ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent},
};
use tokio::sync::Mutex;

use crate::{AgentClient, AgentConfig};

/// Refresh `REFRESH_GUARD_SECS` before the access token expires server-side.
/// 30 s gives the tier-2 refresh-grant round trip + new initial sync ample
/// headroom inside the (typically 300 s) token TTL.
const REFRESH_GUARD_SECS: u64 = 30;
/// Minimum sleep between rotations. Bounds the worst case where the token
/// already arrived close to expiry — better to rotate immediately than to
/// hammer siwx-oidc, but still give one heartbeat tick a chance to fire.
const MIN_CYCLE_SECS: u64 = 15;
/// After this many consecutive `AgentClient::connect` failures, exit so
/// systemd's `Restart=always` brings up a fresh process. The connect path
/// can leak matrix-sdk / SQLite resources on partial-build failures; a
/// fresh process resets that state. `StartLimitBurst` still guards against
/// runaway restarts.
const MAX_CONNECT_FAILURES: u32 = 3;

pub struct HeartbeatStats {
    start: Instant,
    sent: u64,
    last_err: Option<String>,
    commands_handled: u64,
}

impl HeartbeatStats {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            sent: 0,
            last_err: None,
            commands_handled: 0,
        }
    }
}

pub async fn run(config: AgentConfig, target: &str, interval: Duration) {
    // Stats survive client rotations — restart-on-token-rotation should look
    // like uninterrupted uptime to the operator, not a fresh boot.
    let stats = Arc::new(Mutex::new(HeartbeatStats::new()));
    let target = Arc::new(target.to_string());

    tracing::info!(
        "heartbeat daemon starting (heartbeat interval: {}s, target: {}, sync: stream, refresh-guard: {}s)",
        interval.as_secs(),
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
                    "heartbeat: AgentClient::connect failed ({consecutive_failures}/{MAX_CONNECT_FAILURES}): {e:#}"
                );
                if consecutive_failures >= MAX_CONNECT_FAILURES {
                    tracing::error!(
                        "heartbeat: {MAX_CONNECT_FAILURES} consecutive connect failures; exiting for systemd Restart=always (avoids in-process resource accumulation)"
                    );
                    std::process::exit(1);
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
                continue;
            }
        };

        if let Err(e) = agent.join_invited_rooms().await {
            tracing::warn!("heartbeat: join_invited_rooms failed: {e:#}");
        }

        let now = unix_now_secs();
        let ttl = agent
            .expires_at_unix()
            .saturating_sub(now)
            .saturating_sub(REFRESH_GUARD_SECS)
            .max(MIN_CYCLE_SECS);
        let refresh_deadline = tokio::time::Instant::now() + Duration::from_secs(ttl);
        tracing::info!(
            "heartbeat: client cycle starting (token ttl {}s, rotating in {}s)",
            agent.expires_at_unix().saturating_sub(now),
            ttl,
        );

        if first_cycle {
            let hello = format!(
                "[hello] aqua-matrix-heartbeat online @ {} (identity: {}). I send a status payload every {}s. Reply with `#shell help` for the command list.",
                now_string(),
                agent.user_id(),
                interval.as_secs(),
            );
            if let Err(e) = agent.send_dm(&target, &hello).await {
                tracing::warn!("hello send failed: {e:#}");
            }
            first_cycle = false;
        }

        // Upsert the fleet-registry entry on every client cycle start so a
        // freshly-rotated session re-announces itself promptly. Best-effort:
        // never let a registry failure perturb the heartbeat.
        if let Err(e) = agent.update_registry("heartbeat", Some("aqua-matrix-heartbeat")).await {
            tracing::warn!("registry update failed: {e:#}");
        }

        let exit = run_cycle(&agent, &target, interval, &stats, refresh_deadline).await;
        tracing::info!("heartbeat: cycle ended ({exit}); reconnecting");
    }
}

async fn run_cycle(
    agent: &AgentClient,
    target: &Arc<String>,
    interval: Duration,
    stats: &Arc<Mutex<HeartbeatStats>>,
    refresh_deadline: tokio::time::Instant,
) -> &'static str {
    let watermark = Arc::new(AtomicU64::new(now_epoch_ms()));
    register_command_handler(agent.clone(), target.clone(), stats.clone(), watermark);

    let sync_client = agent.client().clone();
    let mut sync_task = tokio::spawn(async move {
        sync_client.sync(SyncSettings::default()).await
    });

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick — the hello (or the previous cycle's last
    // heartbeat) just went out; let the operator see the cadence.
    tick.tick().await;

    let exit = loop {
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(refresh_deadline) => {
                break "refresh-deadline";
            }
            res = &mut sync_task => {
                match res {
                    Ok(Ok(_)) => tracing::warn!("matrix sync returned Ok (unexpected)"),
                    Ok(Err(e)) => tracing::warn!("matrix sync error: {e:#}"),
                    Err(e) => tracing::warn!("matrix sync task join error: {e:#}"),
                }
                break "sync-ended";
            }
            _ = tick.tick() => {
                send_heartbeat(agent, target, stats).await;
            }
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

fn register_command_handler(
    agent: AgentClient,
    target: Arc<String>,
    stats: Arc<Mutex<HeartbeatStats>>,
    watermark: Arc<AtomicU64>,
) {
    agent.client().add_event_handler({
        let agent = agent.clone();
        move |ev: OriginalSyncRoomMessageEvent, _room: Room| {
            let agent = agent.clone();
            let target = target.clone();
            let stats = stats.clone();
            let watermark = watermark.clone();
            async move {
                if let Err(e) = handle_event(ev, &agent, &target, &stats, &watermark).await {
                    tracing::warn!("command handler error: {e:#}");
                }
            }
        }
    });
}

async fn handle_event(
    ev: OriginalSyncRoomMessageEvent,
    agent: &AgentClient,
    target: &str,
    stats: &Arc<Mutex<HeartbeatStats>>,
    watermark: &AtomicU64,
) -> anyhow::Result<()> {
    if ev.sender.as_str() != target {
        return Ok(());
    }
    let ts_ms = u64::from(ev.origin_server_ts.0);
    // Atomic compare-and-swap-up: only advance, never go backward
    let prev = watermark.load(Ordering::Relaxed);
    if ts_ms <= prev {
        return Ok(());
    }

    let body = match &ev.content.msgtype {
        MessageType::Text(t) => t.body.trim().to_string(),
        _ => return Ok(()),
    };

    let lower = body.to_lowercase();
    if !(lower.starts_with("#shell ") || lower == "#shell") {
        // Non-command message from target — advance watermark so we don't keep
        // reconsidering it on future ticks, but don't reply.
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

    tracing::info!("command from {}: {}", target, body);

    let reply = {
        let stats_guard = stats.lock().await;
        handle_command(&body, &stats_guard)
    };

    watermark
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            if ts_ms > v {
                Some(ts_ms)
            } else {
                None
            }
        })
        .ok();

    {
        let mut s = stats.lock().await;
        s.commands_handled += 1;
    }

    if let Err(e) = agent.send_dm(target, &reply).await {
        tracing::warn!("command reply send failed: {e:#}");
    }

    Ok(())
}

async fn send_heartbeat(agent: &AgentClient, target: &str, stats: &Arc<Mutex<HeartbeatStats>>) {
    // Refresh the fleet-registry entry every tick so `last_online` stays
    // current. Best-effort — a registry failure must never skip the heartbeat.
    if let Err(e) = agent.update_registry("heartbeat", Some("aqua-matrix-heartbeat")).await {
        tracing::warn!("registry update failed: {e:#}");
    }

    let body = {
        let s = stats.lock().await;
        build_status(&s)
    };
    match agent.send_dm(target, &body).await {
        Ok(event_id) => {
            let mut s = stats.lock().await;
            s.sent += 1;
            s.last_err = None;
            tracing::info!("heartbeat sent (event: {event_id})");
        }
        Err(e) => {
            let msg = format!("{e:#}");
            tracing::warn!("heartbeat send failed: {msg}");
            let mut s = stats.lock().await;
            s.last_err = Some(msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

fn handle_command(input: &str, stats: &HeartbeatStats) -> String {
    // input starts with "#shell" (case-insensitive). parts[0]="#shell",
    // parts[1]=subcommand, parts[2..]=args.
    let parts: Vec<&str> = input.split_whitespace().collect();
    let cmd = parts.get(1).copied().unwrap_or("").to_lowercase();

    match cmd.as_str() {
        "" | "help" => help_text(),
        "ping" => format!("pong @ {}", now_string()),
        "status" => build_status(stats),
        "uptime" => format!(
            "agent up {} | host up {}",
            format_duration(stats.start.elapsed()),
            host_uptime().unwrap_or_else(|| "?".into()),
        ),
        "restart" => spawn_systemctl_restart("aqua-matrix-heartbeat"),
        "respawn" => spawn_systemctl_restart("claude-bridge"),
        "respawn-channel" => spawn_systemctl_restart("aqua-matrix-claude-channel"),
        "logs" => {
            let n = parts
                .get(2)
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(10)
                .clamp(1, 50);
            recent_logs("aqua-matrix-heartbeat", n)
                .map(|logs| format!("```\n{}\n```", logs.trim_end()))
                .unwrap_or_else(|| "could not read journal logs".into())
        }
        other => format!("unknown command: {other}\n\n{}", help_text()),
    }
}

fn help_text() -> String {
    // The aligned command table is wrapped in a fenced code block so it renders
    // as a monospace panel in Element (HTML collapses the alignment spaces and
    // would treat the `#shell`-prefixed lines as headings otherwise). The
    // surrounding sentences stay as prose so they render as normal text.
    [
        "**aqua-matrix-agent heartbeat** — supported commands (prefix `#shell`):",
        "```",
        "#shell help              this message",
        "#shell status            send a status payload now",
        "#shell ping              reply pong + timestamp",
        "#shell uptime            agent + host uptime",
        "#shell restart           restart the heartbeat systemd unit",
        "#shell respawn           restart claude-bridge (local interactive Claude in tmux)",
        "#shell respawn-channel   restart aqua-matrix-claude-channel (the Matrix LLM channel)",
        "#shell logs [N]          last N journal lines (default 10, max 50)",
        "```",
        "Commands are honored only when sender matches the configured `--target`.",
    ]
    .join("\n")
}

fn spawn_systemctl_restart(unit: &str) -> String {
    match std::process::Command::new("systemctl")
        .args(["--user", "restart", unit])
        .spawn()
    {
        Ok(_) => format!("restarting {unit} (systemctl --user restart {unit})"),
        Err(e) => format!("restart of {unit} failed to spawn systemctl: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Status payload
// ---------------------------------------------------------------------------

fn build_status(stats: &HeartbeatStats) -> String {
    // Wrapped in a fenced code block: the panel uses a `----` divider and
    // column-aligned labels, which Element would otherwise mangle (the divider
    // turns the line above into a setext heading and HTML collapses alignment).
    let mut out = String::from("```\n");
    out.push_str(&format!("aqua-matrix-agent heartbeat @ {}\n", now_string()));
    out.push_str("----------------------------------------\n");

    out.push_str(&format!(
        "agent : up {}, sent {}, cmds {}",
        format_duration(stats.start.elapsed()),
        stats.sent,
        stats.commands_handled,
    ));
    if let Some(err) = &stats.last_err {
        out.push_str(&format!(", last_err: {}", truncate(err, 120)));
    }
    out.push('\n');

    out.push_str("host  : ");
    out.push_str(&host_summary());
    out.push('\n');

    if let Some(claude) = claude_session_summary() {
        out.push_str("claude: ");
        out.push_str(&claude);
        out.push('\n');
    } else {
        out.push_str("claude: no active transcript\n");
    }

    out.push_str("```");
    out
}

// ---------------------------------------------------------------------------
// Host facts
// ---------------------------------------------------------------------------

fn host_summary() -> String {
    let hostname = read_trim("/proc/sys/kernel/hostname").unwrap_or_else(|| "?".into());
    let uptime = host_uptime().unwrap_or_else(|| "?".into());
    let load = read_trim("/proc/loadavg")
        .map(|s| {
            s.split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| "?".into());
    let mem = memory_summary().unwrap_or_else(|| "?".into());
    let disk = disk_summary("/").unwrap_or_else(|| "?".into());
    format!("{hostname} | up {uptime} | load {load} | mem {mem} | disk {disk}")
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.lines().next().map(|s| s.trim().to_string()))
}

fn host_uptime() -> Option<String> {
    let s = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = s.split_whitespace().next()?.parse().ok()?;
    Some(format_duration(Duration::from_secs(secs as u64)))
}

fn memory_summary() -> Option<String> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut total_kb = 0u64;
    let mut avail_kb = 0u64;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_kb = parse_meminfo_kb(rest)?;
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            avail_kb = parse_meminfo_kb(rest)?;
        }
    }
    if total_kb == 0 {
        return None;
    }
    let total_gb = total_kb as f64 / 1024.0 / 1024.0;
    let avail_gb = avail_kb as f64 / 1024.0 / 1024.0;
    let used_pct = ((total_kb - avail_kb) as f64 / total_kb as f64) * 100.0;
    Some(format!(
        "{avail_gb:.1}/{total_gb:.1}GB free ({used_pct:.0}% used)"
    ))
}

fn parse_meminfo_kb(s: &str) -> Option<u64> {
    s.split_whitespace().next()?.parse().ok()
}

fn disk_summary(path: &str) -> Option<String> {
    let out = std::process::Command::new("df")
        .args(["-BG", "--output=avail,pcent", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 2 {
        return None;
    }
    Some(format!("{} free ({} used)", fields[0], fields[1]))
}

// ---------------------------------------------------------------------------
// Claude Code session facts
// ---------------------------------------------------------------------------

fn claude_session_summary() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let projects = PathBuf::from(home).join(".claude/projects");
    let transcript = find_latest_transcript(&projects)?;

    let content = std::fs::read_to_string(&transcript).ok()?;

    let mut input_tokens: u64 = 0;
    let mut model: Option<String> = None;
    let mut last_user: Option<String> = None;
    let mut last_tool: Option<String> = None;

    for line in content.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let msg = v.get("message").unwrap_or(&v);
        let role = msg.get("role").and_then(|r| r.as_str());

        match role {
            Some("user") => {
                last_user = extract_text_from_content(msg.get("content"));
            }
            Some("assistant") => {
                if let Some(usage) = msg.get("usage") {
                    let total = field_u64(usage, "input_tokens")
                        + field_u64(usage, "cache_read_input_tokens")
                        + field_u64(usage, "cache_creation_input_tokens");
                    if total > input_tokens {
                        input_tokens = total;
                        model = msg
                            .get("model")
                            .and_then(|v| v.as_str())
                            .map(String::from);
                    }
                }
                if let Some(arr) = msg.get("content").and_then(|v| v.as_array()) {
                    for item in arr {
                        if item.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                                last_tool = Some(name.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if input_tokens == 0 {
        return None;
    }

    let window = context_window_for(model.as_deref());
    let pct = (input_tokens as f64 / window as f64 * 100.0).round() as u64;

    let session = transcript
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let project = transcript
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let session_short: String = session.chars().take(8).collect();

    let model_str = model.as_deref().unwrap_or("?");
    let mut out = format!(
        "{} | ctx ~{}% of {} ({}) | session {}",
        project,
        pct,
        format_tokens(window),
        model_str,
        session_short,
    );
    if let Some(tool) = last_tool {
        out.push_str(&format!(" | last_tool: {tool}"));
    }
    if let Some(user) = last_user {
        out.push_str(&format!(" | last_user: \"{}\"", truncate(&user, 80)));
    }
    Some(out)
}

fn field_u64(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn extract_text_from_content(content: Option<&serde_json::Value>) -> Option<String> {
    let content = content?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for item in arr {
            if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                return Some(t.to_string());
            }
        }
    }
    None
}

fn find_latest_transcript(root: &Path) -> Option<PathBuf> {
    let mut latest: Option<(PathBuf, std::time::SystemTime)> = None;
    walk(root, &mut |p| {
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            return;
        }
        let Ok(meta) = std::fs::metadata(p) else { return };
        let Ok(modified) = meta.modified() else { return };
        match &latest {
            None => latest = Some((p.to_path_buf(), modified)),
            Some((_, lm)) if modified > *lm => latest = Some((p.to_path_buf(), modified)),
            _ => {}
        }
    });
    latest.map(|(p, _)| p)
}

fn walk(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, f);
        } else {
            f(&path);
        }
    }
}

fn context_window_for(model: Option<&str>) -> u64 {
    if let Ok(s) = std::env::var("CONTEXT_WINDOW") {
        if let Ok(n) = s.parse() {
            return n;
        }
    }
    match model {
        Some(m) if m.contains("[1m]") => 1_000_000,
        _ => 200_000,
    }
}

// ---------------------------------------------------------------------------
// Journal access
// ---------------------------------------------------------------------------

fn recent_logs(unit: &str, n: usize) -> Option<String> {
    let out = std::process::Command::new("journalctl")
        .args([
            "--user",
            "-u",
            unit,
            "--no-pager",
            "-n",
            &n.to_string(),
            "-o",
            "short",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ---------------------------------------------------------------------------
// Misc formatting
// ---------------------------------------------------------------------------

fn now_string() -> String {
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "?".into())
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let days = secs / 86_400;
    let h = (secs % 86_400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.replace('\n', " ")
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push_str("...");
        out.replace('\n', " ")
    }
}
