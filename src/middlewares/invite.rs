use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, Verdict},
};
use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub struct Invite {
    cmd_tx: Sender<Command>,
    command_string: String,
    uses_allowed: Option<u32>,
    expiry: Option<Duration>,
}

impl Invite {
    pub fn new(
        cmd_tx: Sender<Command>,
        command_string: String,
        uses_allowed: Option<u32>,
        expiry: Option<Duration>,
    ) -> Self {
        Self { cmd_tx, command_string, uses_allowed, expiry }
    }
}

#[async_trait]
impl Middleware for Invite {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        tracing::info!("invite middleware running...");
        cancel.cancelled().await;
        tracing::info!("invite middleware shutting down...");
        Ok(())
    }

    fn on_event(&self, evt: &Event) -> Result<Verdict> {
        match &evt.kind {
            EventKind::UserListUpdate { .. } | EventKind::RoomMessage { .. } => {
                // Ignore non-DM events
                return Ok(Verdict::Continue);
            }
            EventKind::DirectMessage { body, user_id, is_local_user } => {
                // Check if the message is the invite command
                if body.trim() == self.command_string {
                    // Only process if user is from the same homeserver/instance
                    if !is_local_user {
                        tracing::info!(
                            user_id=%user_id,
                            "ignoring invite request from non-local user"
                        );

                        // Send a message back explaining why
                        let command = Command::SendDirectMessage {
                            service_id: evt.service_id.clone(),
                            user_id: user_id.clone(),
                            body: "Invite tokens can only be generated for users from this server."
                                .to_string(),
                        };

                        let cmd_tx = self.cmd_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = cmd_tx.send(command).await {
                                tracing::error!(error=%e, "failed to send rejection message");
                            }
                        });

                        return Ok(Verdict::Continue);
                    }

                    // Create oneshot channel for the response
                    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

                    // Create the GenerateInviteToken command
                    let command = Command::GenerateInviteToken {
                        service_id: evt.service_id.clone(),
                        user_id: user_id.clone(),
                        uses_allowed: self.uses_allowed,
                        expiry: self.expiry,
                        response_tx,
                    };

                    // Send the command and wait for the response
                    let cmd_tx = self.cmd_tx.clone();
                    let service_id = evt.service_id.clone();
                    let user_id_clone = user_id.clone();
                    let uses_allowed = self.uses_allowed.unwrap_or(1);
                    let expiry_duration =
                        self.expiry.unwrap_or(Duration::from_secs(7 * 24 * 60 * 60));

                    tokio::spawn(async move {
                        // Send the command
                        if let Err(e) = cmd_tx.send(command).await {
                            tracing::error!(error=%e, "failed to send generate invite token command");
                            return;
                        }

                        // Wait for the response
                        let result = match response_rx.await {
                            Ok(result) => result,
                            Err(e) => {
                                tracing::error!(error=%e, "failed to receive token response (service may have crashed)");
                                return;
                            }
                        };

                        // Format the response message
                        let message = match result {
                            Ok(token) => {
                                tracing::info!(user_id=%user_id_clone, "token generated successfully");

                                // Calculate expiration time
                                let expiry_time = std::time::SystemTime::now() + expiry_duration;
                                let expiry_datetime = expiry_time
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| {
                                        let secs = d.as_secs();
                                        let dt = chrono::DateTime::from_timestamp(secs as i64, 0)
                                            .unwrap_or_default();
                                        dt.format("%Y-%m-%d %H:%M:%S UTC").to_string()
                                    })
                                    .unwrap_or_else(|_| "unknown".to_string());

                                format!(
                                    "Registration token generated: {}\n\n\
                                     Uses allowed: {}\n\
                                     Expires: {}\n\n\
                                     Use this token when registering a new account on this server.",
                                    token, uses_allowed, expiry_datetime
                                )
                            }
                            Err(e) => {
                                tracing::error!(user_id=%user_id_clone, error=%e, "token generation failed");
                                format!(
                                    "Failed to generate registration token. \
                                     The bot may not have admin permissions. Error: {}",
                                    e
                                )
                            }
                        };

                        // Send the result back to the user
                        let reply_command = Command::SendDirectMessage {
                            service_id,
                            user_id: user_id_clone,
                            body: message,
                        };

                        if let Err(e) = cmd_tx.send(reply_command).await {
                            tracing::error!(error=%e, "failed to send invite token response");
                        }
                    });

                    tracing::info!(user_id=%user_id, "processing invite command");
                }
            }
        }

        Ok(Verdict::Continue)
    }
}
