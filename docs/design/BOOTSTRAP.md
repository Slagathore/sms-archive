> **HISTORICAL DESIGN DOC** — this is the pre-implementation blueprint the
> project was built from. It predates the actual code and has not been kept
> in sync; where it disagrees with the crates, the code wins. Do not use it
> as current-state documentation (see `docs/ARCHITECTURE.md` and the
> per-crate `docs/*_reference.md` files instead).

# Rust SMS Archive: Production-Grade Bootstrap (Final)
**Version**: 2.0 (Post-Expert Review)
**Status**: Pre-Implementation Blueprint
**Target**: Cross-platform desktop app for 80GB+ SMS/MMS XML imports

---

## Executive Summary
This document defines the complete architecture, implementation phases, and production hardening required to ship a high-performance Rust application that processes massive SMS XML exports while remaining responsive and reliable on consumer hardware.

**Core Goals:**

- Import 80GB XML in <30 minutes on reference hardware
- Stay under 1GB memory during import
- Maintain 30+ FPS UI during heavy operations
- Support resumable imports with atomic checkpointing
- Ship with SQLite FTS5 search, optional ML embeddings
- Handle consumer reality: low-RAM machines, HDDs, antivirus, thermal throttling

**Timeline:** 3-4 months for solid v1.0

---

## Hard Requirements (Concrete Specs)
### Performance Targets
| Metric            | Target                                     | Reference Hardware              |
| ----------------- | ------------------------------------------ | ------------------------------- |
| Import Speed      | 80GB in 20-30 min                          | Ryzen 5600X, NVMe SSD, 16GB RAM |
| Memory Ceiling    | Never exceed 1GB RSS                       | Measured with platform profiler (massif/Instruments/Process Explorer) |
| UI Responsiveness | <33ms frame time during import, <16ms idle | 30 FPS minimum                  |
| Search Latency    | <500ms for any query                       | 100M+ message corpus            |
| Throughput        | 50k+ messages/sec                          | Sustained, not burst            |

### Resource Requirements Formula
```rust
fn calculate_minimum_resources(xml_size_bytes: u64) -> ResourceRequirements {
    let estimated_db_size = (xml_size_bytes as f64 * 1.2) as u64; // XML->SQLite compression
    let wal_headroom = (estimated_db_size as f64 * 0.5) as u64;    // WAL can be 50% of DB
    let vacuum_space = estimated_db_size * 2;                       // VACUUM needs 2x DB size
    let thumbnail_cache = 10 * 1024_u64.pow(3);                     // 10GB target
    let safety_margin = 5 * 1024_u64.pow(3);                        // 5GB buffer

    ResourceRequirements {
        min_ram: 4 * 1024_u64.pow(3),  // 4GB absolute minimum
        recommended_ram: 8 * 1024_u64.pow(3),
        min_disk: xml_size_bytes + estimated_db_size + wal_headroom
                  + vacuum_space + thumbnail_cache + safety_margin,
        // For 80GB XML: ~380GB disk needed (4.75x multiplier)
    }
}
```

---

## Architecture: Workspace Layout
```
sms-archive/
|-- Cargo.toml                    # Workspace root
|-- rust-toolchain.toml           # Pin Rust version (stable)
|-- .cargo/
|   `-- config.toml               # Build profiles, target configs
|-- crates/
|   |-- types/                    # Core data structures (Message, Attachment, etc.)
|   |-- errors/                   # Unified error types (thiserror)
|   |-- config/                   # Settings, resource profiles, user prefs
|   |-- ingest/                   # XML boundary detection + streaming parser
|   |-- db/                       # SQLite wrapper, migrations, writer
|   |-- search/                   # FTS5 backend (trait for future swap)
|   |-- media/                    # Image/video processing, thumbnails, hashing
|   |-- ml/                       # ONNX embeddings (optional, feature-gated)
|   |-- perf/                     # Benchmarks, profiling harness
|   |-- datagen/                  # Synthetic test data generator
|   |-- cli/                      # Headless import/bench/doctor commands
|   `-- app/                      # Native GUI (egui)
|-- docs/
|   |-- PRIVACY.md
|   |-- ARCHITECTURE.md
|   `-- BENCHMARKS.md
`-- tests/
    |-- integration/
    `-- fixtures/                 # Test XMLs with edge cases
```

### Dependency DAG (Enforced)
```
app    -> ingest, db, search, media, ml, types, errors, config
cli    -> ingest, db, search, types, errors, config
ingest -> types, errors, config
db     -> types, errors, config
search -> db, types, errors, config
media  -> types, errors, config
ml     -> types, errors, config
perf   -> ingest, db, search, types, errors, config
```

**Rules:**

- No circular dependencies (enforced by workspace structure)
- `app` and `cli` are binaries, never dependencies
- Leaf crates use `thiserror`, binaries use `anyhow`

---

## SQLite Configuration: Resource Profiles
### Dynamic PRAGMA Computation
```rust
#[derive(Debug, Clone, Copy)]
pub enum ResourceProfile {
    Low,    // RAM < 8GB, HDD or slow SSD
    Medium, // RAM 8-16GB, SATA SSD
    High,   // RAM > 16GB, NVMe SSD
}

