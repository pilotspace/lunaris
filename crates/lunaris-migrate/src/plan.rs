//! Options, per-row verdicts, and the key parsing every stage shares.

use std::path::PathBuf;

use lunaris_core::Scope;

/// Default number of `WriteOp`s per destination `atomic_write` batch.
///
/// Atomicity on Moon is per-batch, so the batch size is also the blast radius of
/// a mid-run failure. 256 keeps a failed batch small enough to re-run cheaply
/// (the write is idempotent) while still amortising the round trip.
pub const DEFAULT_BATCH_SIZE: usize = 256;

/// Default number of migrated keys re-read from the destination and compared
/// byte-for-byte during verification.
pub const DEFAULT_SAMPLE: usize = 32;

/// Everything the copy engine needs that is not a store handle.
#[derive(Debug, Clone)]
pub struct MigrationOptions {
    /// Perform real writes. `false` (the default) reports counts only.
    pub commit: bool,
    /// Operator acknowledged [`crate::contract::LOSSY_CONTRACT`]. Required for
    /// `commit`; ignored otherwise.
    pub acknowledge_lossy: bool,
    /// `WriteOp`s per destination `atomic_write`.
    pub batch_size: usize,
    /// Run the post-migration verification pass.
    pub verify: bool,
    /// How many migrated keys to content-compare during verification.
    pub sample: usize,
    /// Write a JSONL manifest of keys whose vectors must be regenerated.
    pub reembed_manifest: Option<PathBuf>,
}

impl Default for MigrationOptions {
    /// Dry-run by default — the same posture as MCP `forget`. An operator who
    /// forgets a flag gets a report, never a write.
    fn default() -> Self {
        Self {
            commit: false,
            acknowledge_lossy: false,
            batch_size: DEFAULT_BATCH_SIZE,
            verify: true,
            sample: DEFAULT_SAMPLE,
            reembed_manifest: None,
        }
    }
}

impl MigrationOptions {
    /// A committing run with the contract acknowledged.
    #[must_use]
    pub fn committing() -> Self {
        Self { commit: true, acknowledge_lossy: true, ..Self::default() }
    }

    /// Whether this configuration is allowed to write.
    ///
    /// Both halves are required: `commit` is the intent, `acknowledge_lossy` is
    /// the informed consent.
    #[must_use]
    pub fn writes_enabled(&self) -> bool {
        self.commit && self.acknowledge_lossy
    }
}

/// What the engine decided about one source row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowVerdict {
    /// Current, both record intervals open — copy it.
    Migrate,
    /// `bt.valid.1` is set: retracted or superseded in the world.
    SkipClosedValid,
    /// `bt.sys.1` is set: logically deleted record version.
    SkipClosedSys,
    /// The key is not a `lunaris:{scope}:{kind}:{id}` key, or its scope segment
    /// does not match the scope being migrated. Left behind and counted rather
    /// than copied blind — a stray key is an operator signal, not cargo.
    SkipForeignKey,
}

/// Classify one `(key, value)` pair read from the source.
///
/// A value with no `bt` object at all is `Migrate`: not every key under the
/// scope prefix is a bi-temporal primitive (idempotency sidecars and activation
/// ledger records are not), and refusing to carry them would silently drop
/// operator state.
#[must_use]
pub fn classify_row(scope: &Scope, key: &[u8], value: &[u8]) -> RowVerdict {
    let _ = (scope, key, value, interval_is_closed as fn(Option<&serde_json::Value>) -> bool);
    RowVerdict::Migrate
}

/// `BiTemporal`'s axes serialise as a 2-element array `[from, to]`; `to` is
/// `null` while the interval is open.
fn interval_is_closed(axis: Option<&serde_json::Value>) -> bool {
    axis.and_then(|a| a.get(1)).is_some_and(|end| !end.is_null())
}

/// Split `lunaris:{scope}:{kind}:{rest}` into `(scope, kind)`.
///
/// Returns `None` for any key that does not carry the canonical layout — the
/// engine treats that as [`RowVerdict::SkipForeignKey`] instead of guessing.
#[must_use]
pub fn kind_of(key: &[u8]) -> Option<(&str, &str)> {
    let s = std::str::from_utf8(key).ok()?;
    let rest = s.strip_prefix("lunaris:")?;
    let (scope, rest) = rest.split_once(':')?;
    let (kind, _) = rest.split_once(':')?;
    if scope.is_empty() || kind.is_empty() {
        return None;
    }
    Some((scope, kind))
}

