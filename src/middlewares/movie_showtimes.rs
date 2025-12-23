use crate::core::{
    bus::Command,
    event::Event,
    middleware::{Middleware, Verdict},
    service::ServiceId,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Weekday};
use serde::Deserialize;
use serde_with::{DisplayFromStr, serde_as};
use std::collections::HashMap;
use tokio::sync::mpsc::Sender;
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
#[derive(Debug)]
struct MovieListing {
    title: String,
    year: Option<u16>,
    rating: Option<String>,
    runtime: Option<String>,
    primary_theater: TheaterShowtimes,
    other_theaters: Vec<String>, // Just theater names, no showtimes
}

#[derive(Debug)]
struct TheaterShowtimes {
    name: String,
    times_by_day: HashMap<NaiveDate, Vec<String>>, // Date -> times on that date
}

pub struct MovieShowtimes {
    cmd_tx: Sender<Command>,
    service_id: String,
    room_id: String,
    post_on_day_of_week: Weekday,
    post_at_time: NaiveTime,
    search_location: LatLng,
    search_radius_mi: u16,
    gracenote_api_key: String,
    theater_id_filter: Option<Vec<String>>,
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
    ) -> Self {
        Self {
            cmd_tx,
            service_id,
            room_id,
            post_on_day_of_week,
            post_at_time,
            search_location,
            search_radius_mi,
            gracenote_api_key,
            theater_id_filter,
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

    /// Fetch movie showtimes from TMS API
    async fn fetch_showtimes(&self) -> Result<String> {
        tracing::info!("fetching movie showtimes from TMS API");

        // Build API request URL
        let today = Local::now().format("%Y-%m-%d").to_string();
        let url = format!(
            "http://data.tmsapi.com/v1.1/movies/showings?api_key={}&lat={}&lng={}&radius={}&units=mi&startDate={}&numDays=7",
            self.gracenote_api_key,
            self.search_location.lat,
            self.search_location.lng,
            self.search_radius_mi,
            today
        );

        // Fetch data from API
        let client = reqwest::Client::new();
        let response =
            client.get(&url).send().await.context("failed to send request to TMS API")?;

        if !response.status().is_success() {
            anyhow::bail!("TMS API returned error: {}", response.status());
        }

        let movies: Vec<TmsMovie> =
            response.json().await.context("failed to parse TMS API response")?;

        tracing::info!(movie_count = movies.len(), "received movies from API");

        // Process and filter movies
        let listings = self.process_movies(movies);

        // Format into readable message
        self.format_message(listings)
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
                    self.theater_id_filter
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

    /// Format movie listings into a readable message with markdown
    fn format_message(&self, mut listings: Vec<MovieListing>) -> Result<String> {
        if listings.is_empty() {
            return Ok("🎬 No movies found in your priority theaters this week.".to_string());
        }

        // Sort movies by title
        listings.sort_by(|a, b| a.title.cmp(&b.title));

        let mut message = String::from("# 🎬 Movie Showtimes This Week\n\n");

        for listing in listings {
            // Movie header with title, year, rating, runtime
            message.push_str(&format!("## 📽️ {}", listing.title));

            let mut metadata = Vec::new();
            if let Some(year) = listing.year {
                metadata.push(year.to_string());
            }
            if let Some(rating) = listing.rating {
                metadata.push(rating);
            }
            if let Some(runtime) = listing.runtime {
                metadata.push(runtime);
            }

            if !metadata.is_empty() {
                message.push_str(&format!(" *({})*", metadata.join(" • ")));
            }
            message.push_str("\n\n");

            // Show primary theater with detailed showtimes grouped by day
            message.push_str(&format!("**🎭 {}**\n\n", listing.primary_theater.name));

            // Sort days chronologically
            let mut days: Vec<_> = listing.primary_theater.times_by_day.into_iter().collect();
            days.sort_by_key(|(date, _)| *date);

            for (date, times) in days {
                let day_label = date.format("%a %b %-d").to_string();
                message.push_str(&format!("- **{}**: {}\n", day_label, times.join(", ")));
            }

            // Show other theaters if any
            if !listing.other_theaters.is_empty() {
                message.push_str(&format!(
                    "\n*Also showing at: {}*\n",
                    listing.other_theaters.join(", ")
                ));
            }

            message.push_str("\n---\n\n");
        }

        Ok(message)
    }

    /// Post showtimes to the configured room
    async fn post_showtimes(&self) {
        tracing::info!(
            service_id=%self.service_id,
            room_id=%self.room_id,
            "posting scheduled showtimes"
        );

        match self.fetch_showtimes().await {
            Ok(message) => {
                let command = Command::SendRoomMessage {
                    service_id: ServiceId(self.service_id.clone()),
                    room_id: self.room_id.clone(),
                    body: message.clone(),
                    markdown_body: Some(message),
                    response_tx: None,
                };

                let cmd_tx = self.cmd_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = cmd_tx.send(command).await {
                        tracing::error!(error=%e, "failed to send showtimes message");
                    }
                });
            }
            Err(e) => {
                tracing::error!(error=%e, "failed to fetch showtimes");

                // Optionally send error message to room
                let error_msg = format!("Failed to fetch showtimes: {}", e);
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
        }
    }
}

#[async_trait]
impl Middleware for MovieShowtimes {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        tracing::info!(
            post_on_day_of_week=?self.post_on_day_of_week,
            time=%self.post_at_time,
            "movie_showtimes middleware running..."
        );

        loop {
            let next_time = self.next_scheduled_time();
            let now = Local::now();
            let duration_until =
                (next_time - now).to_std().unwrap_or(std::time::Duration::from_secs(0));

            tracing::info!(
                next_scheduled=%next_time.format("%Y-%m-%d %H:%M:%S %Z"),
                seconds_until=%duration_until.as_secs(),
                "waiting for next scheduled post"
            );

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("movie_showtimes middleware shutting down...");
                    break;
                }
                _ = tokio::time::sleep(duration_until) => {
                    // Post showtimes
                    self.post_showtimes().await;

                    // Wait a bit to avoid posting multiple times if the clock is adjusted
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                }
            }
        }

        Ok(())
    }

    fn on_event(&self, _evt: &Event) -> Result<Verdict> {
        // This middleware doesn't respond to events, only posts on schedule
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
