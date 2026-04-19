-- db/migrations/0001_initial.sql

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    message_id TEXT,
    dedupe_hash BLOB,
    timestamp INTEGER NOT NULL,
    address TEXT NOT NULL,
    body TEXT NOT NULL,
    body_searchable TEXT NOT NULL,
    message_type INTEGER NOT NULL,
    thread_id TEXT,
    created_at INTEGER DEFAULT (strftime('%s', 'now'))
);

CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

INSERT INTO schema_version (version)
SELECT 1
WHERE NOT EXISTS (SELECT 1 FROM schema_version);

CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,
    message_id TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    file_path TEXT NOT NULL,
    file_hash BLOB NOT NULL,
    thumbnail_path TEXT,
    created_at INTEGER DEFAULT (strftime('%s', 'now')),
    FOREIGN KEY (message_id) REFERENCES messages(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_messages_timestamp ON messages(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_messages_address ON messages(address);
CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_message_id ON messages(message_id) WHERE message_id IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_dedupe_hash ON messages(dedupe_hash) WHERE dedupe_hash IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_attachments_message_id ON attachments(message_id);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    body_searchable,
    address,
    content=messages,
    tokenize='unicode61 remove_diacritics 0'
);
