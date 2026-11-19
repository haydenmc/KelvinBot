use crate::core::{
    bus::Command,
    config::{ExponentialBackoff, ReconnectionConfig},
    event::Event,
    middleware::{Middleware, MiddlewareContext},
    service::ServiceId,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{Sink, SinkExt, Stream, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc::Sender};
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Bytes,
    tungstenite::Error as WsError, tungstenite::client::IntoClientRequest,
    tungstenite::protocol::Message,
};
use tokio_util::sync::CancellationToken;

/// How often the client sends a WebSocket ping to prove the connection is alive.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum time to wait for any frame (including pong replies) before the
/// connection is considered dead and torn down for reconnection.
const READ_TIMEOUT: Duration = Duration::from_secs(90);

/// Maximum time to wait for the TCP, TLS and WebSocket handshakes together.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time to spend sending a close frame on shutdown.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// A connection that stays up at least this long is considered healthy, so the
/// next reconnection starts again from the initial backoff delay. Longer than
/// `READ_TIMEOUT`, so a server that accepts connections but never speaks does
/// not count.
const STABLE_CONNECTION: Duration = Duration::from_secs(120);

/// TCP keepalive settings applied to the underlying socket. These keep NAT and
/// conntrack entries fresh and let the kernel detect a dead peer even if no
/// WebSocket traffic is flowing.
const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(60);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const TCP_KEEPALIVE_RETRIES: u32 = 3;

// Configuration for a single announcement destination
#[derive(Debug, Clone)]
pub struct DestinationConfig {
    pub service_id: String,
    pub room_id: String,
}

// EzStream WebSocket message format
#[derive(Debug, Deserialize)]
struct ChannelNotification {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "IsLive")]
    is_live: bool,
}

// Tracks information about an active stream
struct ActiveStream {
    name: String,
    start_time: DateTime<Utc>,
    // Maps (service_id, room_id) -> message_id
    message_ids: HashMap<(String, String), String>,
}

// Shared state for tracking active streams
struct StreamState {
    active_streams: HashMap<String, ActiveStream>, // channel_id -> stream info
}

impl StreamState {
    fn new() -> Self {
        Self { active_streams: HashMap::new() }
    }
}

/// Decides how long to wait before each reconnection attempt.
struct Reconnect {
    backoff: ExponentialBackoff,
    /// Consecutive attempts that did not produce a stable connection.
    failures: u32,
}

impl Reconnect {
    fn new(config: ReconnectionConfig) -> Self {
        Self { backoff: ExponentialBackoff::new(config), failures: 0 }
    }

    /// The delay to wait after a connection attempt ends. `connected_for` is
    /// how long the connection was up, or `None` if it was never established.
    ///
    /// The delay grows while attempts keep failing or connections keep
    /// dropping early, and starts over once a connection has proved stable,
    /// however it ended.
    fn delay_after(&mut self, connected_for: Option<Duration>) -> Duration {
        if connected_for.is_some_and(|up| up >= STABLE_CONNECTION) {
            self.backoff.reset();
            self.failures = 0;
        }
        self.failures += 1;
        self.backoff.next_delay()
    }
}

pub struct EzStreamAnnounce {
    cmd_tx: Sender<Command>,
    websocket_url: String,
    stream_url_template: String,
    start_message_template: String,
    reconnection: ReconnectionConfig,
    end_message_template: String,
    destinations: Vec<DestinationConfig>,
    state: Arc<Mutex<StreamState>>,
}

impl EzStreamAnnounce {
    pub fn new(
        ctx: MiddlewareContext,
        reconnection: ReconnectionConfig,
        websocket_url: String,
        stream_url_template: String,
        start_message_template: String,
        end_message_template: String,
        destinations: Vec<DestinationConfig>,
    ) -> Self {
        Self {
            cmd_tx: ctx.cmd_tx,
            reconnection,
            websocket_url,
            stream_url_template,
            start_message_template,
            end_message_template,
            destinations,
            state: Arc::new(Mutex::new(StreamState::new())),
        }
    }

    // Build stream URL from template
    fn format_stream_url(&self, channel_name: &str, channel_id: &str) -> String {
        let mut result = self.stream_url_template.replace("{{id}}", channel_id);
        result = result.replace("{{name}}", channel_name);
        result
    }

