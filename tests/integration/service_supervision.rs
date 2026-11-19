//! Bus supervision: services that stop are restarted, and one service waiting
//! to restart never holds up the others.

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

/// How a [`FlakyService`] ends its first `failures` runs.
#[derive(Clone, Copy)]
enum Failure {
    ReturnErr,
    ReturnOk,
    Panic,
}

/// Fails its first `failures` runs, then emits one event per run and stays up
/// until cancelled.
struct FlakyService {
    id: ServiceId,
    evt_tx: mpsc::Sender<Event>,
    failure: Failure,
    failures: usize,
    runs: AtomicUsize,
}

impl FlakyService {
    fn new(
        id: &ServiceId,
        evt_tx: &mpsc::Sender<Event>,
        failure: Failure,
        failures: usize,
    ) -> Self {
        Self {
            id: id.clone(),
            evt_tx: evt_tx.clone(),
            failure,
            failures,
            runs: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Service for FlakyService {
    async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let run = self.runs.fetch_add(1, Ordering::SeqCst);
        if run < self.failures {
            match self.failure {
                Failure::ReturnErr => anyhow::bail!("connection refused (test)"),
                Failure::ReturnOk => return Ok(()),
                Failure::Panic => panic!("service panic (test)"),
            }
        }

        let event = Event {
            service_id: self.id.clone(),
            kind: EventKind::RoomMessage {
                room_id: "room".to_string(),
                body: format!("up after {run} failed runs"),
                is_local_user: false,
                sender_id: "test_user".to_string(),
                sender_display_name: None,
                is_self: false,
            },
        };
        let _ = self.evt_tx.send(event).await;
        cancel.cancelled().await;
        Ok(())
    }

    async fn handle_command(&self, _command: Command) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Forwards the service ID of every event it sees to the test.
struct Recorder {
    seen_tx: mpsc::UnboundedSender<ServiceId>,
}

#[async_trait]
impl Middleware for Recorder {
    async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        cancel.cancelled().await;
        Ok(())
    }

    async fn on_event(&self, event: &Event) -> anyhow::Result<()> {
        let _ = self.seen_tx.send(event.service_id.clone());
        Ok(())
    }
}

fn reconnect_after(initial_delay: Duration) -> ReconnectionConfig {
    ReconnectionConfig { initial_delay, jitter_factor: 0.0, ..ReconnectionConfig::default() }
}

struct RunningBus {
    cancel: CancellationToken,
    handle: JoinHandle<anyhow::Result<()>>,
    seen_rx: mpsc::UnboundedReceiver<ServiceId>,
    // Held so the bus's command channel stays open for the test's duration.
    _cmd_tx: mpsc::Sender<Command>,
}

impl RunningBus {
    /// Starts a bus over `services`, with a [`Recorder`] attached to each.
    fn start(
        services: Vec<(ServiceId, Arc<dyn Service>)>,
        evt_rx: mpsc::Receiver<Event>,
        reconnect: ReconnectionConfig,
    ) -> Self {
        let (cmd_tx, cmd_rx) = create_command_channel(10);
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        let recorder: Arc<dyn Middleware> = Arc::new(Recorder { seen_tx });

        let service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>> =
            services.iter().map(|(id, _)| (id.clone(), vec![recorder.clone()])).collect();
        let services: HashMap<ServiceId, Arc<dyn Service>> = services.into_iter().collect();

        let mut bus = Bus::new(evt_rx, cmd_rx, services, service_middlewares, reconnect);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move { bus.run(cancel).await }
        });

        Self { cancel, handle, seen_rx, _cmd_tx: cmd_tx }
    }

    /// Waits for the next event to reach the middleware pipeline.
    async fn next_event_from(&mut self) -> ServiceId {
        tokio::time::timeout(PROMPT, self.seen_rx.recv())
            .await
            .expect("an event should arrive promptly")
            .expect("recorder channel closed")
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

/// A service whose first two runs end with `failure` is running on its third.
async fn assert_restarted_after(failure: Failure) {
    let (evt_tx, evt_rx) = create_event_channel(10);
    let id = ServiceId("flaky".to_string());
    let service = Arc::new(FlakyService::new(&id, &evt_tx, failure, 2));

    let mut bus = RunningBus::start(
        vec![(id.clone(), service.clone())],
        evt_rx,
        reconnect_after(Duration::from_millis(5)),
    );

    assert_eq!(bus.next_event_from().await, id);
    assert_eq!(service.runs.load(Ordering::SeqCst), 3);

    bus.shut_down().await;
}

#[tokio::test]
async fn service_returning_an_error_is_restarted() {
    assert_restarted_after(Failure::ReturnErr).await;
}

#[tokio::test]
async fn service_returning_ok_is_restarted() {
    assert_restarted_after(Failure::ReturnOk).await;
}

#[tokio::test]
async fn service_that_panics_is_restarted() {
    assert_restarted_after(Failure::Panic).await;
}

#[tokio::test]
async fn service_waiting_to_restart_does_not_delay_other_services() {
    let (evt_tx, evt_rx) = create_event_channel(10);

    // Fails on every run, so it spends the whole test in a one-minute backoff.
    let broken_id = ServiceId("broken".to_string());
    let broken = Arc::new(FlakyService::new(&broken_id, &evt_tx, Failure::ReturnErr, usize::MAX));

    let healthy_id = ServiceId("healthy".to_string());
    let (healthy, healthy_control) = MockService::new(healthy_id.clone(), evt_tx.clone());

    let mut bus = RunningBus::start(
        vec![(broken_id, broken.clone()), (healthy_id.clone(), Arc::new(healthy))],
        evt_rx,
        reconnect_after(Duration::from_secs(60)),
    );

    // Let the broken service fail and enter its backoff before sending.
    tokio::time::timeout(PROMPT, async {
        while broken.runs.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("broken service should have been started");
    tokio::time::sleep(Duration::from_millis(20)).await;

    healthy_control.send(3).await.expect("mock service should accept the request");
    for _ in 0..3 {
        assert_eq!(bus.next_event_from().await, healthy_id);
    }

    assert_eq!(broken.runs.load(Ordering::SeqCst), 1, "still waiting out its backoff");

    // Shutdown does not wait for the backoff either.
    bus.shut_down().await;
}
