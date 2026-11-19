use crate::common::MockService;
use async_trait::async_trait;
use kelvin_bot::core::{
    bus::{Bus, create_command_channel, create_event_channel},
    config::ReconnectionConfig,
    event::{Event, EventKind},
    middleware::Middleware,
    service::ServiceId,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_test::assert_ok;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_end_to_end_event_processing_pipeline() {
    // Create a test middleware that counts events
    #[derive(Debug)]
    struct CountingMiddleware {
        count: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl Middleware for CountingMiddleware {
        async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
            cancel.cancelled().await;
            Ok(())
        }

        fn on_event(&self, _event: &Event) -> anyhow::Result<()> {
            let mut count = self.count.lock().unwrap();
            *count += 1;
            Ok(())
        }
    }

    let counter = Arc::new(Mutex::new(0));
    let counting_middleware = Arc::new(CountingMiddleware { count: counter.clone() });

    let (_cmd_tx, cmd_rx) = create_command_channel(10);
    let (evt_tx, evt_rx) = create_event_channel(10);

    // Create a controllable mock service
    let service_id = ServiceId("test_mock".to_string());
    let (mock_service, mock_control) = MockService::new(service_id.clone(), evt_tx.clone());

    // Create services map with our mock service
    let mut services = HashMap::new();
    services.insert(
        service_id.clone(),
        Arc::new(mock_service) as Arc<dyn kelvin_bot::core::service::Service>,
    );

    // Configure middleware pipeline for our service
    let mut service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>> = HashMap::new();
    service_middlewares.insert(service_id, vec![counting_middleware as Arc<dyn Middleware>]);

    let mut bus =
        Bus::new(evt_rx, cmd_rx, services, service_middlewares, ReconnectionConfig::default());

    let cancel_token = CancellationToken::new();

    let bus_handle = {
        let cancel = cancel_token.clone();
        tokio::spawn(async move { bus.run(cancel).await })
    };

    // Give the bus a moment to start up
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Send exactly 5 events through our mock service
    mock_control.send(5).await.expect("Failed to send command to mock service");

    // Give the events time to be processed
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Send 3 more events
    mock_control.send(3).await.expect("Failed to send command to mock service");

    // Give time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    cancel_token.cancel();
    assert_ok!(bus_handle.await.unwrap());

    // Verify that exactly 8 events were processed (5 + 3)
    let final_count = *counter.lock().unwrap();
    assert_eq!(final_count, 8, "Expected exactly 8 events to be processed, got {}", final_count);
}

#[tokio::test]
async fn test_each_middleware_sees_events_in_order() {
    // Create middlewares that record which events they saw, in order
    #[derive(Debug)]
    struct OrderTrackingMiddleware {
        id: &'static str,
        order: Arc<Mutex<Vec<(&'static str, String)>>>,
    }

    #[async_trait]
    impl Middleware for OrderTrackingMiddleware {
        async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
            cancel.cancelled().await;
            Ok(())
        }

        fn on_event(&self, event: &Event) -> anyhow::Result<()> {
            if let EventKind::RoomMessage { body, .. } = &event.kind {
                self.order.lock().unwrap().push((self.id, body.clone()));
            }
            Ok(())
        }
    }

    let order_tracker = Arc::new(Mutex::new(Vec::new()));

    let middleware = |id| Arc::new(OrderTrackingMiddleware { id, order: order_tracker.clone() });
    let middleware1 = middleware("first");
    let middleware2 = middleware("second");
    let middleware3 = middleware("third");

    let (_cmd_tx, cmd_rx) = create_command_channel(10);
    let (evt_tx, evt_rx) = create_event_channel(10);

    // Create a controllable mock service
    let service_id = ServiceId("test_mock".to_string());
    let (mock_service, mock_control) = MockService::new(service_id.clone(), evt_tx.clone());

    // Create services map with our mock service
    let mut services = HashMap::new();
    services.insert(
        service_id.clone(),
        Arc::new(mock_service) as Arc<dyn kelvin_bot::core::service::Service>,
    );

    // Create middleware pipeline for our service: first, second, third
    let mut service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>> = HashMap::new();
    service_middlewares.insert(
        service_id,
        vec![
            middleware1 as Arc<dyn Middleware>,
            middleware2 as Arc<dyn Middleware>,
            middleware3 as Arc<dyn Middleware>,
        ],
    );

    let mut bus =
        Bus::new(evt_rx, cmd_rx, services, service_middlewares, ReconnectionConfig::default());

    let cancel_token = CancellationToken::new();

    let bus_handle = {
        let cancel = cancel_token.clone();
        tokio::spawn(async move { bus.run(cancel).await })
    };

    // Give the bus a moment to start up
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Send exactly 3 events through our mock service
    mock_control.send(3).await.expect("Failed to send command to mock service");

    // Give time for processing
    tokio::time::sleep(Duration::from_millis(50)).await;

    cancel_token.cancel();
    assert_ok!(bus_handle.await.unwrap());

    // Middlewares handle events independently of one another, so only the
    // order within each middleware is defined: every one sees all 3 events,
    // in the order they were sent.
    let final_order = order_tracker.lock().unwrap();
    let expected = ["test message 0", "test message 1", "test message 2"];
    for id in ["first", "second", "third"] {
        let seen: Vec<&str> = final_order
            .iter()
            .filter(|(seen_by, _)| *seen_by == id)
            .map(|(_, body)| body.as_str())
            .collect();
        assert_eq!(seen, expected, "events seen by the {id} middleware");
    }
}
