use crate::core::{
    bus::Command,
    event::Event,
    middleware::{Middleware, Verdict},
    service::ServiceId,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{Datelike, Duration, Local, NaiveTime, TimeZone, Weekday};
use serde_with::{serde_as, DisplayFromStr};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

#[serde_as]
#[derive(Debug, Clone, Copy, serde::Deserialize)]
pub struct LatLng {
    #[serde_as(as = "DisplayFromStr")]
    pub lat: f64,
    #[serde_as(as = "DisplayFromStr")]
    pub lng: f64,
}

pub struct MovieShowtimes {
    cmd_tx: Sender<Command>,
    service_id: String,
    room_id: String,
    post_on_day_of_week: Weekday,
    post_at_time: NaiveTime,
    search_location: LatLng,
    search_radius_mi: u16,
    theater_id_filter: Option<Vec<String>>,
}

impl MovieShowtimes {
    pub fn new(
        cmd_tx: Sender<Command>,
        service_id: String,
        room_id: String,
        post_on_day_of_week: Weekday,
        post_at_time: NaiveTime,
        search_location: LatLng,
        search_radius_mi: u16,
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

    /// Generate test showtimes message
    async fn fetch_showtimes(&self) -> Result<String> {
        tracing::info!("generating test showtimes message");

        let message = format!(
            "🎬 Movie Showtimes Test 🎬\n\nThis is a test message.\n\nScheduled for: {} at {}",
            self.post_on_day_of_week, self.post_at_time
        );

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
                    service_id: ServiceId(self.service_id.clone()),
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
