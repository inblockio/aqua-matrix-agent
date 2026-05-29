// matrix-sdk 0.17's deeply-nested async types overflow rustc's default
// Send-bound recursion budget when a downstream crate spawns `client.sync(...)`
// in a tokio task (the daemon lifecycle now lives in `aqua-matrix-relay`, but
// the bound is hit through this crate's public API so the limit stays here).
#![recursion_limit = "256"]

mod recovery;
mod registry;

use anyhow::{anyhow, Context, Result};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use matrix_sdk::{
    config::SyncSettings,
    room::MessagesOptions,
    ruma::{
        events::{
            room::message::{MessageType, ReplacementMetadata, RoomMessageEventContent},
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        },
        OwnedDeviceId, OwnedEventId, OwnedUserId, RoomId, UInt, UserId,
    },
    Client, SessionMeta, SessionTokens,
};
use serde::{Deserialize, Serialize};
use siwx_oidc_auth::SiwxKey;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Config file (persisted OIDC credentials)
// ---------------------------------------------------------------------------

const DEFAULT_REDIRECT_URI: &str = "http://localhost:0/callback";
const DEFAULT_CLIENT_NAME: &str = "aqua-matrix-agent";

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub oidc: OidcConfig,
    /// Cached Matrix session. Lets restarts within the token's lifetime reuse
    /// the same `device_id` so the SQLite crypto store stays valid.
    /// See docs/ARCHITECTURE.md "Identity and device-id persistence".
    #[serde(default)]
    pub session: Option<SessionCache>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct OidcConfig {
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCache {
    pub access_token: String,
    pub user_id: String,
    pub device_id: String,
    /// Unix seconds when the access token expires. We refresh ~30s early.
    pub expires_at_unix: u64,
    /// Refresh token from siwx-oidc (24h TTL on standalone). When the
    /// access token has expired we exchange this for a new access token,
    /// preserving the device_id and crypto store.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Persisted DID — needed for the refresh-grant call.
    #[serde(default)]
    pub did: Option<String>,
}

impl ConfigFile {
    /// Load config from a TOML file. Returns default if file does not exist.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse config: {}", path.display()))
    }

    /// Save config to a TOML file, creating parent directories if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config dir: {}", parent.display()))?;
        }
        let contents = toml::to_string_pretty(self).context("failed to serialize config")?;
        std::fs::write(path, contents)
            .with_context(|| format!("failed to write config: {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// OIDC dynamic client registration
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RegisterResponse {
    client_id: String,
}

/// Register a new OIDC client with the siwx-oidc server via dynamic registration.
/// Returns (client_id, redirect_uri).
pub async fn register_oidc_client(siwx_url: &str) -> Result<(String, String)> {
    let redirect_uri = DEFAULT_REDIRECT_URI.to_string();
    let url = format!("{}/register", siwx_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "redirect_uris": [&redirect_uri],
        "client_name": DEFAULT_CLIENT_NAME,
        "token_endpoint_auth_method": "none"
    });

    tracing::info!("registering OIDC client at {url}");
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("OIDC client registration request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("OIDC registration returned {status}: {body}");
    }

    let reg: RegisterResponse = resp
        .json()
        .await
        .context("failed to parse OIDC registration response")?;

    tracing::info!("registered OIDC client: {}", reg.client_id);
    Ok((reg.client_id, redirect_uri))
}

// ---------------------------------------------------------------------------
// Agent config and resolution
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AgentConfig {
    pub key_file: PathBuf,
    pub siwx_url: String,
    pub matrix_url: String,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub store_dir: PathBuf,
}

pub struct Message {
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub event_id: String,
}

#[derive(Clone)]
pub struct AgentClient {
    client: Client,
    did: String,
    user_id: OwnedUserId,
    /// Unix seconds when the current access token expires. Used by the daemon
    /// outer-loop to proactively rotate the matrix-sdk Client (which has no
    /// public API to swap tokens in place) ~30 s before the server starts
    /// returning M_UNKNOWN_TOKEN.
    expires_at_unix: u64,
}

impl AgentClient {
    /// Expose the underlying matrix-sdk Client (cheaply cloneable, internally Arc'd)
    /// so the heartbeat and claude-channel modes can register event handlers and
    /// drive `client.sync(...)` directly.
    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn expires_at_unix(&self) -> u64 {
        self.expires_at_unix
    }
}

pub fn did_from_key_file(path: &Path) -> Result<String> {
    if path.exists() {
        let key = SiwxKey::from_pem_file(path).context("failed to load key")?;
        Ok(key.did())
    } else {
        let key = SiwxKey::generate_ed25519();
        std::fs::write(path, key.to_pem()?).context("failed to write key")?;
        Ok(key.did())
    }
}

#[derive(Deserialize)]
struct WhoAmI {
    user_id: String,
    device_id: String,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_store_mismatch(err: &anyhow::Error) -> bool {
    // matrix-sdk wraps this from the crypto store. The string is the load-bearing
    // signal — there is no typed error variant exposed at this layer.
    let chain: String = err.chain().map(|e| e.to_string()).collect::<Vec<_>>().join(" | ");
    chain.contains("account in the store doesn't match")
        || chain.contains("crypto store the account in the store")
}

fn wipe_crypto_store(store_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(store_dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("matrix-sdk-") && name.contains(".sqlite3") {
            let path = entry.path();
            if let Err(e) = std::fs::remove_file(&path) {
                tracing::warn!("failed to remove {}: {e}", path.display());
            } else {
                tracing::info!("wiped {}", path.display());
            }
        }
    }
}

async fn build_and_restore(
    matrix_url: &str,
    store_dir: &Path,
    user_id: &OwnedUserId,
    device_id: &OwnedDeviceId,
    access_token: &str,
) -> Result<Client> {
    let client = Client::builder()
        .homeserver_url(matrix_url)
        .sqlite_store(store_dir, None)
        .build()
        .await
        .context("failed to build Matrix client")?;

    let session = matrix_sdk::authentication::matrix::MatrixSession {
        meta: SessionMeta {
            user_id: user_id.clone(),
            device_id: device_id.clone(),
        },
        tokens: SessionTokens {
            access_token: access_token.to_string(),
            refresh_token: None,
        },
    };
    client
        .matrix_auth()
        .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
        .await
        .context("failed to restore session")?;
    Ok(client)
}

async fn resolve_identity(matrix_url: &str, access_token: &str) -> Result<WhoAmI> {
    let url = format!("{matrix_url}/_matrix/client/v3/account/whoami");
    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .context("whoami request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("whoami returned {status}: {body}");
    }
    resp.json().await.context("whoami JSON parse failed")
}

impl AgentClient {
    async fn find_dm_room(&self, target: &UserId) -> Option<matrix_sdk::Room> {
        if let Some(room) = self.client.get_dm_room(target) {
            return Some(room);
        }
        for room in self.client.joined_rooms() {
            if room.joined_members_count() == 2 {
                if room.get_member(target).await.ok().flatten().is_some() {
                    return Some(room);
                }
            }
        }
        None
    }

    pub async fn connect(config: AgentConfig) -> Result<Self> {
        let key = if config.key_file.exists() {
            tracing::info!("loading key from {}", config.key_file.display());
            SiwxKey::from_pem_file(&config.key_file).context("failed to load key")?
        } else {
            tracing::info!("generating Ed25519 key at {}", config.key_file.display());
            let key = SiwxKey::generate_ed25519();
            std::fs::write(&config.key_file, key.to_pem()?).context("failed to write key")?;
            key
        };
        let did = key.did();
        tracing::info!("agent DID: {did}");

        // Resolve client_id and redirect_uri:
        //   1. Provided in config (CLI flags or env vars)
        //   2. Cached in config file
        //   3. Auto-register with siwx-oidc server
        let config_path = config.store_dir.join("config.toml");
        let mut config_file = ConfigFile::load(&config_path).unwrap_or_default();

        let (client_id, redirect_uri) = match (config.client_id, config.redirect_uri) {
            (Some(cid), Some(ruri)) => (cid, ruri),
            (Some(cid), None) => {
                let ruri = config_file
                    .oidc
                    .redirect_uri
                    .clone()
                    .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string());
                (cid, ruri)
            }
            (None, Some(ruri)) => {
                let cid = config_file.oidc.client_id.clone().ok_or_else(|| {
                    anyhow!("--redirect-uri provided without --client-id, and no cached client_id found")
                })?;
                (cid, ruri)
            }
            (None, None) => {
                // Try config file first
                if let Some(cid) = config_file.oidc.client_id.clone() {
                    let ruri = config_file
                        .oidc
                        .redirect_uri
                        .clone()
                        .unwrap_or_else(|| DEFAULT_REDIRECT_URI.to_string());
                    tracing::info!("using cached OIDC credentials from {}", config_path.display());
                    (cid, ruri)
                } else {
                    // Auto-register
                    let (cid, ruri) = register_oidc_client(&config.siwx_url).await?;
                    config_file.oidc.client_id = Some(cid.clone());
                    config_file.oidc.redirect_uri = Some(ruri.clone());
                    config_file.save(&config_path)?;
                    tracing::info!("saved OIDC credentials to {}", config_path.display());
                    (cid, ruri)
                }
            }
        };

        // Resolve a usable Matrix session in this order of preference:
        //   1. Cached access token still within its TTL  → use it (no network).
        //   2. Cached refresh token            → exchange for a new access token via
        //                                       siwx_oidc_auth::refresh() (preserves
        //                                       device_id → crypto store stays valid).
        //   3. Fresh authentication            → mints a new device_id; the
        //                                       store-mismatch retry below wipes
        //                                       and rebuilds the crypto store.
        // See docs/ARCHITECTURE.md "Identity and device-id persistence".
        let now_unix = unix_now();
        let cached_clone = config_file.session.clone();

        let session_data: Option<(String, String, String, u64)> = if let Some(sess) = cached_clone {
            if sess.expires_at_unix > now_unix + 30 {
                tracing::info!(
                    "cached access token still valid ({}s left, device_id: {})",
                    sess.expires_at_unix.saturating_sub(now_unix),
                    sess.device_id
                );
                match resolve_identity(&config.matrix_url, &sess.access_token).await {
                    Ok(id) if id.user_id == sess.user_id && id.device_id == sess.device_id => {
                        tracing::info!("cached session validated; skipping auth");
                        Some((sess.access_token, sess.user_id, sess.device_id, sess.expires_at_unix))
                    }
                    Ok(_) => {
                        tracing::warn!("cached session whoami returned different identity; falling through");
                        None
                    }
                    Err(e) => {
                        tracing::warn!("cached session whoami failed: {e:#}; falling through");
                        None
                    }
                }
            } else if let (Some(rt), Some(saved_did)) = (sess.refresh_token.as_ref(), sess.did.as_ref()) {
                tracing::info!(
                    "access token expired; attempting refresh grant (device_id: {})",
                    sess.device_id
                );
                match siwx_oidc_auth::refresh(&config.siwx_url, &client_id, rt, saved_did).await {
                    Ok(new_tokens) => {
                        let expires_in = new_tokens.expires_in.unwrap_or(300);
                        tracing::info!(
                            "refresh grant succeeded (new access token expires in {}s, device_id preserved)",
                            expires_in
                        );
                        let refreshed = SessionCache {
                            access_token: new_tokens.access_token.clone(),
                            user_id: sess.user_id.clone(),
                            device_id: sess.device_id.clone(),
                            expires_at_unix: unix_now().saturating_add(expires_in),
                            // If the server rotated the refresh token, persist the new one;
                            // otherwise keep the old one (siwx-oidc may or may not rotate).
                            refresh_token: new_tokens.refresh_token.or_else(|| Some(rt.clone())),
                            did: Some(saved_did.clone()),
                        };
                        config_file.session = Some(refreshed.clone());
                        if let Err(e) = config_file.save(&config_path) {
                            tracing::warn!("failed to persist refreshed session: {e:#}");
                        }
                        Some((refreshed.access_token, refreshed.user_id, refreshed.device_id, refreshed.expires_at_unix))
                    }
                    Err(e) => {
                        tracing::warn!("refresh grant failed: {e:#}; falling through to fresh auth");
                        None
                    }
                }
            } else {
                tracing::info!("cached access token expired and no refresh_token; re-authenticating");
                None
            }
        } else {
            None
        };

        let (access_token, user_id_str, device_id_str, expires_at_unix) = match session_data {
            Some(quad) => quad,
            None => {
                tracing::info!("authenticating against {}", config.siwx_url);
                let tokens = siwx_oidc_auth::authenticate(
                    &config.siwx_url,
                    &client_id,
                    &redirect_uri,
                    &key,
                )
                .await
                .context("siwx-oidc authentication failed")?;
                let expires_in = tokens.expires_in.unwrap_or(300);
                tracing::info!(
                    "access token acquired (expires in {}s, refresh_token: {})",
                    expires_in,
                    if tokens.refresh_token.is_some() { "yes" } else { "no" }
                );

                let identity = resolve_identity(&config.matrix_url, &tokens.access_token).await?;
                tracing::info!(
                    "matrix user: {}, device: {}",
                    identity.user_id,
                    identity.device_id
                );

                let expires_at_unix = unix_now().saturating_add(expires_in);
                config_file.session = Some(SessionCache {
                    access_token: tokens.access_token.clone(),
                    user_id: identity.user_id.clone(),
                    device_id: identity.device_id.clone(),
                    expires_at_unix,
                    refresh_token: tokens.refresh_token,
                    did: Some(tokens.did.clone()),
                });
                if let Err(e) = config_file.save(&config_path) {
                    tracing::warn!("failed to persist session cache: {e:#} (continuing anyway)");
                }
                (tokens.access_token, identity.user_id, identity.device_id, expires_at_unix)
            }
        };

        let user_id: OwnedUserId = user_id_str
            .try_into()
            .map_err(|e| anyhow!("invalid user_id: {e}"))?;
        let device_id: OwnedDeviceId = device_id_str.into();

        std::fs::create_dir_all(&config.store_dir)?;

        // Build client + restore session, with one wipe-and-retry on store mismatch.
        // siwx-oidc mints a new device_id on every fresh auth; the SQLite crypto
        // store binds to the device_id it was created with. When they diverge we
        // wipe matrix-sdk-*.sqlite3* (NOT config.toml) and let cross-signing
        // re-bootstrap on the new device.
        let client = match build_and_restore(
            &config.matrix_url,
            &config.store_dir,
            &user_id,
            &device_id,
            &access_token,
        )
        .await
        {
            Ok(c) => c,
            Err(e) if is_store_mismatch(&e) => {
                tracing::warn!(
                    "crypto store device_id mismatch; wiping store and retrying once: {e:#}"
                );
                wipe_crypto_store(&config.store_dir);
                build_and_restore(
                    &config.matrix_url,
                    &config.store_dir,
                    &user_id,
                    &device_id,
                    &access_token,
                )
                .await
                .context("restore_session failed even after store wipe")?
            }
            Err(e) => return Err(e),
        };

        tracing::info!("running initial sync");
        client
            .sync_once(SyncSettings::default())
            .await
            .context("initial sync failed")?;
        tracing::info!("connected");

        // Cold-start recovery: if a recovery key was persisted and cross-signing
        // is NOT yet complete (e.g. after a store wipe), restore cross-signing +
        // secrets from server-side SSSS BEFORE the bootstrap decision below, so
        // restored keys make the status complete and bootstrap is skipped.
        recovery::restore_if_needed(&client, &config.store_dir).await;

        // Verify device via cross-signing
        tracing::info!("checking cross-signing status");
        match client.encryption().cross_signing_status().await {
            Some(status) if status.is_complete() => {
                tracing::info!(
                    "cross-signing keys already present (master={}, self_signing={}, user_signing={})",
                    status.has_master,
                    status.has_self_signing,
                    status.has_user_signing,
                );
            }
            _ => {
                tracing::info!("bootstrapping cross-signing keys");
                match client
                    .encryption()
                    .bootstrap_cross_signing(None)
                    .await
                {
                    Ok(()) => {
                        tracing::info!("cross-signing bootstrap complete; device is now verified");
                    }
                    Err(e) => {
                        tracing::warn!(
                            "cross-signing bootstrap failed (server may not support it): {e:#}"
                        );
                    }
                }
            }
        }

        // After cross-signing keys exist, enable recovery (creates SSSS +
        // server-side backup) and persist the generated recovery key to disk if
        // it isn't there yet. Best-effort: never fails connect().
        recovery::enable_and_persist_if_absent(&client, &config.store_dir).await;

        Ok(Self {
            client,
            did,
            user_id,
            expires_at_unix,
        })
    }

    pub fn did(&self) -> &str {
        &self.did
    }

    pub fn user_id(&self) -> &str {
        self.user_id.as_str()
    }

    pub async fn join_invited_rooms(&self) -> Result<Vec<String>> {
        let mut joined = Vec::new();
        for room in self.client.invited_rooms() {
            let room_id = room.room_id().to_owned();
            match room.join().await {
                Ok(_) => {
                    tracing::info!("joined invited room: {room_id}");
                    joined.push(room_id.to_string());
                }
                Err(e) => {
                    tracing::warn!("failed to join room {room_id}: {e}");
                }
            }
        }
        Ok(joined)
    }

    pub async fn dm_room_id(&self, target: &str) -> Result<Option<String>> {
        let target: &UserId = target
            .try_into()
            .map_err(|e| anyhow!("invalid user_id: {e}"))?;
        Ok(self
            .find_dm_room(target)
            .await
            .map(|r| r.room_id().to_string()))
    }

    pub async fn send_dm(&self, target: &str, message: &str) -> Result<String> {
        let target: &UserId = target
            .try_into()
            .map_err(|e| anyhow!("invalid target: {e}"))?;
        let room = match self.find_dm_room(target).await {
            Some(room) => room,
            None => self
                .client
                .create_dm(target)
                .await
                .context("create_dm failed")?,
        };
        // Best-effort: mark the room as a direct chat (m.direct global account
        // data) so it is resolvable server-side after a local store wipe. Only
        // mark when the room isn't ALREADY recorded as the DM for this target:
        // matrix-sdk's `mark_as_dm` appends to the m.direct list without
        // deduping, so marking on every send would grow the list with duplicate
        // room IDs unboundedly. `get_dm_room` reads the synced m.direct state.
        // Never fail the send because marking failed.
        let already_marked = self
            .client
            .get_dm_room(target)
            .is_some_and(|r| r.room_id() == room.room_id());
        if !already_marked {
            if let Err(e) = self
                .client
                .account()
                .mark_as_dm(room.room_id(), &[target.to_owned()])
                .await
            {
                tracing::warn!("failed to mark room as DM (m.direct): {e:#}");
            }
        }
        // Render the body as Markdown so Element (Web + X) display formatted
        // text. `text_markdown` attaches an `org.matrix.custom.html`
        // formatted_body (rendered HTML) and keeps the raw text as the plain
        // `body` fallback. Sending `text_plain` carries no formatted body, so
        // clients have nothing to render and show the raw markup verbatim.
        let resp = room
            .send(RoomMessageEventContent::text_markdown(message))
            .await
            .context("failed to send message")?;
        Ok(resp.response.event_id.to_string())
    }

    /// Upsert this agent's fleet-registry entry in global account data under
    /// the custom type `io.inblock.aqua.registry`. Best-effort by caller.
    pub async fn update_registry(&self, role: &str, systemd_unit: Option<&str>) -> Result<()> {
        use matrix_sdk::ruma::events::{
            AnyGlobalAccountDataEventContent, GlobalAccountDataEventType,
        };
        use matrix_sdk::ruma::serde::Raw;

        let last_online = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let content = registry::RegistryContent {
            did: self.did.clone(),
            user_id: self.user_id.to_string(),
            role: role.to_string(),
            systemd_unit: systemd_unit.map(|s| s.to_string()),
            last_online,
            version: env!("CARGO_PKG_VERSION").to_string(),
        };

        let raw: Raw<AnyGlobalAccountDataEventContent> = Raw::new(&content)
            .context("failed to serialize registry content")?
            .cast_unchecked();
        let event_type =
            GlobalAccountDataEventType::from(registry::REGISTRY_EVENT_TYPE.to_string());

        self.client
            .account()
            .set_account_data_raw(event_type, raw)
            .await
            .context("failed to write registry account data")?;
        Ok(())
    }

    pub async fn messages(&self, room_id: &str, limit: u32) -> Result<Vec<Message>> {
        let room_id: &RoomId = room_id
            .try_into()
            .map_err(|e| anyhow!("invalid room_id: {e}"))?;
        let room = self
            .client
            .get_room(room_id)
            .ok_or_else(|| anyhow!("room {room_id} not found"))?;

        let mut opts = MessagesOptions::backward();
        opts.limit = UInt::from(limit);
        let resp = room
            .messages(opts)
            .await
            .context("failed to fetch messages")?;

        let mut messages = Vec::new();
        for event in resp.chunk {
            let Some(event_id) = event.event_id() else {
                continue;
            };
            let Some(sender) = event.sender() else {
                continue;
            };
            let Some(timestamp) = event.timestamp() else {
                continue;
            };

            if event.kind.is_utd() {
                messages.push(Message {
                    sender: sender.to_string(),
                    body: "[unable to decrypt]".into(),
                    timestamp_ms: u64::from(timestamp.0),
                    event_id: event_id.to_string(),
                });
                continue;
            }

            let Ok(deserialized) = event.raw().deserialize() else {
                continue;
            };
            if let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                msg_event,
            )) = deserialized
            {
                if let Some(original) = msg_event.as_original() {
                    let body = match &original.content.msgtype {
                        MessageType::Text(text) => text.body.clone(),
                        MessageType::Notice(notice) => notice.body.clone(),
                        MessageType::Emote(emote) => emote.body.clone(),
                        _ => continue,
                    };
                    messages.push(Message {
                        sender: original.sender.to_string(),
                        body,
                        timestamp_ms: u64::from(original.origin_server_ts.0),
                        event_id: original.event_id.to_string(),
                    });
                }
            }
        }

        messages.reverse();
        Ok(messages)
    }

    pub async fn sync_once(&self) -> Result<()> {
        self.client
            .sync_once(SyncSettings::default())
            .await
            .context("sync failed")?;
        Ok(())
    }

    /// Show a Matrix typing indicator ("<agent> is typing…") to `target` until
    /// the returned guard is dropped. Returns `None` if no DM room exists yet.
    /// The guard refreshes the notice every few seconds (the homeserver expires
    /// it after ~4s) and clears it on drop — use it to signal "working" during a
    /// slow operation like an LLM call.
    pub async fn typing_guard(&self, target: &str) -> Option<TypingGuard> {
        let target: &UserId = target.try_into().ok()?;
        let room = self.find_dm_room(target).await?;
        Some(TypingGuard::start(room))
    }

    /// Begin a streaming reply to `target`: sends a placeholder message now and
    /// returns a handle whose [`ReplyStream::push`] / [`ReplyStream::finish`]
    /// edit that same message in place (Matrix `m.replace`), so an answer
    /// appears to type out instead of landing as one block. Errors if no DM room
    /// exists yet — callers should fall back to [`AgentClient::send_dm`].
    pub async fn reply_stream(&self, target: &str) -> Result<ReplyStream> {
        let target_uid: &UserId = target
            .try_into()
            .map_err(|e| anyhow!("invalid target: {e}"))?;
        let room = self
            .find_dm_room(target_uid)
            .await
            .ok_or_else(|| anyhow!("no DM room with {target} for a streaming reply"))?;
        ReplyStream::start(room).await
    }
}

