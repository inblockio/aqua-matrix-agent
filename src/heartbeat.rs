//! Heartbeat: periodically DM a status payload to a Matrix target.
//!
//! Payload bundles three categories of facts:
//!   * agent-side  — uptime since loop start, heartbeats sent, last error
//!   * host        — hostname, host uptime, load, free RAM, disk on /
//!   * Claude Code — most-recent active transcript's context usage, model, last tool/user
//!
//! The loop never returns under normal operation: send failures are logged and
//! retried on the next tick rather than crashing the daemon. Stop it with a
//! signal (SIGTERM/SIGINT) — systemd handles this for the user unit.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::AgentClient;

pub struct HeartbeatStats {
    start: Instant,
    sent: u64,
    last_err: Option<String>,
}

impl HeartbeatStats {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            sent: 0,
            last_err: None,
        }
    }
}

pub async fn run(agent: &AgentClient, target: &str, interval: Duration) {
    let mut stats = HeartbeatStats::new();
    tracing::info!(
        "heartbeat loop starting (interval: {}s, target: {target})",
        interval.as_secs()
    );

    loop {
        if let Err(e) = agent.sync_once().await {
            tracing::warn!("heartbeat: sync failed: {e:#}");
        }

        let body = build_status(&stats);
        match agent.send_dm(target, &body).await {
            Ok(event_id) => {
                stats.sent += 1;
                stats.last_err = None;
                tracing::info!("heartbeat sent (event: {event_id})");
            }
            Err(e) => {
                let msg = format!("{e:#}");
                tracing::warn!("heartbeat send failed: {msg}");
                stats.last_err = Some(msg);
            }
        }

        tokio::time::sleep(interval).await;
    }
}

fn build_status(stats: &HeartbeatStats) -> String {
    let mut out = String::new();
    out.push_str(&format!("aqua-matrix-agent heartbeat @ {}\n", now_string()));
    out.push_str("----------------------------------------\n");

    out.push_str(&format!(
        "agent : up {}, sent {}",
        format_duration(stats.start.elapsed()),
        stats.sent
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
