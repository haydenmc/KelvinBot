use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use futures::{SinkExt, StreamExt};
use linkify::{LinkFinder, LinkKind};
use mumble_protocol_2x::control::msgs::{
    Authenticate, ChannelState, Ping, ServerSync, TextMessage, UserRemove, UserState, Version,
};
use mumble_protocol_2x::control::{ClientControlCodec, ControlPacket};
use mumble_protocol_2x::{Clientbound, Serverbound};
use secrecy::{ExposeSecret, SecretString};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{self, Sender};
use tokio::time::{Duration, Instant, interval};
use tokio_native_tls::TlsStream;
use tokio_util::codec::Framed;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::core::bus::Command;
use crate::core::event::{Event, EventKind, User};
use crate::core::service::{Service, ServiceId};

// The version the bot announces. Kept below 1.5.0 on purpose: a server sends
// voice to 1.5+ clients in a newer packet format that the protocol crate
// cannot parse, and a single such packet (anyone speaking) fails the read
// and drops the connection. Older clients are sent the format it does parse.
const VERSION_MAJOR: u16 = 1;
const VERSION_MINOR: u8 = 4;
const VERSION_PATCH: u8 = 0;

/// How often a ping is sent to the server, which answers each one.
const PING_INTERVAL: Duration = Duration::from_secs(15);

/// Maximum time without receiving anything (ping replies included) before the
/// connection is considered dead. Three missed ping replies.
const READ_TIMEOUT: Duration = Duration::from_secs(45);

/// Maximum time to wait for the TCP and TLS handshakes and for the opening
/// messages to be sent.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The client version in the legacy format: one byte each for minor and patch
/// below a 16-bit major.
fn version_v1(major: u16, minor: u8, patch: u8) -> u32 {
    (u32::from(major) << 16) | (u32::from(minor) << 8) | u32::from(patch)
}

/// The client version in the current format: 16 bits each for major, minor
/// and patch, above 16 unused bits.
fn version_v2(major: u16, minor: u8, patch: u8) -> u64 {
    (u64::from(major) << 48) | (u64::from(minor) << 32) | (u64::from(patch) << 16)
}

/// Escape the characters that are significant in HTML text and attribute
/// values.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render a plain-text command body as the HTML a Mumble text message
/// carries.
///
/// Mumble clients render message text as HTML, while `Command` bodies are
/// plain text that often originates from users on other services (relayed
/// chat, display names). Escaping here keeps that text from being interpreted
/// as markup; line breaks become `<br/>` so multi-line messages keep their
/// shape, and `http(s)` URLs become links, since Mumble clients do not
/// linkify received text themselves.
fn plain_text_to_html(text: &str) -> String {
    let mut finder = LinkFinder::new();
    finder.kinds(&[LinkKind::Url]);

    let mut out = String::with_capacity(text.len());
    for span in finder.spans(text) {
        let piece = span.as_str();
        // linkify accepts any `scheme://`; only web links are made clickable.
        let is_web_link = span.kind().is_some()
            && (piece.starts_with("https://") || piece.starts_with("http://"));

        if is_web_link {
            let url = escape_html(piece);
            out.push_str(&format!("<a href=\"{url}\">{url}</a>"));
        } else {
            out.push_str(&escape_html(piece).replace("\r\n", "<br/>").replace('\n', "<br/>"));
        }
    }

    out
}

