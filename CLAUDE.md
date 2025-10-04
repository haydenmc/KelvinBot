# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

KelvinBot is an event-driven chat bot written in Rust with a modular architecture supporting multiple messaging platforms. The core design uses an event bus pattern where services (message sources) and middlewares (event processors) communicate through a central bus.

## Key Architecture

### Event Flow Pattern
1. **Services** connect to external platforms (Matrix, Dummy) and emit `Event` objects to the bus via `evt_tx` channel
2. **Bus** receives events and routes them through the middleware pipeline sequentially
3. **Middlewares** process events and return `Verdict::Continue` or `Verdict::Stop` to control the pipeline
4. **Commands** flow back from middlewares to services via `cmd_tx` channel for actions like sending messages

### Core Components
- **Event Bus** (`src/core/bus.rs`): Central orchestrator that spawns services/middlewares and routes events/commands. The bus owns the event and command receivers, while services get event senders and middlewares get command senders.
- **Events** (`src/core/event.rs`): `Event` struct containing `service_id` and `EventKind` (DirectMessage, RoomMessage)
- **Services** (`src/core/service.rs`): Trait with `run()` for lifecycle and `handle_command()` for receiving commands. Instantiated via `instantiate_services_from_config()`.
- **Middlewares** (`src/core/middleware.rs`): Trait with `run()` for lifecycle and `on_event()` returning `Verdict`. Instantiated via `instantiate_middleware_from_config()`.

### Service Implementation Pattern
Services are spawned as async tasks that:
1. Initialize connection to external platform
2. Set up event handlers that convert platform events to `Event` and send via `evt_tx`
3. Listen for cancellation token
4. Implement `handle_command()` to execute actions (send messages, etc.)

See `src/services/matrix.rs` for a full implementation example.

### Middleware Implementation Pattern
Middlewares run as async tasks and:
1. Implement `run()` to wait for cancellation (most are stateless)
2. Implement `on_event()` to process events synchronously
3. Can spawn async tasks to send commands via `cmd_tx` (see `src/middlewares/echo.rs`)

## Development Commands

### Build & Run
```bash
# Development build and run
cargo run

# Production build
cargo run --release

# With logging
RUST_LOG=debug cargo run
RUST_LOG=kelvin_bot=info,matrix_sdk=warn cargo run
```

### Testing
```bash
# Run all tests
cargo test

# Run specific test suites
cargo test --test unit_tests
cargo test --test integration_tests

# Run tests for specific components
cargo test --test unit_tests unit::event
cargo test --test unit_tests unit::service
cargo test --test integration_tests integration::service_lifecycle
cargo test --test integration_tests integration::event_flow

# Run with output
cargo test -- --nocapture
```

### Code Quality
```bash
# Format code (required before commits)
cargo fmt --all

# Linting (must pass with no warnings)
cargo clippy --all-targets --all-features -- -D warnings

# Code coverage (optional)
cargo install cargo-llvm-cov
cargo llvm-cov
```

## Configuration System

Configuration uses hierarchical environment variables:
- Format: `KELVIN__<SECTION>__<KEY>=<VALUE>`
- Nested: `KELVIN__SERVICES__<service_name>__<property>=<value>`
- See `.env.example` for examples
- Parsed in `src/core/config.rs` using the `config` crate

Services are defined as:
```bash
KELVIN__SERVICES__<unique_name>__KIND=dummy|matrix
KELVIN__SERVICES__<unique_name>__<kind_specific_options>=<value>
```

## Adding New Components

### Adding a Service
1. Create new file in `src/services/` implementing `Service` trait
2. Add configuration variant to `ServiceKind` enum in `src/core/config.rs`
3. Update `instantiate_services_from_config()` in `src/core/service.rs` to handle new kind
4. Add unit tests in `tests/unit/service.rs`
5. Services must handle commands via `handle_command()` method

### Adding a Middleware
1. Create new file in `src/middlewares/` implementing `Middleware` trait
2. Update `instantiate_middleware_from_config()` in `src/core/middleware.rs`
3. Add unit tests in `tests/unit/middleware.rs`
4. Middlewares can send commands by receiving `Sender<Command>` in constructor (see Echo middleware)

### Adding Event Types
1. Add new variant to `EventKind` enum in `src/core/event.rs`
2. Update `Display` impl for formatting
3. Add corresponding `Command` variant in `src/core/bus.rs` if services need to send this event type
4. Update middleware implementations to handle new event kind

