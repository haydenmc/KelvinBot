use assert_matches::assert_matches;
use kelvin_bot::core::config::{Config, MiddlewareKind, ServiceKind};

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

#[test]
fn test_config_serde_calendar_agenda_middleware() {
    let config_str = r#"
        [services.matrix1]
        kind = "dummy"

        [middlewares.calendar]
        kind = "calendaragenda"
        service_id = "matrix1"
        room_id = "!room:example.com"
        calendar_url = "webcal://example.com/private.ics"
        calendar_link = "https://example.com/calendar"
        post_at_time = "08:00"
        countdown_days = "90, 60, 30, 14"
        reminder_days = "7,1"
        multi_day_min_days = "3"
        command_string = "!agenda"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    let calendar = &config.middlewares["calendar"];
    assert_matches!(
        &calendar.kind,
        MiddlewareKind::CalendarAgenda {
            calendar_url,
            calendar_link: Some(link),
            calendar_link_text,
            countdown_days: Some(countdown),
            reminder_days: Some(reminder),
            multi_day_min_days: 3,
            heading_today,
            command_string: Some(command),
            ..
        } if calendar_url == "webcal://example.com/private.ics"
            && link == "https://example.com/calendar"
            // Unset optional fields fall back to their defaults.
            && calendar_link_text == "View the full calendar"
            && heading_today == "Today"
            && command == "!agenda"
            && countdown == &["90", "60", "30", "14"]
            && reminder == &["7", "1"]
    );
}

#[test]
fn test_config_serde_calendar_agenda_defaults() {
    let config_str = r#"
        [services.matrix1]
        kind = "dummy"

        [middlewares.calendar]
        kind = "calendaragenda"
        service_id = "matrix1"
        room_id = "!room:example.com"
        calendar_url = "https://example.com/private.ics"
        post_at_time = "08:00"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    assert_matches!(
        &config.middlewares["calendar"].kind,
        MiddlewareKind::CalendarAgenda {
            calendar_link: None,
            countdown_days: None,
            reminder_days: None,
            multi_day_min_days: 2,
            command_string: None,
            ..
        }
    );
}
