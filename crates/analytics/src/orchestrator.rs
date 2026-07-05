//! End-to-end analytics computation for a single contact.
//!
//! This module is the only one in the crate that touches the database.
//! Everything else is a pure function over in-memory data.
//!
//! # Pipeline
//!
//! ```text
//!   load_messages          (SELECT m, a JOIN contact_addresses)
//!         │
//!         ▼
//!   segment_conversations  → Vec<Conversation>
//!         │
//!         ▼
//!   compute_aggregates     → ContactAggregates + daily + hourly
//!         │
//!         ▼
//!   compute_response_metrics → ResponseMetrics
//!         │
//!         ▼
//!   build_conversation_flow → SankeyData
//!         │
//!         ▼
//!   compute_scoring        → per-message + per-convo + per-day points
//!         │  (merge into conversations and daily buckets)
//!         ▼
//!   compute_rating         → 0-100 + 7 components + confidence band
//!         │
//!         ▼
//!   compute_insights       → Vec<Insight>
//!         │
//!         ▼
//!   persist (single transaction)
//!         │
//!         ▼
//!   update contact_analytics_status (clear stale, record elapsed)
//! ```

use crate::aggregator::{compute_aggregates, AggregatesOutput, AggregatorMessage, DailyBucket};
use crate::flow::{build_conversation_flow, SankeyData};
use crate::focus::{compute_focus, FocusOutput};
use crate::inside_jokes::{detect_inside_jokes, InsideJoke, InsideJokesConfig};
use crate::insights::{compute_insights, EngineConfig, Insight, InsightCtx};
use crate::rating::{compute_rating, RatingInput, RatingOutput, RatingThresholds, RatingWeights};
use crate::responses::{
    compute_response_metrics, ResponseConfig, ResponseHistogramJson, ResponseMessage,
    ResponseMetrics,
};
use crate::scoring::{compute_scoring, PointWeights, ScoringOutput};
use crate::segmenter::segment_conversations;
use crate::sentiment::{compute_sentiment_timeline, SentimentTimeline};
use crate::topics::{build_phrase_counts, compute_topics, TopicPhrase, TopicsConfig};
use crate::types::{Conversation, Participant, SegmentationConfig};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sms_errors::{AppError, Result};
use sms_types::MessageDirection;
use std::time::Instant;

/// Bundle of all configuration the orchestrator needs. Built by the caller
/// (UI or CLI) by reading `analytics_meta` and applying any per-contact
/// overrides from `analytics_overrides`.
#[derive(Debug, Clone, Copy)]
pub struct OrchestratorConfig {
    pub segmentation: SegmentationConfig,
    pub response: ResponseConfig,
    pub weights: PointWeights,
    pub rating_weights: RatingWeights,
    pub rating_thresholds: RatingThresholds,
    pub engine: EngineConfig,
    /// Used by both responses (rapid threshold) and scoring (rapid bonus
    /// gating). Same number — keeps "rapid" defined consistently.
    pub rapid_response_threshold_ms: i64,
    /// Top-N emojis to keep per side in `contact_analytics.my_top_emojis` /
    /// `their_top_emojis`. 5 matches the dashboard.
    pub top_emoji_count: usize,
    /// User's local UTC offset, applied for daily / hourly bucketing and
    /// overnight tagging. The UI should set this from system TZ on first run.
    pub tz_offset_secs: i32,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            segmentation: SegmentationConfig::default(),
            response: ResponseConfig::default(),
            weights: PointWeights::default(),
            rating_weights: RatingWeights::default(),
            rating_thresholds: RatingThresholds::default(),
            engine: EngineConfig::default(),
            rapid_response_threshold_ms: 60 * 1000,
            top_emoji_count: 5,
            tz_offset_secs: 0,
        }
    }
}

/// Lightweight summary of one orchestrator run. The caller uses this for
/// progress UI ("Processed 123,456 messages in 2.3s") and logging.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrchestratorOutput {
    pub message_count: usize,
    pub conversation_count: usize,
    pub elapsed_ms: u128,
    /// `false` when the contact had no analyzable messages — the analytics
    /// tables were still cleaned and the status row marked computed, but no
    /// content rows were inserted.
    pub had_data: bool,
}

