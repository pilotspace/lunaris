//! W4.6 / D6.4 — per-scope retention policy.
//!
//! The policy is data, not behaviour: this module defines the shape and the KV
//! location, and `lunaris::retention` enforces it. Keeping the type here means
//! the storage backends and the SDKs can read and write a policy without
//! depending on the engine crate.
//!
//! ## Why enforcement reuses `forget` rather than deleting directly
//!
//! The D6 decision flagged one interaction by name: `forget` soft-deletes by
//! default, and "retention that hard-deletes must not silently change what
//! `.hard()` means". Enforcement therefore goes through `ForgetTarget::Before`
//! on the ordinary scoped forget path, so soft/hard semantics, the chunk
//! sweep, the single-`atomic_write` invariant, and the audit receipt are the
//! same ones a human `forget` gets. A retention sweep that reached past
//! `forget` into `atomic_write` would be a second, quieter definition of
//! delete.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

use crate::scope::Scope;

/// KV key holding `scope`'s retention policy: `lunaris:{scope}:retention`.
///
/// Minted here rather than in a caller for the reason CONVENTIONS records:
/// any caller that builds a Lunaris KV key from a local helper is how
/// collision-prone keys get written (RC-1).
#[inline]
pub fn retention_policy_key(scope: &Scope) -> Vec<u8> {
    format!("{}retention", crate::keyspace::scope_prefix(scope)).into_bytes()
}

/// A scope's retention policy.
///
/// Absent policy means "keep everything" — retention is opt-in per scope, and
/// a scope with no policy is never swept. That default is deliberate: the
/// failure mode of an accidentally-applied retention policy is unrecoverable
/// data loss, and the failure mode of an accidentally-absent one is disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RetentionPolicy {
    /// Rows whose valid-time start is older than `now - max_age_ms` are
    /// eligible. Milliseconds.
    pub max_age_ms: u64,

    /// `false` (the default) soft-deletes, matching `forget`'s default: the
    /// row stays and is hidden from recall by the hydrate sys-gate, so a
    /// mistaken policy is recoverable.
    ///
    /// `true` hard-deletes. Enforcement still obtains a confirmation token the
    /// way a human would — run the preview, derive the token from THAT
    /// receipt — so the D-21 safety rail keeps meaning what it means; the
    /// policy is the standing authorization, not a bypass.
    #[serde(default)]
    pub hard: bool,
}

impl RetentionPolicy {
    /// A soft-delete policy with the given maximum age.
    pub fn max_age_ms(max_age_ms: u64) -> Self {
        Self { max_age_ms, hard: false }
    }

    /// Make this policy hard-delete. See [`RetentionPolicy::hard`].
    pub fn hard(mut self) -> Self {
        self.hard = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_key_is_scope_prefixed() {
        let a = Scope::new("tenant-a").unwrap();
        let b = Scope::new("tenant-b").unwrap();
        assert_eq!(retention_policy_key(&a), b"lunaris:tenant-a:retention".to_vec());
        assert_ne!(retention_policy_key(&a), retention_policy_key(&b));
    }

    #[test]
    fn a_policy_round_trips_and_rejects_unknown_fields() {
        let p = RetentionPolicy::max_age_ms(86_400_000).hard();
        let bytes = serde_json::to_vec(&p).unwrap();
        assert_eq!(serde_json::from_slice::<RetentionPolicy>(&bytes).unwrap(), p);

        // `hard` defaults, so an older policy without it still reads.
        let older: RetentionPolicy = serde_json::from_str(r#"{"max_age_ms":1}"#).unwrap();
        assert_eq!(older, RetentionPolicy { max_age_ms: 1, hard: false });

        // A typo'd field is an error, not a silently-ignored setting — a
        // policy that reads as "keep everything" because `max_age_ms` was
        // spelled `maxAgeMs` is the worst possible failure here.
        assert!(serde_json::from_str::<RetentionPolicy>(r#"{"maxAgeMs":1}"#).is_err());
    }
}
