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
use crate::core::middleware::Middleware;
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
/// keeps routing events and other services' commands meanwhile. It stops once
/// the bus has closed the queue and every command in it has been handled.
async fn dispatch_commands(
    service_id: ServiceId,
    service: Arc<dyn Service>,
    mut commands: Receiver<Command>,
) {
    while let Some(command) = commands.recv().await {
        if let Err(e) = service.handle_command(command).await {
            tracing::error!(service_id=%service_id, error=format!("{e:#}"), "failed to handle command");
        }
    }
}

/// Hands a command to its service's queue without waiting, so a busy service
/// can never stall the bus. Dropping the command also drops any `response_tx`,
/// which tells a waiting sender that it failed.
fn route_command(command_queues: &HashMap<ServiceId, Sender<Command>>, cmd: Command) {
    let Some(queue) = command_queues.get(cmd.service_id()) else {
        tracing::warn!(service_id=%cmd.service_id(), "command sent to unknown service");
        return;
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

/// Runs one middleware's `run` until it returns, and logs how it ended.
async fn run_middleware(middleware: Arc<dyn Middleware>, cancel: CancellationToken) {
    let name = middleware.name();
    match middleware.run(cancel.clone()).await {
        Ok(()) if cancel.is_cancelled() => {
            tracing::info!(middleware=%name, "middleware exited during shutdown");
        }
        // Middleware tasks are expected to run until shutdown. Log loudly if
        // one exits early so a silently dead middleware is visible rather
        // than discovered by missing behaviour.
        Ok(()) => tracing::warn!(middleware=%name, "middleware task exited unexpectedly"),
        Err(e) => tracing::error!(middleware=%name, error=%e, "middleware task failed"),
    }
}

/// How many events may wait for one middleware before new ones are dropped.
const MIDDLEWARE_INBOX_CAPACITY: usize = 1024;

/// Feeds one middleware its events, one at a time and in arrival order.
///
/// Each middleware has its own inbox and delivery task, so a middleware that
/// is slow, or whose `on_event` fails, affects neither the bus nor the other
/// middlewares. A failure is logged and the next event is delivered as usual.
async fn deliver_events(
    middleware: Arc<dyn Middleware>,
    mut inbox: Receiver<Arc<Event>>,
    cancel: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => return,
            maybe_event = inbox.recv() => match maybe_event {
                Some(event) => event,
                None => return,
            },
        };

        if let Err(e) = middleware.on_event(&event).await {
            tracing::error!(
                middleware=%middleware.name(),
                service_id=%event.service_id,
                error=format!("{e:#}"),
                "middleware failed to handle event"
            );
        }
    }
}

/// A middleware's inbox, as seen from the bus.
#[derive(Clone)]
struct Inbox {
    middleware: &'static str,
    events: Sender<Arc<Event>>,
}

/// Describes the exit of a task that should have kept running until shutdown.
fn ended_early(task: &str, joined: Result<(), tokio::task::JoinError>) -> anyhow::Error {
    match joined {
        Ok(()) => anyhow::anyhow!("{task} stopped unexpectedly"),
        Err(e) => anyhow::Error::new(e).context(format!("{task} panicked or was aborted")),
    }
}

/// Logs a task that panicked while being stopped.
fn log_if_panicked(kind: &str, joined: Result<(), tokio::task::JoinError>) {
    if let Err(e) = joined {
        tracing::warn!(kind, error=%e, "task panicked or was aborted during shutdown");
    }
}

/// How long each stage of shutdown may take by default. With three stages the
/// worst case stays under Docker's 10-second stop timeout.
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

pub struct Bus {
    // Receive events from services
    evt_rx: Receiver<Event>,

    // Receive commands from middlewares
    cmd_rx: Receiver<Command>,

    services: HashMap<ServiceId, Arc<dyn Service>>,

    // Per-service middleware pipelines
    service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>>,

    // Middlewares to start whether or not any service routes events to them
    standalone_middlewares: Vec<Arc<dyn Middleware>>,

    // Backoff settings applied to each service's restarts
    reconnect_config: ReconnectionConfig,

    // How long each stage of shutdown may take before its tasks are aborted
    shutdown_grace: Duration,
}

impl Bus {
    pub fn new(
        evt_rx: Receiver<Event>,
        cmd_rx: Receiver<Command>,
        services: HashMap<ServiceId, Arc<dyn Service>>,
        service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>>,
        reconnect_config: ReconnectionConfig,
    ) -> Self {
        Self {
            evt_rx,
            cmd_rx,
            services,
            service_middlewares,
            standalone_middlewares: Vec::new(),
            reconnect_config,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        }
    }

