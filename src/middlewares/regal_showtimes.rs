use crate::core::{
    bus::Command,
    event::Event,
    middleware::{Middleware, Verdict},
    service::ServiceId,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{Datelike, Duration, Local, NaiveTime, TimeZone, Weekday};
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
        Self { cmd_tx, service_id, room_id, day_of_week, time, theater_id }
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

    /// Generate test showtimes message
    async fn fetch_showtimes(&self) -> Result<String> {
        tracing::info!(theater_id=%self.theater_id, "generating test showtimes message");

        let message = format!(
            "🎬 Regal Showtimes Test 🎬\n\nThis is a test message for theater ID: {}\n\nScheduled for: {} at {}",
            self.theater_id, self.day_of_week, self.time
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
            let duration_until =
                (next_time - now).to_std().unwrap_or(std::time::Duration::from_secs(0));

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
