use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, MiddlewareContext},
    service::ServiceId,
};

/// Largest width or height, in pixels, of an image that will be decoded for a
/// thumbnail. Well beyond any camera, but stops absurd dimensions early.
const MAX_DECODE_DIMENSION: u32 = 16_384;

/// Memory budget for decoding one image. Images come from anyone in the
/// relayed room, and a small file can declare enormous dimensions, so the
/// decoder is not allowed its default 512 MiB. 256 MiB still covers an
/// uncompressed 80-megapixel RGB photo.
const MAX_DECODE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;

/// How many thumbnails may be decoded at once across all relays. A burst of
/// photos would otherwise decode in parallel and multiply the budget above.
static THUMBNAIL_JOBS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Decode `raw`, shrink it to fit `max_width` x `max_height` and re-encode it
/// as a JPEG, within the decode limits above.
fn make_thumbnail(
    raw: &[u8],
    max_width: u32,
    max_height: u32,
    jpeg_quality: u8,
) -> anyhow::Result<Vec<u8>> {
    use image::{ImageEncoder, ImageReader, Limits, codecs::jpeg::JpegEncoder};
    use std::io::Cursor;

    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);

    let mut reader = ImageReader::new(Cursor::new(raw)).with_guessed_format()?;
    reader.limits(limits);
    let img = reader.decode()?;

    // Normalise to Rgb8 before encoding — JpegEncoder can silently produce
    // empty output when handed an Rgb8 DynamicImage via write_with_encoder,
    // so we call write_image directly with an explicit color type instead.
    let thumb = img.thumbnail(max_width, max_height).into_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, jpeg_quality).write_image(
        thumb.as_raw(),
        thumb.width(),
        thumb.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(out)
}

pub struct ChatRelayConfig {
    pub source_service_id: String,
    pub source_room_id: Option<String>,
    pub dest_service_id: String,
    pub dest_room_id: String,
    pub prefix_tag: String,
    pub thumbnail_max_width: u32,
    pub thumbnail_max_height: u32,
    pub thumbnail_jpeg_quality: u8,
}

pub struct ChatRelay {
    cmd_tx: Sender<Command>,
    source_service_id: String,
    source_room_id: Option<String>,
    dest_service_id: String,
    dest_room_id: String,
    prefix_tag: String,
    http_client: reqwest::Client,
    thumbnail_max_width: u32,
    thumbnail_max_height: u32,
    thumbnail_jpeg_quality: u8,
}

impl ChatRelay {
    pub fn new(ctx: MiddlewareContext, config: ChatRelayConfig) -> Self {
        Self {
            cmd_tx: ctx.cmd_tx,
            source_service_id: config.source_service_id,
            source_room_id: config.source_room_id,
            dest_service_id: config.dest_service_id,
            dest_room_id: config.dest_room_id,
            prefix_tag: config.prefix_tag,
            http_client: reqwest::Client::new(),
            thumbnail_max_width: config.thumbnail_max_width,
            thumbnail_max_height: config.thumbnail_max_height,
            thumbnail_jpeg_quality: config.thumbnail_jpeg_quality,
        }
    }

    fn format_relayed_message(
        prefix_tag: &str,
        sender_id: &str,
        sender_display_name: Option<&str>,
        body: &str,
    ) -> String {
        let sender_display = sender_display_name.unwrap_or(sender_id);
        format!("[{}] {}: {}", prefix_tag, sender_display, body)
    }