/// Run the full analytics pipeline for one contact and persist the results.
///
/// All writes happen in a single transaction. If any step fails, the
/// existing analytics for this contact are preserved (the transaction
/// rolls back).
pub fn compute_for_contact(
    conn: &Connection,
    contact_id: &str,
    config: &OrchestratorConfig,
) -> Result<OrchestratorOutput> {
    let start = Instant::now();

    // 1. Load all messages + attachments for this contact.
    let messages = load_messages_for_contact(conn, contact_id)?;
    let response_messages: Vec<ResponseMessage> = messages
        .iter()
        .map(|m| ResponseMessage {
            timestamp_ms: m.timestamp_ms,
            sender: m.sender,
        })
        .collect();

    if messages.is_empty() {
        // Still wipe and mark computed so stale status is cleared.
        clear_contact_analytics(conn, contact_id)?;
        update_status(conn, contact_id, start.elapsed().as_millis(), None)?;
        return Ok(OrchestratorOutput {
            message_count: 0,
            conversation_count: 0,
            elapsed_ms: start.elapsed().as_millis(),
            had_data: false,
        });
    }

    // 2. Segment.
    let mut conversations = segment_conversations(
        contact_id,
        &slim_message_refs(&messages),
        &config.segmentation,
    );

    // 3. Aggregate counts + daily/hourly.
    let aggregates = compute_aggregates(&messages, config.tz_offset_secs, config.top_emoji_count);

    // 4. Response metrics.
    let responses = compute_response_metrics(
        &response_messages,
        &config.response,
        &config.segmentation,
        config.tz_offset_secs,
    );

    // 5. Conversation flow (Sankey). Computed in v1, rendered in v2.
    let flow = build_conversation_flow(&conversations);

    // 5b. Direction-of-conversation focus. Pulls "other contact" first names
    // from the contacts table so mentions of e.g. mutual friends show up in
    // the "others" bucket of the donut.
    let other_names = load_other_contact_first_names(conn, contact_id).unwrap_or_default();
    let focus = compute_focus(&messages, &other_names);

    // 5c. Sentiment timeline (lexicon-based, per-day, per-side).
    let sentiment = compute_sentiment_timeline(&messages, config.tz_offset_secs);

    // 5d. Inside jokes — recurring 2-/3-word phrases this pair leans on.
    let inside_jokes = detect_inside_jokes(&messages, &InsideJokesConfig::default());

    // 5e. Topics — distinctive phrases via TF-IDF against a global background.
    // Background corpus = a sample of messages from OTHER contacts. Computed
    // here rather than in compute_topics so the rest of the module stays DB-free.
    let (bg_phrase_counts, bg_message_count) =
        load_background_phrase_counts(conn, contact_id).unwrap_or_default();
    let topics = compute_topics(
        &messages,
        &bg_phrase_counts,
        bg_message_count,
        &TopicsConfig::default(),
    );

    // 6. Scoring — merges into conversations and daily buckets.
    let scoring = compute_scoring(
        &messages,
        &conversations,
        &config.weights,
        config.rapid_response_threshold_ms,
        &config.segmentation,
        config.tz_offset_secs,
    );
    for (i, conv) in conversations.iter_mut().enumerate() {
        conv.points = scoring.conversation_points.get(i).copied().unwrap_or(0.0);
    }

    // 7. Pair-level metrics derived from conversations + responses.
    let pair = derive_pair_metrics(&conversations, &responses, &scoring);

    // 8. Rating + insights need everything else as input.
    let rating_input = RatingInput {
        contact: &aggregates.contact,
        responses: &responses,
        daily: &aggregates.daily,
        conversations_started_by_me: pair.convos_started_by_me,
        conversations_started_by_them: pair.convos_started_by_them,
        first_message_ms: messages.first().map(|m| m.timestamp_ms).unwrap_or(0),
        last_message_ms: messages.last().map(|m| m.timestamp_ms).unwrap_or(0),
        avg_convo_length_msgs: pair.avg_convo_length_msgs,
    };
    let rating = compute_rating(
        &rating_input,
        &config.rating_weights,
        &config.rating_thresholds,
    );

    let insight_ctx = InsightCtx {
        contact: &aggregates.contact,
        responses: &responses,
        conversations_started_by_me: pair.convos_started_by_me,
        conversations_started_by_them: pair.convos_started_by_them,
        conversations_closed_by_me: pair.convos_closed_by_me,
        conversations_closed_by_them: pair.convos_closed_by_them,
        my_convos_missed: pair.my_convos_missed,
        their_convos_missed: pair.their_convos_missed,
        reconnect_count_total: pair.reconnect_count_t1
            + pair.reconnect_count_t2
            + pair.reconnect_count_t3
            + pair.reconnect_count_t4,
        total_conversations: conversations.len() as u32,
    };
    let insights = compute_insights(&insight_ctx, &config.engine);

    // 9. Persist (transactional).
    let computed_at_unix_secs = chrono::Utc::now().timestamp();
    persist_all(
        conn,
        contact_id,
        &conversations,
        &aggregates,
        &responses,
        &flow,
        &focus,
        &sentiment,
        &inside_jokes,
        &topics,
        &scoring,
        &rating,
        &insights,
        &pair,
        &messages,
        computed_at_unix_secs,
    )?;

    let elapsed = start.elapsed().as_millis();
    update_status(conn, contact_id, elapsed, None)?;

    Ok(OrchestratorOutput {
        message_count: messages.len(),
        conversation_count: conversations.len(),
        elapsed_ms: elapsed,
        had_data: true,
    })
}

