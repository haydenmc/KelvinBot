use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, Verdict},
    service::ServiceId,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, TimeZone, Weekday};
use rand::seq::SliceRandom;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};
use tokio_util::sync::CancellationToken;

pub struct WeeklyGatheringConfig {
    pub service_id: String,
    pub room_id: String,
    pub event_day_of_week: Weekday,
    pub event_time: NaiveTime,
    pub announce_minutes_before: u32,
    pub finalize_minutes_before: u32,
    pub reaction_virtual: String,
    pub reaction_in_person: String,
    pub reaction_host: String,
    pub announcement_message: String,
    pub finalization_virtual_message: String,
    pub finalization_in_person_message: String,
    pub finalization_no_votes_message: String,
    pub avoid_repeat_host: bool,
}

#[derive(Debug, Clone)]
enum GatheringPhase {
    Idle,
    Announced { message_id: String },
    Finalized,
}

struct GatheringState {
    phase: GatheringPhase,
    virtual_votes: HashSet<String>,
    in_person_votes: HashSet<String>,
    host_volunteers: HashSet<String>,
    last_host: Option<String>,
}

impl Default for GatheringState {
    fn default() -> Self {
        Self {
            phase: GatheringPhase::Idle,
            virtual_votes: HashSet::new(),
            in_person_votes: HashSet::new(),
            host_volunteers: HashSet::new(),
            last_host: None,
        }
    }
}

#[derive(Debug)]
enum ReactionEvent {
    Added { target_event_id: String, key: String, sender_id: String },
    Removed { target_event_id: Option<String>, key: Option<String>, sender_id: String },
}

pub struct WeeklyGathering {
    cmd_tx: Sender<Command>,
    config: WeeklyGatheringConfig,
    state: Arc<Mutex<GatheringState>>,
    reaction_tx: tokio::sync::mpsc::Sender<ReactionEvent>,
    reaction_rx: Arc<Mutex<tokio::sync::mpsc::Receiver<ReactionEvent>>>,
}

impl WeeklyGathering {
    pub fn new(cmd_tx: Sender<Command>, config: WeeklyGatheringConfig) -> Self {
        let (reaction_tx, reaction_rx) = tokio::sync::mpsc::channel(100);

        Self {
            cmd_tx,
            config,
            state: Arc::new(Mutex::new(GatheringState::default())),
            reaction_tx,
            reaction_rx: Arc::new(Mutex::new(reaction_rx)),
        }
    }

    /// Calculate the next occurrence of the event day/time
    fn next_event_time(&self) -> DateTime<Local> {
        let now = Local::now();
        let target_time = self.config.event_time;
        let target_weekday = self.config.event_day_of_week;

        let current_weekday = now.weekday();
        let current_num = current_weekday.number_from_monday();
        let target_num = target_weekday.number_from_monday();

        let days_until_target = if current_weekday == target_weekday {
            let now_time = now.time();
            if now_time < target_time { 0 } else { 7 }
        } else if target_num > current_num {
            target_num - current_num
        } else {
            7 - (current_num - target_num)
        };

        let target_date = now.date_naive() + Duration::days(days_until_target as i64);
        let target_datetime = target_date.and_time(target_time);

        Local.from_local_datetime(&target_datetime).unwrap()
    }

    /// Calculate when to post the announcement
    fn announcement_time(&self) -> DateTime<Local> {
        self.next_event_time() - Duration::minutes(self.config.announce_minutes_before as i64)
    }

    /// Format event time in a friendly way (e.g., "Today at 7:00pm", "Tomorrow at 7:00pm", "Saturday at 7:00pm")
    fn format_friendly_time(&self) -> String {
        let event_datetime = self.next_event_time();
        let now = Local::now();
        let today = now.date_naive();
        let event_date = event_datetime.date_naive();

        let day_part = if event_date == today {
            "Today".to_string()
        } else if event_date == today + Duration::days(1) {
            "Tomorrow".to_string()
        } else {
            self.config.event_day_of_week.to_string()
        };

        let time_part = self.config.event_time.format("%-I:%M%P").to_string();
        format!("{} at {}", day_part, time_part)
    }

    /// Calculate when to finalize and post results
    fn finalization_time(&self) -> DateTime<Local> {
        self.next_event_time() - Duration::minutes(self.config.finalize_minutes_before as i64)
    }

