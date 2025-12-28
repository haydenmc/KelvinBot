use std::{collections::HashMap, path::PathBuf, time::Duration};

use secrecy::SecretString;
use serde::Deserialize;
use serde_with::{DisplayFromStr, serde_as};
use url::Url;

use crate::middlewares::movie_showtimes::LatLng;

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
    Mumble {
        hostname: String,
        #[serde_as(as = "DisplayFromStr")]
        port: u16,
        username: String,
        password: SecretString,
        #[serde(default)]
        #[serde_as(as = "Option<DisplayFromStr>")]
        accept_invalid_certs: Option<bool>,
    },
    #[serde(other)]
    Unknown,
}

#[serde_as]
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MiddlewareKind {
    Echo {
        command_string: String,
    },
    Invite {
        command_string: String,
        uses_allowed: Option<u32>,
        #[serde(default, with = "humantime_serde")]
        expiry: Option<Duration>,
    },
    Logger {},
    MovieShowtimes {
        service_id: String,
        room_id: String,
        post_on_day_of_week: String, // e.g., "Monday", "Tuesday", etc.
        post_at_time: String,        // e.g., "18:00", "09:30"
        search_location: LatLng,
        #[serde_as(as = "DisplayFromStr")]
        search_radius_mi: u16,
        gracenote_api_key: String,
        #[serde(default, deserialize_with = "deserialize_string_list")]
        theater_id_filter: Option<Vec<String>>,
    },
    AttendanceRelay {
        source_service_id: String,
        source_room_id: Option<String>,
        dest_service_id: String,
        dest_room_id: String,
        session_start_message: String,
        session_end_message: String,
        session_ended_edit_message: String,
    },
    ChatRelay {
        source_service_id: String,
        source_room_id: Option<String>,
        dest_service_id: String,
        dest_room_id: String,
        prefix_tag: String,
    },
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
    #[serde(default, deserialize_with = "deserialize_middleware_list")]
    pub middleware: Option<Vec<String>>, // List of middleware names
}

fn deserialize_middleware_list<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_string_list(deserializer)
}

fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        String(String),
        Vec(Vec<String>),
    }

    let value: Option<StringOrVec> = Option::deserialize(deserializer)?;

    match value {
        None => Ok(None),
        Some(StringOrVec::Vec(vec)) => Ok(Some(vec)),
        Some(StringOrVec::String(s)) => {
            // Parse comma-separated string into Vec
            let items: Vec<String> = s
                .split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect();
            Ok(Some(items))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct MiddlewareCfg {
    #[serde(flatten)]
    pub kind: MiddlewareKind,
}

pub fn load_from_env() -> anyhow::Result<Config> {
    dotenvy::dotenv().ok(); // Load from .env file first
    let cfg = config::Config::builder()
        .add_source(config::Environment::with_prefix(ENV_PREFIX).separator(ENV_SEPARATOR))
        .build()?;
    Ok(cfg.try_deserialize()?)
}
