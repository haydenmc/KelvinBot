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
  Matrix    Routing   Logging, Echo, Invite
  Mumble     Core     Chat Relay, etc.
  Dummy
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

#### Calendar Agenda Middleware
Posts a daily agenda to a room from a shared `webcal`/`ics` calendar feed, including countdowns to upcoming multi-day events and reminders for upcoming one-off events. Nothing is posted on days where none of those sections have content.

**Configuration:**
```bash
KELVIN__MIDDLEWARES__<name>__KIND=calendaragenda
KELVIN__MIDDLEWARES__<name>__SERVICE_ID=<service_name>
KELVIN__MIDDLEWARES__<name>__ROOM_ID=<room_id>
KELVIN__MIDDLEWARES__<name>__CALENDAR_URL=<webcal_or_https_ics_url>
KELVIN__MIDDLEWARES__<name>__CALENDAR_LINK=<public_calendar_url>   # Optional
KELVIN__MIDDLEWARES__<name>__CALENDAR_LINK_TEXT=<link_text>        # Optional
KELVIN__MIDDLEWARES__<name>__POST_AT_TIME=<HH:MM>
KELVIN__MIDDLEWARES__<name>__COUNTDOWN_DAYS=90,60,30,14,7          # Optional
KELVIN__MIDDLEWARES__<name>__REMINDER_DAYS=7,1                     # Optional
KELVIN__MIDDLEWARES__<name>__MULTI_DAY_MIN_DAYS=2                  # Optional
KELVIN__MIDDLEWARES__<name>__HEADING_TODAY=Today                   # Optional
KELVIN__MIDDLEWARES__<name>__HEADING_REMINDERS=Coming up           # Optional
KELVIN__MIDDLEWARES__<name>__HEADING_COUNTDOWNS=Countdowns         # Optional
KELVIN__MIDDLEWARES__<name>__COMMAND_STRING=!events                # Optional
```

**Parameters:**
- `CALENDAR_URL`: The ICS feed the bot reads. `webcal://` and `webcals://` are rewritten to `https://`. Treated as a secret — it is never rendered into a message or logged
- `CALENDAR_LINK`: Optional human-facing calendar link, rendered as a footer on the agenda. Kept separate from `CALENDAR_URL` so the private feed is never shared in chat. Omit for no footer
- `POST_AT_TIME`: Local time of day to post the daily agenda, 24-hour format (e.g., `08:00`)
- `COUNTDOWN_DAYS`: Days-before intervals at which multi-day events get a countdown line (default `90,60,30,14,7`)
- `REMINDER_DAYS`: Days-before intervals at which single-day, non-recurring events get a reminder line (default `7,1`)
- `MULTI_DAY_MIN_DAYS`: Minimum length in days for an event to count as "multi-day" (default `2`)
- `COMMAND_STRING`: Chat command that posts the agenda on demand (default `!events`). Set to an empty string to disable it

**Example:**
```bash
KELVIN__MIDDLEWARES__calendar__KIND=calendaragenda
KELVIN__MIDDLEWARES__calendar__SERVICE_ID=matrix_main
KELVIN__MIDDLEWARES__calendar__ROOM_ID=!abcdef123456:matrix.org
KELVIN__MIDDLEWARES__calendar__CALENDAR_URL=webcal://calendar.example.com/private/feed.ics
KELVIN__MIDDLEWARES__calendar__CALENDAR_LINK=https://calendar.example.com/shared/abc
KELVIN__MIDDLEWARES__calendar__POST_AT_TIME=08:00
```

**Behavior:**
- Fetches and parses the feed at `POST_AT_TIME` each day, expanding recurring events (`RRULE`/`RDATE`/`EXDATE`)
- **Today**: every event *starting* today. Multi-day events are listed once, on their first day, annotated with the day they run through
- **Coming up**: single-day, non-recurring events whose start is exactly one of `REMINDER_DAYS` away. Recurring events are excluded so weekly meetings don't nag
- **Countdowns**: events spanning at least `MULTI_DAY_MIN_DAYS` whose start is exactly one of `COUNTDOWN_DAYS` away
- Posts nothing when all three sections are empty
- Records the last-posted date in its store, so a restart doesn't repost the same day. If the bot is down past `POST_AT_TIME`, it posts on startup instead
- All date and time handling uses the bot's local timezone

