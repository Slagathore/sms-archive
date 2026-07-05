//! Shared types used across analytics modules.
//!
//! These are deliberately small and serializable. Database row mapping happens
//! at the orchestrator layer — these types are the in-memory currency that
//! every algorithm speaks.

use serde::{Deserialize, Serialize};

/// Which side of the conversation a message came from, from the app user's
/// ("me's") perspective. Computed from `messages.message_direction`:
/// outgoing (2) → `Me`, incoming (1) → `Them`. Messages with unknown direction
/// are filtered out before reaching the segmenter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum Participant {
    Me,
    Them,
}

impl Participant {
    /// Returns the integer encoding used by the `conversations` table:
    /// 1 for `Me`, 2 for `Them`. Matches `MessageDirection::Outgoing`/`Incoming`
    /// values for symmetry, even though the meaning differs (we're storing the
    /// participant identity, not the direction relative to a fixed self).
    pub fn as_i32(self) -> i32 {
        match self {
            Self::Me => 1,
            Self::Them => 2,
        }
    }

    pub fn flip(self) -> Self {
        match self {
            Self::Me => Self::Them,
            Self::Them => Self::Me,
        }
    }
}

/// Lightweight per-message data the segmenter and downstream modules need.
///
/// Crucially this is NOT the full `sms_types::Message`. We don't load message
/// bodies, attachments, or thread IDs into segmentation memory — for a contact
/// with millions of messages, the savings are real (and we can always re-fetch
/// the full row by `db_rowid` when we actually need the body).
#[derive(Debug, Clone, Copy)]
pub struct MessageRef {
    /// The `messages.rowid` (insertion order, monotonic). Used as the
    /// tie-breaker for messages with identical timestamps.
    pub db_rowid: i64,
    /// Unix epoch milliseconds. SMS Backup & Restore stores ms-precision dates;
    /// our DB preserves them as-is.
    pub timestamp_ms: i64,
    pub sender: Participant,
    /// True if the message had at least one attachment. Cheap signal that
    /// downstream (aggregator, scoring) cares about; segmenter ignores it.
    pub has_media: bool,
}

/// Configuration for segmentation, sourced from `analytics_meta` (with
/// optional per-contact overrides applied by the caller).
#[derive(Debug, Clone, Copy)]
pub struct SegmentationConfig {
    /// Gap between consecutive messages exceeding this duration starts a new
    /// conversation. Stored in `analytics_meta.conversation_timeout_secs`.
    pub conversation_timeout_ms: i64,
    /// `total_message_count >= this` flips `is_big_moment_static = true`.
    /// Stored in `analytics_meta.big_moment_threshold_static`. Default 20.
    pub big_moment_static_threshold: u32,
    /// Conversations whose `total_message_count` is at or above this percentile
    /// (relative to all of THIS pair's conversations) flip
    /// `is_big_moment_dynamic = true`. Stored as
    /// `analytics_meta.big_moment_threshold_dynamic_pct`. Default 90 (top 10%).
    pub big_moment_dynamic_percentile: u8,
    /// Floor for the dynamic threshold. If the percentile-based cutoff comes
    /// out below this, we use this number instead. Prevents micro-conversation
    /// pairs from getting "big moments" at counts of 2-3.
    pub big_moment_dynamic_floor: u32,
    /// Reconnect tier 1 (the lowest tier) gap threshold in milliseconds.
    /// Default: 24h.
    pub reconnect_tier1_ms: i64,
    /// Tier 2 gap. Default: 7d.
    pub reconnect_tier2_ms: i64,
    /// Tier 3 gap. Default: 30d.
    pub reconnect_tier3_ms: i64,
    /// Tier 4 multiplier — applied to the pair's median inter-conversation gap.
    /// A new convo whose preceding silence exceeds `multiplier × pair_median_gap`
    /// is tier 4 ("significant reconnect"). Default: 3.0.
    pub reconnect_tier4_multiplier: f64,
}

impl Default for SegmentationConfig {
    fn default() -> Self {
        Self {
            conversation_timeout_ms: 4 * 60 * 60 * 1000, // 4 hours
            big_moment_static_threshold: 20,
            big_moment_dynamic_percentile: 90,
            big_moment_dynamic_floor: 10,
            reconnect_tier1_ms: 24 * 60 * 60 * 1000, // 24h
            reconnect_tier2_ms: 7 * 24 * 60 * 60 * 1000, // 7d
            reconnect_tier3_ms: 30 * 24 * 60 * 60 * 1000, // 30d
            reconnect_tier4_multiplier: 3.0,
        }
    }
}

/// Result of segmenting one contact's message stream into conversations.
///
/// Field semantics map directly to columns in the `conversations` table; see
/// `crates/db/migrations/0014_analytics_tables.sql` for the schema this fills.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub contact_id: String,
    /// Unix epoch ms of the first message in the conversation.
    pub start_time_ms: i64,
    /// Unix epoch ms of the last message in the conversation.
    pub end_time_ms: i64,
    pub started_by: Participant,
    /// Whoever sent the LAST message in the conversation. For a missed convo
    /// (only one party spoke), this equals `started_by`.
    pub final_reply_by: Participant,
    pub my_message_count: u32,
    pub their_message_count: u32,
    pub total_message_count: u32,
    /// Whoever sent strictly more messages. Tie-breaks to `Me` for symmetry.
    pub major_contributor: Participant,
    /// True if exactly one side ever spoke in this conversation. The other
    /// side is the `missed_by` party (they never replied).
    pub is_missed: bool,
    pub missed_by: Option<Participant>,
    /// Set during the segmenter pass. `total_message_count >= static threshold`.
    pub is_big_moment_static: bool,
    /// Set in the post-pass against this pair's percentile cutoff.
    pub is_big_moment_dynamic: bool,
    /// 0 = not a reconnect; 1-4 are the tiers (24h / 7d / 30d / 3× pair-median).
    pub reconnect_tier: u8,
    /// Sum of message points. Populated later by the scoring module; left
    /// at 0.0 by the segmenter.
    pub points: f64,
}
