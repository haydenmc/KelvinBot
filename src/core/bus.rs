use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::error::TrySendError;
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
    SendThreadReply {
        service_id: ServiceId,
        room_id: String,
        thread_root_id: String,
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
    AddReaction {
        service_id: ServiceId,
        room_id: String,
        event_id: String,
        key: String,
    },
    SendRoomImage {
        service_id: ServiceId,
        room_id: String,
        caption: String,
        source_url: String,
        thumbnail_data: Vec<u8>,
        thumbnail_mimetype: String,
    },
}

/// Debug placeholder for message text, which is deliberately kept out of logs.
///
/// Bodies can carry secrets (for example the credential reset links sent by
/// the kanidm middleware) as well as private conversation, so only the length
/// is shown.
struct Redacted<'a>(&'a str);

impl std::fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted, {} bytes>", self.0.len())
    }
}

impl Command {
    /// The service this command is addressed to.
    pub fn service_id(&self) -> &ServiceId {
        match self {
            Command::SendDirectMessage { service_id, .. }
            | Command::SendRoomMessage { service_id, .. }
            | Command::SendThreadReply { service_id, .. }
            | Command::EditMessage { service_id, .. }
            | Command::AddReaction { service_id, .. }
            | Command::SendRoomImage { service_id, .. } => service_id,
        }
    }
}

// Implemented manually so message text is redacted (see `Redacted`) and
// because oneshot::Sender has no useful Debug output.
impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::SendDirectMessage { service_id, user_id, body, .. } => f
                .debug_struct("SendDirectMessage")
                .field("service_id", service_id)
                .field("user_id", user_id)
                .field("body", &Redacted(body))
                .field("response_tx", &"<Option<oneshot::Sender>>")
                .finish(),
            Command::SendRoomMessage { service_id, room_id, body, markdown_body, .. } => f
                .debug_struct("SendRoomMessage")
                .field("service_id", service_id)
                .field("room_id", room_id)
                .field("body", &Redacted(body))
                .field("markdown_body", &markdown_body.as_deref().map(Redacted))
                .field("response_tx", &"<Option<oneshot::Sender>>")
                .finish(),
            Command::SendThreadReply {
                service_id,
                room_id,
                thread_root_id,
                body,
                markdown_body,
                ..
            } => f
                .debug_struct("SendThreadReply")
                .field("service_id", service_id)
                .field("room_id", room_id)
                .field("thread_root_id", thread_root_id)
                .field("body", &Redacted(body))
                .field("markdown_body", &markdown_body.as_deref().map(Redacted))
                .field("response_tx", &"<Option<oneshot::Sender>>")
                .finish(),
            Command::EditMessage { service_id, message_id, new_body, new_markdown_body } => f
                .debug_struct("EditMessage")
                .field("service_id", service_id)
                .field("message_id", message_id)
                .field("new_body", &Redacted(new_body))
                .field("new_markdown_body", &new_markdown_body.as_deref().map(Redacted))
                .finish(),
            Command::AddReaction { service_id, room_id, event_id, key } => f
                .debug_struct("AddReaction")
                .field("service_id", service_id)
                .field("room_id", room_id)
                .field("event_id", event_id)
                .field("key", key)
                .finish(),
            Command::SendRoomImage { service_id, room_id, caption, .. } => f
                .debug_struct("SendRoomImage")
                .field("service_id", service_id)
                .field("room_id", room_id)
                .field("caption", &Redacted(caption))
                .finish(),
        }
    }
}

/// A service that stays up at least this long is considered recovered, so its
/// next failure starts again from the initial backoff delay.
const STABLE_RUN: Duration = Duration::from_secs(30);

/// Runs one service until shutdown, restarting it whenever it stops.
///
/// Each service has its own supervisor task, so one service waiting out its
/// backoff never delays the bus or the other services. The service itself
/// runs in a nested task so that a panic is caught and treated like any other
/// exit instead of taking the supervisor down with it.
async fn supervise_service(
    service_id: ServiceId,
    service: Arc<dyn Service>,
    reconnect_config: ReconnectionConfig,
    cancel: CancellationToken,
) {
    let mut backoff = ExponentialBackoff::new(reconnect_config);
    let mut attempt_count: u32 = 0;

    loop {
        let started = Instant::now();

        // A one-task JoinSet rather than a bare `tokio::spawn`: dropping it
        // aborts the service if this supervisor is itself aborted.
        let mut run = JoinSet::new();
        run.spawn({
            let service = service.clone();
            let cancel = cancel.clone();
            async move { service.run(cancel).await }
        });
        let outcome = run.join_next().await.expect("exactly one task was spawned");

        if cancel.is_cancelled() {
            tracing::info!(service_id=%service_id, "service exited during shutdown");
            return;
        }

        if started.elapsed() > STABLE_RUN && attempt_count > 0 {
            tracing::info!(
                service_id=%service_id,
                total_attempts=%attempt_count,
                "service recovered after previous failures"
            );
            backoff.reset();
            attempt_count = 0;
        }
        attempt_count += 1;

        match outcome {
            Ok(Ok(())) => tracing::warn!(
                service_id=%service_id,
                attempt=%attempt_count,
                "service exited unexpectedly, will restart"
            ),
            Ok(Err(e)) => tracing::error!(
                service_id=%service_id,
                attempt=%attempt_count,
                error=format!("{e:#}"),
                "service failed, will restart"
            ),
            Err(e) => tracing::error!(
                service_id=%service_id,
                attempt=%attempt_count,
                error=%e,
                "service panicked, will restart"
            ),
        }

        let delay = backoff.next_delay();
        tracing::info!(
            service_id=%service_id,
            attempt=%attempt_count,
            delay_secs=%delay.as_secs(),
            "waiting before restart"
        );

        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::info!(service_id=%service_id, "cancellation during backoff, not restarting");
                return;
            }
            _ = tokio::time::sleep(delay) => {}
        }

        tracing::info!(service_id=%service_id, "restarting service");
    }
}

