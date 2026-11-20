use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, MiddlewareContext},
    service::ServiceId,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime, TimeZone};
use ical::parser::ical::component::IcalEvent;
use rrule::RRuleSet;
use secrecy::{ExposeSecret, SecretString};
use std::io::BufReader;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};
use tokio_util::sync::CancellationToken;

/// Store key holding the date of the last agenda post, so a restart within the
/// same day doesn't post twice.
const LAST_POSTED_KEY: &str = "last_posted_date";

/// Upper bound on occurrences expanded from a single recurrence rule. Generous
/// enough for a daily rule over the countdown window, small enough that a
/// pathological feed can't hang the scheduler.
const MAX_OCCURRENCES: u16 = 512;

/// Extra days searched past the furthest countdown interval, so an event that
/// starts just outside the window still lands in the expansion.
const EXPANSION_SLACK_DAYS: i64 = 1;

/// How long to wait before trying the daily agenda again after the calendar
/// could not be fetched.
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

pub struct CalendarAgendaConfig {
    pub service_id: String,
    pub room_id: String,
    /// ICS feed URL. Private to the bot: never rendered into a message or a
    /// log line.
    pub calendar_url: SecretString,
    /// Optional human-facing calendar link rendered as a footer.
    pub calendar_link: Option<String>,
    pub calendar_link_text: String,
    pub post_at_time: NaiveTime,
    /// Days-before intervals for multi-day event countdowns, descending.
    pub countdown_days: Vec<u32>,
    /// Days-before intervals for single-day, non-recurring event reminders,
    /// descending.
    pub reminder_days: Vec<u32>,
    /// Minimum span in days for an event to count as "multi-day".
    pub multi_day_min_days: u32,
    pub heading_today: String,
    pub heading_reminders: String,
    pub heading_countdowns: String,
    /// Optional chat command for an on-demand agenda. Disabled when `None`.
    pub command_string: Option<String>,
}

/// A start or end instant from the feed. All-day events carry a bare date;
/// per RFC 5545 their `DTEND` is exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTime {
    AllDay(NaiveDate),
    Timed(DateTime<Local>),
}

impl EventTime {
    fn date(&self) -> NaiveDate {
        match self {
            EventTime::AllDay(d) => *d,
            EventTime::Timed(dt) => dt.date_naive(),
        }
    }

    fn time(&self) -> Option<NaiveTime> {
        match self {
            EventTime::AllDay(_) => None,
            EventTime::Timed(dt) => Some(dt.time()),
        }
    }
}

/// A `VEVENT` from the feed, before recurrence expansion.
#[derive(Debug, Clone)]
pub struct CalendarEvent {
    pub summary: String,
    pub location: Option<String>,
    pub start: EventTime,
    pub end: EventTime,
    /// Reconstructed `DTSTART`/`RRULE`/`RDATE`/`EXDATE` block, present only
    /// when the event actually recurs.
    pub recurrence: Option<String>,
    /// Whether this is one rescheduled or edited instance of a recurring
    /// event (it has a `RECURRENCE-ID`), not an event in its own right.
    pub recurring_instance: bool,
}

/// One concrete instance of an event on the calendar.
#[derive(Debug, Clone)]
pub struct Occurrence {
    pub summary: String,
    pub location: Option<String>,
    pub start_date: NaiveDate,
    /// `None` for all-day events.
    pub start_time: Option<NaiveTime>,
    pub end_time: Option<NaiveTime>,
    /// Inclusive: the last day the event covers.
    pub end_date: NaiveDate,
    pub is_recurring: bool,
}

impl Occurrence {
    /// Length in days, counting the first and last day (a single-day event is 1).
    fn span_days(&self) -> u32 {
        ((self.end_date - self.start_date).num_days() + 1).max(1) as u32
    }
}

pub struct CalendarAgenda {
    cmd_tx: Sender<Command>,
    store: Arc<crate::store::PersistentStore>,
    config: CalendarAgendaConfig,
    query_tx: tokio::sync::mpsc::Sender<()>,
    query_rx: Arc<Mutex<tokio::sync::mpsc::Receiver<()>>>,
}

impl CalendarAgenda {
    pub fn new(ctx: MiddlewareContext, config: CalendarAgendaConfig) -> Self {
        let (query_tx, query_rx) = tokio::sync::mpsc::channel(16);
        Self {
            cmd_tx: ctx.cmd_tx,
            store: ctx.store,
            config,
            query_tx,
            query_rx: Arc::new(Mutex::new(query_rx)),
        }
    }

    /// Whether the daily agenda should be posted now: today's post time has
    /// passed and today's agenda has not gone out yet.
    fn is_due(&self, now: DateTime<Local>, last_posted: Option<NaiveDate>) -> bool {
        now.time() >= self.config.post_at_time
            && last_posted.is_none_or(|last| last < now.date_naive())
    }