impl ResourceProfile {
    pub fn detect() -> Self {
        let total_ram = sysinfo::System::new_all().total_memory() * 1024; // bytes
        let is_ssd = detect_ssd(); // Via filesystem checks or heuristics

        match (total_ram, is_ssd) {
            (ram, _) if ram < 8 * 1024_u64.pow(3) => Self::Low,
            (ram, true) if ram >= 16 * 1024_u64.pow(3) => Self::High,
            _ => Self::Medium,
        }
    }

    pub fn pragmas(&self, connection_role: ConnectionRole) -> Vec<String> {
        let base = match self {
            Self::Low => vec![
                "PRAGMA journal_mode=WAL".into(),
                "PRAGMA synchronous=NORMAL".into(),
                "PRAGMA cache_size=-256000".into(),      // 256MB page cache (keep under 1GB ceiling)
                "PRAGMA temp_store=FILE".into(),         // Avoid OOM on sorts
                "PRAGMA mmap_size=1000000000".into(),    // 1GB mmap
                "PRAGMA page_size=16384".into(),         // 16KB pages (safer)
            ],
            Self::Medium => vec![
                "PRAGMA journal_mode=WAL".into(),
                "PRAGMA synchronous=NORMAL".into(),
                "PRAGMA cache_size=-512000".into(),      // 512MB
                "PRAGMA temp_store=FILE".into(),
                "PRAGMA mmap_size=5000000000".into(),    // 5GB
                "PRAGMA page_size=32768".into(),         // 32KB pages
            ],
            Self::High => vec![
                "PRAGMA journal_mode=WAL".into(),
                "PRAGMA synchronous=NORMAL".into(),
                "PRAGMA cache_size=-768000".into(),      // 768MB
                "PRAGMA temp_store=MEMORY".into(),       // Fast, risky if low headroom
                "PRAGMA mmap_size=10000000000".into(),   // 10GB
                "PRAGMA page_size=32768".into(),
            ],
        };

        // Per-connection tuning
        let role_specific = match connection_role {
            ConnectionRole::Writer => vec![
                "PRAGMA wal_autocheckpoint=4000".into(), // NOT 0, see below
                "PRAGMA busy_timeout=30000".into(),      // 30s retry on lock
            ],
            ConnectionRole::Reader => vec![
                "PRAGMA query_only=1".into(),            // Safety
            ],
        };

        [base, role_specific].concat()
    }
}
```

### Critical: WAL Checkpoint Strategy
```rust
pub struct CheckpointPolicy {
    mode: CheckpointMode,
    interval: Duration,
}

pub enum CheckpointMode {
    AutoPassive,     // Default, non-blocking
    ManualTruncate,  // After import, shrinks WAL
}

impl CheckpointPolicy {
    pub fn for_import_phase(phase: ImportPhase) -> Self {
        match phase {
            ImportPhase::Headless => Self {
                mode: CheckpointMode::AutoPassive,
                interval: Duration::from_secs(300), // Every 5 min
            },
            ImportPhase::Interactive => Self {
                mode: CheckpointMode::AutoPassive,
                interval: Duration::from_secs(60),  // Every 1 min
            },
        }
    }

