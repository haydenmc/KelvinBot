use assert_matches::assert_matches;
use kelvin_bot::core::{
    bus::{Command, create_command_channel},
    config::{Config, MiddlewareCfg, MiddlewareKind},
    event::{Event, EventKind},
    middleware::{
        Middleware, Verdict, build_middleware_pipeline, instantiate_middleware_from_config,
    },
    service::ServiceId,
};
use kelvin_bot::middlewares::{
    chat_relay::{ChatRelay, ChatRelayConfig},
    echo::Echo,
    invite::Invite,
    logger::Logger,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio_test::assert_ok;
use tokio_util::sync::CancellationToken;

#[test]
fn test_verdict_copy_trait() {
    let verdict1 = Verdict::Continue;
    let verdict2 = verdict1; // This should work due to Copy trait
    assert_matches!(verdict1, Verdict::Continue);
    assert_matches!(verdict2, Verdict::Continue);
}

#[tokio::test]
async fn test_logger_middleware_run() {
    let logger = Logger {};
    let cancel_token = CancellationToken::new();

    // Logger run should complete immediately when cancelled
    cancel_token.cancel();
    let result = logger.run(cancel_token).await;
    assert_ok!(result);
}

#[test]
fn test_logger_middleware_on_event() {
    let logger = Logger {};
    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:example.com".to_string(),
            body: "Test message".to_string(),
            is_local_user: false,
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self: false,
        },
    };

    let result = logger.on_event(&event);
    assert_ok!(result);
    assert_matches!(result.unwrap(), Verdict::Continue);
}

#[tokio::test]
async fn test_echo_middleware_with_custom_command() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let echo = Echo::new(cmd_tx, "!test".to_string());

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:example.com".to_string(),
            body: "!test hello world".to_string(),
            is_local_user: false,
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self: false,
        },
    };

    let result = echo.on_event(&event);
    assert_ok!(result);
    assert_matches!(result.unwrap(), Verdict::Continue);

    // Give async command sending time to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should have sent a command
    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::SendDirectMessage { user_id, body, .. } => {
            assert_eq!(user_id, "@user:example.com");
            assert_eq!(body, "hello world");
        }
        _ => panic!("Expected SendDirectMessage command"),
    }
}

#[tokio::test]
async fn test_echo_middleware_ignores_wrong_command() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let echo = Echo::new(cmd_tx, "!echo".to_string());

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:example.com".to_string(),
            body: "!different command".to_string(),
            is_local_user: false,
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self: false,
        },
    };

    let result = echo.on_event(&event);
    assert_ok!(result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT have sent a command
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_middleware_instantiation_with_echo() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "test_echo".to_string(),
        MiddlewareCfg { kind: MiddlewareKind::Echo { command_string: "!mycommand".to_string() } },
    );
    middlewares_map
        .insert("test_logger".to_string(), MiddlewareCfg { kind: MiddlewareKind::Logger {} });

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert_ok!(&result);

    let middlewares = result.unwrap();
    assert_eq!(middlewares.len(), 2);
    assert!(middlewares.contains_key("test_echo"));
    assert!(middlewares.contains_key("test_logger"));
}

#[test]
fn test_build_middleware_pipeline() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);

    let mut all_middlewares: HashMap<String, Arc<dyn Middleware>> = HashMap::new();
    all_middlewares
        .insert("echo1".to_string(), Arc::new(Echo::new(cmd_tx.clone(), "!echo".to_string())));
    all_middlewares.insert("logger1".to_string(), Arc::new(Logger {}));

    let middleware_names = vec!["echo1".to_string(), "logger1".to_string()];

    let result = build_middleware_pipeline(&middleware_names, &all_middlewares);
    assert_ok!(&result);

    let pipeline = result.unwrap();
    assert_eq!(pipeline.len(), 2);
}

#[test]
fn test_build_middleware_pipeline_missing_middleware() {
    let all_middlewares: HashMap<String, Arc<dyn Middleware>> = HashMap::new();
    let middleware_names = vec!["nonexistent".to_string()];

    let result = build_middleware_pipeline(&middleware_names, &all_middlewares);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("nonexistent"));
}

#[test]
fn test_build_middleware_pipeline_empty() {
    let all_middlewares: HashMap<String, Arc<dyn Middleware>> = HashMap::new();
    let middleware_names: Vec<String> = vec![];

    let result = build_middleware_pipeline(&middleware_names, &all_middlewares);
    assert_ok!(&result);
    assert_eq!(result.unwrap().len(), 0);
}

// Invite Middleware Tests

#[tokio::test]
async fn test_invite_middleware_run() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let invite =
        Invite::new(cmd_tx, "!invite".to_string(), Some(1), Some(Duration::from_secs(604800)));
    let cancel_token = CancellationToken::new();

    // Invite run should complete immediately when cancelled
    cancel_token.cancel();
    let result = invite.run(cancel_token).await;
    assert_ok!(result);
}

