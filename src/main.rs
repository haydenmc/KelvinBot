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
    let reconnect_config = cfg.reconnection.clone();
    let bus_task = tokio::spawn({
        async move {
            bus::Bus::new(evt_rx, cmd_rx, services, service_middlewares, reconnect_config)
                .run(bus_cancel)
                .await
        }
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
    // Set a default log level for all crates (warn), then allow RUST_LOG to override
    // This prevents debug noise from dependencies when setting RUST_LOG=kelvin_bot=debug
    //
    // Examples:
    //   RUST_LOG=kelvin_bot=debug          - Only kelvin_bot at debug, everything else at warn
    //   RUST_LOG=debug                      - Everything at debug
    //   RUST_LOG=kelvin_bot=debug,hyper=info - kelvin_bot at debug, hyper at info, rest at warn
    let filter = EnvFilter::builder()
        .with_default_directive(tracing::Level::WARN.into())
        .from_env_lossy()
        .add_directive("matrix_sdk=error".parse().unwrap())
        .add_directive("matrix_sdk_crypto=error".parse().unwrap())
        .add_directive("matrix_sdk_base=error".parse().unwrap());

    fmt().with_env_filter(filter).init();
}