    /// When to wake next: for a pending retry if there is one, otherwise for
    /// the next scheduled post.
    fn next_wake(
        &self,
        now: DateTime<Local>,
        retry_at: Option<DateTime<Local>>,
    ) -> DateTime<Local> {
        let scheduled = self.next_scheduled_time(now);
        retry_at.map_or(scheduled, |retry_at| retry_at.min(scheduled))
    }

    /// The next local datetime at which the daily agenda should be posted.
    fn next_scheduled_time(&self, now: DateTime<Local>) -> DateTime<Local> {
        let target_date = if now.time() < self.config.post_at_time {
            now.date_naive()
        } else {
            now.date_naive() + Duration::days(1)
        };
        local_datetime(target_date, self.config.post_at_time)
    }

    /// How far ahead occurrences need to be expanded to satisfy the furthest
    /// countdown or reminder interval.
    fn lookahead_days(&self) -> i64 {
        let furthest = self
            .config
            .countdown_days
            .iter()
            .chain(self.config.reminder_days.iter())
            .copied()
            .max()
            .unwrap_or(0);
        i64::from(furthest) + EXPANSION_SLACK_DAYS
    }

    /// Fetch, parse, and expand the feed into occurrences relevant to `today`.
    async fn load_occurrences(&self, today: NaiveDate) -> Result<Vec<Occurrence>> {
        let body = fetch_calendar(self.config.calendar_url.expose_secret()).await?;
        let events = parse_calendar(&body)?;
        Ok(expand(&events, today, today + Duration::days(self.lookahead_days())))
    }

    /// Build the agenda for `today`, or `None` when nothing is worth posting.
    async fn build_message(&self, today: NaiveDate) -> Result<Option<String>> {
        let occurrences = self.load_occurrences(today).await?;
        Ok(build_agenda(&occurrences, today, &self.config))
    }

    async fn send(&self, message: String) {
        let command = Command::SendRoomMessage {
            service_id: ServiceId(self.config.service_id.clone()),
            room_id: self.config.room_id.clone(),
            body: message.clone(),
            markdown_body: Some(message),
            response_tx: None,
        };
        if let Err(e) = self.cmd_tx.send(command).await {
            tracing::error!(error=%e, "failed to send calendar agenda message");
        }
    }

    /// Post the daily agenda for `today` and record that we did so. The
    /// last-posted date is recorded even when there was nothing to say, so an
    /// uneventful day isn't retried in a tight loop.
    ///
    /// Returns `false` if the agenda could not be built (the calendar was
    /// unreachable, say), in which case nothing is recorded and the caller
    /// should try again later.
    async fn post_daily_agenda(&self, today: NaiveDate) -> bool {
        match self.build_message(today).await {
            Ok(message) => {
                self.deliver_daily_agenda(today, message).await;
                true
            }
            Err(e) => {
                tracing::error!(error=%e, date=%today, "failed to build daily calendar agenda");
                false
            }
        }
    }

    /// Send the day's agenda (when there is one) and mark the day as handled.
    async fn deliver_daily_agenda(&self, today: NaiveDate, message: Option<String>) {
        match message {
            Some(message) => {
                tracing::info!(date=%today, "posting daily calendar agenda");
                self.send(message).await;
            }
            None => {
                tracing::debug!(date=%today, "no relevant calendar events; skipping agenda");
            }
        }

        if let Err(e) = self.store.set(LAST_POSTED_KEY, &today).await {
            tracing::error!(error=%e, "failed to persist last agenda post date");
        }
    }

    /// Respond to the on-demand command. Does not touch the last-posted date,
    /// so it never suppresses the scheduled post.
    async fn post_on_demand(&self, today: NaiveDate) {
        match self.build_message(today).await {
            Ok(Some(message)) => self.send(message).await,
            Ok(None) => self.send("📅 Nothing on the calendar right now.".to_string()).await,
            Err(e) => {
                tracing::error!(error=%e, "failed to build on-demand calendar agenda");
                self.send("⚠️ Couldn't reach the calendar right now.".to_string()).await;
            }
        }
    }

    async fn last_posted_date(&self) -> Option<NaiveDate> {
        self.store.get::<NaiveDate>(LAST_POSTED_KEY).await
    }

    /// Whether an event is the configured on-demand agenda command.
    fn matches_command(&self, evt: &Event) -> bool {
        let Some(command_string) = &self.config.command_string else {
            return false;
        };

        let EventKind::RoomMessage { room_id, body, is_self, .. } = &evt.kind else {
            return false;
        };

        !*is_self && room_id == &self.config.room_id && body.trim() == command_string
    }
}

#[async_trait]
impl Middleware for CalendarAgenda {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let mut query_rx = self.query_rx.lock().await;

        tracing::info!(
            service_id=%self.config.service_id,
            room_id=%self.config.room_id,
            post_at_time=%self.config.post_at_time,
            countdown_days=?self.config.countdown_days,
            reminder_days=?self.config.reminder_days,
            command_string=?self.config.command_string,
            "calendar_agenda middleware running"
        );