    /// Sets how long each stage of shutdown (middlewares, then command
    /// dispatchers, then services) may take before its tasks are aborted.
    pub fn with_shutdown_grace(mut self, per_stage: Duration) -> Self {
        self.shutdown_grace = per_stage;
        self
    }

    /// Also starts these middlewares, even if no service's pipeline includes
    /// them. A middleware that only works on its own schedule or connection
    /// (and sends commands) needs no events, but still needs to be run.
    /// Instances already in a pipeline are started once, not twice.
    pub fn with_middlewares(
        mut self,
        middlewares: impl IntoIterator<Item = Arc<dyn Middleware>>,
    ) -> Self {
        self.standalone_middlewares.extend(middlewares);
        self
    }

    pub async fn run(&mut self, cancel: CancellationToken) -> anyhow::Result<()> {
        // The bus stops its tasks in stages (see `shut_down`), so each group
        // has its own token rather than a child of `cancel`. Dispatchers have
        // none: they stop once their queue is closed and empty.
        let middleware_cancel = CancellationToken::new();
        let service_cancel = CancellationToken::new();

        // Start all services with supervision
        info!("starting services with supervision...");
        let mut service_tasks: JoinSet<()> = JoinSet::new();

        for (service_id, service) in &self.services {
            service_tasks.spawn(supervise_service(
                service_id.clone(),
                service.clone(),
                self.reconnect_config.clone(),
                service_cancel.clone(),
            ));
        }

        // Start a command dispatcher per service
        let mut dispatch_tasks: JoinSet<()> = JoinSet::new();
        let mut command_queues: HashMap<ServiceId, Sender<Command>> = HashMap::new();

        for (service_id, service) in &self.services {
            let (queue_tx, queue_rx) = tokio::sync::mpsc::channel(SERVICE_COMMAND_QUEUE_CAPACITY);
            command_queues.insert(service_id.clone(), queue_tx);
            dispatch_tasks.spawn(dispatch_commands(service_id.clone(), service.clone(), queue_rx));
        }

        // Run every middleware once, however many pipelines it appears in.
        info!("starting middlewares...");
        let mut middleware_tasks: JoinSet<()> = JoinSet::new();
        let mut running: Vec<&Arc<dyn Middleware>> = Vec::new();

        let all_middlewares =
            self.service_middlewares.values().flatten().chain(&self.standalone_middlewares);
        for middleware in all_middlewares {
            if running.iter().any(|seen| Arc::ptr_eq(seen, middleware)) {
                continue;
            }
            running.push(middleware);
            middleware_tasks.spawn(run_middleware(middleware.clone(), middleware_cancel.clone()));
        }

        // Give each middleware that appears in a pipeline one inbox and one
        // delivery task, and record each service's pipeline as the inboxes to
        // copy its events into.
        let mut delivery_tasks: JoinSet<()> = JoinSet::new();
        let mut inboxes: Vec<(&Arc<dyn Middleware>, Inbox)> = Vec::new();
        let mut pipelines: HashMap<&ServiceId, Vec<Inbox>> = HashMap::new();

        for (service_id, pipeline) in &self.service_middlewares {
            let pipeline_inboxes = pipelines.entry(service_id).or_default();
            for middleware in pipeline {
                let existing = inboxes.iter().find(|(seen, _)| Arc::ptr_eq(seen, middleware));
                let inbox = match existing {
                    Some((_, inbox)) => inbox.clone(),
                    None => {
                        let (events, inbox_rx) =
                            tokio::sync::mpsc::channel(MIDDLEWARE_INBOX_CAPACITY);
                        delivery_tasks.spawn(deliver_events(
                            middleware.clone(),
                            inbox_rx,
                            middleware_cancel.clone(),
                        ));
                        let inbox = Inbox { middleware: middleware.name(), events };
                        inboxes.push((middleware, inbox.clone()));
                        inbox
                    }
                };
                pipeline_inboxes.push(inbox);
            }
        }

        // Begin command/event processing with service supervision
        info!("starting event bus...");

        let outcome: anyhow::Result<()> = loop {
            tokio::select! {
                // Supervisors, dispatchers and event delivery tasks are only
                // stopped by `shut_down`, after this loop. If one ends here, a
                // service is no longer restarted or no longer receives
                // commands, or a middleware no longer receives events, so stop
                // the bus (and with it the process) rather than carry on
                // half-working.
                Some(joined) = service_tasks.join_next() => {
                    break Err(ended_early("service supervisor", joined));
                }
                Some(joined) = dispatch_tasks.join_next() => {
                    break Err(ended_early("command dispatcher", joined));
                }
                Some(joined) = delivery_tasks.join_next() => {
                    break Err(ended_early("middleware event delivery", joined));
                }
                // A middleware's `run` ending early is tolerated (it has
                // already logged why), but a panic is at least made visible.
                Some(joined) = middleware_tasks.join_next() => {
                    if let Err(e) = joined {
                        tracing::error!(error=%e, "middleware task panicked or was aborted");
                    }
                }
                _ = cancel.cancelled() => {
                    info!("shutdown signal received");
                    break Ok(());
                }
                maybe_evt = self.evt_rx.recv() => {
                    info!("event received");
                    let Some(evt) = maybe_evt else { break Ok(()) };

                    // Copy the event into the inbox of each middleware in this
                    // service's pipeline, in pipeline order and without
                    // waiting, so a busy middleware can never stall the bus.
                    let Some(pipeline) = pipelines.get(&evt.service_id) else {
                        tracing::debug!(service_id=%evt.service_id, "no middleware pipeline configured for service");
                        continue;
                    };
                    let evt = Arc::new(evt);
                    for inbox in pipeline {
                        match inbox.events.try_send(evt.clone()) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {
                                tracing::error!(middleware=%inbox.middleware, service_id=%evt.service_id, "middleware inbox is full; dropping event");
                            }
                            Err(TrySendError::Closed(_)) => {
                                tracing::error!(middleware=%inbox.middleware, service_id=%evt.service_id, "middleware event delivery has stopped; dropping event");
                            }
                        }
                    }
                }
                maybe_cmd = self.cmd_rx.recv() => {
                    info!("command received");
                    let Some(cmd) = maybe_cmd else { break Ok(()) };
                    route_command(&command_queues, cmd);
                }
            }
        };

