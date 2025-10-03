use std::sync::Arc;

use crate::core::bus::Command;
use crate::core::config::Config;
use crate::core::event::Event;
use crate::middlewares::{echo::Echo, logger::Logger};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy)]
pub enum Verdict {
    Continue,
    #[allow(dead_code)]
    Stop, // This will be used eventually.
}

#[async_trait]
pub trait Middleware: Send + Sync {
    async fn run(&self, cancel: CancellationToken) -> Result<()>;
    fn on_event(&self, event: &Event) -> Result<Verdict>;
}

/// Instantiates a list of Middleware based on given config
pub fn instantiate_middleware_from_config(
    _config: &Config,
    cmd_tx: &Sender<Command>,
) -> Vec<Arc<dyn Middleware>> {
    vec![Arc::new(Logger {}), Arc::new(Echo::new(cmd_tx.clone()))]
}