/// How many commands may wait for one service before new ones are dropped.
const SERVICE_COMMAND_QUEUE_CAPACITY: usize = 256;

/// Delivers one service's commands to it, one at a time and in order.
///
/// Each service has its own queue and dispatcher task, so a slow
/// `handle_command` only delays later commands for that same service; the bus
/// keeps routing events and other services' commands meanwhile.
async fn dispatch_commands(
    service_id: ServiceId,
    service: Arc<dyn Service>,
    mut commands: Receiver<Command>,
    cancel: CancellationToken,
) {
    loop {
        let command = tokio::select! {
            _ = cancel.cancelled() => return,
            maybe_command = commands.recv() => match maybe_command {
                Some(command) => command,
                None => return,
            },
        };

        if let Err(e) = service.handle_command(command).await {
            tracing::error!(service_id=%service_id, error=format!("{e:#}"), "failed to handle command");
        }
    }
}

/// Checks the exit of a task that is meant to run until shutdown.
///
/// Returns an error if it panicked, or ended while the bus was still running.
fn ensure_stopped_by_shutdown(
    task: &str,
    joined: Result<(), tokio::task::JoinError>,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    match joined {
        Ok(()) if cancel.is_cancelled() => Ok(()),
        Ok(()) => Err(anyhow::anyhow!("{task} stopped unexpectedly")),
        Err(e) => Err(anyhow::Error::new(e).context(format!("{task} panicked or was aborted"))),
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

    // Backoff settings applied to each service's restarts
    reconnect_config: ReconnectionConfig,
}

impl Bus {
    pub fn new(
        evt_rx: Receiver<Event>,
        cmd_rx: Receiver<Command>,
        services: HashMap<ServiceId, Arc<dyn Service>>,
        service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>>,
        reconnect_config: ReconnectionConfig,
    ) -> Self {
        Self { evt_rx, cmd_rx, services, service_middlewares, reconnect_config }
    }

    pub async fn run(&mut self, cancel: CancellationToken) -> anyhow::Result<()> {
        // Start all services with supervision
        info!("starting services with supervision...");
        let mut service_tasks: JoinSet<()> = JoinSet::new();

        for (service_id, service) in &self.services {
            service_tasks.spawn(supervise_service(
                service_id.clone(),
                service.clone(),
                self.reconnect_config.clone(),
                cancel.child_token(),
            ));
        }

        // Start a command dispatcher per service
        let mut dispatch_tasks: JoinSet<()> = JoinSet::new();
        let mut command_queues: HashMap<ServiceId, Sender<Command>> = HashMap::new();

        for (service_id, service) in &self.services {
            let (queue_tx, queue_rx) = tokio::sync::mpsc::channel(SERVICE_COMMAND_QUEUE_CAPACITY);
            command_queues.insert(service_id.clone(), queue_tx);
            dispatch_tasks.spawn(dispatch_commands(
                service_id.clone(),
                service.clone(),
                queue_rx,
                cancel.child_token(),
            ));
        }

        // Start all middlewares (collect unique instances across all services)
        info!("starting middlewares...");
        let mut middleware_tasks: JoinSet<(&'static str, anyhow::Result<()>)> = JoinSet::new();
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
                    middleware_tasks.spawn(async move {
                        let name = middleware_clone.name();
                        let result = middleware_clone.run(child_token).await;
                        (name, result)
                    });
                }
            }
        }

        // Begin command/event processing with service supervision
        info!("starting event bus...");

        loop {
            tokio::select! {
                // Supervisors and dispatchers only return on shutdown. If one
                // ends any other way its service is no longer restarted or no
                // longer receives commands, so stop the bus (and with it the
                // process) rather than carry on half-working.
                Some(joined) = service_tasks.join_next() => {
                    ensure_stopped_by_shutdown("service supervisor", joined, &cancel)?;
                }
                Some(joined) = dispatch_tasks.join_next() => {
                    ensure_stopped_by_shutdown("command dispatcher", joined, &cancel)?;
                }
                // Middleware tasks are expected to run until shutdown. Log
                // loudly if one exits early so a silently dead middleware is
                // visible rather than discovered by missing behaviour.
                Some(joined) = middleware_tasks.join_next() => {
                    match joined {
                        Ok((name, Ok(()))) if cancel.is_cancelled() => {
                            tracing::info!(middleware=%name, "middleware exited during shutdown");
                        }
                        Ok((name, Ok(()))) => {
                            tracing::warn!(middleware=%name, "middleware task exited unexpectedly");
                        }
                        Ok((name, Err(e))) => {
                            tracing::error!(middleware=%name, error=%e, "middleware task failed");
                        }
                        Err(e) => {
                            tracing::error!(error=%e, "middleware task panicked or was aborted");
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

                    // Hand the command to its service's queue without waiting,
                    // so a busy service can never stall the bus. Dropping the
                    // command also drops any `response_tx`, which tells a
                    // waiting sender that it failed.
                    let Some(queue) = command_queues.get(cmd.service_id()) else {
                        tracing::warn!(service_id=%cmd.service_id(), "command sent to unknown service");
                        continue;
                    };
                    match queue.try_send(cmd) {
                        Ok(()) => {}
                        Err(TrySendError::Full(cmd)) => {
                            tracing::error!(service_id=%cmd.service_id(), command=?cmd, "service command queue is full; dropping command");
                        }
                        Err(TrySendError::Closed(cmd)) => {
                            tracing::error!(service_id=%cmd.service_id(), command=?cmd, "service command dispatcher has stopped; dropping command");
                        }
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