// ===================================================================
// LOADING
// ===================================================================

/// Load all analyzable messages for a contact in chronological order.
/// Excludes:
///   - messages with unknown direction (column 0)
///   - group-MMS rows (`address` containing '~')
///
/// Attachments are loaded in a second query and merged into the per-message
/// `mime_types` vec.
fn load_messages_for_contact(
    conn: &Connection,
    contact_id: &str,
) -> Result<Vec<AggregatorMessage>> {
    // Pass 1: messages.
    let mut stmt = conn
        .prepare(
            "SELECT m.rowid, m.id, m.timestamp, m.message_direction, m.body \
             FROM messages m \
             JOIN contact_addresses ca ON ca.address = m.address \
             WHERE ca.contact_id = ?1 \
               AND m.message_direction IN (1, 2) \
               AND m.address NOT LIKE '%~%' \
             ORDER BY m.timestamp ASC, m.rowid ASC",
        )
        .map_err(AppError::Database)?;

    let mut messages: Vec<AggregatorMessage> = Vec::new();
    let mut id_to_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    let rows = stmt
        .query_map(params![contact_id], |row| {
            let rowid: i64 = row.get(0)?;
            let id: String = row.get(1)?;
            let timestamp_ms: i64 = row.get(2)?;
            let direction: i32 = row.get(3)?;
            let body: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();
            let sender = match MessageDirection::from_i32(direction) {
                MessageDirection::Outgoing => Participant::Me,
                MessageDirection::Incoming => Participant::Them,
                MessageDirection::Unknown => Participant::Them, // shouldn't reach here
            };
            Ok((
                rowid,
                id,
                AggregatorMessage {
                    db_rowid: rowid,
                    timestamp_ms,
                    sender,
                    body,
                    mime_types: Vec::new(),
                },
            ))
        })
        .map_err(AppError::Database)?;

    for row in rows {
        let (_, id, msg) = row.map_err(AppError::Database)?;
        id_to_index.insert(id, messages.len());
        messages.push(msg);
    }
    drop(stmt);

    if messages.is_empty() {
        return Ok(messages);
    }

    // Pass 2: attachments. Same JOIN constraints so we don't pull in
    // attachments for filtered-out messages.
    let mut stmt = conn
        .prepare(
            "SELECT a.message_id, a.mime_type \
             FROM attachments a \
             JOIN messages m ON m.id = a.message_id \
             JOIN contact_addresses ca ON ca.address = m.address \
             WHERE ca.contact_id = ?1 \
               AND m.message_direction IN (1, 2) \
               AND m.address NOT LIKE '%~%'",
        )
        .map_err(AppError::Database)?;
    let rows = stmt
        .query_map(params![contact_id], |row| {
            let message_id: String = row.get(0)?;
            let mime_type: String = row.get(1)?;
            Ok((message_id, mime_type))
        })
        .map_err(AppError::Database)?;
    for row in rows {
        let (mid, mime) = row.map_err(AppError::Database)?;
        if let Some(&idx) = id_to_index.get(&mid) {
            messages[idx].mime_types.push(mime);
        }
    }

    Ok(messages)
}

