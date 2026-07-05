//! Mission statement: Provide reusable video keyframe extraction utilities for the SMS Archive
//! media pipeline so CLI and app workflows can share consistent frame sampling logic.

use crate::generate_thumbnail_for_mime;
use sms_errors::{AppError, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

#[derive(Debug, Clone)]
pub struct Keyframe {
    pub path: PathBuf,
    pub index: usize,
    pub time_ms: Option<i64>,
}

pub fn extract_keyframes(
    path: &Path,
    mime_type: &str,
    max_frames: usize,
) -> Result<(Vec<Keyframe>, Option<TempDir>)> {
    if mime_type.starts_with("video/") {
        match extract_video_keyframes_ffmpeg(path, max_frames) {
            Ok((frames, dir)) => return Ok((frames, Some(dir))),
            Err(_) => {
                let (frames, dir) = extract_video_thumbnail_fallback(path, mime_type)?;
                return Ok((frames, Some(dir)));
            }
        }
    }
    Ok((
        vec![Keyframe {
            path: path.to_path_buf(),
            index: 0,
            time_ms: None,
        }],
        None,
    ))
}

pub fn cleanup_temp_dir(dir: Option<TempDir>) {
    drop(dir);
}

fn extract_video_keyframes_ffmpeg(
    path: &Path,
    max_frames: usize,
) -> Result<(Vec<Keyframe>, TempDir)> {
    // #todo: add frame timestamp extraction via ffprobe for timeline-aware embeddings.
    let temp_dir = tempfile::tempdir()?;
    let pattern = temp_dir.path().join("frame_%04d.jpg");
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-skip_frame")
        .arg("nokey")
        .arg("-i")
        .arg(path)
        .arg("-vsync")
        .arg("vfr")
        .arg("-q:v")
        .arg("2")
        .arg("-frames:v")
        .arg(max_frames.to_string())
        .arg(pattern.clone());
    let output = cmd.output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::Media(format!("ffmpeg failed: {}", stderr.trim())));
    }

    let mut frames: Vec<PathBuf> = fs::read_dir(temp_dir.path())
        .map(|iter| {
            iter.filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("jpg"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    frames.sort();
    if frames.is_empty() {
        return Err(AppError::Media("No keyframes extracted".to_string()));
    }
    let keyframes = frames
        .into_iter()
        .enumerate()
        .map(|(index, path)| Keyframe {
            path,
            index,
            time_ms: None,
        })
        .collect::<Vec<_>>();
    Ok((keyframes, temp_dir))
}

fn extract_video_thumbnail_fallback(
    path: &Path,
    mime_type: &str,
) -> Result<(Vec<Keyframe>, TempDir)> {
    // #todo: expose thumbnail size control in the media embeddings settings.
    let temp_dir = tempfile::tempdir()?;
    let out_path = temp_dir.path().join("frame_0001.jpg");
    generate_thumbnail_for_mime(path, &out_path, 640, mime_type)?;
    if !out_path.exists() {
        return Err(AppError::Media(
            "Unable to extract fallback thumbnail".to_string(),
        ));
    }
    Ok((
        vec![Keyframe {
            path: out_path,
            index: 0,
            time_ms: None,
        }],
        temp_dir,
    ))
}
