-- db/migrations/0005_settings.sql

CREATE TABLE IF NOT EXISTS app_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

UPDATE schema_version SET version = 5 WHERE version < 5;