Example output:
```
### 📅 Tuesday, August 4

**Today**
- 8 PM – 10 PM — Game night _(Living room)_
- All day — Camping trip (through Sat, Aug 8)

**Coming up**
- Alice's birthday party — in 7 days (Tue, Aug 11)

**Countdowns**
- Beach week — in 30 days (Sep 3 – Sep 7)

[View the full calendar](https://calendar.example.com/shared/abc)
```

#### Chat Relay Middleware
Relays messages from one service/room to another service/room with a prefix tag indicating the source and sender.

**Configuration:**
```bash
KELVIN__MIDDLEWARES__<name>__KIND=chatrelay
KELVIN__MIDDLEWARES__<name>__SOURCE_SERVICE_ID=<source_service>
KELVIN__MIDDLEWARES__<name>__SOURCE_ROOM_ID=<room_id>        # Optional
KELVIN__MIDDLEWARES__<name>__DEST_SERVICE_ID=<dest_service>
KELVIN__MIDDLEWARES__<name>__DEST_ROOM_ID=<dest_room_id>
KELVIN__MIDDLEWARES__<name>__PREFIX_TAG=<tag>
```

**Parameters:**
- `SOURCE_SERVICE_ID`: Service to relay messages from (e.g., `mumble_main`, `matrix_main`)
- `SOURCE_ROOM_ID`: Optional - specific room/channel to relay from. If omitted, relays from all rooms
- `DEST_SERVICE_ID`: Service to send relayed messages to
- `DEST_ROOM_ID`: Room/channel ID to send relayed messages to
- `PREFIX_TAG`: Tag to prefix relayed messages with

**Example 1: Relay Mumble to Matrix**
```bash
KELVIN__MIDDLEWARES__mumble_relay__KIND=chatrelay
KELVIN__MIDDLEWARES__mumble_relay__SOURCE_SERVICE_ID=mumble_main
KELVIN__MIDDLEWARES__mumble_relay__DEST_SERVICE_ID=matrix_main
KELVIN__MIDDLEWARES__mumble_relay__DEST_ROOM_ID=!voice:matrix.org
KELVIN__MIDDLEWARES__mumble_relay__PREFIX_TAG=Mumble
```

**Example 2: Relay specific Matrix room to another**
```bash
KELVIN__MIDDLEWARES__general_relay__KIND=chatrelay
KELVIN__MIDDLEWARES__general_relay__SOURCE_SERVICE_ID=matrix_main
KELVIN__MIDDLEWARES__general_relay__SOURCE_ROOM_ID=!general:matrix.org
KELVIN__MIDDLEWARES__general_relay__DEST_SERVICE_ID=matrix_main
KELVIN__MIDDLEWARES__general_relay__DEST_ROOM_ID=!announcements:matrix.org
KELVIN__MIDDLEWARES__general_relay__PREFIX_TAG=General
```

**Message Format:**
Relayed messages appear as:
```
[PREFIX_TAG] sender_display_name: message body
```

For example:
```
[Mumble] Alice: Hello everyone!
[General] Bob: Can someone help me?
```

If a user doesn't have a display name, their user ID is used as fallback.

**Behavior:**
- Only relays room/channel messages (not direct messages)
- Automatically filters out the bot's own messages to prevent loops
- Preserves original message content
- Uses sender's display name when available, falls back to user ID
- Operates in real-time as messages arrive
- Can relay between different services (cross-platform) or same service (room-to-room)

**Important:**
- Be careful with bidirectional relays (A→B and B→A) as they may create message loops
- The middleware does not prevent relay loops - configure carefully
- Messages are relayed as plain text; formatting may not be preserved across different platforms

#### Weekly Gathering Middleware

Runs a weekly poll in a room. Ahead of a recurring event it posts an announcement with reactions
for virtual vs in-person, volunteering to host, and (optionally) candidate start times. Later it
posts a finalization message with the results and the selected host.