        // Kept in memory once loaded, so a store that cannot be written to
        // does not make the same day's agenda go out again and again.
        let mut last_posted = self.last_posted_date().await;
        // Set while today's agenda is waiting to be tried again.
        let mut retry_at: Option<DateTime<Local>> = None;

        loop {
            let now = Local::now();
            let today = now.date_naive();

            // Covers the scheduled post, catching up after the bot was down
            // past today's post time, and retrying after a failure.
            if self.is_due(now, last_posted) {
                if retry_at.is_none_or(|retry_at| now >= retry_at) {
                    if self.post_daily_agenda(today).await {
                        last_posted = Some(today);
                        retry_at = None;
                    } else {
                        tracing::warn!(
                            retry_in_secs=%RETRY_DELAY.as_secs(),
                            "daily calendar agenda not posted; will try again"
                        );
                        retry_at = Some(now + RETRY_DELAY);
                    }
                    continue;
                }
            } else {
                // Posted, or a new day began before a retry succeeded.
                retry_at = None;
            }

            let next_time = self.next_wake(now, retry_at);
            let duration_until =
                (next_time - now).to_std().unwrap_or(std::time::Duration::from_secs(0));

            tracing::debug!(
                next_scheduled=%next_time.format("%Y-%m-%d %H:%M:%S %Z"),
                duration_secs=%duration_until.as_secs(),
                "waiting for next calendar agenda post"
            );

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("calendar_agenda middleware shutting down...");
                    break;
                }
                _ = tokio::time::sleep(duration_until) => {}
                Some(()) = query_rx.recv() => {
                    self.post_on_demand(Local::now().date_naive()).await;
                }
            }
        }

        Ok(())
    }

    async fn on_event(&self, evt: &Event) -> Result<()> {
        if self.matches_command(evt) {
            // Use try_send so a full queue can't block the event pipeline.
            if let Err(e) = self.query_tx.try_send(()) {
                tracing::warn!(error=?e, "failed to queue on-demand agenda request");
            }
        }

        Ok(())
    }
}

/// Resolve a local date and time, picking the earlier instant when a DST
/// transition makes it ambiguous and the following hour when it doesn't exist.
pub(crate) fn local_datetime(date: NaiveDate, time: NaiveTime) -> DateTime<Local> {
    let naive = date.and_time(time);
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt,
        chrono::LocalResult::Ambiguous(earlier, _) => earlier,
        chrono::LocalResult::None => Local
            .from_local_datetime(&(naive + Duration::hours(1)))
            .earliest()
            .unwrap_or_else(|| Local.from_utc_datetime(&naive)),
    }
}

/// Download an ICS feed. `webcal://` URLs are rewritten to `https://`.
async fn fetch_calendar(url: &str) -> Result<String> {
    let url = normalize_calendar_url(url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build calendar HTTP client")?;
    // `without_url` keeps the secret feed URL out of the error chain.
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .context("failed to fetch calendar feed")?;
    let status = response.status();
    if !status.is_success() {
        // Deliberately does not include the URL: the feed is a secret.
        return Err(anyhow!("calendar feed returned HTTP {status}"));
    }
    response
        .text()
        .await
        .map_err(reqwest::Error::without_url)
        .context("failed to read calendar feed body")
}

/// `webcal://host/path` is just `https://host/path` with a scheme that tells
/// the OS to hand the URL to a calendar client.
pub fn normalize_calendar_url(url: &str) -> String {
    match url.strip_prefix("webcal://") {
        Some(rest) => format!("https://{rest}"),
        None => match url.strip_prefix("webcals://") {
            Some(rest) => format!("https://{rest}"),
            None => url.to_string(),
        },
    }
}

/// Parse an ICS document into events. Individual malformed `VEVENT`s are
/// skipped rather than failing the whole feed.
///
/// A feed describes a change to one instance of a recurring event as a second
/// `VEVENT` with the same `UID` and a `RECURRENCE-ID` naming the instance it
/// replaces. That instance is removed from the series here, so it is not
/// listed at its original time as well; the replacement stays as an event of
/// its own. Cancelled events (`STATUS:CANCELLED`), whether a whole event or
/// one such instance, are dropped.
pub fn parse_calendar(body: &str) -> Result<Vec<CalendarEvent>> {
    let reader = ical::IcalParser::new(BufReader::new(body.as_bytes()));
    let mut events: Vec<ParsedEvent> = Vec::new();

    for calendar in reader {
        let calendar = calendar.context("failed to parse calendar feed")?;
        for ical_event in calendar.events {
            match parse_event(&ical_event) {
                Ok(Some(event)) => events.push(event),
                Ok(None) => {}
                Err(e) => tracing::warn!(error=%e, "skipping unparseable calendar event"),
            }
        }
    }

    // Take each replaced instance out of the series it belongs to.
    let replaced: Vec<(String, String)> =
        events.iter().filter_map(|e| Some((e.uid.clone()?, e.replaces.clone()?))).collect();
    for (uid, exdate_line) in replaced {
        let series = events
            .iter_mut()
            .filter(|e| e.replaces.is_none() && e.uid.as_deref() == Some(uid.as_str()));
        for master in series {
            if let Some(block) = &mut master.event.recurrence {
                block.push('\n');
                block.push_str(&exdate_line);
            }
        }
    }

    Ok(events.into_iter().filter(|e| !e.cancelled).map(|e| e.event).collect())
}

/// A `VEVENT` together with what is needed to relate it to others in the feed.
struct ParsedEvent {
    event: CalendarEvent,
    uid: Option<String>,
    /// For an instance of a recurring event: an `EXDATE` line that removes the
    /// instance it replaces from the series.
    replaces: Option<String>,
    cancelled: bool,
}

/// Parameters attached to an ICS property, e.g. `TZID=America/Los_Angeles`.
/// Shaped by the `ical` crate: each parameter maps to a list of values.
pub type IcsParams = Vec<(String, Vec<String>)>;

/// Look up a property by name, returning its value and parameters.
fn property<'a>(event: &'a IcalEvent, name: &str) -> Option<(&'a str, Option<&'a IcsParams>)> {
    event
        .properties
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .and_then(|p| p.value.as_deref().map(|v| (v, p.params.as_ref())))
}

