use std::{collections::HashMap, sync::Arc};

use crate::core::bus::Command;
use crate::core::config::{Config, MiddlewareKind};
use crate::core::event::Event;
use crate::middlewares::{echo::Echo, logger::Logger};
use anyhow::{Result, bail};
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::warn;

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

/// Instantiates middleware instances from config as a HashMap keyed by middleware name
pub fn instantiate_middleware_from_config(
    config: &Config,
    cmd_tx: &Sender<Command>,
) -> Result<HashMap<String, Arc<dyn Middleware>>> {
    let mut middlewares = HashMap::new();

    for (name, cfg) in &config.middlewares {
        let middleware: Arc<dyn Middleware> = match &cfg.kind {
            MiddlewareKind::Echo { command_string } => {
                Arc::new(Echo::new(cmd_tx.clone(), command_string.clone()))
            }
            MiddlewareKind::Logger {} => Arc::new(Logger {}),
            MiddlewareKind::Unknown => {
                warn!(middleware_name=%name, "unknown middleware kind, skipping");
                continue;
            }
        };
        middlewares.insert(name.clone(), middleware);
    }

    Ok(middlewares)
}

/// Builds a Vec of middleware instances from a list of middleware names
pub fn build_middleware_pipeline(
    middleware_names: &[String],
    all_middlewares: &HashMap<String, Arc<dyn Middleware>>,
) -> Result<Vec<Arc<dyn Middleware>>> {
    let mut pipeline = Vec::new();

    for name in middleware_names {
        match all_middlewares.get(name) {
            Some(mw) => pipeline.push(mw.clone()),
            None => {
                bail!("middleware '{}' referenced but not defined in config", name);
            }
        }
    }

    Ok(pipeline)
}