**Configuration:**
```bash
KELVIN__MIDDLEWARES__<name>__KIND=weeklygathering
KELVIN__MIDDLEWARES__<name>__SERVICE_ID=<service_name>
KELVIN__MIDDLEWARES__<name>__ROOM_ID=<room_id>
KELVIN__MIDDLEWARES__<name>__EVENT_DAY_OF_WEEK=<Monday..Sunday>
KELVIN__MIDDLEWARES__<name>__EVENT_TIME_OPTIONS=<HH:MM,HH:MM,...>
KELVIN__MIDDLEWARES__<name>__FINALIZE_TIME=<HH:MM>           # Poll closes, on the event day
KELVIN__MIDDLEWARES__<name>__POLL_OPEN_MINUTES=<minutes>     # Poll opens this long before
KELVIN__MIDDLEWARES__<name>__REACTION_VIRTUAL=<emoji>
KELVIN__MIDDLEWARES__<name>__REACTION_IN_PERSON=<emoji>
KELVIN__MIDDLEWARES__<name>__REACTION_HOST=<emoji>
KELVIN__MIDDLEWARES__<name>__ANNOUNCEMENT_MESSAGE=<template>
KELVIN__MIDDLEWARES__<name>__FINALIZATION_VIRTUAL_MESSAGE=<template>
KELVIN__MIDDLEWARES__<name>__FINALIZATION_IN_PERSON_MESSAGE=<template>
KELVIN__MIDDLEWARES__<name>__FINALIZATION_NO_VOTES_MESSAGE=<template>
KELVIN__MIDDLEWARES__<name>__TIME_PROMPT_MESSAGE=<text>      # Optional
KELVIN__MIDDLEWARES__<name>__HOUSEHOLDS__<key>__NAME=<display_name>
KELVIN__MIDDLEWARES__<name>__HOUSEHOLDS__<key>__MEMBERS=<user_id,user_id,...>
```

**Host Selection:**
Hosts are chosen from the volunteers, preferring whoever hosted least recently. Members of the same
household count as a single candidate and have their host history updated together, so a household
isn't picked two weeks running just because a different member volunteered.

**Scheduling:**
The gathering recurs on `EVENT_DAY_OF_WEEK`. `FINALIZE_TIME` is when the poll closes, on the event
day itself, and the poll opens `POLL_OPEN_MINUTES` earlier — so the announcement can land days
ahead (4200 minutes ≈ 2.9 days) while the decision is always made the morning of. There is no
separate event-time setting: the start time comes from `EVENT_TIME_OPTIONS`.

Because finalization always lands on the event day, this middleware only organizes same-day plans.
Once `FINALIZE_TIME` passes, the cycle is considered done and the next gathering is a week out.

> **Migrating from `EVENT_TIME`:** earlier versions took `EVENT_TIME`, `ANNOUNCE_MINUTES_BEFORE` and
> `FINALIZE_MINUTES_BEFORE`, all relative to a fixed event instant. To convert:
> `FINALIZE_TIME` = `EVENT_TIME` − `FINALIZE_MINUTES_BEFORE`,
> `POLL_OPEN_MINUTES` = `ANNOUNCE_MINUTES_BEFORE` − `FINALIZE_MINUTES_BEFORE`, and
> `EVENT_TIME_OPTIONS` = the old `EVENT_TIME` if you don't want a vote. The removed keys are not
> accepted, so a stale config fails at startup rather than silently rescheduling itself.

**Event Time Voting:**
`EVENT_TIME_OPTIONS` is a required comma-separated list of 24-hour `HH:MM` start times (up to 10).
Each is assigned a keycap reaction — 1️⃣ 2️⃣ 3️⃣ … — by position, and participants may approve as many
as suit them. A **single** time means a fixed start with no vote: no keycap reactions are seeded and
no host prompt is shown. With two or more, at finalization:

- **Virtual gatherings** get the most-voted time automatically (ties go to the earlier time).
- **In-person gatherings** leave the choice to the host, since the venue is theirs. Every configured
  time is offered as a reaction on the finalization message, ordered most-preferred first. When the
  host — or anyone in their household — reacts with a time, the message is edited in place to state
  the scheduled time. If nobody picks, the message keeps showing the options.

