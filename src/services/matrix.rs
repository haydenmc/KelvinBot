use std::{fmt, path::PathBuf};

use anyhow::{Result, bail};
use matrix_sdk::{
    Client, Room, RoomState,
    config::SyncSettings,
    encryption::{self, EncryptionSettings},
    ruma::{
        events::{
            True,
            message::OriginalSyncMessageEvent,
            room::{
                member::{MembershipState, StrippedRoomMemberEvent},
                message::{MessageType, OriginalSyncRoomMessageEvent, SyncRoomMessageEvent},
            },
        },
        user_id,
    },
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use url::Url;

use crate::core::{
    event::{
        Event,
        EventKind::{self, RoomMessage},
    },
    service::{self, Service, ServiceId},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MatrixUserId(pub String);

impl fmt::Display for MatrixUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the inner string
        write!(f, "{}", self.0)
    }
}

pub struct MatrixService {
    id: ServiceId,
    homeserver_url: Url,
    user_id: MatrixUserId,
    password: SecretString,
    device_id: String,
    evt_tx: tokio::sync::mpsc::Sender<Event>,
    db_directory: PathBuf,
    db_passphrase: SecretString,
}

impl MatrixService {
    pub fn new(
        id: ServiceId,
        homeserver_url: Url,
        user_id: MatrixUserId,
        password: SecretString,
        device_id: String,
        evt_tx: tokio::sync::mpsc::Sender<Event>,
        data_directory: PathBuf,
        db_passphrase: SecretString,
    ) -> Self {
        // Create storage directory
        let mut sqlite_path = data_directory.clone();
        sqlite_path.push("matrix");
        sqlite_path.push(format!("{}", id.0));
        std::fs::create_dir_all(&sqlite_path).expect("Failed to create storage directory");

        Self {
            id,
            homeserver_url,
            user_id,
            password,
            device_id,
            evt_tx,
            db_directory: sqlite_path,
            db_passphrase,
        }
    }

    async fn setup_event_handlers(&self, client: &Client) -> anyhow::Result<()> {
        // Handle room invites
        client.add_event_handler(
            |event: StrippedRoomMemberEvent, room: Room, client: Client| async move {
                info!("Received room invite for room: {}", room.room_id());
                if let Some(user_id) = client.user_id() {
                    if event.state_key == user_id
                        && event.content.membership == MembershipState::Invite
                    {
                        let bot_server = user_id.server_name();
                        if let Some(room_server) = room.room_id().server_name() {
                            if bot_server == room_server {
                                match room.join().await {
                                    Ok(_) => {
                                        info!(room_id=%room.room_id(), "Successfully joined room")
                                    }
                                    Err(e) => error!("Failed to accept invite: {}", e),
                                }
                            } else {
                                info!(room_server=%room_server, bot_server=%bot_server,
                                    "Ignoring room invite from different homeserver.")
                            }
                        } else {
                            warn!("Room invite is missing server name");
                        }
                    }
                } else {
                    warn!("Client user_id is None, cannot process invite");
                }
            },
        );
        // Handle room messages
        let service_id = self.id.clone();
        let evt_tx = self.evt_tx.clone();
        client.add_event_handler(
            |event: OriginalSyncRoomMessageEvent, room: Room, client: Client| async move {
                if room.state() != RoomState::Joined {
                    return;
                }

                let MessageType::Text(text_content) = event.content.msgtype else {
                    return;
                };

                let Ok(is_direct) = room.is_direct().await else {
                    warn!("could not determine if message was a direct message");
                    return;
                };

                match is_direct {
                    true => {
                        let event = Event {
                            service_id: service_id,
                            kind: EventKind::DirectMessage {
                                user_id: event.sender.to_string(),
                                body: text_content.body,
                            },
                        };

                        let _ = evt_tx.send(event).await;
                    }
                    false => {
                        let event = Event {
                            service_id: service_id,
                            kind: EventKind::RoomMessage {
                                room_id: room.room_id().to_string(),
                                body: text_content.body,
                            },
                        };

                        let _ = evt_tx.send(event).await;
                    }
                }
            },
        );
        Ok(())
    }
}
#[async_trait::async_trait]
impl Service for MatrixService {
    fn id(&self) -> service::ServiceId {
        self.id.clone()
    }
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        info!(service_id=%self.id, homeserver_url=%self.homeserver_url, user_id=%self.user_id,
            "starting matrix service");
        let client = Client::builder()
            .homeserver_url(self.homeserver_url.clone())
            .sqlite_store(&self.db_directory, self.db_passphrase.expose_secret().into())
            .with_encryption_settings(EncryptionSettings {
                backup_download_strategy: encryption::BackupDownloadStrategy::Manual,
                auto_enable_cross_signing: false,
                auto_enable_backups: false,
            })
            .build()
            .await?;
        match client
            .matrix_auth()
            .login_username(self.user_id.to_string(), self.password.expose_secret())
            .device_id(&self.device_id)
            .send()
            .await
        {
            Ok(_) => {
                info!("login successful");
            }
            Err(e) => {
                error!(error=%e, "login error");
                bail!("login error")
            }
        }
        // An initial sync to set up state and so our bot doesn't respond to old messages.
        client.sync_once(SyncSettings::default()).await?;
        self.setup_event_handlers(&client).await?;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(service=%self.id, "shutdown requested");
                    break;
                }
                result = client.sync(SyncSettings::default()) => {
                    warn!("matrix sync returned");
                }
            }
        }
        Ok(())
    }
}
