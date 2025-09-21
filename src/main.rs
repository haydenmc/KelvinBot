mod config;
mod service;
mod services { pub mod dummy; }

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};
use service::Service;

use crate::config::ServiceKind;

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    init_tracing();

    info!("starting...");

    info!("loading configuration...");
    let cfg = config::load_from_env()?;

    // Build concrete services from config
    let cancel_all = CancellationToken::new();
    let mut handles = Vec::new();
    for (name, scfg) in &cfg.services {
        match scfg.kind {
            ServiceKind::Dummy => {
                let svc = services::dummy::DummyService {
                    name: name.clone(),
                    interval_ms: scfg.interval_ms.unwrap_or(1000),
                };
                let child_token = cancel_all.child_token();
                handles.push(tokio::spawn(async move { svc.run(child_token).await }));
            }
            _ => warn!("unknown service kind, skipping"),
        }
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            warn!("Ctrl+C received; shutting down…");
            cancel_all.cancel();
        }
    }

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

    info!("goodbye");
    Ok(())
}

fn init_tracing() {
    // RUST_LOG controls log level (ex. RUST_LOG=debug)
    // otherwise, default to "info"
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();
}