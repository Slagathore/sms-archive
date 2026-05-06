-- Mission: stand up the per-contact analytics module.
--
-- Design principle: compute once, read many. All analytics derive from `messages` but
-- live in pre-aggregated tables that the dashboard reads directly. Recomputation is
-- triggered on demand (user clicks "Run Analysis") or marked stale by the ingest pipeline.
--
-- Group MMS (`address` containing '~') are excluded from analytics at query time, not here.

-- ============================================================
-- conversations: segmented (me <-> contact) chronological exchanges.
-- One row = one conversation, defined by gap > timeout splitting.
-- ============================================================
CREATE TABLE IF NOT EXISTS conversations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    contact_id TEXT NOT NULL,
    start_time INTEGER NOT NULL,                  -- unix epoch ms of first message
    end_time INTEGER NOT NULL,                    -- unix epoch ms of last message
    started_by INTEGER NOT NULL,                  -- 1 = me (outgoing), 2 = them (incoming)
    final_reply_by INTEGER NOT NULL,              -- 1 = me, 2 = them
    my_message_count INTEGER NOT NULL DEFAULT 0,
    their_message_count INTEGER NOT NULL DEFAULT 0,
    total_message_count INTEGER NOT NULL DEFAULT 0,
    major_contributor INTEGER NOT NULL,           -- 1 = me, 2 = them; whoever sent more
    is_missed INTEGER NOT NULL DEFAULT 0,         -- 1 if only one side ever spoke
    missed_by INTEGER,                            -- 1 = me, 2 = them; who never replied (NULL if not missed)
    is_big_moment_static INTEGER NOT NULL DEFAULT 0,   -- total_message_count >= analytics_meta.big_moment_threshold
    is_big_moment_dynamic INTEGER NOT NULL DEFAULT 0,  -- top 10% of conversations for this pair (floor 10)
    reconnect_tier INTEGER NOT NULL DEFAULT 0,    -- 0=none, 1=≥24h, 2=≥7d, 3=≥30d, 4=≥3× pair-median gap
    points REAL NOT NULL DEFAULT 0,               -- sum of message points within this convo
    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_conversations_contact ON conversations(contact_id);
CREATE INDEX IF NOT EXISTS idx_conversations_time ON conversations(contact_id, start_time);

-- ============================================================
-- contact_analytics: per-side aggregate counts.
-- Recomputed in full on each refresh.
-- ============================================================
CREATE TABLE IF NOT EXISTS contact_analytics (
    contact_id TEXT PRIMARY KEY,
    computed_at INTEGER NOT NULL DEFAULT 0,       -- unix epoch s of last successful compute

    -- Volume
    my_message_count INTEGER NOT NULL DEFAULT 0,
    their_message_count INTEGER NOT NULL DEFAULT 0,
    my_word_count INTEGER NOT NULL DEFAULT 0,
    their_word_count INTEGER NOT NULL DEFAULT 0,
    my_unique_word_count INTEGER NOT NULL DEFAULT 0,
    their_unique_word_count INTEGER NOT NULL DEFAULT 0,
    my_character_count INTEGER NOT NULL DEFAULT 0,
    their_character_count INTEGER NOT NULL DEFAULT 0,

    -- Media (per side)
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

    -- Language patterns
    my_top_emojis TEXT NOT NULL DEFAULT '[]',     -- JSON: [{"emoji":"😂","count":100}, ...]
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

    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE
);

