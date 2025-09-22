use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::message::{Message, Address};

pub struct DummyService {
    pub name: String,
    pub interval_ms: u64,
    pub tx: tokio::sync::mpsc::Sender<Message>,
}

#[async_trait::async_trait]
impl crate::service::Service for DummyService {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(self.interval_ms));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(service=%self.name, "shutdown requested");
                    break;
                }
                _ = interval.tick() => {
                    let msg = Message {
                        from: Address {
                            service: self.name.clone(),
                            room_id: "room-1".into(),
                            user_id: None,
                        },
                        body: "hello from dummy".into(),
                    };
                    if let Err(e) = self.tx.send(msg).await {
                        tracing::warn!(?e, "bus receiver dropped; stopping");
                        break;
                    }
                }
            }
        }
        // Perform final cleanup here
        Ok(())
    }
}
