use kelvin_bot::core::{
    bus::create_event_channel,
    config::{Config, ServiceCfg, ServiceKind},
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

    let services = instantiate_services_from_config(&config, &evt_tx);

    // Unknown service types should be skipped
    assert_eq!(services.len(), 0);
}

#[tokio::test]
async fn test_configuration_with_mixed_service_types() {
    let mut services = HashMap::new();

    // Add a valid dummy service
    services.insert(
        "dummy1".to_string(),
        ServiceCfg { kind: ServiceKind::Dummy { interval_ms: Some(100) } },
    );

    // Add an unknown service type
    services.insert("unknown1".to_string(), ServiceCfg { kind: ServiceKind::Unknown });

    let config = Config { services, data_directory: TempDir::new().unwrap().path().to_path_buf() };

    let (evt_tx, _evt_rx) = create_event_channel(10);
    let instantiated_services = instantiate_services_from_config(&config, &evt_tx);

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
