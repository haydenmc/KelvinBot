use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, Verdict},
    service::ServiceId,
};
use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc::Sender};
use tokio_util::sync::CancellationToken;

pub struct AttendanceRelayConfig {
    pub source_service_id: String,
    pub source_room_id: Option<String>,
    pub dest_service_id: String,
    pub dest_room_id: String,
    pub session_start_message: String,
    pub session_end_message: String,
    pub session_ended_edit_message: String,
}

pub struct AttendanceRelay {
    cmd_tx: Sender<Command>,
    source_service_id: String,
    source_room_id: Option<String>,
    dest_service_id: String,
    dest_room_id: String,
    session_start_message: String,
    session_end_message: String,
    session_ended_edit_message: String,
    state: Arc<Mutex<SessionState>>,
}

struct SessionState {
    is_session_active: bool,
    active_participants: HashSet<String>,
    all_participants: HashSet<String>,
    session_start_time: Option<DateTime<Utc>>,
    live_message_id: Option<String>,
}

#[derive(Clone)]
struct DestinationConfig {
    service_id: ServiceId,
    room_id: String,
}

#[derive(Clone)]
struct MessageTemplates {
    session_start: String,
    session_end: String,
    session_ended_edit: String,
}

impl SessionState {
    fn new() -> Self {
        Self {
            is_session_active: false,
            active_participants: HashSet::new(),
            all_participants: HashSet::new(),
            session_start_time: None,
            live_message_id: None,
        }
    }
}

impl AttendanceRelay {
    pub fn new(cmd_tx: Sender<Command>, config: AttendanceRelayConfig) -> Self {
        Self {
            cmd_tx,
            source_service_id: config.source_service_id,
            source_room_id: config.source_room_id,
            dest_service_id: config.dest_service_id,
            dest_room_id: config.dest_room_id,
            session_start_message: config.session_start_message,
            session_end_message: config.session_end_message,
            session_ended_edit_message: config.session_ended_edit_message,
            state: Arc::new(Mutex::new(SessionState::new())),
        }
    }
}

#[async_trait]
impl Middleware for AttendanceRelay {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        tracing::info!(
            source_service=%self.source_service_id,
            source_room=?self.source_room_id,
            dest_service=%self.dest_service_id,
            dest_room=%self.dest_room_id,
            "attendance_relay middleware running..."
        );
        cancel.cancelled().await;
        tracing::info!("attendance_relay middleware shutting down...");
        Ok(())
    }

    fn on_event(&self, event: &Event) -> Result<Verdict> {
        // Filter: only handle events from our source service
        if event.service_id.0 != self.source_service_id {
            return Ok(Verdict::Continue);
        }

        // Filter: only handle UserListUpdate events
        let EventKind::UserListUpdate { users } = &event.kind else {
            return Ok(Verdict::Continue);
        };

        // Filter: if source_room_id is specified, only handle events from that room
        // Note: For services without room concept (like Mumble), this field won't exist in the event
        // and source_room_id should be None
        if let Some(ref expected_room_id) = self.source_room_id {
            // Check if this event has a room_id and if it matches
            // For now, we'll assume UserListUpdate events don't have room filtering
            // If needed in the future, we can extend the Event struct
            // For services like Mumble, source_room_id will be None so this check is skipped
            let _ = expected_room_id; // Silence unused warning for now
        }

        // Extract non-self active users
        let current_active: HashSet<String> = users
            .iter()
            .filter(|u| !u.is_self && u.is_active)
            .map(|u| u.display_name.clone())
            .collect();

        // Clone data for async task
        let state = self.state.clone();
        let cmd_tx = self.cmd_tx.clone();
        let destination = DestinationConfig {
            service_id: ServiceId(self.dest_service_id.clone()),
            room_id: self.dest_room_id.clone(),
        };
        let messages = MessageTemplates {
            session_start: self.session_start_message.clone(),
            session_end: self.session_end_message.clone(),
            session_ended_edit: self.session_ended_edit_message.clone(),
        };

        // Spawn async task to handle state changes
        tokio::spawn(async move {
            let mut state_guard = state.lock().await;

            if let Err(e) = handle_user_list_change(
                &mut state_guard,
                current_active,
                cmd_tx,
                destination,
                messages,
            )
            .await
            {
                tracing::error!(error=%e, "failed to handle user list change");
            }
        });

        Ok(Verdict::Continue)
    }
}

async fn handle_user_list_change(
    state: &mut SessionState,
    current_active: HashSet<String>,
    cmd_tx: Sender<Command>,
    destination: DestinationConfig,
    messages: MessageTemplates,
) -> Result<()> {
    let was_active = state.is_session_active;
    let now_active = !current_active.is_empty();

    match (was_active, now_active) {
        (false, true) => {
            // SESSION START: First user joined
            handle_session_start(
                state,
                current_active,
                cmd_tx,
                destination,
                &messages.session_start,
            )
            .await?;
        }
        (true, true) => {
            // SESSION ONGOING: Update participant list
            handle_session_update(
                state,
                current_active,
                cmd_tx,
                destination,
                &messages.session_start,
            )
            .await?;
        }
        (true, false) => {
            // SESSION END: Last user left
            handle_session_end(
                state,
                cmd_tx,
                destination,
                &messages.session_end,
                &messages.session_ended_edit,
            )
            .await?;
        }
        (false, false) => {
            // No change - no active users
        }
    }

    Ok(())
}

