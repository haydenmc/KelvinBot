use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, Verdict},
    service::ServiceId,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday};
use serde::Deserialize;
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};
use tokio_util::sync::CancellationToken;

#[serde_as]
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct LatLng {
    #[serde_as(as = "DisplayFromStr")]
    pub lat: f64,
    #[serde_as(as = "DisplayFromStr")]
    pub lng: f64,
}

// TMS API response structures
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TmsMovie {
    // tms_id: String, // Unused
    title: String,
    release_year: Option<u16>,
    // genres: Option<Vec<String>>, // Unused
    ratings: Option<Vec<TmsRating>>,
    run_time: Option<String>, // ISO 8601 duration like "PT02H00M"
    showtimes: Vec<TmsShowtime>,
}

#[derive(Debug, Deserialize)]
struct TmsRating {
    body: String,
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TmsShowtime {
    theatre: TmsTheatre,
    date_time: String, // ISO 8601 datetime
                       // barg: Option<bool>, // Unused
                       // ticket_uri: Option<String>, // Unused
}

#[derive(Debug, Deserialize)]
struct TmsTheatre {
    id: String,
    name: String,
}

// Processed movie data for display
#[derive(Debug, Clone)]
struct MovieListing {
    title: String,
    year: Option<u16>,
    rating: Option<String>,
    runtime: Option<String>,
    primary_theater: TheaterShowtimes,
    other_theaters: Vec<String>, // Just theater names, no showtimes
}

#[derive(Debug, Clone)]
struct TheaterShowtimes {
    name: String,
    times_by_day: HashMap<NaiveDate, Vec<String>>, // Date -> times on that date
}

// Configuration needed for fetching movie data
struct MovieFetchConfig {
    search_location: LatLng,
    search_radius_mi: u16,
    gracenote_api_key: String,
    theater_id_filter: Option<Vec<String>>,
}

pub struct MovieShowtimes {
    cmd_tx: Sender<Command>,
    service_id: String,
    room_id: String,
    post_on_day_of_week: Weekday,
    post_at_time: NaiveTime,
    fetch_config: Arc<MovieFetchConfig>,
    cache: Arc<Mutex<Option<Vec<MovieListing>>>>,
    command_string: String,
    query_tx: tokio::sync::mpsc::Sender<String>,
    query_rx: Arc<Mutex<tokio::sync::mpsc::Receiver<String>>>,
}

impl MovieShowtimes {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cmd_tx: Sender<Command>,
        service_id: String,
        room_id: String,
        post_on_day_of_week: Weekday,
        post_at_time: NaiveTime,
        search_location: LatLng,
        search_radius_mi: u16,
        gracenote_api_key: String,
        theater_id_filter: Option<Vec<String>>,
        command_string: Option<String>,
    ) -> Self {
        let (query_tx, query_rx) = tokio::sync::mpsc::channel(100);

        Self {
            cmd_tx,
            service_id,
            room_id,
            post_on_day_of_week,
            post_at_time,
            fetch_config: Arc::new(MovieFetchConfig {
                search_location,
                search_radius_mi,
                gracenote_api_key,
                theater_id_filter,
            }),
            cache: Arc::new(Mutex::new(None)),
            command_string: command_string.unwrap_or_else(|| "!movie".to_string()),
            query_tx,
            query_rx: Arc::new(Mutex::new(query_rx)),
        }
    }

