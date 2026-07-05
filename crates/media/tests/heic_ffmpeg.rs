//! HEIC decoding via the system-ffmpeg fallback.
//!
//! Requires an `ffmpeg` (>= 7, for HEIF demuxing) on PATH, so the test is
//! gated behind an env var and skips everywhere else (CI images ship older
//! ffmpeg or none at all). Run locally with:
//!
//! ```text
//! $env:SMS_HEIC_FFMPEG_TEST = "1"; cargo test -p sms-media --test heic_ffmpeg
//! ```

use std::path::Path;

fn gate() -> bool {
    if std::env::var("SMS_HEIC_FFMPEG_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipping: set SMS_HEIC_FFMPEG_TEST=1 (needs ffmpeg >= 7 on PATH)");
        return false;
    }
    true
}

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny.heic")
}

#[test]
fn open_image_decodes_heic_via_ffmpeg_fallback() {
    if !gate() {
        return;
    }
    let img = sms_media::open_image(&fixture()).expect("HEIC should decode via ffmpeg fallback");
    assert_eq!((img.width(), img.height()), (64, 48));
}

#[test]
fn heic_thumbnail_generation_works_without_libheif() {
    if !gate() {
        return;
    }
    let out_dir = tempfile::tempdir().unwrap();
    let dest = out_dir.path().join("thumb.jpg");
    sms_media::generate_thumbnail_for_mime(&fixture(), &dest, 32, "image/heic")
        .expect("HEIC thumbnail should generate via ffmpeg");
    let thumb = image::open(&dest).unwrap();
    assert!(thumb.width() <= 32 && thumb.height() <= 32);
}