/// Look up a parameter value on a property (e.g. `TZID`, `VALUE`).
fn param<'a>(params: Option<&'a IcsParams>, name: &str) -> Option<&'a str> {
    params?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, values)| values.first())
        .map(|v| v.as_str())
}

fn parse_event(event: &IcalEvent) -> Result<Option<ParsedEvent>> {
    let Some((dtstart_value, dtstart_params)) = property(event, "DTSTART") else {
        // No start: nothing we can schedule against (e.g. a bare VEVENT stub).
        return Ok(None);
    };

    let start = parse_ics_datetime(dtstart_value, dtstart_params)
        .with_context(|| format!("invalid DTSTART '{dtstart_value}'"))?;

    let end = match property(event, "DTEND") {
        Some((value, params)) => {
            parse_ics_datetime(value, params).with_context(|| format!("invalid DTEND '{value}'"))?
        }
        None => match start {
            // RFC 5545: a DATE-valued DTSTART with no DTEND lasts one day, and
            // DTEND is exclusive.
            EventTime::AllDay(d) => EventTime::AllDay(d + Duration::days(1)),
            EventTime::Timed(dt) => EventTime::Timed(dt),
        },
    };

    let summary = property(event, "SUMMARY")
        .map(|(v, _)| unescape_ics_text(v))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(untitled event)".to_string());

    let location =
        property(event, "LOCATION").map(|(v, _)| unescape_ics_text(v)).filter(|s| !s.is_empty());

    let replaces = recurrence_line(event, "RECURRENCE-ID", "EXDATE");
    let cancelled =
        property(event, "STATUS").is_some_and(|(v, _)| v.trim().eq_ignore_ascii_case("CANCELLED"));

    Ok(Some(ParsedEvent {
        event: CalendarEvent {
            summary,
            location,
            start,
            end,
            recurrence: build_recurrence_block(event),
            recurring_instance: replaces.is_some(),
        },
        uid: property(event, "UID").map(|(v, _)| v.to_string()),
        replaces,
        cancelled,
    }))
}

/// Renders the property `name` as a line for the `rrule` parser, under the
/// name `as_name`. Only `TZID` and `VALUE` parameters are forwarded — `rrule`
/// rejects parameters it doesn't recognize.
fn recurrence_line(event: &IcalEvent, name: &str, as_name: &str) -> Option<String> {
    let (value, params) = property(event, name)?;
    let mut line = as_name.to_string();
    for key in ["VALUE", "TZID"] {
        if let Some(found) = param(params, key) {
            line.push_str(&format!(";{key}={found}"));
        }
    }
    line.push(':');
    line.push_str(value);
    Some(line)
}

/// Reconstruct the `DTSTART`/`RRULE`/`RDATE`/`EXDATE` lines the `rrule` crate
/// parses. Only `TZID` and `VALUE` parameters are forwarded — `rrule` rejects
/// parameters it doesn't recognize.
fn build_recurrence_block(event: &IcalEvent) -> Option<String> {
    let recurs = event
        .properties
        .iter()
        .any(|p| p.name.eq_ignore_ascii_case("RRULE") || p.name.eq_ignore_ascii_case("RDATE"));
    if !recurs {
        return None;
    }

    let mut lines = Vec::new();
    for property in &event.properties {
        let name = property.name.to_uppercase();
        if !matches!(name.as_str(), "DTSTART" | "RRULE" | "RDATE" | "EXDATE") {
            continue;
        }
        let Some(value) = &property.value else { continue };

        let mut line = name;
        for key in ["VALUE", "TZID"] {
            if let Some(found) = param(property.params.as_ref(), key) {
                line.push_str(&format!(";{key}={found}"));
            }
        }
        line.push(':');
        line.push_str(value);
        lines.push(line);
    }

    // DTSTART must come first for `RRuleSet`'s parser.
    lines.sort_by_key(|line| !line.starts_with("DTSTART"));
    if !lines.first().is_some_and(|line| line.starts_with("DTSTART")) {
        return None;
    }

    Some(lines.join("\n"))
}

