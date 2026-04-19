# SETUP.md: Workspace Scaffold Guide

**Purpose**: Take you from empty directory to fully configured Rust workspace ready for Phase 0 development.

**Time**: ~15 minutes

**Prerequisites**:

- Rust stable installed (`rustup` recommended)
- Git installed
- 2GB free disk space

---

## Quick Start (Copy-Paste)

```bash
# Clone or create project directory
mkdir sms-archive && cd sms-archive
git init

# Run scaffold script (see below) or follow manual steps
```

---

## Step 1: Create Directory Structure

### Bash/Linux/macOS:

```bash
#!/bin/bash
# scaffold.sh

set -e

echo "  Creating SMS Archive workspace..."

# Create crate directories
mkdir -p crates/{types,errors,config,ingest,db,search,media,ml,perf,datagen,cli,app}
mkdir -p docs tests/integration tests/fixtures
mkdir -p .cargo

# Create placeholder files
touch crates/types/Cargo.toml
touch crates/errors/Cargo.toml
touch crates/config/Cargo.toml
touch crates/ingest/Cargo.toml
touch crates/db/Cargo.toml
touch crates/search/Cargo.toml
touch crates/media/Cargo.toml
touch crates/ml/Cargo.toml
touch crates/perf/Cargo.toml
touch crates/datagen/Cargo.toml
touch crates/cli/Cargo.toml
touch crates/app/Cargo.toml

# Create src directories
for crate in types errors config ingest db search media ml perf datagen; do
    mkdir -p crates/$crate/src
    touch crates/$crate/src/lib.rs
done

# Binary crates need main.rs
mkdir -p crates/cli/src crates/app/src
touch crates/cli/src/main.rs
touch crates/app/src/main.rs

echo " Directory structure created"
```

### PowerShell (Windows):

```powershell
# scaffold.ps1

Write-Host "  Creating SMS Archive workspace..." -ForegroundColor Green

# Create directories
$crates = @("types","errors","config","ingest","db","search","media","ml","perf","datagen","cli","app")
foreach ($crate in $crates) {
    New-Item -Path "crates/$crate/src" -ItemType Directory -Force | Out-Null
    New-Item -Path "crates/$crate/Cargo.toml" -ItemType File -Force | Out-Null
}

New-Item -Path "docs" -ItemType Directory -Force | Out-Null
New-Item -Path "tests/integration" -ItemType Directory -Force | Out-Null
New-Item -Path "tests/fixtures" -ItemType Directory -Force | Out-Null
New-Item -Path ".cargo" -ItemType Directory -Force | Out-Null

# Create lib.rs for library crates
$libCrates = @("types","errors","config","ingest","db","search","media","ml","perf","datagen")
foreach ($crate in $libCrates) {
    New-Item -Path "crates/$crate/src/lib.rs" -ItemType File -Force | Out-Null
}

# Create main.rs for binary crates
New-Item -Path "crates/cli/src/main.rs" -ItemType File -Force | Out-Null
New-Item -Path "crates/app/src/main.rs" -ItemType File -Force | Out-Null

Write-Host " Directory structure created" -ForegroundColor Green
```

**Make it executable and run:**

```bash
chmod +x scaffold.sh
./scaffold.sh
```

---

## Step 2: Root Workspace Configuration

### `Cargo.toml` (root)

