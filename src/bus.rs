use std::sync::Arc;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use crate::message::Message;
use crate::middleware::{Middleware, Verdict};

pub struct Bus {
    rx: Receiver<Message>,
    middleware: Vec<Arc<dyn Middleware>>,
}

impl Bus {
    pub fn new(rx: Receiver<Message>, middleware: Vec<Arc<dyn Middleware>>) -> Self {
        Self { rx, middleware }
    }

    pub async fn run(mut self, cancel: CancellationToken) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                maybe_msg = self.rx.recv() => {
                    let Some(mut msg) = maybe_msg else { break };
                    let mut dropped = false;
                    for mw in &self.middleware {
                        match mw.on_message(&mut msg).await? {
                            Verdict::Continue => {},
                            Verdict::Drop => { dropped = true; break; }
                        }
                    }
                    if dropped {
                        // todo
                    }
                }
            }
        }
        Ok(())
    }
}

// A small helper to make a channel pair available to services.
pub fn channel(cap: usize) -> (Sender<Message>, Receiver<Message>) {
    tokio::sync::mpsc::channel(cap)
}