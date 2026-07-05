use anyhow::Result;
use clap::{Parser, Subcommand};
use rusqlite::{named_params, params, OptionalExtension};
use sms_config::{
    available_disk_bytes, calculate_minimum_resources, detect_resource_limits, init_logging,
    ResourceProfile,
};
use sms_datagen::{generate_xml, DataGenConfig};
use sms_db::{checkpoint_wal, rebuild_fts, ConnectionMode, Database};
use sms_ingest::{
    ingest_file, scan_boundaries, scan_boundaries_full, scan_boundaries_naive, IngestOptions,
    IngestProgress,
};
use sms_media_process::{process_media, MediaProcessOptions};
use sms_ml::{DevicePreference, EmbeddingConfig, EmbeddingService};
use sms_search::{semantic_search, Fts5Backend, SearchBackend};
use sms_types::MessageType;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(serde::Serialize, serde::Deserialize)]
struct ImportSummary {
    input: String,
    db: String,
    messages_seen: u64,
    messages_inserted: u64,
    messages_skipped: u64,
    attachments_written: u64,
    parse_errors: u64,
    elapsed_ms: u128,
    db_total_messages: i64,
}

#[derive(Parser)]
#[command(name = "sms")]
#[command(about = "SMS Archive CLI", long_about = None)]
struct Cli {
    /// Log directory (defaults to ./logs)
    #[arg(long)]
    log_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum WriterMode {
    Import,
    Interactive,
}

#[derive(Subcommand)]
enum Commands {
    /// Import XML file
    Ingest {
        /// Path to XML file
        #[arg(short, long)]
        input: PathBuf,

        /// Path to database
        #[arg(short, long, default_value = "sms.db")]
        db: PathBuf,

        /// Batch size
        #[arg(long, default_value_t = 10_000)]
        batch_size: usize,

        /// Queue byte budget
        #[arg(long, default_value_t = 256 * 1024 * 1024)]
        queue_bytes: usize,

        /// Read buffer bytes
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        read_buffer_bytes: usize,

        /// Use boundary scan + parallel parser
        #[arg(long, default_value_t = true)]
        parallel: bool,

        /// Parser worker threads (0 = auto)
        #[arg(long, default_value_t = 0)]
        parser_threads: usize,

        /// Attempt recovery after parse errors (stream mode)
        #[arg(long, default_value_t = true)]
        recover: bool,

        /// Resume from checkpoint
        #[arg(long, default_value_t = true)]
        resume: bool,

        /// Rebuild FTS index after import
        #[arg(long, default_value_t = true)]
        rebuild_fts: bool,

        /// Media output directory
        #[arg(long)]
        media_dir: Option<PathBuf>,

        /// Write attachments to disk
        #[arg(long, default_value_t = true)]
        write_attachments: bool,

        /// Thumbnail size
        #[arg(long, default_value_t = 256)]
        thumbnail_size: u32,

        /// Defer thumbnail generation to background workers
        #[arg(long, default_value_t = true)]
        defer_thumbnails: bool,

        /// Thumbnail worker threads
        #[arg(long, default_value_t = 2)]
        thumbnail_workers: usize,

        /// Thumbnail queue capacity
        #[arg(long, default_value_t = 1024)]
        thumbnail_queue_capacity: usize,

        /// Writer mode (import or interactive)
        #[arg(long, value_enum, default_value_t = WriterMode::Import)]
        writer_mode: WriterMode,

        /// Emit periodic progress logs
        #[arg(long, default_value_t = true)]
        progress: bool,

        /// Progress interval (ms)
        #[arg(long, default_value_t = 1000)]
        progress_interval_ms: u64,

        /// Run verify after ingest
        #[arg(long, default_value_t = false)]
        verify: bool,

        /// Use atomic import (write to temp DB, then rename)
        #[arg(long, default_value_t = false)]
        atomic: bool,

        /// Overwrite existing DB (atomic mode will backup)
        #[arg(long, default_value_t = false)]
        overwrite: bool,
    },
    /// Run database health checks
    Doctor {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Rebuild FTS index
        #[arg(long)]
        rebuild_fts: bool,
    },
    /// Verify database contents and optional import summary
    Verify {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Optional import summary JSON to compare against
        #[arg(long)]
        summary: Option<PathBuf>,

        /// Rebuild FTS index if out of sync
        #[arg(long)]
        rebuild_fts: bool,
    },
    /// Generate embeddings for messages
    Embed {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Model file path (optional, uses hash embeddings if omitted)
        #[arg(long)]
        model_path: Option<PathBuf>,

        /// Tokenizer JSON path (required for ONNX models)
        #[arg(long)]
        tokenizer: Option<PathBuf>,

        /// Model name
        #[arg(long, default_value = "hash-embed")]
        model_name: String,

        /// Model version
        #[arg(long, default_value = "v1")]
        model_version: String,

        /// Embedding dimensions
        #[arg(long, default_value_t = 384)]
        dimensions: usize,

        /// Max sequence length
        #[arg(long, default_value_t = 256)]
        max_length: usize,

        /// L2 normalize embeddings
        #[arg(long, default_value_t = true)]
        normalize: bool,

        /// Batch size
        #[arg(long, default_value_t = 256)]
        batch_size: usize,

        /// Optional limit
        #[arg(long)]
        limit: Option<usize>,

        /// Filter by timestamp >= (ms since epoch)
        #[arg(long)]
        since: Option<i64>,

        /// Filter by timestamp <= (ms since epoch)
        #[arg(long)]
        until: Option<i64>,

        /// Only report how many rows would be processed
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Process media with CLIP (embeddings + NSFW)
    MediaProcess {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Path to CLIP ONNX model
        #[arg(long)]
        clip_model: PathBuf,

        /// Path to NSFW linear probe weights (safetensors/npz)
        #[arg(long)]
        nsfw_weights: PathBuf,

        /// Batch size for GPU inference
        #[arg(long, default_value_t = 32)]
        batch_size: usize,

        /// Max keyframes per video
        #[arg(long, default_value_t = 5)]
        max_keyframes: usize,

        /// Re-process already processed items
        #[arg(long, default_value_t = false)]
        reprocess: bool,

        /// Limit items to process
        #[arg(long)]
        limit: Option<usize>,

        /// Workers for image loading
        #[arg(long)]
        workers: Option<usize>,

        /// Media root override
        #[arg(long)]
        media_root: Option<PathBuf>,

        /// Only show what would be processed
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Run a search query against the database
    Search {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Query string
        #[arg(short, long)]
        query: String,

        /// Result limit
        #[arg(long, default_value_t = 50)]
        limit: usize,

        /// Output JSON lines
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Run a semantic search query against embeddings
    SemanticSearch {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Query string
        #[arg(short, long)]
        query: String,

        /// Model file path
        #[arg(long)]
        model_path: Option<PathBuf>,

        /// Tokenizer JSON path (required for ONNX models)
        #[arg(long)]
        tokenizer: Option<PathBuf>,

        /// Model name
        #[arg(long, default_value = "hash-embed")]
        model_name: String,

        /// Model version
        #[arg(long, default_value = "v1")]
        model_version: String,

        /// Embedding dimensions
        #[arg(long, default_value_t = 384)]
        dimensions: usize,

        /// Max sequence length
        #[arg(long, default_value_t = 256)]
        max_length: usize,

        /// L2 normalize embeddings
        #[arg(long, default_value_t = true)]
        normalize: bool,

        /// Result limit
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Output JSON lines
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Export messages to JSONL or CSV
    Export {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Output file (defaults to stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Format (jsonl or csv)
        #[arg(long, value_enum, default_value_t = ExportFormat::Jsonl)]
        format: ExportFormat,

        /// Optional FTS query
        #[arg(short, long)]
        query: Option<String>,

        /// Filter by thread id
        #[arg(long)]
        thread_id: Option<String>,

        /// Filter by message type
        #[arg(long, value_enum)]
        message_type: Option<ExportMessageType>,

        /// Filter by address (normalized)
        #[arg(long)]
        address: Option<String>,

        /// Filter by address (LIKE pattern, supports %)
        #[arg(long)]
        address_like: Option<String>,

        /// Filter by body contains (substring)
        #[arg(long)]
        body_contains: Option<String>,

        /// Filter by timestamp >= (ms since epoch)
        #[arg(long)]
        since: Option<i64>,

        /// Filter by timestamp <= (ms since epoch)
        #[arg(long)]
        until: Option<i64>,

        /// Limit rows
        #[arg(long, default_value_t = 1000)]
        limit: usize,

        /// Offset rows (non-FTS only)
        #[arg(long, default_value_t = 0)]
        offset: usize,

        /// Include attachment paths (semicolon-separated)
        #[arg(long, default_value_t = false)]
        with_attachments: bool,
    },
    /// Export attachment metadata
    ExportAttachments {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Output file (defaults to stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Format (jsonl or csv)
        #[arg(long, value_enum, default_value_t = ExportFormat::Jsonl)]
        format: ExportFormat,

        /// Filter by address (normalized)
        #[arg(long)]
        address: Option<String>,

        /// Filter by address (LIKE pattern, supports %)
        #[arg(long)]
        address_like: Option<String>,

        /// Filter by body contains (substring)
        #[arg(long)]
        body_contains: Option<String>,

        /// Filter by thread id
        #[arg(long)]
        thread_id: Option<String>,

        /// Filter by message type
        #[arg(long, value_enum)]
        message_type: Option<ExportMessageType>,

        /// Filter by timestamp >= (ms since epoch)
        #[arg(long)]
        since: Option<i64>,

        /// Filter by timestamp <= (ms since epoch)
        #[arg(long)]
        until: Option<i64>,

        /// Filter by mime type
        #[arg(long)]
        mime: Option<String>,

        /// Limit rows
        #[arg(long, default_value_t = 1000)]
        limit: usize,

        /// Offset rows
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Benchmark boundary detection strategies
    BenchBoundary {
        /// Path to XML file
        #[arg(short, long)]
        input: PathBuf,
    },
    /// Build a Tantivy index from the database
    #[cfg(feature = "tantivy")]
    TantivyBuild {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Index directory
        #[arg(short, long, default_value = "tantivy-index")]
        index_dir: PathBuf,

        /// Rebuild the index directory
        #[arg(long, default_value_t = true)]
        rebuild: bool,
    },
    /// Update Tantivy index with new rows
    #[cfg(feature = "tantivy")]
    TantivyUpdate {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Index directory
        #[arg(short, long, default_value = "tantivy-index")]
        index_dir: PathBuf,
    },
    /// Search Tantivy index
    #[cfg(feature = "tantivy")]
    TantivySearch {
        /// Index directory
        #[arg(short, long, default_value = "tantivy-index")]
        index_dir: PathBuf,

        /// Query string
        #[arg(short, long)]
        query: String,

        /// Result limit
        #[arg(long, default_value_t = 50)]
        limit: usize,

        /// Output JSON lines
        #[arg(long, default_value_t = false)]
        json: bool,

        /// Filter by address (normalized)
        #[arg(long)]
        address: Option<String>,

        /// Filter by thread id
        #[arg(long)]
        thread_id: Option<String>,

        /// Filter by message type
        #[arg(long, value_enum)]
        message_type: Option<ExportMessageType>,

        /// Filter by timestamp >= (ms since epoch)
        #[arg(long)]
        since: Option<i64>,

        /// Filter by timestamp <= (ms since epoch)
        #[arg(long)]
        until: Option<i64>,
    },
    /// Generate test data
    Datagen {
        /// Output path
        #[arg(short, long)]
        output: PathBuf,

        /// Size in GB
        #[arg(short, long, default_value = "1.0")]
        size: f64,

        /// RNG seed (for reproducible output)
        #[arg(long)]
        seed: Option<u64>,

        /// Ratio of MMS messages (0.0 - 1.0)
        #[arg(long, default_value = "0.0")]
        mms_ratio: f64,

        /// Burstiness factor (0.0 - 1.0)
        #[arg(long, default_value = "0.1")]
        burstiness: f64,
    },
    /// List embedding models and coverage
    Models {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Output JSON lines
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Delete an embedding model (also removes embeddings)
    ModelDelete {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Model id (preferred)
        #[arg(long)]
        model_id: Option<String>,

        /// Model name
        #[arg(long)]
        name: Option<String>,

        /// Model version
        #[arg(long)]
        version: Option<String>,

        /// Model sha256 (optional to disambiguate)
        #[arg(long)]
        sha256: Option<String>,
    },
    /// Purge embeddings for a model (keeps model row)
    ModelPurge {
        /// Path to database
        #[arg(short, long)]
        db: PathBuf,

        /// Model id (preferred)
        #[arg(long)]
        model_id: Option<String>,

        /// Model name
        #[arg(long)]
        name: Option<String>,

        /// Model version
        #[arg(long)]
        version: Option<String>,

        /// Model sha256 (optional to disambiguate)
        #[arg(long)]
        sha256: Option<String>,
    },

    /// List contacts with message counts. Useful for picking an `analyze-contact` target.
    ListContacts {
        /// Path to database
        #[arg(short, long, default_value = "sms.db")]
        db: PathBuf,

        /// Hide contacts with fewer than this many messages
        #[arg(long, default_value_t = 50)]
        min_messages: i64,

        /// Maximum rows to print (0 = all)
        #[arg(long, default_value_t = 50)]
        limit: usize,

        /// Output as JSON instead of a table
        #[arg(long, default_value_t = false)]
        json: bool,
    },

    /// Run the analytics pipeline for a single contact and persist results.
    AnalyzeContact {
        /// Path to database
        #[arg(short, long, default_value = "sms.db")]
        db: PathBuf,

        /// Contact UUID (from `list-contacts`)
        #[arg(long)]
        contact_id: String,

        /// User's local UTC offset in seconds (e.g. -21600 for UTC-6)
        #[arg(long, default_value_t = 0)]
        tz_offset_secs: i32,
    },

    /// Show cached analytics for a contact. Reads from pair_analytics — fast.
    AnalyticsShow {
        /// Path to database
        #[arg(short, long, default_value = "sms.db")]
        db: PathBuf,

        /// Contact UUID
        #[arg(long)]
        contact_id: String,

        /// Output as JSON instead of a human-readable summary
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum ExportFormat {
    Jsonl,
    Csv,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum ExportMessageType {
    Sms,
    Mms,
    Rcs,
}

#[derive(serde::Serialize)]
struct ExportRow {
    id: String,
    message_id: Option<String>,
    timestamp: i64,
    address: String,
    body: String,
    message_type: String,
    thread_id: Option<String>,
    attachment_count: i64,
    attachment_paths: Option<String>,
}

#[derive(serde::Serialize)]
struct AttachmentExportRow {
    id: String,
    message_id: String,
    timestamp: i64,
    address: String,
    mime_type: String,
    file_path: String,
    thumbnail_path: Option<String>,
    file_hash: String,
}

struct ExportOptions {
    query: Option<String>,
    thread_id: Option<String>,
    address: Option<String>,
    address_like: Option<String>,
    body_contains: Option<String>,
    message_type: Option<i32>,
    since: Option<i64>,
    until: Option<i64>,
    limit: usize,
    offset: usize,
    with_attachments: bool,
}

struct AttachmentExportOptions {
    address: Option<String>,
    address_like: Option<String>,
    body_contains: Option<String>,
    thread_id: Option<String>,
    message_type: Option<i32>,
    since: Option<i64>,
    until: Option<i64>,
    mime: Option<String>,
    limit: usize,
    offset: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let log_dir = cli.log_dir.clone().unwrap_or_else(|| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("logs")
    });
    let _log_guard = init_logging(&log_dir)?;

    match cli.command {
        Commands::Ingest {
            input,
            db,
            batch_size,
            queue_bytes,
            read_buffer_bytes,
            parallel,
            parser_threads,
            recover,
            resume,
            rebuild_fts: rebuild,
            media_dir,
            write_attachments,
            thumbnail_size,
            defer_thumbnails,
            thumbnail_workers,
            thumbnail_queue_capacity,
            writer_mode,
            progress,
            progress_interval_ms,
            verify,
            atomic,
            overwrite,
        } => {
            let mut db_path_for_ingest = db.clone();
            let mut backup_path: Option<PathBuf> = None;
            let mut temp_db: Option<PathBuf> = None;
            if atomic {
                let temp_db_path = db.with_extension("db.tmp");
                if temp_db_path.exists() {
                    std::fs::remove_file(&temp_db_path)?;
                }
                if db.exists() {
                    if overwrite {
                        let ts = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        let backup = db.with_extension(format!("db.bak.{}", ts));
                        std::fs::rename(&db, &backup)?;
                        backup_path = Some(backup);
                    } else {
                        return Err(anyhow::anyhow!(
                            "Database already exists. Use --overwrite to replace."
                        ));
                    }
                }
                db_path_for_ingest = temp_db_path;
                temp_db = Some(db_path_for_ingest.clone());
            }

            let metadata = std::fs::metadata(&input)?;
            let xml_size = metadata.len();
            let requirements = calculate_minimum_resources(xml_size)?;
            let resources = detect_resource_limits();
            let db_dir = db.parent().unwrap_or_else(|| std::path::Path::new("."));
            let available = available_disk_bytes(db_dir)?;
            if available < requirements.min_disk {
                return Err(anyhow::anyhow!(
                    "Insufficient disk space: need {} GB, have {} GB",
                    requirements.min_disk / 1024_u64.pow(3),
                    available / 1024_u64.pow(3)
                ));
            }
            if resources.total_ram_bytes < requirements.min_ram {
                eprintln!(
                    "Warning: low RAM detected ({} GB total). Import may be slow.",
                    resources.total_ram_bytes / 1024_u64.pow(3)
                );
            }

            let progress_state = Arc::new(IngestProgress::default());
            let done = Arc::new(AtomicBool::new(false));
            if progress {
                let progress_state = Arc::clone(&progress_state);
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    let mut last_messages = 0u64;
                    let mut last_bytes = 0u64;
                    let mut last_time = std::time::Instant::now();
                    loop {
                        if done.load(Ordering::Relaxed) {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(
                            progress_interval_ms.max(200),
                        ));
                        let seen = progress_state.messages_seen.load(Ordering::Relaxed);
                        let inserted = progress_state.messages_inserted.load(Ordering::Relaxed);
                        let bytes = progress_state.bytes_read.load(Ordering::Relaxed);
                        let errors = progress_state.parse_errors.load(Ordering::Relaxed);
                        let total = progress_state.total_bytes.load(Ordering::Relaxed);
                        let elapsed = last_time.elapsed().as_secs_f64().max(0.001);
                        let msg_rate = (seen.saturating_sub(last_messages)) as f64 / elapsed;
                        let byte_rate = (bytes.saturating_sub(last_bytes)) as f64 / elapsed;
                        let pct = if total > 0 {
                            (bytes as f64 / total as f64) * 100.0
                        } else {
                            0.0
                        };
                        eprintln!(
                            "progress: seen {} inserted {} errors {} {:.1}% | {:.0} msg/s | {}/s",
                            seen,
                            inserted,
                            errors,
                            pct.min(100.0),
                            msg_rate,
                            format_bytes(byte_rate as u64)
                        );
                        last_messages = seen;
                        last_bytes = bytes;
                        last_time = std::time::Instant::now();
                    }
                });
            }

            let cancel_progress = Arc::clone(&progress_state);
            let _ = ctrlc::set_handler(move || {
                cancel_progress.cancelled.store(true, Ordering::Relaxed);
            });

            let opts = IngestOptions {
                batch_size,
                queue_bytes,
                read_buffer_bytes,
                use_boundary_scan: parallel,
                parser_threads: if parser_threads == 0 {
                    std::thread::available_parallelism()
                        .map(|n| n.get())
                        .unwrap_or(4)
                } else {
                    parser_threads
                },
                recover_on_error: recover,
                resume,
                media_dir,
                write_attachments,
                thumbnail_size,
                defer_thumbnails,
                thumbnail_workers,
                thumbnail_queue_capacity,
                writer_mode: match writer_mode {
                    WriterMode::Import => ConnectionMode::Import,
                    WriterMode::Interactive => ConnectionMode::Interactive,
                },
                progress: Some(Arc::clone(&progress_state)),
            };
            let stats = match ingest_file(&input, &db_path_for_ingest, &opts) {
                Ok(stats) => stats,
                Err(err) => {
                    done.store(true, Ordering::Relaxed);
                    if let Some(backup) = &backup_path {
                        if !db.exists() {
                            let _ = std::fs::rename(backup, &db);
                        }
                    }
                    if let Some(temp) = &temp_db {
                        eprintln!("Import failed. Temp DB left at: {}", temp.display());
                    }
                    return Err(err.into());
                }
            };
            let skipped = stats.messages_seen.saturating_sub(stats.messages_inserted);
            done.store(true, Ordering::Relaxed);
            println!(
                "Inserted {} / {} messages in {} ms (attachments: {}, parse errors: {})",
                stats.messages_inserted,
                stats.messages_seen,
                stats.elapsed_ms,
                stats.attachments_written,
                stats.parse_errors
            );
            if atomic {
                if let Err(err) = std::fs::rename(&db_path_for_ingest, &db) {
                    if let Some(backup) = &backup_path {
                        if !db.exists() {
                            let _ = std::fs::rename(backup, &db);
                        }
                    }
                    return Err(err.into());
                }
                let temp_checkpoint = db_path_for_ingest.with_extension("checkpoint.json");
                let _ = std::fs::remove_file(temp_checkpoint);
            }
            let db_conn = Database::open(&db, ResourceProfile::detect())?;
            let total_messages: i64 = db_conn
                .connection()
                .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
                .unwrap_or(0);
            let summary = ImportSummary {
                input: input.display().to_string(),
                db: db.display().to_string(),
                messages_seen: stats.messages_seen,
                messages_inserted: stats.messages_inserted,
                messages_skipped: skipped,
                attachments_written: stats.attachments_written,
                parse_errors: stats.parse_errors,
                elapsed_ms: stats.elapsed_ms,
                db_total_messages: total_messages,
            };
            let summary_path = db.with_extension("import_summary.json");
            if let Ok(mut file) = File::create(&summary_path) {
                let _ = serde_json::to_writer_pretty(&mut file, &summary);
            }
            if rebuild {
                rebuild_fts(db_conn.connection())?;
                println!("FTS index rebuilt");
            }
            checkpoint_wal(db_conn.connection())?;
            if verify {
                let summary_path = db.with_extension("import_summary.json");
                let summary_arg = summary_path.exists().then_some(summary_path);
                run_verify(&db, summary_arg.as_deref(), rebuild)?;
            }
            if let Some(backup) = backup_path {
                println!("Backup saved at: {}", backup.display());
            }
            Ok(())
        }
        Commands::Doctor {
            db,
            rebuild_fts: do_rebuild,
        } => {
            println!("Checking database: {}", db.display());
            let db = Database::open(&db, ResourceProfile::detect())?;
            let conn = db.connection();

            let integrity: String =
                conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
            println!(
                "Integrity: {}",
                if integrity == "ok" { "PASS" } else { "FAIL" }
            );

            let msg_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
            let fts_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row.get(0))?;
            if msg_count == fts_count {
                println!("FTS5 index: SYNCED ({} messages)", msg_count);
            } else {
                println!(
                    "FTS5 index: OUT OF SYNC (messages: {}, fts: {})",
                    msg_count, fts_count
                );
                if do_rebuild {
                    rebuild_fts(conn)?;
                    println!("FTS5 index rebuilt");
                }
            }

            let orphan_attachments: i64 = conn.query_row(
                "SELECT COUNT(*) FROM attachments a LEFT JOIN messages m ON a.message_id = m.id WHERE m.id IS NULL",
                [],
                |row| row.get(0),
            )?;
            if orphan_attachments == 0 {
                println!("Attachments: OK (no orphans)");
            } else {
                println!("Attachments: {} orphan rows", orphan_attachments);
            }

            let wal_path = db
                .connection()
                .path()
                .map(|p| PathBuf::from(p).with_extension("db-wal"));
            if let Some(wal_path) = wal_path {
                if let Ok(metadata) = std::fs::metadata(&wal_path) {
                    let wal_mb = metadata.len() / 1024_u64.pow(2);
                    println!("WAL file: {} MB", wal_mb);
                }
            }

            Ok(())
        }
        Commands::Verify {
            db,
            summary,
            rebuild_fts: do_rebuild,
        } => run_verify(&db, summary.as_deref(), do_rebuild),
        Commands::Embed {
            db,
            model_path,
            tokenizer,
            model_name,
            model_version,
            dimensions,
            max_length,
            normalize,
            batch_size,
            limit,
            since,
            until,
            dry_run,
        } => run_embeddings(
            &db,
            model_path.as_deref(),
            tokenizer.as_deref(),
            &model_name,
            &model_version,
            dimensions,
            max_length,
            normalize,
            batch_size,
            limit,
            since,
            until,
            dry_run,
        ),
        Commands::MediaProcess {
            db,
            clip_model,
            nsfw_weights,
            batch_size,
            max_keyframes,
            reprocess,
            limit,
            workers,
            media_root,
            dry_run,
        } => {
            if dry_run {
                let db_handle = Database::open(&db, ResourceProfile::detect())?;
                let conn = db_handle.connection();
                let tasks = sms_db::get_unprocessed_media(conn, limit, reprocess)?;
                println!("Would process {} media item(s)", tasks.len());
                return Ok(());
            }
            let worker_count = workers.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4)
            });
            let options = MediaProcessOptions {
                db_path: db,
                clip_model,
                nsfw_weights,
                batch_size,
                max_keyframes,
                reprocess,
                limit,
                workers: worker_count,
                show_progress: true,
                media_root,
                cancel_flag: None,
                pause_flag: None,
                progress_total: None,
                progress_done: None,
            };
            let stats = process_media(&options)?;
            println!(
                "Processed {} / {} items, {} frames embedded, {} NSFW updated in {} ms",
                stats.processed_tasks,
                stats.total_tasks,
                stats.embedded_frames,
                stats.nsfw_updated,
                stats.elapsed_ms
            );
            Ok(())
        }
        Commands::Search {
            db,
            query,
            limit,
            json,
        } => {
            let backend = Fts5Backend::open(&db, ResourceProfile::detect())?;
            let results = backend.search(&query, limit)?;
            for msg in results {
                if json {
                    println!("{}", serde_json::to_string(&msg)?);
                } else {
                    println!("{} | {} | {}", msg.timestamp, msg.address, msg.body);
                }
            }
            Ok(())
        }
        Commands::SemanticSearch {
            db,
            query,
            model_path,
            tokenizer,
            model_name,
            model_version,
            dimensions,
            max_length,
            normalize,
            limit,
            json,
        } => run_semantic_search(
            &db,
            &query,
            model_path.as_deref(),
            tokenizer.as_deref(),
            &model_name,
            &model_version,
            dimensions,
            max_length,
            normalize,
            limit,
            json,
        ),
        Commands::Export {
            db,
            output,
            format,
            query,
            thread_id,
            message_type,
            address,
            address_like,
            body_contains,
            since,
            until,
            limit,
            offset,
            with_attachments,
        } => {
            let writer: Box<dyn Write> = match output {
                Some(path) => Box::new(File::create(path)?),
                None => Box::new(io::stdout()),
            };
            let normalized_address = address.map(|a| normalize_address(&a));
            let like_pattern = address_like.map(|a| normalize_like(&a));
            let body_pattern = body_contains.map(|b| normalize_like(&b));
            let options = ExportOptions {
                query,
                thread_id,
                address: normalized_address,
                address_like: like_pattern,
                body_contains: body_pattern,
                message_type: message_type.map(message_type_to_i32),
                since,
                until,
                limit,
                offset,
                with_attachments,
            };
            export_messages(&db, writer, format, &options)
        }
        Commands::ExportAttachments {
            db,
            output,
            format,
            address,
            address_like,
            body_contains,
            thread_id,
            message_type,
            since,
            until,
            mime,
            limit,
            offset,
        } => {
            let writer: Box<dyn Write> = match output {
                Some(path) => Box::new(File::create(path)?),
                None => Box::new(io::stdout()),
            };
            let normalized_address = address.map(|a| normalize_address(&a));
            let like_pattern = address_like.map(|a| normalize_like(&a));
            let body_pattern = body_contains.map(|b| normalize_like(&b));
            let options = AttachmentExportOptions {
                address: normalized_address,
                address_like: like_pattern,
                body_contains: body_pattern,
                thread_id,
                message_type: message_type.map(message_type_to_i32),
                since,
                until,
                mime,
                limit,
                offset,
            };
            export_attachments(&db, writer, format, &options)
        }
        Commands::BenchBoundary { input } => {
            let start = std::time::Instant::now();
            let naive = scan_boundaries_naive(&input)?;
            let naive_ms = start.elapsed().as_millis();

            let start = std::time::Instant::now();
            let memchr = scan_boundaries(&input)?;
            let memchr_ms = start.elapsed().as_millis();

            let start = std::time::Instant::now();
            let full = scan_boundaries_full(&input)?;
            let full_ms = start.elapsed().as_millis();

            println!("Boundary scan benchmark:");
            println!("  naive:  {} boundaries in {} ms", naive.len(), naive_ms);
            println!("  memchr: {} boundaries in {} ms", memchr.len(), memchr_ms);
            println!("  full:   {} boundaries in {} ms", full.len(), full_ms);
            Ok(())
        }
        #[cfg(feature = "tantivy")]
        Commands::TantivyBuild {
            db,
            index_dir,
            rebuild,
        } => {
            sms_search::TantivyBackend::build_index(&db, &index_dir, rebuild)?;
            println!("Tantivy index built at {}", index_dir.display());
            Ok(())
        }
        #[cfg(feature = "tantivy")]
        Commands::TantivyUpdate { db, index_dir } => {
            sms_search::TantivyBackend::update_index(&db, &index_dir)?;
            println!("Tantivy index updated at {}", index_dir.display());
            Ok(())
        }
        #[cfg(feature = "tantivy")]
        Commands::TantivySearch {
            index_dir,
            query,
            limit,
            json,
            address,
            thread_id,
            message_type,
            since,
            until,
        } => {
            let backend = sms_search::TantivyBackend::open(&index_dir)?;
            let filter = sms_search::TantivyFilter {
                address: address.map(|a| normalize_address(&a)),
                thread_id,
                message_type: message_type.map(|t| message_type_to_i32(t) as i64),
                since,
                until,
            };
            let results = backend.search_filtered(&query, limit, &filter)?;
            for msg in results {
                if json {
                    println!("{}", serde_json::to_string(&msg)?);
                } else {
                    println!("{} | {} | {}", msg.timestamp, msg.address, msg.body);
                }
            }
            Ok(())
        }
        Commands::Datagen {
            output,
            size,
            seed,
            mms_ratio,
            burstiness,
        } => {
            println!("Generating {}GB test data to: {}", size, output.display());
            generate_xml(
                DataGenConfig {
                    target_size_gb: size,
                    avg_message_size_bytes: 128,
                    seed,
                    mms_ratio,
                    burstiness,
                },
                &output,
            )?;
            Ok(())
        }
        Commands::Models { db, json } => run_models(&db, json),
        Commands::ModelDelete {
            db,
            model_id,
            name,
            version,
            sha256,
        } => run_model_delete(
            &db,
            model_id.as_deref(),
            name.as_deref(),
            version.as_deref(),
            sha256.as_deref(),
        ),
        Commands::ModelPurge {
            db,
            model_id,
            name,
            version,
            sha256,
        } => run_model_purge(
            &db,
            model_id.as_deref(),
            name.as_deref(),
            version.as_deref(),
            sha256.as_deref(),
        ),
        Commands::ListContacts {
            db,
            min_messages,
            limit,
            json,
        } => run_list_contacts(&db, min_messages, limit, json),
        Commands::AnalyzeContact {
            db,
            contact_id,
            tz_offset_secs,
        } => run_analyze_contact(&db, &contact_id, tz_offset_secs),
        Commands::AnalyticsShow {
            db,
            contact_id,
            json,
        } => run_analytics_show(&db, &contact_id, json),
    }
}

fn run_list_contacts(db: &Path, min_messages: i64, limit: usize, json: bool) -> Result<()> {
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();

    let limit_clause = if limit == 0 {
        String::new()
    } else {
        format!(" LIMIT {}", limit)
    };
    let sql = format!(
        "SELECT c.id, c.display_name, c.source, COUNT(m.id) AS msg_count \
         FROM contacts c \
         JOIN contact_addresses ca ON ca.contact_id = c.id \
         LEFT JOIN messages m ON m.address = ca.address AND m.message_direction IN (1, 2) AND m.address NOT LIKE '%~%' \
         GROUP BY c.id \
         HAVING msg_count >= ?1 \
         ORDER BY msg_count DESC{}",
        limit_clause
    );

    #[derive(serde::Serialize)]
    struct Row {
        id: String,
        display_name: String,
        source: String,
        message_count: i64,
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<Row> = stmt
        .query_map(params![min_messages], |r| {
            Ok(Row {
                id: r.get(0)?,
                display_name: r.get(1)?,
                source: r
                    .get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| "unknown".into()),
                message_count: r.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    if json {
        let out = serde_json::to_string_pretty(&rows)?;
        println!("{}", out);
        return Ok(());
    }

    println!(
        "{:<38} {:<28} {:<8} {:>10}",
        "CONTACT_ID", "DISPLAY_NAME", "SOURCE", "MESSAGES"
    );
    println!("{}", "-".repeat(88));
    for row in &rows {
        let name_trunc: String = row.display_name.chars().take(28).collect();
        println!(
            "{:<38} {:<28} {:<8} {:>10}",
            row.id, name_trunc, row.source, row.message_count
        );
    }
    println!();
    println!("{} contact(s) shown", rows.len());
    Ok(())
}

fn run_analyze_contact(db: &Path, contact_id: &str, tz_offset_secs: i32) -> Result<()> {
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();

    let mut config = sms_analytics::OrchestratorConfig::default();
    config.tz_offset_secs = tz_offset_secs;

    println!("Running analytics for contact {} ...", contact_id);
    let started = std::time::Instant::now();
    let out = sms_analytics::compute_for_contact(conn, contact_id, &config)
        .map_err(|e| anyhow::anyhow!("orchestrator failed: {}", e))?;
    let wall_ms = started.elapsed().as_millis();

    println!();
    println!("==== analytics complete ====");
    println!("messages processed:  {}", out.message_count);
    println!("conversations:       {}", out.conversation_count);
    println!("internal compute:    {} ms", out.elapsed_ms);
    println!("wall clock:          {} ms", wall_ms);
    println!("had data:            {}", out.had_data);
    Ok(())
}

fn run_analytics_show(db: &Path, contact_id: &str, json: bool) -> Result<()> {
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();

    // Pull the headline numbers from pair_analytics + contact_analytics. We
    // print a small subset; full data lives in the JSON columns of pair_analytics.
    let display_name: String = conn
        .query_row(
            "SELECT display_name FROM contacts WHERE id = ?1",
            params![contact_id],
            |r| r.get(0),
        )
        .map_err(|_| anyhow::anyhow!("contact not found: {}", contact_id))?;

    #[derive(serde::Serialize)]
    struct Row {
        contact_id: String,
        display_name: String,
        total_conversations: i64,
        my_messages: i64,
        their_messages: i64,
        my_points: f64,
        their_points: f64,
        overall_score: Option<i64>,
        my_median_response_ms: Option<i64>,
        their_median_response_ms: Option<i64>,
        my_rapid_response_pct: Option<f64>,
        their_rapid_response_pct: Option<f64>,
        first_message_at: Option<i64>,
        last_message_at: Option<i64>,
    }

    let row: Row = conn
        .query_row(
            "SELECT \
                p.total_conversations, ca.my_message_count, ca.their_message_count, \
                p.my_points, p.their_points, p.overall_score, \
                p.my_median_response_ms, p.their_median_response_ms, \
                p.my_rapid_response_pct, p.their_rapid_response_pct, \
                p.first_message_at, p.last_message_at \
             FROM pair_analytics p \
             JOIN contact_analytics ca ON ca.contact_id = p.contact_id \
             WHERE p.contact_id = ?1",
            params![contact_id],
            |r| {
                Ok(Row {
                    contact_id: contact_id.to_string(),
                    display_name: display_name.clone(),
                    total_conversations: r.get(0)?,
                    my_messages: r.get(1)?,
                    their_messages: r.get(2)?,
                    my_points: r.get(3)?,
                    their_points: r.get(4)?,
                    overall_score: r.get(5)?,
                    my_median_response_ms: r.get(6)?,
                    their_median_response_ms: r.get(7)?,
                    my_rapid_response_pct: r.get(8)?,
                    their_rapid_response_pct: r.get(9)?,
                    first_message_at: r.get(10)?,
                    last_message_at: r.get(11)?,
                })
            },
        )
        .map_err(|e| {
            anyhow::anyhow!(
                "no analytics found for {} — run `analyze-contact --contact-id {}` first ({})",
                contact_id,
                contact_id,
                e
            )
        })?;

    if json {
        println!("{}", serde_json::to_string_pretty(&row)?);
        return Ok(());
    }

    println!(
        "==== analytics for {} ({}) ====",
        row.display_name, row.contact_id
    );
    println!();
    println!("Total conversations:    {}", row.total_conversations);
    println!("Your messages:          {}", row.my_messages);
    println!("Their messages:         {}", row.their_messages);
    println!("Your points:            {:.0}", row.my_points);
    println!("Their points:           {:.0}", row.their_points);
    if let Some(score) = row.overall_score {
        println!("Overall chat rating:    {}/100", score);
    }
    println!();
    println!("Response medians:");
    if let Some(ms) = row.my_median_response_ms {
        println!("  Yours:                {}", format_ms(ms));
    }
    if let Some(ms) = row.their_median_response_ms {
        println!("  Theirs:               {}", format_ms(ms));
    }
    if let Some(pct) = row.my_rapid_response_pct {
        println!("Rapid response (you):   {:.1}%", pct * 100.0);
    }
    if let Some(pct) = row.their_rapid_response_pct {
        println!("Rapid response (them):  {:.1}%", pct * 100.0);
    }
    println!();
    if let (Some(first), Some(last)) = (row.first_message_at, row.last_message_at) {
        let days = (last - first) as f64 / (24.0 * 60.0 * 60.0 * 1000.0);
        println!("Time span: {:.1} days ({} → {})", days, first, last);
    }
    Ok(())
}

fn format_ms(ms: i64) -> String {
    if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else if ms < 60 * 60_000 {
        format!("{}m", ms / 60_000)
    } else if ms < 24 * 60 * 60_000 {
        format!("{}h", ms / (60 * 60_000))
    } else {
        format!("{}d", ms / (24 * 60 * 60_000))
    }
}

fn run_verify(db: &Path, summary: Option<&Path>, rebuild: bool) -> Result<()> {
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();

    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(anyhow::anyhow!("Integrity check failed: {}", integrity));
    }

    let msg_count: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
    let att_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM attachments", [], |row| row.get(0))?;
    let fts_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row.get(0))?;
    if msg_count != fts_count {
        if rebuild {
            rebuild_fts(conn)?;
            println!("FTS index rebuilt");
        } else {
            return Err(anyhow::anyhow!(
                "FTS out of sync: messages {} vs fts {}",
                msg_count,
                fts_count
            ));
        }
    }

    if let Some(summary_path) = summary {
        let data = std::fs::read(summary_path)?;
        let summary: ImportSummary = serde_json::from_slice(&data)?;
        if summary.db != db.display().to_string() {
            println!("Warning: summary DB path differs from current DB path");
        }
        if summary.messages_inserted as i64 != msg_count {
            println!(
                "Warning: summary inserted {} != db count {}",
                summary.messages_inserted, msg_count
            );
        }
        if summary.attachments_written as i64 != att_count {
            println!(
                "Warning: summary attachments {} != db attachments {}",
                summary.attachments_written, att_count
            );
        }
    }

    let orphan_attachments: i64 = conn.query_row(
        "SELECT COUNT(*) FROM attachments a LEFT JOIN messages m ON a.message_id = m.id WHERE m.id IS NULL",
        [],
        |row| row.get(0),
    )?;
    if orphan_attachments > 0 {
        return Err(anyhow::anyhow!(
            "Found {} orphan attachment rows",
            orphan_attachments
        ));
    }

    checkpoint_wal(conn)?;
    println!(
        "Verify OK (messages: {}, attachments: {})",
        msg_count, att_count
    );
    Ok(())
}

fn run_semantic_search(
    db: &Path,
    query: &str,
    model_path: Option<&Path>,
    tokenizer_path: Option<&Path>,
    model_name: &str,
    model_version: &str,
    dimensions: usize,
    max_length: usize,
    normalize: bool,
    limit: usize,
    json: bool,
) -> Result<()> {
    if model_path.is_some() && tokenizer_path.is_none() {
        return Err(anyhow::anyhow!(
            "Tokenizer path is required when using an ONNX model"
        ));
    }
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();
    let mut service = EmbeddingService::new(EmbeddingConfig {
        model_path: model_path.map(|p| p.to_path_buf()),
        tokenizer_path: tokenizer_path.map(|p| p.to_path_buf()),
        model_name: model_name.to_string(),
        model_version: model_version.to_string(),
        dimensions,
        device: DevicePreference::Cpu,
        max_length,
        normalize,
        input_ids_name: None,
        attention_mask_name: None,
        token_type_ids_name: None,
        output_name: None,
    })?;
    let info = service.model_info().clone();
    let meta = service.model_meta().clone();
    let model_meta = sms_db::ModelMeta {
        dims: Some(meta.dimensions as i64),
        max_length: Some(meta.max_length as i64),
        normalize: Some(meta.normalize),
        tokenizer_path: meta.tokenizer_path.clone(),
        input_ids_name: meta.input_ids_name.clone(),
        attention_mask_name: meta.attention_mask_name.clone(),
        token_type_ids_name: meta.token_type_ids_name.clone(),
        output_name: meta.output_name.clone(),
    };
    let model_id = sms_db::upsert_ml_model_with_meta(
        conn,
        &info.name,
        &info.version,
        info.sha256.as_deref(),
        &model_meta,
    )?;
    let available: i64 = conn.query_row(
        "SELECT COUNT(*) FROM embeddings WHERE model_id = ?1",
        [model_id.as_str()],
        |row| row.get(0),
    )?;
    if available == 0 {
        eprintln!(
            "No embeddings found for model {} {}. Run `sms embed` first.",
            info.name, info.version
        );
        return Ok(());
    }

    let query_vec = service.embed(query)?;
    let hits = semantic_search(db, model_id.as_str(), &query_vec, limit)?;
    for hit in hits {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "score": hit.score,
                    "message": hit.message
                })
            );
        } else {
            println!(
                "{:.4} | {} | {}",
                hit.score, hit.message.address, hit.message.body
            );
        }
    }
    Ok(())
}

fn run_models(db: &Path, json: bool) -> Result<()> {
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();
    let total_messages: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE body_searchable != ''",
        [],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, name, version, sha256, created_at, dims, max_length, normalize, tokenizer_path, \
                (SELECT COUNT(*) FROM embeddings e WHERE e.model_id = ml_models.id) \
         FROM ml_models ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, Option<i64>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<String>>(8)?,
            row.get::<_, i64>(9)?,
        ))
    })?;
    for row in rows {
        let (
            id,
            name,
            version,
            sha256,
            created_at,
            dims,
            max_length,
            normalize,
            tokenizer_path,
            count,
        ) = row?;
        let pct = if total_messages > 0 {
            (count as f64 / total_messages as f64) * 100.0
        } else {
            0.0
        };
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "id": id,
                    "name": name,
                    "version": version,
                    "sha256": sha256,
                    "created_at": created_at,
                    "dims": dims,
                    "max_length": max_length,
                    "normalize": normalize.map(|v| v != 0),
                    "tokenizer_path": tokenizer_path,
                    "embeddings": count,
                    "coverage_pct": pct,
                    "total_messages": total_messages
                })
            );
        } else {
            let norm = normalize.map(|v| v != 0).unwrap_or(false);
            println!(
                "{} {} | {} embeddings ({:.1}%) | dims {:?} | max_len {:?} | norm {}",
                name, version, count, pct, dims, max_length, norm
            );
        }
    }
    Ok(())
}