    pub async fn post_import_finalize(conn: &Connection) -> Result<()> {
        // 1. Wait for all readers to finish (timeout 30s)
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if Self::check_no_active_readers(conn)? {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        // 2. Truncate checkpoint (blocks readers, shrinks WAL)
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)", [])?;

        // 3. Verify WAL shrank
        let db_path = conn.path().ok_or("No DB path")?;
        let wal_path = PathBuf::from(db_path).with_extension("db-wal");
        let wal_size = std::fs::metadata(&wal_path)?.len();

        if wal_size > 10_000_000 { // >10MB is suspicious
            warn!("WAL did not shrink to <10MB, may indicate stale readers");
        }

        Ok(())
    }
}
```

**Why This Matters:**

- `wal_autocheckpoint=0` is acceptable during bulk import only if you checkpoint/truncate after
- Default `wal_autocheckpoint=1000` (1000 pages ~4MB) is too aggressive for bulk import
- `wal_autocheckpoint=4000` (~16MB) balances throughput vs WAL size
- Post-import `TRUNCATE` checkpoint reclaims disk space

---

## Phase 0: Foundation (Days 1-5)
### Deliverables
1. **Workspace Scaffold**
   - All crates created with proper `Cargo.toml` dependencies
   - `rust-toolchain.toml` pinning stable Rust
   - CI/CD skeleton (GitHub Actions)

2. **Error Taxonomy**

   ```rust
   // errors/src/lib.rs
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

       // ... full taxonomy
   }

   pub type Result<T> = std::result::Result<T, AppError>;
   ```

3. **Core Types**

   ```rust
   // types/src/lib.rs
   use serde::{Deserialize, Serialize};
   use uuid::Uuid;

   #[derive(Debug, Clone, Serialize, Deserialize)]
   pub struct Message {
       pub id: Uuid,                    // Internal ID
       pub message_id: Option<String>,  // From XML (may be missing)
       pub dedupe_hash: Option<[u8; 32]>, // Hash for idempotency when message_id missing
       pub timestamp: i64,              // Unix epoch, UTC
       pub address: String,             // Phone number (normalized)
       pub body: String,                // Normalized UTF-8 (NFC)
       pub body_searchable: String,     // NFKD decomposed for FTS
       pub message_type: MessageType,
       pub thread_id: Option<String>,
       pub attachments: Vec<AttachmentRef>,
   }

   #[derive(Debug, Clone)]
   pub enum MessageType {
       Sms,
       Mms,
       Rcs,
   }

   #[derive(Debug, Clone)]
   pub struct AttachmentRef {
       pub id: Uuid,
       pub mime_type: String,
       pub file_path: PathBuf,       // Relative to storage root
       pub file_hash: [u8; 32],      // SHA-256
       pub thumbnail_path: Option<PathBuf>,
   }
   ```

4. **Logging & Metrics**

   ```toml
   [dependencies]
   tracing = "0.1"
   tracing-subscriber = { version = "0.3", features = ["env-filter"] }
   tracing-appender = "0.2"  # Log rotation
   ```

   ```rust
   use tracing::{info, warn, error, span, Level};
   use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

   pub fn init_logging(log_dir: &Path) -> Result<()> {
       let file_appender = tracing_appender::rolling::daily(log_dir, "sms-archive.log");
       let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

       tracing_subscriber::registry()
           .with(tracing_subscriber::EnvFilter::new("info"))
           .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
           .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
           .init();

       // Metrics: use prometheus or metrics crate
       Ok(())
   }
   ```

5. **Resource Detection**

   ```rust
   // config/src/resources.rs
   use sysinfo::{System, SystemExt, DiskExt};

   pub struct SystemResources {
       pub total_ram_bytes: u64,
       pub available_ram_bytes: u64,
       pub cpu_cores: usize,
       pub storage_type: StorageType,
       pub free_disk_bytes: u64,
   }

   pub enum StorageType {
       Hdd,
       Ssd,
       Nvme,
       Unknown,
   }

   impl SystemResources {
       pub fn detect() -> Self {
           let mut sys = System::new_all();
           sys.refresh_all();

           Self {
               total_ram_bytes: sys.total_memory() * 1024,
               available_ram_bytes: sys.available_memory() * 1024,
               cpu_cores: sys.physical_core_count().unwrap_or(1),
               storage_type: Self::detect_storage_type(),
               free_disk_bytes: Self::detect_free_disk(),
           }
       }

       fn detect_storage_type() -> StorageType {
           // Linux: check /sys/block/*/queue/rotational
           // Windows: Use WMI (MediaType property)
           // macOS: Use diskutil info or IOKit
           StorageType::Unknown  // Placeholder
       }
   }
   ```

---

## Phase 0.5: Synthetic Data Generator (Days 3-5)
### Purpose
Generate test XMLs with realistic patterns + edge cases _before_ writing parser.

### Implementation
```rust
// datagen/src/lib.rs
use rand::Rng;
use chrono::{DateTime, Utc, Duration};

pub struct DataGenConfig {
    pub target_size_gb: f64,
    pub avg_message_size_bytes: usize,
    pub burst_pattern: BurstPattern,
    pub encoding: Encoding,
    pub malformed_ratio: f64,  // 0.0-1.0
    pub emoji_heavy: bool,
}

pub enum BurstPattern {
    Uniform,
    WorkHours,  // 9am-5pm weekdays
    Holiday,    // Spikes on holidays
}

pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf8WithBom,
}

pub fn generate_xml(config: DataGenConfig, output: &Path) -> Result<()> {
    let mut writer = BufWriter::new(File::create(output)?);
    writer.write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n")?;
    writer.write_all(b"<smses count=\"{}\">\n")?;

    let total_messages = (config.target_size_gb * 1e9) as usize
                         / config.avg_message_size_bytes;

    let mut rng = rand::thread_rng();
    let mut timestamp = Utc::now() - Duration::days(365 * 3); // 3 years back

    for i in 0..total_messages {
        let msg = if rng.gen::<f64>() < config.malformed_ratio {
            generate_malformed_message()
        } else {
            generate_realistic_message(&mut rng, &mut timestamp, &config)
        };

        writer.write_all(msg.as_bytes())?;

        if i % 10000 == 0 {
            info!("Generated {}/{} messages", i, total_messages);
        }
    }

    writer.write_all(b"</smses>\n")?;
    Ok(())
}

