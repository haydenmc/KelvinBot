use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures::{SinkExt, StreamExt};
use mumble_protocol_2x::control::msgs::{
    Authenticate, ChannelState, Ping, ServerSync, TextMessage, UserRemove, UserState, Version,
};
use mumble_protocol_2x::control::{ClientControlCodec, ControlPacket};
use mumble_protocol_2x::{Clientbound, Serverbound};
use secrecy::{ExposeSecret, SecretString};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{self, Sender};
use tokio::time::{Duration, interval};
use tokio_native_tls::TlsStream;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::core::bus::Command;
use crate::core::event::{Event, EventKind, User};
use crate::core::service::{Service, ServiceId};

const VERSION_MAJOR: u16 = 1;
const VERSION_MINOR: u8 = 5;
const VERSION_PATCH: u8 = 0;

struct MumbleState {
    user_sessions: HashMap<String, u32>,
    session_users: HashMap<u32, String>,
    channel_ids: HashMap<String, u32>,
    id_channels: HashMap<u32, String>,
    own_session_id: Option<u32>,
    initial_sync_complete: bool,
}

impl MumbleState {
    fn new() -> Self {
        Self {
            user_sessions: HashMap::new(),
            session_users: HashMap::new(),
            channel_ids: HashMap::new(),
            id_channels: HashMap::new(),
            own_session_id: None,
            initial_sync_complete: false,
        }
    }
}

pub struct MumbleService {
    id: ServiceId,
    hostname: String,
    port: u16,
    username: String,
    password: SecretString,
    accept_invalid_certs: bool,
    evt_tx: Sender<Event>,
    msg_tx: Arc<Mutex<Option<Sender<ControlPacket<Serverbound>>>>>,
    state: Arc<Mutex<MumbleState>>,
}

