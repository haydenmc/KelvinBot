use async_trait::async_trait;
use kelvin_bot::core::config::{Config, ServiceCfg, ServiceKind};
use kelvin_bot::core::event::{Event, EventKind};
use kelvin_bot::core::service::{Service, ServiceId};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

/// Creates a test configuration with a dummy service for testing
#[allow(dead_code)] // Suppress spurious warning - some compilation units don't include this code.
pub fn create_test_config() -> Config {
    Config {
        services: {
            let mut services = HashMap::new();
            services.insert(
                "test_dummy".to_string(),
                ServiceCfg {
                    kind: ServiceKind::Dummy { interval_ms: Some(100) },
                    middleware: None,
                },
            );
            services
        },
        middlewares: HashMap::new(),
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
    }
}

/// Creates a test configuration with multiple dummy services
#[allow(dead_code)] // Suppress spurious warning - some compilation units don't include this code.
pub fn create_multi_service_config() -> Config {
    let mut services = HashMap::new();
    services.insert(
        "dummy1".to_string(),
        ServiceCfg { kind: ServiceKind::Dummy { interval_ms: Some(100) }, middleware: None },
    );
    services.insert(
        "dummy2".to_string(),
        ServiceCfg { kind: ServiceKind::Dummy { interval_ms: Some(200) }, middleware: None },
    );

    Config {
        services,
        middlewares: HashMap::new(),
        data_directory: TempDir::new().unwrap().path().to_path_buf(),
    }
}

/// A controllable mock service for testing that can send specific events on command
#[allow(dead_code)] // Used by integration tests, not unit tests
#[derive(Debug)]
pub struct MockService {
    pub id: ServiceId,
    pub evt_tx: mpsc::Sender<Event>,
    /// Commands to send events (send event count to this channel)
    pub command_rx: Arc<Mutex<mpsc::Receiver<usize>>>,
}

impl MockService {
    /// Create a new mock service with a command channel for controlling event sending
    #[allow(dead_code)] // Used by integration tests, not unit tests
    pub fn new(id: ServiceId, evt_tx: mpsc::Sender<Event>) -> (Self, mpsc::Sender<usize>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(10);

        let service = MockService { id, evt_tx, command_rx: Arc::new(Mutex::new(cmd_rx)) };

        (service, cmd_tx)
    }
}

#[async_trait]
impl Service for MockService {
    async fn run(&self, cancel: CancellationToken) -> anyhow::Result<()> {
        let mut command_rx = self.command_rx.lock().await;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    break;
                }
                maybe_count = command_rx.recv() => {
                    let Some(count) = maybe_count else { break };

                    // Send the requested number of events
                    for i in 0..count {
                        let event = Event {
                            service_id: self.id.clone(),
                            kind: EventKind::RoomMessage {
                                room_id: format!("room_{}", i),
                                body: format!("test message {}", i),
                            },
                        };

                        if (self.evt_tx.send(event).await).is_err() {
                            // Channel closed, service should stop
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_command(&self, command: kelvin_bot::core::bus::Command) -> anyhow::Result<()> {
        // For mock service, just log the command - tests can verify behavior through other means
        tracing::debug!(?command, "mock service received command");
        Ok(())
    }
}
