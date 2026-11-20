use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use matrix_sdk::{
    Client, Room, RoomMemberships, RoomState,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::{self, EncryptionSettings},
    event_handler::EventHandlerDropGuard,
    ruma::{
        OwnedRoomId, RoomId, UserId,
        events::{
            reaction::OriginalSyncReactionEvent,
            room::{
                member::{MembershipState, StrippedRoomMemberEvent, SyncRoomMemberEvent},
                message::{
                    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
                    TextMessageEventContent,
                },
                redaction::OriginalSyncRoomRedactionEvent,
            },
        },
    },
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
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

/// Whether an image of `size` bytes is over the download cap. An unknown size
/// is let through; the downloaded length is checked afterwards.
fn exceeds_image_cap(size: Option<u64>, max_image_size_bytes: u64) -> bool {
    size.is_some_and(|size| size > max_image_size_bytes)
}

/// How many reactions are remembered for matching against later removals.
const REACTION_REGISTRY_CAPACITY: usize = 10_000;

#[derive(Debug, Clone, PartialEq)]
struct ReactionInfo {
    target_event_id: String,
    key: String,
}

/// Remembers recent reactions by event ID, so that when one is removed (which
/// arrives as a bare redaction) we can say what it was a reaction to.
///
/// Holds at most `capacity` reactions; the oldest are forgotten first. A
/// removal of a forgotten reaction is not reported.
struct ReactionRegistry {
    capacity: usize,
    reactions: HashMap<String, ReactionInfo>,
    /// Event IDs in the order they were added. May include IDs that have
    /// since been removed; those are skipped over when making room.
    order: VecDeque<String>,
}

impl ReactionRegistry {
    fn new(capacity: usize) -> Self {
        Self { capacity, reactions: HashMap::new(), order: VecDeque::new() }
    }

    fn insert(&mut self, reaction_event_id: String, info: ReactionInfo) {
        if self.reactions.insert(reaction_event_id.clone(), info).is_none() {
            self.order.push_back(reaction_event_id);
        }
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.reactions.remove(&oldest);
            }
        }
    }

    fn remove(&mut self, reaction_event_id: &str) -> Option<ReactionInfo> {
        self.reactions.remove(reaction_event_id)
    }
}