impl MumbleService {
    pub async fn create(
        id: ServiceId,
        hostname: String,
        port: u16,
        username: String,
        password: SecretString,
        accept_invalid_certs: bool,
        evt_tx: Sender<Event>,
    ) -> Result<Self> {
        if hostname.is_empty() {
            return Err(anyhow!("hostname cannot be empty"));
        }

        if username.is_empty() {
            return Err(anyhow!("username cannot be empty"));
        }

        Ok(Self {
            id,
            hostname,
            port,
            username,
            password,
            accept_invalid_certs,
            evt_tx,
            msg_tx: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(MumbleState::new())),
        })
    }

    async fn connect(&self) -> Result<Framed<TlsStream<TcpStream>, ClientControlCodec>> {
        info!(hostname=%self.hostname, port=%self.port, "connecting to mumble server");

        let tcp_stream = TcpStream::connect(format!("{}:{}", self.hostname, self.port)).await?;

        let mut tls_connector_builder = native_tls::TlsConnector::builder();
        if self.accept_invalid_certs {
            warn!("accepting invalid TLS certificates");
            tls_connector_builder.danger_accept_invalid_certs(true);
        }
        let tls_connector = tls_connector_builder.build()?;
        let tls_connector = tokio_native_tls::TlsConnector::from(tls_connector);

        let tls_stream = tls_connector.connect(&self.hostname, tcp_stream).await?;

        let framed = Framed::new(tls_stream, ClientControlCodec::new());

        info!("TLS connection established");
        Ok(framed)
    }

    async fn authenticate(
        &self,
        stream: &mut Framed<TlsStream<TcpStream>, ClientControlCodec>,
    ) -> Result<()> {
        info!(username=%self.username, "authenticating");

        let mut version = Version::default();
        version.set_version_v1(VERSION_MAJOR as u32);
        version.set_version_v2(VERSION_MINOR as u64);
        version.set_release(format!("{}.{}.{}", VERSION_MAJOR, VERSION_MINOR, VERSION_PATCH));
        version.set_os("KelvinBot".to_string());
        version.set_os_version(env!("CARGO_PKG_VERSION").to_string());

        stream.send(ControlPacket::Version(Box::new(version))).await?;

        let mut auth = Authenticate::default();
        auth.set_username(self.username.clone());
        if !self.password.expose_secret().is_empty() {
            auth.set_password(self.password.expose_secret().to_string());
        }
        auth.set_opus(true);

        stream.send(ControlPacket::Authenticate(Box::new(auth))).await?;

        Ok(())
    }

    async fn handle_server_sync(&self, msg: ServerSync, state: &mut MumbleState) -> Result<()> {
        info!(session=%msg.session(), "server sync received");
        state.own_session_id = Some(msg.session());
        state.initial_sync_complete = true;

        // Emit initial user list
        self.emit_user_list_update(state).await?;
        Ok(())
    }

    async fn handle_user_state(&self, msg: UserState, state: &mut MumbleState) -> Result<()> {
        let session = msg.session();
        let was_new_user = !state.session_users.contains_key(&session);

        if let Some(name) = msg.name.as_ref() {
            debug!(session=%session, username=%name, "user state update");
            state.user_sessions.insert(name.clone(), session);
            state.session_users.insert(session, name.clone());

            // Emit user list update if initial sync is complete and this is a new user
            if state.initial_sync_complete && was_new_user {
                self.emit_user_list_update(state).await?;
            }
        }

        Ok(())
    }

    fn handle_channel_state(&self, msg: ChannelState, state: &mut MumbleState) {
        let channel_id = msg.channel_id();

        if let Some(name) = msg.name.as_ref() {
            debug!(channel_id=%channel_id, channel_name=%name, "channel state update");
            state.channel_ids.insert(name.clone(), channel_id);
            state.id_channels.insert(channel_id, name.clone());
        }
    }

    async fn handle_user_remove(&self, msg: UserRemove, state: &mut MumbleState) -> Result<()> {
        let session = msg.session();
        debug!(session=%session, "user removed");

        // Remove user from tracking
        if let Some(username) = state.session_users.remove(&session) {
            state.user_sessions.remove(&username);

            // Emit user list update if initial sync is complete
            if state.initial_sync_complete {
                self.emit_user_list_update(state).await?;
            }
        }

        Ok(())
    }

    async fn emit_user_list_update(&self, state: &MumbleState) -> Result<()> {
        let own_session = state.own_session_id;
        let users: Vec<User> = state
            .session_users
            .iter()
            .map(|(session_id, username)| User {
                id: session_id.to_string(),
                username: username.clone(),
                display_name: username.clone(),
                is_active: true,
                is_self: own_session == Some(*session_id),
            })
            .collect();

        let event =
            Event { service_id: self.id.clone(), kind: EventKind::UserListUpdate { users } };

        self.evt_tx.send(event).await?;
        Ok(())
    }

    async fn emit_text_message_event(
        evt_tx: Sender<Event>,
        service_id: ServiceId,
        msg: TextMessage,
        sender_name: String,
        is_local_user: bool,
        channel_ids: HashMap<u32, String>,
    ) -> Result<()> {
        let message_text = msg.message();

        if message_text.is_empty() {
            return Ok(());
        }

        if !msg.session.is_empty() {
            let event = Event {
                service_id,
                kind: EventKind::DirectMessage {
                    user_id: sender_name,
                    body: message_text.to_string(),
                    is_local_user,
                },
            };
            evt_tx.send(event).await?;
        } else if !msg.channel_id.is_empty() {
            let channel_id = msg.channel_id[0];
            let room_id = channel_ids
                .get(&channel_id)
                .cloned()
                .unwrap_or_else(|| format!("channel_{}", channel_id));

            let event = Event {
                service_id,
                kind: EventKind::RoomMessage {
                    room_id,
                    body: message_text.to_string(),
                    is_local_user,
                },
            };
            evt_tx.send(event).await?;
        }

        Ok(())
    }

    async fn handle_control_packet(
        &self,
        packet: ControlPacket<Clientbound>,
        state: &mut MumbleState,
    ) -> Result<Option<ControlPacket<Serverbound>>> {
        match &packet {
            ControlPacket::Ping(_) => {
                debug!("received ping packet");
            }
            ControlPacket::ServerSync(_) => {
                debug!("received server sync");
            }
            ControlPacket::UserState(_) => {
                debug!("received user state");
            }
            ControlPacket::UserRemove(_) => {
                debug!("received user remove");
            }
            ControlPacket::ChannelState(_) => {
                debug!("received channel state");
            }
            ControlPacket::TextMessage(_) => {
                debug!("received text message");
            }
            _ => {
                debug!("received other packet: {:?}", packet);
            }
        }

        match packet {
            ControlPacket::ServerSync(msg) => {
                self.handle_server_sync(*msg, state).await?;
                Ok(None)
            }
            ControlPacket::UserState(msg) => {
                self.handle_user_state(*msg, state).await?;
                Ok(None)
            }
            ControlPacket::UserRemove(msg) => {
                self.handle_user_remove(*msg, state).await?;
                Ok(None)
            }
            ControlPacket::ChannelState(msg) => {
                self.handle_channel_state(*msg, state);
                Ok(None)
            }
            ControlPacket::Ping(msg) => {
                debug!(timestamp=%msg.timestamp(), "received ping (echo from server)");
                // Don't respond - this is likely our own ping being echoed back
                Ok(None)
            }
            ControlPacket::TextMessage(msg) => {
                let evt_tx = self.evt_tx.clone();
                let id = self.id.clone();
                let sender_name = state
                    .session_users
                    .get(&msg.actor())
                    .cloned()
                    .unwrap_or_else(|| format!("user_{}", msg.actor()));
                let is_local_user = state.own_session_id == Some(msg.actor());
                let channel_ids = state.id_channels.clone();

                tokio::spawn(async move {
                    if let Err(e) = Self::emit_text_message_event(
                        evt_tx,
                        id,
                        *msg,
                        sender_name,
                        is_local_user,
                        channel_ids,
                    )
                    .await
                    {
                        error!(error=%e, "failed to handle text message");
                    }
                });
                Ok(None)
            }
            _ => {
                debug!("received unhandled control packet: {:?}", packet);
                Ok(None)
            }
        }
    }
}

