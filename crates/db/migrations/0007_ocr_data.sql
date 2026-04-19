-- db/migrations/0007_ocr_data.sql

ALTER TABLE attachments ADD COLUMN ocr_text TEXT;
ALTER TABLE attachments ADD COLUMN ocr_model TEXT;
ALTER TABLE attachments ADD COLUMN ocr_timestamp INTEGER;

CREATE INDEX IF NOT EXISTS idx_attachments_ocr ON attachments(ocr_text);

UPDATE schema_version SET version = 7 WHERE version < 7;
