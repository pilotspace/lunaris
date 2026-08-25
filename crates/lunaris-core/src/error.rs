//! Error taxonomy — one umbrella enum, one sub-enum per subsystem.

use thiserror::Error;

/// Top-level error type returned by every public `lunaris` API.
///
/// `#[non_exhaustive]` lets us add new top-level subsystems (e.g. a
/// future `Verify(VerifyError)` variant) in a patch release without
/// breaking downstream `match` exhaustiveness checks. Downstream code
/// should always include a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
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
    /// A partition key was rejected by [`crate::Scope::new`].
    ///
    /// Added because `Result<_, LunarisError>` is the natural signature for a
    /// function that talks to Lunaris, and naming a partition inside one is the
    /// second thing every caller does:
    ///
    /// ```
    /// # use lunaris_core::{LunarisError, Scope};
    /// fn open_a_partition(name: &str) -> Result<Scope, LunarisError> {
    ///     Ok(Scope::new(name)?)
    /// }
    /// ```
    ///
    /// Without this variant that `?` is E0277 and every caller has to write a
    /// `map_err` that throws the reason away. Ten cookbook pages taught the
    /// pattern above before anything compiled them (W4.18).
    ///
    /// Kept as its own variant rather than folded into a stringly one: a scope
    /// typo and a storage outage are different problems and a reader triaging a
    /// log should not have to tell them apart by message text.
    #[error("scope: {0}")]
    Scope(#[from] crate::ScopeError),
}

#[derive(Debug, Error)]
#[non_exhaustive]
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
#[non_exhaustive]
pub enum ExtractError {
    #[error("model timeout")]
    Timeout,
    #[error("grammar reject: {0}")]
    GrammarReject(String),
    #[error("backend: {0}")]
    Backend(String),
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ValidateError {
    #[error("temporal: valid_from >= valid_to")]
    Temporal,
    #[error("contradiction: {0}")]
    Contradiction(String),
    /// B-3 fix (Plan 04-05): hard-delete in `Lunaris::forget(target.hard())`
    /// requires a confirmation token from a prior `dry_run` +
    /// `confirm_hard_forget` round-trip. Returned when caller invokes
    /// `.hard()` without `.with_token(...)` (D-21 safety rail).
    #[error("confirmation required: {0}")]
    ConfirmationRequired(String),
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RetrieveError {
    #[error("operator failed: {0}")]
    OperatorFailed(String),
    #[error("backend: {0}")]
    Backend(String),
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConsolError {
    #[error("activation underflow")]
    ActivationUnderflow,
    #[error("backend: {0}")]
    Backend(String),
}