        if let Err(e) = &outcome {
            tracing::error!(error = format!("{e:#}"), "event bus failed; shutting down");
        }

        // Stop everything in dependency order, each stage with its own time
        // limit, so work in flight can finish while its dependencies are
        // still up. Events arriving meanwhile are read and discarded, so no
        // service is left blocked trying to emit one.
        //
        // 1. Middlewares finish the event they are handling and return from
        //    `run`. Commands they send while doing so are still routed.
        info!("stopping middlewares...");
        middleware_cancel.cancel();
        let middlewares_stopped = tokio::time::timeout(self.shutdown_grace, async {
            while !(middleware_tasks.is_empty() && delivery_tasks.is_empty()) {
                tokio::select! {
                    Some(joined) = middleware_tasks.join_next() => log_if_panicked("middleware", joined),
                    Some(joined) = delivery_tasks.join_next() => log_if_panicked("middleware event delivery", joined),
                    Some(cmd) = self.cmd_rx.recv() => route_command(&command_queues, cmd),
                    Some(_) = self.evt_rx.recv() => {}
                    else => break,
                }
            }
        })
        .await;
        if middlewares_stopped.is_err() {
            tracing::warn!(
                middleware_tasks = middleware_tasks.len(),
                delivery_tasks = delivery_tasks.len(),
                "middlewares did not stop in time; aborting them"
            );
            middleware_tasks.abort_all();
            delivery_tasks.abort_all();
        }

        // 2. Dispatchers deliver every command already sent, then stop when
        //    their queues are closed and empty. Services are still running.
        info!("delivering remaining commands...");
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            route_command(&command_queues, cmd);
        }
        drop(command_queues);
        self.stop_tasks("command dispatcher", &mut dispatch_tasks).await;

        // 3. Services disconnect.
        info!("stopping services...");
        service_cancel.cancel();
        self.stop_tasks("service", &mut service_tasks).await;

        info!("exited event bus");
        outcome
    }

    /// Waits up to the shutdown grace period for `tasks` to finish, then
    /// aborts any that are left. Incoming events are discarded meanwhile.
    async fn stop_tasks(&mut self, kind: &str, tasks: &mut JoinSet<()>) {
        let stopped = tokio::time::timeout(self.shutdown_grace, async {
            loop {
                tokio::select! {
                    joined = tasks.join_next() => match joined {
                        Some(joined) => log_if_panicked(kind, joined),
                        None => break,
                    },
                    Some(_) = self.evt_rx.recv() => {}
                }
            }
        })
        .await;

        if stopped.is_err() {
            tracing::warn!(
                kind,
                remaining = tasks.len(),
                "tasks did not stop in time; aborting them"
            );
            tasks.abort_all();
        }
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
