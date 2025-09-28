use assert_matches::assert_matches;
use kelvin_bot::core::event::EventKind;
use kelvin_bot::core::service::{Service, ServiceId};
use kelvin_bot::services::dummy::DummyService;
use tokio::sync::mpsc;
use tokio_test::assert_ok;
use tokio_util::sync::CancellationToken;

#[test]
fn test_service_id_display() {
    let service_id = ServiceId("test_service".to_string());
    assert_eq!(format!("{}", service_id), "test_service");
}

#[test]
fn test_service_id_clone_and_eq() {
    let service_id1 = ServiceId("test".to_string());
    let service_id2 = service_id1.clone();
    assert_eq!(service_id1, service_id2);
}

#[tokio::test]
async fn test_dummy_service_creation() {
    let (evt_tx, _evt_rx) = mpsc::channel(10);
    let service_id = ServiceId("test_dummy".to_string());

    let dummy_service = DummyService { id: service_id.clone(), interval_ms: 100, evt_tx };

    assert_eq!(dummy_service.id, service_id);
    assert_eq!(dummy_service.interval_ms, 100);
}

#[tokio::test]
async fn test_dummy_service_sends_events() {
    let (evt_tx, mut evt_rx) = mpsc::channel(10);
    let service_id = ServiceId("test_dummy".to_string());

    let dummy_service = DummyService {
        id: service_id.clone(),
        interval_ms: 50, // Short interval for fast test
        evt_tx,
    };

    let cancel_token = CancellationToken::new();
    let service_handle = {
        let cancel = cancel_token.clone();
        tokio::spawn(async move { dummy_service.run(cancel).await })
    };

    // Wait for at least one event
    let received_event = tokio::time::timeout(std::time::Duration::from_millis(200), evt_rx.recv())
        .await
        .expect("Timeout waiting for event")
        .expect("Channel closed");

    // Verify the event
    assert_eq!(received_event.service_id, service_id);
    assert_matches!(received_event.kind, EventKind::RoomMessage { .. });

    if let EventKind::RoomMessage { room_id, body } = received_event.kind {
        assert_eq!(room_id, "1");
        assert_eq!(body, "hello from dummy");
    }

    // Cancel the service
    cancel_token.cancel();
    assert_ok!(service_handle.await.unwrap());
}
