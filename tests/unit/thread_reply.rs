use kelvin_bot::core::bus::Command;
use kelvin_bot::core::service::{Service, ServiceId};
use kelvin_bot::services::dummy::DummyService;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_dummy_service_handles_thread_reply() {
    let (evt_tx, _evt_rx) = mpsc::channel(10);
    let service_id = ServiceId("test_dummy".to_string());
    let dummy_service = DummyService { id: service_id.clone(), interval_ms: 100, evt_tx };

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let command = Command::SendThreadReply {
        service_id: service_id.clone(),
        room_id: "!room:example.com".to_string(),
        thread_root_id: "$thread_root:example.com".to_string(),
        body: "Test thread reply".to_string(),
        markdown_body: None,
        response_tx: Some(response_tx),
    };

    // Dummy service should handle the command without error
    let result = dummy_service.handle_command(command).await;
    assert!(result.is_ok());

    // Should receive a dummy message ID response
    let response = response_rx.await.expect("Should receive response");
    assert!(response.is_ok());
    assert_eq!(response.unwrap(), "dummy_message_id_thread_reply");
}

#[tokio::test]
async fn test_dummy_service_handles_thread_reply_without_response_channel() {
    let (evt_tx, _evt_rx) = mpsc::channel(10);
    let service_id = ServiceId("test_dummy".to_string());
    let dummy_service = DummyService { id: service_id.clone(), interval_ms: 100, evt_tx };

    let command = Command::SendThreadReply {
        service_id: service_id.clone(),
        room_id: "!room:example.com".to_string(),
        thread_root_id: "$thread_root:example.com".to_string(),
        body: "Test thread reply".to_string(),
        markdown_body: Some("**Test** thread reply".to_string()),
        response_tx: None,
    };

    // Should handle command even without response channel
    let result = dummy_service.handle_command(command).await;
    assert!(result.is_ok());
}