**Message Placeholders:**

| Placeholder | Available in | Renders |
|---|---|---|
| `{event_time}` | both | `Saturday at 8:00pm`, or `Saturday (time TBD)` while unresolved |
| `{reaction_virtual}`, `{reaction_in_person}`, `{reaction_host}` | announcement | The configured emoji |
| `{time_options}` | announcement | One `1️⃣ 4:30pm` line per configured time |
| `{virtual_count}`, `{in_person_count}` | finalization | Vote counts |
| `{host}` | finalization | Host or household display name |
| `{time_results}` | finalization | Times ranked by votes, marking the selected one |
| `{time_prompt}` | finalization | `TIME_PROMPT_MESSAGE`, only while the host has yet to pick |

**Example:**
```bash
KELVIN__MIDDLEWARES__gathering__KIND=weeklygathering
KELVIN__MIDDLEWARES__gathering__SERVICE_ID=matrix_main
KELVIN__MIDDLEWARES__gathering__ROOM_ID=!yourroom:matrix.org
KELVIN__MIDDLEWARES__gathering__EVENT_DAY_OF_WEEK=Saturday
KELVIN__MIDDLEWARES__gathering__EVENT_TIME_OPTIONS=16:30,20:00,21:30
KELVIN__MIDDLEWARES__gathering__FINALIZE_TIME=14:00            # Poll closes Saturday 2pm
KELVIN__MIDDLEWARES__gathering__POLL_OPEN_MINUTES=4200         # Opens ~2.9 days earlier
KELVIN__MIDDLEWARES__gathering__REACTION_VIRTUAL=💻
KELVIN__MIDDLEWARES__gathering__REACTION_IN_PERSON=🏠
KELVIN__MIDDLEWARES__gathering__REACTION_HOST=🙋
KELVIN__MIDDLEWARES__gathering__ANNOUNCEMENT_MESSAGE="# Gathering Time!\n\nComing up **{event_time}**! Vote for your preference:\n\n - {reaction_virtual} Virtual\n - {reaction_in_person} In-Person\n - {reaction_host} Volunteer to Host\n\nWhich times work for you?\n\n{time_options}"
KELVIN__MIDDLEWARES__gathering__FINALIZATION_VIRTUAL_MESSAGE="# It's Virtual!\n\nMeet **{event_time}**.\n\n{time_results}"
KELVIN__MIDDLEWARES__gathering__FINALIZATION_IN_PERSON_MESSAGE="# It's In-Person!\n\n{host} is hosting, **{event_time}**.\n\n{time_results}\n\n{time_prompt}"
KELVIN__MIDDLEWARES__gathering__FINALIZATION_NO_VOTES_MESSAGE="No votes this week — consider it canceled."
KELVIN__MIDDLEWARES__gathering__TIME_PROMPT_MESSAGE="{host}: react with the time that works for you to lock it in."

# Treat two users as one household for host rotation
KELVIN__MIDDLEWARES__gathering__HOUSEHOLDS__h1__NAME="Alice and Bob"
KELVIN__MIDDLEWARES__gathering__HOUSEHOLDS__h1__MEMBERS=@alice:matrix.org,@bob:matrix.org

KELVIN__SERVICES__matrix_main__MIDDLEWARE=gathering,logger
```

#### Lychee Upload Middleware

