use assert_matches::assert_matches;
use kelvin_bot::core::{
    bus::{Command, create_command_channel},
    config::{Config, MiddlewareCfg, MiddlewareKind, ReconnectionConfig},
    event::{Event, EventKind, User},
    middleware::{
        Middleware, MiddlewareContext, Verdict, build_middleware_pipeline,
        instantiate_middleware_from_config,
    },
    service::ServiceId,
};
use kelvin_bot::middlewares::{
    attendance_relay::{AttendanceRelay, AttendanceRelayConfig},
    chat_relay::{ChatRelay, ChatRelayConfig},
    echo::Echo,
    kanidm::{KanidmConfig, KanidmIdentity},
    logger::Logger,
};
use kelvin_bot::store::PersistentStore;
use secrecy::SecretString;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::mpsc::Sender;
use tokio_test::assert_ok;
use tokio_util::sync::CancellationToken;

fn make_ctx(cmd_tx: Sender<Command>) -> MiddlewareContext {
    MiddlewareContext { cmd_tx, store: Arc::new(PersistentStore::in_memory()) }
}

fn make_ctx_with_store(cmd_tx: Sender<Command>, store: Arc<PersistentStore>) -> MiddlewareContext {
    MiddlewareContext { cmd_tx, store }
}

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
    let echo = Echo::new(make_ctx(cmd_tx), "!test".to_string());

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
    let echo = Echo::new(make_ctx(cmd_tx), "!echo".to_string());

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
async fn test_echo_middleware_ignores_self_messages() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let echo = Echo::new(make_ctx(cmd_tx), "!echo".to_string());

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@bot:example.com".to_string(),
            body: "!echo this is from myself".to_string(),
            is_local_user: true,
            sender_id: "@bot:example.com".to_string(),
            sender_display_name: Some("Bot".to_string()),
            is_self: true,
        },
    };

    let result = echo.on_event(&event);
    assert_ok!(result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT have sent a command because is_self is true
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
        reconnection: ReconnectionConfig::default(),
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
    all_middlewares.insert(
        "echo1".to_string(),
        Arc::new(Echo::new(make_ctx(cmd_tx.clone()), "!echo".to_string())),
    );
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

// Kanidm Identity Middleware Tests

fn make_kanidm(cmd_tx: Sender<Command>) -> KanidmIdentity {
    KanidmIdentity::new(
        make_ctx(cmd_tx),
        "!reset".to_string(),
        "!invite".to_string(),
        KanidmConfig {
            kanidm_url: "https://idm.example.com".to_string(),
            kanidm_token: SecretString::from("kanidm-token"),
            mas_url: "https://auth.example.com".to_string(),
            mas_client_id: "client".to_string(),
            mas_client_secret: SecretString::from("secret"),
            mas_provider_id: "provider".to_string(),
            reset_token_ttl: Duration::from_secs(600),
            invite_token_ttl: Duration::from_secs(86400),
        },
    )
}

fn dm_event(body: &str, is_local_user: bool, is_self: bool) -> Event {
    Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::DirectMessage {
            user_id: "@user:example.com".to_string(),
            body: body.to_string(),
            is_local_user,
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self,
        },
    }
}

#[tokio::test]
async fn test_kanidm_middleware_run() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let kanidm = make_kanidm(cmd_tx);
    let cancel_token = CancellationToken::new();

    // run should complete immediately when cancelled
    cancel_token.cancel();
    let result = kanidm.run(cancel_token).await;
    assert_ok!(result);
}

#[tokio::test]
async fn test_kanidm_rejects_non_local_user() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let kanidm = make_kanidm(cmd_tx);

    let event = dm_event("!reset", false, false);
    let result = kanidm.on_event(&event);
    assert_ok!(&result);
    assert_matches!(result.unwrap(), Verdict::Continue);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Non-local users get a rejection DM, no API call is attempted.
    match cmd_rx.try_recv() {
        Ok(Command::SendDirectMessage { user_id, body, .. }) => {
            assert_eq!(user_id, "@user:example.com");
            assert!(body.contains("can only be used by users on this server"));
        }
        other => panic!("Expected SendDirectMessage rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn test_kanidm_invite_missing_username_replies_usage() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let kanidm = make_kanidm(cmd_tx);

    // Bare "!invite" with no username should return a usage hint (no network).
    let event = dm_event("!invite", true, false);
    assert_ok!(&kanidm.on_event(&event));

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    match cmd_rx.try_recv() {
        Ok(Command::SendDirectMessage { body, .. }) => {
            assert!(body.contains("Usage:"));
            assert!(body.contains("!invite"));
            assert!(body.contains("<email>"));
        }
        other => panic!("Expected usage SendDirectMessage, got {other:?}"),
    }
}

#[tokio::test]
async fn test_kanidm_invite_missing_email_replies_usage() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let kanidm = make_kanidm(cmd_tx);

    // Username but no email should return the usage hint (no network).
    let event = dm_event("!invite alice", true, false);
    assert_ok!(&kanidm.on_event(&event));

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    match cmd_rx.try_recv() {
        Ok(Command::SendDirectMessage { body, .. }) => {
            assert!(body.contains("Usage:"));
            assert!(body.contains("<email>"));
        }
        other => panic!("Expected usage SendDirectMessage, got {other:?}"),
    }
}

#[tokio::test]
async fn test_kanidm_invite_invalid_email_replies_usage() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let kanidm = make_kanidm(cmd_tx);

    // A malformed email should be rejected before any network call.
    let event = dm_event("!invite alice not-an-email", true, false);
    assert_ok!(&kanidm.on_event(&event));

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    match cmd_rx.try_recv() {
        Ok(Command::SendDirectMessage { body, .. }) => {
            assert!(body.contains("Usage:"));
            assert!(body.contains("<email>"));
        }
        other => panic!("Expected usage SendDirectMessage, got {other:?}"),
    }
}

