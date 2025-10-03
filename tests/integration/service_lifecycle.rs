use crate::common::{create_multi_service_config, create_test_config};
use kelvin_bot::core::{
    bus::{Bus, create_command_channel, create_event_channel},
    middleware::instantiate_middleware_from_config,
    service::instantiate_services_from_config,
};
use std::time::Duration;
use tokio_test::assert_ok;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn test_service_instantiation_from_config() {
    let config = create_test_config();
    let (evt_tx, _evt_rx) = create_event_channel(10);

    let services = instantiate_services_from_config(&config, &evt_tx)
        .await
        .expect("Failed to instantiate services");

    assert_eq!(services.len(), 1);
    assert!(services.contains_key(&kelvin_bot::core::service::ServiceId("test_dummy".to_string())));
}

#[tokio::test]
async fn test_service_instantiation_with_multiple_services() {
    let config = create_multi_service_config();
    let (evt_tx, _evt_rx) = create_event_channel(10);

    let services = instantiate_services_from_config(&config, &evt_tx)
        .await
        .expect("Failed to instantiate services");

    assert_eq!(services.len(), 2);
    assert!(services.contains_key(&kelvin_bot::core::service::ServiceId("dummy1".to_string())));
    assert!(services.contains_key(&kelvin_bot::core::service::ServiceId("dummy2".to_string())));
}

#[tokio::test]
async fn test_middleware_instantiation_from_config() {
    let config = create_test_config();
    let (cmd_tx, _cmd_rx) = create_command_channel(10);

    let middlewares = instantiate_middleware_from_config(&config, &cmd_tx);

    // Should at least have the logger middleware
    assert!(!middlewares.is_empty());
}

#[tokio::test]
async fn test_bus_creation_and_startup() {
    let config = create_test_config();
    let (cmd_tx, cmd_rx) = create_command_channel(10);
    let (evt_tx, evt_rx) = create_event_channel(10);

    let services = instantiate_services_from_config(&config, &evt_tx)
        .await
        .expect("Failed to instantiate services");
    let middlewares = instantiate_middleware_from_config(&config, &cmd_tx);

    let mut bus = Bus::new(evt_rx, cmd_rx, services, middlewares);

    let cancel_token = CancellationToken::new();

    // Start the bus in the background
    let bus_handle = {
        let cancel = cancel_token.clone();
        tokio::spawn(async move { bus.run(cancel).await })
    };

    // Let it run for a short time
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel and wait for shutdown
    cancel_token.cancel();
    let result = bus_handle.await.unwrap();
    assert_ok!(result);
}

#[tokio::test]
async fn test_cancellation_propagates_to_services() {
    let config = create_test_config();
    let (cmd_tx, cmd_rx) = create_command_channel(10);
    let (evt_tx, evt_rx) = create_event_channel(10);

    let services = instantiate_services_from_config(&config, &evt_tx)
        .await
        .expect("Failed to instantiate services");
    let middlewares = instantiate_middleware_from_config(&config, &cmd_tx);

    let mut bus = Bus::new(evt_rx, cmd_rx, services, middlewares);

    let cancel_token = CancellationToken::new();

    let bus_handle = {
        let cancel = cancel_token.clone();
        tokio::spawn(async move { bus.run(cancel).await })
    };

    // Let services start up
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel should cause graceful shutdown
    cancel_token.cancel();

    // Should complete within reasonable time
    let result = tokio::time::timeout(Duration::from_secs(1), bus_handle);
    match result.await {
        Ok(join_result) => assert_ok!(join_result.unwrap()),
        Err(_) => panic!("Bus should shutdown gracefully within timeout"),
    }
}
