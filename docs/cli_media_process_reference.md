# Mission statement

This document describes the sms CLI media-process command that triggers CLIP embeddings and NSFW classification on media attachments.

## Module overview

- crates/cli/src/main.rs: MediaProcess subcommand wiring.
- crates/media_process/src/lib.rs: Shared processing logic.

## Command summary

- sms media-process
  - --db: database path
  - --clip-model: CLIP ONNX model path
  - --nsfw-weights: NSFW probe path (safetensors/npz)
  - --batch-size: GPU batch size (default 32)
  - --max-keyframes: frames per video (default 5)
  - --reprocess: re-run on processed items
  - --limit: limit items
  - --workers: parallel image loader threads
  - --media-root: override media directory
  - --dry-run: list count only

## Data flow

1. Query unprocessed attachments.
2. Extract keyframes for videos.
3. Preprocess images for CLIP.
4. Run batch CLIP inference.
5. Insert embeddings + NSFW updates.

## #todo

- #todo: Add JSON output for pipeline stats.
- #todo: Add resume checkpoint support.
