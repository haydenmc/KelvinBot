use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::core::{
    bus::Command,
    event::{Event, EventKind},
    middleware::{Middleware, MiddlewareContext, Verdict},
    service::ServiceId,
};

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
        let caption = Self::format_relayed_message(prefix_tag, sender_id, sender_display_name, body);
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
        image_data: Option<Vec<u8>>,
        thumbnail_max_width: u32,
        thumbnail_max_height: u32,
        thumbnail_jpeg_quality: u8,
    ) {
        // Use pre-fetched bytes when available (e.g. from an authenticated Matrix client).
        // Fall back to an HTTP fetch for services that don't pre-fetch.
        let raw_bytes: Vec<u8> = if let Some(data) = image_data {
            data
        } else {
            let response = match http_client.get(&source_url).send().await {
                Ok(r) => r,
                Err(e) => {
                    error!(error=%e, source_url=%source_url, "failed to fetch image for relay");
                    Self::send_text_fallback(
                        &cmd_tx, &dest_service_id, &dest_room_id, &prefix_tag,
                        &sender_id, sender_display_name.as_deref(), &body, &source_url,
                    ).await;
                    return;
                }
            };
            match response.error_for_status() {
                Ok(r) => match r.bytes().await {
                    Ok(b) => b.to_vec(),
                    Err(e) => {
                        error!(error=%e, "failed to read image bytes");
                        Self::send_text_fallback(
                            &cmd_tx, &dest_service_id, &dest_room_id, &prefix_tag,
                            &sender_id, sender_display_name.as_deref(), &body, &source_url,
                        ).await;
                        return;
                    }
                },
                Err(e) => {
                    error!(error=%e, source_url=%source_url, "image fetch returned error status");
                    Self::send_text_fallback(
                        &cmd_tx, &dest_service_id, &dest_room_id, &prefix_tag,
                        &sender_id, sender_display_name.as_deref(), &body, &source_url,
                    ).await;
                    return;
                }
            }
        };

        info!(raw_bytes=%raw_bytes.len(), "processing image thumbnail for relay");

        // Decode, resize, and re-encode as JPEG on a blocking thread.
        // Normalise to Rgb8 before encoding — JpegEncoder can silently produce
        // empty output when handed an Rgb8 DynamicImage via write_with_encoder,
        // so we call write_image directly with an explicit color type instead.
        let thumbnail_result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            use image::{ImageEncoder, ImageReader, codecs::jpeg::JpegEncoder};
            use std::io::Cursor;
            let img = ImageReader::new(Cursor::new(&raw_bytes))
                .with_guessed_format()?
                .decode()?;
            let thumb = img.thumbnail(thumbnail_max_width, thumbnail_max_height).into_rgb8();
            let mut out = Vec::new();
            JpegEncoder::new_with_quality(&mut out, thumbnail_jpeg_quality).write_image(
                thumb.as_raw(),
                thumb.width(),
                thumb.height(),
                image::ExtendedColorType::Rgb8,
            )?;
            Ok(out)
        }).await;

        let thumbnail_data = match thumbnail_result {
            Ok(Ok(data)) => {
                info!(thumbnail_bytes=%data.len(), "image thumbnail encoded");
                data
            }
            Ok(Err(e)) => {
                error!(error=%e, "failed to process image thumbnail");
                Self::send_text_fallback(
                    &cmd_tx, &dest_service_id, &dest_room_id, &prefix_tag,
                    &sender_id, sender_display_name.as_deref(), &body, &source_url,
                ).await;
                return;
            }
            Err(e) => {
                error!(error=%e, "image processing task panicked");
                Self::send_text_fallback(
                    &cmd_tx, &dest_service_id, &dest_room_id, &prefix_tag,
                    &sender_id, sender_display_name.as_deref(), &body, &source_url,
                ).await;
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

    fn on_event(&self, event: &Event) -> Result<Verdict> {
        // Filter: only handle events from source service
        if event.service_id.0 != self.source_service_id {
            return Ok(Verdict::Continue);
        }

        match &event.kind {
            EventKind::RoomMessage { room_id, body, sender_id, sender_display_name, is_self, .. } => {
                if let Some(ref expected_room) = self.source_room_id
                    && room_id != expected_room
                {
                    return Ok(Verdict::Continue);
                }
                if *is_self {
                    debug!("ignoring message from bot itself");
                    return Ok(Verdict::Continue);
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
                image_data,
                ..
            } => {
                if let Some(ref expected_room) = self.source_room_id
                    && room_id != expected_room
                {
                    return Ok(Verdict::Continue);
                }
                if *is_self {
                    debug!("ignoring image from bot itself");
                    return Ok(Verdict::Continue);
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

        Ok(Verdict::Continue)
    }
}
