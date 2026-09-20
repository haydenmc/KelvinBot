//! Archive photos posted in chat rooms to a Lychee album.
//!
//! Watches one service for `RoomImage` events in a configured set of rooms
//! (or every room when the list is empty; the Matrix service never emits
//! images from direct chats) and uploads each photo to a single Lychee album
//! through the v2 API. Silent on success; on failure a short message is posted
//! back to the originating room so people know the archive missed a photo.
//!
//! The uploaded bytes are exactly what the service fetched: the original file
//! at full resolution with any EXIF intact. This module never decodes,
//! resizes or re-encodes images.
//!
//! Follows the `reqwest` patterns of the kanidm middleware.

use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::{Mutex, mpsc::Sender};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    core::{
        bus::Command,
        event::{Event, EventKind},
        middleware::{Middleware, MiddlewareContext, Verdict},
        service::ServiceId,
    },
    store::PersistentStore,
};

/// Store key under which already-archived `source_url`s are remembered.
pub const LEDGER_KEY: &str = "uploaded_source_urls";
/// Upper bound on remembered `source_url`s; the oldest are dropped first.
pub const LEDGER_MAX_ENTRIES: usize = 1000;
/// Lychee caps photo titles at this many characters.
const MAX_TITLE_CHARS: usize = 100;

/// Static configuration for the Lychee upload middleware.
pub struct LycheeUploadConfig {
    /// Service whose image events are archived.
    pub service_id: String,
    /// Rooms to archive from. Empty means every room on the service.
    pub room_ids: Vec<String>,
    /// Rooms that are never archived, even if listed in `room_ids`.
    pub exclude_room_ids: Vec<String>,
    /// Lychee origin, e.g. `https://photos.example.com`.
    pub lychee_url: String,
    /// Lychee API token, sent as a bearer token.
    pub lychee_token: SecretString,
    /// Random ID of the destination album.
    pub album_id: String,
    /// Largest file, in bytes, that is uploaded. Bigger photos are skipped.
    pub max_file_size_bytes: u64,
    /// Posted to the originating room when a photo could not be archived.
    pub failure_message: String,
    /// Posted when a photo exceeds `max_file_size_bytes`. `{max_mb}` renders
    /// the limit in whole MiB.
    pub oversize_message: String,
    /// Whole-request timeout for the upload.
    pub request_timeout: Duration,
}

pub struct LycheeUpload {
    inner: Arc<Inner>,
}

/// Everything a spawned upload task needs, shared behind one `Arc` so
/// `on_event` can stay synchronous and cheap.
struct Inner {
    cmd_tx: Sender<Command>,
    service_id: String,
    room_ids: Vec<String>,
    exclude_room_ids: Vec<String>,
    max_file_size_bytes: u64,
    failure_message: String,
    /// `oversize_message` with `{max_mb}` already rendered.
    oversize_message: String,
    api: LycheeApi,
    ledger: UploadLedger,
}

/// The per-image fields cloned out of a `RoomImage` event.
struct ImageJob {
    service_id: ServiceId,
    room_id: String,
    sender_id: String,
    sender_display_name: Option<String>,
    body: String,
    source_url: String,
    mimetype: Option<String>,
    image_data: Option<Arc<[u8]>>,
}

impl LycheeUpload {
    pub fn new(ctx: MiddlewareContext, config: LycheeUploadConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .context("failed to build lychee HTTP client")?;
        let oversize_message =
            render_oversize_message(&config.oversize_message, config.max_file_size_bytes);
        Ok(Self {
            inner: Arc::new(Inner {
                cmd_tx: ctx.cmd_tx,
                service_id: config.service_id,
                room_ids: config.room_ids,
                exclude_room_ids: config.exclude_room_ids,
                max_file_size_bytes: config.max_file_size_bytes,
                failure_message: config.failure_message,
                oversize_message,
                api: LycheeApi {
                    http,
                    config: Arc::new(LycheeApiConfig {
                        base_url: config.lychee_url.trim_end_matches('/').to_string(),
                        token: config.lychee_token,
                        album_id: config.album_id,
                    }),
                },
                ledger: UploadLedger::new(ctx.store),
            }),
        })
    }
}