pub struct MatrixService {
    id: ServiceId,
    user_id: MatrixUserId,
    password: SecretString,
    device_id: String,
    /// Where the logged-in session is kept between runs of the process.
    session_path: PathBuf,
    verification_device_id: Option<String>,
    max_image_size_bytes: u64,
    evt_tx: tokio::sync::mpsc::Sender<Event>,
    client: Client,
    reaction_registry: Arc<Mutex<ReactionRegistry>>,
    direct_invites: DirectInvites,
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
        max_image_size_bytes: u64,
    ) -> Result<Self> {
        // Create storage directory
        let mut sqlite_path = data_directory.clone();
        sqlite_path.push("matrix");
        sqlite_path.push(id.to_string());
        std::fs::create_dir_all(&sqlite_path).expect("Failed to create storage directory");
        // Kept beside the crypto store so the two are moved or deleted together.
        let session_path = sqlite_path.join(SESSION_FILE);

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

        let reaction_registry =
            Arc::new(Mutex::new(ReactionRegistry::new(REACTION_REGISTRY_CAPACITY)));

        Ok(Self {
            id,
            user_id,
            password,
            device_id,
            session_path,
            verification_device_id,
            max_image_size_bytes,
            evt_tx,
            client,
            reaction_registry,
            direct_invites: DirectInvites::default(),
        })
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

            let own_user_id = self.client.user_id().unwrap();

            // Ask the homeserver for our account's current devices first. The
            // lookup below only reads the local store, which on a new store
            // may not have been filled in yet.
            encryption
                .request_user_identity(own_user_id)
                .await
                .context("fetching the account's device list")?;

            // Get the target device
            let target_device = encryption.get_device(own_user_id, &device_id).await?;

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
                // Name the sessions that do exist: IDs are case-sensitive and
                // must belong to the bot's own account.
                let known: Vec<String> = encryption
                    .get_user_devices(own_user_id)
                    .await?
                    .devices()
                    .map(|d| d.device_id().to_string())
                    .collect();
                bail!(
                    "target device {} not found among this account's devices: [{}]",
                    target_device_id,
                    known.join(", ")
                );
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
                if let Ok(true) = self.direct_invites.is_direct(&room).await
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

    /// Accepts invites that are still pending from before this run: ones that
    /// arrived while the bot was not running, or whose join failed. The live
    /// invite handler only sees invites that arrive while it is registered.
    async fn accept_pending_invites(&self) {
        for room in self.client.invited_rooms() {
            info!(room_id=%room.room_id(), "found a pending invite");
            let is_direct = room.is_direct().await.unwrap_or(false);
            accept_invite(&room, is_direct, &self.direct_invites).await;
        }
    }

    async fn cleanup_empty_rooms(&self) {
        info!("checking for empty rooms to leave");
        let rooms = self.client.rooms();
        for room in rooms {
            self.check_and_leave_empty_room(&room).await;
        }
    }

    /// Registers the handlers that turn Matrix events into bus events.
    ///
    /// The handlers stay registered only while the returned guards are alive.
    /// `run` holds them for its own duration, so a restarted `run` registers a
    /// fresh set instead of adding a second one that would emit every event
    /// twice.
    fn setup_event_handlers(&self) -> Vec<EventHandlerDropGuard> {
        let mut handles = Vec::new();

        // Handle room invites
        let direct_invites = self.direct_invites.clone();
        handles.push(self.client.add_event_handler(
            move |event: StrippedRoomMemberEvent, room: Room| {
                let direct_invites = direct_invites.clone();
                async move {
                    if event.state_key == room.own_user_id()
                        && event.content.membership == MembershipState::Invite
                    {
                        info!("Received room invite for room: {}", room.room_id());
                        let is_direct = event.content.is_direct == Some(true);
                        accept_invite(&room, is_direct, &direct_invites).await;
                    }
                }
            },
        ));
        // Handle room membership changes to detect when bot becomes the only member
        let bot_user_id =
            self.client.user_id().expect("client should have user_id after login").to_owned();
        handles.push(self.client.add_event_handler(
            |_event: SyncRoomMemberEvent, room: Room| async move {
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
            },
        ));
        // Handle room messages
        let service_id = self.id.clone();
        let evt_tx = self.evt_tx.clone();
        let bot_user_id_for_handler =
            self.client.user_id().expect("client should have user_id after login").to_owned();
        let max_image_size_bytes = self.max_image_size_bytes;
        let direct_invites = self.direct_invites.clone();
        handles.push(self.client.add_event_handler(
            move |event: OriginalSyncRoomMessageEvent, room: Room, _client: Client| {
                let direct_invites = direct_invites.clone();
                let service_id = service_id.clone();
                let evt_tx = evt_tx.clone();
                let bot_user_id_for_handler = bot_user_id_for_handler.clone();
                async move {
                    if room.state() != RoomState::Joined {
                        return;
                    }

                    let Ok(is_direct) = direct_invites.is_direct(&room).await else {
                        warn!("could not determine if message was a direct message");
                        return;
                    };

                    // Check if user is from the same homeserver as the bot
                    let is_local_user =
                        event.sender.server_name() == bot_user_id_for_handler.server_name();

                    // Get sender display name
                    let sender_display_name = room
                        .get_member(&event.sender)
                        .await
                        .ok()
                        .and_then(|m| m)
                        .and_then(|m| m.display_name().map(|s| s.to_string()));

                    let sender_id = event.sender.to_string();
                    let is_self = event.sender == bot_user_id_for_handler;

                    // An edit arrives as a new message event that replaces an
                    // earlier one. It is reported as an edit, never as a new
                    // message, so that commands in it do not run a second time.
                    if is_edit(&event.content) {
                        match edited_text(&event.content) {
                            Some(body) if !is_direct => {
                                let event = Event {
                                    service_id,
                                    kind: EventKind::RoomMessageEdited {
                                        room_id: room.room_id().to_string(),
                                        body,
                                        is_local_user,
                                        sender_id,
                                        sender_display_name,
                                        is_self,
                                    },
                                };
                                let _ = evt_tx.send(event).await;
                            }
                            // Edits in direct chats, and of anything but text.
                            _ => debug!(event_id=%event.event_id, "ignoring message edit"),
                        }
                        return;
                    }

                    match event.content.msgtype {
                        MessageType::Text(text_content) => match is_direct {
                            true => {
                                let event = Event {
                                    service_id,
                                    kind: EventKind::DirectMessage {
                                        user_id: sender_id.clone(),
                                        body: text_content.body,
                                        is_local_user,
                                        sender_id,
                                        sender_display_name: sender_display_name.clone(),
                                        is_self,
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
                                        is_local_user,
                                        sender_id,
                                        sender_display_name,
                                        is_self,
                                    },
                                };
                                let _ = evt_tx.send(event).await;
                            }
                        },
                        MessageType::Image(image_content) => {
                            if is_direct {
                                return; // only relay room images, not DM images
                            }
                            let mimetype =
                                image_content.info.as_ref().and_then(|i| i.mimetype.clone());
                            let declared_size =
                                image_content.info.as_ref().and_then(|i| i.size).map(u64::from);
                            let room_id = room.room_id().to_string();
                            let source_url =
                                format!("https://matrix.to/#/{}/{}", room_id, event.event_id);

                            // Fetch image bytes using the authenticated SDK client.
                            // Spawned so the event handler returns promptly.
                            tokio::spawn(async move {
                                use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
                                let request = MediaRequestParameters {
                                    source: image_content.source.clone(),
                                    format: MediaFormat::File,
                                };
                                // The event is still emitted when the image is too
                                // large or the fetch fails, just without bytes, so
                                // middlewares can report it rather than miss it.
                                let image_data = if exceeds_image_cap(
                                    declared_size,
                                    max_image_size_bytes,
                                ) {
                                    warn!(
                                        room_id=%room_id,
                                        declared_size=?declared_size,
                                        limit=max_image_size_bytes,
                                        "image exceeds max_image_size_bytes; not downloading"
                                    );
                                    None
                                } else {
                                    match _client.media().get_media_content(&request, false).await {
                                        // The declared size is supplied by the sender, so
                                        // the real length is checked as well.
                                        Ok(bytes)
                                            if exceeds_image_cap(
                                                Some(bytes.len() as u64),
                                                max_image_size_bytes,
                                            ) =>
                                        {
                                            warn!(
                                                room_id=%room_id,
                                                size=bytes.len(),
                                                limit=max_image_size_bytes,
                                                "downloaded image exceeds max_image_size_bytes; discarding"
                                            );
                                            None
                                        }
                                        Ok(bytes) => Some(Arc::from(bytes)),
                                        Err(e) => {
                                            warn!(error=%e, "failed to fetch image content for relay");
                                            None
                                        }
                                    }
                                };
                                let event = Event {
                                    service_id,
                                    kind: EventKind::RoomImage {
                                        room_id,
                                        sender_id,
                                        sender_display_name,
                                        is_self,
                                        is_local_user,
                                        body: image_content.body,
                                        source_url,
                                        mimetype,
                                        image_data,
                                    },
                                };
                                let _ = evt_tx.send(event).await;
                            });
                        }
                        _ => {} // ignore other message types
                    }
                }
            },
        ));

        // Handle reactions
        let service_id = self.id.clone();
        let evt_tx = self.evt_tx.clone();
        let reaction_registry = self.reaction_registry.clone();
        let bot_user_id_for_reactions =
            self.client.user_id().expect("client should have user_id after login").to_owned();
        handles.push(self.client.add_event_handler(
            move |event: OriginalSyncReactionEvent, room: Room| {
                let service_id = service_id.clone();
                let evt_tx = evt_tx.clone();
                let reaction_registry = reaction_registry.clone();
                let bot_user_id = bot_user_id_for_reactions.clone();
                async move {
                    if room.state() != RoomState::Joined {
                        return;
                    }

                    let reaction_event_id = event.event_id.to_string();
                    let target_event_id = event.content.relates_to.event_id.to_string();
                    let key = event.content.relates_to.key.clone();
                    let sender_id = event.sender.to_string();
                    let is_self = event.sender == bot_user_id;

                    // Store reaction info for later lookup on redaction
                    {
                        let mut registry = reaction_registry.lock().await;
                        registry.insert(
                            reaction_event_id.clone(),
                            ReactionInfo {
                                target_event_id: target_event_id.clone(),
                                key: key.clone(),
                            },
                        );
                    }

                    // Get sender display name
                    let sender_display_name = room
                        .get_member(&event.sender)
                        .await
                        .ok()
                        .and_then(|m| m)
                        .and_then(|m| m.display_name().map(|s| s.to_string()));

                    let evt = Event {
                        service_id,
                        kind: EventKind::ReactionAdded {
                            room_id: room.room_id().to_string(),
                            event_id: reaction_event_id,
                            target_event_id,
                            key,
                            sender_id,
                            sender_display_name,
                            is_self,
                        },
                    };

                    let _ = evt_tx.send(evt).await;
                }
            },
        ));

        // Handle redactions (for reaction removal)
        let service_id = self.id.clone();
        let evt_tx = self.evt_tx.clone();
        let reaction_registry = self.reaction_registry.clone();
        let bot_user_id_for_redactions =
            self.client.user_id().expect("client should have user_id after login").to_owned();
        handles.push(self.client.add_event_handler(
            move |event: OriginalSyncRoomRedactionEvent, room: Room| {
                let service_id = service_id.clone();
                let evt_tx = evt_tx.clone();
                let reaction_registry = reaction_registry.clone();
                let bot_user_id = bot_user_id_for_redactions.clone();
                async move {
                    if room.state() != RoomState::Joined {
                        return;
                    }

                    // The redacts field is Option in room version 11+
                    let Some(redacted_event_id) = event.redacts else {
                        return;
                    };
                    let redacted_event_id = redacted_event_id.to_string();
                    let sender_id = event.sender.to_string();
                    let is_self = event.sender == bot_user_id;

                    // Check if this redaction is for a reaction we tracked
                    let reaction_info = {
                        let mut registry = reaction_registry.lock().await;
                        registry.remove(&redacted_event_id)
                    };

                    // Only emit ReactionRemoved if we found a tracked reaction
                    if let Some(info) = reaction_info {
                        let evt = Event {
                            service_id,
                            kind: EventKind::ReactionRemoved {
                                room_id: room.room_id().to_string(),
                                event_id: redacted_event_id,
                                target_event_id: Some(info.target_event_id),
                                key: Some(info.key),
                                sender_id,
                                is_self,
                            },
                        };

                        let _ = evt_tx.send(evt).await;
                    }
                }
            },
        ));

        handles.into_iter().map(|handle| self.client.event_handler_drop_guard(handle)).collect()
    }
}
#[async_trait::async_trait]
impl Service for MatrixService {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        info!(service_id=%self.id, homeserver_url=%self.client.homeserver(), user_id=%self.user_id,
            "starting matrix service");