#[tokio::test]
async fn test_kanidm_ignores_wrong_command() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let kanidm = make_kanidm(cmd_tx);

    let event = dm_event("!different", true, false);
    assert_ok!(&kanidm.on_event(&event));

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_kanidm_ignores_self_messages() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let kanidm = make_kanidm(cmd_tx);

    // A message from the bot itself must never trigger a command.
    let event = dm_event("!reset", true, true);
    assert_ok!(&kanidm.on_event(&event));

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_kanidm_ignores_room_messages() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let kanidm = make_kanidm(cmd_tx);

    let event = Event {
        service_id: ServiceId("test".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "!room:example.com".to_string(),
            body: "!reset".to_string(),
            is_local_user: true,
            sender_id: "@user:example.com".to_string(),
            sender_display_name: Some("Test User".to_string()),
            is_self: false,
        },
    };

    assert_ok!(&kanidm.on_event(&event));

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Identity commands are DM-only.
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_kanidm_instantiation_from_config() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "identity".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::Kanidm {
                command_reset: "!reset".to_string(),
                command_invite: "!invite".to_string(),
                kanidm_url: "https://idm.example.com".to_string(),
                kanidm_token: SecretString::from("kanidm-token"),
                mas_url: "https://auth.example.com".to_string(),
                mas_client_id: "client".to_string(),
                mas_client_secret: SecretString::from("secret"),
                mas_provider_id: "provider".to_string(),
                reset_token_ttl: Duration::from_secs(600),
                invite_token_ttl: Duration::from_secs(86400),
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
        reconnection: ReconnectionConfig::default(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert_ok!(&result);

    let middlewares = result.unwrap();
    assert_eq!(middlewares.len(), 1);
    assert!(middlewares.contains_key("identity"));
}

// Chat Relay Middleware Tests

#[tokio::test]
async fn test_chat_relay_middleware_run() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let chat_relay = ChatRelay::new(
        make_ctx(cmd_tx),
        ChatRelayConfig {
            source_service_id: "source".to_string(),
            source_room_id: None,
            dest_service_id: "dest".to_string(),
            dest_room_id: "!dest:example.com".to_string(),
            prefix_tag: "Test".to_string(),
            thumbnail_max_width: 200,
            thumbnail_max_height: 150,
            thumbnail_jpeg_quality: 60,
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
        make_ctx(cmd_tx),
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
            thumbnail_max_width: 200,
            thumbnail_max_height: 150,
            thumbnail_jpeg_quality: 60,
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
        make_ctx(cmd_tx),
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
            thumbnail_max_width: 200,
            thumbnail_max_height: 150,
            thumbnail_jpeg_quality: 60,
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
        make_ctx(cmd_tx),
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
            thumbnail_max_width: 200,
            thumbnail_max_height: 150,
            thumbnail_jpeg_quality: 60,
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
        make_ctx(cmd_tx),
        ChatRelayConfig {
            source_service_id: "matrix".to_string(),
            source_room_id: Some("!general:matrix.org".to_string()),
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!announcements:matrix.org".to_string(),
            prefix_tag: "General".to_string(),
            thumbnail_max_width: 200,
            thumbnail_max_height: 150,
            thumbnail_jpeg_quality: 60,
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
        make_ctx(cmd_tx),
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
            thumbnail_max_width: 200,
            thumbnail_max_height: 150,
            thumbnail_jpeg_quality: 60,
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
        make_ctx(cmd_tx),
        ChatRelayConfig {
            source_service_id: "mumble".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!voice:matrix.org".to_string(),
            prefix_tag: "Mumble".to_string(),
            thumbnail_max_width: 200,
            thumbnail_max_height: 150,
            thumbnail_jpeg_quality: 60,
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
                thumbnail_max_width: 200,
                thumbnail_max_height: 150,
                thumbnail_jpeg_quality: 60,
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
        reconnection: ReconnectionConfig::default(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert_ok!(&result);

    let middlewares = result.unwrap();
    assert_eq!(middlewares.len(), 1);
    assert!(middlewares.contains_key("test_chat_relay"));
}

// Attendance Relay Middleware Tests

#[tokio::test]
async fn test_attendance_relay_middleware_run() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Session started".to_string(),
            session_end_message: "Session ended".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );
    let cancel_token = CancellationToken::new();

    // Attendance relay run should complete immediately when cancelled
    cancel_token.cancel();
    let result = attendance_relay.run(cancel_token).await;
    assert_ok!(result);
}

#[tokio::test]
async fn test_attendance_relay_session_start() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Active participants:".to_string(),
            session_end_message: "Session summary".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );

    // Create a UserListUpdate event with active users (session start: 0 → 2 users)
    let event = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![
                User {
                    id: "user1".to_string(),
                    username: "alice".to_string(),
                    display_name: "Alice".to_string(),
                    is_active: true,
                    is_self: false,
                },
                User {
                    id: "user2".to_string(),
                    username: "bob".to_string(),
                    display_name: "Bob".to_string(),
                    is_active: true,
                    is_self: false,
                },
            ],
        },
    };

    let result = attendance_relay.on_event(&event);
    assert_ok!(&result);
    assert_matches!(result.unwrap(), Verdict::Continue);

    // Give async command sending time to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Should have sent a SendRoomMessage command with initial participants
    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::SendRoomMessage { service_id, room_id, body, .. } => {
            assert_eq!(service_id.0, "matrix");
            assert_eq!(room_id, "!test:example.com");
            assert!(body.contains("Active participants:"));
            assert!(body.contains("- Alice"));
            assert!(body.contains("- Bob"));
        }
        _ => panic!("Expected SendRoomMessage command"),
    }
}

#[tokio::test]
async fn test_attendance_relay_session_update_with_edit() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Active participants:".to_string(),
            session_end_message: "Session summary".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );

    // First event: Start session with Alice
    let event1 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![User {
                id: "user1".to_string(),
                username: "alice".to_string(),
                display_name: "Alice".to_string(),
                is_active: true,
                is_self: false,
            }],
        },
    };

    attendance_relay.on_event(&event1).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Get the initial SendRoomMessage and respond to its oneshot with a message_id
    let cmd = cmd_rx.recv().await.unwrap();
    match cmd {
        Command::SendRoomMessage { response_tx, .. } => {
            if let Some(tx) = response_tx {
                // Simulate command handler responding with a message_id
                let _ = tx.send(Ok("msg_123".to_string()));
            }
        }
        _ => panic!("Expected SendRoomMessage command"),
    }

    // Give time for the async task to process the response and set live_message_id
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Second event: Bob joins (session update: 1 → 2 users)
    let event2 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![
                User {
                    id: "user1".to_string(),
                    username: "alice".to_string(),
                    display_name: "Alice".to_string(),
                    is_active: true,
                    is_self: false,
                },
                User {
                    id: "user2".to_string(),
                    username: "bob".to_string(),
                    display_name: "Bob".to_string(),
                    is_active: true,
                    is_self: false,
                },
            ],
        },
    };

    attendance_relay.on_event(&event2).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Now we should get an EditMessage command (not SendRoomMessage)
    let cmd = cmd_rx.recv().await.unwrap();
    match cmd {
        Command::EditMessage { service_id, message_id, new_body, .. } => {
            assert_eq!(service_id.0, "matrix");
            assert_eq!(message_id, "msg_123");
            assert!(new_body.contains("Active participants:"));
            assert!(new_body.contains("- Alice"));
            assert!(new_body.contains("- Bob"));
        }
        _ => panic!("Expected EditMessage command, got {:?}", cmd),
    }
}

#[tokio::test]
async fn test_attendance_relay_multiple_updates() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Active participants:".to_string(),
            session_end_message: "Session summary".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );

    // Event 1: Alice joins (session start)
    let event1 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![User {
                id: "user1".to_string(),
                username: "alice".to_string(),
                display_name: "Alice".to_string(),
                is_active: true,
                is_self: false,
            }],
        },
    };

    attendance_relay.on_event(&event1).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Respond to initial SendRoomMessage with message_id
    let cmd = cmd_rx.recv().await.unwrap();
    match cmd {
        Command::SendRoomMessage { response_tx, .. } => {
            if let Some(tx) = response_tx {
                let _ = tx.send(Ok("msg_123".to_string()));
            }
        }
        _ => panic!("Expected SendRoomMessage command"),
    }
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Event 2: Bob joins
    let event2 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![
                User {
                    id: "user1".to_string(),
                    username: "alice".to_string(),
                    display_name: "Alice".to_string(),
                    is_active: true,
                    is_self: false,
                },
                User {
                    id: "user2".to_string(),
                    username: "bob".to_string(),
                    display_name: "Bob".to_string(),
                    is_active: true,
                    is_self: false,
                },
            ],
        },
    };

    attendance_relay.on_event(&event2).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Should get EditMessage with Alice and Bob
    let cmd = cmd_rx.recv().await.unwrap();
    match cmd {
        Command::EditMessage { message_id, new_body, .. } => {
            assert_eq!(message_id, "msg_123");
            assert!(new_body.contains("- Alice"));
            assert!(new_body.contains("- Bob"));
        }
        _ => panic!("Expected EditMessage command"),
    }

    // Event 3: Charlie joins
    let event3 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![
                User {
                    id: "user1".to_string(),
                    username: "alice".to_string(),
                    display_name: "Alice".to_string(),
                    is_active: true,
                    is_self: false,
                },
                User {
                    id: "user2".to_string(),
                    username: "bob".to_string(),
                    display_name: "Bob".to_string(),
                    is_active: true,
                    is_self: false,
                },
                User {
                    id: "user3".to_string(),
                    username: "charlie".to_string(),
                    display_name: "Charlie".to_string(),
                    is_active: true,
                    is_self: false,
                },
            ],
        },
    };

    attendance_relay.on_event(&event3).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Should get EditMessage with Alice, Bob, and Charlie
    let cmd = cmd_rx.recv().await.unwrap();
    match cmd {
        Command::EditMessage { message_id, new_body, .. } => {
            assert_eq!(message_id, "msg_123");
            assert!(new_body.contains("- Alice"));
            assert!(new_body.contains("- Bob"));
            assert!(new_body.contains("- Charlie"));
        }
        _ => panic!("Expected EditMessage command"),
    }

    // Event 4: Alice leaves, only Bob and Charlie remain
    let event4 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![
                User {
                    id: "user2".to_string(),
                    username: "bob".to_string(),
                    display_name: "Bob".to_string(),
                    is_active: true,
                    is_self: false,
                },
                User {
                    id: "user3".to_string(),
                    username: "charlie".to_string(),
                    display_name: "Charlie".to_string(),
                    is_active: true,
                    is_self: false,
                },
            ],
        },
    };

    attendance_relay.on_event(&event4).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Should get EditMessage with only Bob and Charlie
    let cmd = cmd_rx.recv().await.unwrap();
    match cmd {
        Command::EditMessage { message_id, new_body, .. } => {
            assert_eq!(message_id, "msg_123");
            assert!(!new_body.contains("- Alice"));
            assert!(new_body.contains("- Bob"));
            assert!(new_body.contains("- Charlie"));
        }
        _ => panic!("Expected EditMessage command"),
    }
}