-- ============================================================
-- pair_analytics: relationship-level rollups derived from conversations.
-- One row per contact.
-- ============================================================
CREATE TABLE IF NOT EXISTS pair_analytics (
    contact_id TEXT PRIMARY KEY,
    computed_at INTEGER NOT NULL DEFAULT 0,

    -- Conversation stats
    total_conversations INTEGER NOT NULL DEFAULT 0,
    convos_started_by_me INTEGER NOT NULL DEFAULT 0,
    convos_started_by_them INTEGER NOT NULL DEFAULT 0,
    convos_closed_by_me INTEGER NOT NULL DEFAULT 0,
    convos_closed_by_them INTEGER NOT NULL DEFAULT 0,
    top_contributor INTEGER,                       -- 1 = me, 2 = them; overall majority
    avg_convo_points REAL NOT NULL DEFAULT 0,
    median_convo_messages REAL NOT NULL DEFAULT 0, -- helper for "big moment dynamic" threshold
    my_double_messages INTEGER NOT NULL DEFAULT 0,
    their_double_messages INTEGER NOT NULL DEFAULT 0,
    my_convos_missed INTEGER NOT NULL DEFAULT 0,   -- they spoke, I never replied
    their_convos_missed INTEGER NOT NULL DEFAULT 0,
    reconnect_count_t1 INTEGER NOT NULL DEFAULT 0, -- ≥24h gap
    reconnect_count_t2 INTEGER NOT NULL DEFAULT 0, -- ≥7d
    reconnect_count_t3 INTEGER NOT NULL DEFAULT 0, -- ≥30d
    reconnect_count_t4 INTEGER NOT NULL DEFAULT 0, -- ≥3× pair-median

    -- Response times (milliseconds). Both median and mean stored — median is headline.
    my_median_response_ms INTEGER,
    their_median_response_ms INTEGER,
    my_mean_response_ms INTEGER,
    their_mean_response_ms INTEGER,
    my_rapid_response_pct REAL,                    -- % of my responses under analytics_meta.rapid_response_threshold_secs
    their_rapid_response_pct REAL,
    my_median_first_response_ms INTEGER,           -- responder's median when other side opened the convo
    their_median_first_response_ms INTEGER,
    my_mean_first_response_ms INTEGER,
    their_mean_first_response_ms INTEGER,

    -- Awake-vs-overnight split (a midnight-crossing pair gets tagged separately).
    my_median_response_awake_ms INTEGER,
    their_median_response_awake_ms INTEGER,
    my_median_response_overnight_ms INTEGER,
    their_median_response_overnight_ms INTEGER,

    -- Response-time histogram (log-scale buckets). JSON {"my":[..8 ints..], "their":[..8 ints..]}
    response_histogram_json TEXT NOT NULL DEFAULT '{}',

    -- Points totals (cumulative).
    my_points INTEGER NOT NULL DEFAULT 0,
    their_points INTEGER NOT NULL DEFAULT 0,

    -- Composite rating (Python-equivalent computed in Rust). 0-100 with breakdown.
    overall_score INTEGER,
    score_responsiveness INTEGER,
    score_balance INTEGER,
    score_engagement INTEGER,
    score_consistency INTEGER,
    score_reciprocity INTEGER,
    score_longevity INTEGER,
    score_mutual_effort INTEGER,                   -- new component beyond spec

    -- Direction-of-conversation (chat focus). Percentages 0-100.
    focus_me_pct REAL,
    focus_them_pct REAL,
    focus_other_pct REAL,

    -- Insights output (rendered list). JSON array of insight objects.
    insights_json TEXT NOT NULL DEFAULT '[]',

    -- Writing milestones (HP-equivalents etc.). JSON.
    writing_milestones_json TEXT NOT NULL DEFAULT '{}',

    -- Sankey/flow data (computed in v1, rendered in v2).
    conversation_flow_json TEXT NOT NULL DEFAULT '{}',

    -- Time span
    first_message_at INTEGER,                      -- unix epoch ms
    last_message_at INTEGER,

    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE
);