        // Authenticate, unless an earlier `run` on this client already did: the
        // session outlives `run`, and logging in twice on one client panics.
        //
        // A session saved by an earlier process is restored rather than logging
        // in again. Logging in with the password on every start re-registers
        // the device, and a homeserver may then drop the device's encryption
        // keys, after which nobody can share room keys with the bot.
        if self.client.matrix_auth().logged_in() {
            info!("already logged in; reusing the existing session");
        } else if let Some(session) = load_session(&self.session_path, &self.device_id) {
            self.client
                .matrix_auth()
                .restore_session(session, RoomLoadSettings::default())
                .await
                .context("restoring the saved session")?;
            info!("restored the saved session");
        } else {
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
                    if let Some(session) = self.client.matrix_auth().session()
                        && let Err(e) = save_session(&self.session_path, &session)
                    {
                        // Not fatal: the next start logs in again instead.
                        warn!(error=%format!("{e:#}"), "could not save the session");
                    }
                }
                Err(e) => {
                    error!(error=%e, "login error");
                    bail!("login error")
                }
            }
        }

        // An initial sync to set up state and so our bot doesn't respond to old messages.
        // This also fetches cross-signing keys from the server.
        //
        // Keep this BEFORE setup_event_handlers(): nothing delivered by this first sync
        // (the initial timeline on a fresh store, or the backlog that accumulated while
        // the bot was down) reaches any handler, and the background sync below resumes
        // from this response's token. Middlewares such as lychee_upload rely on that to
        // never bulk-upload old photos.
        if let Err(e) = self.client.sync_once(SyncSettings::default()).await {
            if session_is_dead(e.client_api_error_kind()) {
                forget_session(&self.session_path);
                return Err(anyhow::Error::new(e).context(DEAD_SESSION));
            }
            return Err(e.into());
        }

        // Set up event handlers before encryption setup so verification events are processed.
        // They are removed again when `run` returns.
        let _event_handlers = self.setup_event_handlers();

        // Spawn sync task in background so verification events can be processed
        let client_for_sync = self.client.clone();
        let cancel_for_sync = cancel.child_token();
        let session_path = self.session_path.clone();
        // Stops the background sync however `run` returns, including on an error.
        let _stop_sync = cancel_for_sync.clone().drop_guard();
        let sync_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel_for_sync.cancelled() => {
                        info!("background sync shutting down");
                        return Ok(());
                    }
                    result = client_for_sync.sync(SyncSettings::default()) => {
                        let Err(e) = result else { continue };
                        if session_is_dead(e.client_api_error_kind()) {
                            // Retrying cannot help: every request on this
                            // session will be refused the same way.
                            forget_session(&session_path);
                            return Err(anyhow::Error::new(e).context(DEAD_SESSION));
                        }
                        // Anything else (the network, the homeserver being
                        // down) is retried here, resuming from the last sync
                        // token so nothing sent in the meantime is missed.
                        error!(error=%e, "background sync error");
                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    }
                }
            }
        });

        // Set up encryption with verification (sync running in background)
        if let Err(e) = self.setup_encryption().await {
            error!(error=%format!("{e:#}"), "failed to set up encryption");
            bail!("failed to set up encryption")
        }

        info!("encryption setup complete, service ready");

        // Done with the handlers registered and the sync running, so the recent
        // messages of each room joined here are delivered like any others and
        // whatever was asked while the invite sat unanswered gets a reply.
        self.accept_pending_invites().await;

        // Clean up any rooms where the bot is the only member
        self.cleanup_empty_rooms().await;

        // Wait for shutdown or sync task to exit
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(service=%self.id, "shutdown requested");
            }
            result = sync_handle => {
                // Hand a sync that gave up to the supervisor, which reports
                // it and retries with backoff.
                result.context("sync task panicked")??;
                info!("sync task completed");
            }
        }
        Ok(())
    }

    async fn handle_command(&self, command: Command) -> Result<()> {
        match command {
            Command::SendDirectMessage { user_id, body, response_tx, .. } => {
                // The body is never logged: DMs carry secrets such as credential
                // reset links.
                info!(service=%self.id, user_id=%user_id, body_len=body.len(), "sending DM");

                // Parse the user ID
                let user_id = match UserId::parse(&user_id) {
                    Ok(uid) => uid,
                    Err(e) => {
                        error!(user_id=%user_id, error=%e, "invalid user ID");
                        if let Some(tx) = response_tx {
                            let _ = tx.send(Err(anyhow::anyhow!("invalid user ID: {}", e)));
                        }
                        return Ok(());
                    }
                };

                // Find existing or create new DM room
                let result = match self.find_or_create_dm(&user_id).await {
                    Ok(room) => {
                        let content = RoomMessageEventContent::text_plain(&body);
                        match room.send(content).await {
                            Ok(response) => {
                                debug!("DM sent successfully");
                                Ok(response.response.event_id.to_string())
                            }
                            Err(e) => {
                                error!(error=%e, "failed to send DM");
                                Err(anyhow::anyhow!("failed to send DM: {}", e))
                            }
                        }
                    }
                    Err(e) => {
                        error!(error=%e, user_id=%user_id, "failed to find/create DM room");
                        Err(anyhow::anyhow!("failed to find/create DM room: {}", e))
                    }
                };

                if let Some(tx) = response_tx {
                    let _ = tx.send(result);
                }
            }
            Command::SendRoomMessage { room_id, body, markdown_body, response_tx, .. } => {
                info!(service=%self.id, room_id=%room_id, body_len=body.len(), "sending room message");

                // Parse the room ID
                let room_id = match RoomId::parse(&room_id) {
                    Ok(rid) => rid,
                    Err(e) => {
                        error!(room_id=%room_id, error=%e, "invalid room ID");
                        if let Some(tx) = response_tx {
                            let _ = tx.send(Err(anyhow::anyhow!("invalid room ID: {}", e)));
                        }
                        return Ok(());
                    }
                };

                // Get the room and send message
                let result = if let Some(room) = self.client.get_room(&room_id) {
                    let content = if let Some(markdown) = markdown_body {
                        RoomMessageEventContent::new(MessageType::Text(
                            TextMessageEventContent::markdown(markdown),
                        ))
                    } else {
                        RoomMessageEventContent::text_plain(&body)
                    };

                    match room.send(content).await {
                        Ok(response) => {
                            debug!("room message sent successfully");
                            Ok(response.response.event_id.to_string())
                        }
                        Err(e) => {
                            error!(error=%e, "failed to send room message");
                            Err(anyhow::anyhow!("failed to send room message: {}", e))
                        }
                    }
                } else {
                    warn!(room_id=%room_id, "room not found or not joined");
                    Err(anyhow::anyhow!("room not found or not joined"))
                };

                if let Some(tx) = response_tx {
                    let _ = tx.send(result);
                }
            }
            Command::SendThreadReply {
                room_id,
                thread_root_id,
                body,
                markdown_body,
                response_tx,
                ..
            } => {
                info!(service=%self.id, room_id=%room_id, thread_root=%thread_root_id, "sending thread reply");

                // Parse the room ID
                let room_id = match RoomId::parse(&room_id) {
                    Ok(rid) => rid,
                    Err(e) => {
                        error!(room_id=%room_id, error=%e, "invalid room ID");
                        if let Some(tx) = response_tx {
                            let _ = tx.send(Err(anyhow::anyhow!("invalid room ID: {}", e)));
                        }
                        return Ok(());
                    }
                };

                // Parse the thread root event ID
                use matrix_sdk::ruma::EventId;
                let thread_root_event_id = match EventId::parse(&thread_root_id) {
                    Ok(eid) => eid,
                    Err(e) => {
                        error!(thread_root_id=%thread_root_id, error=%e, "invalid thread root event ID");
                        if let Some(tx) = response_tx {
                            let _ = tx
                                .send(Err(anyhow::anyhow!("invalid thread root event ID: {}", e)));
                        }
                        return Ok(());
                    }
                };

                // Get the room
                let result = if let Some(room) = self.client.get_room(&room_id) {
                    // Create the message content
                    let mut content = if let Some(markdown) = markdown_body {
                        RoomMessageEventContent::new(MessageType::Text(
                            TextMessageEventContent::markdown(markdown),
                        ))
                    } else {
                        RoomMessageEventContent::text_plain(&body)
                    };

                    // Manually set the thread relation
                    use matrix_sdk::ruma::events::{relation::Thread, room::message::Relation};
                    content.relates_to =
                        Some(Relation::Thread(Thread::without_fallback(thread_root_event_id)));

                    match room.send(content).await {
                        Ok(response) => {
                            debug!("thread reply sent successfully");
                            Ok(response.response.event_id.to_string())
                        }
                        Err(e) => {
                            error!(error=%e, "failed to send thread reply");
                            Err(anyhow::anyhow!("failed to send thread reply: {}", e))
                        }
                    }
                } else {
                    warn!(room_id=%room_id, "room not found or not joined");
                    Err(anyhow::anyhow!("room not found or not joined"))
                };

                if let Some(tx) = response_tx {
                    let _ = tx.send(result);
                }
            }
            Command::EditMessage { room_id, message_id, new_body, new_markdown_body, .. } => {
                info!(service=%self.id, room_id=%room_id, message_id=%message_id, "editing message");

                // Parse the room ID
                let room_id = match RoomId::parse(&room_id) {
                    Ok(rid) => rid,
                    Err(e) => {
                        error!(room_id=%room_id, error=%e, "invalid room ID");
                        return Ok(());
                    }
                };

                // Parse the event ID
                use matrix_sdk::ruma::EventId;
                let event_id = match EventId::parse(&message_id) {
                    Ok(eid) => eid,
                    Err(e) => {
                        error!(message_id=%message_id, error=%e, "invalid event ID");
                        return Ok(());
                    }
                };

                if let Some(room) = self.client.get_room(&room_id) {
                    // Create the new message content
                    let new_content = if let Some(markdown) = new_markdown_body {
                        RoomMessageEventContent::new(MessageType::Text(
                            TextMessageEventContent::markdown(markdown),
                        ))
                    } else {
                        RoomMessageEventContent::text_plain(&new_body)
                    };

                    // Create edit event using the edit helper
                    use matrix_sdk::ruma::events::AnyMessageLikeEventContent;
                    let edit_event =
                        AnyMessageLikeEventContent::RoomMessage(new_content.make_replacement(
                            matrix_sdk::ruma::events::room::message::ReplacementMetadata::new(
                                event_id.to_owned(),
                                None,
                            ),
                        ));

                    if let Err(e) = room.send(edit_event).await {
                        error!(error=%e, "failed to edit message");
                    } else {
                        debug!("message edited successfully");
                    }
                } else {
                    warn!(room_id=%room_id, message_id=%message_id, "cannot edit message: room not found");
                }
            }
            Command::SendRoomImage { .. } => {
                warn!(service=%self.id, "SendRoomImage not implemented for Matrix service");
            }
            Command::AddReaction { room_id, event_id, key, .. } => {
                info!(service=%self.id, room_id=%room_id, event_id=%event_id, key=%key, "adding reaction");

                // Parse the room ID
                let room_id = match RoomId::parse(&room_id) {
                    Ok(rid) => rid,
                    Err(e) => {
                        error!(room_id=%room_id, error=%e, "invalid room ID");
                        return Ok(());
                    }
                };

                // Parse the event ID
                use matrix_sdk::ruma::EventId;
                let event_id = match EventId::parse(&event_id) {
                    Ok(eid) => eid,
                    Err(e) => {
                        error!(event_id=%event_id, error=%e, "invalid event ID");
                        return Ok(());
                    }
                };

                // Get the room and send reaction
                if let Some(room) = self.client.get_room(&room_id) {
                    use matrix_sdk::ruma::events::reaction::ReactionEventContent;
                    use matrix_sdk::ruma::events::relation::Annotation;

                    let reaction_content =
                        ReactionEventContent::new(Annotation::new(event_id.to_owned(), key));

                    match room.send(reaction_content).await {
                        Ok(_) => {
                            debug!("reaction added successfully");
                        }
                        Err(e) => {
                            error!(error=%e, "failed to add reaction");
                        }
                    }
                } else {
                    warn!(room_id=%room_id, "room not found or not joined");
                }
            }
        }
        Ok(())
    }
}