    // Format message with placeholder substitution
    fn format_message(
        &self,
        template: &str,
        channel_name: &str,
        channel_id: &str,
        duration: Option<chrono::Duration>,
    ) -> String {
        let stream_url = self.format_stream_url(channel_name, channel_id);

        let mut result = template.replace("{{name}}", channel_name);
        result = result.replace("{{id}}", channel_id);
        result = result.replace("{{url}}", &stream_url);

        if let Some(dur) = duration {
            let hours = dur.num_hours();
            let minutes = dur.num_minutes() % 60;
            let seconds = dur.num_seconds() % 60;

            let duration_str = if hours > 0 {
                format!("{}h {}m {}s", hours, minutes, seconds)
            } else if minutes > 0 {
                format!("{}m {}s", minutes, seconds)
            } else {
                format!("{}s", seconds)
            };
            result = result.replace("{{duration}}", &duration_str);
        }

        result
    }

    // Handle stream going live
    async fn handle_stream_start(&self, channel_id: String, channel_name: String) -> Result<()> {
        tracing::info!(
            channel_id=%channel_id,
            channel_name=%channel_name,
            "stream started"
        );

        let message_body =
            self.format_message(&self.start_message_template, &channel_name, &channel_id, None);

        // Send announcements to all configured destinations. This is done
        // before taking the state lock so that a slow or stuck service
        // cannot block every other notification behind the mutex.
        let mut message_ids = HashMap::new();
        for dest in &self.destinations {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();

            let command = Command::SendRoomMessage {
                service_id: ServiceId(dest.service_id.clone()),
                room_id: dest.room_id.clone(),
                body: message_body.clone(),
                markdown_body: Some(message_body.clone()),
                response_tx: Some(response_tx),
            };

            self.cmd_tx.send(command).await?;

            // Store message ID for later editing
            match response_rx.await {
                Ok(Ok(message_id)) => {
                    message_ids.insert((dest.service_id.clone(), dest.room_id.clone()), message_id);
                    tracing::info!(
                        service_id=%dest.service_id,
                        room_id=%dest.room_id,
                        "stream start announcement sent"
                    );
                }
                Ok(Err(e)) => {
                    tracing::error!(
                        error=%e,
                        service_id=%dest.service_id,
                        room_id=%dest.room_id,
                        "failed to send stream start announcement"
                    );
                }
                Err(e) => {
                    tracing::error!(
                        error=%e,
                        service_id=%dest.service_id,
                        room_id=%dest.room_id,
                        "failed to receive message ID"
                    );
                }
            }
        }

        // Track this stream in state
        let mut state = self.state.lock().await;
        state.active_streams.insert(
            channel_id.clone(),
            ActiveStream { name: channel_name, start_time: Utc::now(), message_ids },
        );

        Ok(())
    }

    // Handle stream going offline
    async fn handle_stream_end(&self, channel_id: String) -> Result<()> {
        tracing::info!(channel_id=%channel_id, "stream ended");

        // Remove stream from active list and get its info, releasing the
        // lock before issuing any commands to the bus.
        let removed = self.state.lock().await.active_streams.remove(&channel_id);
        let Some(stream) = removed else {
            tracing::warn!(
                channel_id=%channel_id,
                "received stream end notification for unknown stream"
            );
            return Ok(());
        };

        let duration = Utc::now() - stream.start_time;

        let message_body = self.format_message(
            &self.end_message_template,
            &stream.name,
            &channel_id,
            Some(duration),
        );

        // Edit all the original announcements
        for ((service_id, room_id), message_id) in stream.message_ids {
            let command = Command::EditMessage {
                service_id: ServiceId(service_id.clone()),
                room_id: room_id.clone(),
                message_id,
                new_body: message_body.clone(),
                new_markdown_body: Some(message_body.clone()),
            };

            if let Err(e) = self.cmd_tx.send(command).await {
                tracing::error!(
                    error=%e,
                    service_id=%service_id,
                    room_id=%room_id,
                    "failed to edit stream end message"
                );
            } else {
                tracing::info!(
                    service_id=%service_id,
                    room_id=%room_id,
                    "stream end message edited"
                );
            }
        }

        Ok(())
    }

    // Process a notification from EzStream
    async fn handle_notification(&self, notification: ChannelNotification) -> Result<()> {
        if notification.is_live {
            self.handle_stream_start(notification.id, notification.name).await
        } else {
            self.handle_stream_end(notification.id).await
        }
    }

