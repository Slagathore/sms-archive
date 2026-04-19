# SMS Archive — Analytics Dashboard Module Bootstrap

> **Purpose**: Drop-in specification for building a Mimoto-style relationship analytics dashboard.
> Read this entire document before writing code. Follow implementation phases in order.
> Every module, function signature, data structure, and test case is defined here.

**Last updated**: 2026-04-12
**Parent project**: SMS Archive Manager (Tauri + React + Rust + Python sidecar)
**Target**: New `Analytics` tab in the main app nav

---

## Table of Contents

1. [Architecture Overview](#1-architecture-overview)
2. [Data Flow](#2-data-flow)
3. [Database Schema — Migration 002](#3-database-schema--migration-002)
4. [Core Algorithm: Conversation Segmentation](#4-core-algorithm-conversation-segmentation)
5. [Rust Analytics Engine](#5-rust-analytics-engine)
6. [Python Analytics Engine](#6-python-analytics-engine)
7. [Tauri IPC API Contract](#7-tauri-ipc-api-contract)
8. [TypeScript Types](#8-typescript-types)
9. [React Component Architecture](#9-react-component-architecture)
10. [Visualization Specifications](#10-visualization-specifications)
11. [Chat Rating — Composite Scoring Algorithm](#11-chat-rating--composite-scoring-algorithm)
12. [Key Insights — Rule Engine](#12-key-insights--rule-engine)
13. [Performance Budget & Optimization](#13-performance-budget--optimization)
14. [Test Plan](#14-test-plan)
15. [Implementation Phases](#15-implementation-phases)
16. [File Tree (Final State)](#16-file-tree-final-state)

---

## 1. Architecture Overview

### Principle: Compute Once, Read Many

Analytics are **pre-computed and cached** in SQLite tables. The dashboard reads from cache.
Recomputation is triggered by:
- New XML import (automatic, post-import hook)
- User clicks "Refresh Analytics" (manual)
- First-time load for a contact with no cached data

### Compute Location Split

```
┌─────────────────────────────────────────────────────────────────┐
│                        RUST (src-tauri)                         │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ conversation_segmenter.rs  — Segment messages into convos  │ │
│  │ aggregator.rs              — SQL aggregations, time-series │ │
│  │ response_calculator.rs     — Response time math            │ │
│  │ emoji_extractor.rs         — Unicode emoji parsing         │ │
│  │ pattern_matcher.rs         — Regex: laughs, questions, etc │ │
│  │ media_classifier.rs        — Categorize attachment types   │ │
│  │ flow_builder.rs            — Sankey conversation flow data │ │
│  └────────────────────────────────────────────────────────────┘ │
│  WHY RUST: These iterate over every message. Must be fast.      │
│  600k messages × multiple passes = Rust or it takes minutes.    │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ (JSON-RPC over stdio)
┌─────────────────────────────────────────────────────────────────┐
│                      PYTHON (src-python)                        │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ scoring.py                 — Chat points weighted system   │ │
│  │ insights.py                — Rule-based insight generation │ │
│  │ rating.py                  — Composite chat score (0-100)  │ │
│  │ topic_classifier.py        — Direction of conversation     │ │
│  │ writing_milestones.py      — Book-length comparisons       │ │
│  └────────────────────────────────────────────────────────────┘ │
│  WHY PYTHON: These are high-level logic over pre-aggregated     │
│  data. Python is fine here — we're processing summary rows,     │
│  not raw messages.                                              │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ (Tauri IPC → React)
┌─────────────────────────────────────────────────────────────────┐
│                     REACT (src/components)                      │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ AnalyticsDashboard.tsx     — Layout + contact selector     │ │
│  │ 15 visualization components (see Section 9)                │ │
│  │ useAnalytics.ts            — Data fetching hook            │ │
│  └────────────────────────────────────────────────────────────┘ │
│  WHY REACT: It's your frontend. Recharts + d3 for viz.          │
└─────────────────────────────────────────────────────────────────┘
```

### Why This Split (Decision Record)

| Alternative | Why Rejected |
|---|---|
| All in Rust | Insight rules and scoring logic would be painful to iterate on. Python lets you tweak weights and rules rapidly without recompiling. |
| All in Python | Iterating 600k messages in Python for conversation segmentation and response times would take 30-60 seconds. Rust does it in under 2 seconds. |
| Compute on frontend | No. Never send 600k messages to the renderer. |
| Compute on every dashboard load | Wasteful. Pre-compute and cache. Only recompute when data changes. |

---

## 2. Data Flow

```
                    ┌──────────────┐
                    │  XML Import  │
                    │  completes   │
                    └──────┬───────┘
                           │
                           ▼
              ┌────────────────────────┐
              │  Rust: Segmentation    │  Pass 1: Walk all messages for a contact
              │  conversation_         │  chronologically, emit Conversation structs
              │  segmenter.rs          │
              └────────────┬───────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │  Rust: Aggregation     │  Pass 2: Count messages, words, chars,
              │  aggregator.rs         │  media, populate activity_daily and
              │  response_calculator   │  activity_hourly tables
              │  emoji_extractor       │
              │  pattern_matcher       │
              │  media_classifier      │
              └────────────┬───────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │  Write to SQLite       │  contact_analytics, pair_analytics,
              │  analytics tables      │  activity_daily, activity_hourly,
              │                        │  conversations (segment metadata)
              └────────────┬───────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │  Python: Scoring &     │  Reads from analytics tables (NOT raw
              │  Insights              │  messages). Writes back: chat_rating,
              │  scoring.py            │  insights JSON, chat_focus breakdown,
              │  insights.py           │  writing milestones
              │  rating.py             │
              │  topic_classifier.py   │
              └────────────┬───────────┘
                           │
                           ▼
              ┌────────────────────────┐
              │  React: Dashboard      │  Reads all analytics tables via Tauri
              │  renders cached data   │  IPC. Zero computation in the renderer.
              └────────────────────────┘
```

### Invalidation Strategy

A `computed_at` timestamp on each analytics row tracks freshness.
After any import, Rust marks affected contacts as stale (`computed_at = 0`).
When the dashboard opens for a contact:
1. Check `computed_at` — if 0 or older than last import, recompute.
2. Show a loading skeleton while recomputing.
3. Cache result. Subsequent loads are instant.

---

## 3. Database Schema — Migration 002

File: `migrations/002_analytics.sql`

```sql
-- ============================================================
-- CONVERSATIONS TABLE
-- Each row is one segmented conversation between you and a contact.
-- This is the foundational table — most other analytics derive from it.
-- ============================================================
CREATE TABLE IF NOT EXISTS conversations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    contact_id TEXT NOT NULL,              -- normalized phone number / contact key
    start_time INTEGER NOT NULL,           -- unix epoch of first message
    end_time INTEGER NOT NULL,             -- unix epoch of last message
    started_by TEXT NOT NULL,              -- 'me' or 'them'
    final_reply_by TEXT NOT NULL,          -- 'me' or 'them'
    my_message_count INTEGER NOT NULL DEFAULT 0,
    their_message_count INTEGER NOT NULL DEFAULT 0,
    total_message_count INTEGER NOT NULL DEFAULT 0,
    major_contributor TEXT NOT NULL,       -- 'me' or 'them' (whoever sent more)
    is_missed INTEGER NOT NULL DEFAULT 0, -- 1 if only one person spoke
    missed_by TEXT,                        -- 'me' or 'them' (who never replied)
    category TEXT NOT NULL DEFAULT 'everyday', -- 'big_moment' or 'everyday'
    -- A "big moment" conversation has >= BIG_MOMENT_THRESHOLD messages.
    -- Default threshold: 20 messages. Configurable in settings.

    FOREIGN KEY (contact_id) REFERENCES contacts(id)
);
CREATE INDEX IF NOT EXISTS idx_conversations_contact ON conversations(contact_id);
CREATE INDEX IF NOT EXISTS idx_conversations_time ON conversations(contact_id, start_time);


-- ============================================================
-- CONTACT ANALYTICS
-- Per-person aggregate stats. One row per contact.
-- Recomputed in full on each analytics refresh.
-- ============================================================
CREATE TABLE IF NOT EXISTS contact_analytics (
    contact_id TEXT PRIMARY KEY,
    computed_at INTEGER NOT NULL DEFAULT 0,

    -- === Message Volume ===
    my_message_count INTEGER NOT NULL DEFAULT 0,
    their_message_count INTEGER NOT NULL DEFAULT 0,
    my_word_count INTEGER NOT NULL DEFAULT 0,
    their_word_count INTEGER NOT NULL DEFAULT 0,
    my_unique_word_count INTEGER NOT NULL DEFAULT 0,
    their_unique_word_count INTEGER NOT NULL DEFAULT 0,
    my_character_count INTEGER NOT NULL DEFAULT 0,
    their_character_count INTEGER NOT NULL DEFAULT 0,

    -- === Media Stats ===
    my_image_count INTEGER NOT NULL DEFAULT 0,
    their_image_count INTEGER NOT NULL DEFAULT 0,
    my_video_count INTEGER NOT NULL DEFAULT 0,
    their_video_count INTEGER NOT NULL DEFAULT 0,
    my_audio_count INTEGER NOT NULL DEFAULT 0,
    their_audio_count INTEGER NOT NULL DEFAULT 0,
    my_gif_count INTEGER NOT NULL DEFAULT 0,
    their_gif_count INTEGER NOT NULL DEFAULT 0,
    my_link_count INTEGER NOT NULL DEFAULT 0,
    their_link_count INTEGER NOT NULL DEFAULT 0,

    -- === Language Patterns ===
    -- Top emojis stored as JSON: [{"emoji": "😂", "count": 100}, ...]
    my_top_emojis TEXT NOT NULL DEFAULT '[]',
    their_top_emojis TEXT NOT NULL DEFAULT '[]',
    my_emoji_total INTEGER NOT NULL DEFAULT 0,
    their_emoji_total INTEGER NOT NULL DEFAULT 0,
    my_laugh_count INTEGER NOT NULL DEFAULT 0,
    their_laugh_count INTEGER NOT NULL DEFAULT 0,
    my_apology_count INTEGER NOT NULL DEFAULT 0,
    their_apology_count INTEGER NOT NULL DEFAULT 0,
    my_question_count INTEGER NOT NULL DEFAULT 0,
    their_question_count INTEGER NOT NULL DEFAULT 0,
    my_encouragement_count INTEGER NOT NULL DEFAULT 0,
    their_encouragement_count INTEGER NOT NULL DEFAULT 0,

    FOREIGN KEY (contact_id) REFERENCES contacts(id)
);


-- ============================================================
-- PAIR ANALYTICS
-- Relationship-level stats computed from conversations table.
-- ============================================================
CREATE TABLE IF NOT EXISTS pair_analytics (
    contact_id TEXT PRIMARY KEY,
    computed_at INTEGER NOT NULL DEFAULT 0,

    -- === Conversation Stats ===
    total_conversations INTEGER NOT NULL DEFAULT 0,
    convos_started_by_me INTEGER NOT NULL DEFAULT 0,
    convos_started_by_them INTEGER NOT NULL DEFAULT 0,
    convos_closed_by_me INTEGER NOT NULL DEFAULT 0,
    convos_closed_by_them INTEGER NOT NULL DEFAULT 0,
    top_contributor TEXT,                   -- 'me' or 'them' overall
    avg_convo_points REAL NOT NULL DEFAULT 0,
    reconnects INTEGER NOT NULL DEFAULT 0, -- convos starting after 24h+ silence
    my_double_messages INTEGER NOT NULL DEFAULT 0,  -- I sent 2+ in a row
    their_double_messages INTEGER NOT NULL DEFAULT 0,
    my_convos_missed INTEGER NOT NULL DEFAULT 0,    -- they spoke, I never replied
    their_convos_missed INTEGER NOT NULL DEFAULT 0,

    -- === Response Times (stored in milliseconds) ===
    my_avg_response_ms INTEGER,
    their_avg_response_ms INTEGER,
    my_median_response_ms INTEGER,
    their_median_response_ms INTEGER,
    my_rapid_response_pct REAL,    -- % of my responses under 60 seconds
    their_rapid_response_pct REAL,
    my_avg_first_response_ms INTEGER,   -- avg time to reply to convo opener
    their_avg_first_response_ms INTEGER,

    -- === Balance & Rating (populated by Python) ===
    my_points INTEGER NOT NULL DEFAULT 0,
    their_points INTEGER NOT NULL DEFAULT 0,
    balance_ratio REAL,            -- 0.0 to 1.0, where 0.5 = perfect balance
    overall_score INTEGER,         -- 0 to 100
    score_breakdown TEXT,          -- JSON: {"responsiveness": 85, "balance": 92, ...}

    -- === Insights (populated by Python) ===
    insights_json TEXT NOT NULL DEFAULT '[]',
    -- JSON array of Insight objects (see Section 12)

    -- === Chat Focus / Direction (populated by Python) ===
    focus_me_pct REAL,             -- % of convo about me
    focus_them_pct REAL,           -- % of convo about them
    focus_other_pct REAL,          -- % of convo about other people/things

    -- === Writing Milestones (populated by Python) ===
    writing_milestones_json TEXT NOT NULL DEFAULT '{}',
    -- JSON: {"total_chars": N, "total_words": N, "typing_time_estimate_hrs": N,
    --        "milestone_books": [...], "completion_pct": 0.85}

    -- === Conversation Flow / Sankey (populated by Rust) ===
    conversation_flow_json TEXT NOT NULL DEFAULT '{}',
    -- JSON matching SankeyData type (see Section 8)

    -- === Time Span ===
    first_message_at INTEGER,      -- unix epoch
    last_message_at INTEGER,       -- unix epoch

    FOREIGN KEY (contact_id) REFERENCES contacts(id)
);


-- ============================================================
-- ACTIVITY DAILY
-- One row per contact per day. Powers the time-series chart
-- and the GitHub-style heatmap.
-- ============================================================
CREATE TABLE IF NOT EXISTS activity_daily (
    contact_id TEXT NOT NULL,
    day TEXT NOT NULL,                -- 'YYYY-MM-DD' format
    my_messages INTEGER NOT NULL DEFAULT 0,
    their_messages INTEGER NOT NULL DEFAULT 0,
    my_words INTEGER NOT NULL DEFAULT 0,
    their_words INTEGER NOT NULL DEFAULT 0,
    my_media INTEGER NOT NULL DEFAULT 0,
    their_media INTEGER NOT NULL DEFAULT 0,
    -- Points are per-day aggregates, used for the Relationship Growth chart
    my_points INTEGER NOT NULL DEFAULT 0,
    their_points INTEGER NOT NULL DEFAULT 0,

    PRIMARY KEY (contact_id, day)
);
CREATE INDEX IF NOT EXISTS idx_activity_daily_contact ON activity_daily(contact_id);


-- ============================================================
-- ACTIVITY HOURLY
-- One row per contact per (day_of_week, hour) pair.
-- Powers the messaging times heatmap.
-- ============================================================
CREATE TABLE IF NOT EXISTS activity_hourly (
    contact_id TEXT NOT NULL,
    day_of_week INTEGER NOT NULL,    -- 0 = Sunday, 6 = Saturday
    hour INTEGER NOT NULL,           -- 0–23
    message_count INTEGER NOT NULL DEFAULT 0,

    PRIMARY KEY (contact_id, day_of_week, hour)
);


-- ============================================================
-- ANALYTICS META
-- Tracks when analytics were last computed per contact,
-- and stores user-configurable thresholds.
-- ============================================================
CREATE TABLE IF NOT EXISTS analytics_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Default settings (inserted only if not present)
INSERT OR IGNORE INTO analytics_meta (key, value) VALUES
    ('conversation_timeout_secs', '14400'),    -- 4 hours
    ('big_moment_threshold', '20'),            -- messages per convo
    ('rapid_response_threshold_secs', '60'),   -- 1 minute
    ('reconnect_threshold_secs', '86400'),     -- 24 hours
    ('last_global_compute', '0');
```

### Migration Notes

- All analytics tables use `CREATE IF NOT EXISTS` — safe to re-run.
- `conversations` table is a **derived table**, not source data. It can be fully regenerated from the `messages` table at any time.
- JSON columns (`insights_json`, `score_breakdown`, etc.) use TEXT storage. SQLite's `json_extract()` can query them if needed, but the primary consumer is the frontend which deserializes the whole blob.
- The `analytics_meta` table stores tunable parameters so they survive app restarts and are accessible to both Rust and Python.

---

## 4. Core Algorithm: Conversation Segmentation

This is the foundation everything else builds on. Get this right first.

### Algorithm

```
Input:  All messages for a (me, contact) pair, sorted by timestamp ASC
Output: Vec<Conversation>

State:
  current_convo: Option<ConversationBuilder>
  timeout: i64 (from analytics_meta, default 14400 seconds)

For each message M:
  if current_convo is None:
    Start new convo with M
    continue

  gap = M.timestamp - current_convo.last_message_time

  if gap > timeout:
    Finalize current_convo → push to results
    Start new convo with M
  else:
    Append M to current_convo

After loop:
  Finalize current_convo if Some → push to results
```

### Rust Implementation Spec

File: `src-tauri/src/analytics/conversation_segmenter.rs`

```rust
use serde::{Deserialize, Serialize};

/// Who sent the message — from the perspective of the app user ("me").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Participant {
    Me,
    Them,
}

/// Lightweight reference to a message used during segmentation.
/// We do NOT load full message bodies into memory — just what we need.
#[derive(Debug, Clone)]
pub struct MessageRef {
    pub id: i64,             // messages.rowid
    pub timestamp: i64,      // unix epoch
    pub sender: Participant,
    pub word_count: u32,     // pre-counted during import, or count here
    pub has_media: bool,
}

/// Output of the segmentation pass.
#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub contact_id: String,
    pub start_time: i64,
    pub end_time: i64,
    pub started_by: Participant,
    pub final_reply_by: Participant,
    pub my_message_count: u32,
    pub their_message_count: u32,
    pub total_message_count: u32,
    pub major_contributor: Participant,
    pub is_missed: bool,
    pub missed_by: Option<Participant>,
    pub category: ConversationCategory,
}

#[derive(Debug, Clone, Serialize)]
pub enum ConversationCategory {
    BigMoment,  // total_message_count >= threshold
    Everyday,
}

/// Configuration read from analytics_meta table.
pub struct SegmentationConfig {
    pub conversation_timeout_secs: i64,  // default: 14400 (4 hours)
    pub big_moment_threshold: u32,       // default: 20 messages
    pub reconnect_threshold_secs: i64,   // default: 86400 (24 hours)
}

/// Entry point. Processes one contact at a time.
/// Messages MUST be pre-sorted by timestamp ASC.
///
/// IMPORTANT: This function streams messages from SQLite via a cursor.
/// It does NOT load all messages into memory.
pub fn segment_conversations(
    contact_id: &str,
    messages: impl Iterator<Item = MessageRef>,
    config: &SegmentationConfig,
) -> Vec<Conversation> {
    // Implementation follows the algorithm above.
    // See test cases in Section 14 for expected behavior.
    todo!()
}
```

### Critical Edge Cases

| Case | Expected Behavior |
|---|---|
| Single message, no reply | `is_missed = true`, `missed_by = Them`, `total_message_count = 1` |
| Two messages from me, no reply | Still one conversation. `is_missed = true`, `missed_by = Them` |
| Message at exactly `timeout` boundary | `gap > timeout` uses strict greater-than. Equal = same convo. |
| Daylight saving time transition | Use UTC timestamps everywhere. DST does not affect gap calculation. |
| Messages with identical timestamps | Stable sort by rowid. Both land in the same conversation. |
| Contact with zero messages | Return empty Vec. Don't create an empty conversation. |
| Very long conversation (1000+ messages, spans days) | One conversation as long as no gap exceeds timeout. This is correct — a busy day of texting is one conversation. |

---

## 5. Rust Analytics Engine

### Module: `aggregator.rs`

Responsible for populating `contact_analytics`, `activity_daily`, and `activity_hourly`.

```rust
/// Runs a single streaming pass over all messages for a contact.
/// Populates:
///   - contact_analytics (message counts, word counts, character counts)
///   - activity_daily (per-day message/word/media counts)
///   - activity_hourly (per day-of-week × hour counts)
///
/// This is a SINGLE PASS — do not query the messages table multiple times.
/// We stream messages in timestamp order and accumulate into HashMap accumulators,
/// then flush to SQLite in a single transaction at the end.
///
/// Memory budget: ~50MB max for a 600k message contact.
/// The HashMaps for daily (up to ~4500 entries for 12 years) and
/// hourly (7 × 24 = 168 entries) are trivially small.
pub fn compute_aggregates(
    db: &Connection,
    contact_id: &str,
) -> Result<(), AnalyticsError> {
    todo!()
}
```

**Implementation notes:**

1. Use a `SELECT id, timestamp, message_type, body, sender FROM messages WHERE contact_id = ? ORDER BY timestamp ASC` cursor.
2. For each message, compute:
   - `word_count`: Split on whitespace. Fast and good enough. Don't use NLP tokenization.
   - `character_count`: `body.len()` (UTF-8 byte count is fine — Mimoto likely uses chars, so use `body.chars().count()` instead).
   - `has_media`: Check the `attachments` relation or a `has_attachment` flag.
   - `day_of_week` and `hour`: Derive from timestamp using `chrono`. **Use the user's local timezone, not UTC.** Store timezone in `analytics_meta` or read from system.
3. Accumulate into:
   - `HashMap<String, DailyAccum>` keyed by `"YYYY-MM-DD"` string
   - `HashMap<(u8, u8), u32>` keyed by `(day_of_week, hour)`
   - Two `ContactAccum` structs (one for me, one for them)
4. After the loop, write all accumulators to their respective tables in one transaction.

### Module: `response_calculator.rs`

```rust
/// Computes response time metrics from the conversations table
/// and raw message timestamps.
///
/// For each conversation:
///   1. Identify "response pairs" — consecutive messages from different senders.
///   2. The time between them is the response time.
///   3. The first response in a conversation is the "first response time."
///
/// Outputs: Vectors of response times (mine and theirs) for statistical analysis.
///
/// We compute: mean, median, and rapid_response_pct.
/// Median is important because mean is skewed by overnight gaps within a convo.
pub fn compute_response_times(
    db: &Connection,
    contact_id: &str,
    config: &SegmentationConfig,
) -> Result<ResponseTimeResults, AnalyticsError> {
    todo!()
}

pub struct ResponseTimeResults {
    pub my_response_times_ms: Vec<i64>,
    pub their_response_times_ms: Vec<i64>,
    pub my_first_response_times_ms: Vec<i64>,
    pub their_first_response_times_ms: Vec<i64>,
}

impl ResponseTimeResults {
    /// Compute summary statistics from the raw vectors.
    pub fn summarize(&self) -> ResponseTimeSummary {
        // mean, median, rapid_pct for each category
        todo!()
    }
}

pub struct ResponseTimeSummary {
    pub my_avg_response_ms: i64,
    pub their_avg_response_ms: i64,
    pub my_median_response_ms: i64,
    pub their_median_response_ms: i64,
    pub my_rapid_response_pct: f64,
    pub their_rapid_response_pct: f64,
    pub my_avg_first_response_ms: i64,
    pub their_avg_first_response_ms: i64,
}
```

**Response pair identification algorithm:**

```
For each conversation C:
  Walk messages in timestamp order.
  prev_sender = None
  prev_time = None

  For each message M in C:
    if prev_sender is Some and M.sender != prev_sender:
      response_time = M.timestamp - prev_time
      if M.sender == Me:
        my_response_times.push(response_time)
        if this is the first response in C by Me:
          my_first_response_times.push(response_time)
      else:
        their_response_times.push(response_time)
        if this is the first response in C by Them:
          their_first_response_times.push(response_time)

    prev_sender = Some(M.sender)
    prev_time = Some(M.timestamp)
```

**Important**: "Double messages" (me sending 2+ in a row before they reply) should NOT count as response pairs. Only transitions between senders create a pair.

### Module: `emoji_extractor.rs`

```rust
use std::collections::HashMap;

/// Extracts emoji from a message body using Unicode property matching.
///
/// We do NOT use regex for this — it's too slow for 600k messages.
/// Instead, iterate over chars and check Unicode categories:
///   - Emoji_Presentation property
///   - Emoji_Modifier_Base + Modifier sequences
///   - Regional indicators (flags)
///   - ZWJ sequences (family emoji, skin tones)
///
/// Uses the `unicode-segmentation` and `unic-emoji-char` crates.
///
/// Returns a HashMap<String, u32> of emoji → count.
/// Multi-codepoint emoji (👨‍👩‍👧‍👦) are kept as a single key.
pub fn extract_emojis(body: &str) -> HashMap<String, u32> {
    todo!()
}

/// Aggregate emoji counts across all messages for a contact.
/// Returns top N emojis sorted by count descending.
pub fn top_emojis(counts: &HashMap<String, u32>, n: usize) -> Vec<EmojiCount> {
    todo!()
}

#[derive(Debug, Clone, Serialize)]
pub struct EmojiCount {
    pub emoji: String,
    pub count: u32,
}
```

**Crate recommendations:**
- `unic-emoji-char` for `is_emoji()` checks
- `unicode-segmentation` for grapheme cluster iteration (handles ZWJ sequences)

### Module: `pattern_matcher.rs`

```rust
/// Detects language patterns in message bodies using compiled regex.
///
/// IMPORTANT: Compile regex ONCE at init, reuse for all messages.
/// Use `lazy_static` or `once_cell::sync::Lazy` for the regex set.
///
/// Each pattern detector returns a bool (message matches or not).
/// The aggregator counts matches per contact.

use once_cell::sync::Lazy;
use regex::Regex;

// --- Laugh Detection ---
// Matches: lol, lmao, lmfao, rofl, haha, hahaha+, 😂, 🤣, 💀 (skull = dead/dying laughing)
// Case insensitive. Must be word boundary or standalone emoji.
static LAUGH_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(l(o)+l|lmao|lmfao|rofl|ha(ha)+|😂|🤣|💀)\b").unwrap()
});

pub fn is_laugh(body: &str) -> bool {
    LAUGH_RE.is_match(body)
}

// --- Apology Detection ---
// Matches: sorry, my bad, apologize, I apologise, my fault, forgive me
static APOLOGY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(sorry|my bad|apologi[zs]e|my fault|forgive me)\b").unwrap()
});

pub fn is_apology(body: &str) -> bool {
    APOLOGY_RE.is_match(body)
}

// --- Question Detection ---
// Simple: does the message end with a question mark?
// More nuanced: also detect "do you", "can you", "what", "where", "when",
// "why", "how", "who" at start of sentence.
// We use the simple version — ? is reliable and fast.
pub fn is_question(body: &str) -> bool {
    body.trim_end().ends_with('?')
}

// --- Encouragement Detection ---
// Matches: you got this, proud of you, believe in you, you can do it,
// good job, well done, nice work, keep it up, amazing, awesome
static ENCOURAGEMENT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(you got this|proud of you|believe in you|you can do it|good job|well done|nice work|keep it up|keep going|you're amazing|you're awesome|great job|that's awesome|that's amazing|hell yeah|let's go)\b"
    ).unwrap()
});

pub fn is_encouragement(body: &str) -> bool {
    ENCOURAGEMENT_RE.is_match(body)
}

// --- Link Detection ---
// Matches URLs: http(s)://, www., or common TLDs
static LINK_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(https?://|www\.)\S+").unwrap()
});

pub fn contains_link(body: &str) -> bool {
    LINK_RE.is_match(body)
}
```

### Module: `media_classifier.rs`

```rust
/// Classifies attachments into categories based on MIME type.
/// Used to populate the media stats in contact_analytics.
///
/// Categories:
///   Image  — image/jpeg, image/png, image/webp, image/heic, image/heif
///   Video  — video/mp4, video/3gpp, video/quicktime, video/webm
///   Audio  — audio/*, application/ogg
///   GIF    — image/gif (separated from images because Mimoto does)
///   Link   — Detected from message body, not MIME type (see pattern_matcher)
///
/// Called during the aggregation pass when an attachment is encountered.
pub fn classify_media(mime_type: &str) -> MediaCategory {
    match mime_type {
        m if m == "image/gif" => MediaCategory::Gif,
        m if m.starts_with("image/") => MediaCategory::Image,
        m if m.starts_with("video/") => MediaCategory::Video,
        m if m.starts_with("audio/") || m == "application/ogg" => MediaCategory::Audio,
        _ => MediaCategory::Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCategory {
    Image,
    Video,
    Audio,
    Gif,
    Other,
}
```

### Module: `flow_builder.rs`

```rust
/// Builds the Sankey diagram data from the conversations table.
///
/// The flow has 4 columns:
///   1. Started by: [Me, Them]
///   2. Conversation type: [Big Moment, Everyday, No Reply]
///   3. Major contributor: [Me, Them]
///   4. Final reply by: [Me, Them]
///
/// Each node has a label and a count.
/// Each link connects two nodes with a value (count of conversations
/// flowing through that path).
///
/// "No Reply" conversations (is_missed = true) terminate at column 2 —
/// they have no major contributor or final reply.
pub fn build_conversation_flow(
    conversations: &[Conversation],
) -> SankeyData {
    // Count each unique path through the 4 columns.
    // A path is: (started_by, category, major_contributor, final_reply_by)
    // For missed convos: (started_by, "no_reply", None, None)
    todo!()
}

#[derive(Debug, Clone, Serialize)]
pub struct SankeyData {
    pub nodes: Vec<SankeyNode>,
    pub links: Vec<SankeyLink>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SankeyNode {
    pub id: String,      // e.g., "started_me", "big_moment", "contrib_them"
    pub label: String,   // e.g., "Started by: Ben"
    pub value: u32,      // total count flowing through this node
    pub column: u8,      // 0-3 for positioning
}

#[derive(Debug, Clone, Serialize)]
pub struct SankeyLink {
    pub source: String,  // node id
    pub target: String,  // node id
    pub value: u32,      // count of conversations on this path
}
```

### Orchestrator: `mod.rs`

File: `src-tauri/src/analytics/mod.rs`

```rust
pub mod conversation_segmenter;
pub mod aggregator;
pub mod response_calculator;
pub mod emoji_extractor;
pub mod pattern_matcher;
pub mod media_classifier;
pub mod flow_builder;
mod error;

pub use error::AnalyticsError;

/// Master function: recomputes all Rust-side analytics for a contact.
/// Called by Tauri command `compute_analytics`.
///
/// Steps (order matters):
///   1. Delete existing rows for this contact from all analytics tables
///   2. Run conversation segmentation → write to `conversations`
///   3. Run aggregation pass → write to `contact_analytics`,
///      `activity_daily`, `activity_hourly`
///   4. Run response time calculation → update `pair_analytics`
///   5. Build conversation flow → update `pair_analytics.conversation_flow_json`
///   6. Mark `computed_at` timestamps
///   7. Signal Python sidecar to run its pass (scoring, insights, rating)
///
/// All writes are in a SINGLE TRANSACTION. If any step fails, nothing is committed.
pub fn compute_all_analytics(
    db: &Connection,
    contact_id: &str,
    python_rpc: &PythonSidecar,
) -> Result<(), AnalyticsError> {
    let config = load_config(db)?;

    let tx = db.transaction()?;

    // Step 1: Clean slate
    clear_analytics_for_contact(&tx, contact_id)?;

    // Step 2: Segment conversations
    let messages = stream_messages(&tx, contact_id)?;
    let conversations = conversation_segmenter::segment_conversations(
        contact_id, messages, &config
    );
    write_conversations(&tx, &conversations)?;

    // Step 3: Aggregate
    aggregator::compute_aggregates(&tx, contact_id)?;

    // Step 4: Response times
    let response_data = response_calculator::compute_response_times(
        &tx, contact_id, &config
    )?;
    let summary = response_data.summarize();
    write_response_summary(&tx, contact_id, &summary)?;

    // Step 5: Conversation flow
    let flow = flow_builder::build_conversation_flow(&conversations);
    write_conversation_flow(&tx, contact_id, &flow)?;

    // Step 6: Timestamp
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    update_computed_at(&tx, contact_id, now)?;

    tx.commit()?;

    // Step 7: Python pass (outside transaction — reads committed data)
    python_rpc.call("compute_python_analytics", json!({
        "contact_id": contact_id,
    }))?;

    Ok(())
}
```

---

## 6. Python Analytics Engine

All Python modules read from the pre-populated analytics tables. They do NOT touch raw messages.

### Module: `scoring.py`

```python
"""
Chat Points scoring system.

Points are a weighted engagement metric assigned per-message.
They're computed during the Rust aggregation pass (aggregator.rs)
using the weights defined here. The weights are exposed via JSON-RPC
so Rust can request them at startup.

This module also provides the per-day points rollup for the
Relationship Growth chart.
"""

# Weights are intentionally simple — users should intuitively understand
# why one person has more points. Avoid complex formulas.
POINT_WEIGHTS = {
    "text_message": 1,           # base: 1 point per message sent
    "per_word": 0.1,             # longer messages = more effort
    "emoji": 0.2,               # emoji add expressiveness
    "question": 0.5,            # questions drive engagement
    "image": 3,                  # media takes effort to share
    "video": 5,
    "audio": 4,
    "gif": 2,
    "link": 2,
    "started_convo": 5,         # initiating is high-value
    "rapid_response": 2,        # fast reply = engaged
    "encouragement": 3,         # emotional labor
    "long_message_bonus": 2,    # body > 200 chars
}

def get_weights() -> dict:
    """Exposed via JSON-RPC so Rust can use these during aggregation."""
    return POINT_WEIGHTS

def calculate_message_points(
    word_count: int,
    emoji_count: int,
    is_question: bool,
    media_type: str | None,  # 'image', 'video', 'audio', 'gif', or None
    is_convo_starter: bool,
    is_rapid_response: bool,
    is_encouragement: bool,
    char_count: int,
) -> int:
    """
    Calculate points for a single message.
    Called by Rust during aggregation via JSON-RPC.

    NOTE: This is called per-message (600k times). If JSON-RPC overhead
    is too high, export weights to Rust and compute natively.
    Benchmark first.
    """
    points = POINT_WEIGHTS["text_message"]
    points += word_count * POINT_WEIGHTS["per_word"]
    points += emoji_count * POINT_WEIGHTS["emoji"]

    if is_question:
        points += POINT_WEIGHTS["question"]
    if media_type and media_type in POINT_WEIGHTS:
        points += POINT_WEIGHTS[media_type]
    if is_convo_starter:
        points += POINT_WEIGHTS["started_convo"]
    if is_rapid_response:
        points += POINT_WEIGHTS["rapid_response"]
    if is_encouragement:
        points += POINT_WEIGHTS["encouragement"]
    if char_count > 200:
        points += POINT_WEIGHTS["long_message_bonus"]

    return int(points)
```

**Decision: Rust-native scoring vs JSON-RPC per message**

The spec above calls `calculate_message_points` per message. For 600k messages, JSON-RPC overhead (serialize → pipe → deserialize × 2) would add ~10-30 seconds. Instead:

**Preferred approach**: Python exports `get_weights()` once at startup. Rust caches the weights dict and computes points natively using the same formula. Python only provides the weights and the formula logic is duplicated in Rust. This is a deliberate tradeoff: minor code duplication for 100× better performance.

```rust
// In aggregator.rs — scoring logic duplicated from Python
fn calculate_points(msg: &MessageRef, weights: &PointWeights, is_convo_start: bool) -> u32 {
    let mut points = weights.text_message;
    points += msg.word_count as f64 * weights.per_word;
    // ... same logic as Python
    points as u32
}
```

### Module: `rating.py`

```python
"""
Chat Rating: composite score 0–100 representing relationship health.

Components (weighted):
  - Responsiveness (25%): How quickly both parties respond
  - Balance (20%): How evenly split is message volume and initiation
  - Engagement (20%): Average conversation length, media sharing
  - Consistency (15%): Regular communication pattern (low variance in daily activity)
  - Reciprocity (10%): Do both parties ask questions, share media, encourage?
  - Longevity (10%): Bonus for long-running relationships

Each component scores 0–100 independently, then weighted-averaged.
"""

from dataclasses import dataclass

@dataclass
class RatingBreakdown:
    responsiveness: int
    balance: int
    engagement: int
    consistency: int
    reciprocity: int
    longevity: int
    overall: int

def compute_rating(
    pair: dict,       # pair_analytics row as dict
    contact: dict,    # contact_analytics row as dict
    daily: list[dict] # activity_daily rows
) -> RatingBreakdown:
    """
    Main entry point. Reads pre-computed stats and produces the score.

    IMPORTANT: This is called ONCE per contact refresh, not per message.
    Performance is not a concern here.
    """
    r = _score_responsiveness(pair)
    b = _score_balance(pair, contact)
    e = _score_engagement(pair, contact)
    c = _score_consistency(daily)
    rec = _score_reciprocity(contact)
    lon = _score_longevity(pair)

    overall = int(
        r * 0.25 +
        b * 0.20 +
        e * 0.20 +
        c * 0.15 +
        rec * 0.10 +
        lon * 0.10
    )

    return RatingBreakdown(
        responsiveness=r,
        balance=b,
        engagement=e,
        consistency=c,
        reciprocity=rec,
        longevity=lon,
        overall=min(100, max(0, overall)),
    )


def _score_responsiveness(pair: dict) -> int:
    """
    100 = both respond rapidly (median < 5 min)
    50  = one side is slow (median > 1 hour)
    0   = both sides routinely take > 6 hours

    Uses median, not mean, to avoid skew from overnight messages.
    """
    my_med = pair.get("my_median_response_ms", 0) or 0
    their_med = pair.get("their_median_response_ms", 0) or 0
    avg_med = (my_med + their_med) / 2

    # Scale: 0ms → 100, 300_000ms (5 min) → 80, 3_600_000ms (1hr) → 50,
    # 21_600_000ms (6hr) → 0
    if avg_med <= 0:
        return 50  # no data
    if avg_med <= 300_000:
        return 100 - int((avg_med / 300_000) * 20)  # 100 → 80
    if avg_med <= 3_600_000:
        return 80 - int(((avg_med - 300_000) / 3_300_000) * 30)  # 80 → 50
    if avg_med <= 21_600_000:
        return 50 - int(((avg_med - 3_600_000) / 18_000_000) * 50)  # 50 → 0
    return 0


def _score_balance(pair: dict, contact: dict) -> int:
    """
    100 = perfectly balanced message count and initiation
    0   = one-sided (> 80/20 split)

    Combines message volume balance and initiation balance.
    """
    total_msgs = (
        contact.get("my_message_count", 0) +
        contact.get("their_message_count", 0)
    )
    if total_msgs == 0:
        return 50

    msg_ratio = min(
        contact.get("my_message_count", 0),
        contact.get("their_message_count", 0)
    ) / max(total_msgs / 2, 1)
    # msg_ratio: 1.0 = perfect, 0.0 = completely one-sided

    total_convos = (
        pair.get("convos_started_by_me", 0) +
        pair.get("convos_started_by_them", 0)
    )
    if total_convos > 0:
        init_ratio = min(
            pair.get("convos_started_by_me", 0),
            pair.get("convos_started_by_them", 0)
        ) / max(total_convos / 2, 1)
    else:
        init_ratio = 0.5

    combined = (msg_ratio * 0.6 + init_ratio * 0.4)
    return int(combined * 100)


def _score_engagement(pair: dict, contact: dict) -> int:
    """
    Based on average conversation length and media sharing frequency.
    100 = long conversations with rich media
    0   = short, text-only exchanges
    """
    avg_convo = pair.get("avg_convo_points", 0) or 0
    total_msgs = (
        contact.get("my_message_count", 0) +
        contact.get("their_message_count", 0)
    )
    total_media = sum([
        contact.get("my_image_count", 0),
        contact.get("their_image_count", 0),
        contact.get("my_video_count", 0),
        contact.get("their_video_count", 0),
    ])

    # Avg convo length scoring: 5 msgs = 30, 15 msgs = 60, 30+ msgs = 90
    convo_score = min(90, int(avg_convo * 3))

    # Media ratio: what % of messages include media?
    media_ratio = total_media / max(total_msgs, 1)
    media_score = min(100, int(media_ratio * 500))  # 20% media = 100

    return int(convo_score * 0.7 + media_score * 0.3)


def _score_consistency(daily: list[dict]) -> int:
    """
    Measures regularity of communication over time.
    100 = daily communication with low variance
    0   = long gaps, sporadic bursts

    Uses coefficient of variation of weekly message counts.
    """
    if len(daily) < 7:
        return 50  # not enough data

    # Group into weeks
    import statistics
    weekly_counts = []
    week_sum = 0
    for i, row in enumerate(daily):
        week_sum += row.get("my_messages", 0) + row.get("their_messages", 0)
        if (i + 1) % 7 == 0:
            weekly_counts.append(week_sum)
            week_sum = 0

    if len(weekly_counts) < 4:
        return 50

    mean = statistics.mean(weekly_counts)
    if mean == 0:
        return 0
    stdev = statistics.stdev(weekly_counts)
    cv = stdev / mean  # coefficient of variation

    # CV of 0 = perfectly consistent → 100
    # CV of 2+ = highly inconsistent → 0
    return max(0, min(100, int((1 - cv / 2) * 100)))


def _score_reciprocity(contact: dict) -> int:
    """
    Do both parties contribute equally across modalities?
    Compares: questions asked, media shared, encouragement given.
    """
    pairs = [
        (contact.get("my_question_count", 0), contact.get("their_question_count", 0)),
        (
            sum([contact.get(f"my_{t}_count", 0) for t in ["image", "video", "audio", "gif"]]),
            sum([contact.get(f"their_{t}_count", 0) for t in ["image", "video", "audio", "gif"]]),
        ),
        (contact.get("my_encouragement_count", 0), contact.get("their_encouragement_count", 0)),
    ]

    ratios = []
    for mine, theirs in pairs:
        total = mine + theirs
        if total > 10:  # need meaningful sample
            ratios.append(min(mine, theirs) / max(total / 2, 1))

    if not ratios:
        return 50
    return int(statistics.mean(ratios) * 100)


def _score_longevity(pair: dict) -> int:
    """
    Bonus for long-running relationships.
    1 year = 50, 5+ years = 100, < 6 months = 25
    """
    first = pair.get("first_message_at", 0) or 0
    last = pair.get("last_message_at", 0) or 0
    if first == 0 or last == 0:
        return 0

    years = (last - first) / (365.25 * 86400)
    if years >= 5:
        return 100
    if years >= 1:
        return 50 + int((years - 1) / 4 * 50)
    return max(0, int(years * 50))
```

### Module: `insights.py`

```python
"""
Key Insights engine.

Generates 8–12 natural-language observations about a relationship.
Each insight is rule-based with a magnitude score for sorting.

Insight structure:
{
    "icon": "⚡",
    "text": "You respond much faster than your contact.",
    "sentiment": "neutral",    // "positive", "negative", "neutral"
    "magnitude": 0.73          // 0.0–1.0, used for sorting
}
"""

def generate_insights(pair: dict, contact: dict) -> list[dict]:
    """
    Main entry. Runs all insight rules, sorts by magnitude, returns top 12.
    """
    insights = []
    insights.extend(_response_speed_insights(pair))
    insights.extend(_initiation_insights(pair))
    insights.extend(_volume_insights(contact))
    insights.extend(_laugh_insights(contact))
    insights.extend(_apology_insights(contact))
    insights.extend(_encouragement_insights(contact))
    insights.extend(_conversation_ending_insights(pair))
    insights.extend(_missed_convo_insights(pair))
    insights.extend(_reconnection_insights(pair))
    insights.extend(_double_message_insights(pair))
    insights.extend(_media_sharing_insights(contact))

    # Sort by magnitude descending, take top 12
    insights.sort(key=lambda i: i["magnitude"], reverse=True)
    return insights[:12]


# --- Individual rule functions ---
# Each returns a list of 0-2 insights (some rules generate nothing
# if the data doesn't meet the threshold).

def _response_speed_insights(pair: dict) -> list[dict]:
    my_avg = pair.get("my_avg_response_ms", 0) or 1
    their_avg = pair.get("their_avg_response_ms", 0) or 1
    ratio = my_avg / their_avg

    insights = []
    if ratio < 0.3:
        insights.append({
            "icon": "⚡",
            "text": "You respond much faster than your contact.",
            "sentiment": "neutral",
            "magnitude": min(1.0, 1.0 - ratio),
        })
    elif ratio < 0.7:
        insights.append({
            "icon": "⚡",
            "text": "You respond faster than your contact.",
            "sentiment": "neutral",
            "magnitude": min(1.0, 0.7 - ratio),
        })
    elif ratio > 3.0:
        insights.append({
            "icon": "⚡",
            "text": "Your contact responds much faster than you.",
            "sentiment": "neutral",
            "magnitude": min(1.0, ratio - 1.0) / 3,
        })
    elif ratio > 1.5:
        insights.append({
            "icon": "⚡",
            "text": "Your contact responds faster than you.",
            "sentiment": "neutral",
            "magnitude": min(1.0, ratio - 1.0) / 2,
        })

    # New conversation response speed
    my_first = pair.get("my_avg_first_response_ms", 0) or 1
    their_first = pair.get("their_avg_first_response_ms", 0) or 1
    first_ratio = my_first / their_first

    if first_ratio < 0.5:
        insights.append({
            "icon": "⚡",
            "text": "You respond faster than your contact when new conversations begin.",
            "sentiment": "neutral",
            "magnitude": min(1.0, 1.0 - first_ratio) * 0.8,
        })
    elif first_ratio > 2.0:
        insights.append({
            "icon": "⚡",
            "text": "Your contact responds faster than you when new conversations begin.",
            "sentiment": "neutral",
            "magnitude": min(1.0, first_ratio - 1.0) / 3 * 0.8,
        })

    return insights


def _initiation_insights(pair: dict) -> list[dict]:
    my_starts = pair.get("convos_started_by_me", 0)
    their_starts = pair.get("convos_started_by_them", 0)
    total = my_starts + their_starts

    if total < 20:
        return []  # not enough data

    my_pct = my_starts / total
    insights = []

    if my_pct > 0.65:
        insights.append({
            "icon": "🔥",
            "text": "Your started conversations generate more activity than your contact's.",
            "sentiment": "positive",
            "magnitude": my_pct - 0.5,
        })
    elif my_pct < 0.35:
        insights.append({
            "icon": "🔥",
            "text": "They initiate far more conversations than you do.",
            "sentiment": "neutral",
            "magnitude": 0.5 - my_pct,
        })

    return insights


def _volume_insights(contact: dict) -> list[dict]:
    mine = contact.get("my_message_count", 0)
    theirs = contact.get("their_message_count", 0)
    total = mine + theirs

    if total < 100:
        return []

    ratio = mine / max(theirs, 1)
    insights = []

    if 0.9 <= ratio <= 1.1:
        insights.append({
            "icon": "💬",
            "text": "You and your contact send almost the same number of messages.",
            "sentiment": "positive",
            "magnitude": 0.5,
        })
    elif ratio > 1.3:
        insights.append({
            "icon": "💬",
            "text": "You send more messages than your contact.",
            "sentiment": "neutral",
            "magnitude": min(1.0, (ratio - 1.0) / 2),
        })
    elif ratio < 0.7:
        insights.append({
            "icon": "💬",
            "text": "Your contact sends more messages than you.",
            "sentiment": "neutral",
            "magnitude": min(1.0, (1.0 / ratio - 1.0) / 2),
        })

    return insights


def _laugh_insights(contact: dict) -> list[dict]:
    mine = contact.get("my_laugh_count", 0)
    theirs = contact.get("their_laugh_count", 0)
    total = mine + theirs
    if total < 20:
        return []

    ratio = mine / max(theirs, 1)
    if ratio > 1.5:
        return [{"icon": "😂", "text": "You laugh more than your contact.",
                 "sentiment": "neutral", "magnitude": min(1.0, (ratio - 1) / 3)}]
    elif ratio < 0.67:
        return [{"icon": "😂", "text": "Your contact laughs more than you.",
                 "sentiment": "neutral", "magnitude": min(1.0, (1/ratio - 1) / 3)}]
    return []


def _apology_insights(contact: dict) -> list[dict]:
    mine = contact.get("my_apology_count", 0)
    theirs = contact.get("their_apology_count", 0)
    total = mine + theirs
    if total < 10:
        return []

    ratio = mine / max(theirs, 1)
    if ratio > 2.0:
        return [{"icon": "🙏", "text": "You apologize more than your contact.",
                 "sentiment": "neutral", "magnitude": min(1.0, (ratio - 1) / 4)}]
    elif ratio < 0.5:
        return [{"icon": "🙏", "text": "Your contact apologizes more than you.",
                 "sentiment": "neutral", "magnitude": min(1.0, (1/ratio - 1) / 4)}]
    return []


def _encouragement_insights(contact: dict) -> list[dict]:
    mine = contact.get("my_encouragement_count", 0)
    theirs = contact.get("their_encouragement_count", 0)
    total = mine + theirs
    if total < 10:
        return []

    ratio = mine / max(theirs, 1)
    if ratio > 1.5:
        return [{"icon": "👏", "text": "You send more encouragement than your contact.",
                 "sentiment": "positive", "magnitude": min(1.0, (ratio - 1) / 3)}]
    elif ratio < 0.67:
        return [{"icon": "👏", "text": "Your contact sends more encouragement than you.",
                 "sentiment": "positive", "magnitude": min(1.0, (1/ratio - 1) / 3)}]
    return []


def _conversation_ending_insights(pair: dict) -> list[dict]:
    # Based on who closes conversations (sends final message)
    my_closes = pair.get("convos_closed_by_me", 0)
    their_closes = pair.get("convos_closed_by_them", 0)
    total = my_closes + their_closes
    if total < 20:
        return []

    my_pct = my_closes / total
    if 0.4 <= my_pct <= 0.6:
        return [{"icon": "💜", "text": "Conversation endings are evenly shared.",
                 "sentiment": "positive", "magnitude": 0.4}]
    return []


def _missed_convo_insights(pair: dict) -> list[dict]:
    mine = pair.get("my_convos_missed", 0)
    theirs = pair.get("their_convos_missed", 0)
    total = mine + theirs
    if total < 5:
        return []

    if abs(mine - theirs) <= max(2, total * 0.15):
        return [{"icon": "💤", "text": "Your missed conversations are evenly matched.",
                 "sentiment": "neutral", "magnitude": 0.3}]
    return []


def _reconnection_insights(pair: dict) -> list[dict]:
    reconnects = pair.get("reconnects", 0)
    total_convos = pair.get("total_conversations", 1)

    if reconnects > 0 and reconnects / total_convos < 0.05:
        return [{"icon": "🔗", "text": "You rarely reconnect after long silences.",
                 "sentiment": "neutral", "magnitude": 0.3}]
    return []


def _double_message_insights(pair: dict) -> list[dict]:
    # "Double message" = sending 2+ messages before the other replies
    mine = pair.get("my_double_messages", 0)
    theirs = pair.get("their_double_messages", 0)
    total_convos = pair.get("total_conversations", 1)

    if mine > theirs * 2 and mine > 20:
        return [{"icon": "💬", "text": "You double-text much more often than your contact.",
                 "sentiment": "neutral", "magnitude": 0.5}]
    elif theirs > mine * 2 and theirs > 20:
        return [{"icon": "💬", "text": "Your contact double-texts much more than you.",
                 "sentiment": "neutral", "magnitude": 0.5}]
    return []


def _media_sharing_insights(contact: dict) -> list[dict]:
    my_media = sum(contact.get(f"my_{t}_count", 0) for t in ["image", "video", "audio", "gif"])
    their_media = sum(contact.get(f"their_{t}_count", 0) for t in ["image", "video", "audio", "gif"])
    total = my_media + their_media
    if total < 20:
        return []

    ratio = my_media / max(their_media, 1)
    if ratio > 2.0:
        return [{"icon": "📸", "text": "You share far more media than your contact.",
                 "sentiment": "neutral", "magnitude": min(1.0, (ratio - 1) / 4)}]
    elif ratio < 0.5:
        return [{"icon": "📸", "text": "Your contact shares far more media than you.",
                 "sentiment": "neutral", "magnitude": min(1.0, (1/ratio - 1) / 4)}]
    return []
```

### Module: `topic_classifier.py`

```python
"""
Direction of Conversation — classifies who the conversation is ABOUT.

Three categories:
  - "me" (first person: I, me, my, mine, myself)
  - "them" (second person: you, your, yours, yourself)
  - "other" (third person: he, she, they, [names], or neutral topics)

Strategy: Rule-based pronoun counting.
Each message gets classified by which pronoun category dominates.
Messages with no clear pronoun dominance → "other".

This runs in Python over raw message bodies (not pre-aggregated),
but only needs a single pass and is pure string operations — fast enough.
"""

import re
from collections import Counter

FIRST_PERSON = re.compile(r"\b(i|me|my|mine|myself|i'm|i've|i'll|i'd)\b", re.I)
SECOND_PERSON = re.compile(r"\b(you|your|yours|yourself|you're|you've|you'll|you'd)\b", re.I)

def classify_message(body: str) -> str:
    """Returns 'me', 'them', or 'other'."""
    first = len(FIRST_PERSON.findall(body))
    second = len(SECOND_PERSON.findall(body))

    if first == 0 and second == 0:
        return "other"
    if first > second:
        return "me"
    if second > first:
        return "them"
    return "other"  # tie → neutral


def compute_chat_focus(db_path: str, contact_id: str) -> dict:
    """
    Iterates all messages for a contact, classifies each,
    returns percentage breakdown.

    Returns: {"me_pct": 0.47, "them_pct": 0.37, "other_pct": 0.16}
    """
    import sqlite3
    conn = sqlite3.connect(db_path)
    cursor = conn.execute(
        "SELECT body FROM messages WHERE contact_id = ? AND body != ''",
        (contact_id,)
    )

    counts = Counter()
    total = 0
    for (body,) in cursor:
        category = classify_message(body)
        counts[category] += 1
        total += 1

    conn.close()

    if total == 0:
        return {"me_pct": 0.33, "them_pct": 0.33, "other_pct": 0.34}

    return {
        "me_pct": round(counts["me"] / total, 4),
        "them_pct": round(counts["them"] / total, 4),
        "other_pct": round(counts["other"] / total, 4),
    }
```

### Module: `writing_milestones.py`

```python
"""
Writing milestones — compare total writing output to known works.

Milestones are a list of reference works with word counts.
The user's total word count is compared to show progress through the list.
"""

MILESTONES = [
    {"title": "Harry Potter and the Philosopher's Stone", "words": 77_325, "icon": "📖"},
    {"title": "Harry Potter and the Chamber of Secrets", "words": 84_799, "icon": "📖"},
    {"title": "Harry Potter and the Prisoner of Azkaban", "words": 107_253, "icon": "📖"},
    {"title": "Harry Potter and the Goblet of Fire", "words": 190_637, "icon": "📖"},
    {"title": "Harry Potter and the Order of the Phoenix", "words": 257_045, "icon": "📖"},
    {"title": "Harry Potter and the Half-Blood Prince", "words": 168_923, "icon": "📖"},
    {"title": "Harry Potter and the Deathly Hallows", "words": 198_227, "icon": "📖"},
    # Cumulative: 1,084,209 words for full series
]

# Average typing speed for casual texting: ~40 WPM
# Average word length: ~5 chars + 1 space = 6 chars
CHARS_PER_WORD_AVG = 6
TYPING_WPM_ESTIMATE = 40

def compute_milestones(total_chars: int, total_words: int) -> dict:
    """
    Returns milestone progress and estimated typing time.
    """
    cumulative = 0
    completed = []
    for ms in MILESTONES:
        cumulative += ms["words"]
        if total_words >= cumulative:
            completed.append({**ms, "status": "complete"})
        else:
            # Partially complete
            prev_cumulative = cumulative - ms["words"]
            progress = (total_words - prev_cumulative) / ms["words"]
            completed.append({
                **ms,
                "status": "in_progress",
                "progress_pct": round(max(0, progress), 4),
            })
            # Remaining milestones are incomplete
            remaining_idx = MILESTONES.index(ms) + 1
            for remaining in MILESTONES[remaining_idx:]:
                completed.append({**remaining, "status": "incomplete"})
            break
    else:
        # All milestones complete — user has written more than the full series
        pass

    typing_hours = (total_words / TYPING_WPM_ESTIMATE) / 60

    return {
        "total_chars": total_chars,
        "total_words": total_words,
        "typing_time_estimate_hrs": round(typing_hours, 1),
        "milestones": completed,
        "series_completion_pct": round(
            min(1.0, total_words / 1_084_209), 4
        ),
    }
```

---

## 7. Tauri IPC API Contract

File: `src-tauri/src/commands/analytics.rs`

```rust
/// Recompute all analytics for a contact.
/// Shows progress via Tauri events.
///
/// Emits:
///   "analytics-progress" → { phase: string, pct: number }
///   Phases: "segmenting", "aggregating", "response_times", "python_pass"
///
/// Returns: () on success, error string on failure.
#[tauri::command]
pub async fn compute_analytics(
    contact_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    // Spawn on blocking thread pool — this does heavy I/O
    todo!()
}

/// Fetch cached pair analytics for a contact.
/// Returns null if not yet computed.
#[tauri::command]
pub async fn get_pair_analytics(
    contact_id: String,
    state: State<'_, AppState>,
) -> Result<Option<PairAnalyticsResponse>, String> {
    todo!()
}

/// Fetch cached contact analytics.
#[tauri::command]
pub async fn get_contact_analytics(
    contact_id: String,
    state: State<'_, AppState>,
) -> Result<Option<ContactAnalyticsResponse>, String> {
    todo!()
}

/// Fetch daily activity time series for charts.
/// Optional date range filter.
#[tauri::command]
pub async fn get_daily_activity(
    contact_id: String,
    start_date: Option<String>,  // "YYYY-MM-DD"
    end_date: Option<String>,
    state: State<'_, AppState>,
) -> Result<Vec<DailyActivityRow>, String> {
    todo!()
}

/// Fetch hourly heatmap data.
#[tauri::command]
pub async fn get_hourly_heatmap(
    contact_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<HourlyHeatmapRow>, String> {
    todo!()
}

/// Fetch conversation list for a contact (used by flow + conversation analysis).
#[tauri::command]
pub async fn get_conversations(
    contact_id: String,
    limit: Option<u32>,
    offset: Option<u32>,
    state: State<'_, AppState>,
) -> Result<Vec<ConversationRow>, String> {
    todo!()
}

/// Check if analytics are fresh for a contact.
/// Returns the computed_at timestamp (0 = stale/never computed).
#[tauri::command]
pub async fn get_analytics_freshness(
    contact_id: String,
    state: State<'_, AppState>,
) -> Result<i64, String> {
    todo!()
}
```

Register all commands in `main.rs`:
```rust
.invoke_handler(tauri::generate_handler![
    // ... existing commands ...
    analytics::compute_analytics,
    analytics::get_pair_analytics,
    analytics::get_contact_analytics,
    analytics::get_daily_activity,
    analytics::get_hourly_heatmap,
    analytics::get_conversations,
    analytics::get_analytics_freshness,
])
```

---

## 8. TypeScript Types

File: `src/types/analytics.ts`

```typescript
// ============================================================
// Core enums
// ============================================================

export type Participant = "me" | "them";
export type InsightSentiment = "positive" | "negative" | "neutral";
export type ConversationCategory = "big_moment" | "everyday";

// ============================================================
// API Response types (match Rust serialization)
// ============================================================

export interface ContactAnalytics {
  contact_id: string;
  computed_at: number;

  // Message volume (per participant)
  my_message_count: number;
  their_message_count: number;
  my_word_count: number;
  their_word_count: number;
  my_unique_word_count: number;
  their_unique_word_count: number;
  my_character_count: number;
  their_character_count: number;

  // Media stats
  my_image_count: number;
  their_image_count: number;
  my_video_count: number;
  their_video_count: number;
  my_audio_count: number;
  their_audio_count: number;
  my_gif_count: number;
  their_gif_count: number;
  my_link_count: number;
  their_link_count: number;

  // Language patterns
  my_top_emojis: EmojiCount[];
  their_top_emojis: EmojiCount[];
  my_emoji_total: number;
  their_emoji_total: number;
  my_laugh_count: number;
  their_laugh_count: number;
  my_apology_count: number;
  their_apology_count: number;
  my_question_count: number;
  their_question_count: number;
  my_encouragement_count: number;
  their_encouragement_count: number;
}

export interface EmojiCount {
  emoji: string;
  count: number;
}

export interface PairAnalytics {
  contact_id: string;
  computed_at: number;

  // Conversation stats
  total_conversations: number;
  convos_started_by_me: number;
  convos_started_by_them: number;
  convos_closed_by_me: number;
  convos_closed_by_them: number;
  top_contributor: Participant | null;
  avg_convo_points: number;
  reconnects: number;
  my_double_messages: number;
  their_double_messages: number;
  my_convos_missed: number;
  their_convos_missed: number;

  // Response times (milliseconds)
  my_avg_response_ms: number | null;
  their_avg_response_ms: number | null;
  my_median_response_ms: number | null;
  their_median_response_ms: number | null;
  my_rapid_response_pct: number | null;
  their_rapid_response_pct: number | null;
  my_avg_first_response_ms: number | null;
  their_avg_first_response_ms: number | null;

  // Balance & Rating
  my_points: number;
  their_points: number;
  balance_ratio: number | null;
  overall_score: number | null;
  score_breakdown: RatingBreakdown | null;

  // Insights
  insights: Insight[];

  // Chat focus
  focus_me_pct: number | null;
  focus_them_pct: number | null;
  focus_other_pct: number | null;

  // Writing milestones
  writing_milestones: WritingMilestones | null;

  // Conversation flow
  conversation_flow: SankeyData | null;

  // Time span
  first_message_at: number | null;
  last_message_at: number | null;
}

export interface RatingBreakdown {
  responsiveness: number;
  balance: number;
  engagement: number;
  consistency: number;
  reciprocity: number;
  longevity: number;
  overall: number;
}

export interface Insight {
  icon: string;
  text: string;
  sentiment: InsightSentiment;
  magnitude: number;
}

export interface WritingMilestones {
  total_chars: number;
  total_words: number;
  typing_time_estimate_hrs: number;
  milestones: MilestoneItem[];
  series_completion_pct: number;
}

export interface MilestoneItem {
  title: string;
  words: number;
  icon: string;
  status: "complete" | "in_progress" | "incomplete";
  progress_pct?: number;
}

export interface SankeyData {
  nodes: SankeyNode[];
  links: SankeyLink[];
}

export interface SankeyNode {
  id: string;
  label: string;
  value: number;
  column: number;
}

export interface SankeyLink {
  source: string;
  target: string;
  value: number;
}

export interface DailyActivity {
  day: string;            // "YYYY-MM-DD"
  my_messages: number;
  their_messages: number;
  my_words: number;
  their_words: number;
  my_media: number;
  their_media: number;
  my_points: number;
  their_points: number;
}

export interface HourlyHeatmap {
  day_of_week: number;    // 0=Sun, 6=Sat
  hour: number;           // 0-23
  message_count: number;
}

export interface ConversationRow {
  id: number;
  contact_id: string;
  start_time: number;
  end_time: number;
  started_by: Participant;
  final_reply_by: Participant;
  my_message_count: number;
  their_message_count: number;
  total_message_count: number;
  major_contributor: Participant;
  is_missed: boolean;
  missed_by: Participant | null;
  category: ConversationCategory;
}
```

---

## 9. React Component Architecture

### Layout

The dashboard is a single scrollable page with a contact selector at the top. Grid layout matches Mimoto's 3-column structure on desktop, collapses to 1-column on mobile.

```
┌─────────────────────────────────────────────────────────────────┐
│  [Contact Selector ▼]                      [Refresh Analytics]  │
├─────────────────────────────────────────────────────────────────┤
│  HeaderStats (full width)                                       │
├─────────────────────────────────────────────────────────────────┤
│  RelationshipGrowthChart (full width)                           │
├──────────────────┬──────────────────┬───────────────────────────┤
│  ChatRating      │  KeyInsights     │  LanguageAnalysis         │
├──────────────────┤                  │                           │
│  BalanceBar      │                  │                           │
├──────────────────┤                  │                           │
│  WritingSummary  │                  │                           │
├──────────────────┼──────────────────┼───────────────────────────┤
│  MessagingHeatmap│  MessageAnalysis │  MediaStats               │
├──────────────────┤  ResponseStats   │  ConversationAnalysis     │
│  ChatFocusPie    │                  │                           │
├──────────────────┴──────────────────┴───────────────────────────┤
│  ConversationFlow (full width)                                  │
├─────────────────────────────────────────────────────────────────┤
│  ActivityHeatmap (full width)                                   │
└─────────────────────────────────────────────────────────────────┘
```

### Component List

File: `src/components/Analytics/AnalyticsDashboard.tsx`
- Top-level container
- Manages contact selection state
- Calls `useAnalytics(contactId)` hook
- Shows loading skeleton during computation
- Grid layout with CSS Grid

File: `src/components/Analytics/HeaderStats.tsx`
- Props: `{ totalPoints, timePeriod, totalMessages, totalConversations, contactName }`
- 4 stat cards in a row

File: `src/components/Analytics/RelationshipGrowthChart.tsx`
- Props: `{ dailyActivity: DailyActivity[] }`
- Recharts `<AreaChart>` with two series (my points, their points)
- Cumulative sum over time
- X-axis: dates. Y-axis: cumulative points

File: `src/components/Analytics/ChatRating.tsx`
- Props: `{ score: number, breakdown: RatingBreakdown }`
- Circular SVG gauge (0-100)
- Label: "A great relationship" / "Good" / "Needs attention"
- Click to expand breakdown

File: `src/components/Analytics/BalanceBar.tsx`
- Props: `{ myPoints, theirPoints, myName, theirName }`
- Horizontal stacked bar (50/50 split = green, skewed = yellow/red)

File: `src/components/Analytics/WritingSummary.tsx`
- Props: `{ milestones: WritingMilestones }`
- Row of book cover placeholders with checkmarks
- Total chars, estimated typing time

File: `src/components/Analytics/MessagingHeatmap.tsx`
- Props: `{ data: HourlyHeatmap[] }`
- 7×24 grid (rows = days, cols = hours)
- Color intensity = message count
- Custom SVG or d3 — Recharts doesn't have a native heatmap

File: `src/components/Analytics/ChatFocusPie.tsx`
- Props: `{ mePct, themPct, otherPct, myName, theirName }`
- Recharts `<PieChart>` with `innerRadius` (donut)

File: `src/components/Analytics/KeyInsights.tsx`
- Props: `{ insights: Insight[] }`
- Scrollable list of insight cards with icons

File: `src/components/Analytics/LanguageAnalysis.tsx`
- Props: `{ contactAnalytics: ContactAnalytics, myName, theirName }`
- Emoji table (top 5 per person)
- Stat rows: emoji total, laughs, apologies, questions, encouragement

File: `src/components/Analytics/MessageAnalysis.tsx`
- Props: `{ contactAnalytics: ContactAnalytics, myName, theirName }`
- Side-by-side stat cards: messages, words, unique words, characters

File: `src/components/Analytics/ResponseStats.tsx`
- Props: `{ pairAnalytics: PairAnalytics, myName, theirName }`
- Rapid 1st response %, avg 1st response, avg response time

File: `src/components/Analytics/ConversationFlow.tsx`
- Props: `{ data: SankeyData }`
- d3-sankey visualization
- Library: `d3-sankey` (install: `npm install d3-sankey @types/d3-sankey`)

File: `src/components/Analytics/ConversationAnalysis.tsx`
- Props: `{ pairAnalytics: PairAnalytics, myName, theirName }`
- Grid of stat pairs: convos started/closed, top contributor, etc.

File: `src/components/Analytics/MediaStats.tsx`
- Props: `{ contactAnalytics: ContactAnalytics, myName, theirName }`
- Table: images, videos, audios, GIFs, links per person

File: `src/components/Analytics/ActivityHeatmap.tsx`
- Props: `{ dailyActivity: DailyActivity[], dayCount?: number }`
- GitHub contribution–style calendar grid
- Rows = days of week, columns = weeks
- Default: last 500 days

### Data Fetching Hook

File: `src/hooks/useAnalytics.ts`

```typescript
import { invoke } from "@tauri-apps/api/tauri";
import { listen } from "@tauri-apps/api/event";
import { useState, useEffect, useCallback } from "react";

interface AnalyticsState {
  loading: boolean;
  progress: { phase: string; pct: number } | null;
  pair: PairAnalytics | null;
  contact: ContactAnalytics | null;
  daily: DailyActivity[];
  hourly: HourlyHeatmap[];
  error: string | null;
}

export function useAnalytics(contactId: string | null): AnalyticsState & {
  refresh: () => Promise<void>;
} {
  const [state, setState] = useState<AnalyticsState>({
    loading: false,
    progress: null,
    pair: null,
    contact: null,
    daily: [],
    hourly: [],
    error: null,
  });

  const load = useCallback(async () => {
    if (!contactId) return;

    setState((s) => ({ ...s, loading: true, error: null }));

    try {
      // Check freshness first
      const freshness = await invoke<number>("get_analytics_freshness", {
        contactId,
      });

      if (freshness === 0) {
        // Need to compute — listen for progress events
        const unlisten = await listen<{ phase: string; pct: number }>(
          "analytics-progress",
          (event) => {
            setState((s) => ({ ...s, progress: event.payload }));
          }
        );

        await invoke("compute_analytics", { contactId });
        unlisten();
      }

      // Fetch all cached data in parallel
      const [pair, contact, daily, hourly] = await Promise.all([
        invoke<PairAnalytics>("get_pair_analytics", { contactId }),
        invoke<ContactAnalytics>("get_contact_analytics", { contactId }),
        invoke<DailyActivity[]>("get_daily_activity", { contactId }),
        invoke<HourlyHeatmap[]>("get_hourly_heatmap", { contactId }),
      ]);

      setState({
        loading: false,
        progress: null,
        pair,
        contact,
        daily,
        hourly,
        error: null,
      });
    } catch (err) {
      setState((s) => ({
        ...s,
        loading: false,
        error: String(err),
      }));
    }
  }, [contactId]);

  useEffect(() => {
    load();
  }, [load]);

  const refresh = useCallback(async () => {
    if (!contactId) return;
    // Force recompute by setting computed_at to 0
    await invoke("compute_analytics", { contactId });
    await load();
  }, [contactId, load]);

  return { ...state, refresh };
}
```

---

## 10. Visualization Specifications

### Chart Library Choices

| Component | Library | Why |
|---|---|---|
| Relationship Growth (area) | Recharts | Native React, handles time series well |
| Chat Rating (gauge) | Custom SVG | Simple arc — no library needed |
| Balance Bar | Custom div | CSS progress bar — trivial |
| Messaging Heatmap | Custom SVG or d3 | Recharts has no heatmap. d3 is overkill. Use SVG `<rect>` grid. |
| Chat Focus (donut) | Recharts PieChart | Built-in `innerRadius` |
| Conversation Flow (sankey) | d3-sankey + SVG | Only good option. Recharts has no sankey. |
| Activity Heatmap (calendar) | Custom SVG | Same technique as messaging heatmap, different layout |
| All stat cards | Plain HTML/CSS | No chart library needed |

### Color Palette (Dark Theme)

Match Mimoto's dark aesthetic:

```css
:root {
  --bg-primary: #1a1b2e;       /* main background */
  --bg-card: #242540;           /* card/section background */
  --bg-card-alt: #2a2b48;      /* alternate card bg */
  --text-primary: #e8e8f0;     /* main text */
  --text-secondary: #8b8ca0;   /* muted text */
  --accent-green: #4ade80;     /* positive indicators */
  --accent-blue: #60a5fa;      /* "me" color */
  --accent-purple: #a78bfa;    /* "them" color */
  --accent-teal: #2dd4bf;      /* highlight/CTA */
  --accent-red: #f87171;       /* warning */
  --accent-yellow: #fbbf24;    /* caution */
  --border: #3a3b58;           /* card borders */
}
```

### Formatting Utilities

File: `src/utils/analyticsFormatters.ts`

```typescript
/** Format milliseconds as "Xm Ys" or "Xh Ym" */
export function formatDuration(ms: number): string {
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainSeconds = seconds % 60;
  if (minutes < 60) return `${minutes}:${String(remainSeconds).padStart(2, "0")}`;
  const hours = Math.floor(minutes / 60);
  const remainMinutes = minutes % 60;
  return `${hours}:${String(remainMinutes).padStart(2, "0")}:${String(remainSeconds).padStart(2, "0")}`;
}

/** Format large numbers with commas: 153162 → "153,162" */
export function formatNumber(n: number): string {
  return n.toLocaleString();
}

/** Format percentage: 0.73 → "73%" */
export function formatPct(ratio: number): string {
  return `${Math.round(ratio * 100)}%`;
}

/** Score label: 81 → "A great relationship" */
export function scoreLabel(score: number): string {
  if (score >= 80) return "A great relationship";
  if (score >= 60) return "A good relationship";
  if (score >= 40) return "An okay relationship";
  if (score >= 20) return "Needs attention";
  return "Distant";
}
```

---

## 11. Chat Rating — Composite Scoring Algorithm

See `rating.py` in Section 6 for full implementation.

Summary of component weights:

| Component | Weight | What It Measures |
|---|---|---|
| Responsiveness | 25% | How fast both sides reply (median response time) |
| Balance | 20% | Evenness of message volume + initiation |
| Engagement | 20% | Avg convo length + media sharing rate |
| Consistency | 15% | Regularity of communication (low weekly variance) |
| Reciprocity | 10% | Both sides ask questions, share media, encourage |
| Longevity | 10% | Duration of the relationship (bonus for 5+ years) |

---

## 12. Key Insights — Rule Engine

See `insights.py` in Section 6 for full implementation.

Rule categories:

| Rule | Threshold | Example Output |
|---|---|---|
| Response speed | ratio < 0.3 or > 3.0 | "You respond much faster than your contact." |
| First response speed | ratio < 0.5 or > 2.0 | "You respond faster when new conversations begin." |
| Initiation balance | > 65% or < 35% | "They initiate far more conversations than you do." |
| Message volume | ratio > 1.3 or < 0.7 | "You send more messages than your contact." |
| Laugh asymmetry | ratio > 1.5 or < 0.67 | "You laugh more than your contact." |
| Apology asymmetry | ratio > 2.0 or < 0.5 | "You apologize more than your contact." |
| Encouragement | ratio > 1.5 or < 0.67 | "You send more encouragement than your contact." |
| Conversation endings | 40-60% balance | "Conversation endings are evenly shared." |
| Missed convos | within 15% | "Your missed conversations are evenly matched." |
| Reconnection | < 5% of convos | "You rarely reconnect after long silences." |
| Double messaging | > 2× ratio | "You double-text much more often than your contact." |
| Media sharing | ratio > 2.0 or < 0.5 | "Your contact shares far more media than you." |

---

## 13. Performance Budget & Optimization

### Targets

| Operation | Target | Strategy |
|---|---|---|
| Full analytics compute (600k msgs) | < 5 seconds | Single-pass Rust streaming, bulk SQL inserts |
| Dashboard load (cached) | < 200ms | 4 parallel Tauri IPC calls, all from indexed tables |
| Chart render (React) | < 100ms | Memoized components, virtualized if > 5000 data points |
| Python pass (scoring + insights) | < 2 seconds | Reads aggregated rows, not raw messages (except topic classifier) |

### Optimization Rules

1. **Single-pass aggregation**: The Rust aggregator iterates messages ONCE. Every metric (word count, emoji count, pattern matching, media classification, daily/hourly bucketing) is computed in the same loop. No second pass.

2. **Streaming cursor**: Use `rusqlite::Statement::query_map` with a cursor. Do NOT `collect()` into a Vec. Process each row and accumulate into HashMaps.

3. **Bulk inserts**: After the aggregation pass, write all results in a single transaction using `INSERT OR REPLACE` with prepared statements. Batch 1000 rows per `execute_batch` call.

4. **JSON column reads**: When the frontend reads `insights_json` or `score_breakdown`, Rust deserializes the JSON string and returns it as a typed struct. The frontend never parses JSON strings itself — Tauri's serialization handles it.

5. **Lazy computation**: Don't compute analytics for all contacts on import. Only compute when the user navigates to a contact's analytics page. Background computation for top 10 most-messaged contacts is acceptable as a post-import optimization.

6. **Chart data point limits**: The Relationship Growth chart may have 4000+ daily data points for a 12-year relationship. Downsample to weekly or monthly for initial render, load full resolution on zoom. Recharts handles this natively with `<Brush>` for range selection.

7. **Memoize components**: Every visualization component should be wrapped in `React.memo` with a shallow comparison on its props. The `useAnalytics` hook returns stable references (fetched once, never mutated).

### Memory Budget

| Data Structure | Max Size | Notes |
|---|---|---|
| `HashMap<String, DailyAccum>` | ~4500 entries × 64 bytes = ~280KB | 12 years of daily data |
| `HashMap<(u8,u8), u32>` | 168 entries × 12 bytes = ~2KB | 7 days × 24 hours |
| `HashMap<String, u32>` (emoji) | ~500 unique emoji × 20 bytes = ~10KB | Per participant |
| `Vec<i64>` (response times) | ~300k entries × 8 bytes = ~2.4MB | Worst case: every message is a response |
| Conversation structs | ~50k convos × 120 bytes = ~6MB | For a very active 12-year relationship |
| **Total peak** | **< 15MB** | Well within budget |

---

## 14. Test Plan

### 14.1 Unit Tests — Rust

File: `src-tauri/src/analytics/tests/`

#### Conversation Segmenter Tests

```rust
#[cfg(test)]
mod segmenter_tests {
    use super::*;

    fn msg(id: i64, ts: i64, sender: Participant) -> MessageRef {
        MessageRef { id, timestamp: ts, sender, word_count: 5, has_media: false }
    }

    fn default_config() -> SegmentationConfig {
        SegmentationConfig {
            conversation_timeout_secs: 14400, // 4 hours
            big_moment_threshold: 20,
            reconnect_threshold_secs: 86400,
        }
    }

    #[test]
    fn test_empty_messages() {
        // Zero messages → zero conversations
        let convos = segment_conversations("c1", std::iter::empty(), &default_config());
        assert_eq!(convos.len(), 0);
    }

    #[test]
    fn test_single_message_is_missed_convo() {
        // One message from me, no reply → missed conversation
        let msgs = vec![msg(1, 1000, Participant::Me)];
        let convos = segment_conversations("c1", msgs.into_iter(), &default_config());
        assert_eq!(convos.len(), 1);
        assert!(convos[0].is_missed);
        assert_eq!(convos[0].missed_by, Some(Participant::Them));
        assert_eq!(convos[0].started_by, Participant::Me);
        assert_eq!(convos[0].total_message_count, 1);
    }

    #[test]
    fn test_basic_back_and_forth() {
        // Simple exchange within timeout → one conversation
        let msgs = vec![
            msg(1, 1000, Participant::Me),
            msg(2, 1060, Participant::Them),
            msg(3, 1120, Participant::Me),
        ];
        let convos = segment_conversations("c1", msgs.into_iter(), &default_config());
        assert_eq!(convos.len(), 1);
        assert!(!convos[0].is_missed);
        assert_eq!(convos[0].started_by, Participant::Me);
        assert_eq!(convos[0].final_reply_by, Participant::Me);
        assert_eq!(convos[0].my_message_count, 2);
        assert_eq!(convos[0].their_message_count, 1);
        assert_eq!(convos[0].major_contributor, Participant::Me);
    }

    #[test]
    fn test_timeout_splits_conversations() {
        // Gap > 4 hours → two conversations
        let msgs = vec![
            msg(1, 1000, Participant::Me),
            msg(2, 1060, Participant::Them),
            // 5 hour gap (18000 seconds)
            msg(3, 19060, Participant::Them),
            msg(4, 19120, Participant::Me),
        ];
        let convos = segment_conversations("c1", msgs.into_iter(), &default_config());
        assert_eq!(convos.len(), 2);
        assert_eq!(convos[0].started_by, Participant::Me);
        assert_eq!(convos[1].started_by, Participant::Them);
    }

    #[test]
    fn test_exact_timeout_boundary_stays_in_same_convo() {
        // Gap == exactly timeout → same conversation (strict greater-than)
        let timeout = 14400;
        let msgs = vec![
            msg(1, 1000, Participant::Me),
            msg(2, 1000 + timeout, Participant::Them),
        ];
        let convos = segment_conversations("c1", msgs.into_iter(), &default_config());
        assert_eq!(convos.len(), 1); // NOT split
    }

    #[test]
    fn test_big_moment_categorization() {
        // 25 messages → big_moment
        let mut msgs: Vec<MessageRef> = (0..25)
            .map(|i| msg(i, 1000 + i * 60, if i % 2 == 0 { Participant::Me } else { Participant::Them }))
            .collect();
        let convos = segment_conversations("c1", msgs.into_iter(), &default_config());
        assert_eq!(convos.len(), 1);
        assert_eq!(convos[0].category, ConversationCategory::BigMoment);
    }

    #[test]
    fn test_everyday_categorization() {
        // 5 messages → everyday
        let msgs: Vec<MessageRef> = (0..5)
            .map(|i| msg(i, 1000 + i * 60, if i % 2 == 0 { Participant::Me } else { Participant::Them }))
            .collect();
        let convos = segment_conversations("c1", msgs.into_iter(), &default_config());
        assert_eq!(convos.len(), 1);
        assert_eq!(convos[0].category, ConversationCategory::Everyday);
    }

    #[test]
    fn test_identical_timestamps_stable_order() {
        // Two messages at same timestamp → same conversation, order by id
        let msgs = vec![
            msg(1, 1000, Participant::Me),
            msg(2, 1000, Participant::Them),
        ];
        let convos = segment_conversations("c1", msgs.into_iter(), &default_config());
        assert_eq!(convos.len(), 1);
        assert_eq!(convos[0].started_by, Participant::Me); // lower id first
    }

    #[test]
    fn test_one_sided_convo_from_them() {
        // They send 3 messages, I never reply
        let msgs = vec![
            msg(1, 1000, Participant::Them),
            msg(2, 1060, Participant::Them),
            msg(3, 1120, Participant::Them),
        ];
        let convos = segment_conversations("c1", msgs.into_iter(), &default_config());
        assert_eq!(convos.len(), 1);
        assert!(convos[0].is_missed);
        assert_eq!(convos[0].missed_by, Some(Participant::Me));
    }

    #[test]
    fn test_many_conversations_across_days() {
        // Simulate 3 days of texting with gaps
        let day = 86400i64;
        let msgs = vec![
            // Day 1 morning
            msg(1, day * 0 + 36000, Participant::Me),    // 10:00 AM
            msg(2, day * 0 + 36300, Participant::Them),   // 10:05 AM
            // Day 1 evening (gap < 4h, same convo)
            msg(3, day * 0 + 46800, Participant::Me),    // 1:00 PM
            msg(4, day * 0 + 46860, Participant::Them),   // 1:01 PM
            // Day 2 morning (gap > 4h, new convo)
            msg(5, day * 1 + 36000, Participant::Them),   // next day 10:00 AM
            msg(6, day * 1 + 36060, Participant::Me),    // 10:01 AM
            // Day 3 (gap > 4h, new convo)
            msg(7, day * 2 + 36000, Participant::Me),    // two days later
        ];
        let convos = segment_conversations("c1", msgs.into_iter(), &default_config());
        assert_eq!(convos.len(), 3);
    }
}
```

#### Pattern Matcher Tests

```rust
#[cfg(test)]
mod pattern_tests {
    use super::*;

    #[test]
    fn test_laugh_detection() {
        assert!(is_laugh("lol that's hilarious"));
        assert!(is_laugh("LMAO"));
        assert!(is_laugh("hahaha"));
        assert!(is_laugh("😂"));
        assert!(is_laugh("I'm dead 💀"));
        assert!(!is_laugh("I followed the protocol"));  // "lol" inside "followed" — word boundary needed
        assert!(!is_laugh("Hello friend"));
    }

    #[test]
    fn test_apology_detection() {
        assert!(is_apology("I'm sorry about that"));
        assert!(is_apology("my bad dude"));
        assert!(is_apology("I apologize for the delay"));
        assert!(!is_apology("The band Sorry played last night"));  // capitalized proper noun — tricky, accept false positive
    }

    #[test]
    fn test_question_detection() {
        assert!(is_question("What time is dinner?"));
        assert!(is_question("you coming?"));
        assert!(!is_question("See you there!"));
        assert!(!is_question("I wonder if...")); // no question mark
    }

    #[test]
    fn test_encouragement_detection() {
        assert!(is_encouragement("You got this!"));
        assert!(is_encouragement("I'm so proud of you"));
        assert!(is_encouragement("hell yeah!"));
        assert!(!is_encouragement("That restaurant has good food"));
    }

    #[test]
    fn test_link_detection() {
        assert!(contains_link("Check out https://example.com"));
        assert!(contains_link("www.google.com is useful"));
        assert!(contains_link("Visit http://localhost:3000"));
        assert!(!contains_link("I went to the website"));
    }
}
```

#### Emoji Extractor Tests

```rust
#[cfg(test)]
mod emoji_tests {
    use super::*;

    #[test]
    fn test_basic_emoji() {
        let counts = extract_emojis("Hello 😀 world 😀 test 🎉");
        assert_eq!(counts.get("😀"), Some(&2));
        assert_eq!(counts.get("🎉"), Some(&1));
    }

    #[test]
    fn test_no_emoji() {
        let counts = extract_emojis("Just plain text here");
        assert!(counts.is_empty());
    }

    #[test]
    fn test_zwj_sequence() {
        // Family emoji is a ZWJ sequence — should be one entry, not 4 separate emojis
        let counts = extract_emojis("👨‍👩‍👧‍👦");
        assert_eq!(counts.len(), 1);
        assert_eq!(counts.get("👨‍👩‍👧‍👦"), Some(&1));
    }

    #[test]
    fn test_skin_tone_modifier() {
        let counts = extract_emojis("👍🏽");
        assert_eq!(counts.len(), 1);
        // Key should include the modifier
    }

    #[test]
    fn test_flag_emoji() {
        let counts = extract_emojis("🇺🇸 🇯🇵");
        assert_eq!(counts.len(), 2);
    }
}
```

#### Response Calculator Tests

```rust
#[cfg(test)]
mod response_tests {
    // These tests use an in-memory SQLite database seeded with test messages.

    #[test]
    fn test_basic_response_pair() {
        // Me at t=0, Them at t=30s → their response = 30s
        // Expected: their_response_times = [30000ms]
    }

    #[test]
    fn test_double_message_not_counted_as_response() {
        // Me at t=0, Me at t=10s, Them at t=60s
        // Only one response pair: Them responded to Me in 60s
        // NOT: Them responded in 50s (from second message)
        // The response is measured from the LAST message by the other person.
    }

    #[test]
    fn test_first_response_in_convo() {
        // Convo starts with Me at t=0
        // Them replies at t=120s
        // This 120s is both a regular response AND a first response
    }

    #[test]
    fn test_median_not_skewed_by_outliers() {
        // 9 responses at 30s, 1 response at 8 hours
        // Mean would be ~50 minutes (misleading)
        // Median should be 30s (accurate)
    }

    #[test]
    fn test_rapid_response_percentage() {
        // 7 responses under 60s, 3 over 60s
        // rapid_response_pct = 70%
    }
}
```

#### Flow Builder Tests

```rust
#[cfg(test)]
mod flow_tests {
    #[test]
    fn test_basic_flow() {
        // 2 convos: one started by me (big moment), one started by them (everyday)
        // Verify nodes and links are correct
    }

    #[test]
    fn test_missed_convo_terminates_at_column_2() {
        // Missed convo should have a link to "no_reply" node
        // and NO links to columns 3 or 4
    }

    #[test]
    fn test_node_values_sum_correctly() {
        // Sum of all column-0 node values = total conversations
        // Sum of all links out of a node = that node's value
    }
}
```

### 14.2 Unit Tests — Python

File: `src-python/tests/test_analytics.py`

```python
import pytest
from sms_archive.scoring import calculate_message_points, POINT_WEIGHTS
from sms_archive.rating import compute_rating, _score_responsiveness, _score_balance
from sms_archive.insights import generate_insights
from sms_archive.topic_classifier import classify_message
from sms_archive.writing_milestones import compute_milestones

class TestScoring:
    def test_basic_text_message(self):
        """Plain text message with 10 words = 1 + 10*0.1 = 2 points"""
        pts = calculate_message_points(
            word_count=10, emoji_count=0, is_question=False,
            media_type=None, is_convo_starter=False,
            is_rapid_response=False, is_encouragement=False,
            char_count=50,
        )
        assert pts == 2

    def test_convo_starter_bonus(self):
        """Starting a conversation adds 5 points"""
        base = calculate_message_points(
            word_count=5, emoji_count=0, is_question=False,
            media_type=None, is_convo_starter=False,
            is_rapid_response=False, is_encouragement=False,
            char_count=30,
        )
        with_starter = calculate_message_points(
            word_count=5, emoji_count=0, is_question=False,
            media_type=None, is_convo_starter=True,
            is_rapid_response=False, is_encouragement=False,
            char_count=30,
        )
        assert with_starter - base == POINT_WEIGHTS["started_convo"]

    def test_image_worth_more_than_text(self):
        """An image message should score higher than a text-only message"""
        text_pts = calculate_message_points(
            word_count=5, emoji_count=0, is_question=False,
            media_type=None, is_convo_starter=False,
            is_rapid_response=False, is_encouragement=False,
            char_count=30,
        )
        img_pts = calculate_message_points(
            word_count=5, emoji_count=0, is_question=False,
            media_type="image", is_convo_starter=False,
            is_rapid_response=False, is_encouragement=False,
            char_count=30,
        )
        assert img_pts > text_pts

    def test_long_message_bonus(self):
        """Messages over 200 chars get a bonus"""
        short = calculate_message_points(
            word_count=20, emoji_count=0, is_question=False,
            media_type=None, is_convo_starter=False,
            is_rapid_response=False, is_encouragement=False,
            char_count=100,
        )
        long = calculate_message_points(
            word_count=40, emoji_count=0, is_question=False,
            media_type=None, is_convo_starter=False,
            is_rapid_response=False, is_encouragement=False,
            char_count=250,
        )
        assert long > short + POINT_WEIGHTS["per_word"] * 20  # bonus beyond word diff


class TestRating:
    def test_responsiveness_fast_both(self):
        """Both respond in under 1 minute → high score"""
        pair = {"my_median_response_ms": 30_000, "their_median_response_ms": 45_000}
        score = _score_responsiveness(pair)
        assert score >= 80

    def test_responsiveness_one_slow(self):
        """One responds in 2 hours → medium score"""
        pair = {"my_median_response_ms": 30_000, "their_median_response_ms": 7_200_000}
        score = _score_responsiveness(pair)
        assert 20 <= score <= 60

    def test_balance_perfect(self):
        """50/50 message split and initiation → 100"""
        pair = {"convos_started_by_me": 50, "convos_started_by_them": 50}
        contact = {"my_message_count": 1000, "their_message_count": 1000}
        score = _score_balance(pair, contact)
        assert score >= 90

    def test_balance_one_sided(self):
        """90/10 split → low score"""
        pair = {"convos_started_by_me": 90, "convos_started_by_them": 10}
        contact = {"my_message_count": 9000, "their_message_count": 1000}
        score = _score_balance(pair, contact)
        assert score <= 40

    def test_overall_score_bounded(self):
        """Overall score is always 0-100"""
        pair = {
            "my_median_response_ms": 1000, "their_median_response_ms": 1000,
            "convos_started_by_me": 50, "convos_started_by_them": 50,
            "avg_convo_points": 15,
            "first_message_at": 1000000, "last_message_at": 1000000 + 86400 * 365 * 3,
        }
        contact = {
            "my_message_count": 1000, "their_message_count": 1000,
            "my_question_count": 100, "their_question_count": 100,
            "my_encouragement_count": 50, "their_encouragement_count": 50,
            "my_image_count": 30, "their_image_count": 30,
            "my_video_count": 5, "their_video_count": 5,
            "my_audio_count": 0, "their_audio_count": 0,
            "my_gif_count": 10, "their_gif_count": 10,
        }
        daily = [{"my_messages": 5, "their_messages": 5}] * 100
        result = compute_rating(pair, contact, daily)
        assert 0 <= result.overall <= 100


class TestInsights:
    def test_minimum_data_threshold(self):
        """With very little data, few insights should be generated"""
        pair = {
            "my_avg_response_ms": 30000, "their_avg_response_ms": 30000,
            "convos_started_by_me": 2, "convos_started_by_them": 3,
            "total_conversations": 5, "reconnects": 0,
            "convos_closed_by_me": 2, "convos_closed_by_them": 3,
            "my_convos_missed": 0, "their_convos_missed": 0,
            "my_double_messages": 1, "their_double_messages": 1,
            "my_avg_first_response_ms": 30000, "their_avg_first_response_ms": 30000,
        }
        contact = {
            "my_message_count": 10, "their_message_count": 15,
            "my_laugh_count": 1, "their_laugh_count": 2,
            "my_apology_count": 0, "their_apology_count": 0,
            "my_encouragement_count": 0, "their_encouragement_count": 0,
            "my_image_count": 0, "their_image_count": 0,
            "my_video_count": 0, "their_video_count": 0,
            "my_audio_count": 0, "their_audio_count": 0,
            "my_gif_count": 0, "their_gif_count": 0,
        }
        insights = generate_insights(pair, contact)
        # Most rules require minimum 10-20 data points
        assert len(insights) <= 5

    def test_fast_responder_insight(self):
        """If I respond 5x faster, should get a 'much faster' insight"""
        pair = {
            "my_avg_response_ms": 10_000,
            "their_avg_response_ms": 300_000,
            "convos_started_by_me": 50, "convos_started_by_them": 50,
            "total_conversations": 100, "reconnects": 5,
            "convos_closed_by_me": 50, "convos_closed_by_them": 50,
            "my_convos_missed": 5, "their_convos_missed": 5,
            "my_double_messages": 10, "their_double_messages": 10,
            "my_avg_first_response_ms": 10_000,
            "their_avg_first_response_ms": 300_000,
        }
        contact = {
            "my_message_count": 5000, "their_message_count": 5000,
            "my_laugh_count": 100, "their_laugh_count": 100,
            "my_apology_count": 10, "their_apology_count": 10,
            "my_encouragement_count": 20, "their_encouragement_count": 20,
            "my_image_count": 50, "their_image_count": 50,
            "my_video_count": 5, "their_video_count": 5,
            "my_audio_count": 0, "their_audio_count": 0,
            "my_gif_count": 5, "their_gif_count": 5,
        }
        insights = generate_insights(pair, contact)
        speed_insights = [i for i in insights if "faster" in i["text"] and "much" in i["text"]]
        assert len(speed_insights) >= 1

    def test_insights_sorted_by_magnitude(self):
        """Insights should be sorted highest magnitude first"""
        pair = {
            "my_avg_response_ms": 5_000, "their_avg_response_ms": 500_000,
            "convos_started_by_me": 90, "convos_started_by_them": 10,
            "total_conversations": 100, "reconnects": 2,
            "convos_closed_by_me": 60, "convos_closed_by_them": 40,
            "my_convos_missed": 2, "their_convos_missed": 15,
            "my_double_messages": 50, "their_double_messages": 5,
            "my_avg_first_response_ms": 5_000,
            "their_avg_first_response_ms": 500_000,
        }
        contact = {
            "my_message_count": 8000, "their_message_count": 2000,
            "my_laugh_count": 500, "their_laugh_count": 100,
            "my_apology_count": 30, "their_apology_count": 5,
            "my_encouragement_count": 40, "their_encouragement_count": 10,
            "my_image_count": 200, "their_image_count": 20,
            "my_video_count": 10, "their_video_count": 2,
            "my_audio_count": 5, "their_audio_count": 0,
            "my_gif_count": 20, "their_gif_count": 3,
        }
        insights = generate_insights(pair, contact)
        magnitudes = [i["magnitude"] for i in insights]
        assert magnitudes == sorted(magnitudes, reverse=True)

    def test_max_12_insights(self):
        """Never return more than 12 insights"""
        # Use a heavily asymmetric dataset that triggers many rules
        pair = {
            "my_avg_response_ms": 5_000, "their_avg_response_ms": 600_000,
            "convos_started_by_me": 85, "convos_started_by_them": 15,
            "total_conversations": 100, "reconnects": 1,
            "convos_closed_by_me": 70, "convos_closed_by_them": 30,
            "my_convos_missed": 1, "their_convos_missed": 20,
            "my_double_messages": 60, "their_double_messages": 3,
            "my_avg_first_response_ms": 5_000,
            "their_avg_first_response_ms": 600_000,
        }
        contact = {
            "my_message_count": 9000, "their_message_count": 1000,
            "my_laugh_count": 800, "their_laugh_count": 50,
            "my_apology_count": 40, "their_apology_count": 2,
            "my_encouragement_count": 60, "their_encouragement_count": 3,
            "my_image_count": 300, "their_image_count": 10,
            "my_video_count": 20, "their_video_count": 1,
            "my_audio_count": 5, "their_audio_count": 0,
            "my_gif_count": 30, "their_gif_count": 1,
        }
        insights = generate_insights(pair, contact)
        assert len(insights) <= 12


class TestTopicClassifier:
    def test_first_person_dominant(self):
        assert classify_message("I went to the store and bought myself a treat") == "me"

    def test_second_person_dominant(self):
        assert classify_message("You should try the new restaurant near your place") == "them"

    def test_neutral_topic(self):
        assert classify_message("The weather is nice today") == "other"

    def test_tie_goes_to_other(self):
        assert classify_message("I think you are right") == "other"  # 1 first, 1 second

    def test_empty_string(self):
        assert classify_message("") == "other"

    def test_contractions(self):
        assert classify_message("I'm going to I've been I'll try") == "me"
        assert classify_message("You're great you've done you'll see") == "them"


class TestWritingMilestones:
    def test_no_books_completed(self):
        result = compute_milestones(total_chars=1000, total_words=167)
        assert result["milestones"][0]["status"] == "in_progress"
        assert result["series_completion_pct"] < 0.01

    def test_first_book_completed(self):
        result = compute_milestones(
            total_chars=500_000,
            total_words=80_000  # > 77,325 (book 1)
        )
        assert result["milestones"][0]["status"] == "complete"
        assert result["milestones"][1]["status"] == "in_progress"

    def test_all_books_completed(self):
        result = compute_milestones(
            total_chars=7_000_000,
            total_words=1_200_000  # > 1,084,209 (total series)
        )
        assert all(m["status"] == "complete" for m in result["milestones"])
        assert result["series_completion_pct"] >= 1.0

    def test_typing_time_estimate(self):
        # 40_000 words at 40 WPM = 1000 minutes = 16.67 hours
        result = compute_milestones(total_chars=240_000, total_words=40_000)
        assert 16 <= result["typing_time_estimate_hrs"] <= 17
```

### 14.3 Integration Tests

File: `src-tauri/src/analytics/tests/integration.rs`

```rust
/// Integration tests use an in-memory SQLite database seeded with
/// realistic test data. They test the full compute pipeline:
/// segmentation → aggregation → response times → flow building.

#[test]
fn test_full_pipeline_small_dataset() {
    // Seed: 100 messages between me and "contact_a" over 7 days
    // Verify:
    //   - conversations table has reasonable count (5-15 convos for 7 days)
    //   - contact_analytics has correct message counts
    //   - activity_daily has 7 rows
    //   - activity_hourly has entries matching message times
    //   - pair_analytics has response times populated
}

#[test]
fn test_full_pipeline_no_messages() {
    // Seed: contact exists but has zero messages
    // Verify: all analytics tables are empty/zeroed for this contact
    // No crashes, no division by zero
}

#[test]
fn test_recompute_overwrites_old_data() {
    // Seed: compute analytics, then add 50 more messages, recompute
    // Verify: new totals include the added messages
    // Old rows are replaced, not duplicated
}

#[test]
fn test_concurrent_compute_different_contacts() {
    // Two contacts computed in parallel (Tokio tasks)
    // Verify: no data corruption, no cross-contact contamination
}
```

### 14.4 Frontend Tests

File: `src/components/Analytics/__tests__/`

Use React Testing Library + Vitest.

```typescript
// AnalyticsDashboard.test.tsx
describe("AnalyticsDashboard", () => {
  it("shows loading skeleton when analytics are computing", async () => {
    // Mock invoke to delay
    // Verify skeleton elements are visible
  });

  it("displays all sections when data is loaded", async () => {
    // Mock invoke to return test data
    // Verify each section header is present
  });

  it("shows error state when compute fails", async () => {
    // Mock invoke to reject
    // Verify error message is displayed
  });
});

// HeaderStats.test.tsx
describe("HeaderStats", () => {
  it("formats large numbers with commas", () => {
    render(<HeaderStats totalMessages={153162} ... />);
    expect(screen.getByText("153,162")).toBeInTheDocument();
  });

  it("formats time period correctly", () => {
    // 12y 4m 3d → "12y 4m 3d"
  });
});

// formatters.test.ts
describe("formatDuration", () => {
  it("formats seconds", () => expect(formatDuration(45000)).toBe("45s"));
  it("formats minutes", () => expect(formatDuration(120000)).toBe("2:00"));
  it("formats hours", () => expect(formatDuration(3661000)).toBe("1:01:01"));
});
```

### 14.5 Test Data Generator

File: `scripts/generate_test_data.py`

```python
"""
Generates a realistic SQLite database with synthetic message data
for analytics testing.

Usage: python scripts/generate_test_data.py --messages 10000 --contacts 5 --output test.db

Generates:
  - Realistic message timing (clustered around morning/evening, weekday patterns)
  - Varied message lengths and types
  - Emoji, questions, apologies mixed in
  - Media attachments at realistic rates (~15% of messages)
  - Multiple contacts with different relationship patterns:
    - "best_friend": balanced, high volume, fast responses
    - "acquaintance": low volume, long gaps
    - "one_sided": mostly outbound, slow responses
    - "new_contact": short history
    - "ex": high volume past, zero recent
"""
```

---

## 15. Implementation Phases

### Phase 0: Setup (Day 1)

- [ ] Create migration `002_analytics.sql`
- [ ] Run migration against dev database
- [ ] Create `src-tauri/src/analytics/mod.rs` module structure
- [ ] Create `src-python/sms_archive/` analytics module files
- [ ] Install frontend deps: `npm install d3-sankey @types/d3-sankey`
- [ ] Create `src/types/analytics.ts`
- [ ] Create test data generator script
- [ ] Generate test database with 10k messages across 5 contacts

### Phase 1: Conversation Segmentation (Days 2-3)

- [ ] Implement `conversation_segmenter.rs`
- [ ] Write and pass all segmenter unit tests
- [ ] Implement `write_conversations()` to persist to SQLite
- [ ] Verify against test database: inspect conversation count + boundaries

### Phase 2: Aggregation Pass (Days 4-5)

- [ ] Implement `aggregator.rs` — single-pass message iteration
- [ ] Implement `emoji_extractor.rs`
- [ ] Implement `pattern_matcher.rs`
- [ ] Implement `media_classifier.rs`
- [ ] Write and pass all unit tests for each module
- [ ] Verify `contact_analytics`, `activity_daily`, `activity_hourly` populated correctly

### Phase 3: Response Times (Day 6)

- [ ] Implement `response_calculator.rs`
- [ ] Implement median/mean/percentile calculations
- [ ] Write and pass response calculator tests
- [ ] Update `pair_analytics` with response time summary

### Phase 4: Conversation Flow (Day 7)

- [ ] Implement `flow_builder.rs`
- [ ] Write and pass flow builder tests
- [ ] Store flow JSON in `pair_analytics`

### Phase 5: Orchestrator + Tauri Commands (Day 8)

- [ ] Implement `compute_all_analytics()` orchestrator
- [ ] Register Tauri IPC commands
- [ ] Implement progress events
- [ ] Run full pipeline against test database — verify all tables populated
- [ ] Run integration tests

### Phase 6: Python Pass (Days 9-10)

- [ ] Implement `scoring.py` + export weights to Rust
- [ ] Implement `rating.py` with all 6 component scores
- [ ] Implement `insights.py` with all 11 rule categories
- [ ] Implement `topic_classifier.py`
- [ ] Implement `writing_milestones.py`
- [ ] Write and pass all Python tests
- [ ] Wire Python pass into orchestrator via JSON-RPC

### Phase 7: Frontend — Stat Cards (Days 11-12)

- [ ] `AnalyticsDashboard.tsx` layout with contact selector
- [ ] `useAnalytics.ts` hook
- [ ] `HeaderStats.tsx`
- [ ] `ChatRating.tsx` (SVG gauge)
- [ ] `BalanceBar.tsx`
- [ ] `MessageAnalysis.tsx`
- [ ] `MediaStats.tsx`
- [ ] `ConversationAnalysis.tsx`
- [ ] `ResponseStats.tsx`
- [ ] `KeyInsights.tsx`
- [ ] `LanguageAnalysis.tsx`
- [ ] `WritingSummary.tsx`
- [ ] Formatting utilities + tests

### Phase 8: Frontend — Charts (Days 13-15)

- [ ] `RelationshipGrowthChart.tsx` (Recharts area chart)
- [ ] `MessagingHeatmap.tsx` (custom SVG grid)
- [ ] `ChatFocusPie.tsx` (Recharts donut)
- [ ] `ConversationFlow.tsx` (d3-sankey)
- [ ] `ActivityHeatmap.tsx` (custom SVG calendar grid)
- [ ] Downsampling for large datasets
- [ ] Dark theme CSS variables

### Phase 9: Polish (Days 16-17)

- [ ] Loading skeletons for each component
- [ ] Error states
- [ ] "Refresh Analytics" button with progress bar
- [ ] Print/export dashboard as PNG (html2canvas)
- [ ] Frontend tests
- [ ] Performance profiling against 600k message dataset
- [ ] Fix any bottlenecks

---

## 16. File Tree (Final State)

```
sms-archive-manager/
├── migrations/
│   ├── 001_initial.sql
│   └── 002_analytics.sql              ← NEW
│
├── src-tauri/src/
│   ├── analytics/                      ← NEW MODULE
│   │   ├── mod.rs                      # Orchestrator + public API
│   │   ├── conversation_segmenter.rs   # Convo boundary detection
│   │   ├── aggregator.rs              # Single-pass message aggregation
│   │   ├── response_calculator.rs     # Response time math
│   │   ├── emoji_extractor.rs         # Unicode emoji parsing
│   │   ├── pattern_matcher.rs         # Regex: laughs, questions, etc
│   │   ├── media_classifier.rs        # MIME type → category
│   │   ├── flow_builder.rs            # Sankey conversation flow
│   │   ├── error.rs                   # AnalyticsError type
│   │   └── tests/
│   │       ├── mod.rs
│   │       ├── segmenter_tests.rs
│   │       ├── pattern_tests.rs
│   │       ├── emoji_tests.rs
│   │       ├── response_tests.rs
│   │       ├── flow_tests.rs
│   │       └── integration.rs
│   │
│   └── commands/
│       └── analytics.rs               ← NEW (Tauri IPC handlers)
│
├── src-python/
│   ├── sms_archive/
│   │   ├── scoring.py                 ← NEW
│   │   ├── rating.py                  ← NEW
│   │   ├── insights.py                ← NEW
│   │   ├── topic_classifier.py        ← NEW
│   │   └── writing_milestones.py      ← NEW
│   │
│   └── tests/
│       └── test_analytics.py          ← NEW
│
├── src/
│   ├── types/
│   │   └── analytics.ts               ← NEW
│   │
│   ├── utils/
│   │   └── analyticsFormatters.ts     ← NEW
│   │
│   ├── hooks/
│   │   └── useAnalytics.ts            ← NEW
│   │
│   └── components/
│       └── Analytics/                  ← NEW DIRECTORY
│           ├── AnalyticsDashboard.tsx
│           ├── HeaderStats.tsx
│           ├── RelationshipGrowthChart.tsx
│           ├── ChatRating.tsx
│           ├── BalanceBar.tsx
│           ├── WritingSummary.tsx
│           ├── MessagingHeatmap.tsx
│           ├── ChatFocusPie.tsx
│           ├── KeyInsights.tsx
│           ├── LanguageAnalysis.tsx
│           ├── MessageAnalysis.tsx
│           ├── ResponseStats.tsx
│           ├── ConversationFlow.tsx
│           ├── ConversationAnalysis.tsx
│           ├── MediaStats.tsx
│           ├── ActivityHeatmap.tsx
│           └── __tests__/
│               ├── AnalyticsDashboard.test.tsx
│               ├── HeaderStats.test.tsx
│               └── formatters.test.ts
│
└── scripts/
    └── generate_test_data.py          ← NEW
```

---

## Appendix A: Dependencies to Add

### Rust (`Cargo.toml`)
```toml
[dependencies]
chrono = { version = "0.4", features = ["serde"] }
unicode-segmentation = "1.10"
unic-emoji-char = "0.9"
regex = "1"
once_cell = "1"
# rusqlite, serde, serde_json already in project
```

### Python (`requirements.txt`)
```
# Already present: sqlite3 (stdlib), json (stdlib)
# No new deps needed — analytics modules use pure Python + stdlib
```

### Node (`package.json`)
```json
{
  "dependencies": {
    "d3-sankey": "^0.12.3",
    "recharts": "^2.12.0"
  },
  "devDependencies": {
    "@types/d3-sankey": "^0.12.1"
  }
}
```

---

## Appendix B: Configurable Parameters

All stored in `analytics_meta` table. Changeable via Settings UI.

| Key | Default | Description |
|---|---|---|
| `conversation_timeout_secs` | 14400 (4h) | Gap that splits conversations |
| `big_moment_threshold` | 20 | Messages needed for "big moment" category |
| `rapid_response_threshold_secs` | 60 | Response under this = "rapid" |
| `reconnect_threshold_secs` | 86400 (24h) | Gap that counts as a reconnection |
| `scoring_weights` | (see scoring.py) | JSON blob of point weights |
| `insight_min_data_threshold` | 20 | Minimum data points before generating insights |
| `heatmap_day_count` | 500 | Days shown in the activity heatmap |

---

*End of Analytics Bootstrap Specification*