```toml
[workspace]
resolver = "2"
members = [
    "crates/types",
    "crates/errors",
    "crates/config",
    "crates/ingest",
    "crates/db",
    "crates/search",
    "crates/media",
    "crates/ml",
    "crates/perf",
    "crates/datagen",
    "crates/cli",
    "crates/app",
]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT OR Apache-2.0"
authors = ["Your Name <you@example.com>"]
repository = "https://github.com/yourusername/sms-archive"

[workspace.dependencies]
# Core async runtime
tokio = { version = "1.35", features = ["full"] }
tokio-util = "0.7"

# Serialization
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# Error handling
thiserror = "1.0"
anyhow = "1.0"

# Logging & tracing
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
tracing-appender = "0.2"

# Database
rusqlite = { version = "0.31", features = ["bundled", "backup", "blob", "chrono"] }

# XML parsing
quick-xml = { version = "0.31", features = ["serialize"] }

# String searching (SIMD)
memchr = "2.7"

# Concurrency
crossbeam-channel = "0.5"
rayon = "1.8"

# Memory mapping
memmap2 = "0.9"

# Unicode normalization
unicode-normalization = "0.1"

# CLI parsing
clap = { version = "4.4", features = ["derive", "cargo"] }

# Progress bars
indicatif = "0.17"

# System info
sysinfo = "0.30"

# Date/time
chrono = { version = "0.4", features = ["serde"] }

# UUID generation
uuid = { version = "1.6", features = ["v4", "serde"] }

# Hashing
blake3 = "1.5"
sha2 = "0.10"

# Image processing
image = "0.24"
img_hash = "3.2"

# ONNX Runtime (optional)
ort = { version = "2.0.0-rc.2", optional = true }

# GUI framework
egui = "0.25"
eframe = { version = "0.25", default-features = false, features = ["glow", "persistence"] }

# LRU cache
lru = "0.12"

# Random generation (for datagen)
rand = "0.8"

# File system utilities
fs2 = "0.4"
walkdir = "2.4"

# Metrics
metrics = "0.22"
metrics-exporter-prometheus = { version = "0.13", optional = true }

[profile.dev]
opt-level = 0
debug = true

[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
strip = true
panic = "abort"

[profile.bench]
inherits = "release"
strip = false
debug = true

[profile.dev.package."*"]
opt-level = 2  # Optimize dependencies even in dev mode
```

---

## Step 3: Individual Crate Configurations

### `crates/types/Cargo.toml`

```toml
[package]
name = "sms-types"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
uuid = { workspace = true }
chrono = { workspace = true }
```

Create stub `crates/types/src/lib.rs`:

```rust
//! Core data types for SMS archive

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub message_id: Option<String>,
    pub dedupe_hash: Option<[u8; 32]>,
    pub timestamp: i64,
    pub address: String,
    pub body: String,
    pub body_searchable: String,
    pub message_type: MessageType,
    pub direction: MessageDirection,
    pub thread_id: Option<String>,
    pub attachments: Vec<AttachmentRef>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(i32)]
pub enum MessageType {
    Sms = 1,
    Mms = 2,
    Rcs = 3,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[repr(i32)]
pub enum MessageDirection {
    Unknown = 0,
    Incoming = 1,
    Outgoing = 2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub id: Uuid,
    pub mime_type: String,
    pub file_path: String,
    pub file_hash: [u8; 32],
    pub thumbnail_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_roundtrip_serde() {
        let msg = Message {
            id: Uuid::new_v4(),
            message_id: Some("test123".into()),
            dedupe_hash: Some([1u8; 32]),
            timestamp: 1234567890,
            address: "+15551234567".into(),
            body: "Hello world".into(),
            body_searchable: "hello world".into(),
            message_type: MessageType::Sms,
            direction: MessageDirection::Outgoing,
            thread_id: Some("thread-1".into()),
            attachments: Vec::new(),
        };

        let json = serde_json::to_string(&msg).unwrap();
        let decoded: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg.id, decoded.id);
    }
}
```

---

### `crates/errors/Cargo.toml`

```toml
[package]
name = "sms-errors"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
thiserror = { workspace = true }
rusqlite = { workspace = true }
```

Create stub `crates/errors/src/lib.rs`:

```rust
//! Unified error types for SMS archive

use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Parse error at offset {offset}: {details}")]
    Parse { offset: u64, details: String },

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Insufficient disk space: need {needed} GB, have {available} GB")]
    InsufficientDisk { needed: u64, available: u64 },

    #[error("XML entity bomb detected")]
    XmlBomb,

    #[error("Unsupported encoding: {0}")]
    UnsupportedEncoding(String),

    #[error("Checkpoint corrupted")]
    CheckpointCorrupted,

    #[error("Import cancelled by user")]
    Cancelled,

    #[error("FTS5 unavailable in SQLite build")]
    Fts5Unavailable,
}

pub type Result<T> = std::result::Result<T, AppError>;
```

---

### `crates/config/Cargo.toml`

```toml
[package]
name = "sms-config"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
sysinfo = { workspace = true }
tracing = { workspace = true }

sms-errors = { path = "../errors" }
```

Create stub `crates/config/src/lib.rs`:

```rust
//! Configuration and resource detection

use sms_errors::Result;
use sysinfo::{System, SystemExt};

#[derive(Debug, Clone, Copy)]
pub enum ResourceProfile {
    Low,    // RAM < 8GB
    Medium, // RAM 8-16GB
    High,   // RAM > 16GB
}

impl ResourceProfile {
    pub fn detect() -> Self {
        let mut sys = System::new_all();
        sys.refresh_memory();

        let total_ram_gb = sys.total_memory() / (1024 * 1024);

        match total_ram_gb {
            0..=8192 => Self::Low,
            8193..=16384 => Self::Medium,
            _ => Self::High,
        }
    }
}

pub struct SystemResources {
    pub total_ram_bytes: u64,
    pub cpu_cores: usize,
}

impl SystemResources {
    pub fn detect() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();

        Self {
            total_ram_bytes: sys.total_memory() * 1024,
            cpu_cores: sys.physical_core_count().unwrap_or(1),
        }
    }
}
```

---

### `crates/ingest/Cargo.toml`

```toml
[package]
name = "sms-ingest"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
quick-xml = { workspace = true }
memchr = { workspace = true }
memmap2 = { workspace = true }
unicode-normalization = { workspace = true }
crossbeam-channel = { workspace = true }
rayon = { workspace = true }
tracing = { workspace = true }

sms-types = { path = "../types" }
sms-errors = { path = "../errors" }
```

Create stub `crates/ingest/src/lib.rs`:

```rust
//! XML parsing and boundary detection

use sms_errors::Result;
use sms_types::Message;

pub struct MessageBoundary {
    pub start_offset: u64,
    pub end_offset: u64,
}

pub fn scan_boundaries(file_path: &std::path::Path) -> Result<Vec<MessageBoundary>> {
    // TODO: Implement in Phase 1
    Ok(Vec::new())
}

pub fn parse_message_at_offset(
    file: &std::fs::File,
    boundary: MessageBoundary,
) -> Result<Message> {
    // TODO: Implement in Phase 1
    unimplemented!()
}
```

---

### `crates/db/Cargo.toml`

```toml
[package]
name = "sms-db"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
rusqlite = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
chrono = { workspace = true }
tracing = { workspace = true }

sms-types = { path = "../types" }
sms-errors = { path = "../errors" }
sms-config = { path = "../config" }
```

Create stub `crates/db/src/lib.rs`:

```rust
//! SQLite database abstraction

use rusqlite::Connection;
use sms_errors::Result;
use sms_config::ResourceProfile;

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &std::path::Path, profile: ResourceProfile) -> Result<Self> {
        let conn = Connection::open(path)?;
        ensure_fts5_enabled(&conn)?;
        apply_pragmas(&conn, profile)?;
        Ok(Self { conn })
    }
}

fn apply_pragmas(conn: &Connection, profile: ResourceProfile) -> Result<()> {
    // TODO: Implement pragma application from bootstrap
    conn.execute_batch("PRAGMA journal_mode=WAL")?;
    Ok(())
}

fn ensure_fts5_enabled(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA compile_options")?;
    let mut rows = stmt.query([])?;
    let mut has_fts5 = false;
    while let Some(row) = rows.next()? {
        let opt: String = row.get(0)?;
        if opt.contains("FTS5") {
            has_fts5 = true;
            break;
        }
    }

    if !has_fts5 {
        return Err(sms_errors::AppError::Fts5Unavailable);
    }
    Ok(())
}
```

---

### `crates/search/Cargo.toml`

```toml
[package]
name = "sms-search"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
rusqlite = { workspace = true }
tracing = { workspace = true }

sms-types = { path = "../types" }
sms-errors = { path = "../errors" }
sms-db = { path = "../db" }
```

Create stub `crates/search/src/lib.rs`:

```rust
//! FTS5 search backend

use sms_errors::Result;
use sms_types::Message;

pub trait SearchBackend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>>;
}

pub struct Fts5Backend {
    // TODO: Add connection
}

impl SearchBackend for Fts5Backend {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<Message>> {
        // TODO: Implement FTS5 search
        Ok(Vec::new())
    }
}
```

---

### `crates/media/Cargo.toml`

```toml
[package]
name = "sms-media"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
image = { workspace = true }
img_hash = { workspace = true }
blake3 = { workspace = true }
tracing = { workspace = true }

sms-types = { path = "../types" }
sms-errors = { path = "../errors" }
```

Create stub `crates/media/src/lib.rs`:

```rust
//! Media processing (thumbnails, hashing)

use sms_errors::Result;

pub fn generate_thumbnail(
    source: &std::path::Path,
    dest: &std::path::Path,
    max_size: u32,
) -> Result<()> {
    // TODO: Implement thumbnail generation
    Ok(())
}

pub fn hash_file(path: &std::path::Path) -> Result<[u8; 32]> {
    // TODO: Implement file hashing with BLAKE3
    Ok([0u8; 32])
}
```

