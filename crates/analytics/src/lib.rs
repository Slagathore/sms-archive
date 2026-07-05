//! Per-contact analytics engine.
//!
//! This crate produces the data shown on the Analytics dashboard: segmented
//! conversations, per-side aggregate counts, response-time statistics, points
//! totals, composite chat ratings, and rule-driven insights.
//!
//! # Architecture
//!
//! Each module is a **pure function over in-memory data**. The orchestrator
//! (added later) is the only entry point that touches the database — it loads
//! messages for a contact, hands them to these modules, and writes the results
//! back to the analytics tables.
//!
//! Keeping this crate DB-free has two payoffs:
//! 1. **Trivial testability** — every algorithm can be unit-tested with a tiny
//!    fixture vector, no SQLite, no temp files.
//! 2. **Composability** — the same modules can be reused in CLI tools, batch
//!    re-computes, or future export features without dragging the DB layer.
//!
//! # Modules
//!
//! - [`types`]      — Shared types (`Participant`, `Conversation`, `MessageRef`,
//!   `SegmentationConfig`).
//! - [`segmenter`]  — Splits a message stream into conversations on timeout gaps.
//! - [`media`]      — MIME → category classification.
//! - [`patterns`]   — Compile-once regex matchers (laugh/apology/question/etc).
//! - [`emoji`]      — Grapheme-cluster-aware emoji extraction.
//! - [`aggregator`] — Single-pass counts, daily/hourly buckets, language stats.
//! - [`responses`]  — Response-time math (median/mean/awake-overnight/histogram).
//! - [`flow`]       — Sankey path counting (4-column conversation flow).
//! - [`scoring`]    — Per-message points (log-scale word weights, dual-flag aware).
//! - [`rating`]     — Composite 0-100 chat rating + 7 weighted components.
//! - [`insights`]   — Hardcoded rule engine for the colored callouts + chi-square helper.

pub mod aggregator;
pub mod emoji;
pub mod flow;
pub mod focus;
pub mod inside_jokes;
pub mod insights;
pub mod media;
pub mod orchestrator;
pub mod patterns;
pub mod rating;
pub mod responses;
pub mod scoring;
pub mod segmenter;
pub mod sentiment;
pub mod topics;
pub mod types;

pub use aggregator::{
    compute_aggregates, AggregatesOutput, AggregatorMessage, ContactAggregates, DailyBucket,
    HourlyBucket,
};
pub use emoji::{extract_emojis, top_emojis, EmojiCount};
pub use flow::{build_conversation_flow, SankeyData, SankeyLink, SankeyNode};
pub use focus::{compute_focus, FocusOutput};
pub use inside_jokes::{detect_inside_jokes, InsideJoke, InsideJokesConfig};
pub use insights::{
    chi_square_2x2, compute_insights, Confidence, EngineConfig, Insight, InsightCategory,
    InsightCtx,
};
pub use media::{classify_media, MediaCategory};
pub use orchestrator::{compute_for_contact, OrchestratorConfig, OrchestratorOutput};
pub use patterns::{contains_link, is_apology, is_encouragement, is_laugh, is_question};
pub use rating::{
    compute_rating, RatingConfidence, RatingInput, RatingOutput, RatingThresholds, RatingWeights,
};
pub use responses::{
    compute_response_metrics, ResponseConfig, ResponseHistogramJson, ResponseMessage,
    ResponseMetrics, HIST_BUCKET_LABELS,
};
pub use scoring::{compute_message_points, compute_scoring, PointWeights, ScoringOutput};
pub use segmenter::segment_conversations;
pub use sentiment::{compute_sentiment_timeline, SentimentDay, SentimentTimeline};
pub use topics::{build_phrase_counts, compute_topics, TopicPhrase, TopicsConfig};
pub use types::{Conversation, MessageRef, Participant, SegmentationConfig};
