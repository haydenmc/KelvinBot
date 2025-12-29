use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::core::config::{ExponentialBackoff, ReconnectionConfig};
use crate::core::event::Event;
use crate::core::middleware::{Middleware, Verdict};
use crate::core::service::{Service, ServiceId};

pub enum Command {
    SendDirectMessage {
        service_id: ServiceId,
        user_id: String,
        body: String,
        response_tx: Option<tokio::sync::oneshot::Sender<anyhow::Result<String>>>,
    },
    SendRoomMessage {
        service_id: ServiceId,
        room_id: String,
        body: String,
        markdown_body: Option<String>,
        response_tx: Option<tokio::sync::oneshot::Sender<anyhow::Result<String>>>,
    },
    EditMessage {
        service_id: ServiceId,
        message_id: String,
        new_body: String,
        new_markdown_body: Option<String>,
    },
    GenerateInviteToken {
        service_id: ServiceId,
        user_id: String,
        uses_allowed: Option<u32>,
        expiry: Option<Duration>,
        response_tx: tokio::sync::oneshot::Sender<anyhow::Result<String>>,
    },
}

// Implement Debug manually since oneshot::Sender doesn't implement Clone
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::SendDirectMessage { service_id, user_id, body, .. } => f
                .debug_struct("SendDirectMessage")
                .field("service_id", service_id)
                .field("user_id", user_id)
                .field("body", body)
                .field("response_tx", &"<Option<oneshot::Sender>>")
                .finish(),
            Command::SendRoomMessage { service_id, room_id, body, markdown_body, .. } => f
                .debug_struct("SendRoomMessage")
                .field("service_id", service_id)
                .field("room_id", room_id)
                .field("body", body)
                .field("markdown_body", markdown_body)
                .field("response_tx", &"<Option<oneshot::Sender>>")
                .finish(),
            Command::EditMessage { service_id, message_id, new_body, new_markdown_body } => f
                .debug_struct("EditMessage")
                .field("service_id", service_id)
                .field("message_id", message_id)
                .field("new_body", new_body)
                .field("new_markdown_body", new_markdown_body)
                .finish(),
            Command::GenerateInviteToken { service_id, user_id, uses_allowed, expiry, .. } => f
                .debug_struct("GenerateInviteToken")
                .field("service_id", service_id)
                .field("user_id", user_id)
                .field("uses_allowed", uses_allowed)
                .field("expiry", expiry)
                .field("response_tx", &"<oneshot::Sender>")
                .finish(),
        }
    }
}

pub struct Bus {
    // Receive events from services
    evt_rx: Receiver<Event>,

    // Receive commands from middlewares
    cmd_rx: Receiver<Command>,

    services: HashMap<ServiceId, Arc<dyn Service>>,

    // Per-service middleware pipelines
    service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>>,

    // Per-service backoff state
    backoff_state: HashMap<ServiceId, ExponentialBackoff>,

    // Per-service attempt counters
    attempt_counters: HashMap<ServiceId, u32>,

    // Per-service connection start times
    connection_start_times: HashMap<ServiceId, Instant>,
}

impl Bus {
    pub fn new(
        evt_rx: Receiver<Event>,
        cmd_rx: Receiver<Command>,
        services: HashMap<ServiceId, Arc<dyn Service>>,
        service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>>,
        reconnect_config: ReconnectionConfig,
    ) -> Self {
        // Initialize backoff state for each service
        let backoff_state = services
            .keys()
            .map(|id| (id.clone(), ExponentialBackoff::new(reconnect_config.clone())))
            .collect();

        Self {
            evt_rx,
            cmd_rx,
            services,
            service_middlewares,
            backoff_state,
            attempt_counters: HashMap::new(),
            connection_start_times: HashMap::new(),
        }
    }

