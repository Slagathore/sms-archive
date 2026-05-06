-- Mission: distinguish contacts auto-created at ingest from those a user explicitly made.
-- Auto-created contacts are bootstrapped from messages.contact_name (or address fallback)
-- so analytics works on day one. Manual contacts are produced by user UI actions.

ALTER TABLE contacts ADD COLUMN source TEXT NOT NULL DEFAULT 'auto';

-- Existing rows pre-date this migration and were either auto-imported by sync_contact_names_from_xml
-- or hand-created. We can't distinguish them after the fact. Default to 'unknown' for the
-- legacy set; future rows will land as 'auto' or 'manual' explicitly.
UPDATE contacts SET source = 'unknown' WHERE source = 'auto';

-- Index for the common "show me only auto-created contacts" filter.
CREATE INDEX IF NOT EXISTS idx_contacts_source ON contacts(source);

UPDATE schema_version SET version = 15 WHERE version < 15;