fn generate_realistic_message(
    rng: &mut impl Rng,
    timestamp: &mut DateTime<Utc>,
    config: &DataGenConfig
) -> String {
    *timestamp += Duration::seconds(rng.gen_range(60..3600)); // 1min-1hr gaps

    let body = if config.emoji_heavy {
        generate_emoji_heavy_text(rng)
    } else {
        generate_normal_text(rng)
    };

    format!(
        r#"  <sms protocol="0" address="+1555{:07}" date="{}" type="1" body="{}" />"#,
        rng.gen_range(0..10_000_000),
        timestamp.timestamp_millis(),
        xml_escape(&body)
    )
}

fn generate_malformed_message() -> String {
    // Missing closing tag, invalid timestamp, XXE attempt, etc.
    r#"  <sms address="+1555" body="broken"#.into()
}

fn generate_emoji_heavy_text(rng: &mut impl Rng) -> String {
    // Mix of text + emoji-like tokens and ZWJ-like sequences (ASCII placeholders)
    let emojis = [":wave_light_skin:", ":pirate_flag:", ":family:", ":facepalm:"];
    format!(
        "Hello {} this is a test {}",
        emojis[rng.gen_range(0..emojis.len())],
        rng.gen::<bool>()
    )
}
```

**Test Corpus:**

- `test_1gb_uniform.xml` - Uniform distribution, clean
- `test_1gb_emoji.xml` - Emoji-heavy, Unicode edge cases
- `test_1gb_malformed.xml` - 10% malformed records
- `test_10gb_bursts.xml` - Realistic burst patterns
- `test_80gb_prod.xml` - Full production simulation

---

## Phase 1: Ingest Pipeline (Weeks 1-3)
### Architecture
```
File Reader (single thread)
  -> Boundary Scanner (finds <sms> offsets)
  -> Offset Index (Vec<(u64, u64)>)
  -> Parser Pool (4-8 threads, quick-xml streaming)
  -> Work Queue (bounded, byte budget)
  -> DB Writer (single thread, batch inserts)
```

### 1. Boundary Scanner (Critical)
```rust
// ingest/src/boundary.rs
use memchr::memmem;

pub struct MessageBoundary {
    pub start_offset: u64,
    pub end_offset: u64,
}

pub fn scan_boundaries(file_path: &Path) -> Result<Vec<MessageBoundary>> {
    let file = File::open(file_path)?;
    let mmap = unsafe { memmap2::Mmap::map(&file)? };

    // CRITICAL: Validate encoding first
    let encoding = detect_encoding(&mmap[..1024])?;
    if encoding != Encoding::Utf8 {
        return Err(AppError::UnsupportedEncoding(encoding));
    }

    let finder = memmem::Finder::new(b"<sms ");  // SIMD-accelerated
    let mut boundaries = Vec::new();
    let mut last_offset = 0;

    for offset in finder.find_iter(&mmap) {
        // Validate this is a real tag start, not text/attribute content
        if offset > 0 && mmap[offset - 1] != b'\n' && mmap[offset - 1] != b'>' {
            continue;  // Likely inside an attribute value
        }

        // Find closing tag (naive scan forward)
        let end = find_message_end(&mmap[offset..])?;

        boundaries.push(MessageBoundary {
            start_offset: offset as u64,
            end_offset: (offset + end) as u64,
        });

        last_offset = offset;
    }

    info!("Found {} message boundaries", boundaries.len());
    Ok(boundaries)
}

fn detect_encoding(sample: &[u8]) -> Result<Encoding> {
    if sample.starts_with(&[0xFF, 0xFE]) {
        Ok(Encoding::Utf16Le)
    } else if sample.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Ok(Encoding::Utf8WithBom)
    } else if sample.is_ascii() || std::str::from_utf8(sample).is_ok() {
        Ok(Encoding::Utf8)
    } else {
        Err(AppError::UnsupportedEncoding(Encoding::Unknown))
    }
}
```

### 2. Streaming Parser with Unicode Normalization
```rust
// ingest/src/parser.rs
use quick_xml::Reader;
use quick_xml::events::Event;
use unicode_normalization::UnicodeNormalization;

