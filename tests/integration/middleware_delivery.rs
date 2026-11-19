//! Event delivery: each middleware gets its events in order through its own
//! inbox, and one middleware failing does not affect the bus or the others.

use crate::common::MockService;
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_test::assert_ok;
use tokio_util::sync::CancellationToken;

/// Generous bound for things that should happen almost immediately.
const PROMPT: Duration = Duration::from_secs(2);

/// What a [`Probe`] does with each event, after reporting it.
#[derive(Clone, Copy)]
enum Behaviour {
    Succeed,
    ReturnErr,
    Panic,
}

/// Reports each message body it is given, and how often it was started.
struct Probe {
    behaviour: Behaviour,
    seen_tx: mpsc::UnboundedSender<String>,
    runs: AtomicUsize,
}

impl Probe {
    fn new(behaviour: Behaviour) -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        (Arc::new(Self { behaviour, seen_tx, runs: AtomicUsize::new(0) }), seen_rx)
    }
}

#[async_trait]
impl Middleware for Probe {
    async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        cancel.cancelled().await;
        Ok(())
    }

    fn on_event(&self, event: &Event) -> anyhow::Result<()> {
        if let EventKind::RoomMessage { body, .. } = &event.kind {
            let _ = self.seen_tx.send(body.clone());
        }
        match self.behaviour {
            Behaviour::Succeed => Ok(()),
            Behaviour::ReturnErr => anyhow::bail!("middleware error (test)"),
            Behaviour::Panic => panic!("middleware panic (test)"),
        }
    }
}

struct RunningBus {
    cancel: CancellationToken,
    handle: JoinHandle<anyhow::Result<()>>,
    /// Asks each mock service, by name, to emit that many events.
    emit: HashMap<&'static str, mpsc::Sender<usize>>,
    // Held so the bus's channels stay open for the test's duration.
    _cmd_tx: mpsc::Sender<Command>,
    _evt_tx: mpsc::Sender<Event>,
}

impl RunningBus {
    /// Starts a bus with one mock service per pipeline, plus `standalone`
    /// middlewares that are in no pipeline.
    fn start(
        pipelines: Vec<(&'static str, Vec<Arc<dyn Middleware>>)>,
        standalone: Vec<Arc<dyn Middleware>>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = create_command_channel(10);
        let (evt_tx, evt_rx) = create_event_channel(100);

        let mut services: HashMap<ServiceId, Arc<dyn Service>> = HashMap::new();
        let mut service_middlewares = HashMap::new();
        let mut emit = HashMap::new();
        for (name, pipeline) in pipelines {
            let id = ServiceId(name.to_string());
            let (service, control) = MockService::new(id.clone(), evt_tx.clone());
            services.insert(id.clone(), Arc::new(service));
            service_middlewares.insert(id, pipeline);
            emit.insert(name, control);
        }

        let mut bus =
            Bus::new(evt_rx, cmd_rx, services, service_middlewares, ReconnectionConfig::default())
                .with_middlewares(standalone);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move { bus.run(cancel).await }
        });

        Self { cancel, handle, emit, _cmd_tx: cmd_tx, _evt_tx: evt_tx }
    }

    async fn emit(&self, service: &str, count: usize) {
        self.emit[service].send(count).await.expect("mock service should accept the request");
    }

    async fn shut_down(self) {
        self.cancel.cancel();
        let joined = tokio::time::timeout(PROMPT, self.handle)
            .await
            .expect("bus should shut down promptly")
            .expect("bus task panicked");
        assert_ok!(joined);
    }
}

/// Collects the next `count` bodies a probe reports.
async fn next_bodies(seen_rx: &mut mpsc::UnboundedReceiver<String>, count: usize) -> Vec<String> {
    let mut bodies = Vec::new();
    for _ in 0..count {
        let body = tokio::time::timeout(PROMPT, seen_rx.recv())
            .await
            .expect("an event should be delivered promptly")
            .expect("probe channel closed");
        bodies.push(body);
    }
    bodies
}

