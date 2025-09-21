use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub struct DummyService {
    pub name: String,
    pub interval_ms: u64,
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
                    info!(service=%self.name, "tick")
                }
            }
        }
        // Perform final cleanup here
        Ok(())
    }
}
