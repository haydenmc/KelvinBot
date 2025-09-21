use anyhow::Result;
use tokio_util::sync::CancellationToken;

#[async_trait::async_trait]
pub trait Service: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, cancel: CancellationToken) -> Result<()>;
}
