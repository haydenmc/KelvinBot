//! Shutdown: the bus stops middlewares, then command dispatchers, then
//! services, giving each stage time to finish what it is doing.

use async_trait::async_trait;
use kelvin_bot::core::{
    bus::{Bus, Command, create_command_channel, create_event_channel},
    config::ReconnectionConfig,
    event::{Event, EventKind},
    middleware::Middleware,
    service::{Service, ServiceId},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Generous bound for things that should happen almost immediately.
const PROMPT: Duration = Duration::from_secs(2);

/// Long enough that shutdown has certainly begun before the work finishes.
const WORK: Duration = Duration::from_millis(150);

/// A shared, ordered record of what happened during a test.
#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<String>>>);

impl Journal {
    fn note(&self, entry: impl Into<String>) {
        self.0.lock().unwrap().push(entry.into());
    }

    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    fn position(&self, entry: &str) -> usize {
        let entries = self.entries();
        entries
            .iter()
            .position(|e| e == entry)
            .unwrap_or_else(|| panic!("{entry:?} not found in {entries:?}"))
    }
}

/// How a [`TestService`] reacts to being told to stop.
#[derive(Clone, Copy, PartialEq)]
enum OnCancel {
    /// Take a moment to disconnect, then return.
    Disconnect,
    /// Never return.
    Hang,
}

/// Takes [`WORK`] to handle each command. A command with body "panic" makes
/// the handler panic instead.
struct TestService {
    journal: Journal,
    on_cancel: OnCancel,
    /// Signalled each time a command starts being handled.
    command_started: Notify,
    evt_tx: mpsc::Sender<Event>,
    id: ServiceId,
}

impl TestService {
    fn new(journal: &Journal, on_cancel: OnCancel, evt_tx: &mpsc::Sender<Event>) -> Arc<Self> {
        Arc::new(Self {
            journal: journal.clone(),
            on_cancel,
            command_started: Notify::new(),
            evt_tx: evt_tx.clone(),
            id: ServiceId("svc".to_string()),
        })
    }

    async fn emit(&self, body: &str) {
        let event = Event {
            service_id: self.id.clone(),
            kind: EventKind::RoomMessage {
                room_id: "room".to_string(),
                body: body.to_string(),
                is_local_user: false,
                sender_id: "test_user".to_string(),
                sender_display_name: None,
                is_self: false,
            },
        };
        self.evt_tx.send(event).await.expect("bus should be accepting events");
    }
}

#[async_trait]
impl Service for TestService {
    async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        cancel.cancelled().await;
        self.journal.note("service told to stop");
        if self.on_cancel == OnCancel::Hang {
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(WORK).await;
        self.journal.note("service disconnected");
        Ok(())
    }

    async fn handle_command(&self, command: Command) -> anyhow::Result<()> {
        let Command::SendRoomMessage { body, .. } = command else { return Ok(()) };
        assert_ne!(body, "panic", "handler panic (test)");
        self.command_started.notify_one();
        tokio::time::sleep(WORK).await;
        self.journal.note(format!("sent {body:?}"));
        Ok(())
    }
}

/// Takes [`WORK`] over each event, then replies to it with a command.
struct SlowReplier {
    journal: Journal,
    cmd_tx: mpsc::Sender<Command>,
    /// Signalled each time an event starts being handled.
    event_started: Notify,
}

#[async_trait]
impl Middleware for SlowReplier {
    async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        cancel.cancelled().await;
        self.journal.note("middleware told to stop");
        Ok(())
    }

    async fn on_event(&self, event: &Event) -> anyhow::Result<()> {
        let EventKind::RoomMessage { body, .. } = &event.kind else { return Ok(()) };
        self.event_started.notify_one();
        tokio::time::sleep(WORK).await;
        self.cmd_tx.send(room_message(&event.service_id, &format!("re: {body}"))).await?;
        Ok(())
    }
}

fn room_message(service_id: &ServiceId, body: &str) -> Command {
    Command::SendRoomMessage {
        service_id: service_id.clone(),
        room_id: "room".to_string(),
        body: body.to_string(),
        markdown_body: None,
        response_tx: None,
    }
}

struct Fixture {
    journal: Journal,
    service: Arc<TestService>,
    replier: Arc<SlowReplier>,
    cmd_tx: mpsc::Sender<Command>,
    cancel: CancellationToken,
    bus: JoinHandle<anyhow::Result<()>>,
}

