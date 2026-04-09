use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, MiddlewareContext, Verdict},
};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub struct Echo {
    cmd_tx: Sender<Command>,
    command_string: String,
}

impl Echo {
    pub fn new(ctx: MiddlewareContext, command_string: String) -> Self {
        Self { cmd_tx: ctx.cmd_tx, command_string }
    }
}

#[async_trait]
impl Middleware for Echo {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        tracing::info!("echo middleware running...");
        cancel.cancelled().await;
        tracing::info!("echo middleware shutting down...");
        Ok(())
    }

    fn on_event(&self, evt: &Event) -> Result<Verdict> {
        // Only handle message events
        let (body, is_self) = match &evt.kind {
            EventKind::DirectMessage { body, is_self, .. } => (body, *is_self),
            EventKind::RoomMessage { body, is_self, .. } => (body, *is_self),
            EventKind::UserListUpdate { .. }
            | EventKind::ReactionAdded { .. }
            | EventKind::ReactionRemoved { .. } => return Ok(Verdict::Continue),
        };

        // Ignore messages from self to prevent infinite recursion
        if is_self {
            return Ok(Verdict::Continue);
        }

        // Build the prefix with a trailing space
        let prefix = format!("{} ", self.command_string);
        if let Some(echo_content) = body.strip_prefix(&prefix) {
            // Create a oneshot channel to receive the message ID
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();

            // Create the appropriate command based on the event type
            let command = match &evt.kind {
                EventKind::DirectMessage { user_id, .. } => Command::SendDirectMessage {
                    service_id: evt.service_id.clone(),
                    user_id: user_id.clone(),
                    body: echo_content.to_string(),
                    response_tx: Some(response_tx),
                },
                EventKind::RoomMessage { room_id, .. } => Command::SendRoomMessage {
                    service_id: evt.service_id.clone(),
                    room_id: room_id.clone(),
                    body: echo_content.to_string(),
                    markdown_body: None,
                    response_tx: Some(response_tx),
                },
                EventKind::UserListUpdate { .. }
                | EventKind::ReactionAdded { .. }
                | EventKind::ReactionRemoved { .. } => unreachable!(),
            };

            // Send the command and wait for the message ID
            let cmd_tx = self.cmd_tx.clone();
            let echo_content_clone = echo_content.to_string();
            tokio::spawn(async move {
                if let Err(e) = cmd_tx.send(command).await {
                    tracing::error!(error=%e, "failed to send echo command");
                    return;
                }

                // Wait for the message ID response
                match response_rx.await {
                    Ok(Ok(message_id)) => {
                        tracing::debug!(
                            message_id=%message_id,
                            echo_content=%echo_content_clone,
                            "echo message sent successfully with message ID"
                        );
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error=%e, "failed to send echo message");
                    }
                    Err(e) => {
                        tracing::error!(error=%e, "failed to receive message ID response");
                    }
                }
            });

            tracing::info!(echo_content=%echo_content, "processed echo command");
        }

        Ok(Verdict::Continue)
    }
}
