//! Error taxonomy — one umbrella enum, one sub-enum per subsystem.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LunarisError {
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("extract: {0}")]
    Extract(#[from] ExtractError),
    #[error("validate: {0}")]
    Validate(#[from] ValidateError),
    #[error("retrieve: {0}")]
    Retrieve(#[from] RetrieveError),
    #[error("consolidate: {0}")]
    Consolidate(#[from] ConsolError),
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("backend: {0}")]
    Backend(String),
    #[error("not supported: {0}")]
    NotSupported(&'static str),
    #[error("unsupported scheme: {0}")]
    UnsupportedScheme(String),
    #[error("serialization: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("model timeout")]
    Timeout,
    #[error("grammar reject: {0}")]
    GrammarReject(String),
    #[error("backend: {0}")]
    Backend(String),
}

#[derive(Debug, Error)]
pub enum ValidateError {
    #[error("temporal: valid_from >= valid_to")]
    Temporal,
    #[error("contradiction: {0}")]
    Contradiction(String),
}

#[derive(Debug, Error)]
pub enum RetrieveError {
    #[error("operator failed: {0}")]
    OperatorFailed(String),
    #[error("backend: {0}")]
    Backend(String),
}

#[derive(Debug, Error)]
pub enum ConsolError {
    #[error("activation underflow")]
    ActivationUnderflow,
    #[error("backend: {0}")]
    Backend(String),
}
