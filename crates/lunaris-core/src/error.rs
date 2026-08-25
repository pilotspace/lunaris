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

macro_rules! subsystems {
    ($($variant:ident => $label:literal / $code:literal / $sample:expr),+ $(,)?) => {
        /// Coarse subsystem tag for a [`LunarisError`] — one classifying match, inside
        /// the crate that owns the enum.
        ///
        /// Four places classified `LunarisError` by variant independently: the
        /// Prometheus `kind` label, the HTTP status map, and the Python and TypeScript
        /// SDK error codes. Every one ended in a wildcard arm — not by carelessness,
        /// but because `LunarisError` is `#[non_exhaustive]` and a *downstream* crate
        /// has no choice. So the compiler could never flag a new variant going
        /// unclassified, and when `Scope` was added all four silently began reporting
        /// it as unknown. Two of them carried a claim of totality: a comment reading
        /// "New variants in the future MUST extend this match", and a test named
        /// `error_kind_maps_every_lunaris_error_variant`. Writing the instruction down
        /// did not make the next variant obey it.
        ///
        /// `#[non_exhaustive]` does not apply inside the defining crate, so the match
        /// in [`LunarisError::subsystem`] is exhaustiveness-checked for real: adding a
        /// variant without tagging it fails to compile *here*. Consumers still write
        /// their wildcard, but they can now walk [`Subsystem::ALL`] to prove their own
        /// map is total — turning a silent "unknown" into a failing test.
        ///
        /// ```
        /// # use lunaris_core::{LunarisError, ScopeError, Subsystem};
        /// let err = LunarisError::Scope(ScopeError::Invalid("bad:scope".into()));
        /// assert_eq!(err.subsystem(), Subsystem::Scope);
        /// assert_eq!(err.subsystem().label(), "scope");  // metrics / HTTP envelope
        /// assert_eq!(err.subsystem().code(), "SCOPE");   // Python + TypeScript SDKs
        /// ```
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum Subsystem { $($variant),+ }

        impl Subsystem {
            /// Every subsystem, in declaration order. Generated from the same
            /// list as [`LunarisError::subsystem`], so it cannot fall behind
            /// the enum the way a hand-written list does.
            pub const ALL: &'static [Subsystem] = &[$(Subsystem::$variant),+];

            /// Lowercase tag — the Prometheus `kind` label and the `error`
            /// field of the server's JSON envelope.
            pub const fn label(self) -> &'static str {
                match self { $(Subsystem::$variant => $label),+ }
            }

            /// Uppercase tag — the `code` the Python and TypeScript SDKs put
            /// in front of the message so callers can branch without parsing.
            pub const fn code(self) -> &'static str {
                match self { $(Subsystem::$variant => $code),+ }
            }

            /// A representative error of this subsystem.
            ///
            /// Exists so a consumer that maps `LunarisError` to something of
            /// its own — an HTTP status, an SDK code — can walk [`Self::ALL`]
            /// and feed its mapper a real value for every subsystem, instead
            /// of hand-listing the ones whoever wrote the test remembered.
            /// Every such mapper needs a wildcard arm it cannot delete, so
            /// this is the only way to prove the arm is unreachable.
            pub fn sample_error(self) -> LunarisError {
                match self { $(Subsystem::$variant => LunarisError::$variant($sample)),+ }
            }
        }

        impl LunarisError {
            /// The coarse subsystem this error came from.
            ///
            /// Prefer this over matching the variant yourself: a downstream
            /// match needs a wildcard arm and will silently swallow whatever
            /// gets added next.
            pub fn subsystem(&self) -> Subsystem {
                match self { $(LunarisError::$variant(_) => Subsystem::$variant),+ }
            }
        }
    };
}

subsystems! {
    Storage     => "storage"     / "STORAGE"     / StorageError::Backend("sample".into()),
    Extract     => "extract"     / "EXTRACT"     / ExtractError::Backend("sample".into()),
    Validate    => "validate"    / "VALIDATE"    / ValidateError::Temporal,
    Retrieve    => "retrieve"    / "RETRIEVE"    / RetrieveError::Backend("sample".into()),
    Consolidate => "consolidate" / "CONSOLIDATE" / ConsolError::Backend("sample".into()),
    Scope       => "scope"       / "SCOPE"       / crate::ScopeError::Invalid("sample".into()),
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

#[cfg(test)]
mod subsystem_tests {
    use super::*;

    /// The point of the whole type: a variant that reaches a consumer must
    /// already have a tag. This match is exhaustive *inside* the defining
    /// crate, so adding a `LunarisError` variant breaks this test's
    /// compilation — which is the only signal that reliably survives, since
    /// every downstream match is forced to carry a wildcard.
    #[test]
    fn every_error_variant_has_a_tag() {
        let cases: Vec<LunarisError> = vec![
            StorageError::Backend("x".into()).into(),
            ExtractError::Backend("x".into()).into(),
            ValidateError::Temporal.into(),
            RetrieveError::Backend("x".into()).into(),
            ConsolError::Backend("x".into()).into(),
            crate::ScopeError::Invalid("bad:scope".into()).into(),
        ];
        for err in &cases {
            let sub = err.subsystem();
            assert!(Subsystem::ALL.contains(&sub), "{sub:?} is not in Subsystem::ALL");
        }
        assert_eq!(
            cases.len(),
            Subsystem::ALL.len(),
            "this list and Subsystem::ALL disagree — a variant is untested or untagged"
        );

        // `sample_error` must round-trip: the error it hands back for a
        // subsystem has to classify back to that same subsystem, or a
        // consumer walking ALL would be testing the wrong arm.
        for sub in Subsystem::ALL {
            assert_eq!(sub.sample_error().subsystem(), *sub, "{sub:?} sample_error round-trip");
        }
    }

    /// A copy-paste in the macro invocation would give two subsystems the same
    /// tag, which reads as one subsystem in a metrics dashboard and in an SDK
    /// caller's `if code == ...`. Duplicate arms in `label`/`code` are only a
    /// warning, so assert it.
    #[test]
    fn tags_are_distinct_and_consistently_cased() {
        let labels: std::collections::HashSet<_> =
            Subsystem::ALL.iter().map(|s| s.label()).collect();
        let codes: std::collections::HashSet<_> = Subsystem::ALL.iter().map(|s| s.code()).collect();
        assert_eq!(labels.len(), Subsystem::ALL.len(), "duplicate label");
        assert_eq!(codes.len(), Subsystem::ALL.len(), "duplicate code");
        for s in Subsystem::ALL {
            assert_eq!(
                s.code(),
                s.label().to_ascii_uppercase(),
                "{s:?}: code and label must be the same word, cased for their audience"
            );
            assert_ne!(s.label(), "unknown", "\"unknown\" is the wildcard's answer, not a tag");
        }
    }
}
