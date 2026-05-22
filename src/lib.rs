pub mod heartbeat;

use anyhow::{anyhow, Context, Result};
use matrix_sdk::{
    config::SyncSettings,
    room::MessagesOptions,
    ruma::{
        events::{
            room::message::{MessageType, RoomMessageEventContent},
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        },
        OwnedDeviceId, OwnedUserId, RoomId, UInt, UserId,
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
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct OidcConfig {
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
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

pub struct AgentClient {
    client: Client,
    did: String,
    user_id: OwnedUserId,
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

        tracing::info!("authenticating against {}", config.siwx_url);
        let tokens = siwx_oidc_auth::authenticate(
            &config.siwx_url,
            &client_id,
            &redirect_uri,
            &key,
        )
        .await
        .context("siwx-oidc authentication failed")?;
        tracing::info!(
            "access token acquired (expires in {}s)",
            tokens.expires_in.unwrap_or(0)
        );

        tracing::info!("resolving Matrix identity");
        let identity = resolve_identity(&config.matrix_url, &tokens.access_token).await?;
        tracing::info!(
            "matrix user: {}, device: {}",
            identity.user_id,
            identity.device_id
        );

        let user_id: OwnedUserId = identity
            .user_id
            .try_into()
            .map_err(|e| anyhow!("invalid user_id: {e}"))?;
        let device_id: OwnedDeviceId = identity.device_id.into();

        std::fs::create_dir_all(&config.store_dir)?;
        let client = Client::builder()
            .homeserver_url(&config.matrix_url)
            .sqlite_store(&config.store_dir, None)
            .build()
            .await
            .context("failed to build Matrix client")?;

        let session = matrix_sdk::authentication::matrix::MatrixSession {
            meta: SessionMeta {
                user_id: user_id.clone(),
                device_id,
            },
            tokens: SessionTokens {
                access_token: tokens.access_token,
                refresh_token: None,
            },
        };
        client
            .matrix_auth()
            .restore_session(session, matrix_sdk::store::RoomLoadSettings::default())
            .await
            .context("failed to restore session")?;

        tracing::info!("running initial sync");
        client
            .sync_once(SyncSettings::default())
            .await
            .context("initial sync failed")?;
        tracing::info!("connected");

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

        Ok(Self {
            client,
            did,
            user_id,
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
        let resp = room
            .send(RoomMessageEventContent::text_plain(message))
            .await
            .context("failed to send message")?;
        Ok(resp.response.event_id.to_string())
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