Archives photos posted in chat rooms to an album on a self-hosted [Lychee](https://lycheeorg.dev/)
instance (v6 or newer, API v2), so shared media outlives the chat scrollback. Uploads are silent;
when a photo could not be archived, a short message is posted back into the room it came from.

**Prerequisites:**
- A Lychee API token from **Settings → Profile → API Token** (shown once; treat it like a password).
- The destination album's ID: the 24-character string at the end of the album URL.

**Configuration:**
```bash
KELVIN__MIDDLEWARES__<name>__KIND=lycheeupload
KELVIN__MIDDLEWARES__<name>__SERVICE_ID=<service_name>
KELVIN__MIDDLEWARES__<name>__ROOM_IDS=<room_id,room_id,...>        # Optional; empty = all rooms
KELVIN__MIDDLEWARES__<name>__EXCLUDE_ROOM_IDS=<room_id,...>         # Optional
KELVIN__MIDDLEWARES__<name>__LYCHEE_URL=<https://photos.example.com>
KELVIN__MIDDLEWARES__<name>__LYCHEE_TOKEN=<api_token>
KELVIN__MIDDLEWARES__<name>__ALBUM_ID=<album_id>
KELVIN__MIDDLEWARES__<name>__MAX_FILE_SIZE_BYTES=<bytes>            # Optional, default 52428800
KELVIN__MIDDLEWARES__<name>__FAILURE_MESSAGE=<text>                 # Optional
KELVIN__MIDDLEWARES__<name>__OVERSIZE_MESSAGE=<text>                # Optional, {max_mb} placeholder
KELVIN__MIDDLEWARES__<name>__REQUEST_TIMEOUT=<duration>             # Optional, default 60s
```

**Behavior:**
- Only image messages (`m.image`) are archived. Videos and generic file attachments are ignored, as
  are images sent in direct chats with the bot and the bot's own images.
- With `ROOM_IDS` empty or omitted, every room on the service is archived except direct chats.
  `EXCLUDE_ROOM_IDS` always wins.
- **Originals only.** The bot uploads the exact bytes it received from Matrix: full resolution, EXIF
  intact, never resized or re-encoded. Matrix clients can still compress before sending, so ask
  people to send originals if quality matters.
- Uploads go in a single chunk, so `MAX_FILE_SIZE_BYTES` must stay at or below the PHP
  `upload_max_filesize` / `post_max_size` of your Lychee instance. Larger photos are skipped and
  `OVERSIZE_MESSAGE` is posted instead.
- Archived photos are remembered in `<data_directory>/<name>.store.json`, so a restart never uploads
  the same photo twice. Each photo's Lychee description records the sender and room.
- Only photos posted while the bot is running are archived. The Matrix service completes an initial
  sync before it starts delivering events, so nothing from the sync backlog (old history on a fresh
  store, or photos sent while the bot was down) is ever uploaded.

**Example:**
```bash
KELVIN__MIDDLEWARES__lychee__KIND=lycheeupload
KELVIN__MIDDLEWARES__lychee__SERVICE_ID=matrix_main
KELVIN__MIDDLEWARES__lychee__ROOM_IDS=!photos:matrix.org,!trips:matrix.org
KELVIN__MIDDLEWARES__lychee__LYCHEE_URL=https://photos.example.com
KELVIN__MIDDLEWARES__lychee__LYCHEE_TOKEN=your_lychee_api_token
KELVIN__MIDDLEWARES__lychee__ALBUM_ID=AbCdEfGhIjKlMnOpQrStUvWx
KELVIN__SERVICES__matrix_main__MIDDLEWARE=logger,lychee
```

See `.env.lychee.example` for a fuller walkthrough, including curl commands to sanity-check the
token and album ID.

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
KELVIN__MIDDLEWARES__chat_relay__KIND=chatrelay
KELVIN__MIDDLEWARES__chat_relay__SOURCE_SERVICE_ID=mumble_main
KELVIN__MIDDLEWARES__chat_relay__DEST_SERVICE_ID=matrix_main
KELVIN__MIDDLEWARES__chat_relay__DEST_ROOM_ID=!voice:matrix.org
KELVIN__MIDDLEWARES__chat_relay__PREFIX_TAG=Mumble

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
│   ├── matrix.rs         # Matrix homeserver integration
│   └── mumble.rs         # Mumble voice chat integration
└── middlewares/          # Event processors
    ├── attendance_relay.rs  # User presence tracking and announcements
    ├── calendar_agenda.rs   # Daily agenda from a webcal/ics feed
    ├── chat_relay.rs        # Cross-platform message relaying
    ├── echo.rs              # Command echo middleware
    ├── invite.rs            # Registration token generation
    ├── logger.rs            # Event logging middleware
    └── movie_showtimes.rs   # Scheduled movie showtimes posting

tests/                    # Comprehensive test suite
├── unit/                # Component unit tests
├── integration/         # End-to-end integration tests
├── common/             # Shared test utilities and MockService
└── README.md          # Testing documentation
```