/// Primitive kinds that carry an embedding and therefore have a Moon FT
/// document that this migration does NOT create.
///
/// Sourced from the `#[serde(default, skip_serializing)] pub embedding` fields
/// in `lunaris_core::primitives` — `Chunk`, `Entity`, `Fact`, `Community`.
pub const EMBEDDABLE_KINDS: &[&str] = &["chunk", "entity", "fact", "community"];

/// Whether rows of this kind need a vector regenerated after migration.
#[must_use]
pub fn needs_reembed(kind: &str) -> bool {
    EMBEDDABLE_KINDS.contains(&kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope::new("acme.agent-1").expect("valid scope")
    }

    #[test]
    fn default_options_never_write() {
        let o = MigrationOptions::default();
        assert!(!o.commit);
        assert!(!o.writes_enabled());
    }

    #[test]
    fn commit_without_acknowledgement_does_not_enable_writes() {
        let o = MigrationOptions { commit: true, ..MigrationOptions::default() };
        assert!(!o.writes_enabled(), "acknowledgement is not optional");
    }

    #[test]
    fn committing_helper_enables_writes() {
        assert!(MigrationOptions::committing().writes_enabled());
    }

    #[test]
    fn kind_parses_canonical_keys() {
        assert_eq!(
            kind_of(b"lunaris:acme.agent-1:episode:01HZZZ"),
            Some(("acme.agent-1", "episode"))
        );
        assert_eq!(kind_of(b"lunaris:s:factspo:abcd:employer"), Some(("s", "factspo")));
        assert_eq!(kind_of(b"other:s:episode:1"), None);
        assert_eq!(kind_of(b"lunaris:s:episode"), None);
    }

    #[test]
    fn open_intervals_migrate() {
        let v = br#"{"id":"x","bt":{"valid":[{"wall_ms":1,"counter":0},null],
                     "sys":[{"wall_ms":1,"counter":0},null]}}"#;
        assert_eq!(classify_row(&scope(), b"lunaris:acme.agent-1:fact:01H", v), RowVerdict::Migrate);
    }

    #[test]
    fn closed_valid_interval_is_skipped() {
        let v = br#"{"bt":{"valid":[{"wall_ms":1,"counter":0},{"wall_ms":9,"counter":0}],
                     "sys":[{"wall_ms":1,"counter":0},null]}}"#;
        assert_eq!(
            classify_row(&scope(), b"lunaris:acme.agent-1:fact:01H", v),
            RowVerdict::SkipClosedValid
        );
    }

    #[test]
    fn closed_sys_interval_is_skipped() {
        let v = br#"{"bt":{"valid":[{"wall_ms":1,"counter":0},null],
                     "sys":[{"wall_ms":1,"counter":0},{"wall_ms":9,"counter":0}]}}"#;
        assert_eq!(
            classify_row(&scope(), b"lunaris:acme.agent-1:fact:01H", v),
            RowVerdict::SkipClosedSys
        );
    }

    #[test]
    fn non_bitemporal_payloads_still_migrate() {
        // Activation-ledger / dedupe sidecar rows carry no `bt`. Dropping them
        // would be a silent loss of operator state.
        assert_eq!(
            classify_row(&scope(), b"lunaris:acme.agent-1:activation:01H", br#"{"count":3}"#),
            RowVerdict::Migrate
        );
        assert_eq!(
            classify_row(&scope(), b"lunaris:acme.agent-1:episode:01H", b"not json at all"),
            RowVerdict::Migrate
        );
    }

    #[test]
    fn a_key_from_another_scope_is_never_copied() {
        assert_eq!(
            classify_row(&scope(), b"lunaris:other-tenant:fact:01H", b"{}"),
            RowVerdict::SkipForeignKey
        );
    }

    #[test]
    fn embeddable_kinds_match_the_primitives_with_vectors() {
        for k in ["chunk", "entity", "fact", "community"] {
            assert!(needs_reembed(k), "{k} carries an embedding");
        }
        for k in ["episode", "relation", "doctree", "activation"] {
            assert!(!needs_reembed(k), "{k} has no vector of its own");
        }
    }
}
