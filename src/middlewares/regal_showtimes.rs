use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, Verdict},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Datelike, Duration, Local, NaiveTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

pub struct RegalShowtimes {
    cmd_tx: Sender<Command>,
    service_id: String,
    room_id: String,
    day_of_week: Weekday,
    time: NaiveTime,
    theater_id: String,
}

impl RegalShowtimes {
    pub fn new(
        cmd_tx: Sender<Command>,
        service_id: String,
        room_id: String,
        day_of_week: Weekday,
        time: NaiveTime,
        theater_id: String,
    ) -> Self {
        Self {
            cmd_tx,
            service_id,
            room_id,
            day_of_week,
            time,
            theater_id,
        }
    }

    /// Calculate the next scheduled time based on day_of_week and time
    fn next_scheduled_time(&self) -> chrono::DateTime<Local> {
        let now = Local::now();
        let target_time = self.time;

        // Calculate days until next occurrence
        let current_weekday = now.weekday();
        let target_weekday = self.day_of_week;

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

    /// Fetch showtimes from Regal API
    async fn fetch_showtimes(&self) -> Result<String> {
        // Note: This is a placeholder implementation
        // The actual Regal API endpoint and format may vary
        // You'll need to customize this based on the real API

        tracing::info!(theater_id=%self.theater_id, "fetching showtimes from Regal");

        // Example API endpoint (this may need to be updated)
        let url = format!(
            "https://www.regmovies.com/api/v1/theaters/{}/showtimes",
            self.theater_id
        );

        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .header("User-Agent", "KelvinBot/1.0")
            .send()
            .await
            .context("failed to fetch showtimes")?;

        if !response.status().is_success() {
            anyhow::bail!("API returned error status: {}", response.status());
        }

        let data: serde_json::Value = response
            .json()
            .await
            .context("failed to parse JSON response")?;

        // Format the showtimes into a message
        self.format_showtimes(data)
    }

    /// Format showtimes data into a readable message
    fn format_showtimes(&self, data: serde_json::Value) -> Result<String> {
        // This is a placeholder - customize based on actual API response structure
        let mut message = String::from("🎬 **Regal Theater Showtimes** 🎬\n\n");

        // Example parsing - adjust based on actual API structure
        if let Some(movies) = data.get("movies").and_then(|m| m.as_array()) {
            for movie in movies {
                if let (Some(title), Some(times)) = (
                    movie.get("title").and_then(|t| t.as_str()),
                    movie.get("showtimes").and_then(|t| t.as_array()),
                ) {
                    message.push_str(&format!("**{}**\n", title));

                    let times_str: Vec<String> = times
                        .iter()
                        .filter_map(|t| t.as_str().map(|s| s.to_string()))
                        .collect();

                    if !times_str.is_empty() {
                        message.push_str(&format!("  {}\n\n", times_str.join(", ")));
                    }
                }
            }
        } else {
            // Fallback if structure is different
            message.push_str(&format!("Data: {}\n", serde_json::to_string_pretty(&data)?));
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
                    service_id: self.service_id.clone(),
                    room_id: self.room_id.clone(),
                    body: message,
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
                    service_id: self.service_id.clone(),
                    room_id: self.room_id.clone(),
                    body: error_msg,
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
impl Middleware for RegalShowtimes {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        tracing::info!(
            day_of_week=?self.day_of_week,
            time=%self.time,
            theater_id=%self.theater_id,
            "regal_showtimes middleware running..."
        );

        loop {
            let next_time = self.next_scheduled_time();
            let now = Local::now();
            let duration_until = (next_time - now).to_std().unwrap_or(std::time::Duration::from_secs(0));

            tracing::info!(
                next_scheduled=%next_time.format("%Y-%m-%d %H:%M:%S %Z"),
                "waiting for next scheduled post"
            );

            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("regal_showtimes middleware shutting down...");
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
