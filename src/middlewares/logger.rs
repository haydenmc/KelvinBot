use crate::core::{
    event::{Event, EventKind},
    middleware::{Middleware, Verdict},
};
use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub struct Logger;

#[async_trait]
impl Middleware for Logger {
    async fn run(&self, cancel: CancellationToken) -> Result<()> {
        tracing::info!("logger running...");
        cancel.cancelled().await;
        tracing::info!("logger shutting down...");
        Ok(())
    }

    fn on_event(&self, evt: &Event) -> anyhow::Result<Verdict> {
        match &evt.kind {
            EventKind::UserListUpdate { users } => {
                let usernames: Vec<String> = users
                    .iter()
                    .map(|u| {
                        if u.is_self {
                            format!("{} (self)", u.username)
                        } else {
                            u.username.clone()
                        }
                    })
                    .collect();
                tracing::info!(
                    service_id=%evt.service_id,
                    user_count=%users.len(),
                    users=?usernames,
                    "user list update"
                );
            }
            _ => {
                tracing::info!(event=%evt, "inbound event");
            }
        }
        Ok(Verdict::Continue)
    }
}
