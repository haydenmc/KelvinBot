use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, Verdict},
};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub struct Echo {
    cmd_tx: Sender<Command>,
}

impl Echo {
    pub fn new(cmd_tx: Sender<Command>) -> Self {
        Self { cmd_tx }
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
        // Check if the message starts with "!echo "
        let body = match &evt.kind {
            EventKind::DirectMessage { body, .. } => body,
            EventKind::RoomMessage { body, .. } => body,
        };

        if let Some(echo_content) = body.strip_prefix("!echo ") {
            // Create the appropriate command based on the event type
            let command = match &evt.kind {
                EventKind::DirectMessage { user_id, .. } => Command::SendDirectMessage {
                    service_id: evt.service_id.clone(),
                    user_id: user_id.clone(),
                    body: echo_content.to_string(),
                },
                EventKind::RoomMessage { room_id, .. } => Command::SendRoomMessage {
                    service_id: evt.service_id.clone(),
                    room_id: room_id.clone(),
                    body: echo_content.to_string(),
                },
            };

            // Send the command asynchronously
            let cmd_tx = self.cmd_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = cmd_tx.send(command).await {
                    tracing::error!(error=%e, "failed to send echo command");
                }
            });

            tracing::info!(echo_content=%echo_content, "processed echo command");
        }

        Ok(Verdict::Continue)
    }
}