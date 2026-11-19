use assert_matches::assert_matches;
use kelvin_bot::core::config::{Config, MiddlewareKind, ReconnectionConfig, ServiceKind};
use secrecy::ExposeSecret;
use std::time::Duration;

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

    // ...which validation then rejects, naming the service.
    let err = config.validate().expect_err("an unknown kind should not validate");
    assert!(format!("{err:#}").contains("unknown_service"), "got: {err:#}");
}

#[test]
fn test_config_unknown_middleware_kind_is_rejected() {
    let config_str = r#"
        [services.dummy1]
        kind = "dummy"

        [middlewares.typo]
        kind = "ecoh"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    let err = config.validate().expect_err("an unknown kind should not validate");
    assert!(format!("{err:#}").contains("middleware 'typo'"), "got: {err:#}");
}

#[test]
fn test_config_with_known_kinds_validates() {
    let config_str = r#"
        [services.dummy1]
        kind = "dummy"

        [middlewares.log]
        kind = "logger"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    config.validate().expect("a well-formed config should validate");
}

#[test]
fn test_default_reconnection_config_validates() {
    ReconnectionConfig::default().validate().expect("defaults should validate");
}

#[test]
fn test_reconnection_config_rejects_out_of_range_values() {
    type Change = fn(&mut ReconnectionConfig);

    let with = |change: Change| {
        let mut config = ReconnectionConfig::default();
        change(&mut config);
        config.validate().map_err(|e| format!("{e:#}"))
    };

    let cases: [(Change, &str); 8] = [
        (|c| c.initial_delay = Duration::ZERO, "initial_delay"),
        (|c| c.max_delay = c.initial_delay / 2, "max_delay"),
        (|c| c.multiplier = 0.5, "multiplier"),
        (|c| c.multiplier = -2.0, "multiplier"),
        (|c| c.multiplier = f64::NAN, "multiplier"),
        // A negative jitter used to panic when the delay was computed.
        (|c| c.jitter_factor = -0.1, "jitter_factor"),
        (|c| c.jitter_factor = 1.5, "jitter_factor"),
        (|c| c.jitter_factor = f64::NAN, "jitter_factor"),
    ];
    for (change, field) in cases {
        let err = with(change).expect_err("out-of-range value should be rejected");
        assert!(err.contains(field), "expected an error about {field}, got: {err}");
    }

    // The boundaries themselves are allowed.
    assert!(with(|c| c.multiplier = 1.0).is_ok());
    assert!(with(|c| c.jitter_factor = 0.0).is_ok());
    assert!(with(|c| c.jitter_factor = 1.0).is_ok());
    assert!(with(|c| c.max_delay = c.initial_delay).is_ok());
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
        command_string = "!events"
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
        } if calendar_url.expose_secret() == "webcal://example.com/private.ics"
            && link == "https://example.com/calendar"
            // Unset optional fields fall back to their defaults.
            && calendar_link_text == "View the full calendar"
            && heading_today == "Today"
            && command == "!events"
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
            // The on-demand command is on by default.
            command_string: Some(command),
            ..
        } if command == "!events"
    );
}

#[test]
fn test_config_serde_lychee_upload_middleware() {
    let config_str = r#"
        [services.matrix1]
        kind = "dummy"

        [middlewares.lychee]
        kind = "lycheeupload"
        service_id = "matrix1"
        room_ids = "!a:x, !b:x"
        exclude_room_ids = "!c:x"
        lychee_url = "https://photos.example.com"
        lychee_token = "secret"
        album_id = "AbCdEfGhIjKlMnOpQrStUvWx"
        max_file_size_bytes = "20971520"
        failure_message = "nope"
        oversize_message = "big ({max_mb})"
        request_timeout = "30s"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    assert_matches!(
        &config.middlewares["lychee"].kind,
        MiddlewareKind::LycheeUpload {
            service_id,
            room_ids: Some(rooms),
            exclude_room_ids: Some(excluded),
            lychee_url,
            album_id,
            max_file_size_bytes: 20_971_520,
            failure_message,
            oversize_message,
            request_timeout,
            ..
        } if service_id == "matrix1"
            && rooms == &["!a:x", "!b:x"]
            && excluded == &["!c:x"]
            && lychee_url == "https://photos.example.com"
            && album_id == "AbCdEfGhIjKlMnOpQrStUvWx"
            && failure_message == "nope"
            && oversize_message == "big ({max_mb})"
            && *request_timeout == std::time::Duration::from_secs(30)
    );
}

#[test]
fn test_config_serde_lychee_upload_defaults() {
    let config_str = r#"
        [services.matrix1]
        kind = "dummy"

        [middlewares.lychee]
        kind = "lycheeupload"
        service_id = "matrix1"
        lychee_url = "https://photos.example.com"
        lychee_token = "secret"
        album_id = "AbCdEfGhIjKlMnOpQrStUvWx"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    assert_matches!(
        &config.middlewares["lychee"].kind,
        MiddlewareKind::LycheeUpload {
            room_ids: None,
            exclude_room_ids: None,
            max_file_size_bytes: 52_428_800,
            failure_message,
            oversize_message,
            request_timeout,
            ..
        } if failure_message.contains("couldn't archive")
            && oversize_message.contains("{max_mb}")
            && *request_timeout == std::time::Duration::from_secs(60)
    );
}

#[test]
fn test_config_serde_lychee_upload_empty_room_list() {
    let config_str = r#"
        [services.matrix1]
        kind = "dummy"

        [middlewares.lychee]
        kind = "lycheeupload"
        service_id = "matrix1"
        room_ids = ""
        lychee_url = "https://photos.example.com"
        lychee_token = "secret"
        album_id = "AbCdEfGhIjKlMnOpQrStUvWx"
        "#;

    let config: Config = toml::from_str(config_str).expect("Failed to parse config");

    // An explicitly empty list is equivalent to omitting it: every room.
    assert_matches!(
        &config.middlewares["lychee"].kind,
        MiddlewareKind::LycheeUpload { room_ids: Some(rooms), .. } if rooms.is_empty()
    );
}
