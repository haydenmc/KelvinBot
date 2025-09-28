# KelvinBot Test Suite

This directory contains comprehensive unit and integration tests for the KelvinBot project, organized in a component-based structure following Rust best practices.

## Test Structure

### Unit Tests (`unit/`)
Tests individual components in isolation, organized by module:

#### `unit/event.rs`
- Event creation, serialization, and display formatting
- EventKind variants (DirectMessage, RoomMessage)
- JSON serialization/deserialization

#### `unit/service.rs`
- ServiceId struct functionality and traits
- DummyService creation, event generation, and cancellation
- Service trait implementation testing

#### `unit/middleware.rs`
- Middleware trait implementation
- Logger middleware event processing
- Verdict system (Continue/Stop)

#### `unit/config.rs`
- TOML configuration parsing
- Service configuration validation
- Unknown service type handling

#### `unit/bus.rs`
- Channel creation utilities
- Basic bus component testing

### Integration Tests (`integration/`)
Tests component interactions and full system behavior:

#### `integration/service_lifecycle.rs`
- Service instantiation from configuration
- Middleware instantiation and setup
- Bus creation and startup procedures
- Graceful shutdown and cancellation propagation

#### `integration/event_flow.rs`
- End-to-end event processing pipeline with **deterministic event counting**
- Middleware execution order and pipeline stopping behavior validation
- **MockService-based testing** for reliable, non-timing-dependent tests
- Verdict::Stop functionality verification

#### `integration/configuration.rs`
- Complex configuration scenarios
- Mixed service types handling
- Data directory configuration

## Running Tests

### Shared Utilities (`common/`)
- Test configuration builders (`create_test_config`, `create_multi_service_config`)
- **MockService**: Controllable service for deterministic testing
- Common test data structures and helper functions

## Running Tests

```bash
# Run all tests
cargo test

# Run only unit tests
cargo test --test unit_tests

# Run only integration tests
cargo test --test integration_tests

# Run specific component tests
cargo test --test unit_tests unit::event
cargo test --test integration_tests integration::service_lifecycle

# Run a specific test
cargo test test_service_id_display

# Run tests with output
cargo test -- --nocapture
```

## Test Dependencies

- `tokio-test`: Async testing utilities
- `tempfile`: Temporary directory creation for data storage tests
- `assert_matches`: Pattern matching assertions
- `serde_json`: JSON serialization testing

## Coverage Areas

### ✅ Covered
- Core event system (Event, EventKind)
- Service lifecycle and interfaces
- Middleware pipeline processing
- Configuration parsing and validation
- Bus orchestration and shutdown
- Cancellation token propagation

### 🚧 Future Test Additions
- Matrix service integration (requires test Matrix server)
- Error handling and recovery scenarios
- Performance and load testing
- Security and authentication flows
- Network failure simulation
- Database persistence testing

## Adding New Tests

### Adding Unit Tests
1. **Choose the appropriate module**: Add tests to the relevant file in `unit/`
2. **Create new modules**: If testing a new component, create a new file in `unit/` and add it to `unit/mod.rs`

Example:
```rust
// tests/unit/new_component.rs
use kelvin_bot::core::new_component::NewComponent;

#[test]
fn test_new_component_behavior() {
    // Test implementation
}
```

### Adding Integration Tests
1. **Choose the appropriate category**: Add to existing files in `integration/` or create new ones
2. **Use shared utilities**: Import from `crate::common` for test data and helpers

Example:
```rust
// tests/integration/new_workflow.rs
use crate::common::create_test_config;

#[tokio::test]
async fn test_new_workflow() {
    let config = create_test_config();
    // Test implementation
}
```

### Test Organization Guidelines
- **Unit tests**: Test one component/function at a time
- **Integration tests**: Test component interactions and workflows
- **Shared utilities**: Add common helpers to `common/mod.rs`
- **Keep tests focused**: Each test should verify one specific behavior
- **Use descriptive names**: `test_component_behavior_scenario`

### Best Practices
1. Use `tokio_test::assert_ok!` for Result assertions
2. Use `assert_matches!` for pattern matching
3. Use `tempfile::TempDir` for temporary file system operations
4. **Use `MockService` for deterministic testing** instead of timing-based approaches
5. Always test both success and failure scenarios
6. Use cancellation tokens for testing async component shutdown
7. Prefer exact assertions (`assert_eq!`) over ranges when using controllable mocks

### MockService Usage Example
```rust
use crate::common::MockService;

// Create controllable service
let (mock_service, control) = MockService::new(service_id, event_sender);

// Send exactly 3 events
control.send(3).await.expect("Failed to send command");

// Assert exact count
assert_eq!(processed_count, 3);
```