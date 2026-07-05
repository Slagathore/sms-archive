# SMS Archive Manager

High-performance Rust desktop application for ingesting, indexing, searching, and analyzing large SMS/MMS XML exports (Android Backup & Restore / SMS Backup & Restore format).

## Features

- **Streaming XML ingest** — processes multi-GB XML exports without loading the entire file into memory
- **SQLite + FTS5 search** — full-text search with Unicode normalization across all messages
- **Media pipeline** — thumbnail generation, content hashing (exact-duplicate detection), EXIF extraction, video keyframe extraction
- **Semantic search** — CLIP-based image and text embeddings for similarity search
- **NSFW classification** — local ONNX-based classifier, no data leaves the machine
- **AI assistant** — Ollama integration for on-device LLM chat over your archive
- **GUI** — egui desktop app with search, timeline, media gallery, and import UI
- **CLI** — scriptable interface for ingest, search, export, and data generation
- **Privacy-first** — entirely local; no cloud, no telemetry

---

## Prerequisites

| Tool                                                           | Version                            | Notes                                         |
| -------------------------------------------------------------- | ---------------------------------- | --------------------------------------------- |
| [Rust](https://rustup.rs/)                                     | stable (see `rust-toolchain.toml`) | Install via rustup                            |
| [Python](https://www.python.org/)                              | 3.10+                              | Only needed to generate ML models             |
| [Tesseract OCR](https://github.com/UB-Mannheim/tesseract/wiki) | 5.x                                | Optional; enables OCR on images               |
| [FFmpeg](https://ffmpeg.org/download.html)                     | 7.x / 8.x                          | Video keyframes + HEIC/HEIF image decoding    |
| [libheif](https://github.com/strukturag/libheif)               | 1.x                                | Optional; native HEIC via the `heic` feature (ffmpeg fallback covers HEIC without it) |
| CUDA Toolkit                                                   | 11.x / 12.x                        | Optional; enables GPU inference for CLIP/NSFW |

> **Windows note:** Add `tesseract.exe`, `ffmpeg.exe`, and `libheif` DLLs to your `PATH`, or set the
> `tesseract_cmd` key in `config/app_global_settings.json` to the full path.

---

## Setup

### 1. Clone the repo

```powershell
git clone https://github.com/YOUR_USERNAME/sms-archive.git
cd sms-archive
```

### 2. Generate ML models

The ONNX model files are not included in the repo (they are large binary artifacts). Generate them once:

```powershell
pip install -r requirements.txt
python scripts/setup_ml_models.py
```

This exports the CLIP ViT-L/14 visual encoder and the NSFW classifier MLP to `ml/`. It only needs to run once; subsequent runs are skipped if the files already exist.

> For GPU inference install `onnxruntime-gpu` instead of `onnxruntime` and ensure CUDA is available.

#### Optional: higher-accuracy NSFW classifier

The default NSFW model is a small head over CLIP embeddings
(`ml/nsfw_classifier.onnx`, from LAION's CLIP-based detector). For better
accuracy, export the Marqo image-input classifier (~21 MB, scores frames
directly instead of embeddings):

```powershell
python scripts/setup_marqo_nsfw.py
```

This produces `ml/nsfw_marqo_384.onnx`; the app auto-detects the model kind
from its input shape, and autofill prefers the Marqo model when both exist.

### 3. Configure paths

Copy the UI settings template:

```powershell
cp config/app_ui_settings.example.json config/app_ui_settings.json
```

Edit `config/app_global_settings.json` if your Tesseract installation is not on `PATH`:

```json
{
  "tesseract_cmd": "C:\\Program Files\\Tesseract-OCR\\tesseract.exe"
}
```

Model paths default to `./ml/...` relative to the working directory and do not need to be changed if you run from the repo root.

### 4. Build

```powershell
cargo build --release
```

---

## CLI Usage

All commands are run via the `sms` binary. Run from the repo root so relative config paths resolve correctly.

```powershell
cargo run --bin sms -- --help
```

### Ingest

Import an SMS XML backup into a SQLite database:

```powershell
cargo run --bin sms -- ingest --input path\to\sms.xml --db sms.db --media-dir .\media --verify
```

| Flag                        | Description                                     |
| --------------------------- | ----------------------------------------------- |
| `--writer-mode interactive` | Smaller write batches; better GUI concurrency   |
| `--atomic`                  | Import to a temp DB then rename atomically      |
| `--overwrite`               | Replace an existing DB (creates a backup first) |

### Doctor

Repair FTS index and validate database integrity:

```powershell
cargo run --bin sms -- doctor --db sms.db --rebuild-fts
```

### Verify

Cross-check an imported DB against the original XML and a summary JSON:

```powershell
cargo run --bin sms -- verify --db sms.db --summary sms.import_summary.json --rebuild-fts
```

### Search

Full-text search over messages:

```powershell
cargo run --bin sms -- search --db sms.db --query "hello world" --limit 20
```

| Flag     | Description          |
| -------- | -------------------- |
| `--json` | Output as JSON lines |

### Export

Export messages to JSONL or CSV:

```powershell
cargo run --bin sms -- export --db sms.db --format jsonl --limit 1000 --output messages.jsonl
```

Additional filters: `--query`, `--format csv`, `--offset`, `--since`, `--until`, `--address`, `--thread-id`, `--message-type sms|mms|rcs`, `--with-attachments`, `--address-like`, `--body-contains`

### Export Attachments

```powershell
cargo run --bin sms -- export-attachments --db sms.db --format csv --output attachments.csv
```

Additional filters: `--mime`, `--since`, `--address`, `--thread-id`, `--message-type`, `--address-like`, `--body-contains`

### Tantivy Index (Optional, currently broken)

> **Status:** the Tantivy backend does not currently compile against the pinned
> `tantivy 0.22` (the code targets an older API). The `tantivy-*` subcommands are
> hidden unless the feature is enabled. FTS5 is the primary, fully supported
> search backend; treat the commands below as aspirational until the backend is
> fixed or removed.

For high-performance full-text search using Tantivy (in addition to FTS5):

```powershell
cargo run --bin sms --features tantivy -- tantivy-build --db sms.db --index-dir tantivy-index --rebuild
cargo run --bin sms --features tantivy -- tantivy-update --db sms.db --index-dir tantivy-index
cargo run --bin sms --features tantivy -- tantivy-search --index-dir tantivy-index --query "hello" --limit 20
```

Additional filters: `--address`, `--thread-id`, `--message-type`, `--since`

### Synthetic Data Generation

Generate a synthetic SMS XML for testing and benchmarking:

```powershell
cargo run --bin sms -- datagen --output test.xml --size 0.01
```

| Flag               | Description                       |
| ------------------ | --------------------------------- |
| `--seed 42`        | Reproducible output               |
| `--mms-ratio 0.1`  | Fraction of messages that are MMS |
| `--burstiness 0.2` | Conversation burst factor         |

---

## GUI App

```powershell
cargo run --bin sms-archive --release
```

The GUI provides:

- **Search** tab — FTS5 full-text search with filters and thread context view
- **Import** tab — visual ingest pipeline with progress, pause/resume, and checkpoint recovery
- **Media** tab — gallery view with NSFW filter, semantic image search, keyframe viewer
- **Timeline** tab — per-contact message timeline
- **Assistant** tab — Ollama-backed LLM chat over the archive

---

## Configuration

### `config/app_global_settings.json`

Shared settings committed to the repo. Paths are relative to the working directory.

| Key                        | Description                                      |
| -------------------------- | ------------------------------------------------ |
| `clip_model_path`          | CLIP visual encoder ONNX path                    |
| `clip_text_model_path`     | CLIP text encoder ONNX path                      |
| `clip_text_tokenizer_path` | CLIP tokenizer JSON path                         |
| `clip_nsfw_weights_path`   | NSFW classifier ONNX path                        |
| `media_embed_prompt`       | Prompt used when Ollama generates media captions |
| `vision_prompt`            | Prompt used for Ollama vision OCR                |
| `tesseract_cmd`            | Tesseract executable name or full path           |

### `config/app_ui_settings.json`

Personal runtime state (DB paths, UI preferences, import history). **Not committed** — copy from `app_ui_settings.example.json` to get started. This file is written back automatically by the GUI.

---

## Workspace Layout

```
crates/
  app/            # egui GUI application
  assistant/      # Ollama LLM assistant integration
  cli/            # CLI binary (sms)
  clip/           # CLIP ONNX inference (image + text embeddings, NSFW)
  config/         # System resource detection and configuration helpers
  datagen/        # Synthetic SMS XML generator
  db/             # SQLite schema, migrations, FTS5, query layer
  errors/         # Shared error types
  ingest/         # Streaming XML parser and import pipeline
  media/          # Thumbnail generation, content hashing, HEIC, video keyframes
  media_process/  # CLI media processing orchestration
  ml/             # ONNX Runtime wrappers (feature-gated)
  perf/           # Benchmarks (criterion)
  search/         # FTS5 search backend
  types/          # Shared domain types (Message, Attachment, Thread, etc.)

config/
  app_global_settings.json        # Shared settings (committed)
  app_ui_settings.json            # Personal runtime state (gitignored)
  app_ui_settings.example.json    # Template for app_ui_settings.json

ml/                               # Generated ONNX models (gitignored, see setup)
scripts/
  setup_ml_models.py              # Exports CLIP + NSFW models to ONNX
docs/                             # Architecture, benchmarks, privacy notes
```

---

## Running Tests

```powershell
cargo test --workspace
```

---

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