/// Parse the three `DTSTART`/`DTEND` forms: `20260804` (date), `20260804T180000Z`
/// (UTC), and `20260804T180000` (floating, or in the property's `TZID`).
pub fn parse_ics_datetime(value: &str, params: Option<&IcsParams>) -> Result<EventTime> {
    let is_date =
        param(params, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || value.len() == 8;

    if is_date {
        let date = NaiveDate::parse_from_str(value, "%Y%m%d")
            .with_context(|| format!("invalid ICS date '{value}'"))?;
        return Ok(EventTime::AllDay(date));
    }

    if let Some(utc_value) = value.strip_suffix('Z') {
        let naive = chrono::NaiveDateTime::parse_from_str(utc_value, "%Y%m%dT%H%M%S")
            .with_context(|| format!("invalid ICS UTC datetime '{value}'"))?;
        return Ok(EventTime::Timed(chrono::Utc.from_utc_datetime(&naive).with_timezone(&Local)));
    }

    let naive = chrono::NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S")
        .with_context(|| format!("invalid ICS datetime '{value}'"))?;

    // A TZID we don't recognize is treated as floating local time, which is the
    // same fallback the spec suggests for unknown time zones.
    let tzid = param(params, "TZID").and_then(|tz| tz.parse::<chrono_tz::Tz>().ok());
    let local = match tzid {
        Some(tz) => tz
            .from_local_datetime(&naive)
            .earliest()
            .map(|dt| dt.with_timezone(&Local))
            .ok_or_else(|| anyhow!("ICS datetime '{value}' does not exist in {tz}"))?,
        None => local_datetime(naive.date(), naive.time()),
    };

    Ok(EventTime::Timed(local))
}

/// Undo RFC 5545 text escaping (`\n`, `\,`, `\;`, `\\`).
fn unescape_ics_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(escaped) => out.push(escaped),
            None => out.push('\\'),
        }
    }
    out.trim().to_string()
}

