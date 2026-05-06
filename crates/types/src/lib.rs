//! Core data types for SMS archive

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub message_id: Option<String>,
    pub dedupe_hash: Option<[u8; 32]>,
    pub timestamp: i64,
    pub address: String,
    pub body: String,
    pub body_searchable: String,
    pub message_type: MessageType,
    pub direction: MessageDirection,
    pub thread_id: Option<String>,
    pub attachments: Vec<AttachmentRef>,
    /// Display name from the source XML's `contact_name` attribute (SMS Backup & Restore).
    /// May be `None` for MMS rows or messages imported from sources that don't carry the field.
    /// Defaulting via `#[serde(default)]` so older serialized blobs deserialize cleanly.
    #[serde(default)]
    pub contact_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(i32)]
pub enum MessageType {
    Sms = 1,
    Mms = 2,
    Rcs = 3,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[repr(i32)]
pub enum MessageDirection {
    Unknown = 0,
    Incoming = 1,
    Outgoing = 2,
}

impl MessageDirection {
    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::Incoming,
            2 => Self::Outgoing,
            _ => Self::Unknown,
        }
    }

    // #todo: add richer direction states (draft/failed/queued) once the DB schema supports them.
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Incoming => "Received",
            Self::Outgoing => "Sent",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub id: Uuid,
    pub mime_type: String,
    pub file_path: String,
    pub file_hash: [u8; 32],
    pub thumbnail_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrip_serde() {
        let msg = Message {
            id: Uuid::new_v4(),
            message_id: Some("test123".into()),
            dedupe_hash: Some([1u8; 32]),
            timestamp: 1234567890,
            address: "+15551234567".into(),
            body: "Hello world".into(),
            body_searchable: "hello world".into(),
            message_type: MessageType::Sms,
            direction: MessageDirection::Outgoing,
            thread_id: Some("thread-1".into()),
            attachments: Vec::new(),
            contact_name: Some("Test Contact".into()),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, decoded.id);
        assert_eq!(msg.message_type, decoded.message_type);
        assert_eq!(msg.dedupe_hash, decoded.dedupe_hash);
    }
}