fn resolve_model_id(
    conn: &rusqlite::Connection,
    model_id: Option<&str>,
    name: Option<&str>,
    version: Option<&str>,
    sha256: Option<&str>,
) -> Result<String> {
    if let Some(id) = model_id {
        let exists: Option<String> = conn
            .query_row("SELECT id FROM ml_models WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(found) = exists {
            return Ok(found);
        }
        return Err(anyhow::anyhow!("Model id not found"));
    }

    let name = name.ok_or_else(|| anyhow::anyhow!("Model name required"))?;
    let version = version.ok_or_else(|| anyhow::anyhow!("Model version required"))?;
    let mut ids = Vec::new();
    if let Some(sha) = sha256 {
        let mut stmt = conn
            .prepare("SELECT id FROM ml_models WHERE name = ?1 AND version = ?2 AND sha256 = ?3")?;
        let rows = stmt.query_map([name, version, sha], |row| row.get::<_, String>(0))?;
        for row in rows {
            ids.push(row?);
        }
    } else {
        let mut stmt = conn.prepare("SELECT id FROM ml_models WHERE name = ?1 AND version = ?2")?;
        let rows = stmt.query_map([name, version], |row| row.get::<_, String>(0))?;
        for row in rows {
            ids.push(row?);
        }
    }
    if ids.is_empty() {
        return Err(anyhow::anyhow!("No matching model found"));
    }
    if ids.len() > 1 {
        return Err(anyhow::anyhow!(
            "Multiple models match. Provide --sha256 or --model-id."
        ));
    }
    Ok(ids.remove(0))
}