    /// Select a host from volunteers, optionally avoiding the last host
    pub fn select_host(
        volunteers: &HashSet<String>,
        last_host: Option<&String>,
        avoid_repeat: bool,
    ) -> Option<String> {
        if volunteers.is_empty() {
            return None;
        }

        let candidates: Vec<&String> = if avoid_repeat {
            let filtered: Vec<&String> =
                volunteers.iter().filter(|v| Some(*v) != last_host).collect();
            // Fall back to all volunteers if filtering leaves no candidates
            if filtered.is_empty() { volunteers.iter().collect() } else { filtered }
        } else {
            volunteers.iter().collect()
        };

        let mut rng = rand::thread_rng();
        candidates.choose(&mut rng).map(|s| (*s).clone())
    }

    /// Post the announcement message and capture the message ID
    async fn post_announcement(&self) -> Result<Option<String>> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        // Replace placeholders in announcement message
        let event_time_friendly = self.format_friendly_time();
        let message = self
            .config
            .announcement_message
            .replace("{reaction_virtual}", &self.config.reaction_virtual)
            .replace("{reaction_in_person}", &self.config.reaction_in_person)
            .replace("{reaction_host}", &self.config.reaction_host)
            .replace("{event_time}", &event_time_friendly);

        let command = Command::SendRoomMessage {
            service_id: ServiceId(self.config.service_id.clone()),
            room_id: self.config.room_id.clone(),
            body: message.clone(),
            markdown_body: Some(message),
            response_tx: Some(response_tx),
        };

        self.cmd_tx.send(command).await?;