/// Expand events into concrete occurrences starting within
/// `[window_start, window_end]`. Recurring events go through `RRuleSet`;
/// single events pass straight through.
pub fn expand(
    events: &[CalendarEvent],
    window_start: NaiveDate,
    window_end: NaiveDate,
) -> Vec<Occurrence> {
    let mut occurrences = Vec::new();

    for event in events {
        // Length is preserved across occurrences of a recurring event.
        let span = event.end.date() - event.start.date();
        let end_time = match (event.start, event.end) {
            (EventTime::Timed(_), EventTime::Timed(end)) => Some(end.time()),
            _ => None,
        };
        // A timed event shorter than a day belongs to the day it starts on,
        // even if it runs past midnight (9 PM to 1 AM, say).
        let within_a_day = matches!(
            (event.start, event.end),
            (EventTime::Timed(start), EventTime::Timed(end)) if end - start < Duration::days(1)
        );

        let starts: Vec<EventTime> = match &event.recurrence {
            None => vec![event.start],
            Some(block) => match expand_recurrence(block, window_start, window_end) {
                Ok(dates) => dates
                    .into_iter()
                    .map(|dt| match event.start {
                        EventTime::AllDay(_) => EventTime::AllDay(dt.date_naive()),
                        EventTime::Timed(_) => EventTime::Timed(dt),
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!(error=%e, summary=%event.summary, "failed to expand recurrence");
                    continue;
                }
            },
        };

        for start in starts {
            let start_date = start.date();
            if start_date < window_start || start_date > window_end {
                continue;
            }

            // DTEND is exclusive for all-day events, so the last covered day is
            // the day before it.
            let end_date = match event.end {
                EventTime::AllDay(_) => start_date + span - Duration::days(1),
                EventTime::Timed(_) if within_a_day => start_date,
                EventTime::Timed(_) => start_date + span,
            }
            .max(start_date);

            occurrences.push(Occurrence {
                summary: event.summary.clone(),
                location: event.location.clone(),
                start_date,
                start_time: start.time(),
                end_time,
                end_date,
                is_recurring: event.recurrence.is_some() || event.recurring_instance,
            });
        }
    }

    occurrences
}

fn expand_recurrence(
    block: &str,
    window_start: NaiveDate,
    window_end: NaiveDate,
) -> Result<Vec<DateTime<Local>>> {
    let rule_set: RRuleSet =
        block.parse().map_err(|e| anyhow!("failed to parse recurrence rule: {e}"))?;

    // Widen by a day on each side so an occurrence near a timezone boundary
    // isn't clipped before it's converted back to local time.
    let after = local_datetime(window_start - Duration::days(1), NaiveTime::MIN);
    let before = local_datetime(window_end + Duration::days(1), NaiveTime::MIN);

    let result = rule_set
        .after(after.with_timezone(&rrule::Tz::LOCAL))
        .before(before.with_timezone(&rrule::Tz::LOCAL))
        .all(MAX_OCCURRENCES);

    Ok(result.dates.into_iter().map(|dt| dt.with_timezone(&Local)).collect())
}

/// Classify occurrences and render the agenda. Returns `None` when all three
/// sections are empty, so the caller can stay quiet.
pub fn build_agenda(
    occurrences: &[Occurrence],
    today: NaiveDate,
    config: &CalendarAgendaConfig,
) -> Option<String> {
    let min_span = config.multi_day_min_days.max(1);

    // Day 1 only: a multi-day event is announced when it begins, not repeated
    // on every day it spans.
    let mut today_events: Vec<&Occurrence> =
        occurrences.iter().filter(|o| o.start_date == today).collect();
    today_events.sort_by_key(|o| (o.start_time.is_some(), o.start_time, o.summary.clone()));

    let days_until = |o: &Occurrence| (o.start_date - today).num_days();

    let mut countdowns: Vec<(i64, &Occurrence)> = occurrences
        .iter()
        .filter(|o| o.span_days() >= min_span && o.start_date > today)
        .filter_map(|o| {
            let days = days_until(o);
            config.countdown_days.iter().any(|d| i64::from(*d) == days).then_some((days, o))
        })
        .collect();
    countdowns.sort_by_key(|(days, o)| (*days, o.summary.clone()));

    let mut reminders: Vec<(i64, &Occurrence)> = occurrences
        .iter()
        .filter(|o| !o.is_recurring && o.span_days() < min_span && o.start_date > today)
        .filter_map(|o| {
            let days = days_until(o);
            config.reminder_days.iter().any(|d| i64::from(*d) == days).then_some((days, o))
        })
        .collect();
    reminders.sort_by_key(|(days, o)| (*days, o.summary.clone()));

    if today_events.is_empty() && countdowns.is_empty() && reminders.is_empty() {
        return None;
    }

    let mut out = format!("### 📅 {}\n", today.format("%A, %B %-d"));

    if !today_events.is_empty() {
        out.push_str(&format!("\n**{}**\n", config.heading_today));
        for occurrence in today_events {
            out.push_str(&format!("- {}\n", render_today_line(occurrence)));
        }
    }

    if !reminders.is_empty() {
        out.push_str(&format!("\n**{}**\n", config.heading_reminders));
        for (days, occurrence) in reminders {
            out.push_str(&format!(
                "- {} — {} ({})\n",
                occurrence.summary,
                render_days_away(days),
                occurrence.start_date.format("%a, %b %-d")
            ));
        }
    }

    if !countdowns.is_empty() {
        out.push_str(&format!("\n**{}**\n", config.heading_countdowns));
        for (days, occurrence) in countdowns {
            out.push_str(&format!(
                "- {} — {} ({} – {})\n",
                occurrence.summary,
                render_days_away(days),
                occurrence.start_date.format("%b %-d"),
                occurrence.end_date.format("%b %-d")
            ));
        }
    }

    if let Some(link) = &config.calendar_link {
        out.push_str(&format!("\n[{}]({})\n", config.calendar_link_text, link));
    }

    Some(out)
}

fn render_today_line(occurrence: &Occurrence) -> String {
    let when = match (occurrence.start_time, occurrence.end_time) {
        (Some(start), Some(end)) if end != start => {
            format!("{} – {}", render_time(start), render_time(end))
        }
        (Some(start), _) => render_time(start),
        (None, _) => "All day".to_string(),
    };

    let mut line = format!("{when} — {}", occurrence.summary);

    // Multi-day events are listed once, on day 1, so note where they run to.
    if occurrence.end_date > occurrence.start_date {
        line.push_str(&format!(" (through {})", occurrence.end_date.format("%a, %b %-d")));
    }

    if let Some(location) = &occurrence.location {
        line.push_str(&format!(" _({location})_"));
    }

    line
}

fn render_time(time: NaiveTime) -> String {
    if time.format("%M").to_string() == "00" {
        time.format("%-I %p").to_string()
    } else {
        time.format("%-I:%M %p").to_string()
    }
}

fn render_days_away(days: i64) -> String {
    match days {
        1 => "tomorrow".to_string(),
        d => format!("in {d} days"),
    }
}

// Test helpers. These expose scheduling and store internals so the external
// integration tests can drive the middleware without a live feed.
// TODO: move tests in-crate so these can be private.
#[doc(hidden)]
impl CalendarAgenda {
    pub fn test_next_scheduled_time(&self, now: DateTime<Local>) -> DateTime<Local> {
        self.next_scheduled_time(now)
    }

    pub async fn test_deliver_daily_agenda(&self, today: NaiveDate, message: Option<String>) {
        self.deliver_daily_agenda(today, message).await
    }

    pub async fn test_last_posted_date(&self) -> Option<NaiveDate> {
        self.last_posted_date().await
    }

    pub fn test_matches_command(&self, evt: &Event) -> bool {
        self.matches_command(evt)
    }

    pub fn test_config(&self) -> &CalendarAgendaConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::PersistentStore;

    fn config() -> CalendarAgendaConfig {
        CalendarAgendaConfig {
            service_id: "matrix".to_string(),
            room_id: "!room:example.com".to_string(),
            calendar_url: SecretString::from("https://calendar.example.com/feed.ics"),
            calendar_link: None,
            calendar_link_text: "View the full calendar".to_string(),
            post_at_time: NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
            countdown_days: vec![30, 14],
            reminder_days: vec![7, 1],
            multi_day_min_days: 2,
            heading_today: "Today".to_string(),
            heading_reminders: "Coming up".to_string(),
            heading_countdowns: "Countdowns".to_string(),
            command_string: None,
        }
    }

    fn agenda() -> CalendarAgenda {
        let (cmd_tx, _commands) = tokio::sync::mpsc::channel(1);
        let ctx = MiddlewareContext { cmd_tx, store: Arc::new(PersistentStore::in_memory()) };
        CalendarAgenda::new(ctx, config())
    }

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, day).unwrap()
    }

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        local_datetime(date(day), NaiveTime::from_hms_opt(hour, minute, 0).unwrap())
    }

    fn timed_event(start: DateTime<Local>, end: DateTime<Local>) -> CalendarEvent {
        CalendarEvent {
            summary: "Party".to_string(),
            location: None,
            start: EventTime::Timed(start),
            end: EventTime::Timed(end),
            recurrence: None,
            recurring_instance: false,
        }
    }

    #[test]
    fn evening_event_ending_after_midnight_is_a_single_day() {
        let events = [timed_event(at(10, 21, 0), at(11, 1, 0))];

        let occurrences = expand(&events, date(1), date(30));

        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].start_date, date(10));
        assert_eq!(occurrences[0].end_date, date(10));
        assert_eq!(occurrences[0].span_days(), 1);
    }

    #[test]
    fn overnight_event_is_reminded_about_and_not_counted_down_to() {
        let events = [timed_event(at(10, 21, 0), at(11, 1, 0))];
        let occurrences = expand(&events, date(1), date(30));

        // A week ahead: a reminder interval. It is listed under reminders.
        let week_before = build_agenda(&occurrences, date(3), &config()).expect("a reminder");
        assert!(week_before.contains("Coming up"), "got: {week_before}");
        assert!(!week_before.contains("Countdowns"), "got: {week_before}");

        // On the day, it is not described as running "through" the next day.
        let on_the_day = build_agenda(&occurrences, date(10), &config()).expect("today's agenda");
        assert!(on_the_day.contains("9 PM – 1 AM — Party"), "got: {on_the_day}");
        assert!(!on_the_day.contains("through"), "got: {on_the_day}");
    }

    #[test]
    fn timed_event_of_a_day_or_longer_still_spans_its_days() {
        let events = [timed_event(at(10, 21, 0), at(11, 22, 0))];

        let occurrences = expand(&events, date(1), date(30));

        assert_eq!(occurrences[0].end_date, date(11));
        assert_eq!(occurrences[0].span_days(), 2);
    }

    #[test]
    fn agenda_is_due_once_a_day_after_the_post_time() {
        let agenda = agenda();

        assert!(!agenda.is_due(at(10, 7, 59), None), "before the post time");
        assert!(agenda.is_due(at(10, 8, 0), None), "at the post time");
        assert!(agenda.is_due(at(10, 15, 0), Some(date(9))), "later, not yet posted today");
        assert!(!agenda.is_due(at(10, 15, 0), Some(date(10))), "already posted today");
        // After a day of failed attempts: yesterday's agenda is given up on.
        assert!(!agenda.is_due(at(11, 0, 3), Some(date(9))), "a new day, before its post time");
    }

    #[test]
    fn pending_retry_is_woken_for_before_the_next_scheduled_post() {
        let agenda = agenda();
        let now = at(10, 8, 0);

        assert_eq!(agenda.next_wake(now, None), at(11, 8, 0));
        assert_eq!(agenda.next_wake(now, Some(at(10, 8, 5))), at(10, 8, 5));
        // A retry that would fall after the next scheduled post does not delay it.
        assert_eq!(agenda.next_wake(now, Some(at(11, 9, 0))), at(11, 8, 0));
    }

    fn feed(events: &[&str]) -> String {
        let events: String = events
            .iter()
            .map(|lines| {
                format!("BEGIN:VEVENT\r\n{}\r\nEND:VEVENT\r\n", lines.replace('\n', "\r\n"))
            })
            .collect();
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n{events}END:VCALENDAR\r\n")
    }

    /// Start dates and summaries of everything in the feed during June 2026.
    fn june(feed: &str) -> Vec<(NaiveDate, String)> {
        let events = parse_calendar(feed).expect("the feed should parse");
        let mut found: Vec<_> = expand(&events, date(1), date(30))
            .into_iter()
            .map(|o| (o.start_date, o.summary))
            .collect();
        found.sort();
        found
    }

    /// Every Monday in June 2026, all day.
    const WEEKLY: &str = "UID:weekly\nSUMMARY:Weekly sync\nDTSTART;VALUE=DATE:20260601\n\
                          DTEND;VALUE=DATE:20260602\nRRULE:FREQ=WEEKLY;COUNT=4";

    #[test]
    fn moved_instance_is_listed_once_at_its_new_time() {
        let moved = "UID:weekly\nSUMMARY:Weekly sync (moved)\nRECURRENCE-ID;VALUE=DATE:20260608\n\
                     DTSTART;VALUE=DATE:20260610\nDTEND;VALUE=DATE:20260611";

        assert_eq!(
            june(&feed(&[WEEKLY, moved])),
            [
                (date(1), "Weekly sync".to_string()),
                (date(10), "Weekly sync (moved)".to_string()),
                (date(15), "Weekly sync".to_string()),
                (date(22), "Weekly sync".to_string()),
            ]
        );
    }

    #[test]
    fn moved_instance_listed_before_its_series_is_still_matched() {
        let moved = "UID:weekly\nSUMMARY:Moved\nRECURRENCE-ID;VALUE=DATE:20260608\n\
                     DTSTART;VALUE=DATE:20260610\nDTEND;VALUE=DATE:20260611";

        let found = june(&feed(&[moved, WEEKLY]));

        assert_eq!(found.len(), 4);
        assert!(!found.iter().any(|(day, _)| *day == date(8)), "got: {found:?}");
    }

    #[test]
    fn moved_instance_of_a_timed_event_in_a_named_zone_is_matched() {
        let series = "UID:games\nSUMMARY:Game night\n\
                      DTSTART;TZID=America/Los_Angeles:20260602T190000\n\
                      DTEND;TZID=America/Los_Angeles:20260602T210000\nRRULE:FREQ=WEEKLY;COUNT=3";
        let moved = "UID:games\nSUMMARY:Game night (moved)\n\
                     RECURRENCE-ID;TZID=America/Los_Angeles:20260609T190000\n\
                     DTSTART;TZID=America/Los_Angeles:20260610T190000\n\
                     DTEND;TZID=America/Los_Angeles:20260610T210000";

        let found = june(&feed(&[series, moved]));

        // Two left in the series, plus the moved one: not four.
        assert_eq!(found.len(), 3, "got: {found:?}");
        assert_eq!(found.iter().filter(|(_, summary)| summary == "Game night").count(), 2);
    }

    #[test]
    fn moved_instance_counts_as_recurring() {
        let moved = "UID:weekly\nSUMMARY:Moved\nRECURRENCE-ID;VALUE=DATE:20260608\n\
                     DTSTART;VALUE=DATE:20260610\nDTEND;VALUE=DATE:20260611";
        let events = parse_calendar(&feed(&[WEEKLY, moved])).unwrap();

        // So it is not announced with a reminder, like the rest of its series.
        assert!(expand(&events, date(1), date(30)).iter().all(|o| o.is_recurring));
    }

    #[test]
    fn cancelled_instance_is_dropped() {
        let cancelled = "UID:weekly\nSUMMARY:Weekly sync\nSTATUS:CANCELLED\n\
                         RECURRENCE-ID;VALUE=DATE:20260608\n\
                         DTSTART;VALUE=DATE:20260608\nDTEND;VALUE=DATE:20260609";

        let days: Vec<NaiveDate> =
            june(&feed(&[WEEKLY, cancelled])).into_iter().map(|(day, _)| day).collect();

        assert_eq!(days, [date(1), date(15), date(22)]);
    }

    #[test]
    fn cancelled_events_are_dropped() {
        let single = "UID:picnic\nSUMMARY:Picnic\nSTATUS:CANCELLED\n\
                      DTSTART;VALUE=DATE:20260605\nDTEND;VALUE=DATE:20260606";
        let series = format!("{WEEKLY}\nSTATUS:CANCELLED");
        let kept = "UID:dinner\nSUMMARY:Dinner\nSTATUS:CONFIRMED\n\
                    DTSTART;VALUE=DATE:20260606\nDTEND;VALUE=DATE:20260607";

        assert_eq!(june(&feed(&[single, &series, kept])), [(date(6), "Dinner".to_string())]);
    }

    #[test]
    fn instance_of_another_event_leaves_a_series_alone() {
        let other = "UID:something-else\nSUMMARY:Other\nRECURRENCE-ID;VALUE=DATE:20260608\n\
                     DTSTART;VALUE=DATE:20260610\nDTEND;VALUE=DATE:20260611";

        assert_eq!(june(&feed(&[WEEKLY, other])).len(), 5);
    }
}