use matrix_sdk::{ruma::api::error::ErrorKind, store::RoomLoadSettings};

/// Rooms joined from a direct-message invite during this run.
///
/// The SDK only reports a joined room as direct once the bot's `m.direct`
/// account data, updated after joining, has come back through the sync. The
/// room's first messages usually arrive before that. Remembering the invite's
/// flag here lets those be treated as direct messages straight away.
#[derive(Clone, Default)]
struct DirectInvites(Arc<std::sync::Mutex<HashSet<OwnedRoomId>>>);

impl DirectInvites {
    fn insert(&self, room_id: &RoomId) {
        self.0.lock().unwrap().insert(room_id.to_owned());
    }

    fn remove(&self, room_id: &RoomId) {
        self.0.lock().unwrap().remove(room_id);
    }

    fn contains(&self, room_id: &RoomId) -> bool {
        self.0.lock().unwrap().contains(room_id)
    }

    /// Whether `room` is a direct-message room, by this run's invites or
    /// otherwise by the SDK's own record.
    async fn is_direct(&self, room: &Room) -> Result<bool, matrix_sdk::StoreError> {
        if self.contains(room.room_id()) {
            return Ok(true);
        }
        room.is_direct().await
    }
}

/// Joins a room the bot has been invited to, if the room is on the bot's own
/// homeserver. `is_direct` is the invite's direct-message flag.
async fn accept_invite(room: &Room, is_direct: bool, direct_invites: &DirectInvites) {
    let bot_server = room.own_user_id().server_name();
    let Some(room_server) = room.room_id().server_name() else {
        warn!(room_id=%room.room_id(), "Room invite is missing server name");
        return;
    };
    if bot_server != room_server {
        info!(room_server=%room_server, bot_server=%bot_server,
            "Ignoring room invite from different homeserver.");
        return;
    }

    // Recorded before joining: the room's messages can arrive as soon as the
    // join goes through.
    if is_direct {
        direct_invites.insert(room.room_id());
    }
    if let Err(e) = room.join().await {
        error!(room_id=%room.room_id(), "Failed to accept invite: {}", e);
        direct_invites.remove(room.room_id());
        return;
    }
    info!(room_id=%room.room_id(), "Successfully joined room");

    // The SDK only reports a joined room as direct if it appears in the bot's
    // own m.direct account data, which joining alone never updates. Honor the
    // invite's flag so DMs and group DMs are classified correctly afterwards.
    if is_direct {
        match room.set_is_direct(true).await {
            Ok(()) => info!(room_id=%room.room_id(), "marked invited room as direct"),
            Err(e) => {
                warn!(room_id=%room.room_id(), error=%e, "failed to mark invited room as direct")
            }
        }
    }
}

