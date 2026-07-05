//! Mission statement: Preprocess media frames into CLIP-compatible tensors by resizing, cropping,
//! and normalizing pixels to the exact format expected by the configured CLIP ONNX encoder,
//! ensuring deterministic embeddings across GPU and CPU pipelines.

use image::DynamicImage;
use ndarray::Array4;
use sms_errors::{AppError, Result};

#[derive(Debug, Clone)]
pub struct ClipPreprocessConfig {
    pub target_size: u32,
}

impl Default for ClipPreprocessConfig {
    fn default() -> Self {
        Self { target_size: 336 }
    }
}

// #todo: consider exposing a dynamic resize strategy for non-square CLIP input shapes.

const CLIP_MEAN: [f32; 3] = [0.48145466, 0.4578275, 0.40821073];
const CLIP_STD: [f32; 3] = [0.26862954, 0.26130258, 0.27577711];

pub fn clip_preprocess_batch(
    images: &[DynamicImage],
    config: &ClipPreprocessConfig,
) -> Result<Array4<f32>> {
    if images.is_empty() {
        return Err(AppError::Media("No images to preprocess".to_string()));
    }
    let size = config.target_size.max(224);
    let mut tensor = Array4::<f32>::zeros((images.len(), 3, size as usize, size as usize));

    for (index, image) in images.iter().enumerate() {
        let rgb = image.to_rgb8();
        let (width, height) = rgb.dimensions();
        let scale = (size as f32 / width as f32).max(size as f32 / height as f32);
        let new_w = (width as f32 * scale).round().max(1.0) as u32;
        let new_h = (height as f32 * scale).round().max(1.0) as u32;
        let resized =
            image::imageops::resize(&rgb, new_w, new_h, image::imageops::FilterType::Lanczos3);

        let x = (new_w.saturating_sub(size)) / 2;
        let y = (new_h.saturating_sub(size)) / 2;
        let cropped = image::imageops::crop_imm(&resized, x, y, size, size).to_image();

        for (y, row) in cropped.rows().enumerate() {
            for (x, pixel) in row.enumerate() {
                let [r, g, b] = pixel.0;
                let r = r as f32 / 255.0;
                let g = g as f32 / 255.0;
                let b = b as f32 / 255.0;
                tensor[[index, 0, y, x]] = (r - CLIP_MEAN[0]) / CLIP_STD[0];
                tensor[[index, 1, y, x]] = (g - CLIP_MEAN[1]) / CLIP_STD[1];
                tensor[[index, 2, y, x]] = (b - CLIP_MEAN[2]) / CLIP_STD[2];
            }
        }
    }

    Ok(tensor)
}
