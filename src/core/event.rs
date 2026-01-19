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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    ReactionAdded {
        room_id: String,
        event_id: String,
        target_event_id: String,
        key: String,
        sender_id: String,
        sender_display_name: Option<String>,
        is_self: bool,
    },
    ReactionRemoved {
        room_id: String,
        event_id: String,
        target_event_id: Option<String>,
        key: Option<String>,
        sender_id: String,
        is_self: bool,
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
            EventKind::ReactionAdded { room_id, key, sender_id, target_event_id, .. } => {
                write!(f, "[React+] {room_id}: {sender_id} reacted {key} to {target_event_id}")
            }
            EventKind::ReactionRemoved { room_id, sender_id, event_id, .. } => {
                write!(f, "[React-] {room_id}: {sender_id} removed reaction {event_id}")
            }
        }
    }
}