#[tokio::test]
async fn test_attendance_relay_session_end() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Active participants:".to_string(),
            session_end_message: "Session summary".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );

    // First event: Start session with Alice
    let event1 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![User {
                id: "user1".to_string(),
                username: "alice".to_string(),
                display_name: "Alice".to_string(),
                is_active: true,
                is_self: false,
            }],
        },
    };

    attendance_relay.on_event(&event1).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Drain the initial SendRoomMessage
    cmd_rx.try_recv().unwrap();

    // Second event: Everyone leaves (session end: 1 → 0 users)
    let event2 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate { users: vec![] },
    };

    attendance_relay.on_event(&event2).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Without a real command handler, the middleware might not have
    // captured the message_id, so it might not send EditMessage.
    // The session summary message should always be sent though.

    // Try to get the first command - it might be EditMessage or SendRoomMessage
    let cmd1 = cmd_rx.try_recv();
    let cmd2 = cmd_rx.try_recv();

    // At least one command should have been sent (the summary)
    assert!(cmd1.is_ok() || cmd2.is_ok());

    // Check if we got both commands or just the summary
    let mut got_summary = false;

    if let Ok(cmd) = cmd1 {
        match cmd {
            Command::EditMessage { service_id, new_body, .. } => {
                assert_eq!(service_id.0, "matrix");
                assert_eq!(new_body, "Session has ended");
            }
            Command::SendRoomMessage { service_id, room_id, body, .. } => {
                assert_eq!(service_id.0, "matrix");
                assert_eq!(room_id, "!test:example.com");
                assert!(body.contains("Session summary"));
                assert!(body.contains("Duration:"));
                assert!(body.contains("Participants:"));
                assert!(body.contains("- Alice"));
                got_summary = true;
            }
            _ => {}
        }
    }

    if let Ok(cmd) = cmd2 {
        match cmd {
            Command::EditMessage { service_id, new_body, .. } => {
                assert_eq!(service_id.0, "matrix");
                assert_eq!(new_body, "Session has ended");
            }
            Command::SendRoomMessage { service_id, room_id, body, .. } => {
                assert_eq!(service_id.0, "matrix");
                assert_eq!(room_id, "!test:example.com");
                assert!(body.contains("Session summary"));
                assert!(body.contains("Duration:"));
                assert!(body.contains("Participants:"));
                assert!(body.contains("- Alice"));
                got_summary = true;
            }
            _ => {}
        }
    }

    // We should have at least gotten the summary
    assert!(got_summary, "Expected to receive session summary message");
}

#[tokio::test]
async fn test_attendance_relay_ignores_wrong_service() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Active participants:".to_string(),
            session_end_message: "Session summary".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );

    // Event from different service
    let event = Event {
        service_id: ServiceId("different_service".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![User {
                id: "user1".to_string(),
                username: "alice".to_string(),
                display_name: "Alice".to_string(),
                is_active: true,
                is_self: false,
            }],
        },
    };

    let result = attendance_relay.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT have sent any command (wrong service)
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_attendance_relay_ignores_non_userlist_events() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Active participants:".to_string(),
            session_end_message: "Session summary".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );

    // RoomMessage event instead of UserListUpdate
    let event = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::RoomMessage {
            room_id: "general".to_string(),
            body: "Hello!".to_string(),
            is_local_user: false,
            sender_id: "alice".to_string(),
            sender_display_name: Some("Alice".to_string()),
            is_self: false,
        },
    };

    let result = attendance_relay.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

    // Should NOT have sent any command (wrong event type)
    assert!(cmd_rx.try_recv().is_err());
}

#[tokio::test]
async fn test_attendance_relay_filters_self_user() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Active participants:".to_string(),
            session_end_message: "Session summary".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );

    // Event with only the bot (self) user
    let event = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![
                User {
                    id: "bot".to_string(),
                    username: "kelvinbot".to_string(),
                    display_name: "KelvinBot".to_string(),
                    is_active: true,
                    is_self: true, // Bot itself
                },
                User {
                    id: "user1".to_string(),
                    username: "alice".to_string(),
                    display_name: "Alice".to_string(),
                    is_active: true,
                    is_self: false,
                },
            ],
        },
    };

    let result = attendance_relay.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Should have sent a message, but only with Alice (bot filtered out)
    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::SendRoomMessage { body, .. } => {
            assert!(body.contains("- Alice"));
            assert!(!body.contains("KelvinBot"));
        }
        _ => panic!("Expected SendRoomMessage command"),
    }
}

#[tokio::test]
async fn test_attendance_relay_filters_inactive_users() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Active participants:".to_string(),
            session_end_message: "Session summary".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );

    // Event with both active and inactive users
    let event = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![
                User {
                    id: "user1".to_string(),
                    username: "alice".to_string(),
                    display_name: "Alice".to_string(),
                    is_active: true,
                    is_self: false,
                },
                User {
                    id: "user2".to_string(),
                    username: "bob".to_string(),
                    display_name: "Bob".to_string(),
                    is_active: false, // Inactive
                    is_self: false,
                },
            ],
        },
    };

    let result = attendance_relay.on_event(&event);
    assert_ok!(&result);

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Should have sent a message, but only with active Alice (Bob filtered out)
    let cmd = cmd_rx.try_recv();
    assert!(cmd.is_ok());
    match cmd.unwrap() {
        Command::SendRoomMessage { body, .. } => {
            assert!(body.contains("- Alice"));
            assert!(!body.contains("Bob"));
        }
        _ => panic!("Expected SendRoomMessage command"),
    }
}

#[tokio::test]
async fn test_attendance_relay_tracks_all_participants() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let attendance_relay = AttendanceRelay::new(
        make_ctx(cmd_tx),
        AttendanceRelayConfig {
            source_service_id: "dummy".to_string(),
            source_room_id: None,
            dest_service_id: "matrix".to_string(),
            dest_room_id: "!test:example.com".to_string(),
            session_start_message: "Active participants:".to_string(),
            session_end_message: "Session summary".to_string(),
            session_ended_edit_message: "Session has ended".to_string(),
        },
    );

    // Event 1: Alice joins
    let event1 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![User {
                id: "user1".to_string(),
                username: "alice".to_string(),
                display_name: "Alice".to_string(),
                is_active: true,
                is_self: false,
            }],
        },
    };

    attendance_relay.on_event(&event1).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    cmd_rx.try_recv().unwrap(); // Drain

    // Event 2: Bob joins (Alice still active)
    let event2 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![
                User {
                    id: "user1".to_string(),
                    username: "alice".to_string(),
                    display_name: "Alice".to_string(),
                    is_active: true,
                    is_self: false,
                },
                User {
                    id: "user2".to_string(),
                    username: "bob".to_string(),
                    display_name: "Bob".to_string(),
                    is_active: true,
                    is_self: false,
                },
            ],
        },
    };

    attendance_relay.on_event(&event2).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    cmd_rx.try_recv().unwrap(); // Drain

    // Event 3: Alice leaves, only Bob active
    let event3 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate {
            users: vec![User {
                id: "user2".to_string(),
                username: "bob".to_string(),
                display_name: "Bob".to_string(),
                is_active: true,
                is_self: false,
            }],
        },
    };

    attendance_relay.on_event(&event3).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    cmd_rx.try_recv().unwrap(); // Drain

    // Event 4: Everyone leaves - session ends
    let event4 = Event {
        service_id: ServiceId("dummy".to_string()),
        kind: EventKind::UserListUpdate { users: vec![] },
    };

    attendance_relay.on_event(&event4).unwrap();
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Drain all pending commands and find the summary
    let mut summary_found = false;
    while let Ok(cmd) = cmd_rx.try_recv() {
        // Ignore EditMessage and other commands, only check SendRoomMessage
        if let Command::SendRoomMessage { body, .. } = cmd
            && body.contains("Session summary")
        {
            assert!(body.contains("- Alice"));
            assert!(body.contains("- Bob"));
            summary_found = true;
        }
    }

    assert!(summary_found, "Expected to find session summary message with all participants");
}

