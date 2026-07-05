//! XML parsing and ingest pipeline

use base64::engine::general_purpose;
use base64::Engine;
use blake3::Hasher;
use crossbeam_channel::{bounded, Receiver, Sender};
use memchr::memchr_iter;
use memmap2::Mmap;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use sms_errors::{AppError, Result};
use sms_types::{AttachmentRef, Message, MessageDirection, MessageType};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Condvar, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub struct MessageBoundary {
    pub start_offset: u64,
    pub end_offset: u64,
}

#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub batch_size: usize,
    pub queue_bytes: usize,
    pub read_buffer_bytes: usize,
    pub use_boundary_scan: bool,
    pub parser_threads: usize,
    pub recover_on_error: bool,
    pub defer_thumbnails: bool,
    pub thumbnail_workers: usize,
    pub thumbnail_queue_capacity: usize,
    pub resume: bool,
    pub media_dir: Option<PathBuf>,
    pub write_attachments: bool,
    pub thumbnail_size: u32,
    pub writer_mode: sms_db::ConnectionMode,
    pub progress: Option<Arc<IngestProgress>>,
}

impl Default for IngestOptions {
    fn default() -> Self {
        Self {
            batch_size: 10_000,
            queue_bytes: 256 * 1024 * 1024,
            read_buffer_bytes: 8 * 1024 * 1024,
            use_boundary_scan: true,
            parser_threads: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            recover_on_error: true,
            defer_thumbnails: true,
            thumbnail_workers: 2,
            thumbnail_queue_capacity: 1024,
            resume: true,
            media_dir: None,
            write_attachments: true,
            thumbnail_size: 256,
            writer_mode: sms_db::ConnectionMode::Import,
            progress: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct IngestStats {
    pub messages_seen: u64,
    pub messages_inserted: u64,
    pub attachments_written: u64,
    pub bytes_read: u64,
    pub parse_errors: u64,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone, Default)]
struct ParseStats {
    bytes_read: u64,
    messages_seen: u64,
    attachments_written: u64,
    parse_errors: u64,
    incomplete: bool,
}

#[derive(Debug)]
struct ParseMetrics {
    last_emit: Instant,
    last_bytes: u64,
    last_messages: u64,
}

#[derive(Debug, Clone, Default)]
struct WriterStats {
    messages_inserted: u64,
}

#[derive(Debug)]
struct IngestItem {
    msg: Message,
    size: usize,
    offset: u64,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    last_committed_offset: u64,
    messages_imported: u64,
    started_at: String,
}

struct Budget {
    limit: usize,
    current: Mutex<usize>,
    cvar: Condvar,
    /// Set when the consumer (writer) dies on a hard error. Parsers blocked
    /// in `acquire` must wake and proceed to their channel send (which then
    /// fails fast) instead of waiting forever on releases that never come.
    failed: AtomicBool,
}

#[derive(Clone)]
struct AttachmentContext {
    input_dir: Option<PathBuf>,
    media_dir: Option<PathBuf>,
    thumbnail_size: u32,
    thumbnail_queue: Option<sms_media::ThumbnailQueue>,
    progress: Option<Arc<IngestProgress>>,
}

#[derive(Debug, Default)]
pub struct IngestProgress {
    pub total_bytes: AtomicU64,
    pub bytes_read: AtomicU64,
    pub messages_seen: AtomicU64,
    pub messages_inserted: AtomicU64,
    pub attachments_written: AtomicU64,
    pub attachments_skipped: AtomicU64,
    pub parse_errors: AtomicU64,
    pub cancelled: AtomicBool,
    pub paused: AtomicBool,
    pub skip_current_file: AtomicBool,
    pub error_samples: Mutex<Vec<String>>,
    pub skipped_samples: Mutex<Vec<String>>,
    pub current_file: Mutex<Option<String>>,
}

impl Budget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            current: Mutex::new(0),
            cvar: Condvar::new(),
            failed: AtomicBool::new(false),
        }
    }

    /// Clamp oversized reservations the same way in acquire and release —
    /// they must agree or the accounting drifts and backpressure erodes.
    fn clamp(&self, size: usize) -> usize {
        size.min(self.limit)
    }

    fn acquire(&self, size: usize) {
        let size = self.clamp(size);
        if size == 0 {
            return;
        }
        let mut cur = self.current.lock().unwrap();
        while *cur + size > self.limit && !self.failed.load(Ordering::Relaxed) {
            cur = self.cvar.wait(cur).unwrap();
        }
        *cur += size;
    }

    fn release(&self, size: usize) {
        let size = self.clamp(size);
        if size == 0 {
            return;
        }
        let mut cur = self.current.lock().unwrap();
        *cur = cur.saturating_sub(size);
        self.cvar.notify_one();
    }

    /// Poison the budget after a fatal writer error: wake every blocked
    /// producer so the pipeline unwinds via failed channel sends instead of
    /// deadlocking on a consumer that will never release bytes again.
    fn fail(&self) {
        self.failed.store(true, Ordering::Relaxed);
        // Lock to serialize with waiters entering the condvar wait.
        let _cur = self.current.lock().unwrap();
        self.cvar.notify_all();
    }
}