fn run_model_delete(
    db: &Path,
    model_id: Option<&str>,
    name: Option<&str>,
    version: Option<&str>,
    sha256: Option<&str>,
) -> Result<()> {
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();
    let id = resolve_model_id(conn, model_id, name, version, sha256)?;
    let deleted = conn.execute("DELETE FROM ml_models WHERE id = ?1", [id.as_str()])?;
    println!("Deleted models: {}", deleted);
    Ok(())
}

fn run_model_purge(
    db: &Path,
    model_id: Option<&str>,
    name: Option<&str>,
    version: Option<&str>,
    sha256: Option<&str>,
) -> Result<()> {
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();
    let id = resolve_model_id(conn, model_id, name, version, sha256)?;
    let deleted = conn.execute("DELETE FROM embeddings WHERE model_id = ?1", [id.as_str()])?;
    println!("Deleted embeddings: {}", deleted);
    Ok(())
}

fn run_embeddings(
    db: &Path,
    model_path: Option<&Path>,
    tokenizer_path: Option<&Path>,
    model_name: &str,
    model_version: &str,
    dimensions: usize,
    max_length: usize,
    normalize: bool,
    batch_size: usize,
    limit: Option<usize>,
    since: Option<i64>,
    until: Option<i64>,
    dry_run: bool,
) -> Result<()> {
    if model_path.is_some() && tokenizer_path.is_none() {
        return Err(anyhow::anyhow!(
            "Tokenizer path is required when using an ONNX model"
        ));
    }
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();

    let mut service = EmbeddingService::new(EmbeddingConfig {
        model_path: model_path.map(|p| p.to_path_buf()),
        tokenizer_path: tokenizer_path.map(|p| p.to_path_buf()),
        model_name: model_name.to_string(),
        model_version: model_version.to_string(),
        dimensions,
        device: DevicePreference::Cpu,
        max_length,
        normalize,
        input_ids_name: None,
        attention_mask_name: None,
        token_type_ids_name: None,
        output_name: None,
    })?;
    let info = service.model_info().clone();
    let meta = service.model_meta().clone();
    let model_meta = sms_db::ModelMeta {
        dims: Some(meta.dimensions as i64),
        max_length: Some(meta.max_length as i64),
        normalize: Some(meta.normalize),
        tokenizer_path: meta.tokenizer_path.clone(),
        input_ids_name: meta.input_ids_name.clone(),
        attention_mask_name: meta.attention_mask_name.clone(),
        token_type_ids_name: meta.token_type_ids_name.clone(),
        output_name: meta.output_name.clone(),
    };
    let model_id = sms_db::upsert_ml_model_with_meta(
        conn,
        &info.name,
        &info.version,
        info.sha256.as_deref(),
        &model_meta,
    )?;

    let mut total = 0usize;
    let mut last_rowid = 0i64;
    let mut batches = 0usize;
    let batch_size = batch_size.max(1);

    loop {
        let remaining = limit.map(|l| l.saturating_sub(total));
        if remaining == Some(0) {
            break;
        }
        let cap = remaining.unwrap_or(batch_size).min(batch_size);

        let mut stmt = conn.prepare(
            "SELECT rowid, id, body_searchable FROM messages \
             WHERE rowid > :last \
               AND body_searchable != '' \
               AND (:since IS NULL OR timestamp >= :since) \
               AND (:until IS NULL OR timestamp <= :until) \
               AND NOT EXISTS ( \
                   SELECT 1 FROM embeddings \
                   WHERE embeddings.message_id = messages.id \
                     AND embeddings.model_id = :model_id \
               ) \
             ORDER BY rowid ASC \
             LIMIT :limit",
        )?;
        let rows = stmt.query_map(
            named_params! {
                ":last": last_rowid,
                ":since": since,
                ":until": until,
                ":model_id": model_id.as_str(),
                ":limit": cap as i64
            },
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;

        let mut batch = Vec::new();
        for row in rows {
            let (rowid, id, body) = row?;
            last_rowid = rowid;
            batch.push((id, body));
        }
        if batch.is_empty() {
            break;
        }

        if dry_run {
            total += batch.len();
            continue;
        }

        conn.execute_batch("BEGIN IMMEDIATE")?;
        let mut batch_inserted = 0usize;
        let mut limit_reached = false;
        let batch_result: Result<()> = (|| {
            let mut insert = conn.prepare_cached(
                "INSERT OR REPLACE INTO embeddings (message_id, model_id, dims, vector) \
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (id, body) in batch {
                let embedding = service.embed(&body)?;
                let bytes = encode_f32_vec(&embedding);
                insert.execute(params![id, model_id.as_str(), dimensions as i64, bytes])?;
                batch_inserted += 1;
                if let Some(limit) = limit {
                    if total + batch_inserted >= limit {
                        limit_reached = true;
                        break;
                    }
                }
            }
            Ok(())
        })();
        if let Err(err) = batch_result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(err);
        }
        conn.execute_batch("COMMIT")?;
        total += batch_inserted;
        batches += 1;
        if limit_reached {
            break;
        }
    }

    if dry_run {
        println!(
            "Would embed {} messages (model {} {})",
            total, info.name, info.version
        );
    } else {
        println!(
            "Embedded {} messages in {} batches (model {} {})",
            total, batches, info.name, info.version
        );
    }

    Ok(())
}

