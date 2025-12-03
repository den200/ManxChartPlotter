use std::path::PathBuf;

use thiserror::Error;

/// Result type used across the decryption module.
pub type DecryptResult<T> = Result<T, DecryptError>;

/// All possible errors emitted while driving `oexserverd` or parsing key files.
#[derive(Debug, Error)]
pub enum DecryptError {
    #[error("oexserverd binary not found (searched: {0:?})")]
    BinaryNotFound(Vec<PathBuf>),

    #[error("fingerprint file not found: {0}")]
    MissingFpr(PathBuf),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Timeout while waiting for {operation}")]
    Timeout { operation: &'static str },

    #[error("Protocol violation: {0}")]
    Protocol(String),

    #[error("oexserverd exited unexpectedly: {0}")]
    Process(String),

    #[error("No install key available for chart {0}")]
    MissingKey(String),

    #[error("XML parse error: {0}")]
    Xml(String),
}

impl From<quick_xml::Error> for DecryptError {
    fn from(value: quick_xml::Error) -> Self {
        Self::Xml(value.to_string())
    }
}
