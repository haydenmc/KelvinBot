use kelvin_bot::core::bus::{create_command_channel, create_event_channel};

#[test]
fn test_command_channel_creation() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);

    // Verify we can clone the sender
    let _cmd_tx_clone = cmd_tx.clone();

    // Channel should be created successfully with specified capacity
    // Basic smoke test - if we get here, channel creation worked
}

#[test]
fn test_event_channel_creation() {
    let (evt_tx, _evt_rx) = create_event_channel(10);

    // Verify we can clone the sender
    let _evt_tx_clone = evt_tx.clone();

    // Channel should be created successfully with specified capacity
    // Basic smoke test - if we get here, channel creation worked
}

mod command_debug {
    use kelvin_bot::core::bus::Command;
    use kelvin_bot::core::service::ServiceId;

    const SECRET: &str = "https://idm.example.com/ui/reset?token=s3cr3t";

    fn service_id() -> ServiceId {
        ServiceId("svc".to_string())
    }

    fn assert_redacted(command: Command) {
        let rendered = format!("{command:?}");
        assert!(!rendered.contains("s3cr3t"), "message text leaked into Debug output: {rendered}");
        assert!(rendered.contains("redacted"), "expected a redaction marker: {rendered}");
    }

    #[test]
    fn direct_message_body_is_redacted() {
        assert_redacted(Command::SendDirectMessage {
            service_id: service_id(),
            user_id: "@alice:example.com".to_string(),
            body: SECRET.to_string(),
            response_tx: None,
        });
    }

    #[test]
    fn room_message_bodies_are_redacted() {
        assert_redacted(Command::SendRoomMessage {
            service_id: service_id(),
            room_id: "!room:example.com".to_string(),
            body: SECRET.to_string(),
            markdown_body: Some(SECRET.to_string()),
            response_tx: None,
        });
    }

    #[test]
    fn thread_reply_bodies_are_redacted() {
        assert_redacted(Command::SendThreadReply {
            service_id: service_id(),
            room_id: "!room:example.com".to_string(),
            thread_root_id: "$root".to_string(),
            body: SECRET.to_string(),
            markdown_body: Some(SECRET.to_string()),
            response_tx: None,
        });
    }

    #[test]
    fn edit_bodies_are_redacted() {
        assert_redacted(Command::EditMessage {
            service_id: service_id(),
            message_id: "$event".to_string(),
            new_body: SECRET.to_string(),
            new_markdown_body: Some(SECRET.to_string()),
        });
    }

    #[test]
    fn image_caption_is_redacted() {
        assert_redacted(Command::SendRoomImage {
            service_id: service_id(),
            room_id: "General".to_string(),
            caption: SECRET.to_string(),
            source_url: "https://example.com/image".to_string(),
            thumbnail_data: Vec::new(),
            thumbnail_mimetype: "image/jpeg".to_string(),
        });
    }

    #[test]
    fn routing_fields_stay_visible() {
        let rendered = format!(
            "{:?}",
            Command::SendDirectMessage {
                service_id: service_id(),
                user_id: "@alice:example.com".to_string(),
                body: SECRET.to_string(),
                response_tx: None,
            }
        );
        assert!(rendered.contains("@alice:example.com"));
        assert!(rendered.contains("svc"));
    }
}
