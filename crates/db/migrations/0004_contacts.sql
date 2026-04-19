-- db/migrations/0004_contacts.sql

CREATE TABLE IF NOT EXISTS contacts (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    nickname TEXT,
    company TEXT,
    notes TEXT,
    email TEXT,
    phone_primary TEXT,
    phone_secondary TEXT,
    address TEXT,
    birthday TEXT,
    avatar_path TEXT,
    created_at INTEGER DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER DEFAULT (strftime('%s', 'now'))
);

CREATE TABLE IF NOT EXISTS contact_addresses (
    id TEXT PRIMARY KEY,
    contact_id TEXT NOT NULL,
    address TEXT NOT NULL,
    label TEXT,
    created_at INTEGER DEFAULT (strftime('%s', 'now')),
    FOREIGN KEY (contact_id) REFERENCES contacts(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_contact_addresses_unique
    ON contact_addresses(contact_id, address);

CREATE INDEX IF NOT EXISTS idx_contact_addresses_address
    ON contact_addresses(address);

UPDATE schema_version SET version = 4 WHERE version < 4;
