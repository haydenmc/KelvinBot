use assert_matches::assert_matches;
use kelvin_bot::core::{
    event::{Event, EventKind},
    service::ServiceId,
};

#[test]
fn test_event_display_direct_message() {
    let event = Event {
        service_id: ServiceId("test_service".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:example.com".to_string(),
            body: "Hello world".to_string(),
        },
    };

    let display = format!("{}", event);
    assert!(display.contains("[test_service]"));
    assert!(display.contains("[DM]"));
    assert!(display.contains("@user:example.com"));
    assert!(display.contains("Hello world"));
}

#[test]
fn test_event_display_room_message() {
    let event = Event {
        service_id: ServiceId("matrix_service".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "!room123:example.com".to_string(),
            body: "Test message".to_string(),
        },
    };

    let display = format!("{}", event);
    assert!(display.contains("[matrix_service]"));
    assert!(display.contains("[RM]"));
    assert!(display.contains("!room123:example.com"));
    assert!(display.contains("Test message"));
}

#[test]
fn test_event_serialization() {
    let event = Event {
        service_id: ServiceId("test_service".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "!room:example.com".to_string(),
            body: "Hello".to_string(),
        },
    };

    // Test serialization
    let serialized = serde_json::to_string(&event).expect("Failed to serialize");
    assert!(serialized.contains("test_service"));
    assert!(serialized.contains("RoomMessage"));

    // Test deserialization
    let deserialized: Event = serde_json::from_str(&serialized).expect("Failed to deserialize");
    assert_eq!(deserialized.service_id.0, "test_service");
    assert_matches!(deserialized.kind, EventKind::RoomMessage { .. });
}
