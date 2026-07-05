//! Media processing (thumbnails, hashing)

pub mod keyframes;

use blake3::Hasher;
use sms_errors::Result;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

pub fn generate_thumbnail(
    source: &std::path::Path,
    dest: &std::path::Path,
    max_size: u32,
) -> Result<()> {
    let img = image::open(source).map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
    let thumb = img.thumbnail(max_size, max_size);
    thumb
        .save(dest)
        .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
    Ok(())
}

pub fn hash_file(path: &std::path::Path) -> Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let hash = hasher.finalize();
    Ok(*hash.as_bytes())
}

#[derive(Debug, Clone)]
pub struct ThumbnailQueue {
    tx: crossbeam_channel::Sender<ThumbnailJob>,
    pending: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct ThumbnailJob {
    source: PathBuf,
    dest: PathBuf,
    max_size: u32,
    mime_type: String,
}

impl ThumbnailQueue {
    pub fn spawn(worker_count: usize, capacity: usize) -> (Self, Vec<JoinHandle<()>>) {
        let (tx, rx) = crossbeam_channel::bounded::<ThumbnailJob>(capacity.max(1));
        let pending = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::with_capacity(worker_count.max(1));
        for _ in 0..worker_count.max(1) {
            let rx = rx.clone();
            let pending = Arc::clone(&pending);
            let handle = std::thread::spawn(move || {
                while let Ok(job) = rx.recv() {
                    let _ = generate_thumbnail_for_mime(
                        &job.source,
                        &job.dest,
                        job.max_size,
                        &job.mime_type,
                    );
                    pending.fetch_sub(1, Ordering::Relaxed);
                }
            });
            handles.push(handle);
        }
        (Self { tx, pending }, handles)
    }

    pub fn try_enqueue(
        &self,
        source: PathBuf,
        dest: PathBuf,
        max_size: u32,
        mime_type: String,
    ) -> bool {
        let job = ThumbnailJob {
            source,
            dest,
            max_size,
            mime_type,
        };
        match self.tx.try_send(job) {
            Ok(_) => {
                self.pending.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => false,
        }
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }
}

pub fn generate_thumbnail_for_mime(
    source: &std::path::Path,
    dest: &std::path::Path,
    max_size: u32,
    mime_type: &str,
) -> Result<()> {
    if is_heic_mime(mime_type) {
        return generate_thumbnail_heic(source, dest, max_size);
    }
    if is_video_mime(mime_type) {
        return generate_video_thumbnail(source, dest, max_size);
    }
    generate_thumbnail(source, dest, max_size)
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

#[cfg(feature = "heic")]
pub fn generate_thumbnail_heic(
    source: &std::path::Path,
    dest: &std::path::Path,
    max_size: u32,
) -> Result<()> {
    let ctx = libheif_rs::HeifContext::read_from_file(source)
        .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
    let handle = ctx
        .primary_image_handle()
        .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
    let image = handle
        .decode(
            libheif_rs::ColorSpace::Rgb(libheif_rs::RgbChroma::Rgb),
            None,
        )
        .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
    let width = image.width() as u32;
    let height = image.height() as u32;
    let data = image
        .planes()
        .interleaved
        .as_ref()
        .ok_or_else(|| sms_errors::AppError::Media("Missing HEIC data".into()))?;
    let img = image::RgbImage::from_raw(width, height, data.to_vec())
        .ok_or_else(|| sms_errors::AppError::Media("Invalid HEIC buffer".into()))?;
    let dyn_img = image::DynamicImage::ImageRgb8(img);
    let thumb = dyn_img.thumbnail(max_size, max_size);
    thumb
        .save(dest)
        .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
    Ok(())
}

#[cfg(not(feature = "heic"))]
pub fn generate_thumbnail_heic(
    _source: &std::path::Path,
    _dest: &std::path::Path,
    _max_size: u32,
) -> Result<()> {
    Err(sms_errors::AppError::Media(
        "HEIC support not enabled".into(),
    ))
}

#[cfg(feature = "ffmpeg")]
pub fn generate_video_thumbnail(
    source: &std::path::Path,
    dest: &std::path::Path,
    max_size: u32,
) -> Result<()> {
    use ffmpeg_next as ffmpeg;
    let result = (|| {
        ffmpeg::init().map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
        let mut ictx = ffmpeg::format::input(&source)
            .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
        let input = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| sms_errors::AppError::Media("No video stream".into()))?;
        let stream_index = input.index();
        let context_decoder = ffmpeg::codec::context::Context::from_parameters(input.parameters())
            .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
        let mut decoder = context_decoder
            .decoder()
            .video()
            .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
        let mut scaler = ffmpeg::software::scaling::context::Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            ffmpeg::format::Pixel::RGB24,
            max_size,
            max_size,
            ffmpeg::software::scaling::flag::Flags::BILINEAR,
        )
        .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
        let mut frame_index = 0usize;
        for (stream, packet) in ictx.packets() {
            if stream.index() == stream_index {
                decoder
                    .send_packet(&packet)
                    .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
                let mut frame = ffmpeg::util::frame::Video::empty();
                if decoder.receive_frame(&mut frame).is_ok() {
                    let mut rgb_frame = ffmpeg::util::frame::Video::empty();
                    scaler
                        .run(&frame, &mut rgb_frame)
                        .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
                    let data = rgb_frame.data(0);
                    let width = rgb_frame.width();
                    let height = rgb_frame.height();
                    let img = image::RgbImage::from_raw(width, height, data.to_vec()).ok_or_else(
                        || sms_errors::AppError::Media("Invalid video buffer".into()),
                    )?;
                    let dyn_img = image::DynamicImage::ImageRgb8(img);
                    let thumb = dyn_img.thumbnail(max_size, max_size);
                    thumb
                        .save(dest)
                        .map_err(|e| sms_errors::AppError::Media(e.to_string()))?;
                    frame_index += 1;
                    break;
                }
            }
        }
        if frame_index == 0 {
            return Err(sms_errors::AppError::Media(
                "Unable to decode video frame".into(),
            ));
        }
        Ok(())
    })();
    if result.is_err() {
        if generate_video_thumbnail_cli(source, dest, max_size).is_ok() {
            return Ok(());
        }
    }
    result
}

#[cfg(not(feature = "ffmpeg"))]
pub fn generate_video_thumbnail(
    source: &std::path::Path,
    dest: &std::path::Path,
    max_size: u32,
) -> Result<()> {
    generate_video_thumbnail_cli(source, dest, max_size)
}

fn generate_video_thumbnail_cli(source: &Path, dest: &Path, max_size: u32) -> Result<()> {
    let Some(parent) = dest.parent() else {
        return Err(sms_errors::AppError::Media(
            "Invalid thumbnail destination".into(),
        ));
    };
    std::fs::create_dir_all(parent)?;
    let scale_filter = format!(
        "scale={}:{}:force_original_aspect_ratio=decrease",
        max_size, max_size
    );
    let output = Command::new("ffmpeg")
        .args([
            "-y",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            source.to_string_lossy().as_ref(),
            "-vframes",
            "1",
            "-vf",
            &scale_filter,
            "-q:v",
            "2",
            dest.to_string_lossy().as_ref(),
        ])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(sms_errors::AppError::Media(format!(
            "ffmpeg failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hashes_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"hello").unwrap();
        let hash = hash_file(tmp.path()).unwrap();
        assert_eq!(hash.len(), 32);
    }
}
