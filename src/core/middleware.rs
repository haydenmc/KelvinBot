use std::{collections::HashMap, sync::Arc};

use crate::core::bus::Command;
use crate::core::config::{Config, HouseholdCfg, MiddlewareKind};
use crate::core::event::Event;
use crate::middlewares::{
    attendance_relay::{AttendanceRelay, AttendanceRelayConfig},
    calendar_agenda::{CalendarAgenda, CalendarAgendaConfig},
    chat_relay::{ChatRelay, ChatRelayConfig},
    echo::Echo,
    ezstream_announce::EzStreamAnnounce,
    kanidm::{KanidmConfig, KanidmIdentity},
    logger::Logger,
    movie_showtimes::MovieShowtimes,
    weekly_gathering::{Household, WeeklyGathering, WeeklyGatheringConfig},
};
use crate::store::PersistentStore;
use anyhow::{Result, bail};
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::warn;

#[derive(Debug, Clone, Copy)]
pub enum Verdict {
    Continue,
    #[allow(dead_code)]
    Stop, // This will be used eventually.
}

/// Per-middleware context passed to every middleware constructor.
///
/// Bundles the shared command sender and a dedicated persistent store so that
/// any middleware can opt into storage simply by using `ctx.store` — no
/// changes to `instantiate_middleware_from_config` required.
#[derive(Clone)]
pub struct MiddlewareContext {
    pub cmd_tx: Sender<Command>,
    pub store: Arc<PersistentStore>,
}

#[async_trait]
pub trait Middleware: Send + Sync {
    async fn run(&self, cancel: CancellationToken) -> Result<()>;
    fn on_event(&self, event: &Event) -> Result<Verdict>;
}

/// Countdown intervals used when `COUNTDOWN_DAYS` is not configured.
const DEFAULT_COUNTDOWN_DAYS: &[u32] = &[90, 60, 30, 14, 7];

/// Reminder intervals used when `REMINDER_DAYS` is not configured.
const DEFAULT_REMINDER_DAYS: &[u32] = &[7, 1];

/// Parse a configured list of days-before intervals, falling back to `default`
/// when unset. Result is sorted descending and deduplicated.
fn parse_day_intervals(
    configured: Option<&[String]>,
    default: &[u32],
    field: &str,
    middleware_name: &str,
) -> Result<Vec<u32>> {
    let Some(values) = configured.filter(|v| !v.is_empty()) else {
        return Ok(default.to_vec());
    };

    let mut days = values
        .iter()
        .map(|value| {
            value.trim().parse::<u32>().map_err(|_| {
                anyhow::anyhow!(
                    "invalid {field} entry '{value}' for middleware '{middleware_name}'. \
                     Expected a comma-separated list of whole days (e.g. 90,60,30)"
                )
            })
        })
        .collect::<Result<Vec<u32>>>()?;

    days.sort_unstable_by(|a, b| b.cmp(a));
    days.dedup();
    Ok(days)
}