fn mock_bodies(count: usize) -> Vec<String> {
    (0..count).map(|i| format!("test message {i}")).collect()
}

#[tokio::test]
async fn middleware_error_does_not_stop_the_bus_or_other_middlewares() {
    let (failing, mut failing_seen) = Probe::new(Behaviour::ReturnErr);
    let (healthy, mut healthy_seen) = Probe::new(Behaviour::Succeed);
    let bus = RunningBus::start(vec![("svc", vec![failing as _, healthy as _])], vec![]);

    bus.emit("svc", 3).await;

    // The middleware after the failing one still gets every event...
    assert_eq!(next_bodies(&mut healthy_seen, 3).await, mock_bodies(3));
    // ...and the failing one keeps receiving events after its first error.
    assert_eq!(next_bodies(&mut failing_seen, 3).await, mock_bodies(3));

    // The bus is still running: a later batch is delivered too.
    bus.emit("svc", 2).await;
    assert_eq!(next_bodies(&mut healthy_seen, 2).await, mock_bodies(2));

    bus.shut_down().await;
}

#[tokio::test]
async fn each_middleware_receives_events_in_order() {
    let (first, mut first_seen) = Probe::new(Behaviour::Succeed);
    let (second, mut second_seen) = Probe::new(Behaviour::Succeed);
    let bus = RunningBus::start(vec![("svc", vec![first as _, second as _])], vec![]);

    bus.emit("svc", 50).await;

    assert_eq!(next_bodies(&mut first_seen, 50).await, mock_bodies(50));
    assert_eq!(next_bodies(&mut second_seen, 50).await, mock_bodies(50));

    bus.shut_down().await;
}

#[tokio::test]
async fn middleware_in_no_pipeline_is_still_run() {
    let (standalone, mut standalone_seen) = Probe::new(Behaviour::Succeed);
    let (attached, mut attached_seen) = Probe::new(Behaviour::Succeed);
    let bus = RunningBus::start(
        vec![("svc", vec![attached.clone() as _])],
        vec![standalone.clone() as _],
    );

    // Once an event has gone through, startup is certainly complete.
    bus.emit("svc", 1).await;
    assert_eq!(next_bodies(&mut attached_seen, 1).await, mock_bodies(1));

    assert_eq!(standalone.runs.load(Ordering::SeqCst), 1, "started despite being in no pipeline");
    assert_eq!(attached.runs.load(Ordering::SeqCst), 1);
    assert!(standalone_seen.try_recv().is_err(), "it is in no pipeline, so it gets no events");

    bus.shut_down().await;
}

#[tokio::test]
async fn middleware_shared_by_two_services_is_run_once_and_sees_both() {
    let (shared, mut shared_seen) = Probe::new(Behaviour::Succeed);
    let bus = RunningBus::start(
        vec![("one", vec![shared.clone() as _]), ("two", vec![shared.clone() as _])],
        // Listing it again as a standalone middleware must not start it twice.
        vec![shared.clone() as _],
    );

    bus.emit("one", 1).await;
    bus.emit("two", 1).await;
    next_bodies(&mut shared_seen, 2).await;

    assert_eq!(shared.runs.load(Ordering::SeqCst), 1);

    bus.shut_down().await;
}

#[tokio::test]
async fn panic_while_handling_an_event_stops_the_bus_with_an_error() {
    let (panicking, _seen) = Probe::new(Behaviour::Panic);
    let bus = RunningBus::start(vec![("svc", vec![panicking as _])], vec![]);

    bus.emit("svc", 1).await;

    let result = tokio::time::timeout(PROMPT, bus.handle)
        .await
        .expect("the bus should stop promptly")
        .expect("the bus task itself should not panic");
    let err = result.expect_err("a dead delivery task is fatal");
    assert!(format!("{err:#}").contains("middleware event delivery panicked"), "got: {err:#}");
}
