use std::{collections::HashMap, fmt, sync::Arc};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::{
    core::{
        config::{Config, ServiceKind},
        event::Event,
    },
    services::dummy::DummyService,
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
    fn id(&self) -> ServiceId;
    async fn run(&self, cancel: CancellationToken) -> Result<()>;
}

/// Instantiates a map of Services based on given config
pub fn instantiate_services_from_config(
    config: &Config,
    evt_tx: &Sender<Event>,
) -> HashMap<ServiceId, Arc<dyn Service>> {
    let mut services: HashMap<ServiceId, Arc<dyn Service>> = HashMap::new();
    for (id, scfg) in &config.services {
        let service_id = ServiceId(id.clone());
        match scfg.kind {
            ServiceKind::Dummy => {
                let svc = Arc::new(DummyService {
                    id: service_id.clone(),
                    interval_ms: scfg.interval_ms.unwrap_or(1000),
                    evt_tx: evt_tx.clone(),
                });
                services.insert(service_id, svc);
            }
            _ => error!(id=%id, "unknown service kind, skipping"),
        }
    }
    services
}
