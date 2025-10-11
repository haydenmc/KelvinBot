use std::{fmt, path::PathBuf};

use anyhow::{Result, bail};
use matrix_sdk::{
    Client, Room, RoomMemberships, RoomState,
    config::SyncSettings,
    encryption::{self, EncryptionSettings},
    ruma::{
        RoomId, UserId,
        events::room::{
            member::{MembershipState, StrippedRoomMemberEvent, SyncRoomMemberEvent},
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
    verification_device_id: Option<String>,
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
        verification_device_id: Option<String>,
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
                auto_enable_cross_signing: true, // Auto-load existing keys from DB
                auto_enable_backups: false,      // Manual backup control
            })
            .build()
            .await?;

        Ok(Self { id, user_id, password, device_id, verification_device_id, evt_tx, client })
    }

    async fn setup_encryption(&self) -> Result<()> {
        let encryption = self.client.encryption();

        encryption.wait_for_e2ee_initialization_tasks().await;

        // Check cross-signing status
        if let Some(status) = encryption.cross_signing_status().await {
            info!(
                has_master=%status.has_master,
                has_self_signing=%status.has_self_signing,
                has_user_signing=%status.has_user_signing,
                "cross-signing status"
            );
        }

        // Check if we already have valid keys locally
        let device = encryption.get_own_device().await?;
        let already_setup = if let Some(ref dev) = device {
            let is_cross_signed = dev.is_cross_signed_by_owner();
            let is_verified = dev.is_verified();
            info!(
                device_id=%dev.device_id(),
                is_verified=%is_verified,
                is_cross_signed=%is_cross_signed,
                "own device status"
            );

            // Device is fully set up if it's cross-signed
            is_cross_signed
        } else {
            false
        };

        if already_setup {
            info!("device already cross-signed - setup complete");
            return Ok(());
        }

        // Device needs verification - perform interactive verification
        info!("device needs verification");

        if let Some(ref target_device_id) = self.verification_device_id {
            info!(target_device_id=%target_device_id, "requesting interactive verification");

            use matrix_sdk::ruma::OwnedDeviceId;
            let device_id: OwnedDeviceId = target_device_id.as_str().into();

            // Get the target device
            let target_device =
                encryption.get_device(self.client.user_id().unwrap(), &device_id).await?;

            if let Some(device) = target_device {
                info!("found target device, requesting verification");

                // Request verification with emoji SAS method
                use matrix_sdk::ruma::events::key::verification::VerificationMethod;
                let methods = vec![VerificationMethod::SasV1];
                let verification_request =
                    device.request_verification_with_methods(methods).await?;
                info!("verification request sent, waiting for acceptance...");

                // Wait for the request to transition to "ready" state
                use tokio::time::{Duration, timeout};

                let result = timeout(Duration::from_secs(120), async {
                    loop {
                        if verification_request.is_ready() {
                            break;
                        }
                        if verification_request.is_cancelled() || verification_request.is_done() {
                            bail!("verification request was cancelled or failed");
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }

                    info!("verification request ready, waiting for other device to start SAS...");

                    // Wait for the other side to start SAS verification
                    let sas = loop {
                        // Check if there's an active SAS verification
                        if let Some(verification) = verification_request.start_sas().await? {
                            info!("SAS verification started by other device");
                            break verification;
                        }

                        if verification_request.is_cancelled() {
                            bail!("verification was cancelled");
                        }

                        if verification_request.is_done() {
                            bail!("verification completed without SAS");
                        }

                        tokio::time::sleep(Duration::from_millis(500)).await;
                    };

                    // Accept the SAS verification
                    sas.accept().await?;
                    info!("SAS verification accepted, waiting for key agreement...");

                    // Wait for emoji/decimal to be ready
                    loop {
                        if let Some(emoji) = sas.emoji() {
                            info!("Emoji verification codes: {:?}", emoji);
                            info!("Please confirm these emojis match on the other device and accept there");
                            break;
                        }
                        if sas.is_cancelled() || sas.is_done() {
                            bail!("SAS verification was cancelled");
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }

                    // Auto-confirm on this side
                    sas.confirm().await?;
                    info!("confirmed on bot side, waiting for other device to confirm...");

                    // Wait for verification to complete
                    loop {
                        if sas.is_done() {
                            info!("verification complete!");
                            break;
                        }
                        if sas.is_cancelled() {
                            bail!("verification was cancelled");
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }

                    Ok::<(), anyhow::Error>(())
                }).await;

                match result {
                    Ok(Ok(())) => {
                        info!("interactive verification completed successfully");
                    }
                    Ok(Err(e)) => {
                        bail!("verification failed: {}", e);
                    }
                    Err(_) => {
                        bail!("verification timed out after 120 seconds");
                    }
                }
            } else {
                bail!("target device {} not found", target_device_id);
            }
        } else {
            bail!("device needs verification but no verification_device_id configured");
        }

        Ok(())
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

    async fn check_and_leave_empty_room(&self, room: &Room) {
        // Only check joined rooms
        if room.state() != RoomState::Joined {
            return;
        }

        // Get active members in the room
        match room.members(RoomMemberships::ACTIVE).await {
            Ok(members) => {
                // If the bot is the only member, leave the room
                if members.len() == 1
                    && let Some(bot_user_id) = self.client.user_id()
                    && members.iter().any(|m| m.user_id() == bot_user_id)
                {
                    info!(room_id=%room.room_id(), "leaving room with only bot as member");
                    if let Err(e) = room.leave().await {
                        error!(room_id=%room.room_id(), error=%e, "failed to leave empty room");
                    } else {
                        debug!(room_id=%room.room_id(), "successfully left empty room");
                    }
                }
            }
            Err(e) => {
                warn!(room_id=%room.room_id(), error=%e, "failed to get room members");
            }
        }
    }

    async fn cleanup_empty_rooms(&self) {
        info!("checking for empty rooms to leave");
        let rooms = self.client.rooms();
        for room in rooms {
            self.check_and_leave_empty_room(&room).await;
        }
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
        // Handle room membership changes to detect when bot becomes the only member
        let bot_user_id = self.client.user_id().expect("client should have user_id after login").to_owned();
        self.client.add_event_handler(|_event: SyncRoomMemberEvent, room: Room| async move {
            // Check if the bot is now the only member in the room
            if room.state() == RoomState::Joined
                && let Ok(members) = room.members(RoomMemberships::ACTIVE).await
                && members.len() == 1
                && members.iter().any(|m| m.user_id() == bot_user_id)
            {
                info!(room_id=%room.room_id(), "detected bot as only member, leaving room");
                if let Err(e) = room.leave().await {
                    error!(room_id=%room.room_id(), error=%e, "failed to leave empty room");
                } else {
                    debug!(room_id=%room.room_id(), "successfully left empty room");
                }
            }
        });
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
        // This also fetches cross-signing keys from the server.
        self.client.sync_once(SyncSettings::default()).await?;

        // Set up event handlers before encryption setup so verification events are processed
        self.setup_event_handlers().await?;

        // Spawn sync task in background so verification events can be processed
        let client_for_sync = self.client.clone();
        let cancel_for_sync = cancel.child_token();
        let sync_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_for_sync.cancelled() => {
                        info!("background sync shutting down");
                        break;
                    }
                    result = client_for_sync.sync(SyncSettings::default()) => {
                        if let Err(e) = result {
                            error!(error=%e, "background sync error");
                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            }
        });

        // Set up encryption with verification (sync running in background)
        if let Err(e) = self.setup_encryption().await {
            error!(error=%e, "failed to set up encryption");
            cancel.cancel(); // Stop background sync
            bail!("failed to set up encryption")
        }

        info!("encryption setup complete, service ready");

        // Clean up any rooms where the bot is the only member
        self.cleanup_empty_rooms().await;

        // Wait for shutdown or sync task to exit
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(service=%self.id, "shutdown requested");
            }
            result = sync_handle => {
                match result {
                    Ok(_) => info!("sync task completed"),
                    Err(e) => error!(error=%e, "sync task panicked"),
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
