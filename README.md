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

Middlewares process events and can perform actions or stop further processing. Each middleware instance is defined in configuration and can be assigned to specific services.

### Configuring Middlewares

Middlewares are configured in two steps:

1. **Define middleware instances** with their configuration
2. **Assign middlewares to services** using a comma-separated list

**Configuration format:**
```bash
# Define a middleware instance
KELVIN__MIDDLEWARES__<name>__KIND=<middleware_type>
KELVIN__MIDDLEWARES__<name>__<type_specific_options>=<value>

# Assign middlewares to a service (comma-separated)
KELVIN__SERVICES__<service_name>__MIDDLEWARE=<middleware1>,<middleware2>,...
```

### Available Middleware Types

#### Logger Middleware
Logs all incoming events to the console using the configured log level.

**Configuration:**
```bash
KELVIN__MIDDLEWARES__<name>__KIND=logger
```

**Example:**
```bash
KELVIN__MIDDLEWARES__logger__KIND=logger
KELVIN__SERVICES__matrix_main__MIDDLEWARE=logger
```

#### Echo Middleware
Responds to messages starting with a specified command string by echoing back the rest of the message.

**Configuration:**
```bash
KELVIN__MIDDLEWARES__<name>__KIND=echo
KELVIN__MIDDLEWARES__<name>__COMMAND_STRING=<command_prefix>
```

**Example:**
```bash
# Define an echo middleware that responds to "!echo"
KELVIN__MIDDLEWARES__myecho__KIND=echo
KELVIN__MIDDLEWARES__myecho__COMMAND_STRING=!echo

# Assign to service
KELVIN__SERVICES__matrix_main__MIDDLEWARE=myecho,logger
```

When a user sends `!echo hello world`, the bot will respond with `hello world`.

#### Invite Middleware
Generates registration tokens for chat services (currently only implemented
for Matrix). Only accepts requests from local users (same server as the bot).

**Configuration:**
```bash
KELVIN__MIDDLEWARES__<name>__KIND=invite
KELVIN__MIDDLEWARES__<name>__COMMAND_STRING=<command_trigger>
KELVIN__MIDDLEWARES__<name>__USES_ALLOWED=<number>      # Optional, default: 1
KELVIN__MIDDLEWARES__<name>__EXPIRY=<duration>          # Optional, default: 7d
```

**Duration Format:**
The `EXPIRY` parameter accepts human-readable durations:
- `7d` - 7 days
- `1w` - 1 week
- `24h` - 24 hours
- `30m` - 30 minutes
- `2h30m` - 2 hours and 30 minutes
- `1w2d` - 1 week and 2 days

**Example:**
```bash
# Define an invite middleware with custom settings
KELVIN__MIDDLEWARES__myinvite__KIND=invite
KELVIN__MIDDLEWARES__myinvite__COMMAND_STRING=!invite
KELVIN__MIDDLEWARES__myinvite__USES_ALLOWED=1    # Token can be used once
KELVIN__MIDDLEWARES__myinvite__EXPIRY=7d         # Token expires in 7 days

# Assign to Matrix service
KELVIN__SERVICES__matrix_main__MIDDLEWARE=myecho,myinvite,logger
```

**Usage:**
When a local user sends `!invite`, the bot generates a Matrix registration token and responds with:
```
Registration token generated: abc123xyz

Uses allowed: 1
Expires: 2025-11-04 15:30:00 UTC

Use this token when registering a new account on this server.
```

**Requirements:**
- Currently only works with Matrix services
- Bot user must have requisite permissions to generate tokens
- Only local users (same homeserver on Matrix) can request tokens
- Tokens are single-use by default for security

#### Movie Showtimes Middleware
Posts weekly movie showtimes to a specified room on a recurring schedule using the Gracenote TMS API.

**Configuration:**
```bash
KELVIN__MIDDLEWARES__<name>__KIND=movieshowtimes
KELVIN__MIDDLEWARES__<name>__SERVICE_ID=<service_name>
KELVIN__MIDDLEWARES__<name>__ROOM_ID=<room_id>
KELVIN__MIDDLEWARES__<name>__POST_ON_DAY_OF_WEEK=<day>
KELVIN__MIDDLEWARES__<name>__POST_AT_TIME=<HH:MM>
KELVIN__MIDDLEWARES__<name>__SEARCH_LOCATION__LAT=<latitude>
KELVIN__MIDDLEWARES__<name>__SEARCH_LOCATION__LNG=<longitude>
KELVIN__MIDDLEWARES__<name>__SEARCH_RADIUS_MI=<miles>
KELVIN__MIDDLEWARES__<name>__GRACENOTE_API_KEY=<api_key>
KELVIN__MIDDLEWARES__<name>__THEATER_ID_FILTER=<id1>,<id2>,<id3>  # Optional
```

