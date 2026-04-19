//! Mission statement: Provide NSFW classification on CLIP embeddings, supporting both
//! simple linear probes (.npz/.safetensors) and full ONNX MLP models.

use ndarray::{Array1, Array2};
use ndarray_npy::NpzReader;
use ort::session::Session;
use ort::value::Tensor;
use safetensors::tensor::TensorView;
use safetensors::SafeTensors;
use sms_errors::{AppError, Result};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct NsfwScore {
    pub label: String,
    pub score: f32,
}

fn label_from_score(score: f32) -> String {
    if score >= 0.85 {
        "EXPLICIT".to_string()
    } else if score >= 0.70 {
        "NSFW".to_string()
    } else if score >= 0.55 {
        "SUGGESTIVE".to_string()
    } else if score >= 0.40 {
        "QUESTIONABLE".to_string()
    } else {
        "SAFE".to_string()
    }
    // #todo: allow customizing NSFW category thresholds per user profile.
}

/// NSFW classifier - supports either ONNX MLP or simple linear probe
pub enum NsfwClassifier {
    Onnx(OnnxClassifier),
    Probe(NsfwProbe),
}

impl NsfwClassifier {
    /// Load from file - auto-detects format by extension
    pub fn load(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();

        match ext.as_str() {
            "onnx" => Ok(NsfwClassifier::Onnx(OnnxClassifier::load(path)?)),
            "npz" | "safetensors" => Ok(NsfwClassifier::Probe(NsfwProbe::load(path)?)),
            _ => Err(AppError::Media(format!(
                "Unsupported NSFW classifier format: {} (expected .onnx, .npz, or .safetensors)",
                path.display()
            ))),
        }
    }

    /// Score a single embedding
    #[allow(dead_code)]
    pub fn score(&mut self, embedding: &[f32]) -> Result<NsfwScore> {
        match self {
            NsfwClassifier::Onnx(classifier) => classifier.score(embedding),
            NsfwClassifier::Probe(probe) => probe.score(embedding),
        }
    }

    /// Score a batch of embeddings (more efficient for ONNX)
    pub fn score_batch(&mut self, embeddings: &[Vec<f32>]) -> Result<Vec<NsfwScore>> {
        match self {
            NsfwClassifier::Onnx(classifier) => classifier.score_batch(embeddings),
            NsfwClassifier::Probe(probe) => {
                embeddings.iter().map(|e| probe.score(e)).collect()
            }
        }
    }
}

/// ONNX-based NSFW classifier (full MLP)
pub struct OnnxClassifier {
    session: Session,
    input_name: String,
    output_name: String,
}

impl std::fmt::Debug for OnnxClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnnxClassifier")
            .field("input_name", &self.input_name)
            .field("output_name", &self.output_name)
            .finish()
    }
}

impl Clone for OnnxClassifier {
    fn clone(&self) -> Self {
        // ONNX sessions can't be cloned, but we shouldn't need to
        panic!("OnnxClassifier cannot be cloned - use Arc<NsfwClassifier> instead")
    }
}

impl OnnxClassifier {
    pub fn load(path: &Path) -> Result<Self> {
        let mut builder = Session::builder()
            .map_err(|e: ort::Error| AppError::Media(e.to_string()))?;

        // Use CUDA if available
        if std::env::var("SMS_CLIP_USE_CUDA").ok().as_deref() == Some("1") {
            let provider = ort::execution_providers::CUDAExecutionProvider::default().build();
            let updated = builder.clone().with_execution_providers([provider]);
            match updated {
                Ok(updated) => builder = updated,
                Err(err) => tracing::warn!("CUDA provider unavailable for NSFW: {}", err),
            }
        }

        let session = builder
            .commit_from_file(path)
            .map_err(|e: ort::Error| AppError::Media(e.to_string()))?;

        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_string())
            .ok_or_else(|| AppError::Media("NSFW model has no inputs".to_string()))?;

        let output_name = session
            .outputs()
            .first()
            .map(|o| o.name().to_string())
            .ok_or_else(|| AppError::Media("NSFW model has no outputs".to_string()))?;

        Ok(Self {
            session,
            input_name,
            output_name,
        })
    }

    #[allow(dead_code)]
    pub fn score(&mut self, embedding: &[f32]) -> Result<NsfwScore> {
        let scores = self.score_batch(&[embedding.to_vec()])?;
        scores.into_iter().next().ok_or_else(|| {
            AppError::Media("NSFW model returned no results".to_string())
        })
    }

    pub fn score_batch(&mut self, embeddings: &[Vec<f32>]) -> Result<Vec<NsfwScore>> {
        if embeddings.is_empty() {
            return Ok(vec![]);
        }

        let batch_size = embeddings.len();
        let embed_dim = embeddings[0].len();

        // Flatten into [batch, 768]
        let flat: Vec<f32> = embeddings.iter().flatten().copied().collect();
        let array = ndarray::Array2::from_shape_vec((batch_size, embed_dim), flat)
            .map_err(|e| AppError::Media(e.to_string()))?;

        let input = Tensor::<f32>::from_array(array)
            .map_err(|e: ort::Error| AppError::Media(e.to_string()))?;

        let outputs = self
            .session
            .run(ort::inputs! { self.input_name.as_str() => input })
            .map_err(|e: ort::Error| AppError::Media(e.to_string()))?;

        let output = outputs
            .iter()
            .find(|(name, _)| *name == self.output_name.as_str())
            .map(|(_, value)| value)
            .or_else(|| outputs.iter().next().map(|(_, value)| value))
            .ok_or_else(|| AppError::Media("NSFW output missing".to_string()))?;

        let array = output
            .try_extract_array::<f32>()
            .map_err(|e: ort::Error| AppError::Media(e.to_string()))?;

        // Output is [batch, 1] - extract scores
        let scores: Vec<NsfwScore> = array
            .iter()
            .map(|&score| {
                NsfwScore {
                    label: label_from_score(score),
                    score,
                }
            })
            .collect();

        Ok(scores)
    }
}

