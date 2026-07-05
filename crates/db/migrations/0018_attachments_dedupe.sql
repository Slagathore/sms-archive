-- db/migrations/0018_attachments_dedupe.sql
--
-- Re-ingesting the same backup used to insert duplicate attachment rows for
-- every already-imported MMS: the attachments table's only uniqueness was its
-- random UUID primary key, so the batch writer's ON CONFLICT DO NOTHING never
-- fired for attachments. Dedupe existing rows (keep the oldest, which carries
-- any OCR/vision data), then enforce uniqueness per (message_id, file_hash).

DELETE FROM attachments
WHERE rowid NOT IN (
    SELECT MIN(rowid) FROM attachments GROUP BY message_id, file_hash
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_attachments_message_hash
    ON attachments(message_id, file_hash);

UPDATE schema_version SET version = 18 WHERE version < 18;
