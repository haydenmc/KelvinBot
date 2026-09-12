use crate::core::{
    bus::Command,
    config::ExponentialBackoff,
    event::Event,
    middleware::{Middleware, MiddlewareContext, Verdict},
    service::ServiceId,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc::Sender};
use tokio::time::MissedTickBehavior;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Bytes,
    tungstenite::client::IntoClientRequest, tungstenite::protocol::Message,
};
use tokio_util::sync::CancellationToken;

/// How often the client sends a WebSocket ping to prove the connection is alive.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum time to wait for any frame (including pong replies) before the
/// connection is considered dead and torn down for reconnection.
const READ_TIMEOUT: Duration = Duration::from_secs(90);

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

pub struct EzStreamAnnounce {
    cmd_tx: Sender<Command>,
    websocket_url: String,
    stream_url_template: String,
    start_message_template: String,
    end_message_template: String,
    destinations: Vec<DestinationConfig>,
    state: Arc<Mutex<StreamState>>,
}

impl EzStreamAnnounce {
    pub fn new(
        ctx: MiddlewareContext,
        websocket_url: String,
        stream_url_template: String,
        start_message_template: String,
        end_message_template: String,
        destinations: Vec<DestinationConfig>,
    ) -> Self {
        Self {
            cmd_tx: ctx.cmd_tx,
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
        let reconnect_config = crate::core::config::ReconnectionConfig::default();
        let mut backoff = ExponentialBackoff::new(reconnect_config);
        let mut attempt = 0u32;

        loop {
            if cancel.is_cancelled() {
                tracing::info!("ezstream_announce shutting down");
                return Ok(());
            }

            attempt += 1;
            tracing::info!(
                attempt=%attempt,
                url=%self.websocket_url,
                "connecting to EzStream WebSocket"
            );

            match self.connect_and_process(cancel.clone()).await {
                Ok(_) => {
                    tracing::info!("EzStream WebSocket connection closed gracefully");
                    // If connection lasted long enough, reset backoff
                    if attempt > 1 {
                        backoff.reset();
                        attempt = 0;
                    }
                }
                Err(e) => {
                    tracing::error!(error=%e, "EzStream WebSocket connection failed");
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

            // Calculate backoff delay
            let delay = backoff.next_delay();
            tracing::info!(
                attempt=%attempt,
                delay_secs=%delay.as_secs(),
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

    // Connect to WebSocket and process messages
    async fn connect_and_process(&self, cancel: CancellationToken) -> Result<()> {
        // Build WebSocket request with subprotocol
        // We need to create a properly formed client request
        let mut request = self
            .websocket_url
            .as_str()
            .into_client_request()
            .context("failed to build WebSocket request")?;

        // Add the subprotocol header
        request.headers_mut().insert("Sec-WebSocket-Protocol", "stream-updates".parse().unwrap());

        let (ws_stream, response) = connect_async(request)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket connection failed: {}", e))?;

        tracing::info!(
            status = ?response.status(),
            "WebSocket connection established"
        );

        if let Err(e) = Self::enable_tcp_keepalive(&ws_stream) {
            tracing::warn!(error=%e, "failed to enable TCP keepalive on WebSocket socket");
        }

        let (mut write, mut read) = ws_stream.split();

        let mut ping_interval = tokio::time::interval(PING_INTERVAL);
        ping_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The first tick completes immediately; consume it so the first ping
        // is sent after a full interval.
        ping_interval.tick().await;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("closing WebSocket connection");
                    let _ = write.send(Message::Close(None)).await;
                    break;
                }
                _ = ping_interval.tick() => {
                    tracing::debug!("sending ping");
                    write
                        .send(Message::Ping(Bytes::new()))
                        .await
                        .context("failed to send WebSocket ping")?;
                }
                msg = tokio::time::timeout(READ_TIMEOUT, read.next()) => {
                    let msg = match msg {
                        Ok(msg) => msg,
                        Err(_) => {
                            anyhow::bail!(
                                "no WebSocket frames received in {}s; assuming connection is dead",
                                READ_TIMEOUT.as_secs()
                            );
                        }
                    };

                    match msg {
                        Some(Ok(Message::Text(text))) => {
                            tracing::debug!(message=%text, "received WebSocket message");

                            match serde_json::from_str::<ChannelNotification>(&text) {
                                Ok(notification) => {
                                    // Spawn task to handle notification asynchronously
                                    let self_clone = Self {
                                        cmd_tx: self.cmd_tx.clone(),
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

    fn on_event(&self, _event: &Event) -> Result<Verdict> {
        // This middleware doesn't react to events, only to WebSocket notifications
        Ok(Verdict::Continue)
    }
}