#[async_trait::async_trait]
impl Service for MumbleService {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        info!(id=%self.id, "mumble service starting");

        let mut stream = self.connect().await?;
        self.authenticate(&mut stream).await?;

        let (send_tx, mut send_rx) = mpsc::channel::<ControlPacket<Serverbound>>(32);
        *self.msg_tx.lock().await = Some(send_tx);

        // Send periodic pings to keep connection alive (every 15 seconds)
        let mut ping_interval = interval(Duration::from_secs(15));
        ping_interval.tick().await; // First tick completes immediately

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(id=%self.id, "mumble service shutting down");
                    break;
                }
                _ = ping_interval.tick() => {
                    // Send ping to keep connection alive
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;

                    let mut ping = Ping::new();
                    ping.set_timestamp(timestamp);

                    debug!("sending keepalive ping");
                    if let Err(e) = stream.send(ControlPacket::Ping(Box::new(ping))).await {
                        error!(error=%e, "failed to send keepalive ping");
                        return Err(e.into());
                    }
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(packet)) => {
                            let mut state = self.state.lock().await;
                            match self.handle_control_packet(packet, &mut state).await {
                                Ok(Some(response)) => {
                                    // Send response immediately (e.g., ping response)
                                    if let Err(e) = stream.send(response).await {
                                        error!(error=%e, "failed to send response packet");
                                        return Err(e.into());
                                    }
                                }
                                Ok(None) => {
                                    // No response needed
                                }
                                Err(e) => {
                                    error!(error=%e, "failed to handle control packet");
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!(error=%e, "error reading from mumble server");
                            return Err(e.into());
                        }
                        None => {
                            warn!("mumble server connection closed");
                            return Err(anyhow!("connection closed"));
                        }
                    }
                }
                Some(packet) = send_rx.recv() => {
                    if let Err(e) = stream.send(packet).await {
                        error!(error=%e, "failed to send packet to mumble server");
                        return Err(e.into());
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_command(&self, command: Command) -> Result<()> {
        let msg_tx = self.msg_tx.lock().await;
        let Some(tx) = msg_tx.as_ref() else {
            return Err(anyhow!("mumble service not connected"));
        };

        match command {
            Command::SendDirectMessage { user_id, body, .. } => {
                debug!(user_id=%user_id, "sending direct message");

                let state = self.state.lock().await;
                let session_id = state
                    .user_sessions
                    .get(&user_id)
                    .ok_or_else(|| anyhow!("unknown user: {}", user_id))?;

                let mut msg = TextMessage::new();
                msg.set_message(body);
                msg.session = vec![*session_id];

                tx.send(ControlPacket::TextMessage(Box::new(msg))).await?;
            }
            Command::SendRoomMessage { room_id, body, .. } => {
                debug!(room_id=%room_id, "sending room message");

                let state = self.state.lock().await;
                let channel_id = state
                    .channel_ids
                    .get(&room_id)
                    .ok_or_else(|| anyhow!("unknown channel: {}", room_id))?;

                let mut msg = TextMessage::new();
                msg.set_message(body);
                msg.channel_id = vec![*channel_id];

                tx.send(ControlPacket::TextMessage(Box::new(msg))).await?;
            }
            Command::GenerateInviteToken { response_tx, .. } => {
                warn!("mumble does not support invite token generation");
                let _ = response_tx.send(Err(anyhow!("not supported by mumble")));
            }
        }

        Ok(())
    }
}
