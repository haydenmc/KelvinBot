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
                            body: "hello from dummy".into()
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
            Command::SendDirectMessage { user_id, body, .. } => {
                info!(service=%self.id, user_id=%user_id, body=%body, "dummy service: would send DM");
            }
            Command::SendRoomMessage { room_id, body, .. } => {
                info!(service=%self.id, room_id=%room_id, body=%body, "dummy service: would send room message");
            }
        }
        Ok(())
    }
}