    /// Calculate the next scheduled time based on post_on_day_of_week and post_at_time
    fn next_scheduled_time(&self) -> chrono::DateTime<Local> {
        let now = Local::now();
        let target_time = self.post_at_time;

        // Calculate days until next occurrence
        let current_weekday = now.weekday();
        let target_weekday = self.post_on_day_of_week;

        let days_until_target = if current_weekday == target_weekday {
            // Same day - check if time has passed
            let now_time = now.time();
            if now_time < target_time {
                0 // Today, but later
            } else {
                7 // Next week
            }
        } else {
            // Different day - calculate days forward
            let current_num = current_weekday.number_from_monday();
            let target_num = target_weekday.number_from_monday();

            if target_num > current_num {
                target_num - current_num
            } else {
                7 - (current_num - target_num)
            }
        };

        // Create target datetime
        let target_date = now.date_naive() + Duration::days(days_until_target as i64);
        let target_datetime = target_date.and_time(target_time);

        Local.from_local_datetime(&target_datetime).unwrap()
    }

    /// Process API movies into grouped listings, applying theater priority filter
    fn process_movies(&self, movies: Vec<TmsMovie>) -> Vec<MovieListing> {
        movies
            .into_iter()
            .filter_map(|movie| {
                // Group all showtimes by theater first
                let mut theaters_map: HashMap<String, TheaterShowtimes> = HashMap::new();
                for showtime in movie.showtimes {
                    let theater_id = showtime.theatre.id.clone();
                    let theater_name = showtime.theatre.name.clone();

                    // Parse datetime
                    if let Ok(dt) =
                        NaiveDateTime::parse_from_str(&showtime.date_time, "%Y-%m-%dT%H:%M")
                    {
                        let date = dt.date();
                        let time_str = dt.format("%l:%M %p").to_string().trim().to_string();

                        let theater_entry = theaters_map.entry(theater_id).or_insert_with(|| {
                            TheaterShowtimes { name: theater_name, times_by_day: HashMap::new() }
                        });

                        theater_entry.times_by_day.entry(date).or_default().push(time_str);
                    }
                }

                // If theater priority filter is set, find first priority theater with showtimes
                let (primary_theater, other_theaters) = if let Some(ref priority_list) =
                    self.fetch_config.theater_id_filter
                {
                    // Find first priority theater that has this movie
                    let primary =
                        priority_list.iter().find_map(|theater_id| theaters_map.remove(theater_id));

                    // If no priority theater has it, skip this movie
                    let primary = primary?;

                    // Collect remaining priority theaters only (in priority order)
                    let others: Vec<String> = priority_list
                        .iter()
                        .filter_map(|theater_id| {
                            theaters_map.get(theater_id).map(|t| t.name.clone())
                        })
                        .collect();

                    (primary, others)
                } else {
                    // No filter - pick first theater alphabetically as primary
                    let mut theater_entries: Vec<_> = theaters_map.into_iter().collect();
                    theater_entries.sort_by(|a, b| a.1.name.cmp(&b.1.name));

                    if theater_entries.is_empty() {
                        return None;
                    }

                    let primary = theater_entries.remove(0).1;
                    let others: Vec<String> =
                        theater_entries.into_iter().map(|(_, t)| t.name).collect();

                    (primary, others)
                };

                // Extract rating
                let rating = movie
                    .ratings
                    .and_then(|r| {
                        r.into_iter().find(|rating| rating.body.contains("Motion Picture"))
                    })
                    .map(|r| r.code);

                // Parse runtime
                let runtime = movie.run_time.as_ref().and_then(|rt| parse_runtime(rt));

                Some(MovieListing {
                    title: movie.title,
                    year: movie.release_year,
                    rating,
                    runtime,
                    primary_theater,
                    other_theaters,
                })
            })
            .collect()
    }

    /// Format a brief summary of all movies for the main channel
    fn format_summary(&self, listings: &[MovieListing]) -> Result<String> {
        if listings.is_empty() {
            return Ok("🎬 No movies found in your priority theaters this week.".to_string());
        }

        let mut message = String::from("# 🎬 Movie Showtimes This Week\n\n");
        message.push_str(&format!("**{}** movies showing:\n\n", listings.len()));

        for listing in listings {
            // Just movie title and key metadata
            message.push_str(&format!("📽️ **{}**", listing.title));

            let mut metadata = Vec::new();
            if let Some(year) = listing.year {
                metadata.push(year.to_string());
            }
            if let Some(ref rating) = listing.rating {
                metadata.push(rating.clone());
            }

            if !metadata.is_empty() {
                message.push_str(&format!(" *({})*", metadata.join(" • ")));
            }
            message.push('\n');
        }

        message.push_str(&format!(
            "\n*To see detailed showtimes, use: {} <movie title>*",
            self.command_string
        ));
        Ok(message)
    }