pub fn parse_message_at_offset(
    file: &File,
    boundary: MessageBoundary
) -> Result<Message> {
    let mut reader = Reader::from_reader(
        BufReader::new(file.take(boundary.end_offset - boundary.start_offset))
    );

    // CRITICAL: XXE prevention
    reader.check_end_names(false);  // Perf opt
    reader.expand_empty_elements(false);
    reader.config_mut().max_expand_depth = 10;  // Entity bomb protection

    let mut msg = Message::default();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) if e.name().as_ref() == b"sms" => {
                for attr in e.attributes() {
                    let attr = attr?;
                    match attr.key.as_ref() {
                        b"address" => {
                            msg.address = normalize_phone_number(
                                &attr.unescape_value()?
                            );
                        }
                        b"date" => {
                            msg.timestamp = attr.unescape_value()?.parse()?;
                        }
                        b"body" => {
                            let raw_body = attr.unescape_value()?;

                            // CRITICAL: Unicode normalization
                            msg.body = raw_body.nfc().collect();  // NFC for storage
                            msg.body_searchable = raw_body
                                .nfkd()
                                .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
                                .collect();  // NFKD for search
                        }
                        _ => {}
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(msg)
}

fn normalize_phone_number(raw: &str) -> String {
    // Remove spaces, dashes, handle +1 prefix, etc.
    raw.chars()
        .filter(|c| c.is_numeric() || *c == '+')
        .collect()
}
```

### 3. Byte-Budgeted Work Queue
```rust
// ingest/src/queue.rs
use std::sync::Arc;
use tokio::sync::Semaphore;

pub struct ByteBudgetQueue {
    channel: crossbeam_channel::Sender<Message>,
    semaphore: Arc<Semaphore>,
    budget_bytes: usize,
}

impl ByteBudgetQueue {
    pub fn new(budget_bytes: usize) -> (Self, crossbeam_channel::Receiver<Message>) {
        let (tx, rx) = crossbeam_channel::bounded(1000); // Item cap as fallback
        let semaphore = Arc::new(Semaphore::new(budget_bytes));

        (Self { channel: tx, semaphore, budget_bytes }, rx)
    }

    pub async fn enqueue(&self, msg: Message) -> Result<()> {
        let msg_size = std::mem::size_of_val(&msg)
                       + msg.body.len()
                       + msg.address.len();

        // Acquire byte budget (blocks if full)
        let permit = self.semaphore.clone().acquire_many_owned(msg_size as u32).await?;

        self.channel.send(msg)?;

        // Permit released when msg consumed
        std::mem::forget(permit);  // Caller will release
        Ok(())
    }
}
```

### 4. Batch Writer with Checkpointing
```rust
// db/src/writer.rs
use rusqlite::{Connection, Transaction};

pub struct BatchWriter {
    conn: Connection,
    batch_size: usize,
    checkpoint: Checkpoint,
}

#[derive(Serialize, Deserialize)]
pub struct Checkpoint {
    pub last_committed_offset: u64,
    pub messages_imported: u64,
    pub started_at: DateTime<Utc>,
    pub batch_id: Uuid,  // For continuity verification
}

impl BatchWriter {
    pub fn new(db_path: &Path, batch_size: usize) -> Result<Self> {
        let mut conn = Connection::open(db_path)?;
        apply_pragmas(&conn, ResourceProfile::detect(), ConnectionRole::Writer)?;

        let checkpoint = Self::load_checkpoint(db_path)?
            .unwrap_or_else(|| Checkpoint::new());

        Ok(Self { conn, batch_size, checkpoint })
    }

    pub fn write_batch(&mut self, messages: Vec<Message>) -> Result<()> {
        let tx = self.conn.transaction()?;

        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO messages (id, message_id, dedupe_hash, timestamp, address, body, body_searchable, message_type)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT DO NOTHING"  // Idempotency (message_id or dedupe_hash)
            )?;

            for msg in &messages {
                stmt.execute(params![
                    msg.id.to_string(),
                    msg.message_id,
                    msg.dedupe_hash,
                    msg.timestamp,
                    msg.address,
                    msg.body,
                    msg.body_searchable,
                    msg.message_type as i32,
                ])?;
            }
        }

        tx.commit()?;

        // Update checkpoint AFTER commit
        self.checkpoint.messages_imported += messages.len() as u64;
        self.checkpoint.last_committed_offset = messages.last()
            .map(|m| m.source_offset)
            .unwrap_or(0);

        self.save_checkpoint()?;

        Ok(())
    }

    fn save_checkpoint(&self) -> Result<()> {
        let temp_path = self.checkpoint_path().with_extension("tmp");
        let mut file = File::create(&temp_path)?;
        serde_json::to_writer_pretty(&mut file, &self.checkpoint)?;
        file.sync_all()?;  // Ensure written to disk

        // Atomic rename
        std::fs::rename(temp_path, self.checkpoint_path())?;
        // On POSIX, consider fsync on parent dir to persist the rename.
        Ok(())
    }
}
```

---

## Phase 2: Native GUI (Weeks 3-6)
### Framework Choice: egui
**Why egui:**

- Immediate mode = simpler state management
- Fast rendering (wgpu backend)
- Good for data-dense UIs
- Cross-platform without WebView overhead

**Alternative:** `iced` if you need more native feel, but egui is faster to prototype.

### State Management: Query-Driven, Not In-Memory
```rust
// app/src/state.rs

