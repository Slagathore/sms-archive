# App CLIP Processing Reference

Mission statement: Document the CLIP media processing integration inside the SMS Archive app, including the state fields, helpers, and external modules it calls to deliver fast LAION-based CLIP embeddings and NSFW scores for attachments.

## Module scope

This document covers the CLIP media processing section added to the app UI and logic in the app main module.

### Modules referenced

- `sms_media_process`: Provides `process_media` and `MediaProcessOptions` used to run the CLIP + LAION pipeline.
- `sms_media`: Supplies media root resolution helpers and keyframe utilities indirectly through the pipeline.
- `sms_db`: Used by the pipeline for reading attachments and writing embeddings/NSFW labels.
- `sms_clip`: Provides CUDA probing helpers for CLIP execution.
- `std::env`: Used to set `SMS_CLIP_USE_CUDA` for optional GPU acceleration.
- `std::thread`: Used to run CLIP processing in a background job.

## Structs

### `MediaProcessJob`

- **Purpose:** Tracks the background CLIP processing worker thread and its start time.
- **Fields:**
  - `progress`: Shared pause/cancel flags and progress counters for the CLIP worker.
  - `handle`: Join handle for the CLIP worker thread.
  - `started_at`: Timestamp for elapsed-time display.

## App state fields (CLIP-related)

- `clip_model_path`: Path to the CLIP image encoder ONNX file.
- `clip_nsfw_weights_path`: Path to LAION NSFW probe weights.
- `clip_batch_size`: Attachments processed per batch.
- `clip_max_keyframes`: Max keyframes extracted per attachment.
- `clip_workers`: Parallel keyframe extraction workers.
- `clip_reprocess`: Whether to recompute existing embeddings.
- `clip_auto_on_import`: Auto-run CLIP after successful import.
- `clip_use_cuda`: Toggle for CUDA execution provider.
- `clip_status`: Last status message for CLIP processing.
- `clip_cuda_status`: Status text for CUDA probe results.
- `clip_job`: Optional `MediaProcessJob` when CLIP is running.
- `clip_job.progress`: Shared pause/cancel flags plus `total`/`done` counters for progress UI.

## Functions

### `start_clip_processing()`

- **Purpose:** Validates CLIP inputs, configures options, sets CUDA flag, and starts the CLIP job thread.
- **Inputs:** Uses app state (DB path, CLIP paths, batch config, media root).
- **Output:** Updates `clip_status` and `clip_job`.
- **Notes:** If CLIP paths are empty or missing, the app auto-fills from ml/ (prefers `ml/clip-vit-l-14.onnx` and `ml/nsfw_classifier.onnx`, with `ml/nsfw_probe.npz` as fallback). The CLIP job also runs GPS EXIF tagging before embeddings.

### `maybe_start_clip_after_import()`

- **Purpose:** Runs CLIP automatically after import if enabled and properly configured.
- **Inputs:** `clip_auto_on_import`, CLIP model/weights paths, `clip_job` state.
- **Output:** Starts CLIP processing or updates `clip_status` with a skip reason.

## UI elements

- CLIP settings and controls are grouped under "CLIP Media Processing (LAION)" in the Media tab.
- Elapsed time is shown while `clip_job` is active.
- A progress bar shows attachments processed vs total while CLIP runs.
- Status messages are displayed via `clip_status`.
- CUDA probe results are displayed via `clip_cuda_status`.
- Pause/Resume and Cancel controls are available while the CLIP job runs.
- CLIP settings can be saved per database using "Save CLIP settings".
- GPS tagging progress and tagged counts are shown while the CLIP job runs.

## #todo

- Add per-frame progress reporting for CLIP processing.
- Add post-processing summary export for CLIP run statistics.
- Add retry/skip controls for failed media items.
