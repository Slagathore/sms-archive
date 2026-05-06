//! Mission statement: Run CLIP ViT image encoder ONNX inference and pair embeddings
//! with NSFW scores, providing a fast unified media pipeline.

use crate::nsfw::NsfwClassifier;
use crate::preprocessing::{clip_preprocess_batch, ClipPreprocessConfig};
use image::DynamicImage;
use ort::session::Session;
use ort::value::Tensor;
use sms_errors::{AppError, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct ClipResult {
    pub embedding: Vec<f32>,
    pub nsfw_label: String,
    pub nsfw_score: f32,
}

#[derive(Debug, Clone)]
pub struct ClipStats {
    pub batch_size: usize,
    pub dims: usize,
    pub elapsed_ms: u128,
}

pub struct ClipEncoder {
    session: Session,
    input_name: String,
    output_name: String,
    preprocess: ClipPreprocessConfig,
    classifier: NsfwClassifier,
    model_path: PathBuf,
}

/// #todo: expose explicit override for CLIP input size via config or environment.
fn infer_input_size(session: &Session) -> Option<u32> {
    let shape = session.inputs().first()?.dtype().tensor_shape()?;
    if shape.len() < 4 {
        return None;
    }
    let height = shape[shape.len() - 2];
    let width = shape[shape.len() - 1];
    if height > 0 && width > 0 && height == width {
        u32::try_from(height).ok()
    } else {
        None
    }
}

fn infer_input_size_from_preprocessor(model_path: &Path) -> Option<u32> {
    let mut candidates = Vec::new();
    if let Some(parent) = model_path.parent() {
        candidates.push(parent.to_path_buf());
        if let Some(grand) = parent.parent() {
            candidates.push(grand.to_path_buf());
        }
    }
    for base in candidates {
        let config_path = base.join("preprocessor_config.json");
        if !config_path.exists() {
            continue;
        }
        let data = fs::read_to_string(&config_path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&data).ok()?;
        let crop_h = value
            .get("crop_size")
            .and_then(|v| v.get("height"))
            .and_then(|v| v.as_u64());
        let crop_w = value
            .get("crop_size")
            .and_then(|v| v.get("width"))
            .and_then(|v| v.as_u64());
        if let (Some(h), Some(w)) = (crop_h, crop_w) {
            if h > 0 && h == w {
                return u32::try_from(h).ok();
            }
        }
        let size = value
            .get("size")
            .and_then(|v| v.get("shortest_edge").or_else(|| v.get("height")))
            .and_then(|v| v.as_u64())
            .or_else(|| value.get("size").and_then(|v| v.as_u64()));
        if let Some(size) = size {
            if size > 0 {
                return u32::try_from(size).ok();
            }
        }
    }
    None
}

pub fn probe_cuda_support(clip_model: &Path) -> Result<bool> {
    let builder = Session::builder().map_err(|e| AppError::Media(e.to_string()))?;
    let provider = ort::execution_providers::CUDAExecutionProvider::default().build();
    let mut builder = match builder.with_execution_providers([provider]) {
        Ok(updated) => updated,
        Err(_) => return Ok(false),
    };
    let _session = builder
        .commit_from_file(clip_model)
        .map_err(|e| AppError::Media(e.to_string()))?;
    Ok(true)
}

impl ClipEncoder {
    pub fn new(clip_model: &Path, nsfw_weights: &Path) -> Result<Self> {
        let mut builder = Session::builder()
            .map_err(|e| AppError::Media(e.to_string()))?;

        if std::env::var("SMS_CLIP_USE_CUDA").ok().as_deref() == Some("1") {
            let provider = ort::execution_providers::CUDAExecutionProvider::default().build();
            let updated = builder.clone().with_execution_providers([provider]);
            match updated {
                Ok(updated) => builder = updated,
                Err(err) => tracing::warn!("CUDA provider unavailable: {}", err),
            }
        }

        let session = builder
            .commit_from_file(clip_model)
            .map_err(|e| AppError::Media(e.to_string()))?;

        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_string())
            .ok_or_else(|| AppError::Media("CLIP model has no inputs".to_string()))?;

        let output_name = session
            .outputs()
            .first()
            .map(|o| o.name().to_string())
            .ok_or_else(|| AppError::Media("CLIP model has no outputs".to_string()))?;

        // Load NSFW classifier (auto-detects ONNX vs probe format)
        let classifier = NsfwClassifier::load(nsfw_weights)?;

        let mut preprocess = ClipPreprocessConfig::default();
        let inferred_size = infer_input_size(&session)
            .or_else(|| infer_input_size_from_preprocessor(clip_model));
        if let Some(size) = inferred_size {
            preprocess.target_size = size.max(224);
        }

        Ok(Self {
            session,
            input_name,
            output_name,
            preprocess,
            classifier,
            model_path: clip_model.to_path_buf(),
        })
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn encode_batch(&mut self, images: &[DynamicImage]) -> Result<Vec<ClipResult>> {
        let start = Instant::now();

        // Preprocess images -> [batch, 3, 336, 336]
        let tensor = clip_preprocess_batch(images, &self.preprocess)?;
        let input = Tensor::<f32>::from_array(tensor)
            .map_err(|e| AppError::Media(e.to_string()))?;

        // Run CLIP encoder
        let outputs = self
            .session
            .run(ort::inputs! { self.input_name.as_str() => input })
            .map_err(|e| AppError::Media(e.to_string()))?;

        let output = outputs
            .iter()
            .find(|(name, _)| *name == self.output_name.as_str())
            .map(|(_, value)| value)
            .or_else(|| outputs.iter().next().map(|(_, value)| value))
            .ok_or_else(|| AppError::Media("CLIP output missing".to_string()))?;

        let array = output
            .try_extract_array::<f32>()
            .map_err(|e| AppError::Media(e.to_string()))?;

        let array = array
            .into_dimensionality::<ndarray::Ix2>()
            .map_err(|e: ndarray::ShapeError| AppError::Media(e.to_string()))?;

        // L2 normalize embeddings
        let embeddings: Vec<Vec<f32>> = array
            .rows()
            .into_iter()
            .map(|row: ndarray::ArrayView1<'_, f32>| l2_normalize(row.to_vec()))
            .collect();

        // Run NSFW classifier on all embeddings (batch)
        let nsfw_scores = self.classifier.score_batch(&embeddings)?;

        // Combine results
        let results: Vec<ClipResult> = embeddings
            .into_iter()
            .zip(nsfw_scores)
            .map(|(embedding, nsfw)| ClipResult {
                embedding,
                nsfw_label: nsfw.label,
                nsfw_score: nsfw.score,
            })
            .collect();

        let _stats = ClipStats {
            batch_size: results.len(),
            dims: results.first().map(|r| r.embedding.len()).unwrap_or(0),
            elapsed_ms: start.elapsed().as_millis(),
        };

        Ok(results)
    }
}

fn l2_normalize(vec: Vec<f32>) -> Vec<f32> {
    let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm == 0.0 {
        return vec;
    }
    vec.into_iter().map(|v| v / norm).collect()
}
