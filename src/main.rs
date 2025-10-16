use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use kelvin_bot::core::{bus, config::load_from_env, middleware, service};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    init_tracing();

    info!("starting...");

    info!("loading configuration...");
    let cfg = load_from_env()?;

    // Event channel: many producers (services) -> one consumer (bus)
    let (cmd_tx, cmd_rx) = bus::create_command_channel(1024);
    // Command channel: many producers (middleware) -> one consumer (bus)
    let (evt_tx, evt_rx) = bus::create_event_channel(1024);

    info!("instantiating services...");
    let services = service::instantiate_services_from_config(&cfg, &evt_tx).await?;

    info!("instantiating middlewares...");
    let all_middlewares = middleware::instantiate_middleware_from_config(&cfg, &cmd_tx)?;

    info!("building service middleware pipelines...");
    let mut service_middlewares = std::collections::HashMap::new();
    for (service_name, service_cfg) in &cfg.services {
        if let Some(ref middleware_list) = service_cfg.middleware {
            let pipeline =
                middleware::build_middleware_pipeline(middleware_list, &all_middlewares)?;
            service_middlewares.insert(service::ServiceId(service_name.clone()), pipeline);
        }
    }

    // Start bus
    let cancel_all = CancellationToken::new();
    let bus_cancel = cancel_all.child_token();
    let bus_task = tokio::spawn({
        async move { bus::Bus::new(evt_rx, cmd_rx, services, service_middlewares).run(bus_cancel).await }
    });

    // Graceful shutdown on Ctrl+C
    tokio::signal::ctrl_c().await?;
    info!("Ctrl+C received; shutting down…");
    cancel_all.cancel();

    // Join bus
    match bus_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(?e, "bus error"),
        Err(e) => warn!(?e, "bus task panicked/aborted"),
    }

    info!("goodbye");
    Ok(())
}

fn init_tracing() {
    // RUST_LOG controls log level (ex. RUST_LOG=debug)
    // otherwise, default to "info"
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info")
            .add_directive("matrix_sdk=error".parse().unwrap())
            .add_directive("matrix_sdk_crypto=error".parse().unwrap())
            .add_directive("matrix_sdk_base=error".parse().unwrap())
    });
    fmt().with_env_filter(filter).init();
}
