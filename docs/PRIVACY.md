# Privacy

## Local-First by Default
SMS Archive Manager operates fully locally. Your SMS/MMS data stays on your machine unless you explicitly export or share it.

## Telemetry
- No telemetry is sent by default.
- Crash reporting, if added, must be opt-in and redact usernames, file paths, and message content.

## Logs
- Logs should avoid message bodies and attachments.
- When diagnostics are needed, only metadata (counts, timings, offsets) should be recorded.

## Model Downloads
- If ML models are downloaded, verify checksum/signature before use.
- Provide an offline mode that never downloads assets.

## Data Deletion
- Deleting the database file and media directory removes all imported data.
- Provide clear UI messaging about data locations.
