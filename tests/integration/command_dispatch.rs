//! Command dispatch: each service handles its commands in order, and a
//! service that is slow to handle one never holds up the bus.

use crate::common::MockService;
use async_trait::async_trait;
use kelvin_bot::core::{
    bus::{Bus, Command, create_command_channel, create_event_channel},
    config::ReconnectionConfig,
    event::Event,
    middleware::{Middleware, Verdict},
    service::{Service, ServiceId},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_test::assert_ok;
use tokio_util::sync::CancellationToken;

/// Generous bound for things that should happen almost immediately.
const PROMPT: Duration = Duration::from_secs(2);

/// Handles a command only once the test grants it a permit, then reports the
/// command's body. A body of "panic" makes the handler panic instead.
struct GatedService {
    permits: Semaphore,
    handled_tx: mpsc::UnboundedSender<String>,
}

impl GatedService {
    fn new(permits: usize) -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let (handled_tx, handled_rx) = mpsc::unbounded_channel();
        (Arc::new(Self { permits: Semaphore::new(permits), handled_tx }), handled_rx)
    }
}

#[async_trait]
impl Service for GatedService {
    async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        cancel.cancelled().await;
        Ok(())
    }

    async fn handle_command(&self, command: Command) -> anyhow::Result<()> {
        self.permits.acquire().await?.forget();
        if let Command::SendRoomMessage { body, .. } = command {
            assert_ne!(body, "panic", "handler panic (test)");
            let _ = self.handled_tx.send(body);
        }
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

    fn on_event(&self, event: &Event) -> anyhow::Result<Verdict> {
        let _ = self.seen_tx.send(event.service_id.clone());
        Ok(Verdict::Continue)
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

async fn promptly<T>(what: &str, future: impl Future<Output = Option<T>>) -> T {
    tokio::time::timeout(PROMPT, future)
        .await
        .unwrap_or_else(|_| panic!("{what} should happen promptly"))
        .unwrap_or_else(|| panic!("channel closed while waiting for: {what}"))
}

#[tokio::test]
async fn slow_command_does_not_delay_events_or_other_services() {
    let (cmd_tx, cmd_rx) = create_command_channel(10);
    let (evt_tx, evt_rx) = create_event_channel(10);

    // Never granted a permit until the end, so its first command stays in flight.
    let stuck_id = ServiceId("stuck".to_string());
    let (stuck, mut stuck_handled) = GatedService::new(0);

    let quick_id = ServiceId("quick".to_string());
    let (quick, mut quick_handled) = GatedService::new(Semaphore::MAX_PERMITS);

    let source_id = ServiceId("source".to_string());
    let (source, source_control) = MockService::new(source_id.clone(), evt_tx.clone());

    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let recorder: Arc<dyn Middleware> = Arc::new(Recorder { seen_tx });

    let mut services: HashMap<ServiceId, Arc<dyn Service>> = HashMap::new();
    services.insert(stuck_id.clone(), stuck.clone());
    services.insert(quick_id.clone(), quick);
    services.insert(source_id.clone(), Arc::new(source));
    let service_middlewares = HashMap::from([(source_id.clone(), vec![recorder])]);

    let mut bus =
        Bus::new(evt_rx, cmd_rx, services, service_middlewares, ReconnectionConfig::default());
    let cancel = CancellationToken::new();
    let bus_handle = tokio::spawn({
        let cancel = cancel.clone();
        async move { bus.run(cancel).await }
    });

    cmd_tx.send(room_message(&stuck_id, "held up")).await.unwrap();
    cmd_tx.send(room_message(&stuck_id, "queued behind it")).await.unwrap();

    // Events are still routed while that command is in flight...
    source_control.send(2).await.unwrap();
    for _ in 0..2 {
        assert_eq!(promptly("event routing", seen_rx.recv()).await, source_id);
    }

    // ...and so are commands for another service.
    cmd_tx.send(room_message(&quick_id, "hello")).await.unwrap();
    assert_eq!(promptly("the other service's command", quick_handled.recv()).await, "hello");

    assert!(stuck_handled.try_recv().is_err(), "the stuck service has not been released yet");

    // Once released, the stuck service works through its queue in order.
    stuck.permits.add_permits(2);
    assert_eq!(promptly("first queued command", stuck_handled.recv()).await, "held up");
    assert_eq!(promptly("second queued command", stuck_handled.recv()).await, "queued behind it");

    cancel.cancel();
    assert_ok!(tokio::time::timeout(PROMPT, bus_handle).await.expect("prompt shutdown").unwrap());
}

#[tokio::test]
async fn commands_for_one_service_are_handled_in_order() {
    let (cmd_tx, cmd_rx) = create_command_channel(100);
    let (_evt_tx, evt_rx) = create_event_channel(10);

    let id = ServiceId("svc".to_string());
    let (service, mut handled) = GatedService::new(Semaphore::MAX_PERMITS);
    let services: HashMap<ServiceId, Arc<dyn Service>> =
        HashMap::from([(id.clone(), service as _)]);

    let mut bus = Bus::new(evt_rx, cmd_rx, services, HashMap::new(), ReconnectionConfig::default());
    let cancel = CancellationToken::new();
    let bus_handle = tokio::spawn({
        let cancel = cancel.clone();
        async move { bus.run(cancel).await }
    });

    for i in 0..50 {
        cmd_tx.send(room_message(&id, &i.to_string())).await.unwrap();
    }
    for i in 0..50 {
        assert_eq!(promptly("command handling", handled.recv()).await, i.to_string());
    }

    cancel.cancel();
    assert_ok!(tokio::time::timeout(PROMPT, bus_handle).await.expect("prompt shutdown").unwrap());
}

#[tokio::test]
async fn commands_beyond_a_full_queue_are_dropped_and_their_senders_told() {
    let (cmd_tx, cmd_rx) = create_command_channel(1024);
    let (_evt_tx, evt_rx) = create_event_channel(10);

    // Never released, so nothing drains its queue.
    let id = ServiceId("stuck".to_string());
    let (service, _handled) = GatedService::new(0);
    let services: HashMap<ServiceId, Arc<dyn Service>> =
        HashMap::from([(id.clone(), service as _)]);

    let mut bus = Bus::new(evt_rx, cmd_rx, services, HashMap::new(), ReconnectionConfig::default());
    let cancel = CancellationToken::new();
    let bus_handle = tokio::spawn({
        let cancel = cancel.clone();
        async move { bus.run(cancel).await }
    });

    // Far more than the queue holds; the bus must keep accepting them.
    for i in 0..600 {
        let sent = tokio::time::timeout(PROMPT, cmd_tx.send(room_message(&id, &i.to_string())));
        sent.await.expect("the bus should keep draining its command channel").unwrap();
    }

    // A sender waiting on a reply learns that its command was dropped.
    let (response_tx, response_rx) = oneshot::channel();
    cmd_tx
        .send(Command::SendRoomMessage {
            service_id: id.clone(),
            room_id: "room".to_string(),
            body: "overflow".to_string(),
            markdown_body: None,
            response_tx: Some(response_tx),
        })
        .await
        .unwrap();
    let reply = tokio::time::timeout(PROMPT, response_rx).await.expect("prompt failure");
    assert!(reply.is_err(), "the reply channel should be closed without a response");

    cancel.cancel();
    assert_ok!(tokio::time::timeout(PROMPT, bus_handle).await.expect("prompt shutdown").unwrap());
}

#[tokio::test]
async fn panic_while_handling_a_command_stops_the_bus_with_an_error() {
    let (cmd_tx, cmd_rx) = create_command_channel(10);
    let (_evt_tx, evt_rx) = create_event_channel(10);

    let id = ServiceId("svc".to_string());
    let (service, _handled) = GatedService::new(Semaphore::MAX_PERMITS);
    let services: HashMap<ServiceId, Arc<dyn Service>> =
        HashMap::from([(id.clone(), service as _)]);

    let mut bus = Bus::new(evt_rx, cmd_rx, services, HashMap::new(), ReconnectionConfig::default());
    let bus_handle = tokio::spawn(async move { bus.run(CancellationToken::new()).await });

    cmd_tx.send(room_message(&id, "panic")).await.unwrap();

    let result = tokio::time::timeout(PROMPT, bus_handle)
        .await
        .expect("the bus should stop promptly")
        .expect("the bus task itself should not panic");
    let err = result.expect_err("a dead dispatcher is fatal");
    assert!(format!("{err:#}").contains("command dispatcher panicked"), "got: {err:#}");
}