impl Inner {
    async fn archive_image(&self, job: ImageJob) {
        if !self.ledger.try_reserve(&job.source_url).await {
            debug!(source_url=%job.source_url, "photo already archived, skipping");
            return;
        }

        let Some(bytes) = job.image_data.clone() else {
            warn!(
                source_url=%job.source_url,
                room_id=%job.room_id,
                "image bytes unavailable; photo not archived"
            );
            // Not committed: a later replay of the event gets another try.
            self.ledger.release(&job.source_url).await;
            self.send_room_message(&job, self.failure_message.clone()).await;
            return;
        };

        let size = bytes.len() as u64;
        if size > self.max_file_size_bytes {
            warn!(
                size,
                limit=self.max_file_size_bytes,
                file_name=%job.body,
                room_id=%job.room_id,
                "photo exceeds max_file_size_bytes; not archived"
            );
            // Deterministic outcome, so record it and never re-notify on replay.
            self.ledger.commit(&job.source_url).await;
            self.send_room_message(&job, self.oversize_message.clone()).await;
            return;
        }

        // Lychee derives the extension server-side; only the name is sent.
        let (file_name, _extension) = derive_file_name(&job.body, job.mimetype.as_deref());
        let photo = PhotoUpload {
            bytes,
            file_name: file_name.clone(),
            mimetype: job.mimetype.clone(),
            title: photo_title(&job.body),
            description: format_description(
                &job.sender_id,
                job.sender_display_name.as_deref(),
                &job.room_id,
            ),
        };

        match self.api.upload_photo(&photo).await {
            Ok(()) => {
                self.ledger.commit(&job.source_url).await;
                info!(
                    file_name=%file_name,
                    size,
                    room_id=%job.room_id,
                    sender_id=%job.sender_id,
                    "photo archived to lychee"
                );
            }
            Err(e) => {
                self.ledger.release(&job.source_url).await;
                error!(
                    error=%e,
                    file_name=%file_name,
                    room_id=%job.room_id,
                    "failed to archive photo to lychee"
                );
                self.send_room_message(&job, self.failure_message.clone()).await;
            }
        }
    }

    async fn send_room_message(&self, job: &ImageJob, body: String) {
        let command = Command::SendRoomMessage {
            service_id: job.service_id.clone(),
            room_id: job.room_id.clone(),
            body,
            markdown_body: None,
            response_tx: None,
        };
        if let Err(e) = self.cmd_tx.send(command).await {
            error!(
                service_id=%job.service_id.0,
                room_id=%job.room_id,
                error=%e,
                "failed to send lychee upload notice"
            );
        }
    }
}

#[async_trait]
impl Middleware for LycheeUpload {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        let inner = &self.inner;
        info!(
            service=%inner.service_id,
            rooms=?inner.room_ids,
            excluded_rooms=?inner.exclude_room_ids,
            lychee_url=%inner.api.config.base_url,
            album_id=%inner.api.config.album_id,
            max_file_size_bytes=inner.max_file_size_bytes,
            "lychee_upload middleware running..."
        );

        // Best-effort connectivity check. Only ever warns: a Lychee outage at
        // startup must not take the middleware (or the bus) down.
        tokio::select! {
            _ = cancel.cancelled() => {}
            result = inner.api.list_albums() => match result {
                Ok(albums) => {
                    let ids = album_ids(&albums);
                    if ids.iter().any(|id| id == &inner.api.config.album_id) {
                        info!(albums=ids.len(), "lychee connectivity check ok; album found");
                    } else {
                        warn!(
                            albums=ids.len(),
                            album_id=%inner.api.config.album_id,
                            "lychee reachable but album id not among top-level albums (it may be a sub-album)"
                        );
                    }
                }
                Err(e) => warn!(error=%e, "lychee connectivity check failed; uploads may fail"),
            }
        }