/// Simple linear probe for NSFW classification
#[derive(Debug, Clone)]
pub struct NsfwProbe {
    pub weights: Array2<f32>,
    pub bias: Array1<f32>,
}

impl NsfwProbe {
    pub fn load(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        if ext == "safetensors" {
            return load_safetensors(path);
        }
        if ext == "npz" {
            return load_npz(path);
        }
        Err(AppError::Media(format!(
            "Unsupported NSFW probe format: {}",
            path.display()
        )))
    }

    pub fn score(&self, embedding: &[f32]) -> Result<NsfwScore> {
        if embedding.len() != self.weights.shape()[1] {
            return Err(AppError::Media(format!(
                "Embedding dimension mismatch (expected {}, got {})",
                self.weights.shape()[1],
                embedding.len()
            )));
        }
        let emb = Array1::from_vec(embedding.to_vec());
        let logits = self.weights.dot(&emb) + &self.bias;
        let nsfw_logit = logits.get(0).copied().unwrap_or(0.0);
        let safe_logit = logits.get(1).copied().unwrap_or(0.0);
        let score = sigmoid(nsfw_logit - safe_logit);
        Ok(NsfwScore {
            label: label_from_score(score),
            score,
        })
    }
}

fn load_safetensors(path: &Path) -> Result<NsfwProbe> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;
    let tensors = SafeTensors::deserialize(&data)
        .map_err(|err| AppError::Media(format!("Safetensors error: {}", err)))?;
    let weight = find_tensor(&tensors, &["weight", "linear.weight", "classifier.weight"])?;
    let bias = find_tensor(&tensors, &["bias", "linear.bias", "classifier.bias"])?;
    let weight = tensor_to_array2(weight)?;
    let bias = tensor_to_array1(bias)?;
    Ok(NsfwProbe { weights: weight, bias })
}

fn load_npz(path: &Path) -> Result<NsfwProbe> {
    let file = File::open(path)?;
    let mut npz = NpzReader::new(file).map_err(|e| AppError::Media(e.to_string()))?;
    let weight: Array2<f32> = npz
        .by_name("weight.npy")
        .or_else(|_| npz.by_name("linear.weight.npy"))
        .or_else(|_| npz.by_name("classifier.weight.npy"))
        .map_err(|e| AppError::Media(format!("NPZ weight error: {}", e)))?;
    let bias: Array1<f32> = npz
        .by_name("bias.npy")
        .or_else(|_| npz.by_name("linear.bias.npy"))
        .or_else(|_| npz.by_name("classifier.bias.npy"))
        .map_err(|e| AppError::Media(format!("NPZ bias error: {}", e)))?;
    Ok(NsfwProbe { weights: weight, bias })
}

fn find_tensor<'a>(tensors: &'a SafeTensors, keys: &[&str]) -> Result<TensorView<'a>> {
    for key in keys {
        if let Ok(tensor) = tensors.tensor(key) {
            return Ok(tensor);
        }
    }
    Err(AppError::Media("Missing NSFW probe tensor".to_string()))
}

fn tensor_to_array2(tensor: TensorView<'_>) -> Result<Array2<f32>> {
    let shape = tensor.shape();
    if shape.len() != 2 {
        return Err(AppError::Media("NSFW probe weights must be 2D".to_string()));
    }
    let data = tensor.data();
    let data = bytemuck::cast_slice::<u8, f32>(data);
    Array2::from_shape_vec((shape[0], shape[1]), data.to_vec())
        .map_err(|err| AppError::Media(err.to_string()))
}

fn tensor_to_array1(tensor: TensorView<'_>) -> Result<Array1<f32>> {
    let shape = tensor.shape();
    if shape.len() != 1 {
        return Err(AppError::Media("NSFW probe bias must be 1D".to_string()));
    }
    let data = tensor.data();
    let data = bytemuck::cast_slice::<u8, f32>(data);
    Array1::from_shape_vec(shape[0], data.to_vec())
        .map_err(|err| AppError::Media(err.to_string()))
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

impl NsfwScore {
    pub fn as_tuple(&self) -> (String, f32) {
        (self.label.clone(), self.score)
    }
}