/// How often to push an in-place edit while streaming. Editing per token would
/// flood the room with events (each a separate, E2E-encrypted `m.replace`); one
/// edit every ~700ms reads as smooth typing without the spam.
const STREAM_EDIT_INTERVAL: Duration = Duration::from_millis(700);
/// Cap on a streamed reply's size. Matrix accepts more, but be polite.
const STREAM_MAX_BYTES: usize = 16_000;

/// RAII typing indicator, created by [`AgentClient::typing_guard`]. Refreshes
/// the `m.typing` notice on a timer and clears it on drop.
pub struct TypingGuard {
    room: matrix_sdk::Room,
    keepalive: tokio::task::JoinHandle<()>,
}

impl TypingGuard {
    fn start(room: matrix_sdk::Room) -> Self {
        let r = room.clone();
        let keepalive = tokio::spawn(async move {
            // The homeserver expires a typing notice after ~4s; refresh inside
            // that window. matrix-sdk debounces the actual network sends.
            loop {
                if r.typing_notice(true).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
        Self { room, keepalive }
    }
}

impl Drop for TypingGuard {
    fn drop(&mut self) {
        self.keepalive.abort();
        // Drop can't be async; fire-and-forget the "stopped typing" notice.
        let room = self.room.clone();
        tokio::spawn(async move {
            let _ = room.typing_notice(false).await;
        });
    }
}

/// A reply that is edited in place as content arrives, created by
/// [`AgentClient::reply_stream`]. Backed by one message plus throttled
/// `m.replace` edits — the official Matrix way to approximate streaming, since
/// the protocol has no token-stream primitive.
pub struct ReplyStream {
    room: matrix_sdk::Room,
    event_id: OwnedEventId,
    buf: String,
    last_edit: Instant,
    pending: bool,
}

impl ReplyStream {
    async fn start(room: matrix_sdk::Room) -> Result<Self> {
        let resp = room
            .send(RoomMessageEventContent::text_plain("…"))
            .await
            .context("failed to send streaming placeholder")?;
        Ok(Self {
            room,
            event_id: resp.response.event_id.clone(),
            buf: String::new(),
            last_edit: Instant::now(),
            pending: false,
        })
    }

    /// Append `chunk` to the reply, editing the message in place at most once
    /// per [`STREAM_EDIT_INTERVAL`]. Intermediate states get a cursor and
    /// balanced code fences so half-streamed Markdown still renders cleanly.
    pub async fn push(&mut self, chunk: &str) -> Result<()> {
        self.buf.push_str(chunk);
        self.pending = true;
        if self.last_edit.elapsed() >= STREAM_EDIT_INTERVAL {
            let rendered = render_streaming(&self.buf);
            self.edit(&rendered).await?;
            self.last_edit = Instant::now();
            self.pending = false;
        }
        Ok(())
    }

    /// Finalize the reply. `final_text` (e.g. the authoritative full result)
    /// replaces the accumulated buffer when given; otherwise the buffer is used.
    /// The final edit drops the streaming cursor.
    pub async fn finish(self, final_text: Option<&str>) -> Result<()> {
        let text = final_text
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.buf.clone());
        let text = if text.trim().is_empty() {
            "(no output)".to_string()
        } else {
            truncate_bytes(&text, STREAM_MAX_BYTES)
        };
        self.edit(&text).await
    }

    async fn edit(&self, body: &str) -> Result<()> {
        // Build the replacement directly rather than via Room::make_edit_event,
        // which fetches the target event from the server on every call (a
        // round-trip per edit, and racy right after we send the placeholder).
        // We are the author and already hold the event id, so this is safe.
        let content = RoomMessageEventContent::text_markdown(body)
            .make_replacement(ReplacementMetadata::new(self.event_id.clone(), None));
        self.room
            .send(content)
            .await
            .context("failed to send streaming edit")?;
        Ok(())
    }
}

/// Render an in-progress buffer: cap length, balance an odd ``` fence so a
/// half-streamed code block doesn't swallow the rest, and append a cursor.
fn render_streaming(buf: &str) -> String {
    let mut s = truncate_bytes(buf, STREAM_MAX_BYTES);
    if s.matches("```").count() % 2 == 1 {
        s.push_str("\n```");
    }
    s.push_str(" ▌");
    s
}

/// Truncate `s` to at most `max_bytes`, on a UTF-8 char boundary.
fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n[…truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn config_file_roundtrip() {
        let dir = std::env::temp_dir().join("aqua-matrix-agent-test");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        // Load from nonexistent file returns defaults
        let cfg = ConfigFile::load(&path).unwrap();
        assert!(cfg.oidc.client_id.is_none());
        assert!(cfg.oidc.redirect_uri.is_none());

        // Save and reload
        let mut cfg = ConfigFile::default();
        cfg.oidc.client_id = Some("test-id-123".to_string());
        cfg.oidc.redirect_uri = Some("http://localhost:0/callback".to_string());
        cfg.save(&path).unwrap();

        let loaded = ConfigFile::load(&path).unwrap();
        assert_eq!(loaded.oidc.client_id.as_deref(), Some("test-id-123"));
        assert_eq!(
            loaded.oidc.redirect_uri.as_deref(),
            Some("http://localhost:0/callback")
        );

        // Verify the TOML format is human-readable
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[oidc]"));
        assert!(contents.contains("client_id = \"test-id-123\""));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_file_load_partial() {
        let dir = std::env::temp_dir().join("aqua-matrix-agent-test-partial");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Write a config with only client_id
        fs::write(&path, "[oidc]\nclient_id = \"partial-id\"\n").unwrap();
        let loaded = ConfigFile::load(&path).unwrap();
        assert_eq!(loaded.oidc.client_id.as_deref(), Some("partial-id"));
        assert!(loaded.oidc.redirect_uri.is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
