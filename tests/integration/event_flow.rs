use crate::common::MockService;
use async_trait::async_trait;
use kelvin_bot::core::{
    bus::{Bus, create_command_channel, create_event_channel},
    config::ReconnectionConfig,
    event::Event,
    middleware::{Middleware, Verdict},
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

        fn on_event(&self, _event: &Event) -> anyhow::Result<Verdict> {
            let mut count = self.count.lock().unwrap();
            *count += 1;
            Ok(Verdict::Continue)
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

    let mut bus = Bus::new(
        evt_rx,
        evt_tx.clone(),
        cmd_rx,
        services,
        service_middlewares,
        ReconnectionConfig::default(),
    );

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
async fn test_middleware_pipeline_order_and_stopping() {
    // Create middlewares that track processing order and can stop the pipeline
    #[derive(Debug)]
    struct OrderTrackingMiddleware {
        id: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
        should_stop: bool,
    }

    #[async_trait]
    impl Middleware for OrderTrackingMiddleware {
        async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
            cancel.cancelled().await;
            Ok(())
        }

        fn on_event(&self, _event: &Event) -> anyhow::Result<Verdict> {
            let mut order = self.order.lock().unwrap();
            order.push(self.id);

            if self.should_stop { Ok(Verdict::Stop) } else { Ok(Verdict::Continue) }
        }
    }

    let order_tracker = Arc::new(Mutex::new(Vec::new()));

    let middleware1 = Arc::new(OrderTrackingMiddleware {
        id: "first",
        order: order_tracker.clone(),
        should_stop: false,
    });

    let middleware2 = Arc::new(OrderTrackingMiddleware {
        id: "second",
        order: order_tracker.clone(),
        should_stop: true, // This should stop the pipeline
    });

    let middleware3 = Arc::new(OrderTrackingMiddleware {
        id: "third",
        order: order_tracker.clone(),
        should_stop: false,
    });

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

    // Create middleware pipeline for our service: first -> second (stops) -> third (should not execute)
    let mut service_middlewares: HashMap<ServiceId, Vec<Arc<dyn Middleware>>> = HashMap::new();
    service_middlewares.insert(
        service_id,
        vec![
            middleware1 as Arc<dyn Middleware>,
            middleware2 as Arc<dyn Middleware>,
            middleware3 as Arc<dyn Middleware>,
        ],
    );

    let mut bus = Bus::new(
        evt_rx,
        evt_tx.clone(),
        cmd_rx,
        services,
        service_middlewares,
        ReconnectionConfig::default(),
    );

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

    // Verify middleware execution order and stopping behavior
    let final_order = order_tracker.lock().unwrap();

    // We sent exactly 3 events, so we should have exactly 6 middleware executions
    // (first, second) for each of the 3 events = 6 total
    assert_eq!(
        final_order.len(),
        6,
        "Expected exactly 6 middleware executions (3 events × 2 middlewares), got {}",
        final_order.len()
    );

    // Check that we see the expected pattern: first, second, first, second, first, second
    // (third should never appear because second stops the pipeline)
    for (i, &middleware_name) in final_order.iter().enumerate() {
        if i % 2 == 0 {
            assert_eq!(
                middleware_name, "first",
                "Expected 'first' middleware at position {}, got '{}'",
                i, middleware_name
            );
        } else {
            assert_eq!(
                middleware_name, "second",
                "Expected 'second' middleware at position {}, got '{}'",
                i, middleware_name
            );
        }
    }

    // Verify that third middleware never executed (pipeline was stopped)
    assert!(
        !final_order.contains(&"third"),
        "Third middleware should not execute because second middleware stops the pipeline"
    );
}