        match response_rx.await {
            Ok(Ok(message_id)) => {
                tracing::info!(message_id=%message_id, "announcement posted successfully");

                // Pre-populate reactions on the announcement
                for reaction_key in [
                    &self.config.reaction_virtual,
                    &self.config.reaction_in_person,
                    &self.config.reaction_host,
                ] {
                    let command = Command::AddReaction {
                        service_id: ServiceId(self.config.service_id.clone()),
                        room_id: self.config.room_id.clone(),
                        event_id: message_id.clone(),
                        key: reaction_key.clone(),
                    };

                    if let Err(e) = self.cmd_tx.send(command).await {
                        tracing::error!(error=%e, key=%reaction_key, "failed to send add reaction command");
                    }
                }

                Ok(Some(message_id))
            }
            Ok(Err(e)) => {
                tracing::error!(error=%e, "failed to post announcement");
                Ok(None)
            }
            Err(e) => {
                tracing::error!(error=%e, "failed to receive announcement response");
                Ok(None)
            }
        }
    }

    /// Post the finalization message with vote counts and host
    pub async fn post_finalization(&self) {
        let state = self.state.lock().await;

        let virtual_count = state.virtual_votes.len();
        let in_person_count = state.in_person_votes.len();

        // Select host
        let host = Self::select_host(
            &state.host_volunteers,
            state.last_host.as_ref(),
            self.config.avoid_repeat_host,
        );

        let host_display = host.as_deref().unwrap_or("No host volunteered");

        // Choose message based on vote outcome
        let template = if virtual_count == 0 && in_person_count == 0 {
            // No votes
            &self.config.finalization_no_votes_message
        } else if in_person_count > virtual_count {
            &self.config.finalization_in_person_message
        } else {
            // Virtual wins, or tie (prefer virtual)
            &self.config.finalization_virtual_message
        };

        // Replace placeholders
        let event_time_friendly = self.format_friendly_time();
        let message = template
            .replace("{virtual_count}", &virtual_count.to_string())
            .replace("{in_person_count}", &in_person_count.to_string())
            .replace("{host}", host_display)
            .replace("{event_time}", &event_time_friendly);

        drop(state); // Release lock before sending

        let command = Command::SendRoomMessage {
            service_id: ServiceId(self.config.service_id.clone()),
            room_id: self.config.room_id.clone(),
            body: message.clone(),
            markdown_body: Some(message),
            response_tx: None,
        };

        if let Err(e) = self.cmd_tx.send(command).await {
            tracing::error!(error=%e, "failed to send finalization message");
        }

        // Update last_host if a host was selected
        if let Some(selected_host) = host {
            let mut state = self.state.lock().await;
            state.last_host = Some(selected_host);
        }

        tracing::info!(
            virtual_count=%virtual_count,
            in_person_count=%in_person_count,
            "finalization posted"
        );
    }

    /// Reset state for the next week
    async fn reset_for_next_week(&self) {
        let mut state = self.state.lock().await;
        state.phase = GatheringPhase::Idle;
        state.virtual_votes.clear();
        state.in_person_votes.clear();
        state.host_volunteers.clear();
        tracing::info!("state reset for next week");
    }

    /// Process a reaction event
    async fn process_reaction(&self, reaction: ReactionEvent) {
        let mut state = self.state.lock().await;

        // Only process reactions when in Announced phase
        let announcement_message_id = match &state.phase {
            GatheringPhase::Announced { message_id } => message_id.clone(),
            _ => return,
        };

        match reaction {
            ReactionEvent::Added { target_event_id, key, sender_id } => {
                // Check if reaction is on the announcement message
                if target_event_id != announcement_message_id {
                    return;
                }

                // Process based on reaction type
                if key == self.config.reaction_virtual {
                    state.virtual_votes.insert(sender_id.clone());
                    // Remove from in-person if switching
                    state.in_person_votes.remove(&sender_id);
                    tracing::debug!(sender_id=%sender_id, "virtual vote recorded");
                } else if key == self.config.reaction_in_person {
                    state.in_person_votes.insert(sender_id.clone());
                    // Remove from virtual if switching
                    state.virtual_votes.remove(&sender_id);
                    tracing::debug!(sender_id=%sender_id, "in-person vote recorded");
                } else if key == self.config.reaction_host {
                    state.host_volunteers.insert(sender_id.clone());
                    tracing::debug!(sender_id=%sender_id, "host volunteer recorded");
                }
            }
            ReactionEvent::Removed { target_event_id, key, sender_id } => {
                // Check if reaction was on the announcement message
                if let Some(target) = target_event_id
                    && target != announcement_message_id
                {
                    return;
                }

                // Remove vote based on key (if known)
                if let Some(key) = key {
                    if key == self.config.reaction_virtual {
                        state.virtual_votes.remove(&sender_id);
                        tracing::debug!(sender_id=%sender_id, "virtual vote removed");
                    } else if key == self.config.reaction_in_person {
                        state.in_person_votes.remove(&sender_id);
                        tracing::debug!(sender_id=%sender_id, "in-person vote removed");
                    } else if key == self.config.reaction_host {
                        state.host_volunteers.remove(&sender_id);
                        tracing::debug!(sender_id=%sender_id, "host volunteer removed");
                    }
                }
            }
        }
    }
}

// Test helpers - exposed for integration tests in tests/unit/middleware.rs
// TODO: Ideally, move the WeeklyGathering tests into this crate as a #[cfg(test)] mod tests
// block, which would allow these helpers to be conditionally compiled and hidden from the
// public API.
#[doc(hidden)]
impl WeeklyGathering {
    /// Set the phase to Announced with a specific message ID (for testing)
    pub async fn set_announced(&self, message_id: String) {
        let mut state = self.state.lock().await;
        state.phase = GatheringPhase::Announced { message_id };
    }

    /// Get current vote counts (for testing): (virtual, in_person, host_volunteers)
    pub async fn get_vote_counts(&self) -> (usize, usize, usize) {
        let state = self.state.lock().await;
        (state.virtual_votes.len(), state.in_person_votes.len(), state.host_volunteers.len())
    }

    /// Set the last host (for testing)
    pub async fn set_last_host(&self, host: Option<String>) {
        let mut state = self.state.lock().await;
        state.last_host = host;
    }

    /// Get host volunteers (for testing)
    pub async fn get_host_volunteers(&self) -> HashSet<String> {
        let state = self.state.lock().await;
        state.host_volunteers.clone()
    }

    /// Process a reaction directly (for testing)
    pub async fn test_process_reaction_added(
        &self,
        target_event_id: String,
        key: String,
        sender_id: String,
    ) {
        self.process_reaction(ReactionEvent::Added { target_event_id, key, sender_id }).await;
    }