pub fn ingest_file(input: &Path, db_path: &Path, options: &IngestOptions) -> Result<IngestStats> {
    let start = Instant::now();
    // Wall-clock seconds at the moment ingest begins. Used after the writer finishes
    // to identify which messages.created_at rows landed during this run, so we can
    // mark only the affected contacts' analytics caches as stale.
    let start_unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let checkpoint_path = checkpoint_path_for(db_path);
    let mut checkpoint = if options.resume {
        load_checkpoint(&checkpoint_path).unwrap_or_default()
    } else {
        Checkpoint::default()
    };
    if checkpoint.started_at.is_empty() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        checkpoint.started_at = now.to_string();
    }

    let offset = checkpoint.last_committed_offset;
    let file_size = std::fs::metadata(input)?.len();
    if offset > file_size {
        return Err(AppError::CheckpointCorrupted);
    }
    if let Some(progress) = &options.progress {
        progress.total_bytes.store(file_size, Ordering::Relaxed);
        progress.bytes_read.store(0, Ordering::Relaxed);
        progress.messages_seen.store(0, Ordering::Relaxed);
        progress.attachments_written.store(0, Ordering::Relaxed);
        progress.attachments_skipped.store(0, Ordering::Relaxed);
        progress.parse_errors.store(0, Ordering::Relaxed);
        progress.skip_current_file.store(false, Ordering::Relaxed);
        if let Ok(mut lock) = progress.current_file.lock() {
            *lock = Some(input.display().to_string());
        }
    }

    let budget = Arc::new(Budget::new(options.queue_bytes));
    let (tx, rx) = bounded::<IngestItem>(1024);

    let writer_budget = Arc::clone(&budget);
    let db_path = db_path.to_path_buf();
    let writer_db_path = db_path.clone();
    let batch_size = options.batch_size;
    let writer_checkpoint_path = checkpoint_path.clone();
    let writer_checkpoint = checkpoint;

    let writer_mode = options.writer_mode;
    let writer_progress = options.progress.clone();
    let writer_handle = thread::spawn(move || {
        let result = run_writer(
            rx,
            &writer_db_path,
            batch_size,
            &writer_checkpoint_path,
            &writer_budget,
            writer_checkpoint,
            writer_mode,
            writer_progress,
        );
        if result.is_err() {
            // Wake producers blocked on the byte budget; with the receiver
            // dropped their next send fails and the pipeline unwinds instead
            // of deadlocking (nobody will release budget bytes again).
            writer_budget.fail();
        }
        result
    });

    let input_dir = input.parent().map(|p| p.to_path_buf());
    let media_dir = if options.write_attachments {
        let dir = options.media_dir.clone().unwrap_or_else(|| {
            let base = db_path.parent().unwrap_or_else(|| Path::new("."));
            base.join("media")
        });
        Some(dir)
    } else {
        None
    };

    let (thumb_queue, thumb_workers) = if options.write_attachments && options.defer_thumbnails {
        let (queue, workers) = sms_media::ThumbnailQueue::spawn(
            options.thumbnail_workers.max(1),
            options.thumbnail_queue_capacity.max(1),
        );
        (Some(queue), workers)
    } else {
        (None, Vec::new())
    };

    let attach_ctx = AttachmentContext {
        input_dir,
        media_dir,
        thumbnail_size: options.thumbnail_size,
        thumbnail_queue: thumb_queue,
        progress: options.progress.clone(),
    };

    let parse_stats_result = if options.use_boundary_scan {
        parse_parallel_boundaries(
            input,
            offset,
            &tx,
            &budget,
            &attach_ctx,
            options.parser_threads,
            options.progress.clone(),
            options.recover_on_error,
        )
    } else {
        parse_stream(
            input,
            offset,
            options.read_buffer_bytes,
            &tx,
            &budget,
            &attach_ctx,
            options.progress.clone(),
            options.recover_on_error,
        )
    };

    let mut parse_stats = match parse_stats_result {
        Ok(stats) => stats,
        Err(err) => {
            drop(tx);
            if let Ok(Err(writer_err)) = writer_handle.join() {
                return Err(writer_err);
            }
            return Err(err);
        }
    };

    if parse_stats.incomplete && options.recover_on_error && !options.use_boundary_scan {
        let tail_stats = parse_parallel_boundaries(
            input,
            parse_stats.bytes_read,
            &tx,
            &budget,
            &attach_ctx,
            options.parser_threads,
            options.progress.clone(),
            options.recover_on_error,
        )?;
        parse_stats.messages_seen += tail_stats.messages_seen;
        parse_stats.attachments_written += tail_stats.attachments_written;
        parse_stats.parse_errors += tail_stats.parse_errors;
        if tail_stats.bytes_read > parse_stats.bytes_read {
            parse_stats.bytes_read = tail_stats.bytes_read;
        }
    }

    drop(tx);
    for handle in thumb_workers {
        let _ = handle.join();
    }
    let writer_stats = writer_handle.join().map_err(|_| AppError::Parse {
        offset: 0,
        details: "Writer thread panicked".into(),
    })??;

    // Post-ingest finalization. Best-effort — these failing should not abort an
    // otherwise-successful import. The writer connection is dropped at this point,
    // so we open a fresh one for the bookkeeping passes.
    if writer_stats.messages_inserted > 0 {
        finalize_post_ingest(&db_path, start_unix_secs);
    }

    Ok(IngestStats {
        messages_seen: parse_stats.messages_seen,
        messages_inserted: writer_stats.messages_inserted,
        attachments_written: parse_stats.attachments_written,
        bytes_read: parse_stats.bytes_read,
        parse_errors: parse_stats.parse_errors,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

/// Run after the ingest writer thread completes. Bootstraps any new contacts
/// implied by freshly-inserted messages and flags those contacts' analytics
/// caches as stale. Errors are logged but never propagated — a failed
/// bookkeeping pass should never look like a failed import to the caller.
fn finalize_post_ingest(db_path: &Path, ingest_start_unix_secs: i64) {
    let profile = sms_config::ResourceProfile::detect();
    let db = match sms_db::Database::open(db_path, profile) {
        Ok(db) => db,
        Err(err) => {
            tracing::warn!(
                ?err,
                "post-ingest: could not reopen database for finalization"
            );
            return;
        }
    };
    let conn = db.connection();

    match sms_db::auto_create_contacts_from_messages(conn) {
        Ok(stats) => {
            if stats.contacts_created > 0 || stats.addresses_skipped_group > 0 {
                tracing::info!(
                    contacts_created = stats.contacts_created,
                    addresses_linked = stats.addresses_linked,
                    addresses_skipped_group = stats.addresses_skipped_group,
                    "post-ingest: auto-created contacts"
                );
            }
        }
        Err(err) => {
            tracing::warn!(?err, "post-ingest: auto-create contacts failed");
        }
    }

    match sms_db::mark_contact_analytics_stale_since(conn, ingest_start_unix_secs) {
        Ok(count) => {
            if count > 0 {
                tracing::info!(
                    stale_marked = count,
                    "post-ingest: marked contact analytics stale"
                );
            }
        }
        Err(err) => {
            tracing::warn!(?err, "post-ingest: marking analytics stale failed");
        }
    }
}

#[allow(clippy::too_many_arguments)] // pipeline plumbing; a params struct isn't clearer here
fn run_writer(
    rx: Receiver<IngestItem>,
    db_path: &Path,
    batch_size: usize,
    checkpoint_path: &Path,
    budget: &Budget,
    mut checkpoint: Checkpoint,
    writer_mode: sms_db::ConnectionMode,
    progress: Option<Arc<IngestProgress>>,
) -> Result<WriterStats> {
    let profile = sms_config::ResourceProfile::detect();
    let mut writer = sms_db::BatchWriter::new_with_mode(db_path, profile, batch_size, writer_mode)?;
    let mut batch: Vec<IngestItem> = Vec::with_capacity(batch_size);

    let mut stats = WriterStats::default();
    let mut last_offset = checkpoint.last_committed_offset;
    let mut last_emit = Instant::now();
    let mut last_inserted = checkpoint.messages_imported;

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(item) => {
                last_offset = item.offset;
                batch.push(item);
                if batch.len() >= batch_size {
                    flush_batch(
                        &mut writer,
                        &mut batch,
                        &mut stats,
                        &mut last_offset,
                        &mut checkpoint,
                        checkpoint_path,
                        budget,
                        progress.as_ref(),
                    )?;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if !batch.is_empty() {
                    flush_batch(
                        &mut writer,
                        &mut batch,
                        &mut stats,
                        &mut last_offset,
                        &mut checkpoint,
                        checkpoint_path,
                        budget,
                        progress.as_ref(),
                    )?;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                if !batch.is_empty() {
                    flush_batch(
                        &mut writer,
                        &mut batch,
                        &mut stats,
                        &mut last_offset,
                        &mut checkpoint,
                        checkpoint_path,
                        budget,
                        progress.as_ref(),
                    )?;
                }
                break;
            }
        }
        if last_emit.elapsed() >= Duration::from_secs(2) {
            let elapsed = last_emit.elapsed().as_secs_f64().max(0.001);
            let delta = checkpoint.messages_imported.saturating_sub(last_inserted);
            let msg_s = delta as f64 / elapsed;
            tracing::info!(
                messages_inserted = checkpoint.messages_imported,
                msg_per_s = msg_s,
                "ingest_writer"
            );
            last_emit = Instant::now();
            last_inserted = checkpoint.messages_imported;
        }
    }

    Ok(stats)
}

#[allow(clippy::too_many_arguments)] // pipeline plumbing; a params struct isn't clearer here
fn flush_batch(
    writer: &mut sms_db::BatchWriter,
    batch: &mut Vec<IngestItem>,
    stats: &mut WriterStats,
    last_offset: &mut u64,
    checkpoint: &mut Checkpoint,
    checkpoint_path: &Path,
    budget: &Budget,
    progress: Option<&Arc<IngestProgress>>,
) -> Result<()> {
    let messages: Vec<Message> = batch.iter().map(|i| i.msg.clone()).collect();
    let inserted = match writer.insert_batch(&messages) {
        Ok(n) => n,
        Err(err) => {
            // Free the reserved bytes even on failure so producers aren't
            // stranded on the budget while the error propagates.
            for item in batch.drain(..) {
                budget.release(item.size);
            }
            return Err(err);
        }
    };

    for item in batch.drain(..) {
        budget.release(item.size);
    }

    checkpoint.last_committed_offset = *last_offset;
    checkpoint.messages_imported += inserted as u64;
    save_checkpoint(checkpoint_path, checkpoint)?;

    stats.messages_inserted = checkpoint.messages_imported;
    if let Some(progress) = progress {
        progress
            .messages_inserted
            .store(stats.messages_inserted, Ordering::Relaxed);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // pipeline plumbing; a params struct isn't clearer here
fn parse_stream(
    path: &Path,
    start_offset: u64,
    read_buffer_bytes: usize,
    tx: &Sender<IngestItem>,
    budget: &Budget,
    attach_ctx: &AttachmentContext,
    progress: Option<Arc<IngestProgress>>,
    recover_on_error: bool,
) -> Result<ParseStats> {
    let mut file = File::open(path)?;
    detect_encoding(&mut file)?;
    if start_offset > 0 {
        file.seek(SeekFrom::Start(start_offset))?;
    }

    let reader = BufReader::with_capacity(read_buffer_bytes, file);
    let mut xml = Reader::from_reader(reader);
    xml.trim_text(true);

    let mut buf = Vec::new();
    let mut stats = ParseStats {
        bytes_read: start_offset,
        messages_seen: 0,
        attachments_written: 0,
        parse_errors: 0,
        incomplete: false,
    };
    let mut metrics = ParseMetrics {
        last_emit: Instant::now(),
        last_bytes: start_offset,
        last_messages: 0,
    };

    loop {
        check_control(&progress)?;
        match xml.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) => {
                if is_message_tag(&e) {
                    let offset = start_offset + xml.buffer_position() as u64;
                    match parse_message_from_tag(&e) {
                        Ok(msg) => {
                            let msg = finalize_message(msg);
                            stats.messages_seen += 1;
                            stats.attachments_written += msg.attachments.len() as u64;
                            let size = estimate_message_size(&msg);
                            budget.acquire(size);
                            tx.send(IngestItem { msg, size, offset })
                                .map_err(|e| AppError::Channel(e.to_string()))?;
                            stats.bytes_read = offset;
                            update_progress(
                                &progress,
                                stats.messages_seen,
                                stats.attachments_written,
                                stats.bytes_read,
                                stats.parse_errors,
                            );
                            maybe_log_parse_metrics(&mut metrics, &stats);
                        }
                        Err(err) => {
                            if recover_on_error {
                                record_parse_error(&progress, offset, &err);
                                stats.parse_errors += 1;
                                stats.bytes_read = offset;
                                update_progress(
                                    &progress,
                                    stats.messages_seen,
                                    stats.attachments_written,
                                    stats.bytes_read,
                                    stats.parse_errors,
                                );
                                maybe_log_parse_metrics(&mut metrics, &stats);
                            } else {
                                return Err(err);
                            }
                        }
                    }
                }
            }
            Ok(Event::Start(e)) => {
                if is_message_tag(&e) {
                    let msg_result = if e.name().as_ref() == b"mms" {
                        parse_mms_block(
                            &mut xml,
                            &e,
                            attach_ctx.input_dir.as_deref(),
                            attach_ctx.media_dir.as_deref(),
                            attach_ctx.thumbnail_size,
                            attach_ctx.thumbnail_queue.clone(),
                            attach_ctx.progress.clone(),
                        )
                    } else {
                        parse_message_from_tag(&e)
                    };
                    let offset = start_offset + xml.buffer_position() as u64;
                    match msg_result {
                        Ok(msg) => {
                            let msg = finalize_message(msg);
                            stats.messages_seen += 1;
                            stats.attachments_written += msg.attachments.len() as u64;
                            let size = estimate_message_size(&msg);
                            budget.acquire(size);
                            tx.send(IngestItem { msg, size, offset })
                                .map_err(|e| AppError::Channel(e.to_string()))?;
                            stats.bytes_read = offset;
                            update_progress(
                                &progress,
                                stats.messages_seen,
                                stats.attachments_written,
                                stats.bytes_read,
                                stats.parse_errors,
                            );
                            maybe_log_parse_metrics(&mut metrics, &stats);
                        }
                        Err(err) => {
                            if recover_on_error {
                                record_parse_error(&progress, offset, &err);
                                stats.parse_errors += 1;
                                stats.bytes_read = offset;
                                update_progress(
                                    &progress,
                                    stats.messages_seen,
                                    stats.attachments_written,
                                    stats.bytes_read,
                                    stats.parse_errors,
                                );
                                maybe_log_parse_metrics(&mut metrics, &stats);
                            } else {
                                return Err(err);
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                stats.parse_errors += 1;
                record_parse_error(&progress, start_offset + xml.buffer_position() as u64, &e);
                update_progress(
                    &progress,
                    stats.messages_seen,
                    stats.attachments_written,
                    stats.bytes_read,
                    stats.parse_errors,
                );
                maybe_log_parse_metrics(&mut metrics, &stats);
                if recover_on_error {
                    stats.incomplete = true;
                    break;
                } else {
                    return Err(AppError::Parse {
                        offset: stats.bytes_read,
                        details: e.to_string(),
                    });
                }
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(stats)
}

fn maybe_log_parse_metrics(metrics: &mut ParseMetrics, stats: &ParseStats) {
    let now = Instant::now();
    if now.duration_since(metrics.last_emit) < Duration::from_secs(2) {
        return;
    }
    let delta_bytes = stats.bytes_read.saturating_sub(metrics.last_bytes);
    let delta_messages = stats.messages_seen.saturating_sub(metrics.last_messages);
    let elapsed = now
        .duration_since(metrics.last_emit)
        .as_secs_f64()
        .max(0.001);
    let mb_s = (delta_bytes as f64 / (1024.0 * 1024.0)) / elapsed;
    let msg_s = (delta_messages as f64) / elapsed;
    tracing::info!(
        bytes_read = stats.bytes_read,
        messages_seen = stats.messages_seen,
        parse_errors = stats.parse_errors,
        mb_per_s = mb_s,
        msg_per_s = msg_s,
        "ingest_parse"
    );
    metrics.last_emit = now;
    metrics.last_bytes = stats.bytes_read;
    metrics.last_messages = stats.messages_seen;
}

fn check_control(progress: &Option<Arc<IngestProgress>>) -> Result<()> {
    if let Some(progress) = progress {
        if progress.skip_current_file.swap(false, Ordering::Relaxed) {
            record_skipped_file(progress, "Skipped by user");
            return Err(AppError::SkippedFile);
        }
        if progress.cancelled.load(Ordering::Relaxed) {
            return Err(AppError::Cancelled);
        }
        while progress.paused.load(Ordering::Relaxed) {
            if progress.cancelled.load(Ordering::Relaxed) {
                return Err(AppError::Cancelled);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    Ok(())
}

fn update_progress(
    progress: &Option<Arc<IngestProgress>>,
    messages_seen: u64,
    attachments_written: u64,
    bytes_read: u64,
    parse_errors: u64,
) {
    if let Some(progress) = progress {
        progress
            .messages_seen
            .store(messages_seen, Ordering::Relaxed);
        progress
            .attachments_written
            .store(attachments_written, Ordering::Relaxed);
        progress.bytes_read.store(bytes_read, Ordering::Relaxed);
        progress.parse_errors.store(parse_errors, Ordering::Relaxed);
    }
}

fn record_parse_error(
    progress: &Option<Arc<IngestProgress>>,
    offset: u64,
    err: &dyn std::fmt::Display,
) {
    if let Some(progress) = progress {
        if let Ok(mut lock) = progress.error_samples.lock() {
            if lock.len() < 50 {
                lock.push(format!("offset {}: {}", offset, err));
            }
        }
    }
}

fn record_skipped_file(progress: &Arc<IngestProgress>, reason: &str) {
    let label = if let Ok(lock) = progress.current_file.lock() {
        lock.as_ref()
            .map(|path| format!("file {}: {}", path, reason))
            .unwrap_or_else(|| format!("file <unknown>: {}", reason))
    } else {
        format!("file <unknown>: {}", reason)
    };
    if let Ok(mut lock) = progress.skipped_samples.lock() {
        if lock.len() < 100 {
            lock.push(label);
        }
    }
}

fn record_skipped_attachment(progress: &Option<Arc<IngestProgress>>, label: &str) {
    if let Some(progress) = progress {
        progress.attachments_skipped.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut lock) = progress.skipped_samples.lock() {
            if lock.len() < 100 {
                lock.push(label.to_string());
            }
        }
    }
}

#[derive(Debug)]
struct ParsedMessage {
    msg: Message,
    size: usize,
    offset: u64,
    attachments: usize,
}

// One short-lived value per parsed message; the size gap between the Ok and
// Err variants isn't worth a Box indirection on the hot path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum ParseOutcome {
    Ok(ParsedMessage),
    Err { offset: u64 },
}

#[allow(clippy::too_many_arguments)] // pipeline plumbing; a params struct isn't clearer here
fn parse_parallel_boundaries(
    path: &Path,
    start_offset: u64,
    tx: &Sender<IngestItem>,
    budget: &Budget,
    attach_ctx: &AttachmentContext,
    parser_threads: usize,
    progress: Option<Arc<IngestProgress>>,
    recover_on_error: bool,
) -> Result<ParseStats> {
    let mut file = File::open(path)?;
    detect_encoding(&mut file)?;
    // SAFETY: mapping a file that another process truncates or rewrites while
    // we read is undefined behavior (memmap2's documented caveat). We accept
    // that risk for read-only ingest of a user-selected export file; callers
    // must not import a file that is still being written.
    let map = unsafe { Mmap::map(&file)? };

    let threads = parser_threads.max(1);
    let queue_cap = threads.saturating_mul(8).max(16);

    let (work_tx, work_rx) = bounded::<(usize, MessageBoundary)>(queue_cap);
    let (result_tx, result_rx) = bounded::<(usize, ParseOutcome)>(queue_cap);

    let map = Arc::new(map);
    let mut handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let work_rx = work_rx.clone();
        let result_tx = result_tx.clone();
        let map = Arc::clone(&map);
        let input_dir = attach_ctx.input_dir.clone();
        let media_dir = attach_ctx.media_dir.clone();
        let thumbnail_size = attach_ctx.thumbnail_size;
        let progress = progress.clone();
        let handle = thread::spawn(move || {
            for (index, boundary) in work_rx.iter() {
                if let Some(progress) = &progress {
                    if progress.skip_current_file.load(Ordering::Relaxed) {
                        break;
                    }
                    if progress.cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    while progress.paused.load(Ordering::Relaxed) {
                        if progress.cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(100));
                    }
                }
                let outcome = match parse_boundary_slice(
                    &map,
                    boundary,
                    input_dir.as_deref(),
                    media_dir.as_deref(),
                    thumbnail_size,
                    progress.clone(),
                ) {
                    Ok(parsed) => ParseOutcome::Ok(parsed),
                    Err(_) => ParseOutcome::Err {
                        offset: boundary.end_offset,
                    },
                };
                let _ = result_tx.send((index, outcome));
            }
        });
        handles.push(handle);
    }
    drop(result_tx);

    let scan_map = Arc::clone(&map);
    let scan_progress = progress.clone();
    let scan_handle = thread::spawn(move || {
        scan_boundaries_streaming(
            scan_map.as_ref(),
            start_offset,
            work_tx,
            scan_progress.as_ref(),
        )
    });

    let mut stats = ParseStats::default();
    let mut metrics = ParseMetrics {
        last_emit: Instant::now(),
        last_bytes: start_offset,
        last_messages: 0,
    };
    let mut pending = std::collections::BTreeMap::<usize, ParseOutcome>::new();
    let mut next_index = 0usize;
    let mut last_offset = start_offset;

    for (index, outcome) in result_rx.iter() {
        pending.insert(index, outcome);
        while let Some(outcome) = pending.remove(&next_index) {
            match outcome {
                ParseOutcome::Ok(item) => {
                    last_offset = item.offset;
                    stats.attachments_written += item.attachments as u64;
                    stats.messages_seen += 1;
                    update_progress(
                        &progress,
                        stats.messages_seen,
                        stats.attachments_written,
                        last_offset,
                        stats.parse_errors,
                    );
                    maybe_log_parse_metrics(&mut metrics, &stats);
                    budget.acquire(item.size);
                    if tx
                        .send(IngestItem {
                            msg: item.msg,
                            size: item.size,
                            offset: item.offset,
                        })
                        .is_err()
                    {
                        return Err(AppError::Channel("ingest channel closed".into()));
                    }
                }
                ParseOutcome::Err { offset } => {
                    record_parse_error(&progress, offset, &"Boundary parse error");
                    stats.messages_seen += 1;
                    stats.parse_errors += 1;
                    last_offset = offset;
                    update_progress(
                        &progress,
                        stats.messages_seen,
                        stats.attachments_written,
                        last_offset,
                        stats.parse_errors,
                    );
                    maybe_log_parse_metrics(&mut metrics, &stats);
                    if !recover_on_error {
                        return Err(AppError::Parse {
                            offset,
                            details: "Boundary parse failed".into(),
                        });
                    }
                }
            }
            next_index += 1;
        }
    }

    let scanned = scan_handle.join().map_err(|_| AppError::Parse {
        offset: start_offset,
        details: "Boundary scanner panicked".into(),
    })??;

    for handle in handles {
        let _ = handle.join();
    }

    stats.bytes_read = last_offset;
    if (scanned as u64) > stats.messages_seen {
        stats.messages_seen = scanned as u64;
    }
    if let Some(progress) = &progress {
        if progress.skip_current_file.swap(false, Ordering::Relaxed) {
            record_skipped_file(progress, "Skipped by user");
            return Err(AppError::SkippedFile);
        }
        if progress.cancelled.load(Ordering::Relaxed) {
            return Err(AppError::Cancelled);
        }
    }

    Ok(stats)
}

fn parse_boundary_slice(
    map: &Mmap,
    boundary: MessageBoundary,
    input_dir: Option<&Path>,
    media_dir: Option<&Path>,
    thumbnail_size: u32,
    progress: Option<Arc<IngestProgress>>,
) -> Result<ParsedMessage> {
    let start = boundary.start_offset as usize;
    let mut end = boundary.end_offset as usize;
    if start >= map.len() || end >= map.len() || end < start {
        return Err(AppError::Parse {
            offset: boundary.start_offset,
            details: "Boundary out of range".into(),
        });
    }
    end = (end + 1).min(map.len());
    let slice = &map[start..end];

    let msg = parse_message_slice(slice, input_dir, media_dir, thumbnail_size, progress)?;
    let msg = finalize_message(msg);
    let size = estimate_message_size(&msg);
    let attachments = msg.attachments.len();
    Ok(ParsedMessage {
        msg,
        size,
        offset: boundary.end_offset,
        attachments,
    })
}

fn parse_message_slice(
    slice: &[u8],
    input_dir: Option<&Path>,
    media_dir: Option<&Path>,
    thumbnail_size: u32,
    progress: Option<Arc<IngestProgress>>,
) -> Result<Message> {
    let mut reader = Reader::from_reader(std::io::Cursor::new(slice));
    reader.trim_text(true);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) => {
                if is_message_tag(&e) {
                    let msg = parse_message_from_tag(&e)?;
                    return Ok(msg);
                }
            }
            Ok(Event::Start(e)) => {
                if is_message_tag(&e) {
                    let msg = if e.name().as_ref() == b"mms" {
                        parse_mms_block(
                            &mut reader,
                            &e,
                            input_dir,
                            media_dir,
                            thumbnail_size,
                            None,
                            progress.clone(),
                        )?
                    } else {
                        parse_message_from_tag(&e)?
                    };
                    return Ok(msg);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(AppError::Parse {
                    offset: 0,
                    details: e.to_string(),
                })
            }
            _ => {}
        }
        buf.clear();
    }

    Err(AppError::Parse {
        offset: 0,
        details: "No message tag in boundary slice".into(),
    })
}

fn detect_encoding(file: &mut File) -> Result<()> {
    let mut bom = [0u8; 4];
    let read = file.read(&mut bom)?;
    if read >= 2 && bom[0] == 0xFF && bom[1] == 0xFE {
        return Err(AppError::UnsupportedEncoding("UTF-16LE".into()));
    }
    if read >= 2 && bom[0] == 0xFE && bom[1] == 0xFF {
        return Err(AppError::UnsupportedEncoding("UTF-16BE".into()));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn is_message_tag(e: &BytesStart) -> bool {
    matches!(e.name().as_ref(), b"sms" | b"mms")
}

fn parse_message_from_tag(e: &BytesStart) -> Result<Message> {
    let mut msg = Message {
        id: Uuid::new_v4(),
        message_id: None,
        dedupe_hash: None,
        timestamp: 0,
        address: String::new(),
        body: String::new(),
        body_searchable: String::new(),
        message_type: if e.name().as_ref() == b"mms" {
            MessageType::Mms
        } else {
            MessageType::Sms
        },
        direction: MessageDirection::Unknown,
        thread_id: None,
        attachments: Vec::<AttachmentRef>::new(),
        contact_name: None,
    };

    for attr in e.attributes().with_checks(false) {
        let attr = attr.map_err(|e| AppError::Parse {
            offset: 0,
            details: e.to_string(),
        })?;
        let key = attr.key.as_ref();
        let value = attr
            .unescape_value()
            .map_err(|e| AppError::Parse {
                offset: 0,
                details: e.to_string(),
            })?
            .to_string();

        match key {
            b"address" => msg.address = normalize_address(&value),
            b"date" => msg.timestamp = value.parse().unwrap_or(0),
            b"body" => {
                msg.body = value.nfc().collect();
                msg.body_searchable = value.nfkd().collect();
            }
            b"thread_id" => {
                if value != "null" && !value.is_empty() {
                    msg.thread_id = Some(value);
                }
            }
            b"msg_id" | b"m_id" | b"id" => {
                if value != "null" && !value.is_empty() {
                    msg.message_id = Some(value);
                }
            }
            b"type" | b"msg_box" => {
                msg.direction = parse_message_direction(&value);
            }
            b"contact_name" => {
                // SMS Backup & Restore writes the address-book label as `contact_name`.
                // Treat the literal string "(Unknown)" and the JSON-y "null" as missing.
                let trimmed = value.trim();
                if !trimmed.is_empty() && trimmed != "null" && trimmed != "(Unknown)" {
                    msg.contact_name = Some(trimmed.to_string());
                }
            }
            _ => {}
        }
    }

    if msg.body_searchable.is_empty() {
        msg.body_searchable = msg.body.clone();
    }

    Ok(msg)
}

fn parse_message_direction(value: &str) -> MessageDirection {
    let parsed = value.trim().parse::<i32>().unwrap_or(0);
    // #todo: map additional SMS/MMS direction codes (drafts, failed, queued) into richer states.
    MessageDirection::from_i32(parsed)
}

fn parse_mms_block<R: std::io::BufRead>(
    reader: &mut Reader<R>,
    start: &BytesStart,
    input_dir: Option<&Path>,
    media_dir: Option<&Path>,
    thumbnail_size: u32,
    thumb_queue: Option<sms_media::ThumbnailQueue>,
    progress: Option<Arc<IngestProgress>>,
) -> Result<Message> {
    let mut msg = parse_message_from_tag(start)?;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) => {
                if e.name().as_ref() == b"part" {
                    if let Some(part) = parse_part(&e)? {
                        handle_part(
                            &mut msg,
                            part,
                            input_dir,
                            media_dir,
                            thumbnail_size,
                            thumb_queue.clone(),
                            progress.clone(),
                        )?;
                    }
                }
            }
            Ok(Event::Start(e)) => {
                if e.name().as_ref() == b"part" {
                    let parsed = parse_part(&e)?;
                    // Consume until </part>
                    consume_until_end(reader, b"part")?;
                    if let Some(part) = parsed {
                        handle_part(
                            &mut msg,
                            part,
                            input_dir,
                            media_dir,
                            thumbnail_size,
                            thumb_queue.clone(),
                            progress.clone(),
                        )?;
                    }
                } else if e.name().as_ref() == b"mms" {
                    // Nested mms not expected
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"mms" {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(AppError::Parse {
                    offset: reader.buffer_position() as u64,
                    details: e.to_string(),
                })
            }
            _ => {}
        }
        buf.clear();
    }

    Ok(msg)
}

fn consume_until_end<R: std::io::BufRead>(reader: &mut Reader<R>, name: &[u8]) -> Result<()> {
    let mut depth = 1usize;
    let mut buf = Vec::new();
    while depth > 0 {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if e.name().as_ref() == name {
                    depth += 1;
                }
            }
            Ok(Event::End(e)) => {
                if e.name().as_ref() == name {
                    depth = depth.saturating_sub(1);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(AppError::Parse {
                    offset: reader.buffer_position() as u64,
                    details: e.to_string(),
                })
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

struct PartData {
    mime_type: String,
    name: Option<String>,
    data: Option<String>,
    text: Option<String>,
}

fn parse_part(e: &BytesStart) -> Result<Option<PartData>> {
    let mut mime: Option<String> = None;
    let mut name: Option<String> = None;
    let mut data: Option<String> = None;
    let mut text: Option<String> = None;

    for attr in e.attributes().with_checks(false) {
        let attr = attr.map_err(|err| AppError::Parse {
            offset: 0,
            details: err.to_string(),
        })?;
        let key = attr.key.as_ref();
        let value = attr
            .unescape_value()
            .map_err(|err| AppError::Parse {
                offset: 0,
                details: err.to_string(),
            })?
            .to_string();

        match key {
            b"ct" | b"content-type" => mime = Some(value),
            b"name" | b"filename" | b"cl" => name = Some(value),
            b"data" => data = Some(value),
            b"text" => text = Some(value),
            _ => {}
        }
    }

    let mime_type = mime.unwrap_or_else(|| "application/octet-stream".to_string());
    Ok(Some(PartData {
        mime_type,
        name,
        data,
        text,
    }))
}

fn handle_part(
    msg: &mut Message,
    part: PartData,
    input_dir: Option<&Path>,
    media_dir: Option<&Path>,
    thumbnail_size: u32,
    thumb_queue: Option<sms_media::ThumbnailQueue>,
    progress: Option<Arc<IngestProgress>>,
) -> Result<()> {
    let mime = part.mime_type.as_str();
    // Skip SMIL markup entirely – it's layout XML, not user-visible text
    if mime == "application/smil" {
        return Ok(());
    }
    if mime.starts_with("text/") {
        if msg.body.is_empty() {
            let body = part.text.or(part.data);
            if let Some(text) = body {
                msg.body = text.nfc().collect();
                msg.body_searchable = text.nfkd().collect();
            }
        }
        return Ok(());
    }

    if let Some(att) = materialize_attachment(
        part,
        input_dir,
        media_dir,
        thumbnail_size,
        thumb_queue,
        progress,
    )? {
        msg.attachments.push(att);
    }

    Ok(())
}

fn materialize_attachment(
    part: PartData,
    input_dir: Option<&Path>,
    media_dir: Option<&Path>,
    thumbnail_size: u32,
    thumb_queue: Option<sms_media::ThumbnailQueue>,
    progress: Option<Arc<IngestProgress>>,
) -> Result<Option<AttachmentRef>> {
    let media_dir = match media_dir {
        Some(dir) => dir,
        None => return Ok(None),
    };

    fs::create_dir_all(media_dir)?;

    let data = part.data.clone();
    let name = part.name.clone();
    let mime_type = part.mime_type.clone();

    let mut skip_reason: Option<String> = None;
    let bytes_opt = if let Some(d) = data.clone() {
        if looks_like_base64(&d) {
            match general_purpose::STANDARD.decode(d.as_bytes()) {
                Ok(bytes) => Some(bytes),
                Err(_) => {
                    skip_reason = Some("attachment: invalid base64 payload".to_string());
                    None
                }
            }
        } else if let Some(path) = resolve_path(&d, input_dir) {
            match fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(err) => {
                    skip_reason = Some(format!(
                        "attachment: failed to read {} ({})",
                        path.display(),
                        err
                    ));
                    None
                }
            }
        } else {
            skip_reason = Some(format!("attachment: path not found for '{}'", d));
            None
        }
    } else if let Some(n) = name.clone() {
        if let Some(path) = resolve_path(&n, input_dir) {
            match fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(err) => {
                    skip_reason = Some(format!(
                        "attachment: failed to read {} ({})",
                        path.display(),
                        err
                    ));
                    None
                }
            }
        } else {
            skip_reason = Some(format!("attachment: path not found for '{}'", n));
            None
        }
    } else {
        skip_reason = Some("attachment: missing data and name".to_string());
        None
    };

    let bytes = match bytes_opt {
        Some(b) => b,
        None => {
            if let Some(reason) = skip_reason {
                record_skipped_attachment(&progress, &reason);
            }
            return Ok(None);
        }
    };

    let hash = blake3::hash(&bytes);
    let ext = extension_for_mime(&mime_type)
        .map(|s| s.to_string())
        .or_else(|| name.as_deref().and_then(extension_from_name));
    let subdir = format!("{:02x}", hash.as_bytes()[0]);
    let out_dir = media_dir.join(&subdir);
    fs::create_dir_all(&out_dir)?;

    let filename = match ext {
        Some(e) => format!("{}{}", hex_hash(&hash), e),
        None => hex_hash(&hash),
    };
    let out_path = out_dir.join(&filename);
    if !out_path.exists() {
        fs::write(&out_path, &bytes)?;
    }

    let rel_path = out_path
        .strip_prefix(media_dir)
        .unwrap_or(&out_path)
        .to_string_lossy()
        .replace('\\', "/");

    let mut thumb_rel: Option<String> = None;
    if is_thumbnailable_mime(&mime_type) {
        let thumb_dir = media_dir.join("thumbnails").join(&subdir);
        fs::create_dir_all(&thumb_dir)?;
        let thumb_path = thumb_dir.join(format!("{}.jpg", hex_hash(&hash)));
        if !thumb_path.exists() {
            if let Some(queue) = &thumb_queue {
                if !queue.try_enqueue(
                    out_path.clone(),
                    thumb_path.clone(),
                    thumbnail_size,
                    mime_type.clone(),
                ) {
                    let _ = sms_media::generate_thumbnail_for_mime(
                        &out_path,
                        &thumb_path,
                        thumbnail_size,
                        &mime_type,
                    );
                }
            } else {
                let _ = sms_media::generate_thumbnail_for_mime(
                    &out_path,
                    &thumb_path,
                    thumbnail_size,
                    &mime_type,
                );
            }
        }
        let rel = thumb_path
            .strip_prefix(media_dir)
            .unwrap_or(&thumb_path)
            .to_string_lossy()
            .replace('\\', "/");
        thumb_rel = Some(rel);
    }

    Ok(Some(AttachmentRef {
        id: Uuid::new_v4(),
        mime_type,
        file_path: rel_path,
        file_hash: *hash.as_bytes(),
        thumbnail_path: thumb_rel,
    }))
}

fn resolve_path(raw: &str, input_dir: Option<&Path>) -> Option<PathBuf> {
    if raw.starts_with("file://") {
        let trimmed = raw.trim_start_matches("file://");
        let p = PathBuf::from(trimmed);
        if p.exists() {
            return Some(p);
        }
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() && p.exists() {
        return Some(p);
    }
    if let Some(dir) = input_dir {
        let joined = dir.join(raw);
        if joined.exists() {
            return Some(joined);
        }
    }
    None
}

fn looks_like_base64(s: &str) -> bool {
    if s.len() < 4 {
        return false;
    }
    s.bytes().all(|b| {
        b.is_ascii_uppercase()
            || b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || b == b'+'
            || b == b'/'
            || b == b'='
    })
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/jpeg" => Some(".jpg"),
        "image/png" => Some(".png"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        "image/heic" | "image/heif" => Some(".heic"),
        "video/mp4" => Some(".mp4"),
        "video/quicktime" => Some(".mov"),
        "audio/mpeg" => Some(".mp3"),
        _ => None,
    }
}

fn extension_from_name(name: &str) -> Option<String> {
    let ext = name.rsplit_once('.').map(|(_, ext)| ext)?;
    if ext.is_empty() {
        None
    } else {
        Some(format!(".{}", ext))
    }
}

fn is_image_mime(mime: &str) -> bool {
    mime.starts_with("image/")
}

fn is_heic_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/heic" | "image/heif" | "image/heic-sequence" | "image/heif-sequence"
    )
}

fn is_video_mime(mime: &str) -> bool {
    mime.starts_with("video/")
}

fn is_thumbnailable_mime(mime: &str) -> bool {
    is_image_mime(mime) || is_video_mime(mime) || is_heic_mime(mime)
}

fn hex_hash(hash: &blake3::Hash) -> String {
    let mut out = String::with_capacity(64);
    for b in hash.as_bytes() {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn finalize_message(mut msg: Message) -> Message {
    if msg.message_id.is_none() {
        let mut hasher = Hasher::new();
        hasher.update(msg.address.as_bytes());
        hasher.update(&msg.timestamp.to_le_bytes());
        hasher.update(msg.body.as_bytes());
        if let Some(thread_id) = &msg.thread_id {
            hasher.update(thread_id.as_bytes());
        }
        let att_len = msg.attachments.len() as u64;
        hasher.update(&att_len.to_le_bytes());
        for att in &msg.attachments {
            hasher.update(&att.file_hash);
        }
        let hash = hasher.finalize();
        msg.dedupe_hash = Some(*hash.as_bytes());
    }
    msg
}

/// Normalize a phone-like address for thread/dedupe matching.
///
/// Assumption: every `+1XXXXXXXXXX` (12 chars after stripping) is a NANP
/// (US/Canada) number, so the `+1` is dropped to make SMS addresses
/// ("2147172243") and MMS addresses ("+12147172243") resolve to the same
/// thread. Non-NANP numbers are left untouched (they keep their `+` and
/// country code), but a hypothetical non-NANP number that happens to match
/// the `+1` + 10-digit shape would be mis-normalized — accepted trade-off
/// for a US-centric archive.
fn normalize_address(raw: &str) -> String {
    // Strip everything except digits and leading '+'
    let digits_only: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect();
    if digits_only.starts_with("+1") && digits_only.len() == 12 {
        digits_only[2..].to_string()
    } else {
        digits_only
    }
}

fn estimate_message_size(msg: &Message) -> usize {
    let mut size = 64;
    size += msg.address.len();
    size += msg.body.len();
    size += msg.body_searchable.len();
    if msg.thread_id.is_some() {
        size += 32;
    }
    size
}

fn checkpoint_path_for(db_path: &Path) -> PathBuf {
    let mut p = db_path.to_path_buf();
    p.set_extension("checkpoint.json");
    p
}

fn save_checkpoint(path: &Path, checkpoint: &Checkpoint) -> Result<()> {
    let tmp = path.with_extension("checkpoint.tmp");
    let mut file = File::create(&tmp)?;
    serde_json::to_writer_pretty(&mut file, checkpoint)
        .map_err(|e| AppError::Serde(e.to_string()))?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn load_checkpoint(path: &Path) -> Option<Checkpoint> {
    let data = std::fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Memchr-based boundary scan: finds <sms ... /> self-closing tags.
pub fn scan_boundaries(path: &Path) -> Result<Vec<MessageBoundary>> {
    let file = File::open(path)?;
    // SAFETY: mapping a file that another process truncates or rewrites while
    // we read is undefined behavior (memmap2's documented caveat). We accept
    // that risk for read-only ingest of a user-selected export file; callers
    // must not import a file that is still being written.
    let map = unsafe { Mmap::map(&file)? };
    let bytes = &map[..];

    let mut boundaries = Vec::new();
    let mut i = 0;
    while let Some(pos) = memchr_iter(b'<', &bytes[i..]).next() {
        let start = i + pos;
        if bytes.get(start..start + 4) == Some(b"<sms") {
            if let Some(end_rel) = find_self_closing(&bytes[start..]) {
                let end = start + end_rel;
                boundaries.push(MessageBoundary {
                    start_offset: start as u64,
                    end_offset: end as u64,
                });
                i = end + 1;
                continue;
            }
        }
        i = start + 1;
    }

    Ok(boundaries)
}

/// Naive boundary scan: simple byte window search for "<sms".
pub fn scan_boundaries_naive(path: &Path) -> Result<Vec<MessageBoundary>> {
    let file = File::open(path)?;
    // SAFETY: mapping a file that another process truncates or rewrites while
    // we read is undefined behavior (memmap2's documented caveat). We accept
    // that risk for read-only ingest of a user-selected export file; callers
    // must not import a file that is still being written.
    let map = unsafe { Mmap::map(&file)? };
    let bytes = &map[..];

    let mut boundaries = Vec::new();
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"<sms" {
            if let Some(end_rel) = find_self_closing(&bytes[i..]) {
                let end = i + end_rel;
                boundaries.push(MessageBoundary {
                    start_offset: i as u64,
                    end_offset: end as u64,
                });
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    Ok(boundaries)
}

/// Find the end of a self-closing tag (`/>`), skipping any `>` inside quoted
/// attribute values — a literal `>` is legal, unescaped, inside XML attribute
/// quotes (e.g. `body="5 > 3"`), unlike `<`, which must always be escaped.
fn find_self_closing(buf: &[u8]) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut prev: u8 = 0;
    for (at, &b) in buf.iter().enumerate() {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' if prev == b'/' => return Some(at),
                _ => {}
            },
        }
        prev = b;
    }
    None
}

/// Boundary scan that handles <sms> and <mms> start/end tags.
pub fn scan_boundaries_full(path: &Path) -> Result<Vec<MessageBoundary>> {
    let file = File::open(path)?;
    // SAFETY: mapping a file that another process truncates or rewrites while
    // we read is undefined behavior (memmap2's documented caveat). We accept
    // that risk for read-only ingest of a user-selected export file; callers
    // must not import a file that is still being written.
    let map = unsafe { Mmap::map(&file)? };
    let bytes = &map[..];

    let mut boundaries = Vec::new();
    let mut i = 0usize;
    while let Some(pos) = memchr_iter(b'<', &bytes[i..]).next() {
        let start = i + pos;
        let tag = if bytes.get(start..start + 4) == Some(b"<sms") {
            b"sms".as_slice()
        } else if bytes.get(start..start + 4) == Some(b"<mms") {
            b"mms".as_slice()
        } else {
            i = start + 1;
            continue;
        };

        if !is_tag_boundary(bytes, start, tag) {
            i = start + 1;
            continue;
        }

        let (start_end, self_closing) = match find_start_tag_end(bytes, start) {
            Some(v) => v,
            None => {
                i = start + 1;
                continue;
            }
        };

        let end = if self_closing {
            start_end
        } else if let Some(close_end) = find_end_tag(bytes, start_end + 1, tag) {
            close_end
        } else {
            i = start + 1;
            continue;
        };

        boundaries.push(MessageBoundary {
            start_offset: start as u64,
            end_offset: end as u64,
        });

        i = end + 1;
    }

    Ok(boundaries)
}

fn scan_boundaries_streaming(
    map: &Mmap,
    start_offset: u64,
    tx: Sender<(usize, MessageBoundary)>,
    progress: Option<&Arc<IngestProgress>>,
) -> Result<usize> {
    let bytes = &map[..];
    if bytes.is_empty() {
        return Ok(0);
    }

    let mut i = start_offset.min(bytes.len() as u64) as usize;
    let mut count = 0usize;
    while let Some(pos) = memchr_iter(b'<', &bytes[i..]).next() {
        if let Some(progress) = progress {
            if progress.cancelled.load(Ordering::Relaxed) {
                break;
            }
        }
        let start = i + pos;
        let tag = if bytes.get(start..start + 4) == Some(b"<sms") {
            b"sms".as_slice()
        } else if bytes.get(start..start + 4) == Some(b"<mms") {
            b"mms".as_slice()
        } else {
            i = start + 1;
            continue;
        };

        if !is_tag_boundary(bytes, start, tag) {
            i = start + 1;
            continue;
        }

        let (start_end, self_closing) = match find_start_tag_end(bytes, start) {
            Some(v) => v,
            None => {
                i = start + 1;
                continue;
            }
        };

        let end = if self_closing {
            start_end
        } else if let Some(close_end) = find_end_tag(bytes, start_end + 1, tag) {
            close_end
        } else {
            i = start + 1;
            continue;
        };

        if (end as u64) <= start_offset {
            i = end + 1;
            continue;
        }

        if tx
            .send((
                count,
                MessageBoundary {
                    start_offset: start as u64,
                    end_offset: end as u64,
                },
            ))
            .is_err()
        {
            break;
        }
        count += 1;
        i = end + 1;
    }

    Ok(count)
}

fn is_tag_boundary(buf: &[u8], start: usize, tag: &[u8]) -> bool {
    let after = buf.get(start + 1 + tag.len());
    matches!(
        after,
        Some(b'>') | Some(b'/') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
    )
}

/// Walk the start tag beginning at `start` (the `<`) to its closing `>`,
/// skipping any `>` inside quoted attribute values — a literal `>` is legal,
/// unescaped, inside XML attribute quotes (e.g. `body="5 > 3"`), unlike `<`,
/// which must always be escaped. Returns (index of `>`, self_closing).
fn find_start_tag_end(buf: &[u8], start: usize) -> Option<(usize, bool)> {
    let mut quote: Option<u8> = None;
    for (off, &b) in buf[start..].iter().enumerate() {
        let at = start + off;
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'>' => return Some((at, at > start && buf[at - 1] == b'/')),
                _ => {}
            },
        }
    }
    None
}

fn find_end_tag(buf: &[u8], start: usize, tag: &[u8]) -> Option<usize> {
    let mut i = start;
    while let Some(pos) = memchr_iter(b'<', &buf[i..]).next() {
        let at = i + pos;
        if buf.get(at + 1) == Some(&b'/') && buf.get(at + 2..at + 2 + tag.len()) == Some(tag) {
            let next = buf.get(at + 2 + tag.len());
            if matches!(
                next,
                Some(b'>') | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r')
            ) {
                if let Some(end) = memchr_iter(b'>', &buf[at..]).next() {
                    return Some(at + end);
                }
            }
        }
        i = at + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use quick_xml::Reader;
    use std::io::Write;

    #[test]
    fn scans_self_closing_sms_tags() {
        let data = b"<smses><sms address=\"+1\" date=\"1\" body=\"hi\" /><sms address=\"+2\" date=\"2\" body=\"yo\" /></smses>";
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(data).unwrap();

        let boundaries = scan_boundaries(file.path()).unwrap();
        assert_eq!(boundaries.len(), 2);
    }

    #[test]
    fn scans_sms_and_mms_tags() {
        let data = b"<smses><sms address=\"+1\" date=\"1\" body=\"hi\" /><mms address=\"+2\" date=\"2\"><part ct=\"text/plain\" text=\"yo\" /></mms></smses>";
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(data).unwrap();

        let boundaries = scan_boundaries_full(file.path()).unwrap();
        assert_eq!(boundaries.len(), 2);
    }

    #[test]
    fn boundary_scan_survives_gt_inside_attribute_value() {
        // A literal '>' is legal, unescaped, inside a quoted attribute value.
        // The scanner must not treat it as the end of the start tag, and the
        // following message must still be found intact.
        let data = br#"<smses><sms address="+1" date="1" body="5 > 3, still true" /><sms address="+2" date="2" body="yo" /></smses>"#;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(data).unwrap();

        let boundaries = scan_boundaries_full(file.path()).unwrap();
        assert_eq!(boundaries.len(), 2);

        // End-to-end through the production chunk parser (quick_xml).
        let chunk = &data[boundaries[0].start_offset as usize..=boundaries[0].end_offset as usize];
        let msg = parse_message_slice(chunk, None, None, 256, None).unwrap();
        assert_eq!(msg.body, "5 > 3, still true");
    }

    #[test]
    fn boundary_scan_survives_gt_in_single_quoted_attribute() {
        // Single-quoted attribute containing both double quotes and '>'.
        let data = b"<smses><sms address='+1' date='1' body='he said \"5 > 3\"' /><sms address='+2' date='2' body='ok' /></smses>";
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(data).unwrap();

        let boundaries = scan_boundaries_full(file.path()).unwrap();
        assert_eq!(boundaries.len(), 2);
    }

    #[test]
    fn boundary_scan_survives_gt_in_mms_part_attribute() {
        let data = br#"<smses><mms address="+1" date="1"><part ct="text/plain" text="a > b /> c" /></mms><sms address="+2" date="2" body="ok" /></smses>"#;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(data).unwrap();

        let boundaries = scan_boundaries_full(file.path()).unwrap();
        assert_eq!(boundaries.len(), 2);
    }

    #[test]
    fn normalize_address_collapses_nanp_prefix_only() {
        // NANP +1 prefix folds into the bare 10-digit form...
        assert_eq!(normalize_address("+1 (214) 717-2243"), "2147172243");
        assert_eq!(normalize_address("2147172243"), "2147172243");
        // ...while non-NANP international numbers keep country codes.
        assert_eq!(normalize_address("+44 20 7946 0958"), "+442079460958");
        assert_eq!(normalize_address("+12345"), "+12345");
    }

    #[test]
    fn find_self_closing_skips_quoted_gt() {
        let buf = br#"<sms body="a /> b" />"#;
        assert_eq!(find_self_closing(buf), Some(buf.len() - 1));
    }

    #[test]
    fn parses_message_chunk() {
        let chunk = b"<sms address=\"+1555\" date=\"123\" body=\"Hello\" />";
        let msg = parse_message_chunk(chunk).unwrap();
        assert_eq!(msg.address, "+1555");
        assert_eq!(msg.timestamp, 123);
        assert_eq!(msg.body, "Hello");
    }

    #[test]
    fn parses_mms_text_part() {
        let xml = r#"<mms address="+1" date="1"><part ct="text/plain" text="hi there" /></mms>"#;
        let mut reader = Reader::from_str(xml);
        reader.trim_text(true);
        let mut buf = Vec::new();
        let mut parsed: Option<Message> = None;
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) if e.name().as_ref() == b"mms" => {
                    parsed = Some(
                        parse_mms_block(&mut reader, &e, None, None, 256, None, None).unwrap(),
                    );
                    break;
                }
                Ok(Event::Eof) => break,
                _ => {}
            }
            buf.clear();
        }
        let msg = parsed.unwrap();
        assert_eq!(msg.body, "hi there");
    }

    #[test]
    fn parses_mms_attachment_part() {
        let xml = r#"<mms address="+1" date="1"><part ct="image/jpeg" name="pic.jpg" data="YWJj" /></mms>"#;
        let mut reader = Reader::from_str(xml);
        reader.trim_text(true);
        let mut buf = Vec::new();
        let mut parsed: Option<Message> = None;
        let media_dir = tempfile::TempDir::new().unwrap();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) if e.name().as_ref() == b"mms" => {
                    parsed = Some(
                        parse_mms_block(
                            &mut reader,
                            &e,
                            None,
                            Some(media_dir.path()),
                            64,
                            None,
                            None,
                        )
                        .unwrap(),
                    );
                    break;
                }
                Ok(Event::Eof) => break,
                _ => {}
            }
            buf.clear();
        }
        let msg = parsed.unwrap();
        assert_eq!(msg.attachments.len(), 1);
        assert_eq!(msg.attachments[0].mime_type, "image/jpeg");
    }

    // Legacy whitespace-splitting chunk parser, kept only for simple test
    // fixtures. It cannot handle attribute values containing whitespace or
    // '>' — production code parses chunks with quick_xml (parse_message_slice).
    fn parse_message_chunk(chunk: &[u8]) -> Result<Message> {
        let text = std::str::from_utf8(chunk).map_err(|e| AppError::Parse {
            offset: 0,
            details: e.to_string(),
        })?;

        let mut msg = Message {
            id: Uuid::new_v4(),
            message_id: None,
            dedupe_hash: None,
            timestamp: 0,
            address: String::new(),
            body: String::new(),
            body_searchable: String::new(),
            message_type: MessageType::Sms,
            direction: MessageDirection::Unknown,
            thread_id: None,
            attachments: Vec::new(),
            contact_name: None,
        };

        for part in text.split_whitespace() {
            if let Some(rest) = part.strip_prefix("address=") {
                msg.address = trim_quotes(rest).to_string();
            } else if let Some(rest) = part.strip_prefix("date=") {
                msg.timestamp = trim_quotes(rest).parse().unwrap_or(0);
            } else if let Some(rest) = part.strip_prefix("body=") {
                let raw = trim_quotes(rest);
                msg.body = raw.nfc().collect();
                msg.body_searchable = raw.nfkd().collect();
            }
        }

        Ok(msg)
    }

    fn trim_quotes(s: &str) -> &str {
        s.trim_matches('"').trim_matches('\'')
    }
}