        cancel.cancelled().await;
        info!("lychee_upload middleware shutting down...");
        Ok(())
    }

    fn on_event(&self, event: &Event) -> Result<Verdict> {
        if event.service_id.0 != self.inner.service_id {
            return Ok(Verdict::Continue);
        }
        let EventKind::RoomImage {
            room_id,
            sender_id,
            sender_display_name,
            is_self,
            body,
            source_url,
            mimetype,
            image_data, // Option<Arc<[u8]>> — clone is one atomic increment
            ..
        } = &event.kind
        else {
            return Ok(Verdict::Continue);
        };

        if !room_in_scope(room_id, &self.inner.room_ids, &self.inner.exclude_room_ids) {
            debug!(room_id=%room_id, "room not in scope for lychee upload");
            return Ok(Verdict::Continue);
        }
        if *is_self {
            debug!("ignoring image from bot itself");
            return Ok(Verdict::Continue);
        }
        if !is_supported_image(mimetype.as_deref()) {
            debug!(mimetype=?mimetype, "ignoring non-image attachment");
            return Ok(Verdict::Continue);
        }

        let job = ImageJob {
            service_id: event.service_id.clone(),
            room_id: room_id.clone(),
            sender_id: sender_id.clone(),
            sender_display_name: sender_display_name.clone(),
            body: body.clone(),
            source_url: source_url.clone(),
            mimetype: mimetype.clone(),
            image_data: image_data.clone(),
        };
        let inner = self.inner.clone();
        tokio::spawn(async move { inner.archive_image(job).await });

        Ok(Verdict::Continue)
    }
}

// --- Filtering and naming helpers -------------------------------------------

/// Whether an image from `room_id` should be archived: it must be in
/// `room_ids` (or `room_ids` must be empty) and not in `exclude_room_ids`.
pub fn room_in_scope(room_id: &str, room_ids: &[String], exclude_room_ids: &[String]) -> bool {
    if !room_ids.is_empty() && !room_ids.iter().any(|r| r == room_id) {
        return false;
    }
    !exclude_room_ids.iter().any(|r| r == room_id)
}

/// `None` (unknown) or any `image/*` mimetype is accepted; videos and other
/// attachments are not. The Matrix service only emits `RoomImage` for
/// `m.image`, so this is a guard against mislabelled uploads.
pub fn is_supported_image(mimetype: Option<&str>) -> bool {
    match mimetype {
        None => true,
        Some(m) => mime_essence(m).starts_with("image/"),
    }
}

/// Map a mimetype to a file extension (without the dot).
pub fn extension_for_mimetype(mimetype: &str) -> Option<&'static str> {
    match mime_essence(mimetype).as_str() {
        "image/jpeg" | "image/jpg" | "image/pjpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "image/heic" => Some("heic"),
        "image/heif" => Some("heif"),
        "image/avif" => Some("avif"),
        "image/tiff" => Some("tiff"),
        "image/bmp" => Some("bmp"),
        _ => None,
    }
}

/// Derive `(file_name, extension)` for the upload from the Matrix `body`
/// (normally the original filename) and mimetype. A name that already has a
/// short alphanumeric extension is kept verbatim; otherwise the mimetype's
/// extension (falling back to `jpg`) is appended, and an empty body becomes
/// `photo.<ext>`.
pub fn derive_file_name(body: &str, mimetype: Option<&str>) -> (String, String) {
    let name = base_name(body);
    if let Some((_, ext)) = split_extension(name) {
        return (name.to_string(), ext.to_ascii_lowercase());
    }
    let ext = mimetype.and_then(extension_for_mimetype).unwrap_or("jpg");
    let stem = name.trim_end_matches('.');
    if stem.is_empty() {
        (format!("photo.{ext}"), ext.to_string())
    } else {
        (format!("{stem}.{ext}"), ext.to_string())
    }
}

