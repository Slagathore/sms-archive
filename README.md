# SMS Archive Manager

Rust desktop app for loading, indexing, searching, and analyzing large SMS/MMS XML exports (the Android Backup & Restore / SMS Backup & Restore format). It handles exports that are too big to open in anything else, and everything runs locally.

## What it does

- Streams the XML in, so a multi gigabyte export gets processed without loading the whole file into memory.
- Stores messages in SQLite with an FTS5 full text index, with Unicode normalization across all messages.
- Runs a media pipeline: thumbnails, content hashing for exact duplicate detection, EXIF extraction, and video keyframe extraction.
- Does semantic search over images and text using CLIP embeddings.
- Classifies NSFW media with a local ONNX model. No data leaves the machine.
- Includes an Ollama assistant so you can chat with a local LLM over your archive.
- Ships a GUI (egui: search, timeline, media gallery, import) and a CLI for ingest, search, export, and test data generation.
- No cloud, no telemetry. It all runs on your machine.

## Prerequisites

| Tool                                                           | Version                            | Notes                                         |
| -------------------------------------------------------------- | ---------------------------------- | --------------------------------------------- |
| [Rust](https://rustup.rs/)                                     | stable (see `rust-toolchain.toml`) | Install via rustup                            |
| [Python](https://www.python.org/)                              | 3.10+                              | Only needed to generate ML models             |
| [Tesseract OCR](https://github.com/UB-Mannheim/tesseract/wiki) | 5.x                                | Optional; enables OCR on images               |
| [FFmpeg](https://ffmpeg.org/download.html)                     | 7.x / 8.x                          | Video keyframes plus HEIC/HEIF image decoding |
| [libheif](https://github.com/strukturag/libheif)               | 1.x                                | Optional; native HEIC via the `heic` feature (ffmpeg fallback covers HEIC without it) |
| CUDA Toolkit                                                   | 11.x / 12.x                        | Optional; enables GPU inference for CLIP/NSFW |

> **Windows note:** Add `tesseract.exe`, `ffmpeg.exe`, and `libheif` DLLs to your `PATH`, or set the
> `tesseract_cmd` key in `config/app_global_settings.json` to the full path.

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

This produces every model file `config/app_global_settings.json` points at by default:

- **`ml/CLIP1/`** is the CLIP ViT-L/14 vision encoder, text encoder, and tokenizer
  (`vision_model_fp16.onnx`, `text_model_fp16.onnx`, `tokenizer.json`, plus
  supporting config files), downloaded pre-converted from the
  [`Xenova/clip-vit-large-patch14`](https://huggingface.co/Xenova/clip-vit-large-patch14)
  Transformers.js ONNX export on Hugging Face. This is a single download,
  not a local export. The split vision/text ONNX graphs aren't something
  `open_clip`/`torch.onnx.export` reproduces.
- **`ml/nsfw_classifier.onnx`** is the fallback NSFW model (a small MLP head
  over CLIP embeddings, from LAION's CLIP based detector), exported locally
  from the AutoKeras SavedModel bundle in `ml/clip_autokeras_binary_nsfw/`.

It only needs to run once; later runs skip any file that already exists.
Downloaded CLIP1 files are verified against pinned SHA256 checksums in
`scripts/setup_ml_models.py` (see `EXPECTED_SHA256`); see
[`docs/PRIVACY.md`](docs/PRIVACY.md) for details.

> For GPU inference install `onnxruntime-gpu` instead of `onnxruntime` and ensure CUDA is available.

#### Optional: more accurate NSFW classifier

`ml/nsfw_classifier.onnx` (above) is only the fallback. For better accuracy,
export the preferred Marqo image-input classifier (~21 MB, scores frames
directly instead of embeddings):

```powershell
python scripts/setup_marqo_nsfw.py
```

This produces `ml/nsfw_marqo_384.onnx`; the app detects the model kind
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

## CLI Usage

All commands run via the `sms` binary. Run from the repo root so relative config paths resolve correctly.

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

Full text search over messages:

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

## GUI App

```powershell
cargo run --bin sms-archive --release
```

The GUI provides:

- **Search** tab: FTS5 full text search with filters and thread context view
- **Import** tab: visual ingest pipeline with progress, pause/resume, and checkpoint recovery
- **Media** tab: gallery view with NSFW filter, semantic image search, keyframe viewer
- **Timeline** tab: per-contact message timeline
- **Assistant** tab: Ollama-backed LLM chat over the archive

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

Personal runtime state (DB paths, UI preferences, import history). This one is not committed; copy it from `app_ui_settings.example.json` to get started. The GUI writes it back automatically.

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

ml/                               # Downloaded/generated ONNX models (gitignored, see setup; see ml/README.md)
scripts/
  setup_ml_models.py              # Downloads CLIP1 bundle + exports fallback NSFW model to ONNX
  setup_marqo_nsfw.py             # Exports the preferred Marqo NSFW model to ONNX
docs/                             # Architecture, benchmarks, privacy notes
```

## Running Tests

```powershell
cargo test --workspace
```

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your option.
