use std::fmt;

use anyhow::{Result, bail};
use matrix_sdk::{
    Client,
    config::SyncSettings,
    ruma::{events::room::message::SyncRoomMessageEvent, user_id},
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use url::Url;

use crate::core::{
    event::Event,
    service::{self, Service, ServiceId},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MatrixUserId(pub String);

impl fmt::Display for MatrixUserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the inner string
        write!(f, "{}", self.0)
    }
}

pub struct MatrixService {
    id: ServiceId,
    homeserver_url: Url,
    user_id: MatrixUserId,
    password: SecretString,
    evt_tx: tokio::sync::mpsc::Sender<Event>,
}

impl MatrixService {
    pub fn new(
        id: ServiceId,
        homeserver_url: Url,
        user_id: MatrixUserId,
        password: SecretString,
        evt_tx: tokio::sync::mpsc::Sender<Event>,
    ) -> Self {
        Self { id, homeserver_url, user_id, password, evt_tx }
    }
}

#[async_trait::async_trait]
impl Service for MatrixService {
    fn id(&self) -> service::ServiceId {
        self.id.clone()
    }
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        info!(service_id=%self.id, homeserver_url=%self.homeserver_url, user_id=%self.user_id,
            "starting matrix service");
        let client = Client::builder().homeserver_url(self.homeserver_url.clone()).build().await?;
        match client
            .matrix_auth()
            .login_username(self.user_id.to_string(), self.password.expose_secret())
            .send()
            .await
        {
            Ok(_) => {
                info!("login successful");
            }
            Err(e) => {
                error!(error=%e, "login error");
                bail!("login error")
            }
        }
        client.add_event_handler(|ev: SyncRoomMessageEvent| async move {
            info!(event=?ev, "received a message");
        });
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!(service=%self.id, "shutdown requested");
                    break;
                }
                result = client.sync(SyncSettings::default()) => {
                    warn!("matrix sync returned");
                }
            }
        }
        Ok(())
    }
}
