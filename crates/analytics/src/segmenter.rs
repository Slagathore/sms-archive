//! Conversation segmentation.
//!
//! The foundation everything else builds on: takes a chronologically-sorted
//! stream of messages for a single contact and splits it into discrete
//! conversations using gap-based segmentation.
//!
//! # Algorithm
//!
//! 1. **First pass (streaming):** Walk messages in timestamp order. Whenever
//!    the gap between consecutive messages exceeds `timeout_ms`, finalize the
//!    in-progress conversation and start a new one. Track:
//!    - who spoke first (`started_by`),
//!    - who spoke last (`final_reply_by`),
//!    - per-side message counts,
//!    - whether only one side ever spoke (`is_missed`),
//!    - whether the static big-moment threshold was met.
//!
//! 2. **Second pass (post-segmentation):** Compute the pair-specific percentile
//!    cutoff for `is_big_moment_dynamic`, then walk conversations once more to
//!    set both that flag AND `reconnect_tier` (which depends on inter-convo
//!    gaps and the pair's median gap).
//!
//! # Determinism
//!
//! For messages with identical timestamps, the caller is expected to have
//! pre-sorted by `(timestamp_ms ASC, db_rowid ASC)`. SQLite's natural ordering
//! satisfies this. We assert it in debug builds; in release, we trust the
//! caller.
//!
//! # Memory
//!
//! O(N) in the message count for the first pass (we yield one `Conversation`
//! per finalized segment). The second pass holds the message-count vector
//! for the percentile computation — that's O(C) where C = number of
//! conversations, far smaller than N.

use crate::types::{Conversation, MessageRef, Participant, SegmentationConfig};

/// Segment a single contact's message stream into conversations.
///
/// `messages` MUST be pre-sorted by `(timestamp_ms ASC, db_rowid ASC)`. The
/// caller (orchestrator) guarantees this through SQL `ORDER BY`.
///
/// Returns conversations in chronological order with `is_big_moment_dynamic`
/// and `reconnect_tier` already populated. `points` is left at 0.0 — the
/// scoring module fills that later.
pub fn segment_conversations(
    contact_id: &str,
    messages: &[MessageRef],
    config: &SegmentationConfig,
) -> Vec<Conversation> {
    if messages.is_empty() {
        return Vec::new();
    }

    debug_assert!(
        is_strictly_sorted(messages),
        "segment_conversations called with unsorted messages — caller must ORDER BY timestamp ASC, rowid ASC"
    );

    // First pass: gap-based splitting.
    let mut conversations: Vec<Conversation> = Vec::new();
    let mut builder: Option<ConversationBuilder> = None;
    let mut prev_message_time: Option<i64> = None;

    for msg in messages {
        let should_split = match prev_message_time {
            Some(prev) => msg.timestamp_ms - prev > config.conversation_timeout_ms,
            None => false,
        };

        if should_split {
            // Gap exceeded timeout. Close the current conversation and open a new one.
            if let Some(b) = builder.take() {
                conversations.push(b.finalize(contact_id, config));
            }
        }

        // Two-arm dispatch ensures `new` is called exactly once per conversation
        // and `append` for every subsequent message — never both for the same row.
        match builder.as_mut() {
            None => builder = Some(ConversationBuilder::new(*msg)),
            Some(b) => b.append(*msg),
        }
        prev_message_time = Some(msg.timestamp_ms);
    }

    if let Some(b) = builder.take() {
        conversations.push(b.finalize(contact_id, config));
    }

    // Second pass: dynamic big-moment + reconnect tier.
    enrich_pair_metrics(&mut conversations, config);

    conversations
}

/// Internal accumulator used during the first pass.
struct ConversationBuilder {
    start_time_ms: i64,
    end_time_ms: i64,
    started_by: Participant,
    final_reply_by: Participant,
    my_message_count: u32,
    their_message_count: u32,
}

impl ConversationBuilder {
    fn new(first: MessageRef) -> Self {
        Self {
            start_time_ms: first.timestamp_ms,
            end_time_ms: first.timestamp_ms,
            started_by: first.sender,
            final_reply_by: first.sender,
            my_message_count: if matches!(first.sender, Participant::Me) {
                1
            } else {
                0
            },
            their_message_count: if matches!(first.sender, Participant::Them) {
                1
            } else {
                0
            },
        }
    }

