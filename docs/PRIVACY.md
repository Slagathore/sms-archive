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
  `scripts/setup_ml_models.py` downloads the CLIP1 bundle (vision/text
  encoders + tokenizer) from Hugging Face and checks each file's SHA256
  against `EXPECTED_SHA256` in that script. Those hashes are placeholders
  until a maintainer runs the script with network access once and pins the
  printed values (see the `EXPECTED_SHA256` TODO in the script) — until
  pinned, downloads are still fetched and hashed, but integrity is not yet
  enforced against a known-good value. `ml/nsfw_classifier.onnx` is built
  locally (not downloaded) from an already-on-disk AutoKeras bundle, so no
  network-transport checksum applies to it.
- Provide an offline mode that never downloads assets.

## Data Deletion
- Deleting the database file and media directory removes all imported data.
- Provide clear UI messaging about data locations.
