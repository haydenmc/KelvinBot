use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, MiddlewareContext, Verdict},
    service::ServiceId,
};
use crate::store::PersistentStore;
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use rand::seq::SliceRandom;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};
use tokio_util::sync::CancellationToken;

/// Keycap emoji assigned to event time options, by configured position.
const TIME_REACTIONS: [&str; 10] = [
    "1\u{fe0f}\u{20e3}",
    "2\u{fe0f}\u{20e3}",
    "3\u{fe0f}\u{20e3}",
    "4\u{fe0f}\u{20e3}",
    "5\u{fe0f}\u{20e3}",
    "6\u{fe0f}\u{20e3}",
    "7\u{fe0f}\u{20e3}",
    "8\u{fe0f}\u{20e3}",
    "9\u{fe0f}\u{20e3}",
    "\u{1f51f}",
];

#[derive(Debug, Clone)]
pub struct Household {
    pub name: String,
    pub members: Vec<String>,
}

/// A candidate start time participants can vote for.
#[derive(Debug, Clone)]
pub struct EventTimeOption {
    pub time: NaiveTime,
    /// Keycap emoji used to vote for this option.
    pub reaction: String,
    /// Human-friendly rendering, e.g. `4:30pm`.
    pub label: String,
}

/// Parse a comma-separated list of `HH:MM` times into voteable options.
///
/// Each option is assigned a keycap emoji by position. Returns an error for
/// unparseable entries, duplicate times, or more options than there are
/// keycap emoji.
pub fn parse_event_times(spec: &str) -> Result<Vec<EventTimeOption>> {
    let entries: Vec<&str> =
        spec.split(',').map(str::trim).filter(|entry| !entry.is_empty()).collect();

    if entries.len() > TIME_REACTIONS.len() {
        anyhow::bail!(
            "too many event_times ({}); at most {} are supported",
            entries.len(),
            TIME_REACTIONS.len()
        );
    }

    let mut options: Vec<EventTimeOption> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let time = NaiveTime::parse_from_str(entry, "%H:%M").map_err(|_| {
            anyhow::anyhow!(
                "invalid event_times entry '{}'. Expected format: HH:MM (e.g., 16:30)",
                entry
            )
        })?;

        if options.iter().any(|existing| existing.time == time) {
            anyhow::bail!("duplicate event_times entry '{}'", entry);
        }

        options.push(EventTimeOption {
            time,
            reaction: TIME_REACTIONS[index].to_string(),
            label: format_time_label(time),
        });
    }

    Ok(options)
}

/// The date of the gathering upcoming as of `now`.
///
/// Rolls to next week once the current cycle's poll has closed — past `finalize_time` on the day
/// itself there is nothing left to organize, so the next gathering is a week out.
pub fn next_event_date_from(
    now: DateTime<Local>,
    event_day_of_week: Weekday,
    finalize_time: NaiveTime,
) -> NaiveDate {
    let current_num = now.weekday().number_from_monday();
    let target_num = event_day_of_week.number_from_monday();

    let days_until_target = if now.weekday() == event_day_of_week {
        if now.time() < finalize_time { 0 } else { 7 }
    } else if target_num > current_num {
        target_num - current_num
    } else {
        7 - (current_num - target_num)
    };

    now.date_naive() + Duration::days(days_until_target as i64)
}

/// Render a time as `4:30pm`.
fn format_time_label(time: NaiveTime) -> String {
    time.format("%-I:%M%P").to_string()
}

/// Strip variation selectors so keycap emoji compare equal regardless of
/// whether the sending client included U+FE0F.
fn normalize_reaction(key: &str) -> String {
    key.chars().filter(|c| *c != '\u{fe0f}').collect()
}

pub struct WeeklyGatheringConfig {
    pub service_id: String,
    pub room_id: String,
    pub event_day_of_week: Weekday,
    /// Candidate start times. A single option is a fixed time rather than a poll.
    pub event_time_options: Vec<EventTimeOption>,
    /// When the poll closes, on the event day itself.
    pub finalize_time: NaiveTime,
    /// How long the poll stays open before `finalize_time`.
    pub poll_open_minutes: u32,
    pub reaction_virtual: String,
    pub reaction_in_person: String,
    pub reaction_host: String,
    pub announcement_message: String,
    pub finalization_virtual_message: String,
    pub finalization_in_person_message: String,
    pub finalization_no_votes_message: String,
    pub time_prompt_message: String,
    pub households: Vec<Household>,
}