---

### `crates/ml/Cargo.toml`

```toml
[package]
name = "sms-ml"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
tracing = { workspace = true }

sms-types = { path = "../types" }
sms-errors = { path = "../errors" }

# Optional ONNX Runtime
ort = { workspace = true, optional = true }

[features]
default = []
onnx = ["ort"]
```

Create stub `crates/ml/src/lib.rs`:

```rust
//! ML embeddings and inference

use sms_errors::Result;

pub fn generate_embedding(text: &str) -> Result<Vec<f32>> {
    // TODO: Implement ONNX inference
    Ok(vec![0.0; 384])  // Placeholder: 384-dim embedding
}
```

---

### `crates/perf/Cargo.toml`

```toml
[package]
name = "sms-perf"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
criterion = "0.5"

sms-ingest = { path = "../ingest" }
sms-db = { path = "../db" }

[[bench]]
name = "boundary_detection"
harness = false

[[bench]]
name = "db_writes"
harness = false
```

Create stub `crates/perf/src/lib.rs`:

```rust
//! Benchmark harness utilities

pub fn setup_test_db() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}
```

Create `crates/perf/benches/boundary_detection.rs`:

```rust
use criterion::{criterion_group, criterion_main, Criterion};

fn bench_boundary_scan(c: &mut Criterion) {
    c.bench_function("boundary_scan_1mb", |b| {
        b.iter(|| {
            // TODO: Implement benchmark
        });
    });
}

criterion_group!(benches, bench_boundary_scan);
criterion_main!(benches);
```

---

### `crates/datagen/Cargo.toml`

```toml
[package]
name = "sms-datagen"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
rand = { workspace = true }
chrono = { workspace = true }
tracing = { workspace = true }

sms-errors = { path = "../errors" }
```

Create stub `crates/datagen/src/lib.rs`:

```rust
//! Synthetic test data generator

use sms_errors::Result;
use std::path::Path;

pub struct DataGenConfig {
    pub target_size_gb: f64,
    pub avg_message_size_bytes: usize,
}

pub fn generate_xml(config: DataGenConfig, output: &Path) -> Result<()> {
    // TODO: Implement in Phase 0.5
    Ok(())
}
```

---

### `crates/cli/Cargo.toml`

```toml
[package]
name = "sms-cli"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "sms"
path = "src/main.rs"

[dependencies]
clap = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }

sms-types = { path = "../types" }
sms-errors = { path = "../errors" }
sms-config = { path = "../config" }
sms-ingest = { path = "../ingest" }
sms-db = { path = "../db" }
sms-datagen = { path = "../datagen" }
```

Create `crates/cli/src/main.rs`:

```rust
use clap::{Parser, Subcommand};
use anyhow::Result;

#[derive(Parser)]
#[command(name = "sms")]
#[command(about = "SMS Archive CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Import XML file
    Ingest {
        /// Path to XML file
        #[arg(short, long)]
        input: String,

        /// Enable profiling
        #[arg(long)]
        profile: bool,
    },
    /// Run database health checks
    Doctor {
        /// Path to database
        #[arg(short, long)]
        db: String,
    },
    /// Generate test data
    Datagen {
        /// Output path
        #[arg(short, long)]
        output: String,

        /// Size in GB
        #[arg(short, long, default_value = "1.0")]
        size: f64,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Ingest { input, profile } => {
            println!("Importing from: {}", input);
            // TODO: Implement import
            Ok(())
        }
        Commands::Doctor { db } => {
            println!("Checking database: {}", db);
            // TODO: Implement doctor
            Ok(())
        }
        Commands::Datagen { output, size } => {
            println!("Generating {}GB test data to: {}", size, output);
            // TODO: Implement datagen
            Ok(())
        }
    }
}
```

---

### `crates/app/Cargo.toml`

```toml
[package]
name = "sms-app"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "sms-archive"
path = "src/main.rs"

[dependencies]
eframe = { workspace = true }
egui = { workspace = true }
anyhow = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }

sms-types = { path = "../types" }
sms-errors = { path = "../errors" }
sms-config = { path = "../config" }
sms-db = { path = "../db" }
sms-search = { path = "../search" }
```

Create `crates/app/src/main.rs`:

```rust
use eframe::egui;
use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "SMS Archive",
        options,
        Box::new(|_cc| Box::new(SmsArchiveApp::default())),
    ).map_err(|e| anyhow::anyhow!("eframe error: {}", e))
}

#[derive(Default)]
struct SmsArchiveApp {}

impl eframe::App for SmsArchiveApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("SMS Archive");
            ui.label("Welcome! Phase 2 GUI will go here.");
        });
    }
}
```

---

## Step 4: Additional Configuration Files

### `rust-toolchain.toml`

```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
targets = ["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "x86_64-apple-darwin"]
```

---

### `.cargo/config.toml`

```toml
[build]
# Use faster linker on Linux
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=lld"]

# Windows optimizations
[target.x86_64-pc-windows-msvc]
# NOTE: Static CRT can conflict with bundled native libs (onnx/ffmpeg).
# Enable only if you ship fully static dependencies.
# rustflags = ["-C", "target-feature=+crt-static"]

[alias]
# Convenience aliases
quick = "build --release"
bench-all = "bench --workspace"
lint = "clippy --workspace -- -D warnings"
```

---

### `.gitignore`

```
# Rust
target/
**/*.rs.bk
*.pdb

# IDE
.vscode/
.idea/
*.swp
*.swo
*~

# OS
.DS_Store
Thumbs.db

# App-specific
*.db
*.db-wal
*.db-shm
test_data/
logs/
```

---

## Step 5: Verify Setup

```bash
# Check workspace compiles
cargo check --workspace

# Run tests (should all pass with stubs)
cargo test --workspace

# Build CLI
cargo build --bin sms

# Build GUI
cargo build --bin sms-archive

# Run clippy
cargo clippy --workspace

# Verify benches exist
cargo bench --workspace --no-run
```

**Expected output:**

```
   Compiling sms-types v0.1.0
   Compiling sms-errors v0.1.0
   Compiling sms-config v0.1.0
   ...
   Finished dev [unoptimized + debuginfo] target(s) in 45.3s
```

---

## Step 6: First Run

```bash
# CLI help
cargo run --bin sms -- --help

# GUI
cargo run --bin sms-archive
```

You should see:

- CLI: Help menu with `ingest`, `doctor`, `datagen` commands
- GUI: Window with "Welcome! Phase 2 GUI will go here."

---

## Step 7: Create Documentation Stubs

```bash
# Create doc files
cat > docs/ARCHITECTURE.md << 'EOF'
# Architecture

See RUST_BOOTSTRAP_PLAN.md for details.

## Dependency Graph
[Add mermaid diagram here]
EOF

cat > docs/BENCHMARKS.md << 'EOF'
# Benchmark Results

## Baseline (Phase 0)
TBD

## Phase 1
TBD
EOF

cat > docs/PRIVACY.md << 'EOF'
# Privacy Policy

## Data Collection
- No telemetry by default
- Crash reports are opt-in only
- All data stored locally

## Third-Party Services
None (fully offline)
EOF
```

---

## Next Steps

You now have a fully scaffolded workspace. Proceed to **Phase 0 Day 2**:

1.  Implement `errors::AppError` with all variants
2.  Implement `config::ResourceProfile::detect()`
3.  Add integration test in `tests/integration/`
4.  Write `datagen` to produce 1GB test XML
5.  Set up CI/CD (GitHub Actions)

---

## Troubleshooting

### "Cannot find workspace member"

```bash
# Verify all Cargo.toml files exist
find crates -name Cargo.toml

# Re-run from root
cargo metadata --format-version=1 | jq '.workspace_members'
```

### "Linker error on Windows"

Install Visual Studio Build Tools with C++ support.

### "egui fails to compile"

Missing system dependencies on Linux:

```bash
# Ubuntu/Debian
sudo apt-get install libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
     libxkbcommon-dev libssl-dev

# Fedora
sudo dnf install libxcb-devel libxkbcommon-devel openssl-devel
```

---

## Verification Checklist

Before moving to Phase 0 Day 2:

- [ ] `cargo check --workspace` passes
- [ ] `cargo test --workspace` passes
- [ ] `cargo run --bin sms -- --help` shows commands
- [ ] `cargo run --bin sms-archive` launches GUI
- [ ] `cargo clippy --workspace` has no errors
- [ ] All 12 crates have `Cargo.toml` + `src/`
- [ ] Git repo initialized with `.gitignore`
- [ ] CI/CD stub exists (even if empty)

---

** Workspace scaffold complete! Ready for Phase 0 implementation.**