/// Photo title: the file stem of `body` when it carries a real name, else
/// `None` so Lychee derives its own.
pub fn photo_title(body: &str) -> Option<String> {
    let name = base_name(body);
    let stem = split_extension(name).map(|(stem, _)| stem).unwrap_or(name).trim_end_matches('.');
    let stem = stem.trim();
    if stem.is_empty() {
        return None;
    }
    Some(stem.chars().take(MAX_TITLE_CHARS).collect())
}

/// `Sent by <display name or sender id> in <room_id>`.
pub fn format_description(
    sender_id: &str,
    sender_display_name: Option<&str>,
    room_id: &str,
) -> String {
    let who = sender_display_name.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(sender_id);
    format!("Sent by {who} in {room_id}")
}

/// Replace `{max_mb}` in `template` with `max_bytes` expressed in whole MiB.
pub fn render_oversize_message(template: &str, max_bytes: u64) -> String {
    template.replace("{max_mb}", &(max_bytes / (1024 * 1024)).to_string())
}

/// Collect the `id` of every album object found in any top-level array of a
/// Lychee `GET /Albums` response (`albums`, `shared_albums`, ...).
pub fn album_ids(albums: &serde_json::Value) -> Vec<String> {
    let Some(map) = albums.as_object() else {
        return Vec::new();
    };
    map.values()
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .filter_map(|album| album.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect()
}

fn mime_essence(mimetype: &str) -> String {
    mimetype.split(';').next().unwrap_or("").trim().to_ascii_lowercase()
}

fn base_name(body: &str) -> &str {
    body.trim().rsplit(['/', '\\']).next().unwrap_or("").trim()
}

/// Split `name.ext` when `ext` is a short alphanumeric extension.
fn split_extension(name: &str) -> Option<(&str, &str)> {
    let (stem, ext) = name.rsplit_once('.')?;
    let valid = !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= 5
        && ext.chars().all(|c| c.is_ascii_alphanumeric());
    valid.then_some((stem, ext))
}

// --- Dedup ledger ------------------------------------------------------------

/// Remembers which `source_url`s have been archived so a restart (and the
/// initial-sync replay that follows) never uploads the same photo twice.
/// Persisted through the middleware's store; an in-flight set guards the
/// window between reserving and committing.
struct UploadLedger {
    store: Arc<PersistentStore>,
    in_flight: Mutex<HashSet<String>>,
}

impl UploadLedger {
    fn new(store: Arc<PersistentStore>) -> Self {
        Self { store, in_flight: Mutex::new(HashSet::new()) }
    }

    /// Reserve `key` for upload. Returns `false` if it was already archived
    /// or is currently being uploaded.
    async fn try_reserve(&self, key: &str) -> bool {
        let mut in_flight = self.in_flight.lock().await;
        if in_flight.contains(key) {
            return false;
        }
        let done: Vec<String> = self.store.get(LEDGER_KEY).await.unwrap_or_default();
        if done.iter().any(|k| k == key) {
            return false;
        }
        in_flight.insert(key.to_string());
        true
    }

    /// Record `key` as archived and drop the in-flight reservation.
    async fn commit(&self, key: &str) {
        let mut in_flight = self.in_flight.lock().await;
        let mut done: Vec<String> = self.store.get(LEDGER_KEY).await.unwrap_or_default();
        if !done.iter().any(|k| k == key) {
            done.push(key.to_string());
        }
        if done.len() > LEDGER_MAX_ENTRIES {
            let excess = done.len() - LEDGER_MAX_ENTRIES;
            done.drain(..excess);
        }
        if let Err(e) = self.store.set(LEDGER_KEY, &done).await {
            warn!(error=%e, "failed to persist lychee upload ledger");
        }
        in_flight.remove(key);
    }

    /// Drop the in-flight reservation without recording `key`, so a later
    /// replay can retry.
    async fn release(&self, key: &str) {
        self.in_flight.lock().await.remove(key);
    }
}

// --- Lychee API v2 -----------------------------------------------------------

/// One photo ready to upload. `bytes` are passed through untouched.
struct PhotoUpload {
    bytes: Arc<[u8]>,
    file_name: String,
    mimetype: Option<String>,
    title: Option<String>,
    description: String,
}

/// HTTP client bundle for the Lychee v2 API. Cheaply cloneable.
#[derive(Clone)]
struct LycheeApi {
    http: reqwest::Client,
    config: Arc<LycheeApiConfig>,
}

struct LycheeApiConfig {
    /// Origin without a trailing slash.
    base_url: String,
    token: SecretString,
    album_id: String,
}

impl LycheeApi {
    fn endpoint(&self, path: &str) -> String {
        format!("{}/api/v2/{}", self.config.base_url, path)
    }

    /// `GET /Albums`, used only as a startup connectivity check.
    ///
    /// Lychee's JSON routes answer 406 unless the request carries both
    /// `Accept: application/json` and `Content-Type: application/json`, even
    /// on a body-less GET. (The multipart upload is exempt: it must keep its
    /// own content type.)
    async fn list_albums(&self) -> Result<serde_json::Value> {
        let response = self
            .http
            .get(self.endpoint("Albums"))
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .bearer_auth(self.config.token.expose_secret())
            .send()
            .await
            .context("failed to send albums request to lychee")?;
        ensure_success(response, "lychee albums")
            .await?
            .json()
            .await
            .context("failed to parse lychee albums response")
    }

    /// `POST /Photo` as a single-chunk multipart upload into the configured
    /// album. Any 2xx is success; the response body is only logged.
    async fn upload_photo(&self, photo: &PhotoUpload) -> Result<()> {
        let form = build_upload_form(&self.config.album_id, photo);
        let response = self
            .http
            .post(self.endpoint("Photo"))
            .header(reqwest::header::ACCEPT, "application/json")
            .bearer_auth(self.config.token.expose_secret())
            .multipart(form)
            .send()
            .await
            .context("failed to send photo upload to lychee")?;
        let response = ensure_success(response, "lychee photo upload").await?;
        match response.json::<serde_json::Value>().await {
            Ok(meta) => debug!(
                stage=?meta.get("stage"),
                expected_id=?meta.get("expected_id"),
                "lychee accepted photo upload"
            ),
            Err(e) => {
                debug!(error=%e, "lychee upload response was not JSON; treating 2xx as success")
            }
        }
        Ok(())
    }
}

/// Build the multipart form for Lychee's chunked upload endpoint, sending the
/// whole file as chunk 1 of 1. For the first chunk Lychee requires
/// `uuid_name` and `extension` to be empty (`FileUuidRule`, `ExtensionRule`);
/// it derives both server-side.
fn build_upload_form(album_id: &str, photo: &PhotoUpload) -> reqwest::multipart::Form {
    // Raw bytes only: no decoding, resizing or re-encoding, so the original
    // resolution and EXIF survive.
    let make_part =
        || reqwest::multipart::Part::bytes(photo.bytes.to_vec()).file_name(photo.file_name.clone());
    let part = match &photo.mimetype {
        Some(mime) => make_part().mime_str(mime).unwrap_or_else(|_| make_part()),
        None => make_part(),
    };

    let mut form = reqwest::multipart::Form::new()
        .text("album_id", album_id.to_string())
        .text("file_name", photo.file_name.clone())
        .text("uuid_name", "")
        .text("extension", "")
        .text("chunk_number", "1")
        .text("total_chunks", "1")
        .text("description", photo.description.clone());
    if let Some(title) = &photo.title {
        form = form.text("title", title.clone());
    }
    form.part("file", part)
}

/// Return the response if it has a success status, otherwise build an error
/// including the status and response body.
async fn ensure_success(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    anyhow::bail!("{what} request failed: HTTP {status} - {text}")
}