/// File, inside the service's storage directory, holding the saved session.
const SESSION_FILE: &str = "session.json";

const DEAD_SESSION: &str =
    "the homeserver no longer accepts this session; restart the bot to log in again";

/// Reads the session saved by an earlier run, if there is a usable one for
/// `device_id`. A session for another device is left alone and ignored: the
/// configured device has changed, so a fresh login is wanted.
fn load_session(path: &Path, device_id: &str) -> Option<MatrixSession> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path=%path.display(), error=%e, "could not read the saved session");
            return None;
        }
    };
    let session: MatrixSession = match serde_json::from_str(&content) {
        Ok(session) => session,
        Err(e) => {
            warn!(path=%path.display(), error=%e, "saved session is unreadable; ignoring it");
            return None;
        }
    };
    if session.meta.device_id != device_id {
        warn!(
            saved=%session.meta.device_id,
            configured=%device_id,
            "saved session is for a different device; ignoring it"
        );
        return None;
    }
    Some(session)
}

/// Saves the session so the next run can restore it. The file holds the
/// access token, so it is readable by the owner only.
fn save_session(path: &Path, session: &MatrixSession) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};

    let content = serde_json::to_vec(session)?;
    // Removed first so the permissions apply even if an older file exists.
    let _ = std::fs::remove_file(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(&content)?;
    file.sync_all()?;
    Ok(())
}

