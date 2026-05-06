//! ML embeddings and inference

use blake3::Hasher;
#[cfg(feature = "onnx")]
use ndarray::{Axis, IxDyn};
#[cfg(feature = "onnx")]
use ort::session::Session;
#[cfg(feature = "onnx")]
use ort::session::builder::GraphOptimizationLevel;
#[cfg(feature = "onnx")]
use ort::value::{Outlet, Tensor};
use sha2::Digest;
#[cfg(feature = "onnx")]
use sms_errors::AppError;
use sms_errors::Result;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(feature = "onnx")]
use tokenizers::{
    utils::padding::{PaddingDirection, PaddingParams, PaddingStrategy},
    utils::truncation::{TruncationDirection, TruncationParams, TruncationStrategy},
    Tokenizer,
};

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    pub version: String,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelMeta {
    pub dimensions: usize,
    pub max_length: usize,
    pub normalize: bool,
    pub tokenizer_path: Option<String>,
    pub input_ids_name: Option<String>,
    pub attention_mask_name: Option<String>,
    pub token_type_ids_name: Option<String>,
    pub output_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    pub model_path: Option<PathBuf>,
    pub tokenizer_path: Option<PathBuf>,
    pub model_name: String,
    pub model_version: String,
    pub dimensions: usize,
    pub device: DevicePreference,
    pub max_length: usize,
    pub normalize: bool,
    pub input_ids_name: Option<String>,
    pub attention_mask_name: Option<String>,
    pub token_type_ids_name: Option<String>,
    pub output_name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePreference {
    Cpu,
    Gpu,
}

pub struct EmbeddingService {
    info: ModelInfo,
    dimensions: usize,
    normalize: bool,
    meta: ModelMeta,
    backend: Backend,
}

enum Backend {
    Dummy,
    #[cfg(feature = "onnx")]
    Onnx(OnnxBackend),
}

#[cfg(feature = "onnx")]
struct OnnxBackend {
    session: Session,
    tokenizer: Tokenizer,
    input_ids_name: String,
    attention_mask_name: Option<String>,
    token_type_ids_name: Option<String>,
    output_name: String,
}

impl EmbeddingService {
    pub fn new(config: EmbeddingConfig) -> Result<Self> {
        let sha256 = config
            .model_path
            .as_deref()
            .and_then(|path| compute_sha256(path).ok());
        let info = ModelInfo {
            name: config.model_name,
            version: config.model_version,
            sha256,
        };

        let backend = match config.model_path.as_deref() {
            Some(path) => {
                #[cfg(feature = "onnx")]
                {
                    let tokenizer_path = config.tokenizer_path.clone().ok_or_else(|| {
                        AppError::Media("Tokenizer path required for ONNX inference".to_string())
                    })?;
                    let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
                        .map_err(|e| AppError::Media(e.to_string()))?;
                    let max_length = config.max_length.max(8);
                    configure_tokenizer(&mut tokenizer, max_length)
                        .map_err(|e| AppError::Media(e.to_string()))?;

                    let mut builder = Session::builder()
                        .map_err(|e| AppError::Media(e.to_string()))?;
                    if matches!(config.device, DevicePreference::Gpu) {
                        let provider =
                            ort::execution_providers::CUDAExecutionProvider::default().build();
                        let updated = builder.clone().with_execution_providers([provider]);
                        match updated {
                            Ok(updated) => builder = updated,
                            Err(err) => tracing::warn!("CUDA provider unavailable: {}", err),
                        }
                    }
                    let session = match builder.clone().commit_from_file(path) {
                        Ok(session) => session,
                        Err(err) => {
                            tracing::warn!(
                                "ONNX session init failed, retrying with optimizations disabled: {}",
                                err
                            );
                            builder
                                .with_optimization_level(GraphOptimizationLevel::Disable)
                                .map_err(|e| AppError::Media(e.to_string()))?
                                .commit_from_file(path)
                                .map_err(|e| AppError::Media(e.to_string()))?
                        }
                    };

                    let input_ids_name = config
                        .input_ids_name
                        .clone()
                        .or_else(|| {
                            find_input_name(
                                session.inputs(),
                                &["input_ids", "input_ids:0", "ids", "input"],
                            )
                        })
                        .ok_or_else(|| {
                            AppError::Media("Could not find input_ids in model".to_string())
                        })?;
                    let attention_mask_name = config.attention_mask_name.clone().or_else(|| {
                        find_input_name(
                            session.inputs(),
                            &["attention_mask", "attention_mask:0", "mask"],
                        )
                    });
                    let token_type_ids_name = config.token_type_ids_name.clone().or_else(|| {
                        find_input_name(
                            session.inputs(),
                            &["token_type_ids", "token_type_id", "segment_ids"],
                        )
                    });
                    let output_name = config
                        .output_name
                        .clone()
                        .or_else(|| {
                            find_output_name(
                                session.outputs(),
                                &[
                                    "sentence_embedding",
                                    "embeddings",
                                    "embedding",
                                    "pooler_output",
                                    "last_hidden_state",
                                    "output",
                                ],
                            )
                        })
                        .or_else(|| session.outputs().first().map(|o| o.name().to_string()))
                        .ok_or_else(|| AppError::Media("Model has no outputs".to_string()))?;

                    Backend::Onnx(OnnxBackend {
                        session,
                        tokenizer,
                        input_ids_name,
                        attention_mask_name,
                        token_type_ids_name,
                        output_name,
                    })
                }
                #[cfg(not(feature = "onnx"))]
                {
                    let _ = path;
                    Backend::Dummy
                }
            }
            None => Backend::Dummy,
        };

        let meta = ModelMeta {
            dimensions: config.dimensions,
            max_length: config.max_length,
            normalize: config.normalize,
            tokenizer_path: config
                .tokenizer_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            input_ids_name: match &backend {
                #[cfg(feature = "onnx")]
                Backend::Onnx(backend) => Some(backend.input_ids_name.clone()),
                _ => config.input_ids_name.clone(),
            },
            attention_mask_name: match &backend {
                #[cfg(feature = "onnx")]
                Backend::Onnx(backend) => backend.attention_mask_name.clone(),
                _ => config.attention_mask_name.clone(),
            },
            token_type_ids_name: match &backend {
                #[cfg(feature = "onnx")]
                Backend::Onnx(backend) => backend.token_type_ids_name.clone(),
                _ => config.token_type_ids_name.clone(),
            },
            output_name: match &backend {
                #[cfg(feature = "onnx")]
                Backend::Onnx(backend) => Some(backend.output_name.clone()),
                _ => config.output_name.clone(),
            },
        };

        Ok(Self {
            info,
            dimensions: config.dimensions,
            normalize: config.normalize,
            meta,
            backend,
        })
    }

    pub fn model_info(&self) -> &ModelInfo {
        &self.info
    }

    pub fn model_meta(&self) -> &ModelMeta {
        &self.meta
    }

    pub fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        match &mut self.backend {
            Backend::Dummy => {
                let mut out = hash_embed(text, self.dimensions);
                if self.normalize {
                    l2_normalize(&mut out);
                }
                Ok(out)
            }
            #[cfg(feature = "onnx")]
            Backend::Onnx(backend) => embed_onnx(backend, text, self.dimensions, self.normalize),
        }
    }
}

