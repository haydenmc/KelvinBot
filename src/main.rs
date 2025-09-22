mod config;
mod service;
mod services {
    pub mod dummy;
}
mod message;
mod middleware;
mod middlewares {
    pub mod logger;
}
mod bus;

use std::sync::Arc;

use anyhow::Result;
use service::Service;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use crate::{config::ServiceKind, middleware::Middleware, middlewares::logger::Logger};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    init_tracing();

    info!("starting...");

    info!("loading configuration...");
    let cfg = config::load_from_env()?;

    // Bus channel: many producers (services) -> one consumer (bus)
    let (tx, rx) = bus::channel(1024);

    // Middlewares (just the logger for now)
    let middlewares: Vec<Arc<dyn middleware::Middleware>> = vec![Arc::new(Logger)];

    // Start bus
    let cancel_all = CancellationToken::new();
    let bus_cancel = cancel_all.child_token();
    let bus_task = tokio::spawn({
        let middlewares = middlewares.clone();
        async move { bus::Bus::new(rx, middlewares).run(bus_cancel).await }
    });

    // Build and start services
    let mut handles = Vec::new();
    for (name, scfg) in &cfg.services {
        match scfg.kind {
            ServiceKind::Dummy => {
                let svc = services::dummy::DummyService {
                    name: name.clone(),
                    interval_ms: scfg.interval_ms.unwrap_or(1000),
                    tx: tx.clone(),
                };
                let child_token = cancel_all.child_token();
                handles.push(tokio::spawn(async move { svc.run(child_token).await }));
            }
            _ => warn!("unknown service kind, skipping"),
        }
    }

    // Graceful shutdown on Ctrl+C
    tokio::signal::ctrl_c().await?;
    warn!("Ctrl+C received; shutting down…");
    cancel_all.cancel();

    // Join services
    for h in handles {
        match h.await {
            // Task joined OK, and the task returned Ok(())
            Ok(Ok(())) => {}

            // Task joined OK, but your task code returned an error
            Ok(Err(run_err)) => {
                warn!(?run_err, "task returned error");
            }

            // Join failed (panic or abort)
            Err(join_err) => {
                warn!(?join_err, "task panicked or was aborted");
            }
        }
    }

    // Join bus
    match bus_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(?e, "bus error"),
        Err(e) => warn!(?e, "bus panicked/aborted"),
    }

    info!("goodbye");
    Ok(())
}

fn init_tracing() {
    // RUST_LOG controls log level (ex. RUST_LOG=debug)
    // otherwise, default to "info"
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}