/// Discards the saved session once the homeserver has rejected it, so the
/// next start of the process logs in with the password again.
fn forget_session(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => warn!("discarded the saved session; the next start will log in again"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(path=%path.display(), error=%e, "could not discard the saved session"),
    }
}

/// Whether a failed request means the session itself is gone (logged out,
/// token revoked, account disabled), as opposed to a failure that may pass.
fn session_is_dead(kind: Option<&ErrorKind>) -> bool {
    matches!(
        kind,
        Some(
            ErrorKind::UnknownToken(_)
                | ErrorKind::MissingToken
                | ErrorKind::UserDeactivated
                | ErrorKind::UserLocked
        )
    )
}

/// Whether a message event is an edit of an earlier message (`m.replace`),
/// as opposed to a new message.
fn is_edit(content: &RoomMessageEventContent) -> bool {
    use matrix_sdk::ruma::events::room::message::Relation;
    matches!(content.relates_to, Some(Relation::Replacement(_)))
}

/// The new text of an edited text message; `None` if this is not an edit, or
/// the edited message is not text.
fn edited_text(content: &RoomMessageEventContent) -> Option<String> {
    use matrix_sdk::ruma::events::room::message::Relation;
    match &content.relates_to {
        Some(Relation::Replacement(replacement)) => match &replacement.new_content.msgtype {
            MessageType::Text(text) => Some(text.body.clone()),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_invites_remember_and_forget_rooms() {
        use matrix_sdk::ruma::room_id;
        let invites = DirectInvites::default();
        let room = room_id!("!dm:example.org");
        assert!(!invites.contains(room));

        invites.insert(room);
        assert!(invites.contains(room));
        assert!(invites.clone().contains(room), "clones share the same set");
        assert!(!invites.contains(room_id!("!other:example.org")));

        invites.remove(room);
        assert!(!invites.contains(room));
    }

    fn session(device_id: &str) -> MatrixSession {
        MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: matrix_sdk::ruma::user_id!("@bot:example.org").to_owned(),
                device_id: device_id.into(),
            },
            tokens: matrix_sdk::SessionTokens {
                access_token: "secret-token".to_string(),
                refresh_token: None,
            },
        }
    }

    #[test]
    fn saved_session_is_loaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SESSION_FILE);

        assert_eq!(load_session(&path, "DEV"), None, "nothing saved yet");
        save_session(&path, &session("DEV")).unwrap();

        assert_eq!(load_session(&path, "DEV"), Some(session("DEV")));
    }

    #[test]
    fn saved_session_is_readable_by_the_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SESSION_FILE);
        // An older, more widely readable file must not keep its permissions.
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        save_session(&path, &session("DEV")).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn session_for_another_device_is_ignored_but_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SESSION_FILE);
        save_session(&path, &session("OLD")).unwrap();

        assert_eq!(load_session(&path, "NEW"), None);
        assert!(path.exists());
    }

    #[test]
    fn unreadable_session_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SESSION_FILE);
        std::fs::write(&path, "{ not json").unwrap();

        assert_eq!(load_session(&path, "DEV"), None);
    }

    #[test]
    fn forgotten_session_is_not_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SESSION_FILE);
        save_session(&path, &session("DEV")).unwrap();

        forget_session(&path);
        forget_session(&path); // already gone: no error

        assert_eq!(load_session(&path, "DEV"), None);
    }

    fn reaction(target: &str) -> ReactionInfo {
        ReactionInfo { target_event_id: target.to_string(), key: "+1".to_string() }
    }

    #[test]
    fn registry_returns_a_reaction_once() {
        let mut registry = ReactionRegistry::new(10);
        registry.insert("r1".to_string(), reaction("m1"));

        assert_eq!(registry.remove("r1"), Some(reaction("m1")));
        assert_eq!(registry.remove("r1"), None);
        assert_eq!(registry.remove("never-seen"), None);
    }

    #[test]
    fn registry_forgets_the_oldest_reactions_when_full() {
        let mut registry = ReactionRegistry::new(3);
        for i in 1..=5 {
            registry.insert(format!("r{i}"), reaction(&format!("m{i}")));
        }

        assert_eq!(registry.remove("r1"), None);
        assert_eq!(registry.remove("r2"), None);
        for i in 3..=5 {
            assert_eq!(registry.remove(&format!("r{i}")), Some(reaction(&format!("m{i}"))));
        }
    }

    #[test]
    fn registry_stays_bounded_when_reactions_come_and_go() {
        let mut registry = ReactionRegistry::new(3);
        for i in 0..1000 {
            let id = format!("r{i}");
            registry.insert(id.clone(), reaction("m"));
            registry.remove(&id);
        }

        assert!(registry.order.len() <= 3);
        assert!(registry.reactions.is_empty());
    }

    #[test]
    fn only_errors_that_end_the_session_stop_the_sync() {
        assert!(session_is_dead(Some(&ErrorKind::UnknownToken(Default::default()))));
        assert!(session_is_dead(Some(&ErrorKind::MissingToken)));
        assert!(session_is_dead(Some(&ErrorKind::UserDeactivated)));
        assert!(session_is_dead(Some(&ErrorKind::UserLocked)));

        // Not a homeserver error at all (the network, say), or one that may pass.
        assert!(!session_is_dead(None));
        assert!(!session_is_dead(Some(&ErrorKind::Unknown)));
        assert!(!session_is_dead(Some(&ErrorKind::Forbidden)));
    }

    fn message_content(json: serde_json::Value) -> RoomMessageEventContent {
        serde_json::from_value(json).expect("valid m.room.message content")
    }

    #[test]
    fn edits_are_recognised() {
        let edit = message_content(serde_json::json!({
            "msgtype": "m.text",
            "body": "* !echo fixed typo",
            "m.new_content": { "msgtype": "m.text", "body": "!echo fixed typo" },
            "m.relates_to": { "rel_type": "m.replace", "event_id": "$original:example.com" },
        }));
        assert!(is_edit(&edit));
        // The new text, not the "* ..." fallback that older clients display.
        assert_eq!(edited_text(&edit).as_deref(), Some("!echo fixed typo"));
    }

    #[test]
    fn new_messages_replies_and_thread_messages_are_not_edits() {
        let plain = message_content(serde_json::json!({ "msgtype": "m.text", "body": "hello" }));
        assert!(!is_edit(&plain));
        assert_eq!(edited_text(&plain), None);

        let reply = message_content(serde_json::json!({
            "msgtype": "m.text",
            "body": "hello back",
            "m.relates_to": { "m.in_reply_to": { "event_id": "$original:example.com" } },
        }));
        assert!(!is_edit(&reply));

        let threaded = message_content(serde_json::json!({
            "msgtype": "m.text",
            "body": "in a thread",
            "m.relates_to": { "rel_type": "m.thread", "event_id": "$root:example.com" },
        }));
        assert!(!is_edit(&threaded));
    }

    #[test]
    fn image_cap_applies_to_known_sizes() {
        assert!(!exceeds_image_cap(Some(0), 100));
        assert!(!exceeds_image_cap(Some(100), 100));
        assert!(exceeds_image_cap(Some(101), 100));
    }

    #[test]
    fn image_cap_lets_unknown_sizes_through() {
        assert!(!exceeds_image_cap(None, 100));
    }
}
