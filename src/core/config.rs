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
    #[serde(default)]
    pub reconnection: ReconnectionConfig,
}

fn default_data_directory() -> PathBuf {
    PathBuf::from("./data")
}

// Reconnection configuration with exponential backoff
#[derive(Debug, Clone, Deserialize)]
pub struct ReconnectionConfig {
    #[serde(default = "default_initial_delay", with = "humantime_serde")]
    pub initial_delay: Duration,
    #[serde(default = "default_max_delay", with = "humantime_serde")]
    pub max_delay: Duration,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    #[serde(default = "default_jitter_factor")]
    pub jitter_factor: f64,
}

impl Default for ReconnectionConfig {
    fn default() -> Self {
        Self {
            initial_delay: default_initial_delay(),
            max_delay: default_max_delay(),
            multiplier: default_multiplier(),
            jitter_factor: default_jitter_factor(),
        }
    }
}

fn default_initial_delay() -> Duration {
    Duration::from_secs(1)
}

fn default_max_delay() -> Duration {
    Duration::from_secs(60)
}

fn default_multiplier() -> f64 {
    2.0
}

fn default_jitter_factor() -> f64 {
    0.1
}

// Helper for calculating exponential backoff delays
pub struct ExponentialBackoff {
    config: ReconnectionConfig,
    attempt: u32,
}

impl ExponentialBackoff {
    pub fn new(config: ReconnectionConfig) -> Self {
        Self { config, attempt: 0 }
    }

    pub fn next_delay(&mut self) -> Duration {
        let base_delay_secs = self.config.initial_delay.as_secs_f64()
            * self.config.multiplier.powi(self.attempt as i32);
        let capped_delay_secs = base_delay_secs.min(self.config.max_delay.as_secs_f64());

        // Apply jitter
        let jitter = {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            1.0 + rng.gen_range(-self.config.jitter_factor..=self.config.jitter_factor)
        };
        let final_delay_secs = capped_delay_secs * jitter;

        self.attempt += 1;
        Duration::from_secs_f64(final_delay_secs.max(0.0))
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
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
