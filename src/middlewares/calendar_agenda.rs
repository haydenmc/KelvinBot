use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, MiddlewareContext, Verdict},
    service::ServiceId,
};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Local, NaiveDate, NaiveTime, TimeZone};
use ical::parser::ical::component::IcalEvent;
use rrule::RRuleSet;
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

pub struct CalendarAgendaConfig {
    pub service_id: String,
    pub room_id: String,
    /// ICS feed URL. Private to the bot: never rendered into a message.
    pub calendar_url: String,
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
        let body = fetch_calendar(&self.config.calendar_url).await?;
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
    async fn post_daily_agenda(&self, today: NaiveDate) {
        match self.build_message(today).await {
            Ok(message) => self.deliver_daily_agenda(today, message).await,
            Err(e) => {
                // Leave last_posted_date alone so the next tick retries.
                tracing::error!(error=%e, date=%today, "failed to build daily calendar agenda");
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
            "calendar_agenda middleware running"
        );

        // Catch up if the bot was down (or restarted) past today's post time.
        let now = Local::now();
        let today = now.date_naive();
        if now.time() >= self.config.post_at_time
            && self.last_posted_date().await.is_none_or(|last| last < today)
        {
            tracing::info!(date=%today, "posting missed daily calendar agenda on startup");
            self.post_daily_agenda(today).await;
        }

        loop {
            let now = Local::now();
            let next_time = self.next_scheduled_time(now);
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
                _ = tokio::time::sleep(duration_until) => {
                    let today = Local::now().date_naive();
                    if self.last_posted_date().await.is_some_and(|last| last >= today) {
                        tracing::debug!(date=%today, "agenda already posted today; skipping");
                        // Nudge past the scheduled instant so the next loop
                        // iteration targets tomorrow rather than spinning.
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                    self.post_daily_agenda(today).await;
                }
                Some(()) = query_rx.recv() => {
                    self.post_on_demand(Local::now().date_naive()).await;
                }
            }
        }

        Ok(())
    }

    fn on_event(&self, evt: &Event) -> Result<Verdict> {
        if self.matches_command(evt) {
            // Use try_send so a full queue can't block the event pipeline.
            if let Err(e) = self.query_tx.try_send(()) {
                tracing::warn!(error=?e, "failed to queue on-demand agenda request");
            }
        }

        Ok(Verdict::Continue)
    }
}

/// Resolve a local date and time, picking the earlier instant when a DST
/// transition makes it ambiguous and the following hour when it doesn't exist.
fn local_datetime(date: NaiveDate, time: NaiveTime) -> DateTime<Local> {
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
    let response = client.get(&url).send().await.context("failed to fetch calendar feed")?;
    let status = response.status();
    if !status.is_success() {
        // Deliberately does not include the URL: the feed is a secret.
        return Err(anyhow!("calendar feed returned HTTP {status}"));
    }
    response.text().await.context("failed to read calendar feed body")
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
pub fn parse_calendar(body: &str) -> Result<Vec<CalendarEvent>> {
    let reader = ical::IcalParser::new(BufReader::new(body.as_bytes()));
    let mut events = Vec::new();

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

    Ok(events)
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

fn parse_event(event: &IcalEvent) -> Result<Option<CalendarEvent>> {
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

    Ok(Some(CalendarEvent {
        summary,
        location,
        start,
        end,
        recurrence: build_recurrence_block(event),
    }))
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
                is_recurring: event.recurrence.is_some(),
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
