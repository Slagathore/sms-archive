//! Background thumbnail decoding and the on-disk preview cache.
//!
//! Decoding (and ffmpeg/HEIC preview generation) runs on worker threads so it
//! never blocks the UI render pass — previously the biggest media-browsing
//! jank source.

use sms_media::generate_thumbnail_for_mime;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime};

pub(crate) struct PreviewCache {
    pub(crate) dir: PathBuf,
    max_bytes: u64,
    current_bytes: u64,
    pub(crate) last_scan: Instant,
}

impl PreviewCache {
    pub(crate) fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes,
            current_bytes: 0,
            last_scan: Instant::now(),
        }
    }

    pub(crate) fn rescan(&mut self) {
        let mut total = 0u64;
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        total = total.saturating_add(meta.len());
                    }
                }
            }
        }
        self.current_bytes = total;
        self.last_scan = Instant::now();
    }

    pub(crate) fn prune_if_needed(&mut self) {
        if self.current_bytes <= self.max_bytes {
            return;
        }
        let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        if let Ok(read_dir) = fs::read_dir(&self.dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                        entries.push((path, meta.len(), modified));
                    }
                }
            }
        }
        entries.sort_by_key(|(_, _, modified)| *modified);
        for (path, size, _) in entries {
            if self.current_bytes <= self.max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                self.current_bytes = self.current_bytes.saturating_sub(size);
            }
        }
    }
}

/// A decode request handed to a background thumbnail worker.
pub(crate) struct ThumbJob {
    pub(crate) key: String,
    pub(crate) file_path: Option<PathBuf>,
    pub(crate) thumb_path: Option<PathBuf>,
    pub(crate) mime: String,
    pub(crate) cache_dir: PathBuf,
}

/// A decoded thumbnail (or a failure) ready to upload on the UI thread.
pub(crate) enum ThumbReady {
    Ok(String, egui::ColorImage),
    Fail(String),
}

/// Background thumbnail decoder: worker threads resolve/generate a preview
/// and decode it to a `ColorImage` off the UI thread; the UI thread uploads
/// the result to a texture. `inflight`/`failed` dedupe requests so a given
/// key is only decoded once.
pub(crate) struct ThumbnailLoader {
    tx: crossbeam_channel::Sender<ThumbJob>,
    pub(crate) rx: crossbeam_channel::Receiver<ThumbReady>,
    pub(crate) inflight: HashSet<String>,
    pub(crate) failed: HashSet<String>,
    _workers: Vec<JoinHandle<()>>,
}

impl Default for ThumbnailLoader {
    fn default() -> Self {
        let (job_tx, job_rx) = crossbeam_channel::unbounded::<ThumbJob>();
        let (res_tx, res_rx) = crossbeam_channel::unbounded::<ThumbReady>();
        let workers = (0..3)
            .map(|_| {
                let job_rx = job_rx.clone();
                let res_tx = res_tx.clone();
                std::thread::spawn(move || {
                    while let Ok(job) = job_rx.recv() {
                        let ready = match decode_thumb_job(&job) {
                            Some(img) => ThumbReady::Ok(job.key, img),
                            None => ThumbReady::Fail(job.key),
                        };
                        if res_tx.send(ready).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
        Self {
            tx: job_tx,
            rx: res_rx,
            inflight: HashSet::new(),
            failed: HashSet::new(),
            _workers: workers,
        }
    }
}

impl ThumbnailLoader {
    /// Enqueue a decode unless the key is already cached-in-flight or has
    /// previously failed (so we don't hammer unreadable files every frame).
    pub(crate) fn request(&mut self, job: ThumbJob) {
        if self.inflight.contains(&job.key) || self.failed.contains(&job.key) {
            return;
        }
        self.inflight.insert(job.key.clone());
        let _ = self.tx.send(job);
    }
}

/// Resolve a job's source image (a DB thumbnail, or a freshly generated
/// preview) and decode it to a `ColorImage`, capping the long edge so GPU
/// uploads stay cheap. Runs on a worker thread.
fn decode_thumb_job(job: &ThumbJob) -> Option<egui::ColorImage> {
    let source = if let Some(thumb) = &job.thumb_path {
        thumb.clone()
    } else {
        let file = job.file_path.as_ref()?;
        if !file.exists() {
            return None;
        }
        let preview = preview_path_for(file, &job.cache_dir);
        if !preview.exists() {
            let _ = fs::create_dir_all(&job.cache_dir);
            generate_thumbnail_for_mime(file, &preview, 256, &job.mime).ok()?;
        }
        preview
    };
    let img = image::open(&source).ok()?;
    let img = if img.width().max(img.height()) > 640 {
        img.thumbnail(640, 640)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, &rgba))
}

/// A neutral square placeholder shown while a thumbnail decodes in the
/// background (keeps grid layout stable so items don't reflow when the real
/// image pops in).
pub(crate) fn thumb_placeholder(ui: &mut egui::Ui, edge: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(edge, edge), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 4.0, ui.visuals().extreme_bg_color);
}

fn preview_path_for(source: &Path, cache_dir: &Path) -> PathBuf {
    let mut hasher = blake3::Hasher::new();
    hasher.update(source.to_string_lossy().as_bytes());
    if let Ok(meta) = fs::metadata(source) {
        hasher.update(&meta.len().to_le_bytes());
        if let Ok(modified) = meta.modified() {
            if let Ok(duration) = modified.duration_since(SystemTime::UNIX_EPOCH) {
                hasher.update(&duration.as_secs().to_le_bytes());
                hasher.update(&duration.subsec_nanos().to_le_bytes());
            }
        }
    }
    let hash = hasher.finalize().to_hex().to_string();
    cache_dir.join(format!("{}.jpg", hash))
}
