-- Mission: persist sentiment timeline + inside-jokes detection results so the
-- dashboard can render them without recomputing on every view.
--
-- All three columns are nullable and default to '{}' / '[]' so old rows
-- (computed before this migration) load cleanly.

ALTER TABLE pair_analytics ADD COLUMN sentiment_timeline_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE pair_analytics ADD COLUMN inside_jokes_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE pair_analytics ADD COLUMN topics_json TEXT NOT NULL DEFAULT '[]';

UPDATE schema_version SET version = 16 WHERE version < 16;