    /// Fetch and process movie showtimes from TMS API, returning the movie listings
    async fn fetch_and_process_showtimes(&self) -> Result<Vec<MovieListing>> {
        tracing::info!("fetching movie showtimes from TMS API");

        // Build API request (same as before)
        let today = Local::now().format("%Y-%m-%d").to_string();
        let url = format!(
            "http://data.tmsapi.com/v1.1/movies/showings?api_key={}&lat={}&lng={}&radius={}&units=mi&startDate={}&numDays=7",
            self.fetch_config.gracenote_api_key,
            self.fetch_config.search_location.lat,
            self.fetch_config.search_location.lng,
            self.fetch_config.search_radius_mi,
            today
        );

        // Fetch from API
        let client = reqwest::Client::new();
        let response =
            client.get(&url).send().await.context("failed to send request to TMS API")?;

        if !response.status().is_success() {
            anyhow::bail!("TMS API returned error: {}", response.status());
        }

        let movies: Vec<TmsMovie> =
            response.json().await.context("failed to parse TMS API response")?;
        tracing::info!(movie_count = movies.len(), "received movies from API");

        // Process and filter
        let mut listings = self.process_movies(movies);
        // Sort movies by title for consistent ordering
        listings.sort_by(|a, b| a.title.cmp(&b.title));

        Ok(listings)
    }

    /// Get cached listings or fetch fresh if cache is empty
    async fn get_or_fetch_listings(&self) -> Result<Vec<MovieListing>> {
        // Try cache first
        {
            let cache = self.cache.lock().await;
            if let Some(ref listings) = *cache {
                tracing::debug!("using cached listings ({} movies)", listings.len());
                return Ok(listings.clone());
            }
        }

        // Cache miss - fetch fresh data
        tracing::info!("cache empty, fetching fresh showtimes");
        let listings = self.fetch_and_process_showtimes().await?;

        // Update cache
        {
            let mut cache = self.cache.lock().await;
            *cache = Some(listings.clone());
        }

        Ok(listings)
    }