#[tokio::test]
async fn test_invite_middleware_accepts_local_user() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let invite =
        Invite::new(cmd_tx, "!invite".to_string(), Some(1), Some(Duration::from_secs(604800)));

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:example.com".to_string(),
            body: "!invite".to_string(),
            is_local_user: true, // Local user
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self: false,
        },
    };

    let result = invite.on_event(&event);
    assert_ok!(&result);
    assert_matches!(result.unwrap(), Verdict::Continue);

    // Give async command sending time to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should have sent a GenerateInviteToken command
    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::GenerateInviteToken { user_id, uses_allowed, expiry, .. } => {
            assert_eq!(user_id, "@user:example.com");
            assert_eq!(uses_allowed, Some(1));
            assert_eq!(expiry, Some(Duration::from_secs(604800)));
        }
        _ => panic!("Expected GenerateInviteToken command"),
    }
}

#[tokio::test]
async fn test_invite_middleware_rejects_non_local_user() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let invite =
        Invite::new(cmd_tx, "!invite".to_string(), Some(1), Some(Duration::from_secs(604800)));

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:different.com".to_string(),
            body: "!invite".to_string(),
            is_local_user: false, // Non-local user
            sender_id: "@user:different.com".to_string(),
            sender_display_name: Some("Different User".to_string()),
            is_self: false,
        },
    };

    let result = invite.on_event(&event);
    assert_ok!(&result);
    assert_matches!(result.unwrap(), Verdict::Continue);

    // Give async command sending time to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should have sent a rejection message, not a GenerateInviteToken
    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::SendDirectMessage { user_id, body, .. } => {
            assert_eq!(user_id, "@user:different.com");
            assert!(body.contains("only be generated for users from this server"));
        }
        _ => panic!("Expected SendDirectMessage command for rejection"),
    }
}

#[tokio::test]
async fn test_invite_middleware_ignores_wrong_command() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let invite =
        Invite::new(cmd_tx, "!invite".to_string(), Some(1), Some(Duration::from_secs(604800)));

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:example.com".to_string(),
            body: "!different".to_string(),
            is_local_user: true,
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self: false,
        },
    };

    let result = invite.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT have sent any command
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_invite_middleware_ignores_room_messages() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let invite =
        Invite::new(cmd_tx, "!invite".to_string(), Some(1), Some(Duration::from_secs(604800)));

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "!room:example.com".to_string(),
            body: "!invite".to_string(),
            is_local_user: true,
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self: false,
        },
    };

    let result = invite.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT process invite commands in rooms
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_invite_middleware_with_default_config() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    // Create invite with no explicit config (will use defaults)
    let invite = Invite::new(cmd_tx, "!invite".to_string(), None, None);

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:example.com".to_string(),
            body: "!invite".to_string(),
            is_local_user: true,
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self: false,
        },
    };

    let result = invite.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::GenerateInviteToken { uses_allowed, expiry, .. } => {
            // Should pass None values, letting service apply defaults
            assert_eq!(uses_allowed, None);
            assert_eq!(expiry, None);
        }
        _ => panic!("Expected GenerateInviteToken command"),
    }
}

#[tokio::test]
async fn test_invite_middleware_with_custom_expiry() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let custom_expiry = Duration::from_secs(3600); // 1 hour
    let invite = Invite::new(cmd_tx, "!invite".to_string(), Some(5), Some(custom_expiry));

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:example.com".to_string(),
            body: "!invite".to_string(),
            is_local_user: true,
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self: false,
        },
    };

    let result = invite.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::GenerateInviteToken { uses_allowed, expiry, .. } => {
            assert_eq!(uses_allowed, Some(5));
            assert_eq!(expiry, Some(custom_expiry));
        }
        _ => panic!("Expected GenerateInviteToken command"),
    }
}

#[tokio::test]
async fn test_invite_middleware_instantiation_from_config() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "test_invite".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::Invite {
                command_string: "!token".to_string(),
                uses_allowed: Some(3),
                expiry: Some(Duration::from_secs(86400)), // 1 day
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert_ok!(&result);

    let middlewares = result.unwrap();
    assert_eq!(middlewares.len(), 1);
    assert!(middlewares.contains_key("test_invite"));
}

// Chat Relay Middleware Tests

#[tokio::test]
async fn test_chat_relay_middleware_run() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let chat_relay = ChatRelay::new(
        cmd_tx,
        ChatRelayConfig {
            source_service_id: "source".to_string(),
            source_room_id: None,
            dest_service_id: "dest".to_string(),
            dest_room_id: "!dest:example.com".to_string(),
            prefix_tag: "Test".to_string(),
        },
    );
    let cancel_token = CancellationToken::new();

    // Chat relay run should complete immediately when cancelled
    cancel_token.cancel();
    let result = chat_relay.run(cancel_token).await;
    assert_ok!(result);
}