    /// Add a subsequent message to the in-progress conversation. Caller
    /// guarantees this is NEVER invoked for the message that constructed the
    /// builder (i.e. messages 2..N only). The two-arm dispatch in
    /// `segment_conversations` enforces that invariant.
    fn append(&mut self, msg: MessageRef) {
        self.end_time_ms = msg.timestamp_ms;
        self.final_reply_by = msg.sender;
        match msg.sender {
            Participant::Me => self.my_message_count += 1,
            Participant::Them => self.their_message_count += 1,
        }
    }

    fn total(&self) -> u32 {
        self.my_message_count + self.their_message_count
    }

    fn finalize(self, contact_id: &str, config: &SegmentationConfig) -> Conversation {
        let total = self.total();
        let major_contributor = if self.my_message_count >= self.their_message_count {
            // Tie-breaks to `Me` deliberately. The Sankey major-contributor field
            // is symmetric with how we frame the user — ties are visually rendered
            // as "you" for consistency on the dashboard.
            Participant::Me
        } else {
            Participant::Them
        };
        let is_missed = self.my_message_count == 0 || self.their_message_count == 0;
        let missed_by = if is_missed {
            // The one party that didn't speak is the one who "missed" the convo.
            Some(self.started_by.flip())
        } else {
            None
        };

        Conversation {
            contact_id: contact_id.to_string(),
            start_time_ms: self.start_time_ms,
            end_time_ms: self.end_time_ms,
            started_by: self.started_by,
            final_reply_by: self.final_reply_by,
            my_message_count: self.my_message_count,
            their_message_count: self.their_message_count,
            total_message_count: total,
            major_contributor,
            is_missed,
            missed_by,
            is_big_moment_static: total >= config.big_moment_static_threshold,
            is_big_moment_dynamic: false, // populated in second pass
            reconnect_tier: 0,            // populated in second pass
            points: 0.0,                  // populated by scoring later
        }
    }
}

/// Second-pass enrichment: dynamic big-moment + reconnect tier.
///
/// Both metrics depend on aggregate properties of the full conversation list
/// for this pair, so they can't be computed during the streaming first pass.
fn enrich_pair_metrics(conversations: &mut [Conversation], config: &SegmentationConfig) {
    if conversations.is_empty() {
        return;
    }

    // ---------- Dynamic big moment ----------
    // We want the percentile cutoff to be the value at or above which a
    // conversation counts as "big" for THIS pair. Sort message counts ascending
    // and pick the value at index `floor(N * pct/100)`.
    let mut counts: Vec<u32> = conversations
        .iter()
        .map(|c| c.total_message_count)
        .collect();
    counts.sort_unstable();
    let pct = config.big_moment_dynamic_percentile.min(100) as f64 / 100.0;
    let idx = ((counts.len() as f64) * pct).floor() as usize;
    let idx = idx.min(counts.len().saturating_sub(1));
    let dynamic_threshold = counts[idx].max(config.big_moment_dynamic_floor);

    // ---------- Reconnect tier 4 (depends on pair-median gap) ----------
    // Compute median gap between consecutive conversations' boundaries
    // (gap = next.start - prev.end). Tier 4 fires when the actual preceding
    // gap exceeds `multiplier × median_gap`.
    let mut inter_convo_gaps: Vec<i64> = Vec::with_capacity(conversations.len().saturating_sub(1));
    for window in conversations.windows(2) {
        let gap = window[1].start_time_ms - window[0].end_time_ms;
        if gap > 0 {
            inter_convo_gaps.push(gap);
        }
    }
    inter_convo_gaps.sort_unstable();
    let median_gap_ms = if inter_convo_gaps.is_empty() {
        // Fewer than 2 conversations means tier 4 can never fire.
        i64::MAX
    } else {
        inter_convo_gaps[inter_convo_gaps.len() / 2]
    };
    let tier4_threshold_ms =
        ((median_gap_ms as f64) * config.reconnect_tier4_multiplier).min(i64::MAX as f64) as i64;

    // ---------- Walk conversations once more to set both flags ----------
    let mut prev_end_ms: Option<i64> = None;
    for convo in conversations.iter_mut() {
        // Dynamic big moment: count >= dynamic threshold (already floor-clamped).
        convo.is_big_moment_dynamic = convo.total_message_count >= dynamic_threshold;

        // Reconnect tier: classify the silence preceding this conversation.
        // The very first conversation has no preceding silence, so tier = 0.
        if let Some(prev_end) = prev_end_ms {
            let gap = convo.start_time_ms - prev_end;
            convo.reconnect_tier = classify_reconnect_tier(gap, config, tier4_threshold_ms);
        }
        prev_end_ms = Some(convo.end_time_ms);
    }
}

