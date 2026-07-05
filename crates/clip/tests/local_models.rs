//! Smoke tests against locally generated/downloaded model files. `ml/` is
//! gitignored, so these skip on machines without the models (including CI)
//! and run wherever a real setup exists.

use image::DynamicImage;
use std::path::PathBuf;

fn repo_ml(rel: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../ml")
        .join(rel);
    path.exists().then_some(path)
}

/// Full pipeline with the Marqo image-input NSFW classifier
/// (scripts/setup_marqo_nsfw.py): frames are scored directly as pixels
/// while embeddings still come from CLIP.
#[test]
fn clip1_vision_plus_marqo_image_classifier_end_to_end() {
    let (Some(vision), Some(nsfw)) = (
        repo_ml("CLIP1/vision_model_fp16.onnx"),
        repo_ml("nsfw_marqo_384.onnx"),
    ) else {
        eprintln!("skipping: ml/ model files not present");
        return;
    };

    let mut encoder = sms_clip::ClipEncoder::new(&vision, &nsfw).expect("models should load");
    let gray = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        512,
        384,
        image::Rgb([128, 128, 128]),
    ));
    let noise = {
        let mut img = image::RgbImage::new(400, 300);
        let mut seed = 0x12345678u32;
        for px in img.pixels_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let b = seed.to_le_bytes();
            *px = image::Rgb([b[0], b[1], b[2]]);
        }
        DynamicImage::ImageRgb8(img)
    };
    let results = encoder
        .encode_batch(&[gray, noise])
        .expect("encode should run");
    assert_eq!(results.len(), 2);
    for r in &results {
        assert_eq!(r.embedding.len(), 768);
        assert!(
            (0.0..=1.0).contains(&r.nsfw_score),
            "score must be a probability, got {}",
            r.nsfw_score
        );
        assert!(!r.nsfw_label.is_empty());
    }
}

/// Full pipeline: CLIP1 fp16 vision encoder → 768-dim L2-normalized
/// embedding → LAION-derived NSFW classifier → probability + label.
/// Also guards against ONNX Runtime graph-optimizer regressions on the fp16
/// CLIP graph (newer ORT releases have a SimplifiedLayerNormFusion bug that
/// breaks this exact model when optimizations are enabled).
#[test]
fn clip1_vision_plus_nsfw_classifier_end_to_end() {
    let (Some(vision), Some(nsfw)) = (
        repo_ml("CLIP1/vision_model_fp16.onnx"),
        repo_ml("nsfw_classifier.onnx"),
    ) else {
        eprintln!("skipping: ml/ model files not present");
        return;
    };

    let mut encoder = sms_clip::ClipEncoder::new(&vision, &nsfw).expect("models should load");
    let gray = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        64,
        64,
        image::Rgb([128, 128, 128]),
    ));
    let results = encoder.encode_batch(&[gray]).expect("encode should run");
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert_eq!(r.embedding.len(), 768, "expected projected CLIP embedding");
    let norm: f32 = r.embedding.iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-2,
        "embedding should be L2-normalized, norm={norm}"
    );
    assert!(
        (0.0..=1.0).contains(&r.nsfw_score),
        "score must be a probability, got {}",
        r.nsfw_score
    );
    assert!(!r.nsfw_label.is_empty());
}
