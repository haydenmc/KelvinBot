use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use crate::{core::{event::Event, middleware::Middleware}, middleware::Verdict};

pub struct Logger;

#[async_trait]
impl Middleware for Logger {
    fn name(&self) -> &str { "logger" }

    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        tracing::info!("logger running...");
        cancel.cancelled().await;
        tracing::info!("logger shutting down...");
        Ok(())
    }

    fn on_event(&self, evt: &Event) -> anyhow::Result<Verdict> {
        tracing::info!("inbound event");
        Ok(Verdict::Continue)
    }
}