/// Instantiates middleware instances from config as a HashMap keyed by middleware name
pub fn instantiate_middleware_from_config(
    config: &Config,
    cmd_tx: &Sender<Command>,
) -> Result<HashMap<String, Arc<dyn Middleware>>> {
    let mut middlewares = HashMap::new();

    for (name, cfg) in &config.middlewares {
        // Lazily build a MiddlewareContext for this middleware. Calling make_ctx()
        // opens (or creates) the middleware's dedicated store file on disk. Only
        // middlewares that actually need the context call this.
        let make_ctx = || -> Result<MiddlewareContext> {
            let store_path = config.data_directory.join(format!("{name}.store.json"));
            let store = Arc::new(PersistentStore::load(store_path)?);
            Ok(MiddlewareContext { cmd_tx: cmd_tx.clone(), store })
        };

        let middleware: Arc<dyn Middleware> = match &cfg.kind {
            MiddlewareKind::Echo { command_string } => {
                Arc::new(Echo::new(make_ctx()?, command_string.clone()))
            }
            MiddlewareKind::Kanidm {
                command_reset,
                command_invite,
                kanidm_url,
                kanidm_token,
                mas_url,
                mas_client_id,
                mas_client_secret,
                mas_provider_id,
                reset_token_ttl,
                invite_token_ttl,
            } => Arc::new(KanidmIdentity::new(
                make_ctx()?,
                command_reset.clone(),
                command_invite.clone(),
                KanidmConfig {
                    kanidm_url: kanidm_url.clone(),
                    kanidm_token: kanidm_token.clone(),
                    mas_url: mas_url.clone(),
                    mas_client_id: mas_client_id.clone(),
                    mas_client_secret: mas_client_secret.clone(),
                    mas_provider_id: mas_provider_id.clone(),
                    reset_token_ttl: *reset_token_ttl,
                    invite_token_ttl: *invite_token_ttl,
                },
            )),
            MiddlewareKind::Logger {} => Arc::new(Logger {}),
            MiddlewareKind::CalendarAgenda {
                service_id,
                room_id,
                calendar_url,
                calendar_link,
                calendar_link_text,
                post_at_time,
                countdown_days,
                reminder_days,
                multi_day_min_days,
                heading_today,
                heading_reminders,
                heading_countdowns,
                command_string,
            } => {
                let post_at_time = chrono::NaiveTime::parse_from_str(post_at_time, "%H:%M")
                    .map_err(|_| {
                        anyhow::anyhow!(
                            "invalid post_at_time '{}' for middleware '{}'. Expected format: HH:MM (e.g., 08:00)",
                            post_at_time,
                            name
                        )
                    })?;

                Arc::new(CalendarAgenda::new(
                    make_ctx()?,
                    CalendarAgendaConfig {
                        service_id: service_id.clone(),
                        room_id: room_id.clone(),
                        calendar_url: calendar_url.clone(),
                        calendar_link: calendar_link.clone(),
                        calendar_link_text: calendar_link_text.clone(),
                        post_at_time,
                        countdown_days: parse_day_intervals(
                            countdown_days.as_deref(),
                            DEFAULT_COUNTDOWN_DAYS,
                            "countdown_days",
                            name,
                        )?,
                        reminder_days: parse_day_intervals(
                            reminder_days.as_deref(),
                            DEFAULT_REMINDER_DAYS,
                            "reminder_days",
                            name,
                        )?,
                        multi_day_min_days: *multi_day_min_days,
                        heading_today: heading_today.clone(),
                        heading_reminders: heading_reminders.clone(),
                        heading_countdowns: heading_countdowns.clone(),
                        command_string: command_string.clone(),
                    },
                ))
            }
            MiddlewareKind::MovieShowtimes {
                service_id,
                room_id,
                post_on_day_of_week,
                post_at_time,
                search_location,
                search_radius_mi,
                gracenote_api_key,
                theater_id_filter,
                command_string,
            } => {
                // Parse day_of_week string to Weekday
                let weekday = post_on_day_of_week.parse::<chrono::Weekday>()
                    .map_err(|_| anyhow::anyhow!(
                        "invalid day_of_week '{}' for middleware '{}'. Valid values: Monday, Tuesday, Wednesday, Thursday, Friday, Saturday, Sunday",
                        post_on_day_of_week, name
                    ))?;

                // Parse time string (HH:MM format)
                let naive_time = chrono::NaiveTime::parse_from_str(post_at_time, "%H:%M")
                    .map_err(|_| anyhow::anyhow!(
                        "invalid time format '{}' for middleware '{}'. Expected format: HH:MM (e.g., 18:00)",
                        post_at_time, name
                    ))?;

                Arc::new(MovieShowtimes::new(
                    make_ctx()?,
                    service_id.clone(),
                    room_id.clone(),
                    weekday,
                    naive_time,
                    *search_location,
                    *search_radius_mi,
                    gracenote_api_key.clone(),
                    theater_id_filter.clone(),
                    command_string.clone(),
                ))
            }
            MiddlewareKind::AttendanceRelay {
                source_service_id,
                source_room_id,
                dest_service_id,
                dest_room_id,
                session_start_message,
                session_end_message,
                session_ended_edit_message,
            } => Arc::new(AttendanceRelay::new(
                make_ctx()?,
                AttendanceRelayConfig {
                    source_service_id: source_service_id.clone(),
                    source_room_id: source_room_id.clone(),
                    dest_service_id: dest_service_id.clone(),
                    dest_room_id: dest_room_id.clone(),
                    session_start_message: session_start_message.clone(),
                    session_end_message: session_end_message.clone(),
                    session_ended_edit_message: session_ended_edit_message.clone(),
                },
            )),
            MiddlewareKind::ChatRelay {
                source_service_id,
                source_room_id,
                dest_service_id,
                dest_room_id,
                prefix_tag,
                thumbnail_max_width,
                thumbnail_max_height,
                thumbnail_jpeg_quality,
            } => Arc::new(ChatRelay::new(
                make_ctx()?,
                ChatRelayConfig {
                    source_service_id: source_service_id.clone(),
                    source_room_id: source_room_id.clone(),
                    dest_service_id: dest_service_id.clone(),
                    dest_room_id: dest_room_id.clone(),
                    prefix_tag: prefix_tag.clone(),
                    thumbnail_max_width: *thumbnail_max_width,
                    thumbnail_max_height: *thumbnail_max_height,
                    thumbnail_jpeg_quality: *thumbnail_jpeg_quality,
                },
            )),
            MiddlewareKind::EzStreamAnnounce {
                websocket_url,
                stream_url_template,
                start_message_template,
                end_message_template,
                destinations,
            } => {
                use crate::middlewares::ezstream_announce::DestinationConfig;

                let dest_configs: Vec<DestinationConfig> = destinations
                    .values()
                    .map(|d| DestinationConfig {
                        service_id: d.service_id.clone(),
                        room_id: d.room_id.clone(),
                    })
                    .collect();

                Arc::new(EzStreamAnnounce::new(
                    make_ctx()?,
                    websocket_url.clone(),
                    stream_url_template.clone(),
                    start_message_template.clone(),
                    end_message_template.clone(),
                    dest_configs,
                ))
            }
            MiddlewareKind::WeeklyGathering {
                service_id,
                room_id,
                event_day_of_week,
                event_time_options,
                finalize_time,
                poll_open_minutes,
                reaction_virtual,
                reaction_in_person,
                reaction_host,
                announcement_message,
                finalization_virtual_message,
                finalization_in_person_message,
                finalization_no_votes_message,
                time_prompt_message,
                households,
            } => {
                // Parse day_of_week string to Weekday
                let weekday = event_day_of_week.parse::<chrono::Weekday>()
                    .map_err(|_| anyhow::anyhow!(
                        "invalid event_day_of_week '{}' for middleware '{}'. Valid values: Monday, Tuesday, Wednesday, Thursday, Friday, Saturday, Sunday",
                        event_day_of_week, name
                    ))?;

                // Parse the poll close time (HH:MM format), which lands on the event day
                let finalize_naive_time = chrono::NaiveTime::parse_from_str(finalize_time, "%H:%M")
                    .map_err(|_| anyhow::anyhow!(
                        "invalid finalize_time format '{}' for middleware '{}'. Expected format: HH:MM (e.g., 14:00)",
                        finalize_time, name
                    ))?;

                let time_options =
                    crate::middlewares::weekly_gathering::parse_event_times(event_time_options)
                        .map_err(|e| anyhow::anyhow!("{} for middleware '{}'", e, name))?;

                if time_options.is_empty() {
                    bail!(
                        "event_time_options is required for middleware '{}'. Provide at least one HH:MM start time (a single time means a fixed start, with no vote)",
                        name
                    );
                }

                let runtime_households: Vec<Household> = households
                    .values()
                    .map(|h: &HouseholdCfg| Household {
                        name: h.name.clone(),
                        members: h
                            .members
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect(),
                    })
                    .collect();

                Arc::new(WeeklyGathering::new(
                    make_ctx()?,
                    WeeklyGatheringConfig {
                        service_id: service_id.clone(),
                        room_id: room_id.clone(),
                        event_day_of_week: weekday,
                        event_time_options: time_options,
                        finalize_time: finalize_naive_time,
                        poll_open_minutes: *poll_open_minutes,
                        reaction_virtual: reaction_virtual.clone(),
                        reaction_in_person: reaction_in_person.clone(),
                        reaction_host: reaction_host.clone(),
                        announcement_message: announcement_message.clone(),
                        finalization_virtual_message: finalization_virtual_message.clone(),
                        finalization_in_person_message: finalization_in_person_message.clone(),
                        finalization_no_votes_message: finalization_no_votes_message.clone(),
                        time_prompt_message: time_prompt_message.clone(),
                        households: runtime_households,
                    },
                ))
            }
            MiddlewareKind::Unknown => {
                warn!(middleware_name=%name, "unknown middleware kind, skipping");
                continue;
            }
        };
        middlewares.insert(name.clone(), middleware);
    }

    Ok(middlewares)
}

/// Builds a Vec of middleware instances from a list of middleware names
pub fn build_middleware_pipeline(
    middleware_names: &[String],
    all_middlewares: &HashMap<String, Arc<dyn Middleware>>,
) -> Result<Vec<Arc<dyn Middleware>>> {
    let mut pipeline = Vec::new();

    for name in middleware_names {
        match all_middlewares.get(name) {
            Some(mw) => pipeline.push(mw.clone()),
            None => {
                bail!("middleware '{}' referenced but not defined in config", name);
            }
        }
    }

    Ok(pipeline)
}
