use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, Verdict},
    service::ServiceId,
};

pub struct ChatRelayConfig {
    pub source_service_id: String,
    pub source_room_id: Option<String>,
    pub dest_service_id: String,
    pub dest_room_id: String,
    pub prefix_tag: String,
}

pub struct ChatRelay {
    cmd_tx: Sender<Command>,
    source_service_id: String,
    source_room_id: Option<String>,
    dest_service_id: String,
    dest_room_id: String,
    prefix_tag: String,
}

impl ChatRelay {
    pub fn new(cmd_tx: Sender<Command>, config: ChatRelayConfig) -> Self {
        Self {
            cmd_tx,
            source_service_id: config.source_service_id,
            source_room_id: config.source_room_id,
            dest_service_id: config.dest_service_id,
            dest_room_id: config.dest_room_id,
            prefix_tag: config.prefix_tag,
        }
    }

    fn format_relayed_message(
        prefix_tag: &str,
        sender_id: &str,
        sender_display_name: Option<&str>,
        body: &str,
    ) -> String {
        let sender_display = sender_display_name.unwrap_or(sender_id);
        format!("[{}] {}: {}", prefix_tag, sender_display, body)
    }
}

#[async_trait]
impl Middleware for ChatRelay {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        info!(
            source_service=%self.source_service_id,
            source_room=?self.source_room_id,
            dest_service=%self.dest_service_id,
            dest_room=%self.dest_room_id,
            prefix_tag=%self.prefix_tag,
            "chat_relay middleware running..."
        );
        cancel.cancelled().await;
        info!("chat_relay middleware shutting down...");
        Ok(())
    }

    fn on_event(&self, event: &Event) -> Result<Verdict> {
        // Filter: only handle events from source service
        if event.service_id.0 != self.source_service_id {
            return Ok(Verdict::Continue);
        }

        // Filter: only handle RoomMessage events
        let EventKind::RoomMessage {
            room_id, body, sender_id, sender_display_name, is_self, ..
        } = &event.kind
        else {
            return Ok(Verdict::Continue);
        };

        // Filter: check source room if specified
        if let Some(ref expected_room) = self.source_room_id
            && room_id != expected_room
        {
            return Ok(Verdict::Continue);
        }

        // Filter: ignore messages from the bot itself
        if *is_self {
            debug!("ignoring message from bot itself");
            return Ok(Verdict::Continue);
        }

        // Format and relay the message
        let formatted_body = Self::format_relayed_message(
            &self.prefix_tag,
            sender_id,
            sender_display_name.as_deref(),
            body,
        );

        let cmd_tx = self.cmd_tx.clone();
        let dest_service_id = ServiceId(self.dest_service_id.clone());
        let dest_room_id = self.dest_room_id.clone();

        tokio::spawn(async move {
            let command = Command::SendRoomMessage {
                service_id: dest_service_id.clone(),
                room_id: dest_room_id.clone(),
                body: formatted_body.clone(),
                markdown_body: Some(formatted_body),
                response_tx: None,
            };

            if let Err(e) = cmd_tx.send(command).await {
                error!(
                    dest_service=%dest_service_id.0,
                    dest_room=%dest_room_id,
                    error=%e,
                    "failed to send chat relay command"
                );
            }
        });

        Ok(Verdict::Continue)
    }
}