#[tokio::test]
async fn test_chat_relay_forwards_message_with_correct_format() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let chat_relay = ChatRelay::new(
        cmd_tx,
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
        },
    );

    let event = Event {
        service_id: ServiceId("mumble".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "general".to_string(),
            body: "Hello everyone!".to_string(),
            is_local_user: false,
            sender_id: "alice".to_string(),
            sender_display_name: Some("Alice".to_string()),
            is_self: false,
        },
    };

    let result = chat_relay.on_event(&event);
    assert_ok!(&result);
    assert_matches!(result.unwrap(), Verdict::Continue);

    // Give async command sending time to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should have sent a relayed message
    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::SendRoomMessage { service_id, room_id, body, .. } => {
            assert_eq!(service_id.0, "matrix");
            assert_eq!(room_id, "!voice:matrix.org");
            assert_eq!(body, "[Mumble] Alice: Hello everyone!");
        }
        _ => panic!("Expected SendRoomMessage command"),
    }
}

#[tokio::test]
async fn test_chat_relay_filters_bot_messages() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let chat_relay = ChatRelay::new(
        cmd_tx,
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
        },
    );

    let event = Event {
        service_id: ServiceId("mumble".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "general".to_string(),
            body: "I am the bot".to_string(),
            is_local_user: true,
            sender_id: "kelvin_bot".to_string(),
            sender_display_name: Some("KelvinBot".to_string()),
            is_self: true, // Bot's own message
        },
    };

    let result = chat_relay.on_event(&event);
    assert_ok!(&result);
    assert_matches!(result.unwrap(), Verdict::Continue);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT have sent any command (bot's own message filtered)
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_chat_relay_ignores_wrong_service() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let chat_relay = ChatRelay::new(
        cmd_tx,
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
        },
    );

    let event = Event {
        service_id: ServiceId("different_service".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "general".to_string(),
            body: "Hello!".to_string(),
            is_local_user: false,
            sender_id: "alice".to_string(),
            sender_display_name: Some("Alice".to_string()),
            is_self: false,
        },
    };

    let result = chat_relay.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT relay messages from wrong service
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_chat_relay_filters_by_source_room() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let chat_relay = ChatRelay::new(
        cmd_tx,
        ChatRelayConfig {
            source_service_id: "matrix".to_string(),
            source_room_id: Some("!general:matrix.org".to_string()),
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!announcements:matrix.org".to_string(),
            prefix_tag: "General".to_string(),
        },
    );

    // Message from correct room - should be relayed
    let event_correct_room = Event {
        service_id: ServiceId("matrix".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "!general:matrix.org".to_string(),
            body: "Important message".to_string(),
            is_local_user: false,
            sender_id: "@alice:matrix.org".to_string(),
            sender_display_name: Some("Alice".to_string()),
            is_self: false,
        },
    };

    let result = chat_relay.on_event(&event_correct_room);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::SendRoomMessage { body, .. } => {
            assert_eq!(body, "[General] Alice: Important message");
        }
        _ => panic!("Expected SendRoomMessage command"),
    }

    // Message from different room - should NOT be relayed
    let event_wrong_room = Event {
        service_id: ServiceId("matrix".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "!offtopic:matrix.org".to_string(),
            body: "Random message".to_string(),
            is_local_user: false,
            sender_id: "@bob:matrix.org".to_string(),
            sender_display_name: Some("Bob".to_string()),
            is_self: false,
        },
    };

    let result = chat_relay.on_event(&event_wrong_room);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT relay from wrong room
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_chat_relay_ignores_direct_messages() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let chat_relay = ChatRelay::new(
        cmd_tx,
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
        },
    );

    let event = Event {
        service_id: ServiceId("mumble".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "alice".to_string(),
            body: "Private message".to_string(),
            is_local_user: false,
            sender_id: "alice".to_string(),
            sender_display_name: Some("Alice".to_string()),
            is_self: false,
        },
    };

    let result = chat_relay.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT relay direct messages
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_chat_relay_handles_missing_display_name() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let chat_relay = ChatRelay::new(
        cmd_tx,
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
        },
    );

    let event = Event {
        service_id: ServiceId("mumble".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "general".to_string(),
            body: "Test message".to_string(),
            is_local_user: false,
            sender_id: "user123".to_string(),
            sender_display_name: None, // No display name
            is_self: false,
        },
    };

    let result = chat_relay.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::SendRoomMessage { body, .. } => {
            // Should use sender_id as fallback
            assert_eq!(body, "[Mumble] user123: Test message");
        }
        _ => panic!("Expected SendRoomMessage command"),
    }
}

#[tokio::test]
async fn test_chat_relay_instantiation_from_config() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "test_chat_relay".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::ChatRelay {
                source_service_id: "mumble_main".to_string(),
                source_room_id: Some("General".to_string()),
                dest_service_id: "matrix_main".to_string(),
                dest_room_id: "!voice:matrix.org".to_string(),
                prefix_tag: "Mumble".to_string(),
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert_ok!(&result);

    let middlewares = result.unwrap();
    assert_eq!(middlewares.len(), 1);
    assert!(middlewares.contains_key("test_chat_relay"));
}
