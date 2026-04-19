-- db/migrations/0006_extended_contacts.sql

-- Add new contact fields
ALTER TABLE contacts ADD COLUMN phone_primary_type TEXT DEFAULT 'mobile';
ALTER TABLE contacts ADD COLUMN phone_secondary_type TEXT DEFAULT 'home';
ALTER TABLE contacts ADD COLUMN website TEXT;
ALTER TABLE contacts ADD COLUMN social_media TEXT;
ALTER TABLE contacts ADD COLUMN last_contacted INTEGER;
ALTER TABLE contacts ADD COLUMN favorite INTEGER DEFAULT 0;

-- Contact groups
CREATE TABLE IF NOT EXISTS contact_groups (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    color TEXT,
    created_at INTEGER DEFAULT (strftime('%s', 'now'))
);

CREATE TABLE IF NOT EXISTS contact_group_members (
    contact_id TEXT NOT NULL,
    group_id TEXT NOT NULL,
    added_at INTEGER DEFAULT (strftime('%s', 'now')),
    PRIMARY KEY (contact_id, group_id),
    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE,
    FOREIGN KEY (group_id) REFERENCES contact_groups(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_group_members_contact ON contact_group_members(contact_id);
CREATE INDEX IF NOT EXISTS idx_group_members_group ON contact_group_members(group_id);
CREATE INDEX IF NOT EXISTS idx_contacts_favorite ON contacts(favorite) WHERE favorite = 1;

UPDATE schema_version SET version = 6 WHERE version < 6;
