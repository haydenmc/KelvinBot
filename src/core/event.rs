use std::fmt;

use serde::{Deserialize, Serialize};

use crate::core::service::ServiceId;

#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    pub service_id: ServiceId,
    pub kind: EventKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum EventKind {
    DirectMessage { user_id: String, body: String, is_local_user: bool },
    RoomMessage { room_id: String, body: String, is_local_user: bool },
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}]", &self.service_id)?;
        match &self.kind {
            EventKind::DirectMessage { user_id, body, .. } => {
                write!(f, "[DM] {user_id}: {body}")
            }
            EventKind::RoomMessage { room_id, body, .. } => {
                write!(f, "[RM] {room_id}: {body}")
            }
        }
    }
}