## Test Organization

Tests are split into separate test binaries:
- `tests/unit_tests.rs`: Entry point for unit tests (modules in `tests/unit/`)
- `tests/integration_tests.rs`: Entry point for integration tests (modules in `tests/integration/`)
- `tests/common/`: Shared test utilities, including `MockService` for deterministic testing

**Important**: Use `MockService` from `tests/common/mod.rs` for event flow testing instead of timing-based approaches. It provides controllable event emission for deterministic tests.

## Code Style Notes

- Max line width: 100 characters (enforced by rustfmt.toml)
- Use hard tabs: false
- All code must pass `cargo fmt` and `cargo clippy --all-targets --all-features -- -D warnings`
- Use `tracing` crate for logging, not `println!`
- Async functions use `async_trait` macro
- Prefer `anyhow::Result` for error handling

## Matrix Service Specifics

The Matrix service (`src/services/matrix.rs`) has special considerations:
- Uses `matrix-sdk` with bundled SQLite for state storage
- Stores data in `<data_directory>/matrix/<service_id>/`
- Requires two passphrases:
  - `DB_PASSPHRASE`: Encrypts local SQLite database
  - `RECOVERY_PASSPHRASE`: Encrypts cross-signing keys backed up to homeserver
- Auto-accepts room invites and marks DMs for proper routing
- Implements auto-join for invited rooms
- Uses `find_or_create_dm()` to locate or create direct message rooms

### E2E Encryption & Cross-Signing (CURRENT WORK IN PROGRESS)

**Status:** Implementing manual cross-signing setup with recovery passphrase for cross-device verification.

**Current Implementation (`src/services/matrix.rs:setup_encryption()`):**
- `auto_enable_cross_signing: false` - Manual control to avoid SDK auto-enable races
- `auto_enable_backups: false` - Manual backup creation with passphrase
- `backup_download_strategy: Manual` - We explicitly handle recovery

**Setup Flow:**
1. **Check local state:** If device has all cross-signing keys (`has_master/has_self_signing/has_user_signing=true`) AND is cross-signed by owner (`is_cross_signed_by_owner()=true`), skip setup
2. **Check server backup:** Use `encryption.backups().fetch_exists_on_server()` to detect backup
3. **Recovery path:** If backup exists, call `recovery.recover(RECOVERY_PASSPHRASE)` to import keys from server
4. **Fresh setup path:** If no backup OR recovery fails, call `recovery.enable().with_passphrase(RECOVERY_PASSPHRASE)` to create new identity
5. **Self-verify:** Call `device.verify()` on own device

**Key Discovery:**
- Cross-signing keys are stored in TWO places: local SQLite DB AND server's secret storage (encrypted backup)
- The SDK will auto-import from secret storage if `auto_enable_*: true`, but this races with our manual logic
- Local DB keys can be DIFFERENT from server backup keys, causing mismatch errors
- When mismatch detected: "The public key of the imported private key doesn't match to the public key that was uploaded to the server"

**Current Issue:**
- After multiple resets, local DB may have stale cross-signing keys that don't match server backup
- Recovery attempts import server keys, SDK detects public key mismatch with what's on server, clears local keys
- **Solution:** Delete `./data/matrix/` directory completely when getting mismatch errors to start fresh

**Configuration Required:**
```bash
KELVIN__SERVICES__matrix_main__RECOVERY_PASSPHRASE=your_recovery_passphrase
```
Same passphrase must be used across all devices/environments for cross-device verification to work.

**Diagnostic Logging:**
The setup logs key status:
- `has_master/has_self_signing/has_user_signing` - Which cross-signing keys exist locally
- `is_verified` - Whether device is locally verified
- `is_cross_signed` - Whether device is signed by cross-signing keys (KEY INDICATOR for other users)

**Testing Cross-Device Verification:**
1. First run: Creates cross-signing identity + backup with passphrase
2. Change `DEVICE_ID` and restart: Should recover from backup, device gets auto-signed
3. Users who verified bot's identity once will trust all future devices automatically

## Current Branch Context

Working on `e2ee-verification` branch - implementing manual E2EE cross-signing with passphrase-protected backup for seamless cross-device verification. Current challenge: handling SDK state consistency between local DB and server secret storage.
