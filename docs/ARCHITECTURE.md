# Architecture

## Overview
SMS Archive Manager is a Rust-first desktop application designed to ingest, index, and search very large SMS/MMS XML exports while remaining responsive. The system is split into focused crates and is designed to keep I/O, parsing, and DB writes bounded and observable.

## Workspace Map
- app: native GUI (egui)
- cli: headless tooling (ingest, verify, doctor, datagen)
- ingest: XML parsing, attachments, checkpointing
- db: SQLite access, migrations, batch writer
- search: FTS5 backend (future backend swap via trait)
- media: thumbnails and hashing
- config: resource detection and sizing
- types: core data types
- errors: unified error taxonomy

## Data Flow
1) Reader: streams XML from disk using a large buffer.
2) Parser: quick-xml extracts sms/mms nodes; text normalized to NFC/NFKD.
3) Queue: byte-budgeted channel to cap memory usage.
4) Writer: single SQLite writer, WAL enabled, batched inserts.
5) Post import: optional FTS rebuild and WAL checkpoint/truncate.

## Database
- SQLite with FTS5 for search.
- Single writer with WAL and batched transactions.
- Unique message_id and dedupe_hash indexes provide idempotent inserts.
- Attachments table references messages with ON DELETE CASCADE.

## Search
- Default backend: FTS5.
- Search backend behind a trait to allow future swap (Tantivy or external).

## Media Pipeline
- MMS parts are materialized to disk when enabled.
- Images optionally generate thumbnails in a separate subdirectory.
- Files are stored by content hash to avoid duplication.

## Import Resume
- Checkpoint file stores last committed XML offset and counters.
- Checkpoints advance only after successful DB commit.

## Resource Profiles
- Low/Medium/High profiles tune SQLite pragmas and cache sizing.
- Disk and RAM are checked before import; users are warned if low.

## Observability
- CLI outputs summary metrics per run.
- Verify command checks integrity, FTS sync, and orphans.

## Security Notes
- External entities are not expanded.
- UTF-16 XML is rejected unless explicitly supported.
- Logs should avoid message body content by default.
