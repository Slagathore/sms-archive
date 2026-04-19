-- Mission: persist media embeddings per attachment/keyframe for semantic search and clustering.

CREATE TABLE IF NOT EXISTS media_embeddings (
    attachment_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    frame_index INTEGER NOT NULL,
    frame_time_ms INTEGER,
    caption TEXT,
    dims INTEGER NOT NULL,
    vector BLOB NOT NULL,
    created_at INTEGER DEFAULT (strftime('%s', 'now')),
    FOREIGN KEY (attachment_id) REFERENCES attachments(id) ON DELETE CASCADE,
    FOREIGN KEY (model_id) REFERENCES ml_models(id) ON DELETE CASCADE,
    UNIQUE(attachment_id, model_id, frame_index)
);

CREATE INDEX IF NOT EXISTS idx_media_embeddings_attachment
    ON media_embeddings(attachment_id);

UPDATE schema_version SET version = 11 WHERE version < 11;
