use std::{fmt, path::PathBuf};

use anyhow::{Result, bail};
use matrix_sdk::{
    Client, Room, RoomMemberships, RoomState,
    config::SyncSettings,
    encryption::{self, EncryptionSettings},
    ruma::{
        RoomId, UserId,
        events::room::{
            member::{MembershipState, StrippedRoomMemberEvent},
            message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
        },
    },
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use url::Url;

use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    service::{Service, ServiceId},
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
    user_id: MatrixUserId,
    password: SecretString,
    device_id: String,
    evt_tx: tokio::sync::mpsc::Sender<Event>,
    client: Client,
}

impl MatrixService {
    #[allow(clippy::too_many_arguments)] // TODO: make this less gross
    pub async fn create(
        id: ServiceId,
        homeserver_url: Url,
        user_id: MatrixUserId,
        password: SecretString,
        device_id: String,
        evt_tx: tokio::sync::mpsc::Sender<Event>,
        data_directory: PathBuf,
        db_passphrase: SecretString,
    ) -> Result<Self> {
        // Create storage directory
        let mut sqlite_path = data_directory.clone();
        sqlite_path.push("matrix");
        sqlite_path.push(id.to_string());
        std::fs::create_dir_all(&sqlite_path).expect("Failed to create storage directory");

        let client = Client::builder()
            .homeserver_url(homeserver_url.clone())
            .sqlite_store(sqlite_path, db_passphrase.expose_secret().into())
            .with_encryption_settings(EncryptionSettings {
                backup_download_strategy: encryption::BackupDownloadStrategy::Manual,
                auto_enable_cross_signing: true,
                auto_enable_backups: false,
            })
            .build()
            .await?;

        Ok(Self { id, user_id, password, device_id, evt_tx, client })
    }

    async fn find_or_create_dm(&self, user_id: &UserId) -> Result<Room, matrix_sdk::Error> {
        // First, try to find existing DM room
        for room in self.client.rooms() {
            if room.state() == RoomState::Joined {
                // Check if it's a DM with this specific user
                if let Ok(true) = room.is_direct().await
                    && let Ok(members) = room.members(RoomMemberships::ACTIVE).await
                    && members.len() == 2
                    && members.iter().any(|m| m.user_id() == user_id)
                {
                    debug!("found existing DM room: {}", room.room_id());
                    return Ok(room);
                }
            }
        }

        // If not found, create new DM room
        debug!("creating new DM room for {}", user_id);
        self.client.create_dm(user_id).await
    }

    async fn setup_event_handlers(&self) -> anyhow::Result<()> {
        // Handle room invites
        self.client.add_event_handler(
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
        self.client.add_event_handler(
            |event: OriginalSyncRoomMessageEvent, room: Room, _client: Client| async move {
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
                            service_id,
                            kind: EventKind::DirectMessage {
                                user_id: event.sender.to_string(),
                                body: text_content.body,
                            },
                        };

                        let _ = evt_tx.send(event).await;
                    }
                    false => {
                        let event = Event {
                            service_id,
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
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        info!(service_id=%self.id, homeserver_url=%self.client.homeserver(), user_id=%self.user_id,
            "starting matrix service");

        // Attempt to authenticate
        match self
            .client
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
        self.client.sync_once(SyncSettings::default()).await?;

        // Verify our own device
        if let Some(device) = self.client.encryption().get_own_device().await? {
            if !device.is_verified() {
                info!("matrix device is not verified; self-verifying...");
                if let Err(e) = device.verify().await {
                    error!(error=%e, "could not self-verify device");
                } else {
                    info!("device self-verified!");
                }
            } else {
                info!("matrix device is already verified");
            }
        } else {
            error!("could not fetch matrix device");
        }

        // Set up event handlers
        self.setup_event_handlers().await?;

        // Begin listening
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(service=%self.id, "shutdown requested");
                    break;
                }
                _result = self.client.sync(SyncSettings::default()) => {
                    warn!("matrix sync returned");
                }
            }
        }
        Ok(())
    }

    async fn handle_command(&self, command: Command) -> Result<()> {
        match command {
            Command::SendDirectMessage { user_id, body, .. } => {
                info!(service=%self.id, user_id=%user_id, body=%body, "sending DM");

                // Parse the user ID
                let user_id = match UserId::parse(&user_id) {
                    Ok(uid) => uid,
                    Err(e) => {
                        error!(user_id=%user_id, error=%e, "invalid user ID");
                        return Ok(());
                    }
                };

                // Find existing or create new DM room
                match self.find_or_create_dm(&user_id).await {
                    Ok(room) => {
                        let content = RoomMessageEventContent::text_plain(&body);
                        if let Err(e) = room.send(content).await {
                            error!(error=%e, "failed to send DM");
                        } else {
                            debug!("DM sent successfully");
                        }
                    }
                    Err(e) => {
                        error!(error=%e, user_id=%user_id, "failed to find/create DM room");
                    }
                }
            }
            Command::SendRoomMessage { room_id, body, .. } => {
                info!(service=%self.id, room_id=%room_id, body=%body, "sending room message");

                // Parse the room ID
                let room_id = match RoomId::parse(&room_id) {
                    Ok(rid) => rid,
                    Err(e) => {
                        error!(room_id=%room_id, error=%e, "invalid room ID");
                        return Ok(());
                    }
                };

                // Get the room and send message
                if let Some(room) = self.client.get_room(&room_id) {
                    let content = RoomMessageEventContent::text_plain(&body);
                    if let Err(e) = room.send(content).await {
                        error!(error=%e, "failed to send room message");
                    } else {
                        debug!("room message sent successfully");
                    }
                } else {
                    warn!(room_id=%room_id, "room not found or not joined");
                }
            }
        }
        Ok(())
    }
}