/// Build the HTML for a relayed image: caption, inline thumbnail and a link
/// to the original. Every interpolated value is escaped, and the link is only
/// emitted for `http(s)` URLs; anything else is shown as text.
fn image_message_html(
    caption: &str,
    thumbnail_mimetype: &str,
    thumbnail_base64: &str,
    source_url: &str,
) -> String {
    let caption = plain_text_to_html(caption);
    let mimetype = escape_html(thumbnail_mimetype);
    let thumbnail = escape_html(thumbnail_base64);
    let url = escape_html(source_url);

    let link = if source_url.starts_with("https://") || source_url.starts_with("http://") {
        format!("<a href=\"{url}\">View full image</a>")
    } else {
        url
    };

    format!("{caption}<br/><img src=\"data:{mimetype};base64,{thumbnail}\"/><br/>{link}")
}

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

    async fn authenticate<T>(&self, stream: &mut Framed<T, ClientControlCodec>) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        info!(username=%self.username, "authenticating");

        let mut version = Version::default();
        version.set_version_v1(version_v1(VERSION_MAJOR, VERSION_MINOR, VERSION_PATCH));
        version.set_version_v2(version_v2(VERSION_MAJOR, VERSION_MINOR, VERSION_PATCH));
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
                    user_id: sender_name.clone(),
                    body: message_text.to_string(),
                    is_local_user,
                    sender_id: sender_name.clone(),
                    sender_display_name: Some(sender_name),
                    is_self: is_local_user,
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
                    sender_id: sender_name.clone(),
                    sender_display_name: Some(sender_name),
                    is_self: is_local_user,
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