    /// Find a movie by title query (case-insensitive partial match)
    fn find_movie_by_query<'a>(
        listings: &'a [MovieListing],
        query: &str,
    ) -> Option<&'a MovieListing> {
        let query_lower = query.to_lowercase();

        // First try exact match (case-insensitive)
        let exact_match =
            listings.iter().find(|listing| listing.title.to_lowercase() == query_lower);

        if exact_match.is_some() {
            return exact_match;
        }

        // Then try contains match
        listings.iter().find(|listing| listing.title.to_lowercase().contains(&query_lower))
    }

    /// Handle a movie query from a user in the configured room
    async fn handle_movie_query(&self, query: &str) {
        // Get cached or fresh listings
        let listings = match self.get_or_fetch_listings().await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error=%e, "failed to fetch movie listings");
                self.send_room_response(
                    "Failed to fetch movie showtimes. Please try again later.".to_string(),
                    None,
                )
                .await;
                return;
            }
        };

        // Search for movie
        match Self::find_movie_by_query(&listings, query) {
            Some(movie) => {
                // Found - send detailed showtimes
                if let Ok(detail) = Self::format_movie_detail_static(movie) {
                    self.send_room_response(detail.clone(), Some(detail)).await;
                }
            }
            None => {
                // Not found - send helpful message (no movie list, too many to display)
                let message = format!(
                    "Movie '{}' not found in cached listings. Check the most recent summary for available movies.",
                    query
                );

                self.send_room_response(message, None).await;
            }
        }
    }

    /// Format detailed showtimes for a single movie (helper function)
    fn format_movie_detail_static(listing: &MovieListing) -> Result<String> {
        let mut message = String::new();

        message.push_str(&format!("## 📽️ {}", listing.title));

        let mut metadata = Vec::new();
        if let Some(year) = listing.year {
            metadata.push(year.to_string());
        }
        if let Some(ref rating) = listing.rating {
            metadata.push(rating.clone());
        }
        if let Some(ref runtime) = listing.runtime {
            metadata.push(runtime.clone());
        }

        if !metadata.is_empty() {
            message.push_str(&format!(" *({})*", metadata.join(" • ")));
        }
        message.push_str("\n\n");

        message.push_str(&format!("**🎭 {}**\n\n", listing.primary_theater.name));

        let mut days: Vec<_> = listing.primary_theater.times_by_day.iter().collect();
        days.sort_by_key(|(date, _)| *date);

        for (date, times) in days {
            let day_label = date.format("%a %b %-d").to_string();
            message.push_str(&format!("- **{}**: {}\n", day_label, times.join(", ")));
        }

        if !listing.other_theaters.is_empty() {
            message
                .push_str(&format!("\n*Also showing at: {}*\n", listing.other_theaters.join(", ")));
        }

        Ok(message)
    }

    /// Send a response message to the configured room
    async fn send_room_response(&self, body: String, markdown_body: Option<String>) {
        let command = Command::SendRoomMessage {
            service_id: ServiceId(self.service_id.clone()),
            room_id: self.room_id.clone(),
            body,
            markdown_body,
            response_tx: None,
        };

        if let Err(e) = self.cmd_tx.send(command).await {
            tracing::error!(error=%e, "failed to send movie query response");
        }
    }

    /// Send help message when !movie is called without arguments
    async fn send_help_message(&self) {
        let message = {
            let cache_guard = self.cache.lock().await;
            if cache_guard.is_some() {
                format!(
                    "**Movie Showtimes Help**\n\nUsage: `{} <movie title>`\n\nCheck the most recent summary for available movies.",
                    self.command_string
                )
            } else {
                format!(
                    "**Movie Showtimes Help**\n\nUsage: `{} <movie title>`\n\nNo showtimes cached yet. Check back after the next scheduled update.",
                    self.command_string
                )
            }
        };

        self.send_room_response(message.clone(), Some(message)).await;
    }

    /// Send an error message to the room
    async fn send_error_message(&self, error_msg: String) {
        let command = Command::SendRoomMessage {
            service_id: ServiceId(self.service_id.clone()),
            room_id: self.room_id.clone(),
            body: error_msg,
            markdown_body: None,
            response_tx: None,
        };

        let cmd_tx = self.cmd_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = cmd_tx.send(command).await {
                tracing::error!(error=%e, "failed to send error message");
            }
        });
    }

    /// Post showtimes summary to the configured room
    async fn post_showtimes(&self) {
        tracing::info!(
            service_id=%self.service_id,
            room_id=%self.room_id,
            "posting scheduled showtimes"
        );

        // Fetch and process movie listings
        let listings = match self.fetch_and_process_showtimes().await {
            Ok(listings) => listings,
            Err(e) => {
                tracing::error!(error=%e, "failed to fetch showtimes");
                self.send_error_message(format!("Failed to fetch showtimes: {}", e)).await;
                return;
            }
        };

        // Cache the listings
        {
            let mut cache = self.cache.lock().await;
            *cache = Some(listings.clone());
            tracing::debug!("cached {} movie listings", listings.len());
        }

        // Format and send summary message (no thread posting)
        let summary = match self.format_summary(&listings) {
            Ok(msg) => msg,
            Err(e) => {
                tracing::error!(error=%e, "failed to format summary");
                return;
            }
        };

        let command = Command::SendRoomMessage {
            service_id: ServiceId(self.service_id.clone()),
            room_id: self.room_id.clone(),
            body: summary.clone(),
            markdown_body: Some(summary),
            response_tx: None,
        };

        if let Err(e) = self.cmd_tx.send(command).await {
            tracing::error!(error=%e, "failed to send summary message");
        }

        tracing::info!("finished posting movie showtimes summary");
    }
}

