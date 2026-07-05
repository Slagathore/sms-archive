//! Per-message points scoring.
//!
//! Each message is assigned a points value reflecting the engagement it
//! represents. Per-side totals + per-day rollups + per-conversation totals
//! all derive from the same single pass.
//!
//! # Weight semantics
//!
//! - `text_message`: base, awarded once per message.
//! - `per_word_log`: multiplied by `ln(words + 1)`. Log scale is intentional
//!   per the Q6g design decision — a 50-word message is "substantial" but
//!   a 500-word message isn't 10× substantial.
//! - `emoji`: per emoji extracted from the body.
//! - `image`/`video`/`audio`/`gif`: per attachment of that category.
//! - `question`/`apology`/`encouragement`/`link`: per message that has the
//!   property (one shot, not per occurrence).
//! - `started_convo`: per message that opens a new conversation.
//! - `rapid_response`: per message that's a rapid response to a sender flip
//!   within the same conversation.

use crate::aggregator::AggregatorMessage;
use crate::emoji::extract_emojis;
use crate::media::{classify_media, MediaCategory};
use crate::patterns::{contains_link, is_apology, is_encouragement, is_question};
use crate::types::{Conversation, Participant, SegmentationConfig};
use chrono::{Datelike, FixedOffset, TimeZone};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PointWeights {
    pub text_message: f64,
    pub per_word_log: f64,
    pub emoji: f64,
    pub question: f64,
    pub image: f64,
    pub video: f64,
    pub audio: f64,
    pub gif: f64,
    pub link: f64,
    pub started_convo: f64,
    pub rapid_response: f64,
    pub encouragement: f64,
    pub apology: f64,
}