impl MumbleService {
    /// Runs an established, authenticated connection until it fails, goes
    /// silent for `READ_TIMEOUT`, or `cancel` fires.
    async fn process<T>(
        &self,
        mut stream: Framed<T, ClientControlCodec>,
        cancel: CancellationToken,
    ) -> Result<()>
    where
        T: AsyncRead + AsyncWrite + Unpin,
    {
        let (send_tx, mut send_rx) = mpsc::channel::<ControlPacket<Serverbound>>(32);
        *self.msg_tx.lock().await = Some(send_tx);

        // Send periodic pings to keep connection alive
        let mut ping_interval = interval(PING_INTERVAL);
        ping_interval.tick().await; // First tick completes immediately

        // When the connection is given up on. Pushed back by every packet
        // received; the server answers our pings, so a healthy connection is
        // never silent for long.
        let mut read_deadline = Instant::now() + READ_TIMEOUT;
        let silent = || {
            anyhow!(
                "nothing received from mumble server in {}s; assuming connection is dead",
                READ_TIMEOUT.as_secs()
            )
        };

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(id=%self.id, "mumble service shutting down");
                    break;
                }
                _ = tokio::time::sleep_until(read_deadline) => return Err(silent()),
                _ = ping_interval.tick() => {
                    // Send ping to keep connection alive
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;

                    let mut ping = Ping::new();
                    ping.set_timestamp(timestamp);

                    debug!("sending keepalive ping");
                    // A dead peer can also stop accepting writes, so the send
                    // is held to the same deadline.
                    let sent = tokio::time::timeout_at(
                        read_deadline,
                        stream.send(ControlPacket::Ping(Box::new(ping))),
                    );
                    if let Err(e) = sent.await.map_err(|_| silent())? {
                        error!(error=%e, "failed to send keepalive ping");
                        return Err(e.into());
                    }
                }
                msg = stream.next() => {
                    match msg {
                        Some(Ok(packet)) => {
                            read_deadline = Instant::now() + READ_TIMEOUT;
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
}

#[async_trait::async_trait]
impl Service for MumbleService {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        info!(id=%self.id, "mumble service starting");

        // Reset state on each run (important for reconnections after disconnect)
        *self.state.lock().await = MumbleState::new();

        // Bounded, because a server that accepts the connection and then
        // stalls would otherwise hold us here forever.
        let connecting = tokio::time::timeout(CONNECT_TIMEOUT, async {
            let mut stream = self.connect().await?;
            self.authenticate(&mut stream).await?;
            anyhow::Ok(stream)
        });
        let stream = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            connected = connecting => connected.map_err(|_| {
                anyhow!("mumble connection not established within {}s", CONNECT_TIMEOUT.as_secs())
            })??,
        };

        self.process(stream, cancel).await
    }

    async fn handle_command(&self, command: Command) -> Result<()> {
        let msg_tx = self.msg_tx.lock().await;
        let Some(tx) = msg_tx.as_ref() else {
            return Err(anyhow!("mumble service not connected"));
        };

        match command {
            Command::SendDirectMessage { user_id, body, response_tx, .. } => {
                debug!(user_id=%user_id, "sending direct message");

                let state = self.state.lock().await;
                let result = match state.user_sessions.get(&user_id) {
                    Some(session_id) => {
                        let mut msg = TextMessage::new();
                        msg.set_message(plain_text_to_html(&body));
                        msg.session = vec![*session_id];

                        match tx.send(ControlPacket::TextMessage(Box::new(msg))).await {
                            Ok(_) => {
                                // Mumble doesn't provide message IDs, so we return an empty string
                                Ok(String::new())
                            }
                            Err(e) => Err(anyhow!("failed to send message: {}", e)),
                        }
                    }
                    None => Err(anyhow!("unknown user: {}", user_id)),
                };

                if let Some(tx) = response_tx {
                    let _ = tx.send(result);
                } else {
                    result?;
                }
            }
            Command::SendRoomMessage { room_id, body, response_tx, .. } => {
                debug!(room_id=%room_id, "sending room message");

                let state = self.state.lock().await;
                let result = match state.channel_ids.get(&room_id) {
                    Some(channel_id) => {
                        let mut msg = TextMessage::new();
                        msg.set_message(plain_text_to_html(&body));
                        msg.channel_id = vec![*channel_id];

                        match tx.send(ControlPacket::TextMessage(Box::new(msg))).await {
                            Ok(_) => {
                                // Mumble doesn't provide message IDs, so we return an empty string
                                Ok(String::new())
                            }
                            Err(e) => Err(anyhow!("failed to send message: {}", e)),
                        }
                    }
                    None => Err(anyhow!("unknown channel: {}", room_id)),
                };

                if let Some(tx) = response_tx {
                    let _ = tx.send(result);
                } else {
                    result?;
                }
            }
            Command::EditMessage { .. } => {
                warn!("mumble does not support editing messages");
            }
            Command::SendThreadReply { response_tx, .. } => {
                warn!("mumble does not support thread replies");
                if let Some(tx) = response_tx {
                    let _ = tx.send(Err(anyhow!("thread replies not supported by mumble")));
                }
            }
            Command::AddReaction { .. } => {
                warn!("mumble does not support reactions");
            }
            Command::SendRoomImage {
                room_id,
                caption,
                source_url,
                thumbnail_data,
                thumbnail_mimetype,
                ..
            } => {
                debug!(room_id=%room_id, "sending image to mumble channel");

                let state = self.state.lock().await;
                let result = match state.channel_ids.get(&room_id) {
                    Some(channel_id) => {
                        use base64::Engine as _;
                        let encoded =
                            base64::engine::general_purpose::STANDARD.encode(&thumbnail_data);
                        let html = image_message_html(
                            &caption,
                            &thumbnail_mimetype,
                            &encoded,
                            &source_url,
                        );

                        let mut msg = TextMessage::new();
                        msg.set_message(html);
                        msg.channel_id = vec![*channel_id];

                        match tx.send(ControlPacket::TextMessage(Box::new(msg))).await {
                            Ok(_) => Ok(String::new()),
                            Err(e) => Err(anyhow!("failed to send image message: {}", e)),
                        }
                    }
                    None => Err(anyhow!("unknown channel: {}", room_id)),
                };

                if let Err(e) = result {
                    error!(error=%e, room_id=%room_id, "failed to relay image to mumble");
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mumble_protocol_2x::control::ServerControlCodec;
    use tokio::io::DuplexStream;

    #[test]
    fn version_is_packed_the_way_mumble_expects() {
        // 1.5.0
        assert_eq!(version_v1(1, 5, 0), 0x0001_0500);
        assert_eq!(version_v2(1, 5, 0), 0x0001_0005_0000_0000);
        // Each part lands in its own field.
        assert_eq!(version_v1(1, 2, 3), 0x0001_0203);
        assert_eq!(version_v2(1, 2, 3), 0x0001_0002_0003_0000);
    }

    #[test]
    fn announced_version_is_older_than_the_new_voice_format() {
        // Servers switch to a voice format the codec cannot parse at 1.5.0.
        let announced = version_v1(VERSION_MAJOR, VERSION_MINOR, VERSION_PATCH);
        assert!(announced < version_v1(1, 5, 0), "announced {announced:#x}");
    }

    async fn service() -> MumbleService {
        let (evt_tx, _events) = mpsc::channel(16);
        MumbleService::create(
            ServiceId("mumble".to_string()),
            "mumble.example.com".to_string(),
            64738,
            "kelvin".to_string(),
            SecretString::from(""),
            false,
            evt_tx,
        )
        .await
        .unwrap()
    }

    /// The client and server ends of a Mumble control connection over an
    /// in-memory pipe.
    fn connection()
    -> (Framed<DuplexStream, ClientControlCodec>, Framed<DuplexStream, ServerControlCodec>) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        (
            Framed::new(client, ClientControlCodec::new()),
            Framed::new(server, ServerControlCodec::new()),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn silent_connection_is_given_up_on() {
        let service = service().await;
        // The server end stays open but never answers a ping.
        let (client, _server) = connection();

        let started = Instant::now();
        let result = service.process(client, CancellationToken::new()).await;

        let err = result.expect_err("a silent connection should be reported as dead");
        assert!(format!("{err:#}").contains("nothing received"), "got: {err:#}");
        assert_eq!(started.elapsed(), READ_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn connection_that_answers_pings_stays_up() {
        let service = service().await;
        let (client, mut server) = connection();
        tokio::spawn(async move {
            while let Some(Ok(packet)) = server.next().await {
                if matches!(packet, ControlPacket::Ping(_)) {
                    let _ = server.send(ControlPacket::Ping(Box::new(Ping::new()))).await;
                }
            }
        });

        let cancel = CancellationToken::new();
        let stop_later = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                tokio::time::sleep(READ_TIMEOUT * 10).await;
                cancel.cancel();
            }
        });

        let started = Instant::now();
        let result = service.process(client, cancel).await;

        result.expect("the connection should last until cancelled");
        assert!(started.elapsed() >= READ_TIMEOUT * 10, "ended after {:?}", started.elapsed());
        stop_later.await.unwrap();
    }

    #[test]
    fn escape_html_escapes_markup_characters() {
        assert_eq!(
            escape_html(r#"<a href="x">Tom & 'Jerry'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; &#39;Jerry&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn escape_html_leaves_ordinary_text_alone() {
        assert_eq!(escape_html("hello, world! 🎉 1 + 1 = 2"), "hello, world! 🎉 1 + 1 = 2");
        assert_eq!(escape_html(""), "");
    }

    #[test]
    fn escape_html_does_not_double_decode_entities() {
        // Text that already looks like an entity is shown literally.
        assert_eq!(escape_html("&lt;b&gt;"), "&amp;lt;b&amp;gt;");
    }

    #[test]
    fn plain_text_markup_is_not_interpreted() {
        let html = plain_text_to_html("[Matrix] Mallory: <img src=\"http://evil/x.png\">");
        assert!(!html.contains("<img"), "got: {html}");
        assert!(html.contains("&lt;img src=&quot;"), "got: {html}");
        // The address inside the escaped tag is still an ordinary link.
        assert!(html.contains("<a href=\"http://evil/x.png\">"), "got: {html}");
    }

    #[test]
    fn plain_text_line_breaks_become_br() {
        assert_eq!(plain_text_to_html("one\ntwo\r\nthree"), "one<br/>two<br/>three");
    }

    #[test]
    fn urls_become_links() {
        assert_eq!(
            plain_text_to_html("watch at https://example.com/live now"),
            "watch at <a href=\"https://example.com/live\">https://example.com/live</a> now"
        );
        assert_eq!(
            plain_text_to_html("http://example.com"),
            "<a href=\"http://example.com\">http://example.com</a>"
        );
    }

    #[test]
    fn multiple_urls_and_line_breaks_are_handled_together() {
        assert_eq!(
            plain_text_to_html("a https://one.example\nb https://two.example/x"),
            "a <a href=\"https://one.example\">https://one.example</a><br/>\
             b <a href=\"https://two.example/x\">https://two.example/x</a>"
        );
    }

    #[test]
    fn url_special_characters_are_escaped_in_link() {
        assert_eq!(
            plain_text_to_html("https://example.com/?a=1&b=2"),
            "<a href=\"https://example.com/?a=1&amp;b=2\">https://example.com/?a=1&amp;b=2</a>"
        );
    }

    #[test]
    fn url_cannot_break_out_of_the_link() {
        let html = plain_text_to_html("https://example.com/\"><script>alert(1)</script>");
        assert!(html.starts_with("<a href=\"https://example.com/\">https://example.com/</a>"));
        assert!(!html.contains("<script>"), "got: {html}");

        let html = plain_text_to_html("https://example.com/'onmouseover='x");
        assert!(!html.contains('\''), "got: {html}");
    }

    #[test]
    fn trailing_punctuation_is_not_linked() {
        let link = "<a href=\"https://example.com/a\">https://example.com/a</a>";
        assert_eq!(plain_text_to_html("see https://example.com/a."), format!("see {link}."));
        assert_eq!(plain_text_to_html("(https://example.com/a)"), format!("({link})"));
        assert_eq!(plain_text_to_html("https://example.com/a, ok?"), format!("{link}, ok?"));
        // The chat relay's image fallback wraps the URL in brackets.
        assert_eq!(
            plain_text_to_html("[image: https://example.com/a]"),
            format!("[image: {link}]")
        );
    }

    #[test]
    fn balanced_brackets_stay_in_the_url() {
        let url = "https://en.wikipedia.org/wiki/Rust_(programming_language)";
        assert_eq!(plain_text_to_html(url), format!("<a href=\"{url}\">{url}</a>"));
    }

    #[test]
    fn matrix_to_links_are_linked_whole() {
        let url = "https://matrix.to/#/!room:example.com/$event";
        assert_eq!(plain_text_to_html(url), format!("<a href=\"{url}\">{url}</a>"));
    }

    #[test]
    fn non_urls_are_not_linked() {
        for text in [
            "no links here",
            "https://",
            "http:// nothing",
            "xhttps://example.com",
            "ftp://example.com",
            "javascript:alert(1)",
        ] {
            assert!(!plain_text_to_html(text).contains("<a "), "{text:?} should not be linked");
        }
    }

    #[test]
    fn non_ascii_text_around_urls_is_preserved() {
        assert_eq!(
            plain_text_to_html("café → https://example.com/é ✓"),
            "café → <a href=\"https://example.com/é\">https://example.com/é</a> ✓"
        );
    }

    #[test]
    fn image_message_escapes_every_field() {
        let html = image_message_html(
            "[Matrix] <b>Mallory</b>:",
            "image/jpeg\"><script>",
            "QUJD",
            "https://matrix.to/#/!room:example.com/$event?a=1&b=\"2\"",
        );

        assert!(html.starts_with("[Matrix] &lt;b&gt;Mallory&lt;/b&gt;:<br/>"), "got: {html}");
        assert!(!html.contains("<script>"), "got: {html}");
        assert!(!html.contains("<b>"), "got: {html}");
        assert!(
            html.contains(
                "<a href=\"https://matrix.to/#/!room:example.com/$event?a=1&amp;b=&quot;2&quot;\">"
            ),
            "got: {html}"
        );
        assert!(html.contains("<img src=\"data:image/jpeg&quot;&gt;"), "got: {html}");
    }

    #[test]
    fn image_message_keeps_expected_shape() {
        assert_eq!(
            image_message_html("[Matrix] Alice:", "image/jpeg", "QUJD", "https://example.com/i"),
            "[Matrix] Alice:<br/><img src=\"data:image/jpeg;base64,QUJD\"/><br/>\
             <a href=\"https://example.com/i\">View full image</a>"
        );
    }

    #[test]
    fn image_message_only_links_http_urls() {
        let html = image_message_html("c", "image/jpeg", "QUJD", "javascript:alert(1)");
        assert!(!html.contains("<a "), "got: {html}");
        assert!(html.ends_with("<br/>javascript:alert(1)"), "got: {html}");
    }
}