#[tokio::test]
async fn test_attendance_relay_instantiation_from_config() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "test_attendance_relay".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::AttendanceRelay {
                source_service_id: "dummy".to_string(),
                source_room_id: None,
                dest_service_id: "matrix".to_string(),
                dest_room_id: "!announcements:matrix.org".to_string(),
                session_start_message: "Session in progress".to_string(),
                session_end_message: "Session completed".to_string(),
                session_ended_edit_message: "Session has ended".to_string(),
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
        reconnection: ReconnectionConfig::default(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert_ok!(&result);

    let middlewares = result.unwrap();
    assert_eq!(middlewares.len(), 1);
    assert!(middlewares.contains_key("test_attendance_relay"));
}

// Weekly Gathering Middleware Tests

use chrono::{Local, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use kelvin_bot::middlewares::weekly_gathering::{
    Household, WeeklyGathering, WeeklyGatheringConfig, next_event_date_from, parse_event_times,
};

/// Run `post_finalization` and return the body of the message it posts.
///
/// Finalization now waits for the send to report the new message's ID (so a time pick can be
/// applied to it later), so the response channel has to be answered concurrently.
async fn finalize_and_capture(
    middleware: &WeeklyGathering,
    cmd_rx: &mut tokio::sync::mpsc::Receiver<Command>,
    message_id: &str,
) -> String {
    let capture = async {
        match cmd_rx.recv().await.expect("expected a finalization command") {
            Command::SendRoomMessage { body, response_tx, .. } => {
                response_tx
                    .expect("finalization should request a message ID")
                    .send(Ok(message_id.to_string()))
                    .expect("failed to answer finalization response");
                body
            }
            _ => panic!("Expected SendRoomMessage command"),
        }
    };

    let (_, body) =
        tokio::join!(middleware.post_finalization(middleware.test_event_date()), capture);
    body
}

/// Drain any `AddReaction` commands seeded on the finalization message, returning their keys.
fn drain_reaction_keys(cmd_rx: &mut tokio::sync::mpsc::Receiver<Command>) -> Vec<String> {
    let mut keys = Vec::new();
    while let Ok(Command::AddReaction { key, .. }) = cmd_rx.try_recv() {
        keys.push(key);
    }
    keys
}

fn create_weekly_gathering_config() -> WeeklyGatheringConfig {
    WeeklyGatheringConfig {
        service_id: "matrix".to_string(),
        room_id: "!test:example.com".to_string(),
        event_day_of_week: Weekday::Sat,
        event_time_options: parse_event_times("19:00").unwrap(),
        finalize_time: NaiveTime::from_hms_opt(17, 0, 0).unwrap(),
        poll_open_minutes: 4200, // ~2.9 days
        reaction_virtual: "💻".to_string(),
        reaction_in_person: "🏠".to_string(),
        reaction_host: "🙋".to_string(),
        announcement_message: "Weekly Gathering Poll!".to_string(),
        finalization_virtual_message: "This week is VIRTUAL! Host: {host}. {virtual_count} virtual, {in_person_count} in-person votes.".to_string(),
        finalization_in_person_message: "This week is IN-PERSON! Host: {host}. {virtual_count} virtual, {in_person_count} in-person votes.".to_string(),
        finalization_no_votes_message: "No votes received - gathering cancelled.".to_string(),
        time_prompt_message: "Pick a time!".to_string(),
        households: vec![],
    }
}

fn make_weekly_gathering(cmd_tx: Sender<Command>) -> WeeklyGathering {
    WeeklyGathering::new(make_ctx(cmd_tx), create_weekly_gathering_config())
}

#[tokio::test]
async fn test_weekly_gathering_middleware_run() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let weekly_gathering = make_weekly_gathering(cmd_tx);
    let cancel_token = CancellationToken::new();

    // WeeklyGathering run should complete when cancelled
    cancel_token.cancel();
    let result = weekly_gathering.run(cancel_token).await;
    assert_ok!(result);
}

#[test]
fn test_weekly_gathering_host_selection_empty() {
    use std::collections::HashSet;
    let volunteers: HashSet<String> = HashSet::new();
    let history = HashMap::new();
    let result = WeeklyGathering::select_host(&volunteers, &history, &[]);
    assert!(result.is_none());
}

#[test]
fn test_weekly_gathering_host_selection_prefers_least_recently_hosted() {
    use std::collections::HashSet;
    let mut volunteers = HashSet::new();
    volunteers.insert("user1".to_string());
    volunteers.insert("user2".to_string());

    // user1 hosted recently; user2 has no history → user2 should always be chosen
    let mut history = HashMap::new();
    history.insert("user1".to_string(), Utc::now());

    for _ in 0..10 {
        let result = WeeklyGathering::select_host(&volunteers, &history, &[]);
        assert_eq!(result.map(|(id, _)| id), Some("user2".to_string()));
    }
}

#[test]
fn test_weekly_gathering_host_selection_only_candidate_chosen_despite_history() {
    use std::collections::HashSet;
    let mut volunteers = HashSet::new();
    volunteers.insert("user1".to_string());

    // user1 is the only volunteer; must be chosen even though they hosted recently
    let mut history = HashMap::new();
    history.insert("user1".to_string(), Utc::now());

    let result = WeeklyGathering::select_host(&volunteers, &history, &[]);
    assert_eq!(result.map(|(id, _)| id), Some("user1".to_string()));
}

// Functional tests for vote processing and state transitions

#[tokio::test]
async fn test_weekly_gathering_ignores_reactions_when_idle() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    // Don't set phase to Announced - middleware starts in Idle
    // Process a reaction - should be ignored since we're in Idle phase
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "user1".to_string())
        .await;

    let (virtual_count, in_person_count, host_count) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 0);
    assert_eq!(in_person_count, 0);
    assert_eq!(host_count, 0);
}

#[tokio::test]
async fn test_weekly_gathering_records_virtual_votes() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // Two users vote virtual
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "bob".to_string())
        .await;

    let (virtual_count, in_person_count, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 2);
    assert_eq!(in_person_count, 0);
}

#[tokio::test]
async fn test_weekly_gathering_records_in_person_votes() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // Three users vote in-person
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "bob".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "charlie".to_string())
        .await;

    let (virtual_count, in_person_count, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 0);
    assert_eq!(in_person_count, 3);
}

#[tokio::test]
async fn test_weekly_gathering_dual_vote_counts_for_both() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // Alice reacts with both virtual and in-person — she should count in both sets
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "alice".to_string())
        .await;

    let (virtual_count, in_person_count, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 1);
    assert_eq!(in_person_count, 1);
}

#[tokio::test]
async fn test_weekly_gathering_explicit_reaction_removal_switches_vote() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // Alice votes virtual
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;

    let (virtual_count, in_person_count, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 1);
    assert_eq!(in_person_count, 0);

    // Alice explicitly removes her virtual reaction then adds in-person
    middleware
        .test_process_reaction_removed(
            Some("msg123".to_string()),
            Some("💻".to_string()),
            "alice".to_string(),
        )
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "alice".to_string())
        .await;

    let (virtual_count, in_person_count, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 0);
    assert_eq!(in_person_count, 1);
}

