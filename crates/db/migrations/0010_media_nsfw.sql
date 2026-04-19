-- Mission: store NSFW classification metadata for media so the UI can filter and label content.

ALTER TABLE attachments ADD COLUMN nsfw_label TEXT;
ALTER TABLE attachments ADD COLUMN nsfw_score REAL;
ALTER TABLE attachments ADD COLUMN nsfw_model TEXT;
ALTER TABLE attachments ADD COLUMN nsfw_timestamp INTEGER;

UPDATE schema_version SET version = 10 WHERE version < 10;
