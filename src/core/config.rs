use std::{collections::HashMap, path::PathBuf, time::Duration};

use secrecy::SecretString;
use serde::Deserialize;
use serde_with::{DisplayFromStr, serde_as};
use url::Url;

use crate::middlewares::movie_showtimes::LatLng;

pub const ENV_PREFIX: &str = "KELVIN";
pub const ENV_SEPARATOR: &str = "__";

#[derive(Debug, Clone, Deserialize)]
pub struct AnnouncementDestination {
    pub service_id: String,
    pub room_id: String,
}

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

#[derive(Debug, Clone, Deserialize)]
pub struct HouseholdCfg {
    pub name: String,
    /// Comma-separated list of member user IDs.
    /// Stored as a string for env-var config compatibility.
    pub members: String,
}

#[serde_as]
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MiddlewareKind {
    Echo {
        command_string: String,
    },
    Kanidm {
        /// DM command that generates a credential reset link for the caller's
        /// own kanidm account (default `!reset`).
        #[serde(default = "default_reset_command")]
        command_reset: String,
        /// DM command that creates a new kanidm person account and returns a
        /// credential reset link (default `!invite`).
        #[serde(default = "default_invite_command")]
        command_invite: String,
        /// Base URL / origin of the kanidm server, e.g. `https://idm.example.com`.
        /// Also used to build the `/ui/reset?token=...` link.
        kanidm_url: String,
        /// Service-account API token used as `Authorization: Bearer` against the
        /// kanidm REST API. The account must have people write/onboarding rights.
        kanidm_token: SecretString,
        /// Base URL of the Matrix Authentication Service, used to resolve a
        /// Matrix user to their linked kanidm account for `!reset`.
        mas_url: String,
        /// MAS OAuth2 client id (client_credentials grant, `urn:mas:admin` scope).
        mas_client_id: String,
        /// MAS OAuth2 client secret.
        mas_client_secret: SecretString,
        /// The MAS upstream OAuth provider id corresponding to kanidm. Used to
        /// select the correct upstream link when resolving `!reset`.
        mas_provider_id: String,
        /// Validity window for `!reset` links (kanidm clamps to 5m..=24h).
        #[serde(default = "default_reset_ttl", with = "humantime_serde")]
        reset_token_ttl: Duration,
        /// Validity window for `!invite` links, which are forwarded out-of-band
        /// to a new person (kanidm clamps to 5m..=24h).
        #[serde(default = "default_invite_ttl", with = "humantime_serde")]
        invite_token_ttl: Duration,
    },
    Logger {},
    CalendarAgenda {
        service_id: String,
        room_id: String,
        /// `webcal://` or `https://` ICS feed URL. Private to the bot: it is
        /// never rendered into a message.
        calendar_url: String,
        /// Optional human-facing calendar link rendered as a footer on the
        /// agenda. Distinct from `calendar_url`; omit for no footer.
        #[serde(default)]
        calendar_link: Option<String>,
        /// Link text for `calendar_link`.
        #[serde(default = "default_calendar_link_text")]
        calendar_link_text: String,
        /// Local time of day to post the daily agenda, 24h `HH:MM`.
        post_at_time: String,
        /// Comma-separated days-before intervals for multi-day event
        /// countdowns (e.g. `90,60,30,14`).
        #[serde(default, deserialize_with = "deserialize_string_list")]
        countdown_days: Option<Vec<String>>,
        /// Comma-separated days-before intervals for single-day, non-recurring
        /// event reminders (e.g. `7,1`).
        #[serde(default, deserialize_with = "deserialize_string_list")]
        reminder_days: Option<Vec<String>>,
        /// Minimum span in days for an event to count as "multi-day".
        #[serde(default = "default_multi_day_min_days")]
        #[serde_as(as = "DisplayFromStr")]
        multi_day_min_days: u32,
        #[serde(default = "default_heading_today")]
        heading_today: String,
        #[serde(default = "default_heading_reminders")]
        heading_reminders: String,
        #[serde(default = "default_heading_countdowns")]
        heading_countdowns: String,
        /// Optional chat command for an on-demand agenda (e.g. `!agenda`).
        /// Disabled when unset.
        #[serde(default)]
        command_string: Option<String>,
    },
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
        #[serde(default)]
        command_string: Option<String>,
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
        #[serde(default = "default_thumbnail_max_width")]
        thumbnail_max_width: u32,
        #[serde(default = "default_thumbnail_max_height")]
        thumbnail_max_height: u32,
        #[serde(default = "default_thumbnail_jpeg_quality")]
        thumbnail_jpeg_quality: u8,
    },
    EzStreamAnnounce {
        websocket_url: String,
        stream_url_template: String,
        start_message_template: String,
        end_message_template: String,
        #[serde(default)]
        destinations: HashMap<String, AnnouncementDestination>,
    },
    WeeklyGathering {
        service_id: String,
        room_id: String,
        event_day_of_week: String,
        /// Comma-separated list of candidate start times in 24h `HH:MM` form
        /// (e.g. `16:30,20:00`). Stored as a string for env-var config
        /// compatibility. A single time means a fixed start with no vote.
        event_time_options: String,
        /// When the poll closes, as 24h `HH:MM` on the event day itself.
        finalize_time: String,
        /// How long the poll stays open before `finalize_time`.
        #[serde_as(as = "DisplayFromStr")]
        poll_open_minutes: u32,
        reaction_virtual: String,
        reaction_in_person: String,
        reaction_host: String,
        announcement_message: String,
        finalization_virtual_message: String,
        finalization_in_person_message: String,
        finalization_no_votes_message: String,
        /// Call to action rendered into `{time_prompt}` while the host still
        /// has an event time to pick.
        #[serde(default = "default_time_prompt_message")]
        time_prompt_message: String,
        #[serde(default)]
        households: HashMap<String, HouseholdCfg>,
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

fn default_reset_command() -> String {
    "!reset".to_string()
}

fn default_invite_command() -> String {
    "!invite".to_string()
}

fn default_calendar_link_text() -> String {
    "View the full calendar".to_string()
}

fn default_multi_day_min_days() -> u32 {
    2
}

fn default_heading_today() -> String {
    "Today".to_string()
}

fn default_heading_reminders() -> String {
    "Coming up".to_string()
}

fn default_heading_countdowns() -> String {
    "Countdowns".to_string()
}

fn default_time_prompt_message() -> String {
    "React with the time that works best to lock it in.".to_string()
}

fn default_reset_ttl() -> Duration {
    Duration::from_secs(10 * 60) // 10 minutes
}

fn default_invite_ttl() -> Duration {
    Duration::from_secs(24 * 60 * 60) // 24 hours
}

fn default_thumbnail_max_width() -> u32 {
    480
}

fn default_thumbnail_max_height() -> u32 {
    360
}

fn default_thumbnail_jpeg_quality() -> u8 {
    75
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