    pub async fn run(&mut self, cancel: CancellationToken) -> anyhow::Result<()> {
        // Start all services with supervision
        info!("starting services with supervision...");
        let mut service_tasks: HashMap<ServiceId, tokio::task::JoinHandle<anyhow::Result<()>>> =
            HashMap::new();

        for (service_id, service) in &self.services {
            let child_token = cancel.child_token();
            let service_clone = service.clone();

            let handle = tokio::spawn(async move { service_clone.run(child_token).await });

            service_tasks.insert(service_id.clone(), handle);
            // Track connection start time
            self.connection_start_times.insert(service_id.clone(), Instant::now());
        }

        // Start all middlewares (collect unique instances across all services)
        info!("starting middlewares...");
        let mut middleware_handles = Vec::new();
        let mut started_middlewares: Vec<Arc<dyn Middleware>> = Vec::new();

        for pipeline in self.service_middlewares.values() {
            for middleware in pipeline {
                // Use Arc::ptr_eq to track unique instances
                let already_started =
                    started_middlewares.iter().any(|started| Arc::ptr_eq(started, middleware));

                if !already_started {
                    started_middlewares.push(middleware.clone());
                    let child_token = cancel.child_token();
                    let middleware_clone = middleware.clone();
                    middleware_handles
                        .push(tokio::spawn(async move { middleware_clone.run(child_token).await }));
                }
            }
        }

        // Begin command/event processing with service supervision
        info!("starting event bus...");

        loop {
            // Check for completed service tasks
            let mut completed_services = Vec::new();
            service_tasks.retain(|service_id, handle| {
                if handle.is_finished() {
                    completed_services.push(service_id.clone());
                    false // Remove from map
                } else {
                    true // Keep in map
                }
            });

            // Handle completed services (restart with backoff if not shutting down)
            for completed_service_id in completed_services {
                if cancel.is_cancelled() {
                    // Graceful shutdown - don't restart
                    tracing::info!(service_id=%completed_service_id, "service exited during shutdown");
                } else {
                    // Service exited unexpectedly - apply backoff and restart
                    let attempt_count =
                        self.attempt_counters.get(&completed_service_id).copied().unwrap_or(0);
                    let connection_start =
                        self.connection_start_times.get(&completed_service_id).copied();

                    // If service ran successfully for >30s, consider it a success and reset backoff
                    let was_long_running =
                        connection_start.map(|t| t.elapsed().as_secs() > 30).unwrap_or(false);

                    if was_long_running && attempt_count > 0 {
                        // Service recovered - reset backoff and attempts
                        if let Some(backoff) = self.backoff_state.get_mut(&completed_service_id) {
                            backoff.reset();
                        }
                        self.attempt_counters.insert(completed_service_id.clone(), 0);
                        tracing::info!(
                            service_id=%completed_service_id,
                            total_attempts=%attempt_count,
                            "service recovered after previous failures"
                        );
                    }

                    // Increment attempt counter
                    let new_attempt = attempt_count + 1;
                    self.attempt_counters.insert(completed_service_id.clone(), new_attempt);

                    tracing::warn!(
                        service_id=%completed_service_id,
                        attempt=%new_attempt,
                        "service exited unexpectedly, will reconnect"
                    );

                    // Calculate backoff delay
                    let delay =
                        if let Some(backoff) = self.backoff_state.get_mut(&completed_service_id) {
                            backoff.next_delay()
                        } else {
                            Duration::from_secs(1)
                        };

                    tracing::info!(
                        service_id=%completed_service_id,
                        attempt=%new_attempt,
                        delay_secs=%delay.as_secs(),
                        "waiting before restart"
                    );

                    // Sleep with cancellation support
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            tracing::info!(service_id=%completed_service_id, "cancellation during backoff, not restarting");
                            continue;
                        }
                        _ = tokio::time::sleep(delay) => {
                            // Continue to restart
                        }
                    }

                    // Restart the service
                    if let Some(service) = self.services.get(&completed_service_id) {
                        let child_token = cancel.child_token();
                        let service_clone = service.clone();

                        let handle =
                            tokio::spawn(async move { service_clone.run(child_token).await });

                        service_tasks.insert(completed_service_id.clone(), handle);
                        self.connection_start_times
                            .insert(completed_service_id.clone(), Instant::now());
                        tracing::info!(service_id=%completed_service_id, "service restarted");
                    }
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("shutdown signal received");
                    break;
                }
                maybe_evt = self.evt_rx.recv() => {
                    info!("event received");
                    let Some(evt) = maybe_evt else { break };

                    // Get the middleware pipeline for this service
                    if let Some(pipeline) = self.service_middlewares.get(&evt.service_id) {
                        for mw in pipeline {
                            match mw.on_event(&evt)? {
                                Verdict::Continue => {},
                                Verdict::Stop => { break; }
                            }
                        }
                    } else {
                        tracing::debug!(service_id=%evt.service_id, "no middleware pipeline configured for service");
                    }
                }
                maybe_cmd = self.cmd_rx.recv() => {
                    info!("command received");
                    let Some(cmd) = maybe_cmd else { break };

                    // Extract service_id from command
                    let service_id = match &cmd {
                        Command::SendDirectMessage { service_id, .. } => service_id.clone(),
                        Command::SendRoomMessage { service_id, .. } => service_id.clone(),
                        Command::EditMessage { service_id, .. } => service_id.clone(),
                        Command::GenerateInviteToken { service_id, .. } => service_id.clone(),
                    };

                    // Dispatch command to appropriate service
                    if let Some(service) = self.services.get(&service_id) {
                        if let Err(e) = service.handle_command(cmd).await {
                            tracing::error!(service_id=%service_id, error=%e, "failed to handle command");
                        }
                    } else {
                        tracing::warn!(service_id=%service_id, "command sent to unknown service");
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    // Periodic check for service completions
                    // This ensures we detect service exits even if no events/commands are flowing
                }
            }
        }
        info!("exited event bus");
        Ok(())
    }
}

// A small helper to make a Command channel pair available to middlewares.
pub fn create_command_channel(cap: usize) -> (Sender<Command>, Receiver<Command>) {
    tokio::sync::mpsc::channel(cap)
}

// A small helper to make an Event channel pair available to services.
pub fn create_event_channel(cap: usize) -> (Sender<Event>, Receiver<Event>) {
    tokio::sync::mpsc::channel(cap)
}