-- ============================================================
-- activity_daily: per-contact per-day buckets.
-- Powers the relationship growth chart and the GitHub-style 500-day heatmap.
-- ============================================================
CREATE TABLE IF NOT EXISTS activity_daily (
    contact_id TEXT NOT NULL,
    day TEXT NOT NULL,                             -- 'YYYY-MM-DD' in user's local TZ
    my_messages INTEGER NOT NULL DEFAULT 0,
    their_messages INTEGER NOT NULL DEFAULT 0,
    my_words INTEGER NOT NULL DEFAULT 0,
    their_words INTEGER NOT NULL DEFAULT 0,
    my_media INTEGER NOT NULL DEFAULT 0,
    their_media INTEGER NOT NULL DEFAULT 0,
    my_points REAL NOT NULL DEFAULT 0,
    their_points REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (contact_id, day),
    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_activity_daily_contact ON activity_daily(contact_id);

-- ============================================================
-- activity_hourly: per-contact per-(day_of_week × hour) buckets.
-- Powers the messaging-times heatmap.
-- ============================================================
CREATE TABLE IF NOT EXISTS activity_hourly (
    contact_id TEXT NOT NULL,
    day_of_week INTEGER NOT NULL,                  -- 0 = Sunday, 6 = Saturday
    hour INTEGER NOT NULL,                         -- 0-23 in user's local TZ
    message_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (contact_id, day_of_week, hour),
    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE
);

-- ============================================================
-- analytics_meta: global tunables. Settings UI reads/writes here.
-- ============================================================
CREATE TABLE IF NOT EXISTS analytics_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Defaults — only inserted if not already present.
INSERT OR IGNORE INTO analytics_meta (key, value) VALUES
    -- Segmentation
    ('conversation_timeout_secs', '14400'),         -- 4 hours
    ('big_moment_threshold_static', '20'),
    ('big_moment_threshold_dynamic_pct', '90'),     -- top 10% (i.e. p90) of pair's conversations
    ('big_moment_threshold_dynamic_floor', '10'),
    ('reconnect_tier1_secs', '86400'),              -- 24h
    ('reconnect_tier2_secs', '604800'),             -- 7d
    ('reconnect_tier3_secs', '2592000'),            -- 30d
    ('reconnect_tier4_multiplier', '3.0'),          -- 3× pair-median gap

    -- Response times
    ('rapid_response_threshold_secs', '60'),
    ('overnight_window_start_hour', '23'),          -- 11pm
    ('overnight_window_end_hour', '7'),             -- 7am

    -- Point weights (per-message scoring)
    ('weight_text_message', '1.0'),
    ('weight_per_word_log', '0.1'),                 -- multiplied by ln(words+1)
    ('weight_emoji', '0.2'),
    ('weight_question', '0.5'),
    ('weight_image', '3.0'),
    ('weight_video', '5.0'),
    ('weight_audio', '4.0'),
    ('weight_gif', '2.0'),
    ('weight_link', '2.0'),
    ('weight_started_convo', '5.0'),
    ('weight_rapid_response', '2.0'),
    ('weight_encouragement', '3.0'),
    ('weight_apology', '2.0'),                      -- new vs spec

    -- Insight thresholds (tiered)
    ('insight_tier1_ratio', '1.2'),
    ('insight_tier2_ratio', '1.5'),
    ('insight_tier3_ratio', '2.0'),
    ('insight_tier4_ratio', '3.0'),
    ('insight_tier1_min_pct_of_total', '0.01'),     -- abs diff must be ≥ 1% of combined total
    ('insight_min_sample_per_rule', '50'),
    ('insight_low_confidence_max_sample', '200'),

    -- Rating component weights (sum to 1.0)
    ('rating_weight_responsiveness', '0.20'),
    ('rating_weight_balance', '0.15'),
    ('rating_weight_engagement', '0.15'),
    ('rating_weight_consistency', '0.15'),
    ('rating_weight_reciprocity', '0.10'),
    ('rating_weight_longevity', '0.10'),
    ('rating_weight_mutual_effort', '0.15'),        -- new component

    -- Data sufficiency
    ('rating_hide_below_messages', '50'),
    ('rating_low_confidence_max_messages', '200'),

    -- Display
    ('contact_picker_min_messages', '50');          -- hide noise contacts in picker by default

-- ============================================================
-- analytics_overrides: per-contact override of any analytics_meta key.
-- ============================================================
CREATE TABLE IF NOT EXISTS analytics_overrides (
    contact_id TEXT NOT NULL,
    setting_key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (contact_id, setting_key),
    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE
);

-- ============================================================
-- contact_analytics_status: cache freshness tracking.
-- Separate from contact_analytics so we can mark stale without touching data.
-- ============================================================
CREATE TABLE IF NOT EXISTS contact_analytics_status (
    contact_id TEXT PRIMARY KEY,
    last_computed_at INTEGER NOT NULL DEFAULT 0,
    is_stale INTEGER NOT NULL DEFAULT 1,            -- 1 = needs recompute
    last_compute_ms INTEGER,                        -- how long the last compute took
    last_error TEXT,                                -- last compute error message, if any
    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE
);

UPDATE schema_version SET version = 14 WHERE version < 14;
