use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::core::event::Event;
use crate::core::middleware::{Middleware, Verdict};
use crate::core::service::{Service, ServiceId};

pub enum Command {
    SendDirectMessage {
        service_id: ServiceId,
        user_id: String,
        body: String,
    },
    SendRoomMessage {
        service_id: ServiceId,
        room_id: String,
        body: String,
    },
    GenerateInviteToken {
        service_id: ServiceId,
        user_id: String,
        response_tx: tokio::sync::oneshot::Sender<anyhow::Result<String>>,
    },
}

// Implement Debug manually since oneshot::Sender doesn't implement Clone
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::SendDirectMessage { service_id, user_id, body } => f
                .debug_struct("SendDirectMessage")
                .field("service_id", service_id)
                .field("user_id", user_id)
                .field("body", body)
                .finish(),
            Command::SendRoomMessage { service_id, room_id, body } => f
                .debug_struct("SendRoomMessage")
                .field("service_id", service_id)
                .field("room_id", room_id)
                .field("body", body)
                .finish(),
            Command::GenerateInviteToken { service_id, user_id, .. } => f
                .debug_struct("GenerateInviteToken")
                .field("service_id", service_id)
                .field("user_id", user_id)
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
}

impl Bus {
    pub fn new(
        evt_rx: Receiver<Event>,
        cmd_rx: Receiver<Command>,
        services: HashMap<ServiceId, Arc<dyn Service>>,
        service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>>,
    ) -> Self {
        Self { evt_rx, cmd_rx, services, service_middlewares }
    }

    pub async fn run(&mut self, cancel: CancellationToken) -> anyhow::Result<()> {
        // Start all services
        info!("starting services...");
        let mut service_handles = Vec::new();
        for service in self.services.values() {
            let child_token = cancel.child_token();
            let service_clone = service.clone();
            service_handles.push(tokio::spawn(async move { service_clone.run(child_token).await }));
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

        // Begin command/event processing
        info!("starting event bus...");
        loop {
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