impl Default for PointWeights {
    fn default() -> Self {
        Self {
            text_message: 1.0,
            per_word_log: 0.1,
            emoji: 0.2,
            question: 0.5,
            image: 3.0,
            video: 5.0,
            audio: 4.0,
            gif: 2.0,
            link: 2.0,
            started_convo: 5.0,
            rapid_response: 2.0,
            encouragement: 3.0,
            apology: 2.0,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScoringOutput {
    /// Per-conversation total points, index-aligned with the input
    /// `conversations` slice.
    pub conversation_points: Vec<f64>,
    /// Map of `YYYY-MM-DD` (in user's local TZ) to `(my_points, their_points)`.
    /// Aligns with the daily buckets the aggregator produces.
    pub daily_points: HashMap<String, (f64, f64)>,
    pub total_my_points: f64,
    pub total_their_points: f64,
    /// Per-message points, index-aligned with the input `messages` slice.
    /// Useful for debugging / future "why is this convo's score so high?" UIs.
    pub per_message_points: Vec<f64>,
}

/// Run scoring over a contact's message stream.
///
/// `messages` and `conversations` MUST both be sorted by ascending start
/// time. We walk them in lockstep — for each message, we know exactly which
/// conversation it's in.
///
/// `seg_config` is reused so we use the same convo-boundary timeout the
/// segmenter used; that keeps "is this a convo starter?" consistent.
pub fn compute_scoring(
    messages: &[AggregatorMessage],
    conversations: &[Conversation],
    weights: &PointWeights,
    rapid_threshold_ms: i64,
    seg_config: &SegmentationConfig,
    tz_offset_secs: i32,
) -> ScoringOutput {
    let mut out = ScoringOutput {
        conversation_points: vec![0.0; conversations.len()],
        per_message_points: Vec::with_capacity(messages.len()),
        ..ScoringOutput::default()
    };
    if messages.is_empty() {
        return out;
    }

    let tz = FixedOffset::east_opt(tz_offset_secs)
        .unwrap_or_else(|| FixedOffset::east_opt(0).expect("UTC always valid"));

    let mut conv_idx: usize = 0;
    let mut prev_conv_idx: Option<usize> = None;
    let mut prev_msg: Option<&AggregatorMessage> = None;

    for msg in messages.iter() {
        // Advance conv_idx until we find the conversation containing this msg.
        while conv_idx < conversations.len()
            && msg.timestamp_ms > conversations[conv_idx].end_time_ms
        {
            conv_idx += 1;
        }
        if conv_idx >= conversations.len() {
            // Message past the last conversation — shouldn't happen if
            // segmentation was run on the same data, but stay defensive.
            out.per_message_points.push(0.0);
            continue;
        }
        if msg.timestamp_ms < conversations[conv_idx].start_time_ms {
            // Message falls in a gap between conversations. Either a bug or
            // a filtered-out unknown-direction message — skip scoring.
            out.per_message_points.push(0.0);
            continue;
        }

        let is_starter = prev_conv_idx != Some(conv_idx);

        // Rapid response: only within the same conversation, only when the
        // sender flipped, and only when the gap is ≤ rapid threshold.
        let is_rapid_response = match (prev_msg, prev_conv_idx) {
            (Some(p), Some(prev_idx)) if prev_idx == conv_idx => {
                p.sender != msg.sender
                    && (msg.timestamp_ms - p.timestamp_ms) <= rapid_threshold_ms
                    // Also require the gap to be within the seg timeout, just
                    // to bullet-proof against weird configs:
                    && (msg.timestamp_ms - p.timestamp_ms) <= seg_config.conversation_timeout_ms
            }
            _ => false,
        };

        let pts = compute_message_points(msg, weights, is_starter, is_rapid_response);
        out.per_message_points.push(pts);
        out.conversation_points[conv_idx] += pts;

        match msg.sender {
            Participant::Me => out.total_my_points += pts,
            Participant::Them => out.total_their_points += pts,
        }

        // Daily bucket — local-tz date string.
        if let chrono::LocalResult::Single(local) = tz.timestamp_millis_opt(msg.timestamp_ms) {
            let day = format!(
                "{:04}-{:02}-{:02}",
                local.year(),
                local.month(),
                local.day()
            );
            let entry = out.daily_points.entry(day).or_insert((0.0, 0.0));
            match msg.sender {
                Participant::Me => entry.0 += pts,
                Participant::Them => entry.1 += pts,
            }
        }

        prev_conv_idx = Some(conv_idx);
        prev_msg = Some(msg);
    }

    out
}

/// Score one message in isolation. Public so unit tests and future
/// debug-view UIs can replay scoring on a single message.
pub fn compute_message_points(
    msg: &AggregatorMessage,
    weights: &PointWeights,
    is_starter: bool,
    is_rapid_response: bool,
) -> f64 {
    let mut pts = weights.text_message;

    // Word count → log-scale bonus.
    let word_count = msg.body.split_whitespace().count();
    pts += weights.per_word_log * ((word_count as f64) + 1.0).ln();

    // Emoji bonus (per emoji).
    let emoji_count: u32 = extract_emojis(&msg.body).values().sum();
    pts += weights.emoji * emoji_count as f64;

    // Per-message flags.
    if is_question(&msg.body) {
        pts += weights.question;
    }
    if is_apology(&msg.body) {
        pts += weights.apology;
    }
    if is_encouragement(&msg.body) {
        pts += weights.encouragement;
    }
    if contains_link(&msg.body) {
        pts += weights.link;
    }

    // Media (per attachment).
    for mime in &msg.mime_types {
        match classify_media(mime) {
            MediaCategory::Image => pts += weights.image,
            MediaCategory::Video => pts += weights.video,
            MediaCategory::Audio => pts += weights.audio,
            MediaCategory::Gif => pts += weights.gif,
            MediaCategory::Other => {}
        }
    }

    // Conversation-level flags.
    if is_starter {
        pts += weights.started_convo;
    }
    if is_rapid_response {
        pts += weights.rapid_response;
    }

    pts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weights() -> PointWeights {
        PointWeights::default()
    }

    fn am(
        rowid: i64,
        ts_ms: i64,
        sender: Participant,
        body: &str,
        mimes: &[&str],
    ) -> AggregatorMessage {
        AggregatorMessage {
            db_rowid: rowid,
            timestamp_ms: ts_ms,
            sender,
            body: body.to_string(),
            mime_types: mimes.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn one_convo_for(messages: &[AggregatorMessage]) -> Vec<Conversation> {
        if messages.is_empty() {
            return Vec::new();
        }
        let first = messages.first().unwrap();
        let last = messages.last().unwrap();
        // Determine started_by, final_reply_by, etc. from messages directly so
        // tests don't care about full segmenter behavior.
        let mut my = 0u32;
        let mut them = 0u32;
        for m in messages {
            match m.sender {
                Participant::Me => my += 1,
                Participant::Them => them += 1,
            }
        }
        let total = my + them;
        let major = if my >= them {
            Participant::Me
        } else {
            Participant::Them
        };
        vec![Conversation {
            contact_id: "c1".to_string(),
            start_time_ms: first.timestamp_ms,
            end_time_ms: last.timestamp_ms,
            started_by: first.sender,
            final_reply_by: last.sender,
            my_message_count: my,
            their_message_count: them,
            total_message_count: total,
            major_contributor: major,
            is_missed: my == 0 || them == 0,
            missed_by: None,
            is_big_moment_static: total >= 20,
            is_big_moment_dynamic: false,
            reconnect_tier: 0,
            points: 0.0,
        }]
    }

    #[test]
    fn empty_input_yields_zero_output() {
        let out = compute_scoring(
            &[],
            &[],
            &weights(),
            60_000,
            &SegmentationConfig::default(),
            0,
        );
        assert_eq!(out.total_my_points, 0.0);
        assert_eq!(out.total_their_points, 0.0);
        assert!(out.conversation_points.is_empty());
        assert!(out.daily_points.is_empty());
    }

    #[test]
    fn single_text_message_gets_base_plus_starter() {
        let messages = vec![am(1, 1_000_000, Participant::Me, "hello world", &[])];
        let convs = one_convo_for(&messages);
        let out = compute_scoring(
            &messages,
            &convs,
            &weights(),
            60_000,
            &SegmentationConfig::default(),
            0,
        );
        // Expected: text_message(1.0) + per_word_log * ln(2+1=3) + started_convo(5.0)
        let expected = 1.0 + 0.1 * (3.0_f64).ln() + 5.0;
        assert!(
            (out.per_message_points[0] - expected).abs() < 1e-9,
            "got {}, expected ~{}",
            out.per_message_points[0],
            expected
        );
        assert_eq!(out.total_my_points, out.per_message_points[0]);
        assert_eq!(out.total_their_points, 0.0);
        assert_eq!(out.conversation_points[0], out.per_message_points[0]);
    }

    #[test]
    fn rapid_response_bonus_only_when_within_threshold_and_flipped() {
        let messages = vec![
            am(1, 1_000_000, Participant::Me, "ping", &[]),
            am(2, 1_030_000, Participant::Them, "pong", &[]), // 30s flip → rapid
            am(3, 1_120_000, Participant::Me, "ok", &[]),     // 90s flip → NOT rapid (>60s)
        ];
        let convs = one_convo_for(&messages);
        let out = compute_scoring(
            &messages,
            &convs,
            &weights(),
            60_000,
            &SegmentationConfig::default(),
            0,
        );
        // Their msg gets rapid_response bonus; mine does not.
        // Their pts = 1.0 + 0.1*ln(1+1=2) + 2.0(rapid)
        let their_expected = 1.0 + 0.1 * (2.0_f64).ln() + 2.0;
        assert!(
            (out.per_message_points[1] - their_expected).abs() < 1e-9,
            "their msg pts: got {}, expected ~{}",
            out.per_message_points[1],
            their_expected
        );
        // My second msg pts = 1.0 + 0.1*ln(2) + 0 (not rapid)
        let mine_expected = 1.0 + 0.1 * (2.0_f64).ln();
        assert!(
            (out.per_message_points[2] - mine_expected).abs() < 1e-9,
            "mine msg pts: got {}, expected ~{}",
            out.per_message_points[2],
            mine_expected
        );
    }

    #[test]
    fn double_message_does_not_get_rapid_bonus() {
        let messages = vec![
            am(1, 1_000_000, Participant::Me, "first", &[]),
            am(2, 1_010_000, Participant::Me, "second", &[]), // same sender, no rapid
        ];
        let convs = one_convo_for(&messages);
        let out = compute_scoring(
            &messages,
            &convs,
            &weights(),
            60_000,
            &SegmentationConfig::default(),
            0,
        );
        // Second msg should NOT have rapid bonus.
        let expected_second = 1.0 + 0.1 * (2.0_f64).ln(); // 1 word, no rapid
        assert!(
            (out.per_message_points[1] - expected_second).abs() < 1e-9,
            "got {}, expected ~{}",
            out.per_message_points[1],
            expected_second
        );
    }

    #[test]
    fn media_and_pattern_flags_accumulate() {
        // Body designed to fire every flag: trailing '?' for question,
        // "sorry" for apology, "you got this" for encouragement,
        // an http link, an emoji, plus image+video attachments.
        let body = "sorry but you got this 😂 https://example.com - are you sure?";
        let messages = vec![am(
            1,
            1_000_000,
            Participant::Me,
            body,
            &["image/jpeg", "video/mp4"],
        )];
        let convs = one_convo_for(&messages);
        let out = compute_scoring(
            &messages,
            &convs,
            &weights(),
            60_000,
            &SegmentationConfig::default(),
            0,
        );
        // 11 whitespace-delimited tokens.
        let words = body.split_whitespace().count() as f64;
        let emoji_count = 1.0; // 😂
        let expected = 1.0
            + 0.1 * (words + 1.0).ln()
            + 0.2 * emoji_count
            + 0.5    // question (trailing '?')
            + 2.0    // apology ("sorry")
            + 3.0    // encouragement ("you got this")
            + 2.0    // link (https://...)
            + 3.0    // image
            + 5.0    // video
            + 5.0; // started_convo
        assert!(
            (out.per_message_points[0] - expected).abs() < 1e-9,
            "got {}, expected ~{}",
            out.per_message_points[0],
            expected
        );
    }

    #[test]
    fn per_conversation_points_aligned_with_conv_index() {
        // Two conversations, one message each. With 4h timeout (default), a
        // gap of 5 hours forces a new conversation.
        let messages = vec![
            am(1, 0, Participant::Me, "first conv", &[]),
            am(2, 5 * 60 * 60 * 1000, Participant::Them, "second conv", &[]),
        ];
        // Build two conversations explicitly.
        let convs = vec![
            Conversation {
                contact_id: "c1".to_string(),
                start_time_ms: 0,
                end_time_ms: 0,
                started_by: Participant::Me,
                final_reply_by: Participant::Me,
                my_message_count: 1,
                their_message_count: 0,
                total_message_count: 1,
                major_contributor: Participant::Me,
                is_missed: true,
                missed_by: Some(Participant::Them),
                is_big_moment_static: false,
                is_big_moment_dynamic: false,
                reconnect_tier: 0,
                points: 0.0,
            },
            Conversation {
                contact_id: "c1".to_string(),
                start_time_ms: 5 * 60 * 60 * 1000,
                end_time_ms: 5 * 60 * 60 * 1000,
                started_by: Participant::Them,
                final_reply_by: Participant::Them,
                my_message_count: 0,
                their_message_count: 1,
                total_message_count: 1,
                major_contributor: Participant::Them,
                is_missed: true,
                missed_by: Some(Participant::Me),
                is_big_moment_static: false,
                is_big_moment_dynamic: false,
                reconnect_tier: 0,
                points: 0.0,
            },
        ];
        let out = compute_scoring(
            &messages,
            &convs,
            &weights(),
            60_000,
            &SegmentationConfig::default(),
            0,
        );
        // Each conversation gets exactly its one message's points.
        assert_eq!(out.conversation_points.len(), 2);
        assert!(out.conversation_points[0] > 0.0);
        assert!(out.conversation_points[1] > 0.0);
        assert_eq!(out.conversation_points[0], out.per_message_points[0]);
        assert_eq!(out.conversation_points[1], out.per_message_points[1]);
    }

    #[test]
    fn daily_points_split_by_local_date_and_side() {
        // Two messages on the same UTC day but different local days.
        // Aligns with aggregator's daily-bucketing test.
        let ts1: i64 = 1_711_058_400_000; // 2024-03-21T22:00:00Z → 2024-03-22 04:00 in UTC+6
        let ts2: i64 = 1_711_033_200_000; // 2024-03-21T15:00:00Z → 2024-03-21 21:00 in UTC+6
        let mut messages = vec![
            am(1, ts2, Participant::Them, "earlier hi", &[]),
            am(2, ts1, Participant::Me, "later wave", &[]),
        ];
        // Sort by timestamp because compute_scoring assumes ascending order.
        messages.sort_by_key(|m| m.timestamp_ms);
        let convs = one_convo_for(&messages);
        let out = compute_scoring(
            &messages,
            &convs,
            &weights(),
            60_000,
            &SegmentationConfig::default(),
            6 * 3600, // UTC+6
        );
        // Two distinct local days.
        assert_eq!(out.daily_points.len(), 2);
        let day_21 = out.daily_points.get("2024-03-21").expect("missing 03-21");
        let day_22 = out.daily_points.get("2024-03-22").expect("missing 03-22");
        // 03-21 has Their msg → second tuple field non-zero.
        assert!(
            day_21.0 == 0.0 && day_21.1 > 0.0,
            "03-21 expected (0, >0), got {:?}",
            day_21
        );
        // 03-22 has My msg → first tuple field non-zero.
        assert!(
            day_22.0 > 0.0 && day_22.1 == 0.0,
            "03-22 expected (>0, 0), got {:?}",
            day_22
        );
    }

    #[test]
    fn log_scale_words_caps_extreme_messages() {
        // A 50-word message vs a 500-word message. Linear scoring would give
        // 10× the per-word component to the latter; log gives roughly 2×.
        let m_short = am(1, 1_000_000, Participant::Me, &"word ".repeat(50), &[]);
        let m_long = am(2, 1_000_000, Participant::Me, &"word ".repeat(500), &[]);
        let p_short = compute_message_points(&m_short, &weights(), false, false);
        let p_long = compute_message_points(&m_long, &weights(), false, false);
        // Subtract the base text_message so we're comparing only word components.
        let words_only_short = p_short - 1.0;
        let words_only_long = p_long - 1.0;
        // long / short should be much less than 10× (closer to ln(501)/ln(51) ≈ 1.58)
        let ratio = words_only_long / words_only_short;
        assert!(
            ratio < 3.0,
            "log-scale word ratio should be <3×, got {}",
            ratio
        );
        assert!(
            ratio > 1.0,
            "longer message should still score higher, got ratio {}",
            ratio
        );
    }
}