/// State of the finalization message, retained so the host's time pick can be
/// applied and the message re-rendered in place.
#[derive(Debug, Clone)]
struct FinalizedState {
    /// Date of the gathering this finalization is for.
    event_date: NaiveDate,
    /// `None` if the finalization message failed to send.
    message_id: Option<String>,
    /// User IDs permitted to pick the time — the selected host and their
    /// household. Empty when the time is not the host's to choose.
    pickers: HashSet<String>,
    /// Index into `config.event_time_options`.
    selected_time: Option<usize>,
    virtual_count: usize,
    in_person_count: usize,
    host_display: String,
}

#[derive(Debug, Clone)]
enum GatheringPhase {
    Idle,
    Announced { event_date: NaiveDate, message_id: String },
    Finalized(FinalizedState),
}

struct GatheringState {
    phase: GatheringPhase,
    virtual_votes: HashSet<String>,
    in_person_votes: HashSet<String>,
    host_volunteers: HashSet<String>,
    /// Votes per event time option, parallel to `config.event_time_options`.
    time_votes: Vec<HashSet<String>>,
}

impl GatheringState {
    fn new(time_option_count: usize) -> Self {
        Self {
            phase: GatheringPhase::Idle,
            virtual_votes: HashSet::new(),
            in_person_votes: HashSet::new(),
            host_volunteers: HashSet::new(),
            time_votes: vec![HashSet::new(); time_option_count],
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
    store: Arc<PersistentStore>,
    reaction_tx: tokio::sync::mpsc::Sender<ReactionEvent>,
    reaction_rx: Arc<Mutex<tokio::sync::mpsc::Receiver<ReactionEvent>>>,
}

impl WeeklyGathering {
    pub fn new(ctx: MiddlewareContext, config: WeeklyGatheringConfig) -> Self {
        let MiddlewareContext { cmd_tx, store } = ctx;
        let (reaction_tx, reaction_rx) = tokio::sync::mpsc::channel(100);

        let state = GatheringState::new(config.event_time_options.len());

        Self {
            cmd_tx,
            config,
            state: Arc::new(Mutex::new(state)),
            store,
            reaction_tx,
            reaction_rx: Arc::new(Mutex::new(reaction_rx)),
        }
    }

    /// The date of the upcoming gathering.
    fn next_event_date(&self) -> NaiveDate {
        next_event_date_from(Local::now(), self.config.event_day_of_week, self.config.finalize_time)
    }

    /// When the poll for `event_date` closes — `finalize_time` on the day itself
    fn finalization_time(&self, event_date: NaiveDate) -> DateTime<Local> {
        Local
            .from_local_datetime(&event_date.and_time(self.config.finalize_time))
            .single()
            .unwrap_or_else(Local::now)
    }

    /// When the poll for `event_date` opens
    fn announcement_time(&self, event_date: NaiveDate) -> DateTime<Local> {
        self.finalization_time(event_date) - Duration::minutes(self.config.poll_open_minutes as i64)
    }

    /// Day the event falls on, phrased relative to today (e.g. "Today", "Tomorrow", "Saturday")
    fn friendly_day(&self, event_date: NaiveDate) -> String {
        let today = Local::now().date_naive();

        if event_date == today {
            "Today".to_string()
        } else if event_date == today + Duration::days(1) {
            "Tomorrow".to_string()
        } else {
            self.config.event_day_of_week.to_string()
        }
    }

    /// Format event time in a friendly way (e.g., "Today at 7:00pm", "Tomorrow at 7:00pm", "Saturday at 7:00pm")
    ///
    /// `selected_time` is the index of the chosen option; while no time has been settled on, the
    /// day is rendered with a "time TBD" marker instead.
    fn format_friendly_time(&self, event_date: NaiveDate, selected_time: Option<usize>) -> String {
        let day_part = self.friendly_day(event_date);

        let time = selected_time
            .and_then(|i| self.config.event_time_options.get(i))
            .map(|option| option.time);

        match time {
            Some(time) => format!("{} at {}", day_part, format_time_label(time)),
            None => format!("{} (time TBD)", day_part),
        }
    }

    /// Whether participants get a say in the start time.
    ///
    /// A lone configured option is a fixed time, not a poll.
    fn time_voting_enabled(&self) -> bool {
        self.config.event_time_options.len() > 1
    }

    /// The option index settled on without anyone picking: the sole option when the time is
    /// fixed, otherwise the most preferred voted time.
    fn default_time(&self, time_votes: &[HashSet<String>]) -> Option<usize> {
        if self.time_voting_enabled() {
            self.top_time_option(time_votes)
        } else {
            self.config.event_time_options.first().map(|_| 0)
        }
    }

    /// Index of the event time option matching a reaction key, if any
    fn time_option_index(&self, key: &str) -> Option<usize> {
        let key = normalize_reaction(key);
        self.config
            .event_time_options
            .iter()
            .position(|option| normalize_reaction(&option.reaction) == key)
    }

    /// Event time option indices ordered by vote count descending, ties broken by earliest time
    fn rank_time_options(&self, time_votes: &[HashSet<String>]) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.config.event_time_options.len()).collect();
        indices.sort_by(|a, b| {
            let votes_a = time_votes.get(*a).map_or(0, HashSet::len);
            let votes_b = time_votes.get(*b).map_or(0, HashSet::len);
            votes_b.cmp(&votes_a).then_with(|| {
                self.config.event_time_options[*a]
                    .time
                    .cmp(&self.config.event_time_options[*b].time)
            })
        });
        indices
    }