#[async_trait]
impl Middleware for MovieShowtimes {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let mut query_rx = self.query_rx.lock().await;
        let mut next_time = self.next_scheduled_time();
        let mut in_cooldown = false;

        tracing::info!(
            post_on_day_of_week=?self.post_on_day_of_week,
            time=%self.post_at_time,
            next_scheduled=%next_time.format("%Y-%m-%d %H:%M:%S %Z"),
            "movie_showtimes middleware running"
        );

        loop {
            let now = Local::now();
            let duration_until = if in_cooldown {
                // Short cooldown to avoid re-posting if time calculation is slightly off
                std::time::Duration::from_secs(2)
            } else {
                (next_time - now).to_std().unwrap_or(std::time::Duration::from_secs(0))
            };

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("movie_showtimes middleware shutting down...");
                    break;
                }
                _ = tokio::time::sleep(duration_until) => {
                    if in_cooldown {
                        // Cooldown finished, recalculate next scheduled time
                        in_cooldown = false;
                        next_time = self.next_scheduled_time();
                        tracing::info!(
                            next_scheduled=%next_time.format("%Y-%m-%d %H:%M:%S %Z"),
                            "next scheduled post"
                        );
                    } else {
                        // Time for scheduled post
                        self.post_showtimes().await;
                        in_cooldown = true;
                    }
                }
                Some(query) = query_rx.recv() => {
                    // Process movie query - not blocked by cooldown!
                    if query.is_empty() {
                        self.send_help_message().await;
                    } else {
                        self.handle_movie_query(&query).await;
                    }
                }
            }
        }

        Ok(())
    }

    fn on_event(&self, evt: &Event) -> Result<Verdict> {
        // Only handle room messages in the configured room
        let (room_id, body, is_self) = match &evt.kind {
            EventKind::RoomMessage { room_id, body, is_self, .. } => (room_id, body, is_self),
            _ => return Ok(Verdict::Continue),
        };

        // Only respond in the configured room
        if room_id != &self.room_id {
            return Ok(Verdict::Continue);
        }

        // Ignore messages from self
        if *is_self {
            return Ok(Verdict::Continue);
        }

        // Check for movie command
        let prefix = format!("{} ", self.command_string);
        if let Some(query) = body.strip_prefix(&prefix) {
            let query = query.trim().to_string();

            // Queue the query for async processing
            // Use try_send to avoid blocking if the channel is full
            if let Err(e) = self.query_tx.try_send(query) {
                tracing::warn!(error=?e, "failed to queue movie query");
            }
        }

        Ok(Verdict::Continue)
    }
}

/// Parse ISO 8601 duration like "PT02H00M" into readable format
fn parse_runtime(duration: &str) -> Option<String> {
    let duration = duration.strip_prefix("PT")?;

    let hours = if let Some(h_pos) = duration.find('H') {
        duration[..h_pos].parse::<u32>().ok()?
    } else {
        0
    };

    let minutes = if let Some(m_pos) = duration.find('M') {
        let start = duration.find('H').map(|p| p + 1).unwrap_or(0);
        duration[start..m_pos].parse::<u32>().ok()?
    } else {
        0
    };

    if hours > 0 && minutes > 0 {
        Some(format!("{}h {}m", hours, minutes))
    } else if hours > 0 {
        Some(format!("{}h", hours))
    } else if minutes > 0 {
        Some(format!("{}m", minutes))
    } else {
        None
    }
}