impl Fixture {
    /// Starts a bus with one service and one middleware attached to it.
    fn start(on_cancel: OnCancel, shutdown_grace: Option<Duration>) -> Self {
        let (cmd_tx, cmd_rx) = create_command_channel(10);
        let (evt_tx, evt_rx) = create_event_channel(10);
        let journal = Journal::default();

        let service = TestService::new(&journal, on_cancel, &evt_tx);
        let replier = Arc::new(SlowReplier {
            journal: journal.clone(),
            cmd_tx: cmd_tx.clone(),
            event_started: Notify::new(),
        });

        let services: HashMap<ServiceId, Arc<dyn Service>> =
            HashMap::from([(service.id.clone(), service.clone() as _)]);
        let pipelines: HashMap<ServiceId, Vec<Arc<dyn Middleware>>> =
            HashMap::from([(service.id.clone(), vec![replier.clone() as _])]);

        let mut bus = Bus::new(evt_rx, cmd_rx, services, pipelines, ReconnectionConfig::default());
        if let Some(grace) = shutdown_grace {
            bus = bus.with_shutdown_grace(grace);
        }
        let cancel = CancellationToken::new();
        let bus = tokio::spawn({
            let cancel = cancel.clone();
            async move { bus.run(cancel).await }
        });

        Self { journal, service, replier, cmd_tx, cancel, bus }
    }

    /// Waits for the bus to return, which it should do within a few [`WORK`]s.
    async fn bus_result(self) -> (anyhow::Result<()>, Journal) {
        let result = tokio::time::timeout(PROMPT, self.bus)
            .await
            .expect("bus should stop promptly")
            .expect("bus task panicked");
        (result, self.journal)
    }
}

async fn promptly(what: &str, notified: impl Future<Output = ()>) {
    tokio::time::timeout(PROMPT, notified)
        .await
        .unwrap_or_else(|_| panic!("{what} should happen promptly"));
}

#[tokio::test]
async fn commands_in_flight_and_queued_are_delivered_before_services_stop() {
    let fx = Fixture::start(OnCancel::Disconnect, None);

    fx.cmd_tx.send(room_message(&fx.service.id, "first")).await.unwrap();
    fx.cmd_tx.send(room_message(&fx.service.id, "second")).await.unwrap();
    promptly("handling the first command", fx.service.command_started.notified()).await;

    // Shut down while the first command is in flight and the second is queued.
    fx.cancel.cancel();
    let (result, journal) = fx.bus_result().await;

    assert!(result.is_ok(), "a requested shutdown is not an error: {result:?}");
    assert!(journal.position("sent \"first\"") < journal.position("sent \"second\""));
    assert!(
        journal.position("sent \"second\"") < journal.position("service told to stop"),
        "the service should stay up until its commands are delivered: {:?}",
        journal.entries()
    );
    assert!(journal.entries().contains(&"service disconnected".to_string()));
}

#[tokio::test]
async fn middleware_finishes_its_event_and_its_reply_is_delivered() {
    let fx = Fixture::start(OnCancel::Disconnect, None);

    fx.service.emit("hello").await;
    promptly("handling the event", fx.replier.event_started.notified()).await;

    // Shut down while the middleware is still working on the event.
    fx.cancel.cancel();
    let (result, journal) = fx.bus_result().await;

    assert!(result.is_ok(), "{result:?}");
    assert!(
        journal.position("middleware told to stop") < journal.position("sent \"re: hello\""),
        "the reply should be sent during shutdown: {:?}",
        journal.entries()
    );
    assert!(journal.position("sent \"re: hello\"") < journal.position("service told to stop"));
}

#[tokio::test]
async fn task_that_ignores_shutdown_is_aborted_after_the_grace_period() {
    let fx = Fixture::start(OnCancel::Hang, Some(Duration::from_millis(50)));

    fx.cancel.cancel();
    let (result, journal) = fx.bus_result().await;

    assert!(result.is_ok(), "{result:?}");
    assert!(journal.entries().contains(&"service told to stop".to_string()));
    assert!(!journal.entries().contains(&"service disconnected".to_string()));
}

#[tokio::test]
async fn fatal_error_still_shuts_everything_down_in_order() {
    let fx = Fixture::start(OnCancel::Disconnect, None);

    // Kills the service's command dispatcher, which is fatal for the bus.
    fx.cmd_tx.send(room_message(&fx.service.id, "panic")).await.unwrap();
    let (result, journal) = fx.bus_result().await;

    let err = result.expect_err("a dead dispatcher is fatal");
    assert!(format!("{err:#}").contains("command dispatcher panicked"), "got: {err:#}");
    assert!(
        journal.position("middleware told to stop") < journal.position("service disconnected"),
        "everything else should still be stopped cleanly: {:?}",
        journal.entries()
    );
}
