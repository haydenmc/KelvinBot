use std::collections::HashMap;

use serde::Deserialize;

pub const ENV_PREFIX: &str = "KELVIN";
pub const ENV_SEPARATOR: &str = "__";

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceKind {
    Dummy,
    Matrix,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub services: HashMap<String, ServiceCfg>, // key = service name
}

#[derive(Debug, Deserialize)]
pub struct ServiceCfg {
    pub kind: ServiceKind,
    pub interval_ms: Option<u64>,
}

pub fn load_from_env() -> anyhow::Result<Config> {
    dotenvy::dotenv().ok(); // Load from .env file first
    let cfg = config::Config::builder()
        .add_source(config::Environment::with_prefix(ENV_PREFIX).separator(ENV_SEPARATOR))
        .build()?;
    Ok(cfg.try_deserialize()?)
}