    /// Process a reaction removal directly (for testing)
    pub async fn test_process_reaction_removed(
        &self,
        target_event_id: Option<String>,
        key: Option<String>,
        sender_id: String,
    ) {
        self.process_reaction(ReactionEvent::Removed { target_event_id, key, sender_id }).await;
    }
}

#[async_trait]
impl Middleware for WeeklyGathering {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let mut reaction_rx = self.reaction_rx.lock().await;

        tracing::info!(
            service_id=%self.config.service_id,
            room_id=%self.config.room_id,
            event_day_of_week=?self.config.event_day_of_week,
            event_time=%self.config.event_time,
            announce_minutes_before=%self.config.announce_minutes_before,
            finalize_minutes_before=%self.config.finalize_minutes_before,
            "weekly_gathering middleware running"
        );

        loop {
            let now = Local::now();
            let phase = {
                let state = self.state.lock().await;
                state.phase.clone()
            };

            // Calculate next action time based on current phase
            let (next_action_time, action_name) = match &phase {
                GatheringPhase::Idle => {
                    let announce_time = self.announcement_time();
                    (announce_time, "announce")
                }
                GatheringPhase::Announced { .. } => {
                    let finalize_time = self.finalization_time();
                    (finalize_time, "finalize")
                }
                GatheringPhase::Finalized => {
                    // Wait until next event time, then reset
                    let next_event = self.next_event_time();
                    (next_event, "reset")
                }
            };

            let duration_until =
                (next_action_time - now).to_std().unwrap_or(std::time::Duration::from_secs(1));

            tracing::debug!(
                phase=?phase,
                next_action=%action_name,
                next_action_time=%next_action_time.format("%Y-%m-%d %H:%M:%S"),
                duration_secs=%duration_until.as_secs(),
                "waiting for next action"
            );

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("weekly_gathering middleware shutting down");
                    break;
                }
                _ = tokio::time::sleep(duration_until) => {
                    match action_name {
                        "announce" => {
                            tracing::info!("posting weekly gathering announcement");
                            if let Ok(Some(message_id)) = self.post_announcement().await {
                                let mut state = self.state.lock().await;
                                state.phase = GatheringPhase::Announced { message_id };
                            }
                        }
                        "finalize" => {
                            tracing::info!("posting weekly gathering finalization");
                            self.post_finalization().await;
                            let mut state = self.state.lock().await;
                            state.phase = GatheringPhase::Finalized;
                        }
                        "reset" => {
                            self.reset_for_next_week().await;
                        }
                        _ => {}
                    }
                }
                Some(reaction) = reaction_rx.recv() => {
                    self.process_reaction(reaction).await;
                }
            }
        }

        Ok(())
    }

    fn on_event(&self, evt: &Event) -> Result<Verdict> {
        // Only process reaction events from the configured service
        if evt.service_id.0 != self.config.service_id {
            return Ok(Verdict::Continue);
        }

        match &evt.kind {
            EventKind::ReactionAdded {
                room_id, target_event_id, key, sender_id, is_self, ..
            } => {
                // Ignore reactions from self
                if *is_self {
                    return Ok(Verdict::Continue);
                }

                // Only process reactions in the configured room
                if room_id != &self.config.room_id {
                    return Ok(Verdict::Continue);
                }

                let reaction = ReactionEvent::Added {
                    target_event_id: target_event_id.clone(),
                    key: key.clone(),
                    sender_id: sender_id.clone(),
                };

                if let Err(e) = self.reaction_tx.try_send(reaction) {
                    tracing::warn!(error=?e, "failed to queue reaction event");
                }
            }
            EventKind::ReactionRemoved {
                room_id,
                target_event_id,
                key,
                sender_id,
                is_self,
                ..
            } => {
                // Ignore reactions from self
                if *is_self {
                    return Ok(Verdict::Continue);
                }

                // Only process reactions in the configured room
                if room_id != &self.config.room_id {
                    return Ok(Verdict::Continue);
                }

                let reaction = ReactionEvent::Removed {
                    target_event_id: target_event_id.clone(),
                    key: key.clone(),
                    sender_id: sender_id.clone(),
                };

                if let Err(e) = self.reaction_tx.try_send(reaction) {
                    tracing::warn!(error=?e, "failed to queue reaction removal event");
                }
            }
            _ => {}
        }

        Ok(Verdict::Continue)
    }
}
