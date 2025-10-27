use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, Verdict},
};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub struct Invite {
    cmd_tx: Sender<Command>,
    command_string: String,
}

impl Invite {
    pub fn new(cmd_tx: Sender<Command>, command_string: String) -> Self {
        Self { cmd_tx, command_string }
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
                        response_tx,
                    };

                    // Send the command and wait for the response
                    let cmd_tx = self.cmd_tx.clone();
                    let service_id = evt.service_id.clone();
                    let user_id_clone = user_id.clone();

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
                                format!(
                                    "Registration token generated: {}\n\n\
                                     Use this token when registering a new account on this server.",
                                    token
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
            EventKind::RoomMessage { .. } => {
                // Ignore room messages for invite commands
                tracing::debug!("invite command only supported in direct messages");
            }
        }

        Ok(Verdict::Continue)
    }
}
