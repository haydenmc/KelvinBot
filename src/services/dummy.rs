use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    service::{Service, ServiceId},
};

pub struct DummyService {
    pub id: ServiceId,
    pub interval_ms: u64,
    pub evt_tx: tokio::sync::mpsc::Sender<Event>,
}

#[async_trait::async_trait]
impl Service for DummyService {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(self.interval_ms));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(service=%self.id, "shutdown requested");
                    break;
                }
                _ = interval.tick() => {
                    let msg = Event {
                        service_id: self.id.clone(),
                        kind: EventKind::RoomMessage{
                            room_id: "1".into(),
                            body: "hello from dummy".into(),
                            is_local_user: false,
                            sender_id: "dummy_user".into(),
                            sender_display_name: Some("Dummy User".into()),
                            is_self: false,
                        }
                    };
                    if let Err(e) = self.evt_tx.send(msg).await {
                        tracing::error!(?e, "bus event receiver dropped");
                        break;
                    }
                }
            }
        }
        // Perform final cleanup here
        Ok(())
    }

    async fn handle_command(&self, command: Command) -> Result<()> {
        match command {
            Command::SendDirectMessage { user_id, body, response_tx, .. } => {
                info!(service=%self.id, user_id=%user_id, body=%body, "dummy service: would send DM");
                if let Some(tx) = response_tx {
                    let _ = tx.send(Ok("dummy_message_id_dm".to_string()));
                }
            }
            Command::SendRoomMessage { room_id, body, response_tx, .. } => {
                info!(service=%self.id, room_id=%room_id, body=%body, "dummy service: would send room message");
                if let Some(tx) = response_tx {
                    let _ = tx.send(Ok("dummy_message_id_room".to_string()));
                }
            }
            Command::EditMessage { message_id, new_body, .. } => {
                info!(service=%self.id, message_id=%message_id, new_body=%new_body, "dummy service: would edit message");
            }
            Command::GenerateInviteToken { user_id, uses_allowed, expiry, response_tx, .. } => {
                info!(service=%self.id, user_id=%user_id, uses_allowed=?uses_allowed, expiry=?expiry, "dummy service: generating fake invite token");
                // Send a fake token response
                let _ = response_tx.send(Ok("DUMMY_TOKEN_12345".to_string()));
            }
        }
        Ok(())
    }
}
