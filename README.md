# KelvinBot

An event-driven chat bot with a modular architecture supporting multiple messaging platforms and extensible middleware.

## Architecture Overview

KelvinBot uses an event-driven architecture with three main components:

- **Services**: Message sources that connect to external platforms (Matrix, etc.)
- **Middlewares**: Event processors that handle, filter, or respond to messages
- **Event Bus**: Central router that coordinates event flow between services and middlewares

```
Services → Event Bus → Middlewares
    ↑          ↓          ↓
  Matrix    Routing   Logging
  Dummy      Core    [Future: AI, Commands, etc.]
```

## Services

Services connect to external messaging platforms and generate events.

### Dummy Service
A test service that generates periodic messages.

**Configuration:**
```bash
KELVIN__SERVICES__<name>__KIND=dummy
KELVIN__SERVICES__<name>__INTERVAL_MS=1000  # Optional, defaults to 1000ms
```

### Matrix Service
Connects to Matrix homeservers for real-time messaging with E2EE support.

**Configuration:**
```bash
KELVIN__SERVICES__<name>__KIND=matrix
KELVIN__SERVICES__<name>__HOMESERVER_URL=https://matrix.example.com
KELVIN__SERVICES__<name>__USER_ID=@bot:example.com
KELVIN__SERVICES__<name>__PASSWORD=your_password
KELVIN__SERVICES__<name>__DEVICE_ID=KELVINBOT_01
KELVIN__SERVICES__<name>__DB_PASSPHRASE=encryption_key
KELVIN__SERVICES__<name>__VERIFICATION_DEVICE_ID=YOURDEVICEID
```

**Setting up E2EE Verification:**

The Matrix service requires interactive device verification to send/receive encrypted messages. Follow these steps:

1. **Log in to Element with the bot's account:**
   - Open Element (web, desktop, or mobile) in a separate session
   - Log in using the **same credentials as the bot** (`USER_ID` and `PASSWORD`)
   - Complete the initial security setup if this is the first time logging in
   - Ensure this Element session is verified (you may need to verify with another existing session or use the recovery key)

2. **Get your Element device ID:**
   - In Element, go to Settings → Security & Privacy → Session
   - Find your Element device ID (e.g., `ABCDEFGHIJ`)
   - This is the device ID of your verified Element session

3. **Configure the bot:**
   - Add `VERIFICATION_DEVICE_ID` to your `.env` with your Element device ID from step 2
   - This tells the bot which device to verify against

4. **Start the bot and verify:**
   - Start the bot: `cargo run`
   - The bot will send a verification request to your Element session
   - In Element, accept the verification request
   - Click "Start verification" when prompted
   - **Watch the bot logs for emoji codes** - they will be printed to the console
   - Compare the emojis in the bot logs with those shown in Element
   - If they match, click "They match" in Element
   - The bot will automatically confirm and complete verification

5. **Verification persists:**
   - Once verified, the bot's device is cross-signed
   - Future restarts won't require re-verification (unless you change `DEVICE_ID`)
   - The bot will refuse to start if verification fails
   - You can close your Element session after verification is complete

**Important:** The bot will **not start** if it cannot complete verification. This ensures all encrypted messages are properly secured.

## Middlewares

Middlewares process events and can perform actions or stop further processing.

### Logger Middleware
Logs all incoming events (automatically enabled).

**Future middlewares might include:**
- Command processor (`!help`, `!weather`, etc.)
- AI response generator
- Message filtering and moderation
- Cross-platform message bridging

## Configuration

Configuration is handled through environment variables or a `.env` file.

### Environment Variable Format
```
KELVIN__<SECTION>__<KEY>=<VALUE>
KELVIN__<SECTION>__<SUBSECTION>__<KEY>=<VALUE>
```

### Data Directory
```bash
KELVIN__DATA_DIRECTORY=./data  # Default: ./data
```

### Example: Multi-Service Setup
```bash
# Dummy service for testing
KELVIN__SERVICES__test_dummy__KIND=dummy
KELVIN__SERVICES__test_dummy__INTERVAL_MS=5000

# Matrix service for production
KELVIN__SERVICES__matrix_main__KIND=matrix
KELVIN__SERVICES__matrix_main__HOMESERVER_URL=https://matrix.org
KELVIN__SERVICES__matrix_main__USER_ID=@kelvinbot:matrix.org
KELVIN__SERVICES__matrix_main__PASSWORD=secret_password
KELVIN__SERVICES__matrix_main__DEVICE_ID=KELVIN_PROD
KELVIN__SERVICES__matrix_main__DB_PASSPHRASE=encryption_secret
KELVIN__SERVICES__matrix_main__VERIFICATION_DEVICE_ID=DEVICEIDHERE

# Custom data directory
KELVIN__DATA_DIRECTORY=/opt/kelvinbot/data
```

