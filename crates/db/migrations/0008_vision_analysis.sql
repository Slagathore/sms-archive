-- db/migrations/0008_vision_analysis.sql

ALTER TABLE attachments ADD COLUMN vision_analysis TEXT;
ALTER TABLE attachments ADD COLUMN vision_model TEXT;
ALTER TABLE attachments ADD COLUMN vision_timestamp INTEGER;

UPDATE schema_version SET version = 8 WHERE version < 8;