#[tokio::test]
async fn test_weekly_gathering_host_volunteers() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // Two users volunteer to host
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), "bob".to_string())
        .await;

    let (_, _, host_count) = middleware.get_vote_counts().await;
    assert_eq!(host_count, 2);

    let volunteers = middleware.get_host_volunteers().await;
    assert!(volunteers.contains("alice"));
    assert!(volunteers.contains("bob"));
}

#[tokio::test]
async fn test_weekly_gathering_reaction_removal() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // Alice votes virtual
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;

    let (virtual_count, _, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 1);

    // Alice removes her vote
    middleware
        .test_process_reaction_removed(
            Some("msg123".to_string()),
            Some("💻".to_string()),
            "alice".to_string(),
        )
        .await;

    let (virtual_count, _, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 0);
}

#[tokio::test]
async fn test_weekly_gathering_ignores_reactions_on_wrong_message() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // Reaction on a different message should be ignored
    middleware
        .test_process_reaction_added("other_msg".to_string(), "💻".to_string(), "alice".to_string())
        .await;

    let (virtual_count, _, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 0);
}

#[tokio::test]
async fn test_weekly_gathering_finalization_virtual_wins() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // 3 virtual, 1 in-person
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "bob".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "charlie".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "dave".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), "alice".to_string())
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("VIRTUAL"));
    assert!(body.contains("3 virtual"));
    assert!(body.contains("1 in-person"));
    assert!(body.contains("alice")); // Host
}

#[tokio::test]
async fn test_weekly_gathering_finalization_in_person_wins() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // 1 virtual, 3 in-person
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "bob".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "charlie".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "dave".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), "bob".to_string())
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("IN-PERSON"));
    assert!(body.contains("1 virtual"));
    assert!(body.contains("3 in-person"));
    assert!(body.contains("bob")); // Host
}

#[tokio::test]
async fn test_weekly_gathering_finalization_tie_prefers_virtual() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // 2 virtual, 2 in-person - tie should prefer virtual
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "bob".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "charlie".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), "dave".to_string())
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("VIRTUAL"));
    assert!(body.contains("2 virtual"));
    assert!(body.contains("2 in-person"));
}

#[tokio::test]
async fn test_weekly_gathering_finalization_no_votes() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // No votes at all
    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("No votes received"));
}

#[tokio::test]
async fn test_weekly_gathering_finalization_no_host_volunteer() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // Votes but no host volunteer
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("No host volunteered"));
}

#[tokio::test]
async fn test_weekly_gathering_finalization_prefers_least_recently_hosted() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // alice hosted recently; bob has no history → bob should be chosen
    let mut history = HashMap::new();
    history.insert("alice".to_string(), Utc::now());
    middleware.set_host_history(history).await;

    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), "bob".to_string())
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("bob"));
}

#[tokio::test]
async fn test_weekly_gathering_finalization_sole_volunteer_chosen_despite_history() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // alice hosted recently and is the only volunteer → must still be chosen
    let mut history = HashMap::new();
    history.insert("alice".to_string(), Utc::now());
    middleware.set_host_history(history).await;

    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), "alice".to_string())
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("alice"));
}

#[tokio::test]
async fn test_weekly_gathering_instantiation_from_config() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let data_dir = TempDir::new().unwrap();

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "test_weekly_gathering".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::WeeklyGathering {
                service_id: "matrix".to_string(),
                room_id: "!gathering:matrix.org".to_string(),
                event_day_of_week: "Saturday".to_string(),
                event_time_options: "16:30,20:00".to_string(),
                finalize_time: "14:00".to_string(),
                poll_open_minutes: 4200,
                reaction_virtual: "💻".to_string(),
                reaction_in_person: "🏠".to_string(),
                reaction_host: "🙋".to_string(),
                announcement_message: "Weekly poll!".to_string(),
                finalization_virtual_message: "Virtual!".to_string(),
                finalization_in_person_message: "In-person!".to_string(),
                finalization_no_votes_message: "No votes!".to_string(),
                time_prompt_message: "Pick a time!".to_string(),
                households: HashMap::new(),
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: data_dir.path().to_path_buf(),
        reconnection: ReconnectionConfig::default(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert_ok!(&result);

    let middlewares = result.unwrap();
    assert_eq!(middlewares.len(), 1);
    assert!(middlewares.contains_key("test_weekly_gathering"));
}

#[tokio::test]
async fn test_weekly_gathering_instantiation_invalid_day_of_week() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let data_dir = TempDir::new().unwrap();

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "test_weekly_gathering".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::WeeklyGathering {
                service_id: "matrix".to_string(),
                room_id: "!gathering:matrix.org".to_string(),
                event_day_of_week: "InvalidDay".to_string(),
                event_time_options: "19:00".to_string(),
                finalize_time: "14:00".to_string(),
                poll_open_minutes: 4200,
                reaction_virtual: "💻".to_string(),
                reaction_in_person: "🏠".to_string(),
                reaction_host: "🙋".to_string(),
                announcement_message: "Weekly poll!".to_string(),
                finalization_virtual_message: "Virtual!".to_string(),
                finalization_in_person_message: "In-person!".to_string(),
                finalization_no_votes_message: "No votes!".to_string(),
                time_prompt_message: "Pick a time!".to_string(),
                households: HashMap::new(),
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: data_dir.path().to_path_buf(),
        reconnection: ReconnectionConfig::default(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("invalid") && err_msg.contains("day"));
}

#[tokio::test]
async fn test_weekly_gathering_instantiation_invalid_time_format() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let data_dir = TempDir::new().unwrap();

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "test_weekly_gathering".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::WeeklyGathering {
                service_id: "matrix".to_string(),
                room_id: "!gathering:matrix.org".to_string(),
                event_day_of_week: "Saturday".to_string(),
                event_time_options: "19:00".to_string(),
                finalize_time: "2pm".to_string(),
                poll_open_minutes: 4200,
                reaction_virtual: "💻".to_string(),
                reaction_in_person: "🏠".to_string(),
                reaction_host: "🙋".to_string(),
                announcement_message: "Weekly poll!".to_string(),
                finalization_virtual_message: "Virtual!".to_string(),
                finalization_in_person_message: "In-person!".to_string(),
                finalization_no_votes_message: "No votes!".to_string(),
                time_prompt_message: "Pick a time!".to_string(),
                households: HashMap::new(),
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: data_dir.path().to_path_buf(),
        reconnection: ReconnectionConfig::default(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert!(result.is_err());
    let err_msg = result.err().unwrap().to_string();
    assert!(err_msg.contains("invalid") && err_msg.contains("time"));
}

// Household grouping tests

#[test]
fn test_select_host_household_deduplication() {
    use std::collections::HashSet;

    // @hayden and @gun are the same household; @alice is solo.
    // Give the household a very recent history so @alice always wins.
    let households = vec![Household {
        name: "Hayden and Gunnar".to_string(),
        members: vec!["@hayden".to_string(), "@gun".to_string()],
    }];

    let mut volunteers = HashSet::new();
    volunteers.insert("@hayden".to_string());
    volunteers.insert("@gun".to_string());
    volunteers.insert("@alice".to_string());

    let mut history = HashMap::new();
    history.insert("@hayden".to_string(), Utc::now());
    history.insert("@gun".to_string(), Utc::now());
    // @alice has no history → epoch → always preferred

    for _ in 0..10 {
        let result = WeeklyGathering::select_host(&volunteers, &history, &households);
        assert_eq!(result.map(|(id, _)| id), Some("@alice".to_string()));
    }
}

#[test]
fn test_select_host_household_effective_time() {
    use std::collections::HashSet;

    // Household: [@hayden, @gun]. @gun hosted 1 day ago.
    // @alice hosted 2 days ago → @alice is older, so @alice should be preferred.
    let households = vec![Household {
        name: "Hayden and Gunnar".to_string(),
        members: vec!["@hayden".to_string(), "@gun".to_string()],
    }];

    let mut volunteers = HashSet::new();
    volunteers.insert("@hayden".to_string());
    volunteers.insert("@alice".to_string());

    let mut history = HashMap::new();
    let one_day_ago = Utc::now() - chrono::Duration::days(1);
    let two_days_ago = Utc::now() - chrono::Duration::days(2);
    history.insert("@gun".to_string(), one_day_ago); // household effective time = 1 day ago
    history.insert("@alice".to_string(), two_days_ago); // alice = 2 days ago → older → preferred

    for _ in 0..10 {
        let result = WeeklyGathering::select_host(&volunteers, &history, &households);
        assert_eq!(result.map(|(id, _)| id), Some("@alice".to_string()));
    }
}

#[test]
fn test_select_host_household_display_name() {
    use std::collections::HashSet;

    let households = vec![Household {
        name: "Hayden and Gunnar".to_string(),
        members: vec!["@hayden".to_string(), "@gun".to_string()],
    }];

    let mut volunteers = HashSet::new();
    volunteers.insert("@hayden".to_string());

    let history = HashMap::new();

    let result = WeeklyGathering::select_host(&volunteers, &history, &households);
    assert!(result.is_some());
    let (user_id, display_name) = result.unwrap();
    assert_eq!(user_id, "@hayden");
    assert_eq!(display_name, "Hayden and Gunnar");
}

#[tokio::test]
async fn test_finalization_propagates_history_to_all_household_members() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let store = Arc::new(PersistentStore::in_memory());

    let mut config = create_weekly_gathering_config();
    config.households = vec![Household {
        name: "Hayden and Gunnar".to_string(),
        members: vec!["@hayden".to_string(), "@gun".to_string()],
    }];

    let middleware = WeeklyGathering::new(make_ctx_with_store(cmd_tx, store.clone()), config);

    middleware.set_announced("msg123".to_string()).await;

    // @hayden is the only host volunteer (but @gun is in the same household)
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "@hayden".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), "@hayden".to_string())
        .await;

    finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    // Both household members should have the same timestamp in host_history
    let host_history: HashMap<String, chrono::DateTime<Utc>> =
        store.get("host_history").await.expect("host history should be persisted");

    assert!(host_history.contains_key("@hayden"), "@hayden should be in host history");
    assert!(host_history.contains_key("@gun"), "@gun should be in host history (same household)");
    assert_eq!(
        host_history["@hayden"], host_history["@gun"],
        "both household members should have the same timestamp"
    );
}