async fn handle_session_start(
    state: &mut SessionState,
    current_active: HashSet<String>,
    cmd_tx: Sender<Command>,
    destination: DestinationConfig,
    session_start_message: &str,
) -> Result<()> {
    tracing::info!("session started with {} user(s)", current_active.len());

    state.is_session_active = true;
    state.session_start_time = Some(Utc::now());
    state.active_participants = current_active.clone();
    state.all_participants = current_active.clone();

    // Format the initial message
    let body = format_live_message(session_start_message, &state.active_participants);

    // Send initial message with response channel to get message ID
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();

    let command = Command::SendRoomMessage {
        service_id: destination.service_id,
        room_id: destination.room_id,
        body: body.clone(),
        markdown_body: Some(body),
        response_tx: Some(response_tx),
    };

    cmd_tx.send(command).await?;

    // Wait for message ID
    match response_rx.await {
        Ok(Ok(message_id)) => {
            state.live_message_id = Some(message_id);
            tracing::info!("session start message sent");
        }
        Ok(Err(e)) => {
            tracing::error!(error=%e, "failed to send session start message");
        }
        Err(e) => {
            tracing::error!(error=%e, "failed to receive message ID");
        }
    }

    Ok(())
}

async fn handle_session_update(
    state: &mut SessionState,
    current_active: HashSet<String>,
    cmd_tx: Sender<Command>,
    destination: DestinationConfig,
    session_start_message: &str,
) -> Result<()> {
    // Update tracking
    for user in &current_active {
        state.all_participants.insert(user.clone());
    }
    state.active_participants = current_active.clone();

    // Edit the live message if we have a message ID
    if let Some(message_id) = &state.live_message_id {
        let body = format_live_message(session_start_message, &state.active_participants);

        let command = Command::EditMessage {
            service_id: destination.service_id,
            message_id: message_id.clone(),
            new_body: body.clone(),
            new_markdown_body: Some(body),
        };

        cmd_tx.send(command).await?;
        tracing::debug!(
            "updated live message with {} participants",
            state.active_participants.len()
        );
    } else {
        // No message ID yet - this can happen if the initial message send failed
        // (e.g., due to service not being ready at startup). Try to send a new message.
        tracing::info!("no live message ID found, attempting to send new session start message");

        let body = format_live_message(session_start_message, &state.active_participants);

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();

        let command = Command::SendRoomMessage {
            service_id: destination.service_id,
            room_id: destination.room_id,
            body: body.clone(),
            markdown_body: Some(body),
            response_tx: Some(response_tx),
        };

        cmd_tx.send(command).await?;

        // Wait for message ID
        match response_rx.await {
            Ok(Ok(message_id)) => {
                state.live_message_id = Some(message_id);
                tracing::info!("session start message sent (retry after initial failure)");
            }
            Ok(Err(e)) => {
                tracing::warn!(error=%e, "failed to send session start message (will retry on next update)");
            }
            Err(e) => {
                tracing::warn!(error=%e, "failed to receive message ID (will retry on next update)");
            }
        }
    }

    Ok(())
}

async fn handle_session_end(
    state: &mut SessionState,
    cmd_tx: Sender<Command>,
    destination: DestinationConfig,
    session_end_message: &str,
    session_ended_edit_message: &str,
) -> Result<()> {
    let duration = state.session_start_time.map(|start| Utc::now() - start).unwrap_or_default();

    let all_participants: Vec<String> = state.all_participants.iter().cloned().collect();

    tracing::info!(
        "session ended after {} seconds with {} total participants",
        duration.num_seconds(),
        all_participants.len()
    );

    // Edit the original message with the configured ended message
    if let Some(message_id) = &state.live_message_id {
        let edit_body = session_ended_edit_message.to_string();

        let command = Command::EditMessage {
            service_id: destination.service_id.clone(),
            message_id: message_id.clone(),
            new_body: edit_body.clone(),
            new_markdown_body: Some(edit_body),
        };

        cmd_tx.send(command).await?;
    }

    // Send summary message
    let summary_body = format_session_summary(session_end_message, &all_participants, duration);

    let command = Command::SendRoomMessage {
        service_id: destination.service_id,
        room_id: destination.room_id,
        body: summary_body.clone(),
        markdown_body: Some(summary_body),
        response_tx: None,
    };

    cmd_tx.send(command).await?;

    // Reset state
    state.is_session_active = false;
    state.active_participants.clear();
    state.all_participants.clear();
    state.session_start_time = None;
    state.live_message_id = None;

    Ok(())
}

fn format_live_message(prefix: &str, participants: &HashSet<String>) -> String {
    let mut sorted: Vec<_> = participants.iter().collect();
    sorted.sort();

    let participant_list = if sorted.is_empty() {
        "No active participants".to_string()
    } else {
        sorted.iter().map(|s| format!("- {}", s)).collect::<Vec<_>>().join("\n")
    };

    format!("{}\n\n{}", prefix, participant_list)
}

fn format_session_summary(
    end_message: &str,
    all_participants: &[String],
    duration: chrono::Duration,
) -> String {
    let mut sorted = all_participants.to_vec();
    sorted.sort();

    let hours = duration.num_hours();
    let minutes = duration.num_minutes() % 60;
    let seconds = duration.num_seconds() % 60;

    let duration_str = if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    };

    let participant_list = sorted.iter().map(|s| format!("- {}", s)).collect::<Vec<_>>().join("\n");

    format!("{}\n\nDuration: {}\n\nParticipants:\n{}", end_message, duration_str, participant_list)
}