fn export_messages(
    db: &Path,
    writer: Box<dyn Write>,
    format: ExportFormat,
    options: &ExportOptions,
) -> Result<()> {
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();
    let query = options.query.as_deref();
    let thread_id = options.thread_id.as_deref();
    let address = options.address.as_deref();
    let address_like = options.address_like.as_deref();
    let body_contains = options.body_contains.as_deref();
    let message_type = options.message_type;
    let since = options.since;
    let until = options.until;
    let limit = options.limit;
    let offset = options.offset;
    let with_attachments = options.with_attachments;
    let attachments_select = if with_attachments {
        "(SELECT group_concat(file_path, ';') FROM attachments WHERE message_id = messages.id)"
    } else {
        "NULL"
    };
    let select_prefix = format!(
        "SELECT messages.id, messages.message_id, messages.timestamp, messages.address, \
                messages.body, messages.message_type, messages.thread_id, \
                (SELECT COUNT(*) FROM attachments WHERE message_id = messages.id) AS attachment_count, \
                {} AS attachment_paths",
        attachments_select
    );
    let fts_sql = format!(
        "{} FROM messages_fts \
         JOIN messages ON messages.rowid = messages_fts.rowid \
         WHERE messages_fts MATCH :q \
           AND (:since IS NULL OR messages.timestamp >= :since) \
           AND (:until IS NULL OR messages.timestamp <= :until) \
           AND (:address IS NULL OR messages.address = :address) \
           AND (:address_like IS NULL OR messages.address LIKE :address_like) \
           AND (:body_contains IS NULL OR messages.body_searchable LIKE :body_contains) \
           AND (:thread_id IS NULL OR messages.thread_id = :thread_id) \
           AND (:message_type IS NULL OR messages.message_type = :message_type) \
         ORDER BY bm25(messages_fts) \
         LIMIT :limit OFFSET :offset",
        select_prefix
    );
    let plain_sql = format!(
        "{} FROM messages \
         WHERE (:since IS NULL OR timestamp >= :since) \
           AND (:until IS NULL OR timestamp <= :until) \
           AND (:address IS NULL OR address = :address) \
           AND (:address_like IS NULL OR address LIKE :address_like) \
           AND (:body_contains IS NULL OR body_searchable LIKE :body_contains) \
           AND (:thread_id IS NULL OR thread_id = :thread_id) \
           AND (:message_type IS NULL OR message_type = :message_type) \
         ORDER BY timestamp DESC \
         LIMIT :limit OFFSET :offset",
        select_prefix
    );

    match format {
        ExportFormat::Jsonl => {
            let mut writer = BufWriter::new(writer);
            if let Some(q) = query {
                let mut stmt = conn.prepare(&fts_sql)?;
                let iter = stmt.query_map(
                    named_params! {
                        ":q": q,
                        ":since": since,
                        ":until": until,
                        ":address": address,
                        ":address_like": address_like,
                        ":body_contains": body_contains,
                        ":thread_id": thread_id,
                        ":message_type": message_type,
                        ":limit": limit as i64,
                        ":offset": offset as i64
                    },
                    |row| {
                        Ok(ExportRow {
                            id: row.get::<_, String>(0)?,
                            message_id: row.get(1)?,
                            timestamp: row.get(2)?,
                            address: row.get(3)?,
                            body: row.get(4)?,
                            message_type: message_type_to_str(row.get::<_, i32>(5)?).to_string(),
                            thread_id: row.get(6)?,
                            attachment_count: row.get(7)?,
                            attachment_paths: row.get(8)?,
                        })
                    },
                )?;
                for row in iter {
                    writeln!(writer, "{}", serde_json::to_string(&row?)?)?;
                }
            } else {
                let mut stmt = conn.prepare(&plain_sql)?;
                let iter = stmt.query_map(
                    named_params! {
                        ":since": since,
                        ":until": until,
                        ":address": address,
                        ":address_like": address_like,
                        ":body_contains": body_contains,
                        ":thread_id": thread_id,
                        ":message_type": message_type,
                        ":limit": limit as i64,
                        ":offset": offset as i64
                    },
                    |row| {
                        Ok(ExportRow {
                            id: row.get::<_, String>(0)?,
                            message_id: row.get(1)?,
                            timestamp: row.get(2)?,
                            address: row.get(3)?,
                            body: row.get(4)?,
                            message_type: message_type_to_str(row.get::<_, i32>(5)?).to_string(),
                            thread_id: row.get(6)?,
                            attachment_count: row.get(7)?,
                            attachment_paths: row.get(8)?,
                        })
                    },
                )?;
                for row in iter {
                    writeln!(writer, "{}", serde_json::to_string(&row?)?)?;
                }
            }
        }
        ExportFormat::Csv => {
            let mut csv = csv::WriterBuilder::new().from_writer(BufWriter::new(writer));
            if let Some(q) = query {
                let mut stmt = conn.prepare(&fts_sql)?;
                let iter = stmt.query_map(
                    named_params! {
                        ":q": q,
                        ":since": since,
                        ":until": until,
                        ":address": address,
                        ":address_like": address_like,
                        ":body_contains": body_contains,
                        ":thread_id": thread_id,
                        ":message_type": message_type,
                        ":limit": limit as i64,
                        ":offset": offset as i64
                    },
                    |row| {
                        Ok(ExportRow {
                            id: row.get::<_, String>(0)?,
                            message_id: row.get(1)?,
                            timestamp: row.get(2)?,
                            address: row.get(3)?,
                            body: row.get(4)?,
                            message_type: message_type_to_str(row.get::<_, i32>(5)?).to_string(),
                            thread_id: row.get(6)?,
                            attachment_count: row.get(7)?,
                            attachment_paths: row.get(8)?,
                        })
                    },
                )?;
                for row in iter {
                    csv.serialize(row?)?;
                }
            } else {
                let mut stmt = conn.prepare(&plain_sql)?;
                let iter = stmt.query_map(
                    named_params! {
                        ":since": since,
                        ":until": until,
                        ":address": address,
                        ":address_like": address_like,
                        ":body_contains": body_contains,
                        ":thread_id": thread_id,
                        ":message_type": message_type,
                        ":limit": limit as i64,
                        ":offset": offset as i64
                    },
                    |row| {
                        Ok(ExportRow {
                            id: row.get::<_, String>(0)?,
                            message_id: row.get(1)?,
                            timestamp: row.get(2)?,
                            address: row.get(3)?,
                            body: row.get(4)?,
                            message_type: message_type_to_str(row.get::<_, i32>(5)?).to_string(),
                            thread_id: row.get(6)?,
                            attachment_count: row.get(7)?,
                            attachment_paths: row.get(8)?,
                        })
                    },
                )?;
                for row in iter {
                    csv.serialize(row?)?;
                }
            }
            csv.flush()?;
        }
    }
    Ok(())
}