#[tokio::test]
async fn test_finalization_household_display_name_in_message() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);

    let mut config = create_weekly_gathering_config();
    config.households = vec![Household {
        name: "Hayden and Gunnar".to_string(),
        members: vec!["@hayden".to_string(), "@gun".to_string()],
    }];

    let middleware = WeeklyGathering::new(make_ctx(cmd_tx), config);

    middleware.set_announced("msg123".to_string()).await;

    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "@hayden".to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), "@hayden".to_string())
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(
        body.contains("Hayden and Gunnar"),
        "message should use household display name, got: {body}"
    );
}

#[tokio::test]
async fn test_weekly_gathering_finalization_virtual_wins_when_some_voted_both() {
    // Regression: users who reacted with both emojis previously had their virtual vote
    // silently removed, causing in-person to win despite virtual having more raw reactions.
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // 4 users react 💻 (virtual). 2 of them also react 🏠.
    // Virtual raw reactions: 4, in-person raw reactions: 2 → virtual should win.
    for user in ["alice", "bob", "charlie", "dave"] {
        middleware
            .test_process_reaction_added("msg123".to_string(), "💻".to_string(), user.to_string())
            .await;
    }
    for user in ["alice", "bob"] {
        middleware
            .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), user.to_string())
            .await;
    }

    let (virtual_count, in_person_count, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 4);
    assert_eq!(in_person_count, 2);

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("VIRTUAL"), "virtual should win with 4 vs 2, got: {body}");
}

#[tokio::test]
async fn test_weekly_gathering_finalization_in_person_wins_when_some_voted_both() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(10);
    let middleware = make_weekly_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // 4 users react 🏠. 2 of them also react 💻.
    // In-person raw reactions: 4, virtual raw reactions: 2 → in-person should win.
    for user in ["alice", "bob", "charlie", "dave"] {
        middleware
            .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), user.to_string())
            .await;
    }
    for user in ["alice", "bob"] {
        middleware
            .test_process_reaction_added("msg123".to_string(), "💻".to_string(), user.to_string())
            .await;
    }

    let (virtual_count, in_person_count, _) = middleware.get_vote_counts().await;
    assert_eq!(virtual_count, 2);
    assert_eq!(in_person_count, 4);

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("IN-PERSON"), "in-person should win with 4 vs 2, got: {body}");
}

#[tokio::test]
async fn test_weekly_gathering_instantiation_with_households() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let data_dir = TempDir::new().unwrap();

    use kelvin_bot::core::config::HouseholdCfg;

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "test_weekly_gathering".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::WeeklyGathering {
                service_id: "matrix".to_string(),
                room_id: "!gathering:matrix.org".to_string(),
                event_day_of_week: "Saturday".to_string(),
                event_time_options: "16:30,20:00".to_string(),
                finalize_time: "14:00".to_string(),
                poll_open_minutes: 4200,
                reaction_virtual: "💻".to_string(),
                reaction_in_person: "🏠".to_string(),
                reaction_host: "🙋".to_string(),
                announcement_message: "Weekly poll!".to_string(),
                finalization_virtual_message: "Virtual!".to_string(),
                finalization_in_person_message: "In-person!".to_string(),
                finalization_no_votes_message: "No votes!".to_string(),
                time_prompt_message: "Pick a time!".to_string(),
                households: {
                    let mut m = HashMap::new();
                    m.insert(
                        "h1".to_string(),
                        HouseholdCfg {
                            name: "Hayden and Gunnar".to_string(),
                            members: "@hayden:warmitup.chat,@gun:warmitup.chat".to_string(),
                        },
                    );
                    m
                },
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: data_dir.path().to_path_buf(),
        reconnection: ReconnectionConfig::default(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    assert_ok!(&result);
    assert_eq!(result.unwrap().len(), 1);
}

// Event time voting

/// Config with three voteable start times, wired into templates that surface the new placeholders.
fn create_time_voting_config() -> WeeklyGatheringConfig {
    let mut config = create_weekly_gathering_config();
    config.event_time_options = parse_event_times("16:30,20:00,21:30").unwrap();
    config.announcement_message = "Poll!\n{time_options}".to_string();
    config.finalization_virtual_message =
        "VIRTUAL at {event_time}. Host: {host}.\n{time_results}\n{time_prompt}".to_string();
    config.finalization_in_person_message =
        "IN-PERSON at {event_time}. Host: {host}.\n{time_results}\n{time_prompt}".to_string();
    config
}

fn make_time_voting_gathering(cmd_tx: Sender<Command>) -> WeeklyGathering {
    WeeklyGathering::new(make_ctx(cmd_tx), create_time_voting_config())
}

/// Cast in-person and host votes so finalization picks `host` and awaits their time pick.
async fn vote_in_person_with_host(middleware: &WeeklyGathering, host: &str) {
    middleware.set_announced("msg123".to_string()).await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🏠".to_string(), host.to_string())
        .await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🙋".to_string(), host.to_string())
        .await;
}

#[test]
fn test_parse_event_times_assigns_keycaps_in_order() {
    let options = parse_event_times("16:30, 20:00").unwrap();

    assert_eq!(options.len(), 2);
    assert_eq!(options[0].reaction, "1\u{fe0f}\u{20e3}");
    assert_eq!(options[0].label, "4:30pm");
    assert_eq!(options[1].reaction, "2\u{fe0f}\u{20e3}");
    assert_eq!(options[1].label, "8:00pm");
}

#[test]
fn test_parse_event_times_empty_yields_no_options() {
    // Rejected at the config layer; see test_weekly_gathering_instantiation_requires_time_options
    assert!(parse_event_times("").unwrap().is_empty());
    assert!(parse_event_times("  ").unwrap().is_empty());
}

#[test]
fn test_parse_event_times_rejects_bad_input() {
    assert!(parse_event_times("7pm").is_err(), "unparseable time should be rejected");
    assert!(parse_event_times("16:30,16:30").is_err(), "duplicate times should be rejected");

    let eleven = (0..11).map(|h| format!("{h:02}:00")).collect::<Vec<_>>().join(",");
    assert!(parse_event_times(&eleven).is_err(), "more than 10 options should be rejected");
}

#[tokio::test]
async fn test_time_votes_recorded_and_removed() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_time_voting_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // alice is free at two of the three times; bob only at the second
    middleware
        .test_process_reaction_added(
            "msg123".to_string(),
            "1\u{fe0f}\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;
    middleware
        .test_process_reaction_added(
            "msg123".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;
    middleware
        .test_process_reaction_added(
            "msg123".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "bob".to_string(),
        )
        .await;

    assert_eq!(middleware.get_time_votes().await, vec![1, 2, 0]);

    middleware
        .test_process_reaction_removed(
            Some("msg123".to_string()),
            Some("1\u{fe0f}\u{20e3}".to_string()),
            "alice".to_string(),
        )
        .await;

    assert_eq!(middleware.get_time_votes().await, vec![0, 2, 0]);
}

#[tokio::test]
async fn test_time_vote_matches_keycap_without_variation_selector() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_time_voting_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;

    // Some clients send keycaps without U+FE0F; both spellings are the same option
    middleware
        .test_process_reaction_added(
            "msg123".to_string(),
            "1\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;

    assert_eq!(middleware.get_time_votes().await, vec![1, 0, 0]);
}

#[tokio::test]
async fn test_unknown_reaction_records_no_vote() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middleware = make_time_voting_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "🎉".to_string(), "alice".to_string())
        .await;

    assert_eq!(middleware.get_time_votes().await, vec![0, 0, 0]);
    assert_eq!(middleware.get_vote_counts().await, (0, 0, 0));
}

