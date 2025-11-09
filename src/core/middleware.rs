use std::{collections::HashMap, sync::Arc};

use crate::core::bus::Command;
use crate::core::config::{Config, MiddlewareKind};
use crate::core::event::Event;
use crate::middlewares::{
    echo::Echo, invite::Invite, logger::Logger, regal_showtimes::RegalShowtimes,
};
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
            MiddlewareKind::Invite { command_string, uses_allowed, expiry } => Arc::new(
                Invite::new(cmd_tx.clone(), command_string.clone(), *uses_allowed, *expiry),
            ),
            MiddlewareKind::Logger {} => Arc::new(Logger {}),
            MiddlewareKind::RegalShowtimes {
                service_id,
                room_id,
                day_of_week,
                time,
                theater_id,
            } => {
                // Parse day_of_week string to Weekday
                let weekday = day_of_week.parse::<chrono::Weekday>()
                    .map_err(|_| anyhow::anyhow!(
                        "invalid day_of_week '{}' for middleware '{}'. Valid values: Monday, Tuesday, Wednesday, Thursday, Friday, Saturday, Sunday",
                        day_of_week, name
                    ))?;

                // Parse time string (HH:MM format)
                let naive_time = chrono::NaiveTime::parse_from_str(time, "%H:%M")
                    .map_err(|_| anyhow::anyhow!(
                        "invalid time format '{}' for middleware '{}'. Expected format: HH:MM (e.g., 18:00)",
                        time, name
                    ))?;

                Arc::new(RegalShowtimes::new(
                    cmd_tx.clone(),
                    service_id.clone(),
                    room_id.clone(),
                    weekday,
                    naive_time,
                    theater_id.clone(),
                ))
            }
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
