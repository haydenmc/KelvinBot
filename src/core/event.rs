use std::fmt;

use serde::{Deserialize, Serialize};

use crate::core::service::ServiceId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub is_active: bool,
    pub is_self: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Event {
    pub service_id: ServiceId,
    pub kind: EventKind,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum EventKind {
    DirectMessage {
        user_id: String,
        body: String,
        is_local_user: bool,
        sender_id: String,
        sender_display_name: Option<String>,
        is_self: bool,
    },
    RoomMessage {
        room_id: String,
        body: String,
        is_local_user: bool,
        sender_id: String,
        sender_display_name: Option<String>,
        is_self: bool,
    },
    UserListUpdate {
        users: Vec<User>,
    },
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
            EventKind::UserListUpdate { users } => {
                write!(f, "[UserList] {} users", users.len())
            }
        }
    }
}