#[cfg(feature = "onnx")]
fn embed_onnx(
    backend: &mut OnnxBackend,
    text: &str,
    dims: usize,
    normalize: bool,
) -> Result<Vec<f32>> {
    let encoding = backend
        .tokenizer
        .encode(text, true)
        .map_err(|e| AppError::Media(e.to_string()))?;
    let ids: Vec<i64> = encoding.get_ids().iter().map(|&v| v as i64).collect();
    let mask: Vec<i64> = encoding
        .get_attention_mask()
        .iter()
        .map(|&v| v as i64)
        .collect();
    let type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|&v| v as i64).collect();

    let seq_len = ids.len().max(1);
    let input_ids = Tensor::<i64>::from_array(([1usize, seq_len], ids.into_boxed_slice()))
        .map_err(|e| AppError::Media(e.to_string()))?;
    let attention_mask =
        Tensor::<i64>::from_array(([1usize, seq_len], mask.clone().into_boxed_slice()))
            .map_err(|e| AppError::Media(e.to_string()))?;
    let token_type_ids =
        Tensor::<i64>::from_array(([1usize, seq_len], type_ids.into_boxed_slice()))
            .map_err(|e| AppError::Media(e.to_string()))?;

    let outputs = match (
        backend.attention_mask_name.as_ref(),
        backend.token_type_ids_name.as_ref(),
    ) {
        (Some(mask_name), Some(type_name)) => backend
            .session
            .run(ort::inputs! {
                backend.input_ids_name.as_str() => input_ids,
                mask_name.as_str() => attention_mask,
                type_name.as_str() => token_type_ids
            })
            .map_err(|e| AppError::Media(e.to_string()))?,
        (Some(mask_name), None) => backend
            .session
            .run(ort::inputs! {
                backend.input_ids_name.as_str() => input_ids,
                mask_name.as_str() => attention_mask
            })
            .map_err(|e| AppError::Media(e.to_string()))?,
        (None, _) => backend
            .session
            .run(ort::inputs! {
                backend.input_ids_name.as_str() => input_ids
            })
            .map_err(|e| AppError::Media(e.to_string()))?,
    };

    let output = if outputs.contains_key(backend.output_name.as_str()) {
        &outputs[backend.output_name.as_str()]
    } else {
        &outputs[0]
    };
    let array = output
        .try_extract_array::<f32>()
        .map_err(|e| AppError::Media(e.to_string()))?;

    let mut embedding = extract_embedding(array.into(), Some(&mask))?;
    if dims > 0 && embedding.len() != dims {
        if embedding.len() > dims {
            embedding.truncate(dims);
        } else {
            embedding.resize(dims, 0.0);
        }
    }
    if normalize {
        l2_normalize(&mut embedding);
    }
    Ok(embedding)
}

