//! Mission statement: Provide a high-throughput CLIP media processing pipeline that batches
//! attachment frames, generates embeddings and NSFW scores, and writes results into the SMS
//! Archive database for fast search and moderation workflows.

use image::DynamicImage;
use indicatif::{ProgressBar, ProgressStyle};
use rusqlite::OptionalExtension;
use sms_clip::{ClipEncoder, ClipResult};
use sms_db::{self, Database};
use sms_errors::{AppError, Result};
use sms_media::keyframes::extract_keyframes;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct MediaProcessOptions {
    pub db_path: PathBuf,
    pub clip_model: PathBuf,
    pub nsfw_weights: PathBuf,
    pub batch_size: usize,
    pub max_keyframes: usize,
    pub reprocess: bool,
    pub limit: Option<usize>,
    pub workers: usize,
    pub show_progress: bool,
    pub media_root: Option<PathBuf>,
    pub cancel_flag: Option<Arc<AtomicBool>>,
    pub pause_flag: Option<Arc<AtomicBool>>,
    pub progress_total: Option<Arc<AtomicU64>>,
    pub progress_done: Option<Arc<AtomicU64>>,
}

#[derive(Debug, Clone)]
pub struct MediaProcessStats {
    pub total_tasks: usize,
    pub processed_tasks: usize,
    pub embedded_frames: usize,
    pub nsfw_updated: usize,
    pub elapsed_ms: u128,
}

// #todo: report per-frame throughput metrics for richer CLIP progress telemetry.

pub fn process_media(options: &MediaProcessOptions) -> Result<MediaProcessStats> {
    let db = Database::open(&options.db_path, sms_config::ResourceProfile::detect())?;
    let conn = db.connection();

    let model_meta = sms_db::ModelMeta {
        dims: Some(768),
        max_length: None,
        normalize: Some(true),
        tokenizer_path: None,
        input_ids_name: None,
        attention_mask_name: None,
        token_type_ids_name: None,
        output_name: None,
    };
    // #todo: derive CLIP model name + input size from metadata instead of hardcoding.
    let model_id = sms_db::upsert_ml_model_with_meta(
        conn,
        "clip-vit-l-14-224",
        "clip-media",
        None,
        &model_meta,
    )?;

    let tasks = sms_db::get_unprocessed_media(conn, options.limit, options.reprocess)?;
    let total_tasks = tasks.len();
    if let Some(total) = &options.progress_total {
        total.store(total_tasks as u64, Ordering::Relaxed);
    }
    if let Some(done) = &options.progress_done {
        done.store(0, Ordering::Relaxed);
    }
    if total_tasks == 0 {
        return Ok(MediaProcessStats {
            total_tasks: 0,
            processed_tasks: 0,
            embedded_frames: 0,
            nsfw_updated: 0,
            elapsed_ms: 0,
        });
    }

    let media_root = resolve_media_root(&options.db_path, options.media_root.clone());

    let progress = if options.show_progress {
        let bar = ProgressBar::new(total_tasks as u64);
        bar.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} {pos}/{len} | {wide_msg} | {elapsed_precise}",
            )
            .unwrap(),
        );
        Some(bar)
    } else {
        None
    };

    let mut encoder = ClipEncoder::new(&options.clip_model, &options.nsfw_weights)?;

    let start = Instant::now();
    let mut processed_tasks = 0usize;
    let mut embedded_frames = 0usize;
    let mut nsfw_updated = 0usize;

    for chunk in tasks.chunks(options.batch_size.max(1)) {
        wait_if_paused(&options.pause_flag, &options.cancel_flag)?;
        if is_cancelled(&options.cancel_flag) {
            return Err(AppError::Media("Cancelled".to_string()));
        }
        let frames = match load_frames(
            chunk,
            media_root.as_ref(),
            options.max_keyframes,
            options.workers,
            &options.pause_flag,
            &options.cancel_flag,
        ) {
            Ok(f) => f,
            Err(AppError::Media(msg)) if msg == "Cancelled" => {
                return Err(AppError::Media(msg));
            }
            Err(_) => {
                // Skip batch on non-fatal errors (corrupt files, etc.)
                processed_tasks += chunk.len();
                if let Some(done) = &options.progress_done {
                    done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
                if let Some(bar) = &progress {
                    bar.inc(chunk.len() as u64);
                }
                continue;
            }
        };
        if frames.images.is_empty() {
            processed_tasks += chunk.len();
            if let Some(done) = &options.progress_done {
                done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            if let Some(bar) = &progress {
                bar.inc(chunk.len() as u64);
            }
            continue;
        }

        let results = encoder.encode_batch(&frames.images)?;
        let (frame_rows, nsfw_rows) = build_db_rows(&frames.map, &results)?;

        embedded_frames += frame_rows.len();
        nsfw_updated += nsfw_rows.len();
        sms_db::insert_media_results_batch(conn, &frame_rows, &nsfw_rows, &model_id)?;

        processed_tasks += chunk.len();
        if let Some(done) = &options.progress_done {
            done.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        if let Some(bar) = &progress {
            bar.inc(chunk.len() as u64);
        }
    }

    if let Some(bar) = &progress {
        bar.finish_and_clear();
    }

    Ok(MediaProcessStats {
        total_tasks,
        processed_tasks,
        embedded_frames,
        nsfw_updated,
        elapsed_ms: start.elapsed().as_millis(),
    })
}

struct FrameBatch {
    images: Vec<DynamicImage>,
    map: Vec<FrameMeta>,
}

#[derive(Debug, Clone)]
struct FrameMeta {
    attachment_id: String,
    frame_index: i64,
    frame_time_ms: Option<i64>,
}

fn load_frames(
    tasks: &[sms_db::MediaTask],
    media_root: Option<&PathBuf>,
    max_keyframes: usize,
    workers: usize,
    pause_flag: &Option<Arc<AtomicBool>>,
    cancel_flag: &Option<Arc<AtomicBool>>,
) -> Result<FrameBatch> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers.max(1))
        .build()
        .map_err(|err| AppError::Media(err.to_string()))?;

    // Decode tasks in parallel across the pool (previously a sequential loop
    // inside pool.install(), which used exactly one thread no matter what
    // `workers` was set to), then flatten in task order so frame mapping
    // stays deterministic. Failures are logged instead of silently skipped.
    let per_task: Vec<std::result::Result<Vec<(DynamicImage, FrameMeta)>, AppError>> = pool
        .install(|| {
            use rayon::prelude::*;
            tasks
                .par_iter()
                .map(|task| {
                    wait_if_paused(pause_flag, cancel_flag)?;
                    if is_cancelled(cancel_flag) {
                        return Err(AppError::Media("Cancelled".to_string()));
                    }
                    let media_path = match resolve_media_path(task, media_root) {
                        Ok(p) => p,
                        Err(err) => {
                            tracing::warn!(
                                attachment_id = %task.attachment_id,
                                %err,
                                "media processing: skipping unresolvable file path"
                            );
                            return Ok(Vec::new());
                        }
                    };
                    let (frames, temp_dir) =
                        match extract_keyframes(&media_path, &task.mime_type, max_keyframes) {
                            Ok(result) => result,
                            Err(err) => {
                                tracing::warn!(
                                    path = %media_path.display(),
                                    %err,
                                    "media processing: keyframe extraction failed, skipping"
                                );
                                return Ok(Vec::new());
                            }
                        };
                    let mut out = Vec::new();
                    for frame in frames {
                        match image::open(&frame.path) {
                            Ok(image) => out.push((
                                image,
                                FrameMeta {
                                    attachment_id: task.attachment_id.clone(),
                                    frame_index: frame.index as i64,
                                    frame_time_ms: frame.time_ms,
                                },
                            )),
                            Err(err) => {
                                tracing::warn!(
                                    path = %frame.path.display(),
                                    %err,
                                    "media processing: frame decode failed, skipping \
                                     (HEIC needs the sms-media `heic` feature)"
                                );
                            }
                        }
                    }
                    sms_media::keyframes::cleanup_temp_dir(temp_dir);
                    Ok(out)
                })
                .collect()
        });

    let mut images = Vec::new();
    let mut map = Vec::new();
    for result in per_task {
        for (image, meta) in result? {
            images.push(image);
            map.push(meta);
        }
    }
    Ok(FrameBatch { images, map })
}