**Parameters:**
- `POST_ON_DAY_OF_WEEK`: Day to post - one of: `Monday`, `Tuesday`, `Wednesday`, `Thursday`, `Friday`, `Saturday`, `Sunday`
- `POST_AT_TIME`: Time to post in 24-hour format (e.g., `09:00`, `18:30`)
- `SEARCH_LOCATION`: Latitude/longitude coordinates for theater search
- `SEARCH_RADIUS_MI`: Search radius in miles from location
- `GRACENOTE_API_KEY`: API key from [Gracenote Developer](https://developer.tmsapi.com/)
- `THEATER_ID_FILTER`: Optional comma-separated priority list of theater IDs

**Example:**
```bash
KELVIN__MIDDLEWARES__movies__KIND=movieshowtimes
KELVIN__MIDDLEWARES__movies__SERVICE_ID=matrix_main
KELVIN__MIDDLEWARES__movies__ROOM_ID=!abcdef123456:matrix.org
KELVIN__MIDDLEWARES__movies__POST_ON_DAY_OF_WEEK=Friday
KELVIN__MIDDLEWARES__movies__POST_AT_TIME=09:00
KELVIN__MIDDLEWARES__movies__SEARCH_LOCATION__LAT=47.6062
KELVIN__MIDDLEWARES__movies__SEARCH_LOCATION__LNG=-122.3321
KELVIN__MIDDLEWARES__movies__SEARCH_RADIUS_MI=10
KELVIN__MIDDLEWARES__movies__GRACENOTE_API_KEY=your_api_key_here
KELVIN__MIDDLEWARES__movies__THEATER_ID_FILTER=1234,5678,9012
```

**Behavior:**
- Fetches 7 days of showtimes from TMS API at scheduled time
- Groups showtimes by day for each movie
- If `THEATER_ID_FILTER` is set: shows detailed times for first matching theater, lists others as "also showing at"
- Without filter: shows all theaters within radius
- Posts markdown-formatted message with movie metadata (title, year, rating, runtime)
- Runs independently as background task

### Middleware Pipelines

Services can have multiple middlewares that process events sequentially:

```bash
# Define multiple middleware instances
KELVIN__MIDDLEWARES__logger__KIND=logger
KELVIN__MIDDLEWARES__echo1__KIND=echo
KELVIN__MIDDLEWARES__echo1__COMMAND_STRING=!echo
KELVIN__MIDDLEWARES__echo2__KIND=echo
KELVIN__MIDDLEWARES__echo2__COMMAND_STRING=!test

# Service 1 uses all three middlewares
KELVIN__SERVICES__matrix_main__MIDDLEWARE=logger,echo1,echo2

# Service 2 uses only the logger
KELVIN__SERVICES__test_dummy__MIDDLEWARE=logger
```

**Processing order:**
1. Events flow through middlewares in the order specified
2. Each middleware returns a `Verdict`:
   - `Continue`: Pass event to next middleware
   - `Stop`: Halt processing for this event
3. Middleware instances can be reused across multiple services

### Future Middleware Ideas

Potential middlewares for future development:
- Command processor with help system (`!help`, `!weather`, etc.)
- AI response generator using LLMs
- Message filtering and moderation
- Sentiment analysis
- Cross-platform message bridging
- Rate limiting and spam prevention
- Scheduled announcements and reminders
- RSS feed monitoring and posting

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

### Example: Multi-Service Setup with Middlewares
```bash
# Data directory
KELVIN__DATA_DIRECTORY=/opt/kelvinbot/data

# Define middleware instances
KELVIN__MIDDLEWARES__logger__KIND=logger
KELVIN__MIDDLEWARES__myecho__KIND=echo
KELVIN__MIDDLEWARES__myecho__COMMAND_STRING=!echo
KELVIN__MIDDLEWARES__myinvite__KIND=invite
KELVIN__MIDDLEWARES__myinvite__COMMAND_STRING=!invite
KELVIN__MIDDLEWARES__myinvite__USES_ALLOWED=1
KELVIN__MIDDLEWARES__myinvite__EXPIRY=7d

# Dummy service for testing
KELVIN__SERVICES__test_dummy__KIND=dummy
KELVIN__SERVICES__test_dummy__INTERVAL_MS=5000
KELVIN__SERVICES__test_dummy__MIDDLEWARE=logger

# Matrix service for production
KELVIN__SERVICES__matrix_main__KIND=matrix
KELVIN__SERVICES__matrix_main__HOMESERVER_URL=https://matrix.org
KELVIN__SERVICES__matrix_main__USER_ID=@kelvinbot:matrix.org
KELVIN__SERVICES__matrix_main__PASSWORD=secret_password
KELVIN__SERVICES__matrix_main__DEVICE_ID=KELVIN_PROD
KELVIN__SERVICES__matrix_main__DB_PASSPHRASE=encryption_secret
KELVIN__SERVICES__matrix_main__VERIFICATION_DEVICE_ID=DEVICEIDHERE
KELVIN__SERVICES__matrix_main__MIDDLEWARE=myecho,testcmd,logger
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

1. Create middleware struct in `src/middlewares/` implementing the `Middleware` trait
2. Add configuration variant to `MiddlewareKind` enum in `src/core/config.rs`
3. Update `instantiate_middleware_from_config()` in `src/core/middleware.rs` to handle the new kind
4. Add tests in `tests/unit/middleware.rs`
5. Update README.md with configuration examples

**Example: Adding a new "Greeter" middleware**

```rust
// src/middlewares/greeter.rs
use crate::core::{event::Event, middleware::{Middleware, Verdict}};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub struct Greeter {
    greeting: String,
}

impl Greeter {
    pub fn new(greeting: String) -> Self {
        Self { greeting }
    }
}

#[async_trait]
impl Middleware for Greeter {
    async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        cancel.cancelled().await;
        Ok(())
    }

    fn on_event(&self, event: &Event) -> anyhow::Result<Verdict> {
        // Process event and potentially send greeting
        Ok(Verdict::Continue)
    }
}
```

Then update `MiddlewareKind` enum and `instantiate_middleware_from_config()` to support it.

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
    ├── echo.rs          # Command echo middleware
    ├── invite.rs        # Registration token generation
    ├── logger.rs        # Event logging middleware
    └── movie_showtimes.rs  # Scheduled movie showtimes posting

tests/                    # Comprehensive test suite
├── unit/                # Component unit tests
├── integration/         # End-to-end integration tests
├── common/             # Shared test utilities and MockService
└── README.md          # Testing documentation
```