use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinSet;
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

struct ServiceState {
    backoff: ExponentialBackoff,
    attempt_count: u32,
    connection_start: Instant,
}

impl ServiceState {
    fn new(reconnect_config: ReconnectionConfig) -> Self {
        Self {
            backoff: ExponentialBackoff::new(reconnect_config),
            attempt_count: 0,
            connection_start: Instant::now(),
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

    // Per-service state tracking for reconnection
    service_state: HashMap<ServiceId, ServiceState>,
}

impl Bus {
    pub fn new(
        evt_rx: Receiver<Event>,
        cmd_rx: Receiver<Command>,
        services: HashMap<ServiceId, Arc<dyn Service>>,
        service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>>,
        reconnect_config: ReconnectionConfig,
    ) -> Self {
        // Initialize state for each service
        let service_state = services
            .keys()
            .map(|id| (id.clone(), ServiceState::new(reconnect_config.clone())))
            .collect();

        Self { evt_rx, cmd_rx, services, service_middlewares, service_state }
    }

    pub async fn run(&mut self, cancel: CancellationToken) -> anyhow::Result<()> {
        // Start all services with supervision
        info!("starting services with supervision...");
        let mut service_tasks: JoinSet<(ServiceId, anyhow::Result<()>)> = JoinSet::new();

        for (service_id, service) in &self.services {
            let child_token = cancel.child_token();
            let service_clone = service.clone();
            let id = service_id.clone();

            service_tasks.spawn(async move {
                let result = service_clone.run(child_token).await;
                (id, result)
            });

            // Track connection start time
            if let Some(state) = self.service_state.get_mut(service_id) {
                state.connection_start = Instant::now();
            }
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
            tokio::select! {
                // Wait for any service task to complete
                Some(Ok((completed_service_id, _result))) = service_tasks.join_next() => {
                    if cancel.is_cancelled() {
                        // Graceful shutdown - don't restart
                        tracing::info!(service_id=%completed_service_id, "service exited during shutdown");
                    } else {
                        // Service exited unexpectedly - apply backoff and restart
                        let state = self.service_state.get_mut(&completed_service_id);

                        if let Some(state) = state {
                            // If service ran successfully for >30s, consider it a success and reset backoff
                            let was_long_running = state.connection_start.elapsed().as_secs() > 30;

                            if was_long_running && state.attempt_count > 0 {
                                // Service recovered - reset backoff and attempts
                                tracing::info!(
                                    service_id=%completed_service_id,
                                    total_attempts=%state.attempt_count,
                                    "service recovered after previous failures"
                                );
                                state.backoff.reset();
                                state.attempt_count = 0;
                            }

                            // Increment attempt counter
                            state.attempt_count += 1;

                            tracing::warn!(
                                service_id=%completed_service_id,
                                attempt=%state.attempt_count,
                                "service exited unexpectedly, will reconnect"
                            );

                            // Calculate backoff delay
                            let delay = state.backoff.next_delay();

                            tracing::info!(
                                service_id=%completed_service_id,
                                attempt=%state.attempt_count,
                                delay_secs=%delay.as_secs(),
                                "waiting before restart"
                            );

                            // Sleep with cancellation support
                            tokio::select! {
                                _ = cancel.cancelled() => {
                                    tracing::info!(service_id=%completed_service_id, "cancellation during backoff, not restarting");
                                }
                                _ = tokio::time::sleep(delay) => {
                                    // Restart the service
                                    if let Some(service) = self.services.get(&completed_service_id) {
                                        let child_token = cancel.child_token();
                                        let service_clone = service.clone();
                                        let id = completed_service_id.clone();

                                        service_tasks.spawn(async move {
                                            let result = service_clone.run(child_token).await;
                                            (id, result)
                                        });

                                        // Update connection start time
                                        if let Some(state) = self.service_state.get_mut(&completed_service_id) {
                                            state.connection_start = Instant::now();
                                        }

                                        tracing::info!(service_id=%completed_service_id, "service restarted");
                                    }
                                }
                            }
                        }
                    }
                }
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