/// Load up to N message bodies from contacts OTHER than the given one,
/// build a phrase-frequency map for use as the TF-IDF background corpus.
/// Returns (phrase_counts, total_messages_sampled).
///
/// Sample size is bounded so this scales with contact count, not the full
/// 26M-message archive. 50k messages is plenty to estimate phrase rarity.
fn load_background_phrase_counts(
    conn: &Connection,
    contact_id: &str,
) -> Result<(std::collections::HashMap<String, u32>, u32)> {
    const BG_SAMPLE_LIMIT: i64 = 50_000;
    let mut stmt = conn
        .prepare(
            "SELECT m.body \
             FROM messages m \
             JOIN contact_addresses ca ON ca.address = m.address \
             WHERE ca.contact_id != ?1 \
               AND m.message_direction IN (1, 2) \
               AND m.address NOT LIKE '%~%' \
               AND m.body IS NOT NULL AND m.body != '' \
             LIMIT ?2",
        )
        .map_err(AppError::Database)?;
    let rows: Vec<String> = stmt
        .query_map(params![contact_id, BG_SAMPLE_LIMIT], |r| {
            r.get::<_, String>(0)
        })
        .map_err(AppError::Database)?
        .filter_map(|r| r.ok())
        .collect();
    let count = rows.len() as u32;
    let phrases = build_phrase_counts(rows.iter().map(|s| s.as_str()), 3);
    Ok((phrases, count))
}