/// Map a single gap in ms to a tier 0-4. Higher tiers take precedence — i.e.
/// a 60-day gap is tier 3 (≥30d) regardless of whether it ALSO meets the tier 4
/// (3× median) condition. Tier 4 fires only when ≥3× median AND that yields a
/// stricter threshold than tier 3.
fn classify_reconnect_tier(
    gap_ms: i64,
    config: &SegmentationConfig,
    tier4_threshold_ms: i64,
) -> u8 {
    // Tier 4 is the "significant reconnect" — a gap that's BOTH at least 3×
    // the pair's median AND at least at the tier 3 floor. We require the tier 3
    // floor to prevent tier 4 from firing on a chatty pair where median gap is
    // 5 minutes and a 16-minute gap technically exceeds 3×.
    if gap_ms >= tier4_threshold_ms && gap_ms >= config.reconnect_tier3_ms {
        return 4;
    }
    if gap_ms >= config.reconnect_tier3_ms {
        return 3;
    }
    if gap_ms >= config.reconnect_tier2_ms {
        return 2;
    }
    if gap_ms >= config.reconnect_tier1_ms {
        return 1;
    }
    0
}

/// Debug-only sanity check on caller-provided ordering.
fn is_strictly_sorted(messages: &[MessageRef]) -> bool {
    messages
        .windows(2)
        .all(|w| (w[0].timestamp_ms, w[0].db_rowid) <= (w[1].timestamp_ms, w[1].db_rowid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SegmentationConfig {
        SegmentationConfig::default()
    }

    fn msg(rowid: i64, ts_ms: i64, sender: Participant) -> MessageRef {
        MessageRef {
            db_rowid: rowid,
            timestamp_ms: ts_ms,
            sender,
            has_media: false,
        }
    }

    #[test]
    fn empty_input_yields_no_conversations() {
        let out = segment_conversations("c1", &[], &cfg());
        assert!(out.is_empty());
    }

    #[test]
    fn single_message_yields_one_missed_conversation() {
        let messages = vec![msg(1, 1_000, Participant::Me)];
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!(c.total_message_count, 1);
        assert_eq!(c.my_message_count, 1);
        assert_eq!(c.their_message_count, 0);
        assert_eq!(c.started_by, Participant::Me);
        assert_eq!(c.final_reply_by, Participant::Me);
        assert!(c.is_missed);
        assert_eq!(c.missed_by, Some(Participant::Them));
    }

    #[test]
    fn two_messages_same_sender_one_convo_still_missed() {
        let messages = vec![
            msg(1, 1_000, Participant::Me),
            msg(2, 2_000, Participant::Me),
        ];
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!(c.my_message_count, 2);
        assert_eq!(c.their_message_count, 0);
        assert!(c.is_missed);
        assert_eq!(c.missed_by, Some(Participant::Them));
    }

    #[test]
    fn gap_just_under_timeout_stays_one_convo() {
        let timeout_ms = cfg().conversation_timeout_ms;
        let messages = vec![
            msg(1, 0, Participant::Me),
            msg(2, timeout_ms, Participant::Them), // gap == timeout: NOT split
        ];
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].total_message_count, 2);
    }

    #[test]
    fn gap_just_over_timeout_splits_into_two() {
        let timeout_ms = cfg().conversation_timeout_ms;
        let messages = vec![
            msg(1, 0, Participant::Me),
            msg(2, timeout_ms + 1, Participant::Them), // gap > timeout: split
        ];
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 2);
        assert!(out[0].is_missed);
        assert!(out[1].is_missed);
    }

    #[test]
    fn back_and_forth_one_conversation_not_missed() {
        let messages = vec![
            msg(1, 0, Participant::Them),
            msg(2, 60_000, Participant::Me),
            msg(3, 120_000, Participant::Them),
            msg(4, 180_000, Participant::Me),
        ];
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!(c.total_message_count, 4);
        assert_eq!(c.started_by, Participant::Them);
        assert_eq!(c.final_reply_by, Participant::Me);
        assert!(!c.is_missed);
        assert_eq!(c.missed_by, None);
        // Tied counts → Me wins by deliberate tie-break.
        assert_eq!(c.major_contributor, Participant::Me);
    }

    #[test]
    fn major_contributor_reflects_actual_majority() {
        let messages = vec![
            msg(1, 0, Participant::Them),
            msg(2, 60_000, Participant::Them),
            msg(3, 120_000, Participant::Them),
            msg(4, 180_000, Participant::Me),
        ];
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out[0].major_contributor, Participant::Them);
    }

    #[test]
    fn big_moment_static_fires_at_threshold() {
        let mut messages = Vec::new();
        // 20 messages alternating, well within timeout.
        for i in 0..20 {
            let sender = if i % 2 == 0 {
                Participant::Me
            } else {
                Participant::Them
            };
            messages.push(msg(i, i * 60_000, sender));
        }
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 1);
        assert!(
            out[0].is_big_moment_static,
            "expected big moment at exactly threshold"
        );
    }

    #[test]
    fn big_moment_static_does_not_fire_below_threshold() {
        let mut messages = Vec::new();
        for i in 0..19 {
            let sender = if i % 2 == 0 {
                Participant::Me
            } else {
                Participant::Them
            };
            messages.push(msg(i, i * 60_000, sender));
        }
        let out = segment_conversations("c1", &messages, &cfg());
        assert!(!out[0].is_big_moment_static);
    }

    #[test]
    fn big_moment_dynamic_uses_pair_specific_top_decile() {
        // Build 10 conversations with sizes: nine at 5 messages, one at 50.
        // p90 of [5,5,5,5,5,5,5,5,5,50] (sorted) at idx floor(10*0.9)=9 is 50.
        // Threshold should be max(50, floor=10) = 50. Only the giant convo fires.
        let mut messages = Vec::new();
        let timeout = cfg().conversation_timeout_ms;
        let mut t = 0i64;
        let mut rowid = 0i64;
        for convo_idx in 0..10 {
            let n = if convo_idx == 9 { 50 } else { 5 };
            for i in 0..n {
                let sender = if i % 2 == 0 {
                    Participant::Me
                } else {
                    Participant::Them
                };
                rowid += 1;
                messages.push(msg(rowid, t, sender));
                t += 60_000; // 1 min between within a convo
            }
            // Force a split between conversations.
            t += timeout + 1;
        }
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 10);
        let big_dynamic_count = out.iter().filter(|c| c.is_big_moment_dynamic).count();
        assert_eq!(
            big_dynamic_count, 1,
            "exactly one convo should be dynamic-big"
        );
        assert!(out[9].is_big_moment_dynamic);
    }

    #[test]
    fn big_moment_dynamic_floor_prevents_micro_promotion() {
        // Three tiny convos of sizes 1, 2, 3. p90 cutoff would be 3, but the
        // floor (default 10) clamps the threshold up. Nothing should fire.
        let timeout = cfg().conversation_timeout_ms;
        let mut messages = Vec::new();
        let mut rowid = 0i64;
        let mut t = 0i64;
        for n in [1, 2, 3] {
            for i in 0..n {
                let sender = if i % 2 == 0 {
                    Participant::Me
                } else {
                    Participant::Them
                };
                rowid += 1;
                messages.push(msg(rowid, t, sender));
                t += 60_000;
            }
            t += timeout + 1;
        }
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|c| !c.is_big_moment_dynamic));
    }

    #[test]
    fn reconnect_tiers_classify_correctly() {
        // Build single-message conversations with hand-picked gaps. For THIS pair,
        // the median inter-convo gap will end up at 7d, so 3× median = 21d.
        //
        // Gaps (between successive convo END and next START):
        //   12h  → tier 0 (below tier 1 floor of 24h)
        //   24h  → tier 1
        //   7d   → tier 2
        //   30d  → tier 4! (≥30d AND ≥21d=3×median; tier 4 wins as the more specific tier)
        //
        // The 30d → tier 4 case is intentional: for a pair whose typical silence
        // is 7 days, a 30-day gap is not just a "monthly reconnect" — it's
        // significantly out of pattern.
        let mut messages = Vec::new();
        let mut rowid = 0i64;
        let timestamps = [
            0,
            12 * 60 * 60 * 1000,                      // +12h
            36 * 60 * 60 * 1000,                      // +24h after prev
            (36 + 7 * 24) * 60 * 60 * 1000,           // +7d
            (36 + 7 * 24 + 30 * 24) * 60 * 60 * 1000, // +30d
        ];
        for ts in timestamps {
            rowid += 1;
            messages.push(msg(rowid, ts, Participant::Me));
        }
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 5);
        assert_eq!(
            out[0].reconnect_tier, 0,
            "first convo always tier 0 (no preceding gap)"
        );
        assert_eq!(out[1].reconnect_tier, 0, "12h < 24h → tier 0");
        assert_eq!(out[2].reconnect_tier, 1, "24h exactly → tier 1");
        assert_eq!(out[3].reconnect_tier, 2, "7d → tier 2");
        assert_eq!(
            out[4].reconnect_tier, 4,
            "30d gap on a 7d-median pair → tier 4"
        );
    }

    #[test]
    fn tier_3_fires_when_pair_median_too_large_for_tier_4() {
        // For a pair whose median gap is ALREADY large, a 30-day gap shouldn't
        // qualify as tier 4 (because 3× median > 30d). This test demonstrates
        // tier 3 firing where tier 4 doesn't.
        //
        // We engineer a sequence where the median inter-convo gap is ~25 days,
        // making 3× median = 75d. Then we insert a 30d gap which is ≥30d (tier 3)
        // but < 75d (NOT tier 4).
        let mut messages = Vec::new();
        let mut rowid = 0i64;
        let day_ms: i64 = 24 * 60 * 60 * 1000;

        // Five "chatty" conversations with 25-day gaps between them.
        let mut t = 0i64;
        for _ in 0..5 {
            rowid += 1;
            messages.push(msg(rowid, t, Participant::Me));
            t += 25 * day_ms;
        }
        // One more convo at exactly +30d after the previous (rather than +25d).
        rowid += 1;
        messages.push(msg(rowid, t + 30 * day_ms - 25 * day_ms, Participant::Me));

        let out = segment_conversations("c1", &messages, &cfg());
        // Last convo's preceding gap is 30d. Median is 25d, so 3× = 75d.
        // 30d >= 30d (tier 3) but 30d < 75d (NOT tier 4) → tier 3.
        assert_eq!(out.last().unwrap().reconnect_tier, 3);
    }

    #[test]
    fn same_sender_same_timestamp_distinct_rowids_both_count() {
        // Regression test: the original implementation used `get_or_insert_with`
        // followed by `append`, which double-handled the first message and
        // silently dropped a same-sender same-ts second message. The two-arm
        // dispatch fixes that.
        let messages = vec![
            msg(1, 5_000, Participant::Me),
            msg(2, 5_000, Participant::Me),
        ];
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!(
            c.total_message_count, 2,
            "both same-sender same-ts messages must count"
        );
        assert_eq!(c.my_message_count, 2);
        assert_eq!(c.their_message_count, 0);
        assert!(c.is_missed);
    }

    #[test]
    fn tied_timestamps_with_distinct_rowids_are_accepted() {
        // Pre-sorted by (ts, rowid). Two messages with identical ts both belong
        // to the same conversation. Order should follow rowid.
        let messages = vec![
            msg(10, 5_000, Participant::Me),
            msg(11, 5_000, Participant::Them), // same ts, later rowid
        ];
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 1);
        let c = &out[0];
        assert_eq!(c.total_message_count, 2);
        assert_eq!(c.started_by, Participant::Me); // earlier rowid wins for "started by"
        assert_eq!(c.final_reply_by, Participant::Them); // later rowid is "final"
        assert!(!c.is_missed);
    }

    #[test]
    fn long_continuous_conversation_does_not_split() {
        // Messages every 30 min for 10 hours. Each gap is far below the 4h timeout.
        let mut messages = Vec::new();
        let half_hour_ms = 30 * 60 * 1000;
        for i in 0..20 {
            let sender = if i % 2 == 0 {
                Participant::Me
            } else {
                Participant::Them
            };
            messages.push(msg(i, i * half_hour_ms, sender));
        }
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(
            out.len(),
            1,
            "10-hour run with 30-min gaps must stay one convo"
        );
        assert!(out[0].is_big_moment_static);
    }

    #[test]
    fn missed_by_is_other_party_for_one_sided_convo() {
        let messages = vec![
            msg(1, 0, Participant::Them),
            msg(2, 1_000, Participant::Them),
            msg(3, 2_000, Participant::Them),
        ];
        let out = segment_conversations("c1", &messages, &cfg());
        assert_eq!(out.len(), 1);
        assert!(out[0].is_missed);
        assert_eq!(out[0].missed_by, Some(Participant::Me));
    }

    #[test]
    fn debug_assert_catches_unsorted_input() {
        // Only enabled in debug builds. We test the predicate directly so the
        // suite passes in both modes.
        let bad = vec![
            msg(1, 1000, Participant::Me),
            msg(2, 500, Participant::Them), // earlier timestamp than predecessor
        ];
        assert!(!is_strictly_sorted(&bad));

        let good = vec![
            msg(1, 500, Participant::Me),
            msg(2, 500, Participant::Them), // tied ts, later rowid → ok
            msg(3, 1000, Participant::Me),
        ];
        assert!(is_strictly_sorted(&good));
    }
}
