use async_trait::async_trait;
use crate::message::Message;
use crate::middleware::Verdict;
use crate::Middleware;

pub struct Logger;

#[async_trait]
impl Middleware for Logger {
    fn name(&self) -> &str { "logger" }

    async fn on_message(&self, msg: &mut Message) -> anyhow::Result<Verdict> {
        tracing::info!(service=%msg.from.service, room=%msg.from.room_id, text=%msg.body, "inbound");
        Ok(Verdict::Continue)
    }
}