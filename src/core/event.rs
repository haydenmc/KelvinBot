use serde::{Deserialize, Serialize};

use crate::core::service::ServiceId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Address {
    pub service_id: ServiceId,
    pub room_id: String,
    pub user_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Event {
    Message { from: Address, body: String },
}
