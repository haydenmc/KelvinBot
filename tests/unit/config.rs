use assert_matches::assert_matches;
use kelvin_bot::core::config::{Config, ServiceKind};

#[test]
fn test_config_serde_dummy_service() {
    let config_str = r#"
        [services.dummy1]
        kind = "dummy"
        interval_ms = "5000"

        [services.dummy2]
        kind = "dummy"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    assert_eq!(config.services.len(), 2);

    let dummy1 = &config.services["dummy1"];
    assert_matches!(&dummy1.kind, ServiceKind::Dummy { interval_ms: Some(5000) });

    let dummy2 = &config.services["dummy2"];
    assert_matches!(&dummy2.kind, ServiceKind::Dummy { interval_ms: None });
}

#[test]
fn test_config_unknown_service_type() {
    let config_str = r#"
        [services.unknown_service]
        kind = "unknown_type"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");
    let unknown_service = &config.services["unknown_service"];

    // Unknown service types should deserialize as Unknown variant
    assert_matches!(&unknown_service.kind, ServiceKind::Unknown);
}
