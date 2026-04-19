//! Mission statement: Provide a unified CLIP image encoding pipeline that produces visual embeddings
//! and NSFW scores in a single forward pass, enabling fast media search, clustering, and moderation
//! across the SMS Archive project with a GPU-first design and reliable CPU fallback.

mod encoder;
mod nsfw;
mod preprocessing;

pub use encoder::{probe_cuda_support, ClipEncoder, ClipResult, ClipStats};
pub use nsfw::{NsfwProbe, NsfwScore};
pub use preprocessing::{clip_preprocess_batch, ClipPreprocessConfig};
