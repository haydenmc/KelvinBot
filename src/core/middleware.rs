use std::{collections::HashMap, sync::Arc};

use crate::core::bus::Command;
use crate::core::config::{Config, MiddlewareKind};
use crate::core::event::Event;
use crate::store::PersistentStore;
use crate::middlewares::{
    attendance_relay::{AttendanceRelay, AttendanceRelayConfig},
    chat_relay::{ChatRelay, ChatRelayConfig},
    echo::Echo,
    ezstream_announce::EzStreamAnnounce,
    invite::Invite,
    logger::Logger,
    movie_showtimes::MovieShowtimes,
    weekly_gathering::{WeeklyGathering, WeeklyGatheringConfig},
};
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
            MiddlewareKind::Invite { command_string, uses_allowed, expiry } => Arc::new(
                Invite::new(make_ctx()?, command_string.clone(), *uses_allowed, *expiry),
            ),
            MiddlewareKind::Logger {} => Arc::new(Logger {}),
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
            } => Arc::new(ChatRelay::new(
                make_ctx()?,
                ChatRelayConfig {
                    source_service_id: source_service_id.clone(),
                    source_room_id: source_room_id.clone(),
                    dest_service_id: dest_service_id.clone(),
                    dest_room_id: dest_room_id.clone(),
                    prefix_tag: prefix_tag.clone(),
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
                event_time,
                announce_minutes_before,
                finalize_minutes_before,
                reaction_virtual,
                reaction_in_person,
                reaction_host,
                announcement_message,
                finalization_virtual_message,
                finalization_in_person_message,
                finalization_no_votes_message,
            } => {
                // Parse day_of_week string to Weekday
                let weekday = event_day_of_week.parse::<chrono::Weekday>()
                    .map_err(|_| anyhow::anyhow!(
                        "invalid event_day_of_week '{}' for middleware '{}'. Valid values: Monday, Tuesday, Wednesday, Thursday, Friday, Saturday, Sunday",
                        event_day_of_week, name
                    ))?;

                // Parse time string (HH:MM format)
                let naive_time = chrono::NaiveTime::parse_from_str(event_time, "%H:%M")
                    .map_err(|_| anyhow::anyhow!(
                        "invalid event_time format '{}' for middleware '{}'. Expected format: HH:MM (e.g., 19:00)",
                        event_time, name
                    ))?;

                Arc::new(WeeklyGathering::new(
                    make_ctx()?,
                    WeeklyGatheringConfig {
                        service_id: service_id.clone(),
                        room_id: room_id.clone(),
                        event_day_of_week: weekday,
                        event_time: naive_time,
                        announce_minutes_before: *announce_minutes_before,
                        finalize_minutes_before: *finalize_minutes_before,
                        reaction_virtual: reaction_virtual.clone(),
                        reaction_in_person: reaction_in_person.clone(),
                        reaction_host: reaction_host.clone(),
                        announcement_message: announcement_message.clone(),
                        finalization_virtual_message: finalization_virtual_message.clone(),
                        finalization_in_person_message: finalization_in_person_message.clone(),
                        finalization_no_votes_message: finalization_no_votes_message.clone(),
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
