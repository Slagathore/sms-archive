# Mission statement

This document describes the sms-media keyframe utilities used to extract consistent frames from video attachments for embeddings and NSFW classification.

## Module overview

- crates/media/src/keyframes.rs: Keyframe extraction and cleanup utilities.

## Modules and classes used

- sms_media::generate_thumbnail_for_mime: Fallback thumbnail extraction.
- tempfile::TempDir: Temporary directory management.
- std::process::Command: ffmpeg CLI invocation.

## Public structs and functions

- Keyframe
  - path: Path to the extracted frame image.
  - index: Frame index within the extraction batch.
  - time_ms: Optional timestamp (currently none).

- extract_keyframes(): Extracts keyframes for videos or returns a single frame for images.
- cleanup_temp_dir(): Drops TempDir to clean up.

## #todo

- #todo: Extract per-frame timestamps with ffprobe.
- #todo: Allow configurable JPEG quality for keyframes.