pub struct AppState {
    // DB connections
    read_conn: Arc<Mutex<Connection>>,
    write_conn: Arc<Mutex<Connection>>,  // Used by import thread

    // UI state (minimal)
    pub visible_range: Range<usize>,  // Row indices currently on screen
    pub search_query: String,
    pub filters: FilterState,

    // Import progress (atomic for lock-free reads)
    pub import_progress: Arc<AtomicU64>,
    pub import_total: Arc<AtomicU64>,

    // Message cache (only visible rows + prefetch)
    message_cache: LruCache<usize, MessageSummary>,
}

impl AppState {
    pub fn get_messages_for_range(&mut self, range: Range<usize>) -> Vec<MessageSummary> {
        let conn = self.read_conn.lock().unwrap();

        // Query ONLY the visible range
        let mut stmt = conn.prepare_cached(
            "SELECT id, timestamp, address, body
             FROM messages
             ORDER BY timestamp DESC
             LIMIT ?1 OFFSET ?2"
        ).unwrap();

        let rows = stmt.query_map(
            params![range.len(), range.start],
            |row| {
                Ok(MessageSummary {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    address: row.get(2)?,
                    body_preview: row.get::<_, String>(3)?
                        .chars()
                        .take(100)
                        .collect(),
                })
            }
        ).unwrap();

        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    pub fn search(&mut self, query: &str) -> Vec<MessageSummary> {
        let conn = self.read_conn.lock().unwrap();

        // FTS5 search
        let mut stmt = conn.prepare_cached(
            "SELECT messages.id, timestamp, address, snippet(messages_fts, -1, '<b>', '</b>', '...', 50)
             FROM messages_fts
             JOIN messages ON messages.rowid = messages_fts.rowid
             WHERE messages_fts MATCH ?1
             ORDER BY rank
             LIMIT 100"
        ).unwrap();

        // ... execute and return
    }
}
```

### Virtual Scrolling Implementation
```rust
// app/src/ui/message_list.rs
use egui::{ScrollArea, Ui};

pub fn render_message_list(ui: &mut Ui, state: &mut AppState) {
    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(
            ui,
            20.0,  // Row height
            state.total_message_count(),
            |ui, row_range| {
                // Fetch only visible rows from DB
                let messages = state.get_messages_for_range(row_range.clone());

                for msg in messages {
                    ui.horizontal(|ui| {
                        ui.label(format_timestamp(msg.timestamp));
                        ui.label(&msg.address);
                        ui.label(&msg.body_preview);
                    });
                }
            },
        );
}
```

### Progress UI with ETA
```rust
pub fn render_import_progress(ui: &mut Ui, state: &AppState) {
    let progress = state.import_progress.load(Ordering::Relaxed);
    let total = state.import_total.load(Ordering::Relaxed);
    let fraction = progress as f32 / total as f32;

    ui.add(egui::ProgressBar::new(fraction).text(format!(
        "{:.1}% - {} of {} messages - ETA: {}",
        fraction * 100.0,
        progress,
        total,
        estimate_remaining_time(progress, total, state.start_time)
    )));

    // Throughput graph (last 60 samples)
    plot_throughput(ui, &state.throughput_history);

    // Controls
    ui.horizontal(|ui| {
        if ui.button("Pause").clicked() {
            state.import_pause_token.store(true, Ordering::Relaxed);
        }
        if ui.button("Cancel").clicked() {
            state.import_cancel_token.store(true, Ordering::Relaxed);
        }
    });
}
```

---

## Production Hardening Checklist
### Pre-Launch Essentials
#### 1. First Launch Experience
```rust
// app/src/onboarding.rs

pub struct OnboardingWizard {
    step: OnboardingStep,
}

pub enum OnboardingStep {
    Welcome,
    HardwareCheck,
    FilePicker,
    ImportSettings,
    TestImport,
    Ready,
}

impl OnboardingWizard {
    pub fn hardware_check(&self) -> HardwareCheckResult {
        let resources = SystemResources::detect();
        let issues = vec![];

        if resources.total_ram_bytes < 4 * 1024_u64.pow(3) {
            issues.push(HardwareIssue::LowRam {
                have: resources.total_ram_bytes,
                recommended: 8 * 1024_u64.pow(3),
            });
        }

        if matches!(resources.storage_type, StorageType::Hdd) {
            issues.push(HardwareIssue::SlowDisk {
                note: "Import will be 3-5x slower on HDD vs SSD".into(),
            });
        }

        HardwareCheckResult { issues }
    }