    #[allow(clippy::too_many_arguments)]
    async fn send_text_fallback(
        cmd_tx: &Sender<Command>,
        dest_service_id: &ServiceId,
        dest_room_id: &str,
        prefix_tag: &str,
        sender_id: &str,
        sender_display_name: Option<&str>,
        body: &str,
        source_url: &str,
    ) {
        let caption =
            Self::format_relayed_message(prefix_tag, sender_id, sender_display_name, body);
        let text = format!("{caption} [image: {source_url}]");
        let command = Command::SendRoomMessage {
            service_id: dest_service_id.clone(),
            room_id: dest_room_id.to_string(),
            body: text.clone(),
            markdown_body: Some(text),
            response_tx: None,
        };
        if let Err(e) = cmd_tx.send(command).await {
            error!(error=%e, "failed to send text fallback for image relay");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn relay_image(
        http_client: reqwest::Client,
        cmd_tx: Sender<Command>,
        dest_service_id: ServiceId,
        dest_room_id: String,
        prefix_tag: String,
        sender_id: String,
        sender_display_name: Option<String>,
        body: String,
        source_url: String,
        image_data: Option<Arc<[u8]>>,
        thumbnail_max_width: u32,
        thumbnail_max_height: u32,
        thumbnail_jpeg_quality: u8,
    ) {
        // Use pre-fetched bytes when available (e.g. from an authenticated Matrix client).
        // Fall back to an HTTP fetch for services that don't pre-fetch.
        let raw_bytes: Arc<[u8]> = if let Some(data) = image_data {
            data
        } else {
            let response = match http_client.get(&source_url).send().await {
                Ok(r) => r,
                Err(e) => {
                    error!(error=%e, source_url=%source_url, "failed to fetch image for relay");
                    Self::send_text_fallback(
                        &cmd_tx,
                        &dest_service_id,
                        &dest_room_id,
                        &prefix_tag,
                        &sender_id,
                        sender_display_name.as_deref(),
                        &body,
                        &source_url,
                    )
                    .await;
                    return;
                }
            };
            match response.error_for_status() {
                Ok(r) => match r.bytes().await {
                    Ok(b) => Arc::from(b.as_ref()),
                    Err(e) => {
                        error!(error=%e, "failed to read image bytes");
                        Self::send_text_fallback(
                            &cmd_tx,
                            &dest_service_id,
                            &dest_room_id,
                            &prefix_tag,
                            &sender_id,
                            sender_display_name.as_deref(),
                            &body,
                            &source_url,
                        )
                        .await;
                        return;
                    }
                },
                Err(e) => {
                    error!(error=%e, source_url=%source_url, "image fetch returned error status");
                    Self::send_text_fallback(
                        &cmd_tx,
                        &dest_service_id,
                        &dest_room_id,
                        &prefix_tag,
                        &sender_id,
                        sender_display_name.as_deref(),
                        &body,
                        &source_url,
                    )
                    .await;
                    return;
                }
            }
        };

        info!(raw_bytes=%raw_bytes.len(), "processing image thumbnail for relay");

        // Decode, resize, and re-encode as JPEG on a blocking thread, a
        // bounded number at a time (see `THUMBNAIL_JOBS`).
        let thumbnail_result = {
            let _permit = THUMBNAIL_JOBS.acquire().await;
            tokio::task::spawn_blocking(move || {
                make_thumbnail(
                    &raw_bytes,
                    thumbnail_max_width,
                    thumbnail_max_height,
                    thumbnail_jpeg_quality,
                )
            })
            .await
        };

        let thumbnail_data = match thumbnail_result {
            Ok(Ok(data)) => {
                info!(thumbnail_bytes=%data.len(), "image thumbnail encoded");
                data
            }
            Ok(Err(e)) => {
                error!(error=%e, "failed to process image thumbnail");
                Self::send_text_fallback(
                    &cmd_tx,
                    &dest_service_id,
                    &dest_room_id,
                    &prefix_tag,
                    &sender_id,
                    sender_display_name.as_deref(),
                    &body,
                    &source_url,
                )
                .await;
                return;
            }
            Err(e) => {
                error!(error=%e, "image processing task panicked");
                Self::send_text_fallback(
                    &cmd_tx,
                    &dest_service_id,
                    &dest_room_id,
                    &prefix_tag,
                    &sender_id,
                    sender_display_name.as_deref(),
                    &body,
                    &source_url,
                )
                .await;
                return;
            }
        };

        let sender_display = sender_display_name.as_deref().unwrap_or(&sender_id);
        let caption = format!("[{prefix_tag}] {sender_display}:");

        let command = Command::SendRoomImage {
            service_id: dest_service_id,
            room_id: dest_room_id,
            caption,
            source_url,
            thumbnail_data,
            thumbnail_mimetype: "image/jpeg".to_string(),
        };

        if let Err(e) = cmd_tx.send(command).await {
            error!(error=%e, "failed to send SendRoomImage command");
        }
    }
}

#[async_trait]
impl Middleware for ChatRelay {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        info!(
            source_service=%self.source_service_id,
            source_room=?self.source_room_id,
            dest_service=%self.dest_service_id,
            dest_room=%self.dest_room_id,
            prefix_tag=%self.prefix_tag,
            "chat_relay middleware running..."
        );
        cancel.cancelled().await;
        info!("chat_relay middleware shutting down...");
        Ok(())
    }

    fn on_event(&self, event: &Event) -> Result<()> {
        // Filter: only handle events from source service
        if event.service_id.0 != self.source_service_id {
            return Ok(());
        }

        match &event.kind {
            EventKind::RoomMessage {
                room_id,
                body,
                sender_id,
                sender_display_name,
                is_self,
                ..
            } => {
                if let Some(ref expected_room) = self.source_room_id
                    && room_id != expected_room
                {
                    return Ok(());
                }
                if *is_self {
                    debug!("ignoring message from bot itself");
                    return Ok(());
                }

                let formatted_body = Self::format_relayed_message(
                    &self.prefix_tag,
                    sender_id,
                    sender_display_name.as_deref(),
                    body,
                );

                let cmd_tx = self.cmd_tx.clone();
                let dest_service_id = ServiceId(self.dest_service_id.clone());
                let dest_room_id = self.dest_room_id.clone();

                tokio::spawn(async move {
                    let command = Command::SendRoomMessage {
                        service_id: dest_service_id.clone(),
                        room_id: dest_room_id.clone(),
                        body: formatted_body.clone(),
                        markdown_body: Some(formatted_body),
                        response_tx: None,
                    };
                    if let Err(e) = cmd_tx.send(command).await {
                        error!(
                            dest_service=%dest_service_id.0,
                            dest_room=%dest_room_id,
                            error=%e,
                            "failed to send chat relay command"
                        );
                    }
                });
            }
            EventKind::RoomImage {
                room_id,
                sender_id,
                sender_display_name,
                is_self,
                body,
                source_url,
                image_data, // Option<Arc<[u8]>> — clone is one atomic increment
                ..
            } => {
                if let Some(ref expected_room) = self.source_room_id
                    && room_id != expected_room
                {
                    return Ok(());
                }
                if *is_self {
                    debug!("ignoring image from bot itself");
                    return Ok(());
                }

                let http_client = self.http_client.clone();
                let cmd_tx = self.cmd_tx.clone();
                let dest_service_id = ServiceId(self.dest_service_id.clone());
                let dest_room_id = self.dest_room_id.clone();
                let prefix_tag = self.prefix_tag.clone();
                let sender_id = sender_id.clone();
                let sender_display_name = sender_display_name.clone();
                let body = body.clone();
                let source_url = source_url.clone();
                let image_data = image_data.clone();
                let thumbnail_max_width = self.thumbnail_max_width;
                let thumbnail_max_height = self.thumbnail_max_height;
                let thumbnail_jpeg_quality = self.thumbnail_jpeg_quality;

                tokio::spawn(Self::relay_image(
                    http_client,
                    cmd_tx,
                    dest_service_id,
                    dest_room_id,
                    prefix_tag,
                    sender_id,
                    sender_display_name,
                    body,
                    source_url,
                    image_data,
                    thumbnail_max_width,
                    thumbnail_max_height,
                    thumbnail_jpeg_quality,
                ));
            }
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbImage};
    use std::io::Cursor;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        RgbImage::new(width, height)
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .expect("failed to encode test image");
        out
    }

    #[test]
    fn thumbnail_is_a_jpeg_within_the_requested_bounds() {
        let thumbnail = make_thumbnail(&png(800, 400), 200, 150, 60).expect("thumbnail");

        let decoded = image::load_from_memory(&thumbnail).expect("thumbnail should decode");
        assert_eq!(image::guess_format(&thumbnail).unwrap(), ImageFormat::Jpeg);
        assert!(decoded.width() <= 200 && decoded.height() <= 150);
        // Aspect ratio is preserved: 800x400 fits 200x150 as 200x100.
        assert_eq!((decoded.width(), decoded.height()), (200, 100));
    }

    #[test]
    fn oversized_dimensions_are_rejected() {
        // Tiny on the wire, but one side is past the decode limit.
        let tall = png(1, MAX_DECODE_DIMENSION + 1);
        assert!(tall.len() < 1024 * 1024);
        assert!(make_thumbnail(&tall, 200, 150, 60).is_err());

        let wide = png(MAX_DECODE_DIMENSION + 1, 1);
        assert!(make_thumbnail(&wide, 200, 150, 60).is_err());
    }

    #[test]
    fn largest_allowed_dimension_still_decodes() {
        assert!(make_thumbnail(&png(1, MAX_DECODE_DIMENSION), 200, 150, 60).is_ok());
    }

    #[test]
    fn garbage_input_is_an_error() {
        assert!(make_thumbnail(b"not an image", 200, 150, 60).is_err());
    }
}
