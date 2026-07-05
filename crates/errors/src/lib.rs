//! Unified error types for SMS archive

use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Parse error at offset {offset}: {details}")]
    Parse { offset: u64, details: String },

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Insufficient disk space: need {needed} GB, have {available} GB")]
    InsufficientDisk { needed: u64, available: u64 },

    #[error("Unsupported encoding: {0}")]
    UnsupportedEncoding(String),

    #[error("Checkpoint corrupted")]
    CheckpointCorrupted,

    #[error("Import cancelled by user")]
    Cancelled,

    #[error("Import skipped current file")]
    SkippedFile,

    #[error("FTS5 unavailable in SQLite build")]
    Fts5Unavailable,

    #[error("Media error: {0}")]
    Media(String),

    #[error("Channel error: {0}")]
    Channel(String),

    #[error("Serde error: {0}")]
    Serde(String),

    #[error("Search backend unsupported: {0}")]
    SearchUnsupported(String),

    #[error("External service error: {0}")]
    External(String),
}

pub type Result<T> = std::result::Result<T, AppError>;
