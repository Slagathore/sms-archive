-- Mission: capture the `contact_name` attribute that SMS Backup & Restore writes on every
-- SMS row. This preserves the contact label that was in the user's address book at backup
-- time so analytics and UI can show human-readable names without a separate JOIN.
--
-- Nullable: MMS rows in the same XML often lack contact_name, and historical ingests
-- ran before this column existed.

ALTER TABLE messages ADD COLUMN contact_name TEXT;

UPDATE schema_version SET version = 13 WHERE version < 13;