fn export_attachments(
    db: &Path,
    writer: Box<dyn Write>,
    format: ExportFormat,
    options: &AttachmentExportOptions,
) -> Result<()> {
    let db_conn = Database::open(db, ResourceProfile::detect())?;
    let conn = db_conn.connection();
    let address = options.address.as_deref();
    let address_like = options.address_like.as_deref();
    let body_contains = options.body_contains.as_deref();
    let thread_id = options.thread_id.as_deref();
    let message_type = options.message_type;
    let since = options.since;
    let until = options.until;
    let mime = options.mime.as_deref();
    let limit = options.limit;
    let offset = options.offset;
    match format {
        ExportFormat::Jsonl => {
            let mut writer = BufWriter::new(writer);
            let mut stmt = conn.prepare(
                "SELECT attachments.id, attachments.message_id, messages.timestamp, messages.address, \
                        attachments.mime_type, attachments.file_path, attachments.thumbnail_path, attachments.file_hash \
                 FROM attachments \
                 JOIN messages ON attachments.message_id = messages.id \
                 WHERE (:since IS NULL OR messages.timestamp >= :since) \
                   AND (:until IS NULL OR messages.timestamp <= :until) \
                   AND (:address IS NULL OR messages.address = :address) \
                   AND (:address_like IS NULL OR messages.address LIKE :address_like) \
                   AND (:body_contains IS NULL OR messages.body_searchable LIKE :body_contains) \
                   AND (:thread_id IS NULL OR messages.thread_id = :thread_id) \
                   AND (:message_type IS NULL OR messages.message_type = :message_type) \
                   AND (:mime IS NULL OR attachments.mime_type = :mime) \
                 ORDER BY messages.timestamp DESC \
                 LIMIT :limit OFFSET :offset",
            )?;
            let iter = stmt.query_map(
                named_params! {
                    ":since": since,
                    ":until": until,
                    ":address": address,
                    ":address_like": address_like,
                    ":body_contains": body_contains,
                    ":thread_id": thread_id,
                    ":message_type": message_type,
                    ":mime": mime,
                    ":limit": limit as i64,
                    ":offset": offset as i64
                },
                |row| {
                    let hash: Vec<u8> = row.get(7)?;
                    Ok(AttachmentExportRow {
                        id: row.get::<_, String>(0)?,
                        message_id: row.get::<_, String>(1)?,
                        timestamp: row.get(2)?,
                        address: row.get(3)?,
                        mime_type: row.get(4)?,
                        file_path: row.get(5)?,
                        thumbnail_path: row.get(6)?,
                        file_hash: hex_bytes(&hash),
                    })
                },
            )?;
            for row in iter {
                writeln!(writer, "{}", serde_json::to_string(&row?)?)?;
            }
        }
        ExportFormat::Csv => {
            let mut csv = csv::WriterBuilder::new().from_writer(BufWriter::new(writer));
            let mut stmt = conn.prepare(
                "SELECT attachments.id, attachments.message_id, messages.timestamp, messages.address, \
                        attachments.mime_type, attachments.file_path, attachments.thumbnail_path, attachments.file_hash \
                 FROM attachments \
                 JOIN messages ON attachments.message_id = messages.id \
                 WHERE (:since IS NULL OR messages.timestamp >= :since) \
                   AND (:until IS NULL OR messages.timestamp <= :until) \
                   AND (:address IS NULL OR messages.address = :address) \
                   AND (:address_like IS NULL OR messages.address LIKE :address_like) \
                   AND (:body_contains IS NULL OR messages.body_searchable LIKE :body_contains) \
                   AND (:thread_id IS NULL OR messages.thread_id = :thread_id) \
                   AND (:message_type IS NULL OR messages.message_type = :message_type) \
                   AND (:mime IS NULL OR attachments.mime_type = :mime) \
                 ORDER BY messages.timestamp DESC \
                 LIMIT :limit OFFSET :offset",
            )?;
            let iter = stmt.query_map(
                named_params! {
                    ":since": since,
                    ":until": until,
                    ":address": address,
                    ":address_like": address_like,
                    ":body_contains": body_contains,
                    ":thread_id": thread_id,
                    ":message_type": message_type,
                    ":mime": mime,
                    ":limit": limit as i64,
                    ":offset": offset as i64
                },
                |row| {
                    let hash: Vec<u8> = row.get(7)?;
                    Ok(AttachmentExportRow {
                        id: row.get::<_, String>(0)?,
                        message_id: row.get::<_, String>(1)?,
                        timestamp: row.get(2)?,
                        address: row.get(3)?,
                        mime_type: row.get(4)?,
                        file_path: row.get(5)?,
                        thumbnail_path: row.get(6)?,
                        file_hash: hex_bytes(&hash),
                    })
                },
            )?;
            for row in iter {
                csv.serialize(row?)?;
            }
            csv.flush()?;
        }
    }
    Ok(())
}

fn message_type_to_str(message_type: i32) -> &'static str {
    match message_type {
        x if x == MessageType::Mms as i32 => "mms",
        x if x == MessageType::Rcs as i32 => "rcs",
        _ => "sms",
    }
}

fn message_type_to_i32(message_type: ExportMessageType) -> i32 {
    match message_type {
        ExportMessageType::Sms => MessageType::Sms as i32,
        ExportMessageType::Mms => MessageType::Mms as i32,
        ExportMessageType::Rcs => MessageType::Rcs as i32,
    }
}

fn normalize_address(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect()
}

fn normalize_like(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains('%') {
        trimmed.to_string()
    } else {
        format!("%{}%", trimmed)
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn encode_f32_vec(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for v in vector {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    bytes
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}
