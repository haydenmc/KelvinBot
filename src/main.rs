use std::future::Future;

use anyhow::{Context, Result, anyhow};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use kelvin_bot::core::{bus, config::load_from_env, middleware, service};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    init_tracing();

    info!("starting...");

    info!("loading configuration...");
    let cfg = load_from_env()?;

    // Command channel: many producers (middleware) -> one consumer (bus)
    let (cmd_tx, cmd_rx) = bus::create_command_channel(1024);
    // Event channel: many producers (services) -> one consumer (bus)
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

    // Register signal handlers before waiting so a failure surfaces at startup.
    let shutdown = shutdown_signal()?;

    if let Err(e) = run_until_shutdown(bus_task, shutdown, cancel_all).await {
        error!(error = format!("{e:#}"), "exiting with failure");
        return Err(e);
    }

    info!("goodbye");
    Ok(())
}

/// Waits for either a shutdown signal or the bus task ending.
///
/// On a signal, cancels `cancel`, waits for the bus to finish and returns
/// `Ok`. If the bus ends first, for any reason, nothing is left routing
/// events, so this returns `Err` and the process exits non-zero for the
/// container runtime to restart it.
async fn run_until_shutdown(
    mut bus_task: JoinHandle<Result<()>>,
    shutdown: impl Future<Output = &'static str>,
    cancel: CancellationToken,
) -> Result<()> {
    tokio::select! {
        result = &mut bus_task => {
            return Err(match result {
                Ok(Ok(())) => anyhow!("bus stopped unexpectedly"),
                Ok(Err(e)) => e.context("bus stopped with an error"),
                Err(e) => anyhow::Error::new(e).context("bus task panicked or was aborted"),
            });
        }
        signal = shutdown => info!(signal, "shutdown signal received; shutting down…"),
    }

    cancel.cancel();

    match bus_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!(?e, "bus error during shutdown"),
        Err(e) => warn!(?e, "bus task panicked/aborted during shutdown"),
    }
    Ok(())
}

/// Returns a future that resolves with the name of the first shutdown signal
/// received: SIGINT (Ctrl+C) or SIGTERM (`docker stop`, systemd, Kubernetes).
#[cfg(unix)]
fn shutdown_signal() -> Result<impl Future<Output = &'static str>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;
    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;

    Ok(async move {
        tokio::select! {
            _ = sigint.recv() => "SIGINT",
            _ = sigterm.recv() => "SIGTERM",
        }
    })
}

/// Returns a future that resolves when Ctrl+C is received.
#[cfg(not(unix))]
fn shutdown_signal() -> Result<impl Future<Output = &'static str>> {
    Ok(async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            warn!(?e, "cannot listen for Ctrl+C; shutdown signals are unavailable");
            std::future::pending::<()>().await;
        }
        "Ctrl+C"
    })
}

fn init_tracing() {
    let filter =
        EnvFilter::builder().with_default_directive(tracing::Level::WARN.into()).from_env_lossy();

    fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::time::Duration;

    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn bus_returning_ok_before_a_signal_is_a_failure() {
        let bus_task = tokio::spawn(async { Ok(()) });

        let result = tokio::time::timeout(
            TEST_TIMEOUT,
            run_until_shutdown(bus_task, pending(), CancellationToken::new()),
        )
        .await
        .expect("should return once the bus ends");

        let err = result.expect_err("a bus that stops on its own is a failure");
        assert!(format!("{err:#}").contains("stopped unexpectedly"), "got: {err:#}");
    }

    #[tokio::test]
    async fn bus_error_before_a_signal_is_reported() {
        let bus_task = tokio::spawn(async { Err(anyhow!("middleware exploded")) });

        let result = tokio::time::timeout(
            TEST_TIMEOUT,
            run_until_shutdown(bus_task, pending(), CancellationToken::new()),
        )
        .await
        .expect("should return once the bus ends");

        let err = result.expect_err("a bus error is a failure");
        let message = format!("{err:#}");
        assert!(message.contains("bus stopped with an error"), "got: {message}");
        assert!(message.contains("middleware exploded"), "got: {message}");
    }

    #[tokio::test]
    async fn bus_panic_before_a_signal_is_a_failure() {
        let bus_task: JoinHandle<Result<()>> = tokio::spawn(async { panic!("bus panic (test)") });

        let result = tokio::time::timeout(
            TEST_TIMEOUT,
            run_until_shutdown(bus_task, pending(), CancellationToken::new()),
        )
        .await
        .expect("should return once the bus ends");

        let err = result.expect_err("a bus panic is a failure");
        assert!(format!("{err:#}").contains("panicked"), "got: {err:#}");
    }

    #[tokio::test]
    async fn signal_cancels_the_bus_and_exits_cleanly() {
        let cancel = CancellationToken::new();
        let bus_cancel = cancel.child_token();
        let bus_task = tokio::spawn(async move {
            bus_cancel.cancelled().await;
            Ok(())
        });

        let result = tokio::time::timeout(
            TEST_TIMEOUT,
            run_until_shutdown(bus_task, async { "SIGTERM" }, cancel.clone()),
        )
        .await
        .expect("should return once the bus has shut down");

        assert!(result.is_ok(), "a signalled shutdown is a clean exit: {result:?}");
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn bus_error_during_shutdown_is_still_a_clean_exit() {
        let cancel = CancellationToken::new();
        let bus_cancel = cancel.child_token();
        let bus_task = tokio::spawn(async move {
            bus_cancel.cancelled().await;
            Err(anyhow!("failed while stopping"))
        });

        let result = tokio::time::timeout(
            TEST_TIMEOUT,
            run_until_shutdown(bus_task, async { "SIGINT" }, cancel),
        )
        .await
        .expect("should return once the bus has shut down");

        assert!(result.is_ok(), "got: {result:?}");
    }
}
