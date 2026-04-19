-- db/migrations/0002_ml.sql

CREATE TABLE IF NOT EXISTS ml_models (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    version TEXT NOT NULL,
    sha256 TEXT,
    created_at INTEGER DEFAULT (strftime('%s', 'now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_ml_models_unique
    ON ml_models(name, version, sha256);

UPDATE schema_version SET version = 2 WHERE version < 2;

CREATE TABLE IF NOT EXISTS embeddings (
    message_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    dims INTEGER NOT NULL,
    vector BLOB NOT NULL,
    created_at INTEGER DEFAULT (strftime('%s', 'now')),
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE,
    FOREIGN KEY (model_id) REFERENCES ml_models(id) ON DELETE CASCADE,
    UNIQUE(message_id, model_id)
);
