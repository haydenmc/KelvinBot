use anyhow::Result;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    init_tracing();

    info!("starting...");

    tokio::select! {
        _ = run() => {},
        _ = tokio::signal::ctrl_c() => {
            info!("received Ctrl+C, shutting down...")
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

async fn run() {
    loop {
        info!("tick");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}