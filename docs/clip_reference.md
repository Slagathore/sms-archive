# Mission statement

This document describes the sms-clip crate, which provides a unified CLIP image encoder and NSFW probe pipeline for fast media embeddings and content moderation.

## Module overview

- crates/clip/src/lib.rs: Public API re-exports for CLIP encoding and preprocessing utilities.
- crates/clip/src/encoder.rs: CLIP ONNX session wrapper and batch inference pipeline.
- crates/clip/src/preprocessing.rs: CLIP-specific image preprocessing (resize/crop/normalize).
- crates/clip/src/nsfw.rs: Linear probe NSFW classifier loading and scoring.

## Modules and classes used

- ort::session::Session: ONNX Runtime session for CLIP inference.
- ort::value::Tensor: ONNX input tensor wrapper.
- image::DynamicImage: Image container for frames.
- ndarray::{Array1, Array2, Array4}: Tensor/weight storage.
- safetensors::SafeTensors: NSFW probe weights loader.
- ndarray_npy::NpzReader: NPZ probe loader fallback.
- sms_errors::{AppError, Result}: Error propagation.

## Public structs and functions

- ClipEncoder
  - session: ONNX Runtime session used for CLIP inference.
  - input_name: CLIP input tensor name.
  - output_name: CLIP output tensor name.
  - preprocess: ClipPreprocessConfig used for resize/normalize.
  - probe: NsfwProbe with linear weights/bias.
  - model_path: Path to the CLIP ONNX model.
  - new(): Creates a CLIP encoder and loads the NSFW probe.
  - encode_batch(): Encodes a batch of images, returns embeddings + NSFW scores.
  - model_path(): Returns the CLIP model path.

- ClipResult
  - embedding: 768-dim CLIP embedding (normalized).
  - nsfw_label: "SAFE" or "NSFW".
  - nsfw_score: NSFW probability score.

- ClipStats
  - batch_size: Number of images processed.
  - dims: Embedding dimension.
  - elapsed_ms: Inference duration.

- ClipPreprocessConfig
  - target_size: Output resolution (default 336).

- clip_preprocess_batch(): Converts images to CLIP input tensors.

- NsfwProbe
  - weights: Linear probe weights.
  - bias: Linear probe bias.
  - load(): Loads from safetensors or npz.
  - score(): Returns NSFW label + score for an embedding.

- NsfwScore
  - label: "SAFE" or "NSFW".
  - score: Probability score.
  - as_tuple(): Convenience conversion.

## Variables and constants

- CLIP_MEAN / CLIP_STD: Normalization constants used by CLIP preprocessing.

## #todo

- #todo: Add CLIP input size auto-detection from model metadata.
- #todo: Add optional half-precision (FP16) inference for CUDA.
- #todo: Add multi-model support for alternative CLIP variants.
