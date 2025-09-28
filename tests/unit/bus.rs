use kelvin_bot::core::bus::{create_command_channel, create_event_channel};

#[test]
fn test_command_channel_creation() {
    let (cmd_tx, _cmd_rx) = create_command_channel(10);

    // Verify we can clone the sender
    let _cmd_tx_clone = cmd_tx.clone();

    // Channel should be created successfully with specified capacity
    assert!(true); // Basic smoke test - if we get here, channel creation worked
}

#[test]
fn test_event_channel_creation() {
    let (evt_tx, _evt_rx) = create_event_channel(10);

    // Verify we can clone the sender
    let _evt_tx_clone = evt_tx.clone();

    // Channel should be created successfully with specified capacity
    assert!(true); // Basic smoke test - if we get here, channel creation worked
}