    // Main WebSocket connection loop with reconnection
    async fn websocket_loop(&self, cancel: CancellationToken) -> Result<()> {
        let mut reconnect = Reconnect::new(self.reconnection.clone());

        loop {
            if cancel.is_cancelled() {
                tracing::info!("ezstream_announce shutting down");
                return Ok(());
            }

            tracing::info!(
                failures=%reconnect.failures,
                url=%self.websocket_url,
                "connecting to EzStream WebSocket"
            );

            // How long the connection was up, if it was established at all.
            let mut connected_for = None;
            match self.connect(&cancel).await {
                Ok(Some(ws_stream)) => {
                    let connected_at = Instant::now();
                    match self.process_connection(ws_stream, cancel.clone()).await {
                        Ok(()) => tracing::info!("EzStream WebSocket connection closed"),
                        Err(e) => {
                            tracing::error!(error=%format!("{e:#}"), "EzStream WebSocket connection lost")
                        }
                    }
                    connected_for = Some(connected_at.elapsed());
                }
                // Cancelled while connecting.
                Ok(None) => return Ok(()),
                Err(e) => {
                    tracing::error!(error=%format!("{e:#}"), "EzStream WebSocket connection failed")
                }
            }

            // Clear state after disconnection
            {
                let mut state = self.state.lock().await;
                state.active_streams.clear();
                tracing::info!("cleared stream state after disconnection");
            }

            if cancel.is_cancelled() {
                return Ok(());
            }

            let delay = reconnect.delay_after(connected_for);
            tracing::info!(
                failures=%reconnect.failures,
                delay_secs=%delay.as_secs_f64(),
                "waiting before reconnection attempt"
            );

            // Sleep with cancellation support
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("cancellation during backoff");
                    return Ok(());
                }
                _ = tokio::time::sleep(delay) => {
                    // Continue to next connection attempt
                }
            }
        }
    }

    /// Opens the WebSocket connection. Returns `None` if cancelled first.
    async fn connect(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<WebSocketStream<MaybeTlsStream<TcpStream>>>> {
        // Build WebSocket request with subprotocol
        // We need to create a properly formed client request
        let mut request = self
            .websocket_url
            .as_str()
            .into_client_request()
            .context("failed to build WebSocket request")?;

        // Add the subprotocol header
        request.headers_mut().insert("Sec-WebSocket-Protocol", "stream-updates".parse().unwrap());

        // Bounded, because a server that accepts the TCP connection and then
        // never completes the handshake would otherwise hold us here forever.
        let connecting = tokio::time::timeout(CONNECT_TIMEOUT, connect_async(request));
        let (ws_stream, response) = tokio::select! {
            _ = cancel.cancelled() => return Ok(None),
            connected = connecting => connected
                .map_err(|_| {
                    anyhow::anyhow!(
                        "WebSocket connection not established within {}s",
                        CONNECT_TIMEOUT.as_secs()
                    )
                })?
                .context("WebSocket connection failed")?,
        };

        tracing::info!(
            status = ?response.status(),
            "WebSocket connection established"
        );

        if let Err(e) = Self::enable_tcp_keepalive(&ws_stream) {
            tracing::warn!(error=%e, "failed to enable TCP keepalive on WebSocket socket");
        }

        Ok(Some(ws_stream))
    }

    /// Processes one established connection until it closes, fails, goes
    /// silent for `READ_TIMEOUT`, or `cancel` fires.
    async fn process_connection<S>(&self, ws_stream: S, cancel: CancellationToken) -> Result<()>
    where
        S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
    {
        let (mut write, mut read) = ws_stream.split();

        let mut ping_interval = tokio::time::interval(PING_INTERVAL);
        ping_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The first tick completes immediately; consume it so the first ping
        // is sent after a full interval.
        ping_interval.tick().await;

        // When the connection is given up on. Pushed back by every frame that
        // arrives, and by nothing else: it must outlive the iterations of this
        // loop, or the ping tick (which is more frequent) would keep restarting
        // the wait and it could never expire.
        let mut read_deadline = Instant::now() + READ_TIMEOUT;
        let silent = || {
            anyhow::anyhow!(
                "no WebSocket frames received in {}s; assuming connection is dead",
                READ_TIMEOUT.as_secs()
            )
        };

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("closing WebSocket connection");
                    let _ = tokio::time::timeout(CLOSE_TIMEOUT, write.send(Message::Close(None))).await;
                    break;
                }
                _ = tokio::time::sleep_until(read_deadline) => return Err(silent()),
                _ = ping_interval.tick() => {
                    tracing::debug!("sending ping");
                    // A dead peer can also stop accepting writes, so the send
                    // is held to the same deadline.
                    tokio::time::timeout_at(read_deadline, write.send(Message::Ping(Bytes::new())))
                        .await
                        .map_err(|_| silent())?
                        .context("failed to send WebSocket ping")?;
                }
                msg = read.next() => {
                    if matches!(msg, Some(Ok(_))) {
                        read_deadline = Instant::now() + READ_TIMEOUT;
                    }

                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            tracing::debug!(message=%text, "received WebSocket message");

                            match serde_json::from_str::<ChannelNotification>(&text) {
                                Ok(notification) => {
                                    // Spawn task to handle notification asynchronously
                                    let self_clone = Self {
                                        cmd_tx: self.cmd_tx.clone(),
                                        reconnection: self.reconnection.clone(),
                                        websocket_url: self.websocket_url.clone(),
                                        stream_url_template: self.stream_url_template.clone(),
                                        start_message_template: self.start_message_template.clone(),
                                        end_message_template: self.end_message_template.clone(),
                                        destinations: self.destinations.clone(),
                                        state: self.state.clone(),
                                    };
                                    tokio::spawn(async move {
                                        if let Err(e) = self_clone.handle_notification(notification).await {
                                            tracing::error!(error=%e, "failed to handle notification");
                                        }
                                    });
                                }
                                Err(e) => {
                                    tracing::error!(error=%e, text=%text, "failed to parse notification");
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            tracing::info!("WebSocket server closed connection");
                            break;
                        }
                        Some(Ok(Message::Ping(data))) => {
                            tracing::debug!("received ping, sending pong");
                            let _ = write.send(Message::Pong(data)).await;
                        }
                        Some(Ok(Message::Pong(_))) => {
                            tracing::debug!("received pong");
                        }
                        Some(Ok(_)) => {
                            // Ignore other message types (Binary, Frame, etc.)
                        }
                        Some(Err(e)) => {
                            tracing::error!(error=%e, "WebSocket error");
                            return Err(e.into());
                        }
                        None => {
                            tracing::warn!("WebSocket stream ended");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    // Enable TCP keepalive on the socket underlying a WebSocket stream
    fn enable_tcp_keepalive(ws_stream: &WebSocketStream<MaybeTlsStream<TcpStream>>) -> Result<()> {
        let tcp: &TcpStream = match ws_stream.get_ref() {
            MaybeTlsStream::Plain(tcp) => tcp,
            MaybeTlsStream::NativeTls(tls) => tls.get_ref().get_ref().get_ref(),
            _ => anyhow::bail!("unsupported stream type"),
        };

        let keepalive = socket2::TcpKeepalive::new()
            .with_time(TCP_KEEPALIVE_IDLE)
            .with_interval(TCP_KEEPALIVE_INTERVAL)
            .with_retries(TCP_KEEPALIVE_RETRIES);

        socket2::SockRef::from(tcp)
            .set_tcp_keepalive(&keepalive)
            .context("failed to set TCP keepalive")?;

        tracing::debug!(
            idle_secs=%TCP_KEEPALIVE_IDLE.as_secs(),
            interval_secs=%TCP_KEEPALIVE_INTERVAL.as_secs(),
            retries=%TCP_KEEPALIVE_RETRIES,
            "TCP keepalive enabled"
        );
        Ok(())
    }
}

#[async_trait]
impl Middleware for EzStreamAnnounce {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        tracing::info!(
            url=%self.websocket_url,
            destinations=%self.destinations.len(),
            "ezstream_announce middleware running"
        );

        self.websocket_loop(cancel).await
    }

    async fn on_event(&self, _event: &Event) -> Result<()> {
        // This middleware doesn't react to events, only to WebSocket notifications
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PersistentStore;
    use tokio::io::DuplexStream;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::protocol::Role;

    fn middleware() -> (EzStreamAnnounce, mpsc::Receiver<Command>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let ctx = MiddlewareContext { cmd_tx, store: Arc::new(PersistentStore::in_memory()) };
        let middleware = EzStreamAnnounce::new(
            ctx,
            ReconnectionConfig::default(),
            "ws://unused".to_string(),
            "https://example.com/{{name}}".to_string(),
            "{{name}} is live".to_string(),
            "{{name}} was live for {{duration}}".to_string(),
            Vec::new(),
        );
        (middleware, cmd_rx)
    }

    /// Both ends of a WebSocket over an in-memory pipe, with no handshake.
    async fn websocket_pair() -> (WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        (
            WebSocketStream::from_raw_socket(client, Role::Client, None).await,
            WebSocketStream::from_raw_socket(server, Role::Server, None).await,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn silent_connection_is_given_up_on_after_the_read_timeout() {
        let (middleware, _commands) = middleware();
        // The server end stays open but is never read, so no pong comes back.
        let (client, _server) = websocket_pair().await;

        let started = Instant::now();
        let result = middleware.process_connection(client, CancellationToken::new()).await;

        let err = result.expect_err("a silent connection should be reported as dead");
        assert!(format!("{err:#}").contains("no WebSocket frames received"), "got: {err:#}");
        assert_eq!(started.elapsed(), READ_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn connection_that_answers_pings_stays_up() {
        let (middleware, _commands) = middleware();
        let (client, mut server) = websocket_pair().await;
        // Reading is enough for the server end to answer pings with pongs.
        tokio::spawn(async move { while server.next().await.is_some() {} });

        let cancel = CancellationToken::new();
        let stop_later = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                tokio::time::sleep(READ_TIMEOUT * 10).await;
                cancel.cancel();
            }
        });

        let started = Instant::now();
        let result = middleware.process_connection(client, cancel).await;

        result.expect("the connection should last until cancelled");
        assert!(started.elapsed() >= READ_TIMEOUT * 10, "ended after {:?}", started.elapsed());
        stop_later.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn silence_is_measured_from_the_last_frame() {
        let (middleware, _commands) = middleware();
        let (client, mut server) = websocket_pair().await;

        // One frame part-way through, then nothing (and no pongs, since the
        // server end is never read).
        let quiet_after = Duration::from_secs(50);
        let server_task = tokio::spawn(async move {
            tokio::time::sleep(quiet_after).await;
            server.send(Message::Text("not a notification".into())).await.unwrap();
            std::future::pending::<()>().await;
        });

        let started = Instant::now();
        let result = middleware.process_connection(client, CancellationToken::new()).await;

        assert!(result.is_err());
        assert_eq!(started.elapsed(), quiet_after + READ_TIMEOUT);
        server_task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn close_from_the_server_ends_the_connection_cleanly() {
        let (middleware, _commands) = middleware();
        let (client, mut server) = websocket_pair().await;
        server.send(Message::Close(None)).await.unwrap();

        let result = middleware.process_connection(client, CancellationToken::new()).await;

        result.expect("a server close is not an error");
    }

    fn steady_backoff() -> Reconnect {
        Reconnect::new(ReconnectionConfig {
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            multiplier: 2.0,
            jitter_factor: 0.0,
        })
    }

    #[test]
    fn delay_grows_while_connections_fail_or_drop_early() {
        let mut reconnect = steady_backoff();

        assert_eq!(reconnect.delay_after(None), Duration::from_secs(1));
        assert_eq!(reconnect.delay_after(None), Duration::from_secs(2));
        // Connected, but dropped again straight away: still backing off.
        assert_eq!(reconnect.delay_after(Some(Duration::from_secs(5))), Duration::from_secs(4));
        // Accepted and then silent until the read timeout: likewise.
        assert_eq!(reconnect.delay_after(Some(READ_TIMEOUT)), Duration::from_secs(8));
        assert_eq!(reconnect.failures, 4);
    }

    #[test]
    fn delay_starts_over_after_a_stable_connection() {
        let mut reconnect = steady_backoff();
        for _ in 0..5 {
            reconnect.delay_after(None);
        }

        // However the stable connection ended, the next wait is the initial one.
        assert_eq!(reconnect.delay_after(Some(STABLE_CONNECTION)), Duration::from_secs(1));
        assert_eq!(reconnect.failures, 1);
        assert_eq!(reconnect.delay_after(None), Duration::from_secs(2));
    }
}
