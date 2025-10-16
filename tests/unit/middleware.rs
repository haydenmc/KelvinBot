use assert_matches::assert_matches;
use kelvin_bot::core::{
    bus::{create_command_channel, Command},
    event::{Event, EventKind},
    middleware::{build_middleware_pipeline, instantiate_middleware_from_config, Middleware, Verdict},
    service::ServiceId,
    config::{Config, MiddlewareCfg, MiddlewareKind},
};
use kelvin_bot::middlewares::{echo::Echo, logger::Logger};
use std::collections::HashMap;
use std::sync::Arc;
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
        MiddlewareCfg {
            kind: MiddlewareKind::Echo { command_string: "!mycommand".to_string() },
        },
    );
    middlewares_map.insert(
        "test_logger".to_string(),
        MiddlewareCfg { kind: MiddlewareKind::Logger {} },
    );

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
    all_middlewares.insert(
        "echo1".to_string(),
        Arc::new(Echo::new(cmd_tx.clone(), "!echo".to_string())),
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
