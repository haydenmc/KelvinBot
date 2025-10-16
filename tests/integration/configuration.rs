use kelvin_bot::core::{
    bus::{create_command_channel, create_event_channel},
    config::{Config, MiddlewareCfg, MiddlewareKind, ServiceCfg, ServiceKind},
    middleware::instantiate_middleware_from_config,
    service::instantiate_services_from_config,
};
use std::collections::HashMap;
use tempfile::TempDir;

#[tokio::test]
async fn test_unknown_service_kind_handling() {
    let config_str = r#"
        [services.unknown_service]
        kind = "unknown_type"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");
    let (evt_tx, _evt_rx) = create_event_channel(10);

    let services = instantiate_services_from_config(&config, &evt_tx)
        .await
        .expect("Failed to instantiate services");

    // Unknown service types should be skipped
    assert_eq!(services.len(), 0);
}

#[tokio::test]
async fn test_configuration_with_mixed_service_types() {
    let mut services = HashMap::new();

    // Add a valid dummy service
    services.insert(
        "dummy1".to_string(),
        ServiceCfg {
            kind: ServiceKind::Dummy { interval_ms: Some(100) },
            middleware: None,
        },
    );

    // Add an unknown service type
    services.insert(
        "unknown1".to_string(),
        ServiceCfg { kind: ServiceKind::Unknown, middleware: None },
    );

    let config = Config {
        services,
        middlewares: HashMap::new(),
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
    };

    let (evt_tx, _evt_rx) = create_event_channel(10);
    let instantiated_services = instantiate_services_from_config(&config, &evt_tx)
        .await
        .expect("Failed to instantiate services");

    // Only the valid dummy service should be instantiated
    assert_eq!(instantiated_services.len(), 1);
    assert!(
        instantiated_services
            .contains_key(&kelvin_bot::core::service::ServiceId("dummy1".to_string()))
    );
}

#[test]
fn test_config_default_data_directory() {
    let config_str = r#"
        [services.dummy1]
        kind = "dummy"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    // Should use default data directory when not specified
    assert_eq!(config.data_directory.to_string_lossy(), "./data");
}

#[test]
fn test_config_custom_data_directory() {
    let config_str = r#"
        data_directory = "/custom/path"

        [services.dummy1]
        kind = "dummy"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    // Should use custom data directory when specified
    assert_eq!(config.data_directory.to_string_lossy(), "/custom/path");
}

#[tokio::test]
async fn test_service_with_middleware_list_configuration() {
    let mut services = HashMap::new();
    services.insert(
        "service1".to_string(),
        ServiceCfg {
            kind: ServiceKind::Dummy { interval_ms: Some(100) },
            middleware: Some(vec!["echo1".to_string(), "logger1".to_string()]),
        },
    );
    services.insert(
        "service2".to_string(),
        ServiceCfg {
            kind: ServiceKind::Dummy { interval_ms: Some(200) },
            middleware: Some(vec!["logger1".to_string()]),
        },
    );

    let mut middlewares_map = HashMap::new();
    middlewares_map.insert(
        "echo1".to_string(),
        MiddlewareCfg {
            kind: MiddlewareKind::Echo { command_string: "!test".to_string() },
        },
    );
    middlewares_map.insert(
        "logger1".to_string(),
        MiddlewareCfg { kind: MiddlewareKind::Logger {} },
    );

    let config = Config {
        services,
        middlewares: middlewares_map,
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
    };

    let (cmd_tx, _cmd_rx) = create_command_channel(10);
    let middlewares = instantiate_middleware_from_config(&config, &cmd_tx)
        .expect("Failed to instantiate middlewares");

    // Verify all middleware instances were created
    assert_eq!(middlewares.len(), 2);
    assert!(middlewares.contains_key("echo1"));
    assert!(middlewares.contains_key("logger1"));

    // Verify service configurations have correct middleware lists
    let service1_cfg = config.services.get("service1").unwrap();
    assert!(service1_cfg.middleware.is_some());
    assert_eq!(service1_cfg.middleware.as_ref().unwrap().len(), 2);

    let service2_cfg = config.services.get("service2").unwrap();
    assert!(service2_cfg.middleware.is_some());
    assert_eq!(service2_cfg.middleware.as_ref().unwrap().len(), 1);
}
