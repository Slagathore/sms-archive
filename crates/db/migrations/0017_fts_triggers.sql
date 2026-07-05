-- db/migrations/0017_fts_triggers.sql
--
-- Keep the external-content FTS index (messages_fts) in sync with the
-- messages table at the engine level. Before this migration the index was
-- only synced by full rebuilds (rebuild_fts), so any writer that skipped the
-- rebuild left search silently stale. Triggers cover every writer: the
-- ingest BatchWriter, GUI backfill inserts, and the CLI.
--
-- This is the canonical FTS5 external-content pattern from the SQLite docs.

-- The index may be stale or missing (e.g. a crash mid-rebuild); rebuild it
-- once so the triggers start from a consistent state.
DROP TABLE IF EXISTS messages_fts;
CREATE VIRTUAL TABLE messages_fts USING fts5(
    body_searchable,
    address,
    content=messages,
    tokenize='unicode61 remove_diacritics 0'
);
INSERT INTO messages_fts(rowid, body_searchable, address)
SELECT rowid, body_searchable, address FROM messages;
INSERT INTO messages_fts(messages_fts) VALUES('optimize');

CREATE TRIGGER IF NOT EXISTS messages_fts_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, body_searchable, address)
    VALUES (new.rowid, new.body_searchable, new.address);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body_searchable, address)
    VALUES ('delete', old.rowid, old.body_searchable, old.address);
END;

CREATE TRIGGER IF NOT EXISTS messages_fts_au
AFTER UPDATE OF body_searchable, address ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, body_searchable, address)
    VALUES ('delete', old.rowid, old.body_searchable, old.address);
    INSERT INTO messages_fts(rowid, body_searchable, address)
    VALUES (new.rowid, new.body_searchable, new.address);
END;

UPDATE schema_version SET version = 17 WHERE version < 17;