#[tokio::test]
async fn test_virtual_finalization_auto_picks_most_voted_time() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;
    for user in ["alice", "bob"] {
        middleware
            .test_process_reaction_added("msg123".to_string(), "💻".to_string(), user.to_string())
            .await;
        middleware
            .test_process_reaction_added(
                "msg123".to_string(),
                "3\u{fe0f}\u{20e3}".to_string(),
                user.to_string(),
            )
            .await;
    }
    middleware
        .test_process_reaction_added(
            "msg123".to_string(),
            "1\u{fe0f}\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("VIRTUAL at ") && body.contains(" at 9:30pm"), "got: {body}");
    assert!(body.contains("3\u{fe0f}\u{20e3} 9:30pm — 2 votes **← selected**"), "got: {body}");
    assert!(!body.contains("Pick a time!"), "virtual needs no host pick, got: {body}");
    assert_eq!(middleware.get_selected_time().await, Some(2));
    assert!(drain_reaction_keys(&mut cmd_rx).is_empty(), "no pick reactions should be seeded");
}

#[tokio::test]
async fn test_virtual_finalization_time_tie_prefers_earlier() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;
    // 8:00pm and 9:30pm each get one vote — the earlier one wins
    middleware
        .test_process_reaction_added(
            "msg123".to_string(),
            "3\u{fe0f}\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;
    middleware
        .test_process_reaction_added(
            "msg123".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "bob".to_string(),
        )
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("VIRTUAL at ") && body.contains(" at 8:00pm"), "got: {body}");
    assert_eq!(middleware.get_selected_time().await, Some(1));
}

#[tokio::test]
async fn test_virtual_finalization_without_time_votes_is_tbd() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    middleware.set_announced("msg123".to_string()).await;
    middleware
        .test_process_reaction_added("msg123".to_string(), "💻".to_string(), "alice".to_string())
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("VIRTUAL at ") && body.contains(" (time TBD)"), "got: {body}");
    assert_eq!(middleware.get_selected_time().await, None);
}

#[tokio::test]
async fn test_in_person_finalization_awaits_host_pick() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    vote_in_person_with_host(&middleware, "alice").await;
    // 9:30pm is the most popular, 8:00pm second, 4:30pm unvoted
    for user in ["alice", "bob"] {
        middleware
            .test_process_reaction_added(
                "msg123".to_string(),
                "3\u{fe0f}\u{20e3}".to_string(),
                user.to_string(),
            )
            .await;
    }
    middleware
        .test_process_reaction_added(
            "msg123".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "bob".to_string(),
        )
        .await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("IN-PERSON at ") && body.contains(" (time TBD)"), "got: {body}");
    assert!(body.contains("Pick a time!"), "host should be prompted, got: {body}");
    assert!(!body.contains("← selected"), "nothing is selected yet, got: {body}");
    assert_eq!(middleware.get_selected_time().await, None);

    // Every configured time is offered, ordered most-preferred first
    assert_eq!(
        drain_reaction_keys(&mut cmd_rx),
        vec!["3\u{fe0f}\u{20e3}", "2\u{fe0f}\u{20e3}", "1\u{fe0f}\u{20e3}"]
    );
}

#[tokio::test]
async fn test_host_pick_edits_finalization_message() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    vote_in_person_with_host(&middleware, "alice").await;
    finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;
    drain_reaction_keys(&mut cmd_rx);

    middleware
        .test_process_reaction_added(
            "final1".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;

    assert_eq!(middleware.get_selected_time().await, Some(1));
    match cmd_rx.try_recv().expect("expected an edit command") {
        Command::EditMessage { message_id, new_body, .. } => {
            assert_eq!(message_id, "final1");
            assert!(
                new_body.contains("IN-PERSON at ") && new_body.contains(" at 8:00pm"),
                "got: {new_body}"
            );
            assert!(new_body.contains("2\u{fe0f}\u{20e3} 8:00pm — 0 votes **← selected**"));
            assert!(!new_body.contains("Pick a time!"), "prompt should be gone: {new_body}");
        }
        _ => panic!("Expected EditMessage command"),
    }
}

#[tokio::test]
async fn test_host_can_change_pick() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    vote_in_person_with_host(&middleware, "alice").await;
    finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;
    drain_reaction_keys(&mut cmd_rx);

    for key in ["2\u{fe0f}\u{20e3}", "1\u{fe0f}\u{20e3}"] {
        middleware
            .test_process_reaction_added("final1".to_string(), key.to_string(), "alice".to_string())
            .await;
    }

    assert_eq!(middleware.get_selected_time().await, Some(0), "the later pick wins");
}

#[tokio::test]
async fn test_host_removing_pick_reverts_to_pending() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    vote_in_person_with_host(&middleware, "alice").await;
    finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;
    drain_reaction_keys(&mut cmd_rx);

    middleware
        .test_process_reaction_added(
            "final1".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;
    let _ = cmd_rx.try_recv();

    middleware
        .test_process_reaction_removed(
            Some("final1".to_string()),
            Some("2\u{fe0f}\u{20e3}".to_string()),
            "alice".to_string(),
        )
        .await;

    assert_eq!(middleware.get_selected_time().await, None);
    match cmd_rx.try_recv().expect("expected an edit command") {
        Command::EditMessage { new_body, .. } => {
            assert!(
                new_body.contains("IN-PERSON at ") && new_body.contains(" (time TBD)"),
                "got: {new_body}"
            );
            assert!(new_body.contains("Pick a time!"), "prompt should return: {new_body}");
        }
        _ => panic!("Expected EditMessage command"),
    }
}

#[tokio::test]
async fn test_non_host_pick_is_ignored() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    vote_in_person_with_host(&middleware, "alice").await;
    finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;
    drain_reaction_keys(&mut cmd_rx);

    middleware
        .test_process_reaction_added(
            "final1".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "mallory".to_string(),
        )
        .await;

    assert_eq!(middleware.get_selected_time().await, None);
    assert!(cmd_rx.try_recv().is_err(), "no edit should be issued");
}