## Running

### Local Development
```bash
# Development with dummy service
cp .env.example .env
# Edit .env with your configuration
cargo run

# Production
RUST_LOG=info cargo run --release
```

### Docker
```bash
# Pull from GitHub Container Registry
docker pull ghcr.io/haydenmc/kelvinbot:latest

# Run with environment file
docker run -d \
  --name kelvinbot \
  --env-file .env \
  -v $(pwd)/data:/app/data \
  ghcr.io/haydenmc/kelvinbot:latest

# Or with individual environment variables
docker run -d \
  --name kelvinbot \
  -e KELVIN__SERVICES__dummy__KIND=dummy \
  -e KELVIN__SERVICES__dummy__INTERVAL_MS=5000 \
  -v $(pwd)/data:/app/data \
  ghcr.io/haydenmc/kelvinbot:latest

# View logs
docker logs kelvinbot

# Stop and remove
docker stop kelvinbot && docker rm kelvinbot
```

### Docker Compose
```yaml
version: '3.8'
services:
  kelvinbot:
    image: ghcr.io/haydenmc/kelvinbot:latest
    env_file: .env
    volumes:
      - ./data:/app/data
    restart: unless-stopped
```

## Testing

The project includes comprehensive unit and integration tests:

```bash
# Run all tests
cargo test

# Run specific test categories
cargo test --test unit_tests
cargo test --test integration_tests

# Run specific component tests
cargo test --test unit_tests unit::event
cargo test --test integration_tests integration::service_lifecycle
```

See [`tests/README.md`](tests/README.md) for detailed testing documentation.

### Continuous Integration

The project uses GitHub Actions for automated testing:

- **CI Pipeline**: Runs on all commits to `main` and PRs
  - Code formatting (`cargo fmt`)
  - Linting (`cargo clippy`)
  - All tests (unit + integration + doc tests)
  - Code coverage reporting on PRs
  - Release binary building (main branch only)
- **PR Quick Check**: Fast feedback on pull requests
- **Docker Publishing**: Builds and publishes container images on release tags

[![CI](https://github.com/haydenmc/KelvinBot/workflows/CI/badge.svg)](https://github.com/haydenmc/KelvinBot/actions)
[![Docker Publish](https://github.com/haydenmc/KelvinBot/workflows/Docker%20Publish/badge.svg)](https://github.com/haydenmc/KelvinBot/actions)
[![codecov](https://codecov.io/gh/haydenmc/KelvinBot/branch/main/graph/badge.svg)](https://codecov.io/gh/haydenmc/KelvinBot)

## Event Flow

1. **Services** generate `Event` objects from external sources
2. **Event Bus** receives events and routes them to middlewares
3. **Middlewares** process events in order, each returning a `Verdict`:
   - `Continue`: Pass event to next middleware
   - `Stop`: Halt processing for this event

## Development

### Getting Started

1. **Clone and setup**:
   ```bash
   git clone https://github.com/haydenmc/KelvinBot.git
   cd KelvinBot
   cp .env.example .env
   # Edit .env with your configuration
   ```

2. **Install development tools**:
   ```bash
   # Format code
   rustup component add rustfmt

   # Linting
   rustup component add clippy

   # Coverage (optional)
   cargo install cargo-llvm-cov
   ```

3. **Run in development**:
   ```bash
   cargo run
   ```

4. **Before committing**:
   ```bash
   cargo fmt --all
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test
   ```

### Adding a New Service

1. Create service struct implementing the `Service` trait
2. Add configuration variant to `ServiceKind` enum
3. Update `instantiate_services_from_config()` function
4. Add tests in `tests/unit/service.rs`

### Adding a New Middleware

1. Create middleware struct implementing the `Middleware` trait
2. Update `instantiate_middleware_from_config()` function
3. Add tests in `tests/unit/middleware.rs`

### Event Types

Currently supported event types:
- `DirectMessage`: Private message from a user
- `RoomMessage`: Message in a group chat/room

Add new event types by extending the `EventKind` enum.

## Project Structure

```
src/
├── main.rs                 # Application entry point
├── lib.rs                  # Library interface for testing
├── core/                   # Core framework components
│   ├── bus.rs             # Event routing and service orchestration
│   ├── config.rs          # Configuration loading and types
│   ├── event.rs           # Event types and definitions
│   ├── middleware.rs      # Middleware trait and management
│   └── service.rs         # Service trait and management
├── services/              # Platform integrations
│   ├── dummy.rs          # Test service for development
│   └── matrix.rs         # Matrix homeserver integration
└── middlewares/          # Event processors
    └── logger.rs        # Event logging middleware

tests/                    # Comprehensive test suite
├── unit/                # Component unit tests
├── integration/         # End-to-end integration tests
├── common/             # Shared test utilities and MockService
└── README.md          # Testing documentation
```