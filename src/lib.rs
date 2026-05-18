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
use serde::Deserialize;
use siwx_oidc_auth::SiwxKey;
use std::path::{Path, PathBuf};

pub struct AgentConfig {
    pub key_file: PathBuf,
    pub siwx_url: String,
    pub matrix_url: String,
    pub client_id: String,
    pub redirect_uri: String,
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

        tracing::info!("authenticating against {}", config.siwx_url);
        let tokens = siwx_oidc_auth::authenticate(
            &config.siwx_url,
            &config.client_id,
            &config.redirect_uri,
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