#[tokio::test]
async fn test_household_member_can_pick_for_host() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let mut config = create_time_voting_config();
    config.households = vec![Household {
        name: "Hayden and Gunnar".to_string(),
        members: vec!["@hayden".to_string(), "@gun".to_string()],
    }];
    let middleware = WeeklyGathering::new(make_ctx(cmd_tx), config);

    vote_in_person_with_host(&middleware, "@hayden").await;
    finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;
    drain_reaction_keys(&mut cmd_rx);

    // @gun didn't volunteer, but shares a household with the selected host
    middleware
        .test_process_reaction_added(
            "final1".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "@gun".to_string(),
        )
        .await;

    assert_eq!(middleware.get_selected_time().await, Some(1));
}

#[tokio::test]
async fn test_pick_on_other_message_is_ignored() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    vote_in_person_with_host(&middleware, "alice").await;
    finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;
    drain_reaction_keys(&mut cmd_rx);

    middleware
        .test_process_reaction_added(
            "msg123".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;

    assert_eq!(middleware.get_selected_time().await, None);
    assert!(cmd_rx.try_recv().is_err(), "no edit should be issued");
}

#[tokio::test]
async fn test_announcement_seeds_time_reactions_and_renders_options() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_time_voting_gathering(cmd_tx);

    let announce = async {
        match cmd_rx.recv().await.expect("expected an announcement command") {
            Command::SendRoomMessage { body, response_tx, .. } => {
                response_tx.unwrap().send(Ok("msg123".to_string())).unwrap();
                body
            }
            _ => panic!("Expected SendRoomMessage command"),
        }
    };
    let (_, body) = tokio::join!(middleware.test_post_announcement(), announce);

    assert!(body.contains("1\u{fe0f}\u{20e3} 4:30pm"), "got: {body}");
    assert!(body.contains("3\u{fe0f}\u{20e3} 9:30pm"), "got: {body}");

    assert_eq!(
        drain_reaction_keys(&mut cmd_rx),
        vec!["💻", "🏠", "🙋", "1\u{fe0f}\u{20e3}", "2\u{fe0f}\u{20e3}", "3\u{fe0f}\u{20e3}"]
    );
}

#[tokio::test]
async fn test_finalization_with_single_fixed_time() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let middleware = make_weekly_gathering(cmd_tx);

    vote_in_person_with_host(&middleware, "alice").await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    // A single option is a fixed time, not a poll: templates render exactly as before
    assert_eq!(
        body,
        "This week is IN-PERSON! Host: alice. 0 virtual, 1 in-person votes.".to_string()
    );
    assert!(drain_reaction_keys(&mut cmd_rx).is_empty(), "no pick reactions for a fixed time");
    assert_eq!(middleware.get_selected_time().await, Some(0), "the sole option is the time");

    // ...and a reaction on the finalization message changes nothing
    middleware
        .test_process_reaction_added(
            "final1".to_string(),
            "2\u{fe0f}\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;
    assert!(cmd_rx.try_recv().is_err(), "no edit should be issued");
}

#[test]
fn test_next_event_date_before_poll_closes_is_today() {
    // Saturday 09:00, poll closes 14:00 — the gathering is still today
    let now = Local.with_ymd_and_hms(2026, 8, 1, 9, 0, 0).unwrap();
    let finalize = NaiveTime::from_hms_opt(14, 0, 0).unwrap();

    let date = next_event_date_from(now, Weekday::Sat, finalize);

    assert_eq!(date, now.date_naive(), "before the poll closes, today is still the event day");
}

#[test]
fn test_next_event_date_after_poll_closes_rolls_to_next_week() {
    // Saturday 15:00, poll closed at 14:00 — this cycle is done
    let now = Local.with_ymd_and_hms(2026, 8, 1, 15, 0, 0).unwrap();
    let finalize = NaiveTime::from_hms_opt(14, 0, 0).unwrap();

    let date = next_event_date_from(now, Weekday::Sat, finalize);

    assert_eq!(date, now.date_naive() + chrono::Duration::days(7));
}

#[test]
fn test_next_event_date_from_other_days() {
    let finalize = NaiveTime::from_hms_opt(14, 0, 0).unwrap();

    // Wednesday → the coming Saturday
    let wednesday = Local.with_ymd_and_hms(2026, 7, 29, 18, 0, 0).unwrap();
    assert_eq!(
        next_event_date_from(wednesday, Weekday::Sat, finalize),
        NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()
    );

    // Sunday, the day after → nearly a full week out
    let sunday = Local.with_ymd_and_hms(2026, 8, 2, 10, 0, 0).unwrap();
    assert_eq!(
        next_event_date_from(sunday, Weekday::Sat, finalize),
        NaiveDate::from_ymd_opt(2026, 8, 8).unwrap()
    );
}

#[tokio::test]
async fn test_single_time_option_seeds_no_time_reactions() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let mut config = create_weekly_gathering_config();
    config.announcement_message = "Poll at {event_time}!".to_string();
    let middleware = WeeklyGathering::new(make_ctx(cmd_tx), config);

    let announce = async {
        match cmd_rx.recv().await.expect("expected an announcement command") {
            Command::SendRoomMessage { body, response_tx, .. } => {
                response_tx.unwrap().send(Ok("msg123".to_string())).unwrap();
                body
            }
            _ => panic!("Expected SendRoomMessage command"),
        }
    };
    let (_, body) = tokio::join!(middleware.test_post_announcement(), announce);

    // A fixed time is stated up front rather than put to a vote
    assert!(body.contains("at 7:00pm"), "got: {body}");
    assert_eq!(drain_reaction_keys(&mut cmd_rx), vec!["💻", "🏠", "🙋"]);
}

#[tokio::test]
async fn test_single_time_option_needs_no_host_pick() {
    let (cmd_tx, mut cmd_rx) = create_command_channel(20);
    let mut config = create_weekly_gathering_config();
    config.finalization_in_person_message =
        "IN-PERSON at {event_time}. Host: {host}.{time_prompt}".to_string();
    let middleware = WeeklyGathering::new(make_ctx(cmd_tx), config);

    vote_in_person_with_host(&middleware, "alice").await;

    let body = finalize_and_capture(&middleware, &mut cmd_rx, "final1").await;

    assert!(body.contains("at 7:00pm"), "got: {body}");
    assert!(!body.contains("Pick a time!"), "nothing to pick, got: {body}");
    assert!(drain_reaction_keys(&mut cmd_rx).is_empty());

    // Even the host reacting with a keycap changes nothing
    middleware
        .test_process_reaction_added(
            "final1".to_string(),
            "1\u{fe0f}\u{20e3}".to_string(),
            "alice".to_string(),
        )
        .await;
    assert!(cmd_rx.try_recv().is_err(), "no edit should be issued");
}

#[tokio::test]
async fn test_weekly_gathering_instantiation_requires_time_options() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let data_dir = TempDir::new().unwrap();

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "gathering".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::WeeklyGathering {
                service_id: "matrix".to_string(),
                room_id: "!gathering:matrix.org".to_string(),
                event_day_of_week: "Saturday".to_string(),
                event_time_options: String::new(),
                finalize_time: "14:00".to_string(),
                poll_open_minutes: 4200,
                reaction_virtual: "💻".to_string(),
                reaction_in_person: "🏠".to_string(),
                reaction_host: "🙋".to_string(),
                announcement_message: "Weekly poll!".to_string(),
                finalization_virtual_message: "Virtual!".to_string(),
                finalization_in_person_message: "In-person!".to_string(),
                finalization_no_votes_message: "No votes!".to_string(),
                time_prompt_message: "Pick a time!".to_string(),
                households: HashMap::new(),
            },
        },
    );

    let config = Config {
        services: HashMap::new(),
        middlewares: middlewares_map,
        data_directory: data_dir.path().to_path_buf(),
        reconnection: Default::default(),
    };

    let result = instantiate_middleware_from_config(&config, &cmd_tx);
    let err = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("an empty event_time_options should be rejected"),
    };
    assert!(err.contains("event_time_options is required"), "got: {err}");
}
