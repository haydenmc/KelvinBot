use async_trait::async_trait;
use crate::message::Message;

#[derive(Debug, Clone, Copy)]
pub enum Verdict { Continue, Drop }

#[async_trait]
pub trait Middleware: Send + Sync {
    fn name(&self) -> &str;
    async fn on_message(&self, _msg: &mut Message) -> anyhow::Result<Verdict> {
        Ok(Verdict::Continue)
    }
}