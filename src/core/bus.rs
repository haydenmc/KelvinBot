use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::core::event::Event;
use crate::core::middleware::{Middleware, Verdict};
use crate::core::service::{Service, ServiceId};

pub enum Command {
    Unknown,
}

pub struct Bus {
    // Receive events from services
    evt_rx: Receiver<Event>,

    // Receive commands from middlewares
    cmd_rx: Receiver<Command>,

    services: HashMap<ServiceId, Arc<dyn Service>>,

    middlewares: Vec<Arc<dyn Middleware>>,
}

impl Bus {
    pub fn new(
        evt_rx: Receiver<Event>,
        cmd_rx: Receiver<Command>,
        services: HashMap<ServiceId, Arc<dyn Service>>,
        middlewares: Vec<Arc<dyn Middleware>>,
    ) -> Self {
        Self { evt_rx, cmd_rx, services, middlewares }
    }

    pub async fn run(&mut self, cancel: CancellationToken) -> anyhow::Result<()> {
        // Start all services
        info!("starting services...");
        let mut service_handles = Vec::new();
        for service in self.services.values().cloned() {
            let child_token = cancel.child_token();
            service_handles.push(tokio::spawn(async move { service.run(child_token).await }));
        }

        // Start all middlewares
        info!("starting middlewares...");
        let mut middleware_handles = Vec::new();
        for middleware in self.middlewares.iter().cloned() {
            let child_token = cancel.child_token();
            middleware_handles.push(tokio::spawn(async move { middleware.run(child_token).await }));
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
                    for mw in &self.middlewares {
                        match mw.on_event(&evt)? {
                            Verdict::Continue => {},
                            Verdict::Stop => { break; }
                        }
                    }
                }
                maybe_cmd = self.cmd_rx.recv() => {
                    info!("command received");
                    // let Some(cmd) = maybe_cmd else { break };
                    // TODO: match command, dispatch to service
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