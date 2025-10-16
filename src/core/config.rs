use std::{collections::HashMap, path::PathBuf};

use secrecy::SecretString;
use serde::Deserialize;
use serde_with::{DisplayFromStr, serde_as};
use url::Url;

pub const ENV_PREFIX: &str = "KELVIN";
pub const ENV_SEPARATOR: &str = "__";

#[serde_as]
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ServiceKind {
    Dummy {
        #[serde_as(as = "Option<DisplayFromStr>")]
        interval_ms: Option<u64>,
    },
    Matrix {
        homeserver_url: Url,
        user_id: String,
        password: SecretString,
        device_id: String,
        db_passphrase: SecretString,
        verification_device_id: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MiddlewareKind {
    Echo { command_string: String },
    Logger {},
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub services: HashMap<String, ServiceCfg>, // key = service name
    #[serde(default)]
    pub middlewares: HashMap<String, MiddlewareCfg>, // key = middleware name
    #[serde(default = "default_data_directory")]
    pub data_directory: PathBuf,
}

fn default_data_directory() -> PathBuf {
    PathBuf::from("./data")
}

#[derive(Debug, Deserialize)]
pub struct ServiceCfg {
    #[serde(flatten)]
    pub kind: ServiceKind,
    #[serde(default)]
    pub middleware: Option<Vec<String>>, // List of middleware names
}

#[derive(Debug, Deserialize)]
pub struct MiddlewareCfg {
    #[serde(flatten)]
    pub kind: MiddlewareKind,
}

pub fn load_from_env() -> anyhow::Result<Config> {
    dotenvy::dotenv().ok(); // Load from .env file first
    let cfg = config::Config::builder()
        .add_source(
            config::Environment::with_prefix(ENV_PREFIX)
                .separator(ENV_SEPARATOR)
                .list_separator(",")
                .with_list_parse_key("services.*.middleware"),
        )
        .build()?;
    Ok(cfg.try_deserialize()?)
}