    /// The most preferred time option, or `None` if nobody voted on a time
    fn top_time_option(&self, time_votes: &[HashSet<String>]) -> Option<usize> {
        let top = *self.rank_time_options(time_votes).first()?;
        if time_votes.get(top).is_none_or(HashSet::is_empty) { None } else { Some(top) }
    }

    /// Render the configured time options as a list, one per line
    fn render_time_options(&self) -> String {
        self.config
            .event_time_options
            .iter()
            .map(|option| format!("- {} {}", option.reaction, option.label))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Render time vote results, most preferred first, marking the selected option
    fn render_time_results(
        &self,
        time_votes: &[HashSet<String>],
        selected_time: Option<usize>,
    ) -> String {
        self.rank_time_options(time_votes)
            .into_iter()
            .map(|index| {
                let option = &self.config.event_time_options[index];
                let count = time_votes.get(index).map_or(0, HashSet::len);
                let plural = if count == 1 { "vote" } else { "votes" };
                let marker =
                    if selected_time == Some(index) { " **← selected by host**" } else { "" };
                format!("- {} {} — {} {}{}", option.reaction, option.label, count, plural, marker)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Select a host from volunteers, preferring those who have hosted least recently.
    ///
    /// Volunteers absent from `host_history` are treated as having never hosted and are
    /// always preferred over those with any recorded history. Among candidates with equal
    /// last-hosted timestamps, one is chosen at random.
    ///
    /// Members of the same household are treated as a single candidate unit. The household's
    /// effective last-hosted time is the most recent time any member has hosted — so if one
    /// member hosted last week, the whole household is considered to have hosted last week.
    /// When a household member hosts, all members should have their history updated together.
    ///
    /// Returns `(user_id, display_name)` where `display_name` is the household name for
    /// household volunteers, or the user_id for solo volunteers.
    pub fn select_host(
        volunteers: &HashSet<String>,
        host_history: &HashMap<String, DateTime<Utc>>,
        households: &[Household],
    ) -> Option<(String, String)> {
        if volunteers.is_empty() {
            return None;
        }

        // Treat "never hosted" as the Unix epoch so they sort before anyone who has hosted.
        let epoch = DateTime::<Utc>::UNIX_EPOCH;

        struct CandidateUnit {
            effective_time: DateTime<Utc>,
            volunteering_members: Vec<String>,
            display_name: String,
        }

        let mut seen_households: HashSet<String> = HashSet::new();
        let mut units: Vec<CandidateUnit> = Vec::new();

        for volunteer in volunteers {
            if let Some(household) = households.iter().find(|h| h.members.contains(volunteer)) {
                if seen_households.contains(&household.name) {
                    continue; // Already added this household as a unit
                }
                seen_households.insert(household.name.clone());

                // Effective time = max of all household members' history
                let effective_time = household
                    .members
                    .iter()
                    .map(|m| host_history.get(m).copied().unwrap_or(epoch))
                    .max()
                    .unwrap_or(epoch);

                // Only the members who actually volunteered are candidates for selection
                let volunteering_members: Vec<String> =
                    household.members.iter().filter(|m| volunteers.contains(*m)).cloned().collect();

                units.push(CandidateUnit {
                    effective_time,
                    volunteering_members,
                    display_name: household.name.clone(),
                });
            } else {
                // Solo volunteer — not in any household
                units.push(CandidateUnit {
                    effective_time: host_history.get(volunteer).copied().unwrap_or(epoch),
                    volunteering_members: vec![volunteer.clone()],
                    display_name: volunteer.clone(),
                });
            }
        }

        let min_time = units.iter().map(|u| u.effective_time).min().expect("units is non-empty");

        let tied_units: Vec<&CandidateUnit> =
            units.iter().filter(|u| u.effective_time == min_time).collect();

        let mut rng = rand::thread_rng();
        let chosen_unit = tied_units.choose(&mut rng)?;

        // Pick one of the volunteering members from the chosen unit at random
        let user_id = chosen_unit.volunteering_members.choose(&mut rng)?.clone();

        Some((user_id, chosen_unit.display_name.clone()))
    }

    /// Post the announcement message and capture the message ID
    async fn post_announcement(&self, event_date: NaiveDate) -> Result<Option<String>> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        // Replace placeholders in announcement message. The start time is not settled at
        // announcement, unless there is only one option to begin with.
        let event_time_friendly = self.format_friendly_time(event_date, self.default_time(&[]));
        let message = self
            .config
            .announcement_message
            .replace("{reaction_virtual}", &self.config.reaction_virtual)
            .replace("{reaction_in_person}", &self.config.reaction_in_person)
            .replace("{reaction_host}", &self.config.reaction_host)
            .replace("{time_options}", &self.render_time_options())
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

                // Pre-populate reactions on the announcement. Time options are only worth
                // offering when there is a genuine choice between them.
                let time_reactions = self
                    .config
                    .event_time_options
                    .iter()
                    .filter(|_| self.time_voting_enabled())
                    .map(|option| &option.reaction);

                let reaction_keys = [
                    &self.config.reaction_virtual,
                    &self.config.reaction_in_person,
                    &self.config.reaction_host,
                ]
                .into_iter()
                .chain(time_reactions);

                for reaction_key in reaction_keys {
                    self.seed_reaction(&message_id, reaction_key).await;
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

    /// Render the finalization message for the current vote outcome.
    ///
    /// Called both when finalizing and again whenever the host picks a time, so the message can be
    /// edited in place.
    fn render_finalization(
        &self,
        finalized: &FinalizedState,
        time_votes: &[HashSet<String>],
    ) -> String {
        let FinalizedState { virtual_count, in_person_count, selected_time, .. } = *finalized;

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

        // The prompt is only shown while a host who has yet to choose still could. Substituted
        // first so its own placeholders are resolved by the replacements below.
        let awaiting_pick = !finalized.pickers.is_empty() && selected_time.is_none();
        let time_prompt = if awaiting_pick { self.config.time_prompt_message.as_str() } else { "" };

        let event_time_friendly = self.format_friendly_time(finalized.event_date, selected_time);
        template
            .replace("{time_prompt}", time_prompt)
            .replace("{virtual_count}", &virtual_count.to_string())
            .replace("{in_person_count}", &in_person_count.to_string())
            .replace("{host}", &finalized.host_display)
            .replace("{time_results}", &self.render_time_results(time_votes, selected_time))
            .replace("{event_time}", &event_time_friendly)
    }

    /// Post the finalization message with vote counts and host
    pub async fn post_finalization(&self, event_date: NaiveDate) {
        // Load host history before acquiring the state lock.
        let mut host_history: HashMap<String, DateTime<Utc>> =
            self.store.get("host_history").await.unwrap_or_default();

        let state = self.state.lock().await;

        let virtual_count = state.virtual_votes.len();
        let in_person_count = state.in_person_votes.len();

        // Select host
        let host =
            Self::select_host(&state.host_volunteers, &host_history, &self.config.households);

        let host_display = match &host {
            Some((_, display)) => display.clone(),
            None => "No host volunteered".to_string(),
        };

        // The host only picks the time for in-person gatherings — a virtual gathering imposes no
        // venue constraint, so the most preferred time simply wins.
        let is_in_person = in_person_count > virtual_count;
        let pickers: HashSet<String> = match (&host, is_in_person) {
            (Some((user_id, _)), true) if self.time_voting_enabled() => {
                self.household_members(user_id).into_iter().collect()
            }
            _ => HashSet::new(),
        };

        let awaiting_pick = !pickers.is_empty();
        let selected_time = if awaiting_pick { None } else { self.default_time(&state.time_votes) };

        let mut finalized = FinalizedState {
            event_date,
            message_id: None,
            pickers,
            selected_time,
            virtual_count,
            in_person_count,
            host_display,
        };

        let message = self.render_finalization(&finalized, &state.time_votes);

        // Order the pick reactions by preference so they read left-to-right most-wanted first.
        let ranked_reactions: Vec<String> = if awaiting_pick {
            self.rank_time_options(&state.time_votes)
                .into_iter()
                .map(|index| self.config.event_time_options[index].reaction.clone())
                .collect()
        } else {
            Vec::new()
        };

        drop(state); // Release lock before sending

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let command = Command::SendRoomMessage {
            service_id: ServiceId(self.config.service_id.clone()),
            room_id: self.config.room_id.clone(),
            body: message.clone(),
            markdown_body: Some(message),
            response_tx: Some(response_tx),
        };

        if let Err(e) = self.cmd_tx.send(command).await {
            tracing::error!(error=%e, "failed to send finalization message");
        }

        let message_id = match response_rx.await {
            Ok(Ok(message_id)) => Some(message_id),
            Ok(Err(e)) => {
                tracing::error!(error=%e, "failed to post finalization");
                None
            }
            Err(e) => {
                tracing::error!(error=%e, "failed to receive finalization response");
                None
            }
        };

        // Offer every configured time so the host is never boxed out of an unvoted slot.
        if let Some(message_id) = &message_id {
            for reaction_key in &ranked_reactions {
                self.seed_reaction(message_id, reaction_key).await;
            }
        }

        {
            finalized.message_id = message_id;
            let mut state = self.state.lock().await;
            state.phase = GatheringPhase::Finalized(finalized);
        }

        // Persist the newly selected host so future weeks prefer someone else.
        // If the host is part of a household, update all members at the same timestamp.
        if let Some((ref selected_user_id, _)) = host {
            let now = Utc::now();
            for member in &self.household_members(selected_user_id) {
                host_history.insert(member.clone(), now);
            }
            if let Err(e) = self.store.set("host_history", &host_history).await {
                tracing::error!(error=%e, "failed to persist host history");
            }
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
        for votes in &mut state.time_votes {
            votes.clear();
        }
        tracing::info!("state reset for next week");
    }

    /// All members of the household `user_id` belongs to, or just `user_id` if they are solo
    fn household_members(&self, user_id: &str) -> Vec<String> {
        self.config
            .households
            .iter()
            .find(|h| h.members.iter().any(|m| m == user_id))
            .map(|h| h.members.clone())
            .unwrap_or_else(|| vec![user_id.to_string()])
    }

    /// Add one of the bot's own reactions to a message, for others to click
    async fn seed_reaction(&self, message_id: &str, key: &str) {
        let command = Command::AddReaction {
            service_id: ServiceId(self.config.service_id.clone()),
            room_id: self.config.room_id.clone(),
            event_id: message_id.to_string(),
            key: key.to_string(),
        };

        if let Err(e) = self.cmd_tx.send(command).await {
            tracing::error!(error=%e, key=%key, "failed to send add reaction command");
        }
    }

    /// Process a reaction event
    async fn process_reaction(&self, reaction: ReactionEvent) {
        let edit = {
            let mut state = self.state.lock().await;

            match state.phase.clone() {
                GatheringPhase::Announced { message_id, .. } => {
                    self.process_vote_reaction(&mut state, &message_id, reaction);
                    None
                }
                GatheringPhase::Finalized(finalized) => {
                    self.process_pick_reaction(&mut state, &finalized, reaction)
                }
                GatheringPhase::Idle => None,
            }
        };

        // Reflect a changed pick in the finalization message, outside the state lock.
        if let Some((message_id, body)) = edit {
            let command = Command::EditMessage {
                service_id: ServiceId(self.config.service_id.clone()),
                message_id,
                new_body: body.clone(),
                new_markdown_body: Some(body),
            };

            if let Err(e) = self.cmd_tx.send(command).await {
                tracing::error!(error=%e, "failed to edit finalization message");
            }
        }
    }

    /// Record a vote from a reaction on the announcement message
    fn process_vote_reaction(
        &self,
        state: &mut GatheringState,
        announcement_message_id: &str,
        reaction: ReactionEvent,
    ) {
        match reaction {
            ReactionEvent::Added { target_event_id, key, sender_id } => {
                // Check if reaction is on the announcement message
                if target_event_id != announcement_message_id {
                    return;
                }

                // Process based on reaction type
                if key == self.config.reaction_virtual {
                    state.virtual_votes.insert(sender_id.clone());
                    tracing::debug!(sender_id=%sender_id, "virtual vote recorded");
                } else if key == self.config.reaction_in_person {
                    state.in_person_votes.insert(sender_id.clone());
                    tracing::debug!(sender_id=%sender_id, "in-person vote recorded");
                } else if key == self.config.reaction_host {
                    state.host_volunteers.insert(sender_id.clone());
                    tracing::debug!(sender_id=%sender_id, "host volunteer recorded");
                } else if let Some(index) = self.time_option_index(&key) {
                    state.time_votes[index].insert(sender_id.clone());
                    tracing::debug!(sender_id=%sender_id, index=%index, "time vote recorded");
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
                    } else if let Some(index) = self.time_option_index(&key) {
                        state.time_votes[index].remove(&sender_id);
                        tracing::debug!(sender_id=%sender_id, index=%index, "time vote removed");
                    }
                }
            }
        }
    }

    /// Apply the host's time pick from a reaction on the finalization message.
    ///
    /// Returns the message ID and re-rendered body when the selection changed.
    fn process_pick_reaction(
        &self,
        state: &mut GatheringState,
        finalized: &FinalizedState,
        reaction: ReactionEvent,
    ) -> Option<(String, String)> {
        let message_id = finalized.message_id.clone()?;

        let (target_event_id, key, sender_id, added) = match reaction {
            ReactionEvent::Added { target_event_id, key, sender_id } => {
                (Some(target_event_id), Some(key), sender_id, true)
            }
            ReactionEvent::Removed { target_event_id, key, sender_id } => {
                (target_event_id, key, sender_id, false)
            }
        };

        // Only reactions on the finalization message, from the host or their household, count.
        if target_event_id.is_some_and(|target| target != message_id) {
            return None;
        }
        if !finalized.pickers.contains(&sender_id) {
            return None;
        }

        let index = self.time_option_index(&key?)?;

        // Adding picks a time; removing only clears the currently selected one.
        let selected_time = if added {
            Some(index)
        } else if finalized.selected_time == Some(index) {
            None
        } else {
            return None;
        };

        if selected_time == finalized.selected_time {
            return None;
        }

        let mut updated = finalized.clone();
        updated.selected_time = selected_time;
        if let GatheringPhase::Finalized(current) = &mut state.phase {
            current.selected_time = selected_time;
        }

        tracing::info!(sender_id=%sender_id, selected_time=?selected_time, "event time pick updated");

        let body = self.render_finalization(&updated, &state.time_votes);

        Some((message_id, body))
    }
}

// Test helpers - exposed for integration tests in tests/unit/middleware.rs
// TODO: Ideally, move the WeeklyGathering tests into this crate as a #[cfg(test)] mod tests
// block, which would allow these helpers to be conditionally compiled and hidden from the
// public API.
#[doc(hidden)]
impl WeeklyGathering {
    /// The upcoming event date (for testing)
    pub fn test_event_date(&self) -> NaiveDate {
        self.next_event_date()
    }

    /// Set the phase to Announced with a specific message ID (for testing)
    pub async fn set_announced(&self, message_id: String) {
        let mut state = self.state.lock().await;
        let event_date = self.next_event_date();
        state.phase = GatheringPhase::Announced { event_date, message_id };
    }

    /// Post the announcement directly (for testing)
    pub async fn test_post_announcement(&self) {
        if let Err(e) = self.post_announcement(self.next_event_date()).await {
            tracing::error!(error=%e, "failed to post announcement");
        }
    }

    /// Set the phase to Finalized with a specific message ID and pickers (for testing)
    pub async fn set_finalized(&self, message_id: Option<String>, pickers: HashSet<String>) {
        let mut state = self.state.lock().await;
        state.phase = GatheringPhase::Finalized(FinalizedState {
            event_date: self.next_event_date(),
            message_id,
            pickers,
            selected_time: None,
            virtual_count: state.virtual_votes.len(),
            in_person_count: state.in_person_votes.len(),
            host_display: "test-host".to_string(),
        });
    }

    /// Get the index of the selected event time, if any (for testing)
    pub async fn get_selected_time(&self) -> Option<usize> {
        let state = self.state.lock().await;
        match &state.phase {
            GatheringPhase::Finalized(finalized) => finalized.selected_time,
            _ => None,
        }
    }

    /// Get vote counts per event time option, in configured order (for testing)
    pub async fn get_time_votes(&self) -> Vec<usize> {
        let state = self.state.lock().await;
        state.time_votes.iter().map(HashSet::len).collect()
    }

    /// Get current vote counts (for testing): (virtual, in_person, host_volunteers)
    pub async fn get_vote_counts(&self) -> (usize, usize, usize) {
        let state = self.state.lock().await;
        (state.virtual_votes.len(), state.in_person_votes.len(), state.host_volunteers.len())
    }

    /// Set the host history in the backing store (for testing)
    pub async fn set_host_history(&self, history: HashMap<String, DateTime<Utc>>) {
        self.store
            .set("host_history", &history)
            .await
            .expect("failed to set host history in store");
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
            event_time_options=%self.config.event_time_options.len(),
            finalize_time=%self.config.finalize_time,
            poll_open_minutes=%self.config.poll_open_minutes,
            "weekly_gathering middleware running"
        );

        loop {
            let now = Local::now();
            let phase = {
                let state = self.state.lock().await;
                state.phase.clone()
            };

            // Each phase waits for its own schedule point, for a known cycle. Announced and
            // Finalized carry their cycle's date so a poll that closes in the morning doesn't
            // get re-derived onto the same day it just finished.
            let (next_action_time, action_name, event_date) = match &phase {
                GatheringPhase::Idle => {
                    let event_date = self.next_event_date();
                    (self.announcement_time(event_date), "announce", event_date)
                }
                GatheringPhase::Announced { event_date, .. } => {
                    (self.finalization_time(*event_date), "finalize", *event_date)
                }
                GatheringPhase::Finalized(finalized) => {
                    // This cycle is done; wait for the next one to open.
                    let event_date = finalized.event_date + Duration::days(7);
                    (self.announcement_time(event_date), "announce", event_date)
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
                            // Clear last cycle's votes before opening the new poll.
                            self.reset_for_next_week().await;

                            tracing::info!(event_date=%event_date, "posting weekly gathering announcement");
                            if let Ok(Some(message_id)) = self.post_announcement(event_date).await {
                                let mut state = self.state.lock().await;
                                state.phase = GatheringPhase::Announced { event_date, message_id };
                            }
                        }
                        "finalize" => {
                            tracing::info!(event_date=%event_date, "posting weekly gathering finalization");
                            // post_finalization moves the phase to Finalized itself, since it
                            // owns the message ID and host needed to accept a time pick.
                            self.post_finalization(event_date).await;
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
