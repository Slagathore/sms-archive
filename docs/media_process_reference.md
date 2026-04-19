# Mission statement

This document describes the sms-media-process crate, which orchestrates CLIP media embedding and NSFW classification in batch for the SMS Archive database.

## Module overview

- crates/media_process/src/lib.rs: Public API for media processing pipeline and batch statistics.

## Modules and classes used

- sms_clip::ClipEncoder: CLIP encoder + NSFW probe.
- sms_db::{Database, get_unprocessed_media, insert_media_results_batch}: Database access and inserts.
- sms_media::keyframes::extract_keyframes: Keyframe extraction for video processing.
- image::DynamicImage: Image loading for frames.
- rayon::ThreadPoolBuilder: Parallel image loading.
- indicatif::ProgressBar: CLI progress indicator.

## Public structs and functions

- MediaProcessOptions
  - db_path: Path to SQLite database.
  - clip_model: Path to CLIP ONNX model.
  - nsfw_weights: Path to NSFW probe weights.
  - batch_size: GPU batch size.
  - max_keyframes: Max frames per video.
  - reprocess: Whether to reprocess existing items.
  - limit: Optional row limit.
  - workers: Image loading workers.
  - show_progress: Whether to show progress bars.
  - media_root: Optional media root override.

- MediaProcessStats
  - total_tasks: Total attachments considered.
  - processed_tasks: Attachments processed.
  - embedded_frames: Frame embeddings written.
  - nsfw_updated: Attachments with NSFW updated.
  - elapsed_ms: Duration.

- process_media(): Orchestrates media batch processing.
  - Registers media embeddings as model name "clip-vit-l-14-224" with version "clip-media".

## Internal helpers

- FrameBatch: Container for loaded frame images and metadata.
- FrameMeta: Maps a frame to attachment id and timing data.
- load_frames(): Loads keyframes in parallel.
- resolve_media_root(): Resolves media root from override or app_settings.
- resolve_media_path(): Resolves relative attachment paths.
- build_db_rows(): Maps CLIP outputs to DB insert rows.

## #todo

- #todo: Add incremental processing for new imports.
- #todo: Add video frame timestamp extraction via ffprobe.
- #todo: Add robust retry logic for corrupt images.