    pub fn validate_xml(&self, path: &Path) -> Result<XmlValidation> {
        let metadata = std::fs::metadata(path)?;
        let size = metadata.len();

        // Quick sanity check (first 1MB)
        let mut file = File::open(path)?;
        let mut sample = vec![0u8; 1024 * 1024];
        file.read_exact(&mut sample)?;

        let encoding = detect_encoding(&sample)?;
        let has_sms_tags = sample.windows(5).any(|w| w == b"<sms ");

        if !has_sms_tags {
            return Err(AppError::InvalidXml("No <sms> tags found in file".into()));
        }

        let estimated_messages = (size / 512) as usize;  // Rough estimate
        let required_disk = calculate_minimum_resources(size).min_disk;

        Ok(XmlValidation {
            size_bytes: size,
            encoding,
            estimated_messages,
            required_disk,
            estimated_time: estimate_import_time(size, &SystemResources::detect()),
        })
    }
}
```

#### 2. Disk Space Preflight
```rust
pub fn preflight_disk_check(xml_path: &Path) -> Result<()> {
    let xml_size = std::fs::metadata(xml_path)?.len();
    let required = calculate_minimum_resources(xml_size).min_disk;

    let db_dir = get_database_directory()?;
    let available = fs2::available_space(&db_dir)?;

    if available < required {
        return Err(AppError::InsufficientDisk {
            needed: required / 1024_u64.pow(3),  // GB
            available: available / 1024_u64.pow(3),
        });
    }

    // Show warning if <20GB buffer
    if available < required + 20 * 1024_u64.pow(3) {
        warn!("Disk space is tight: {}GB available, {}GB needed",
              available / 1024_u64.pow(3),
              required / 1024_u64.pow(3));
    }

    Ok(())
}
```

#### 3. Import Rollback
```rust
pub struct AtomicImport {
    temp_db_path: PathBuf,
    final_db_path: PathBuf,
}

impl AtomicImport {
    pub fn new(final_path: &Path) -> Result<Self> {
        let temp_path = final_path.with_extension("db.tmp");

        // Copy schema from template or create fresh
        Self::initialize_temp_db(&temp_path)?;

        Ok(Self {
            temp_db_path: temp_path,
            final_db_path: final_path.to_path_buf(),
        })
    }

    pub fn commit(self) -> Result<()> {
        // Atomic rename
        std::fs::rename(&self.temp_db_path, &self.final_db_path)?;
        Ok(())
    }

    pub fn rollback(self) -> Result<()> {
        // Delete temp DB
        std::fs::remove_file(&self.temp_db_path)?;
        Ok(())
    }
}

// Usage:
let import = AtomicImport::new(&final_db_path)?;
match run_import(&import.temp_db_path) {
    Ok(_) => import.commit()?,
    Err(e) => {
        import.rollback()?;
        return Err(e);
    }
}
```

#### 4. Database Health Check ("Doctor" Command)
```rust
// cli/src/commands/doctor.rs

pub fn run_doctor(db_path: &Path) -> Result<()> {
    let conn = Connection::open(db_path)?;

    println!(" Running database diagnostics...\n");

    // Check integrity
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    println!("Integrity: {}", if integrity == "ok" { "PASS" } else { "FAIL" });

    // Check FTS5 sync
    let msg_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
    let fts_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row.get(0))?;

    if msg_count == fts_count {
        println!("FTS5 index: SYNCED ({} messages)", msg_count);
    } else {
        println!("FTS5 index: OUT OF SYNC (messages: {}, fts: {})", msg_count, fts_count);
        println!("   Run 'sms doctor --rebuild-fts' to fix");
    }

    // Check WAL size
    let wal_path = db_path.with_extension("db-wal");
    if let Ok(metadata) = std::fs::metadata(&wal_path) {
        let wal_mb = metadata.len() / 1024_u64.pow(2);
        if wal_mb > 100 {
            println!("WAL file: LARGE ({}MB)", wal_mb);
            println!("   Run 'sms doctor --checkpoint' to shrink");
        } else {
            println!("WAL file: OK ({}MB)", wal_mb);
        }
    }

    // More checks...
    Ok(())
}
```

#### 5. Unicode & FTS5 Configuration
```sql
-- db/migrations/0001_initial.sql

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    message_id TEXT,          -- From XML, may be NULL
    dedupe_hash BLOB,         -- Hash of address+timestamp+body when message_id missing
    timestamp INTEGER NOT NULL,
    address TEXT NOT NULL,
    body TEXT NOT NULL,       -- NFC normalized
    body_searchable TEXT NOT NULL,  -- NFKD for FTS
    message_type INTEGER NOT NULL,
    thread_id TEXT,
    created_at INTEGER DEFAULT (strftime('%s', 'now'))
);