/// Load every OTHER contact's display name from `contacts`, so the focus
/// detector can scan message bodies for mentions of mutual friends, family,
/// etc. Returns first-name tokens only; `compute_focus` does its own
/// safety filtering.
fn load_other_contact_first_names(conn: &Connection, contact_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT display_name FROM contacts WHERE id != ?1")
        .map_err(AppError::Database)?;
    let rows: Vec<String> = stmt
        .query_map(params![contact_id], |r| r.get::<_, String>(0))
        .map_err(AppError::Database)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Project the heavier `AggregatorMessage` slice down to the slim
/// `MessageRef`s that the segmenter operates on. Pure projection — no
/// allocations beyond the Vec.
fn slim_message_refs(messages: &[AggregatorMessage]) -> Vec<crate::types::MessageRef> {
    messages
        .iter()
        .map(|m| crate::types::MessageRef {
            db_rowid: m.db_rowid,
            timestamp_ms: m.timestamp_ms,
            sender: m.sender,
            has_media: !m.mime_types.is_empty(),
        })
        .collect()
}

// ===================================================================
// PAIR-LEVEL DERIVED METRICS
// ===================================================================

#[derive(Debug, Clone, Default)]
struct PairMetrics {
    convos_started_by_me: u32,
    convos_started_by_them: u32,
    convos_closed_by_me: u32,
    convos_closed_by_them: u32,
    my_convos_missed: u32,
    their_convos_missed: u32,
    reconnect_count_t1: u32,
    reconnect_count_t2: u32,
    reconnect_count_t3: u32,
    reconnect_count_t4: u32,
    total_messages_in_convos: u32,
    avg_convo_points: f64,
    avg_convo_length_msgs: f64,
    median_convo_msgs: f64,
    top_contributor: Option<i32>,
    first_message_ms: i64,
    last_message_ms: i64,
}

fn derive_pair_metrics(
    conversations: &[Conversation],
    responses: &ResponseMetrics,
    _scoring: &ScoringOutput,
) -> PairMetrics {
    let mut p = PairMetrics::default();
    if conversations.is_empty() {
        return p;
    }

    for conv in conversations {
        match conv.started_by {
            Participant::Me => p.convos_started_by_me += 1,
            Participant::Them => p.convos_started_by_them += 1,
        }
        if !conv.is_missed {
            match conv.final_reply_by {
                Participant::Me => p.convos_closed_by_me += 1,
                Participant::Them => p.convos_closed_by_them += 1,
            }
        } else {
            // missed_by tracks who DIDN'T reply.
            match conv.missed_by {
                Some(Participant::Me) => p.my_convos_missed += 1,
                Some(Participant::Them) => p.their_convos_missed += 1,
                None => {}
            }
        }
        p.total_messages_in_convos += conv.total_message_count;
        match conv.reconnect_tier {
            1 => p.reconnect_count_t1 += 1,
            2 => p.reconnect_count_t2 += 1,
            3 => p.reconnect_count_t3 += 1,
            4 => p.reconnect_count_t4 += 1,
            _ => {}
        }
    }

    p.avg_convo_length_msgs = p.total_messages_in_convos as f64 / conversations.len() as f64;
    p.avg_convo_points =
        conversations.iter().map(|c| c.points).sum::<f64>() / conversations.len() as f64;

    let mut counts: Vec<u32> = conversations
        .iter()
        .map(|c| c.total_message_count)
        .collect();
    counts.sort_unstable();
    p.median_convo_msgs = if counts.len() % 2 == 1 {
        counts[counts.len() / 2] as f64
    } else {
        (counts[counts.len() / 2 - 1] + counts[counts.len() / 2]) as f64 / 2.0
    };

    let my_total: u32 = conversations.iter().map(|c| c.my_message_count).sum();
    let their_total: u32 = conversations.iter().map(|c| c.their_message_count).sum();
    p.top_contributor = if my_total > their_total {
        Some(Participant::Me.as_i32())
    } else if their_total > my_total {
        Some(Participant::Them.as_i32())
    } else {
        None
    };

    p.first_message_ms = conversations.first().map(|c| c.start_time_ms).unwrap_or(0);
    p.last_message_ms = conversations.last().map(|c| c.end_time_ms).unwrap_or(0);

    let _ = responses; // placeholder so future expansion is obvious

    p
}

// ===================================================================
// PERSISTENCE
// ===================================================================

#[allow(clippy::too_many_arguments)]
fn persist_all(
    conn: &Connection,
    contact_id: &str,
    conversations: &[Conversation],
    aggregates: &AggregatesOutput,
    responses: &ResponseMetrics,
    flow: &SankeyData,
    focus: &FocusOutput,
    sentiment: &SentimentTimeline,
    inside_jokes: &[InsideJoke],
    topics: &[TopicPhrase],
    scoring: &ScoringOutput,
    rating: &RatingOutput,
    insights: &[Insight],
    pair: &PairMetrics,
    messages: &[AggregatorMessage],
    computed_at_unix_secs: i64,
) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")
        .map_err(AppError::Database)?;

    let inner = (|| -> rusqlite::Result<()> {
        // Wipe prior content.
        for table in [
            "conversations",
            "contact_analytics",
            "pair_analytics",
            "activity_daily",
            "activity_hourly",
        ] {
            // Safe: table names are static literals.
            let sql = format!("DELETE FROM {} WHERE contact_id = ?1", table);
            conn.execute(&sql, params![contact_id])?;
        }

        // Insert conversations.
        let mut stmt = conn.prepare_cached(
            "INSERT INTO conversations (contact_id, start_time, end_time, started_by, final_reply_by, \
                my_message_count, their_message_count, total_message_count, \
                major_contributor, is_missed, missed_by, \
                is_big_moment_static, is_big_moment_dynamic, reconnect_tier, points) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?;
        for c in conversations {
            stmt.execute(params![
                contact_id,
                c.start_time_ms,
                c.end_time_ms,
                c.started_by.as_i32(),
                c.final_reply_by.as_i32(),
                c.my_message_count,
                c.their_message_count,
                c.total_message_count,
                c.major_contributor.as_i32(),
                if c.is_missed { 1 } else { 0 },
                c.missed_by.map(|p| p.as_i32()),
                if c.is_big_moment_static { 1 } else { 0 },
                if c.is_big_moment_dynamic { 1 } else { 0 },
                c.reconnect_tier as i32,
                c.points,
            ])?;
        }
        drop(stmt);

        // contact_analytics — one row.
        let agg = &aggregates.contact;
        let my_top_emojis_json = serde_json::to_string(&agg.my_top_emojis).unwrap_or("[]".into());
        let their_top_emojis_json =
            serde_json::to_string(&agg.their_top_emojis).unwrap_or("[]".into());
        conn.execute(
            "INSERT INTO contact_analytics (contact_id, computed_at, \
                my_message_count, their_message_count, my_word_count, their_word_count, \
                my_unique_word_count, their_unique_word_count, my_character_count, their_character_count, \
                my_image_count, their_image_count, my_video_count, their_video_count, \
                my_audio_count, their_audio_count, my_gif_count, their_gif_count, \
                my_link_count, their_link_count, \
                my_top_emojis, their_top_emojis, my_emoji_total, their_emoji_total, \
                my_laugh_count, their_laugh_count, my_apology_count, their_apology_count, \
                my_question_count, their_question_count, my_encouragement_count, their_encouragement_count) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32)",
            params![
                contact_id,
                computed_at_unix_secs,
                agg.my_message_count,
                agg.their_message_count,
                agg.my_word_count,
                agg.their_word_count,
                agg.my_unique_word_count,
                agg.their_unique_word_count,
                agg.my_character_count,
                agg.their_character_count,
                agg.my_image_count,
                agg.their_image_count,
                agg.my_video_count,
                agg.their_video_count,
                agg.my_audio_count,
                agg.their_audio_count,
                agg.my_gif_count,
                agg.their_gif_count,
                agg.my_link_count,
                agg.their_link_count,
                my_top_emojis_json,
                their_top_emojis_json,
                agg.my_emoji_total,
                agg.their_emoji_total,
                agg.my_laugh_count,
                agg.their_laugh_count,
                agg.my_apology_count,
                agg.their_apology_count,
                agg.my_question_count,
                agg.their_question_count,
                agg.my_encouragement_count,
                agg.their_encouragement_count,
            ],
        )?;

        // pair_analytics — one row.
        let histogram_json = serde_json::to_string(&ResponseHistogramJson::from_metrics(responses))
            .unwrap_or("{}".into());
        let insights_json = serde_json::to_string(insights).unwrap_or("[]".into());
        let flow_json = serde_json::to_string(flow).unwrap_or("{}".into());
        let total_chars = agg
            .my_character_count
            .saturating_add(agg.their_character_count);
        let total_words = agg.my_word_count.saturating_add(agg.their_word_count);
        const HP1_CHARS: u64 = 440_000;
        let hp_equivalents = total_chars as f64 / HP1_CHARS as f64;
        let writing_milestones_json = serde_json::json!({
            "total_chars": total_chars,
            "total_words": total_words,
            "harry_potter_equivalents": hp_equivalents,
        })
        .to_string();
        let first_message_at = messages.first().map(|m| m.timestamp_ms).unwrap_or(0);
        let last_message_at = messages.last().map(|m| m.timestamp_ms).unwrap_or(0);

        conn.execute(
            "INSERT INTO pair_analytics (contact_id, computed_at, \
                total_conversations, convos_started_by_me, convos_started_by_them, \
                convos_closed_by_me, convos_closed_by_them, top_contributor, \
                avg_convo_points, median_convo_messages, \
                my_double_messages, their_double_messages, \
                my_convos_missed, their_convos_missed, \
                reconnect_count_t1, reconnect_count_t2, reconnect_count_t3, reconnect_count_t4, \
                my_median_response_ms, their_median_response_ms, \
                my_mean_response_ms, their_mean_response_ms, \
                my_rapid_response_pct, their_rapid_response_pct, \
                my_median_first_response_ms, their_median_first_response_ms, \
                my_mean_first_response_ms, their_mean_first_response_ms, \
                my_median_response_awake_ms, their_median_response_awake_ms, \
                my_median_response_overnight_ms, their_median_response_overnight_ms, \
                response_histogram_json, \
                my_points, their_points, \
                overall_score, score_responsiveness, score_balance, score_engagement, score_consistency, score_reciprocity, score_longevity, score_mutual_effort, \
                focus_me_pct, focus_them_pct, focus_other_pct, \
                insights_json, writing_milestones_json, conversation_flow_json, \
                first_message_at, last_message_at, \
                sentiment_timeline_json, inside_jokes_json, topics_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46, ?47, ?48, ?49, ?50, ?51, ?52, ?53, ?54)",
            params![
                contact_id,
                computed_at_unix_secs,
                conversations.len() as u32,
                pair.convos_started_by_me,
                pair.convos_started_by_them,
                pair.convos_closed_by_me,
                pair.convos_closed_by_them,
                pair.top_contributor,
                pair.avg_convo_points,
                pair.median_convo_msgs,
                responses.my_double_messages,
                responses.their_double_messages,
                pair.my_convos_missed,
                pair.their_convos_missed,
                pair.reconnect_count_t1,
                pair.reconnect_count_t2,
                pair.reconnect_count_t3,
                pair.reconnect_count_t4,
                responses.my_median_response_ms,
                responses.their_median_response_ms,
                responses.my_mean_response_ms,
                responses.their_mean_response_ms,
                responses.my_rapid_response_pct,
                responses.their_rapid_response_pct,
                responses.my_median_first_response_ms,
                responses.their_median_first_response_ms,
                responses.my_mean_first_response_ms,
                responses.their_mean_first_response_ms,
                responses.my_median_response_awake_ms,
                responses.their_median_response_awake_ms,
                responses.my_median_response_overnight_ms,
                responses.their_median_response_overnight_ms,
                histogram_json,
                scoring.total_my_points,
                scoring.total_their_points,
                rating.overall as i32,
                rating.responsiveness as i32,
                rating.balance as i32,
                rating.engagement as i32,
                rating.consistency as i32,
                rating.reciprocity as i32,
                rating.longevity as i32,
                rating.mutual_effort as i32,
                focus.focus_me_pct,
                focus.focus_them_pct,
                focus.focus_other_pct,
                insights_json,
                writing_milestones_json,
                flow_json,
                first_message_at,
                last_message_at,
                serde_json::to_string(sentiment).unwrap_or_else(|_| "{}".into()),
                serde_json::to_string(inside_jokes).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(topics).unwrap_or_else(|_| "[]".into()),
            ],
        )?;

        // activity_daily — merged from aggregates + scoring's daily points.
        let mut daily_stmt = conn.prepare_cached(
            "INSERT INTO activity_daily (contact_id, day, my_messages, their_messages, my_words, their_words, my_media, their_media, my_points, their_points) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        for bucket in &aggregates.daily {
            let (my_pts, their_pts) = scoring
                .daily_points
                .get(&bucket.day)
                .copied()
                .unwrap_or((0.0, 0.0));
            daily_stmt.execute(params![
                contact_id,
                bucket.day,
                bucket.my_messages,
                bucket.their_messages,
                bucket.my_words,
                bucket.their_words,
                bucket.my_media,
                bucket.their_media,
                my_pts,
                their_pts,
            ])?;
        }
        drop(daily_stmt);

        // activity_hourly.
        let mut hourly_stmt = conn.prepare_cached(
            "INSERT INTO activity_hourly (contact_id, day_of_week, hour, message_count) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for bucket in &aggregates.hourly {
            hourly_stmt.execute(params![
                contact_id,
                bucket.day_of_week as i32,
                bucket.hour as i32,
                bucket.message_count,
            ])?;
        }
        drop(hourly_stmt);

        // Suppress an unused-variable warning for the rare case where
        // DailyBucket gains future fields we don't pass here yet.
        let _: &[DailyBucket] = &aggregates.daily;
        Ok(())
    })();

    match inner {
        Ok(()) => {
            conn.execute_batch("COMMIT").map_err(AppError::Database)?;
            Ok(())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(AppError::Database(err))
        }
    }
}

fn clear_contact_analytics(conn: &Connection, contact_id: &str) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE TRANSACTION")
        .map_err(AppError::Database)?;
    let result: rusqlite::Result<()> = (|| {
        for table in [
            "conversations",
            "contact_analytics",
            "pair_analytics",
            "activity_daily",
            "activity_hourly",
        ] {
            let sql = format!("DELETE FROM {} WHERE contact_id = ?1", table);
            conn.execute(&sql, params![contact_id])?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT").map_err(AppError::Database)?;
            Ok(())
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(AppError::Database(err))
        }
    }
}

fn update_status(
    conn: &Connection,
    contact_id: &str,
    elapsed_ms: u128,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO contact_analytics_status (contact_id, last_computed_at, is_stale, last_compute_ms, last_error) \
         VALUES (?1, strftime('%s', 'now'), 0, ?2, ?3) \
         ON CONFLICT(contact_id) DO UPDATE SET \
             last_computed_at = excluded.last_computed_at, \
             is_stale = 0, \
             last_compute_ms = excluded.last_compute_ms, \
             last_error = excluded.last_error",
        params![contact_id, elapsed_ms as i64, error],
    )
    .map_err(AppError::Database)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sms_config::ResourceProfile;
    use sms_db::Database;
    use uuid::Uuid;

    /// Set up a temp DB with one contact and a small chat history.
    /// Returns (db, contact_id).
    fn fixture_db() -> (tempfile::NamedTempFile, String) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path(), ResourceProfile::Low).unwrap();
        let conn = db.connection();

        // Create one contact with one address.
        let contact_id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO contacts (id, display_name, source) VALUES (?1, ?2, 'manual')",
            params![contact_id, "Test Contact"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contact_addresses (id, contact_id, address) VALUES (?1, ?2, ?3)",
            params![Uuid::new_v4().to_string(), contact_id, "5551234567"],
        )
        .unwrap();

        // Three messages: Me opening, Them replying, Me replying.
        for (i, (ts_ms, dir, body)) in [
            (1_000_000_000_000i64, 2, "hello there"),  // Me
            (1_000_000_060_000i64, 1, "hi back!"),     // Them
            (1_000_000_120_000i64, 2, "how are you?"), // Me
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO messages (id, timestamp, address, body, body_searchable, message_type, message_direction) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)",
                params![
                    format!("msg-{}", i),
                    ts_ms,
                    "5551234567",
                    body,
                    body,
                    dir,
                ],
            )
            .unwrap();
        }

        drop(db);
        (tmp, contact_id)
    }

    #[test]
    fn end_to_end_writes_all_analytics_tables() {
        let (tmp, contact_id) = fixture_db();
        let db = Database::open(tmp.path(), ResourceProfile::Low).unwrap();
        let conn = db.connection();

        let config = OrchestratorConfig::default();
        let out = compute_for_contact(conn, &contact_id, &config).expect("orchestrator failed");
        assert!(out.had_data);
        assert_eq!(out.message_count, 3);
        assert!(out.conversation_count >= 1);

        // conversations row exists
        let convo_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE contact_id = ?1",
                params![contact_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(convo_count >= 1);

        // contact_analytics row exists with the message counts we expect
        let (my, theirs): (u32, u32) = conn
            .query_row(
                "SELECT my_message_count, their_message_count FROM contact_analytics WHERE contact_id = ?1",
                params![contact_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(my, 2);
        assert_eq!(theirs, 1);

        // pair_analytics has scoring totals > 0
        let (my_pts, their_pts): (f64, f64) = conn
            .query_row(
                "SELECT my_points, their_points FROM pair_analytics WHERE contact_id = ?1",
                params![contact_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(my_pts > 0.0);
        assert!(their_pts > 0.0);

        // activity_daily and hourly populated
        let daily_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM activity_daily WHERE contact_id = ?1",
                params![contact_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(daily_count >= 1);
        let hourly_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM activity_hourly WHERE contact_id = ?1",
                params![contact_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(hourly_count >= 1);

        // status row has is_stale=0
        let is_stale: i64 = conn
            .query_row(
                "SELECT is_stale FROM contact_analytics_status WHERE contact_id = ?1",
                params![contact_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(is_stale, 0);
    }

    #[test]
    fn rerunning_replaces_prior_analytics() {
        let (tmp, contact_id) = fixture_db();
        let db = Database::open(tmp.path(), ResourceProfile::Low).unwrap();
        let conn = db.connection();
        let config = OrchestratorConfig::default();

        // First run.
        compute_for_contact(conn, &contact_id, &config).unwrap();
        let convo_count_1: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE contact_id = ?1",
                params![contact_id],
                |r| r.get(0),
            )
            .unwrap();

        // Second run — should not duplicate rows.
        compute_for_contact(conn, &contact_id, &config).unwrap();
        let convo_count_2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE contact_id = ?1",
                params![contact_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            convo_count_1, convo_count_2,
            "second run must replace, not append"
        );

        // contact_analytics row count must be 1.
        let ca_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM contact_analytics WHERE contact_id = ?1",
                params![contact_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ca_count, 1);
    }

    #[test]
    fn empty_contact_completes_with_had_data_false() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db = Database::open(tmp.path(), ResourceProfile::Low).unwrap();
        let conn = db.connection();

        // Create a contact with no messages.
        let contact_id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO contacts (id, display_name, source) VALUES (?1, ?2, 'manual')",
            params![contact_id, "Empty Contact"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO contact_addresses (id, contact_id, address) VALUES (?1, ?2, ?3)",
            params![Uuid::new_v4().to_string(), contact_id, "5559999999"],
        )
        .unwrap();

        let config = OrchestratorConfig::default();
        let out = compute_for_contact(conn, &contact_id, &config).unwrap();
        assert!(!out.had_data);
        assert_eq!(out.message_count, 0);
    }
}