fn is_cancelled(flag: &Option<Arc<AtomicBool>>) -> bool {
    flag.as_ref()
        .map(|v| v.load(Ordering::Relaxed))
        .unwrap_or(false)
}

fn wait_if_paused(
    pause_flag: &Option<Arc<AtomicBool>>,
    cancel_flag: &Option<Arc<AtomicBool>>,
) -> Result<()> {
    loop {
        let paused = pause_flag
            .as_ref()
            .map(|v| v.load(Ordering::Relaxed))
            .unwrap_or(false);
        if !paused {
            return Ok(());
        }
        if is_cancelled(cancel_flag) {
            return Err(AppError::Media("Cancelled".to_string()));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn resolve_media_path(task: &sms_db::MediaTask, media_root: Option<&PathBuf>) -> Result<PathBuf> {
    if let Some(root) = media_root {
        let path = root.join(&task.file_path);
        if path.exists() {
            return Ok(path);
        }
    }
    Err(AppError::Media(format!(
        "Missing media file: {}",
        task.file_path
    )))
}

fn resolve_media_root(db_path: &Path, override_root: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(root) = override_root {
        return Some(root);
    }
    let db = Database::open(db_path, sms_config::ResourceProfile::detect()).ok()?;
    let conn = db.connection();
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'media_dir'",
            [],
            |row| row.get(0),
        )
        .optional()
        .ok()?
        .flatten();
    if let Some(value) = stored {
        return Some(PathBuf::from(value));
    }
    db_path.parent().map(|p| p.join("media"))
}

fn build_db_rows(
    metas: &[FrameMeta],
    results: &[ClipResult],
) -> Result<(Vec<sms_db::MediaEmbeddingRow>, Vec<sms_db::MediaNsfwRow>)> {
    if metas.len() != results.len() {
        return Err(AppError::Media("Frame/result count mismatch".to_string()));
    }
    let mut embeds = Vec::with_capacity(results.len());
    let mut nsfw_map: HashMap<String, ClipResult> = HashMap::new();

    for (meta, result) in metas.iter().zip(results.iter()) {
        embeds.push(sms_db::MediaEmbeddingRow {
            attachment_id: meta.attachment_id.clone(),
            frame_index: meta.frame_index,
            frame_time_ms: meta.frame_time_ms,
            embedding: result.embedding.clone(),
        });
        let entry = nsfw_map
            .entry(meta.attachment_id.clone())
            .or_insert_with(|| result.clone());
        if result.nsfw_score > entry.nsfw_score {
            *entry = result.clone();
        }
    }

    let nsfw_rows = nsfw_map
        .into_iter()
        .map(|(attachment_id, result)| sms_db::MediaNsfwRow {
            attachment_id,
            nsfw_label: result.nsfw_label,
            nsfw_score: result.nsfw_score,
        })
        .collect();

    Ok((embeds, nsfw_rows))
}