#[cfg(feature = "onnx")]
fn configure_tokenizer(tokenizer: &mut Tokenizer, max_length: usize) -> Result<()> {
    let trunc = TruncationParams {
        max_length,
        stride: 0,
        strategy: TruncationStrategy::LongestFirst,
        direction: TruncationDirection::Right,
    };
    tokenizer
        .with_truncation(Some(trunc))
        .map_err(|e| AppError::Media(e.to_string()))?;

    let pad_id = tokenizer
        .get_padding()
        .map(|p| p.pad_id)
        .or_else(|| tokenizer.token_to_id("[PAD]"))
        .or_else(|| tokenizer.token_to_id("<pad>"))
        .unwrap_or(0);
    let pad_token = tokenizer
        .id_to_token(pad_id)
        .unwrap_or_else(|| "[PAD]".to_string());
    let padding = PaddingParams {
        strategy: PaddingStrategy::Fixed(max_length),
        direction: PaddingDirection::Right,
        pad_to_multiple_of: None,
        pad_id,
        pad_type_id: 0,
        pad_token,
    };
    tokenizer.with_padding(Some(padding));
    Ok(())
}

#[cfg(feature = "onnx")]
fn find_input_name(inputs: &[Outlet], candidates: &[&str]) -> Option<String> {
    for cand in candidates {
        if let Some(found) = inputs.iter().find(|i| i.name().eq_ignore_ascii_case(cand)) {
            return Some(found.name().to_string());
        }
        if let Some(found) = inputs.iter().find(|i| {
            i.name()
                .to_ascii_lowercase()
                .contains(&cand.to_ascii_lowercase())
        }) {
            return Some(found.name().to_string());
        }
    }
    None
}

#[cfg(feature = "onnx")]
fn find_output_name(outputs: &[Outlet], candidates: &[&str]) -> Option<String> {
    for cand in candidates {
        if let Some(found) = outputs.iter().find(|o| o.name().eq_ignore_ascii_case(cand)) {
            return Some(found.name().to_string());
        }
        if let Some(found) = outputs.iter().find(|o| {
            o.name()
                .to_ascii_lowercase()
                .contains(&cand.to_ascii_lowercase())
        }) {
            return Some(found.name().to_string());
        }
    }
    None
}

#[cfg(feature = "onnx")]
fn extract_embedding(
    array: ndarray::CowArray<f32, IxDyn>,
    attention_mask: Option<&[i64]>,
) -> Result<Vec<f32>> {
    let shape = array.shape().to_vec();
    match shape.len() {
        1 => Ok(array.iter().cloned().collect()),
        2 => {
            let view = array.index_axis(Axis(0), 0);
            Ok(view.iter().cloned().collect())
        }
        3 => {
            let seq_dim = shape[1];
            let hidden = shape[2];
            let mut out = vec![0.0f32; hidden];
            let mut denom = 0.0f32;
            for i in 0..seq_dim {
                let weight = attention_mask.and_then(|m| m.get(i)).cloned().unwrap_or(1) as f32;
                if weight == 0.0 {
                    continue;
                }
                denom += weight;
                for j in 0..hidden {
                    out[j] += array[[0, i, j]] * weight;
                }
            }
            if denom > 0.0 {
                for v in &mut out {
                    *v /= denom;
                }
            }
            Ok(out)
        }
        _ => Err(AppError::Media(format!(
            "Unexpected embedding shape: {:?}",
            shape
        ))),
    }
}

fn l2_normalize(vector: &mut [f32]) {
    let mut norm = 0.0f32;
    for v in vector.iter() {
        norm += v * v;
    }
    if norm > 0.0 {
        let inv = 1.0 / norm.sqrt();
        for v in vector.iter_mut() {
            *v *= inv;
        }
    }
}

fn compute_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let digest = hasher.finalize();
    Ok(format!("{:x}", digest))
}

fn hash_embed(text: &str, dims: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(dims);
    let mut counter = 0u64;
    while out.len() < dims {
        let mut hasher = Hasher::new();
        hasher.update(text.as_bytes());
        hasher.update(&counter.to_le_bytes());
        let digest = hasher.finalize();
        for chunk in digest.as_bytes().chunks(4) {
            if out.len() >= dims {
                break;
            }
            let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
            let value = u32::from_le_bytes(bytes);
            let norm = (value as f32 / u32::MAX as f32) * 2.0 - 1.0;
            out.push(norm);
        }
        counter = counter.wrapping_add(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_embed_is_deterministic() {
        let a = hash_embed("hello", 8);
        let b = hash_embed("hello", 8);
        let c = hash_embed("world", 8);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 8);
    }
}