CREATE INDEX idx_messages_timestamp ON messages(timestamp DESC);
CREATE INDEX idx_messages_address ON messages(address);
CREATE UNIQUE INDEX idx_messages_message_id ON messages(message_id) WHERE message_id IS NOT NULL;
CREATE UNIQUE INDEX idx_messages_dedupe_hash ON messages(dedupe_hash) WHERE dedupe_hash IS NOT NULL;

-- FTS5 with proper tokenizer
CREATE VIRTUAL TABLE messages_fts USING fts5(
    body_searchable,
    sender,
    content=messages,
    tokenize='unicode61 remove_diacritics 0'
);

-- NO triggers (build FTS after import completes)
```

```rust
// After import:
pub fn rebuild_fts_index(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM messages_fts", [])?;
    conn.execute("INSERT INTO messages_fts(rowid, body_searchable, sender)
                  SELECT rowid, body_searchable, address FROM messages", [])?;
    conn.execute("INSERT INTO messages_fts(messages_fts) VALUES('optimize')", [])?;
    Ok(())
}
```

---

## DevOps & Release
### Code Signing (Critical for Windows/macOS)
```yaml
# .github/workflows/release.yml
name: Release

on:
  push:
    tags:
      - "v*"

jobs:
  build:
    strategy:
      matrix:
        include:
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            ext: .exe
          - os: macos-latest
            target: x86_64-apple-darwin
            ext: ""
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            ext: ""

    runs-on: ${{ matrix.os }}

    steps:
      - uses: actions/checkout@v3

      - name: Install Rust
        uses: actions-rs/toolchain@v1
        with:
          toolchain: stable
          target: ${{ matrix.target }}

      - name: Build release
        run: cargo build --release --target ${{ matrix.target }}

      - name: Sign (Windows)
        if: matrix.os == 'windows-latest'
        run: |
          # Use Azure Code Signing or DigiCert
          signtool sign /f cert.pfx /p ${{ secrets.CERT_PASSWORD }} \
                   /t http://timestamp.digicert.com \
                   target/${{ matrix.target }}/release/sms-archive.exe

      - name: Notarize (macOS)
        if: matrix.os == 'macos-latest'
        run: |
          # Apple notarization required for macOS Gatekeeper
          xcrun notarytool submit \
            --apple-id ${{ secrets.APPLE_ID }} \
            --password ${{ secrets.APPLE_APP_PASSWORD }} \
            --team-id ${{ secrets.APPLE_TEAM_ID }} \
            sms-archive.zip
```

### Packaging: Native Dependencies
```
release/
 sms-archive(.exe)          # Main binary
 libs/
    windows/
       onnxruntime.dll
       ffmpeg.dll
       tesseract.dll
    macos/
       libonnxruntime.dylib
       ...
    linux/
        libonnxruntime.so
        ...
 LICENSE
```

---

## Final Pre-Coding Checklist
Before writing line 1 of Phase 0:

- [ ] Read this entire document twice
- [ ] Clone the example workspace structure
- [ ] Set up logging to a file with rotation
- [ ] Write error taxonomy enum (20+ variants)
- [ ] Implement resource detection (RAM, disk, CPU, storage type)
- [ ] Generate 1GB test XML with datagen
- [ ] Benchmark boundary detection (naive vs memchr vs two-pass)
- [ ] Write schema v1 SQL with Unicode + FTS5 config
- [ ] Test SQLite pragmas on low-end hardware (4GB RAM laptop)
- [ ] Draft privacy policy (even if minimal)
- [ ] Set up GitHub repo with CI/CD skeleton

---

## Critical Reminders
1. **Disable `wal_autocheckpoint` only during bulk import; always checkpoint/truncate after**
2. **Always normalize text to NFC for storage, NFKD for search**
3. **FTS5 tokenizer MUST be `unicode61 remove_diacritics 0` (categories optional if supported)**
4. **Boundary detection MUST validate encoding first**
5. **Queue sizing MUST be by bytes-in-flight, not message count**
6. **UI state MUST be query-driven, not in-memory Vec**
7. **Disk space check MUST use 4.75x multiplier, not 2x**
8. **Checkpoints MUST only advance after DB commit, not parse**
9. **Page size (32KB) is a one-way doordecide early**
10. **Code signing is mandatory for Windows/macOS adoption**

---

## Success Metrics (How to Know You Shipped v1.0)
- [ ] Import 80GB in <30 min on reference hardware
- [ ] Memory stays under 1GB during import (verified with platform profiler)
- [ ] UI maintains 30+ FPS during import (measured with puffin)
- [ ] Search returns results in <500ms for any query
- [ ] App passes "doctor" command health checks
- [ ] Signed and notarized for Windows/macOS
- [ ] Privacy policy published and opt-in crash reporting works
- [ ] 5 users complete real imports without filing bugs

---

**END OF BOOTSTRAP**

Want me to scaffold the Cargo workspace now, or drill into any specific section?


