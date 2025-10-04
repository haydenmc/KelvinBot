use std::{collections::HashMap, fmt, sync::Arc};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::{
    core::{
        bus::Command,
        config::{Config, ServiceKind},
        event::Event,
    },
    services::{
        dummy::DummyService,
        matrix::{MatrixService, MatrixUserId},
    },
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServiceId(pub String);

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the inner string
        write!(f, "{}", self.0)
    }
}

#[async_trait::async_trait]
pub trait Service: Send + Sync {
    async fn run(&self, cancel: CancellationToken) -> Result<()>;
    async fn handle_command(&self, command: Command) -> Result<()>;
}

/// Instantiates a map of Services based on given config
pub async fn instantiate_services_from_config(
    config: &Config,
    evt_tx: &Sender<Event>,
) -> Result<HashMap<ServiceId, Arc<dyn Service>>> {
    let mut services: HashMap<ServiceId, Arc<dyn Service>> = HashMap::new();
    for (id, scfg) in &config.services {
        let service_id = ServiceId(id.clone());
        match &scfg.kind {
            ServiceKind::Dummy { interval_ms } => {
                let svc = Arc::new(DummyService {
                    id: service_id.clone(),
                    interval_ms: interval_ms.unwrap_or(1000),
                    evt_tx: evt_tx.clone(),
                });
                services.insert(service_id, svc);
            }
            ServiceKind::Matrix {
                homeserver_url,
                user_id,
                password,
                device_id,
                db_passphrase,
                recovery_passphrase,
            } => {
                match MatrixService::create(
                    service_id.clone(),
                    homeserver_url.clone(),
                    MatrixUserId(user_id.clone()),
                    password.clone(),
                    device_id.clone(),
                    evt_tx.clone(),
                    config.data_directory.clone(),
                    db_passphrase.clone(),
                    recovery_passphrase.clone(),
                )
                .await
                {
                    Ok(svc) => {
                        services.insert(service_id, Arc::new(svc));
                    }
                    Err(e) => {
                        error!(id=%id, error=%e, "could not instantiate matrix service");
                    }
                }
            }
            _ => error!(id=%id, "unknown service kind, skipping"),
        }
    }
    Ok(services)
}
