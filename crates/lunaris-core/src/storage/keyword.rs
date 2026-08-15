//! Keyword (BM25) port — extension trait. NOT part of the locked
//! [`StoragePort`](super::port::StoragePort) trait because (a) Phase 1 froze
//! that trait shape per blueprint §6 (`STORE-01`), and (b) some third-party
//! storage impls may not have a BM25 path. Backends that DO support keyword
//! search opt in by implementing this extension trait.
//!
//! Phase 2 (`RETRIEVE-02`) wires `MoonStorage` (`FT.SEARCH ... SCORER BM25`
//! via `moon-client`'s `text().search`). Since 0.7.0 that is the only in-tree
//! implementor — the `tsvector` + `ts_rank_cd` Postgres impl was deleted with
//! its backend — but the trait stays separate from `StoragePort` on purpose:
//! it is the seam a third-party store with no BM25 path declines. The umbrella
//! `lunaris::Lunaris` handle stores `Arc<dyn KeywordPort>` alongside
//! `Arc<dyn StoragePort>` (both point at the same backend struct via two trait
//! Arcs — no duplicate connection).
//!
//! ## Score normalization contract
//!
//! `KeywordHit.score` MUST be in `[0.0, 1.0]` after min-max normalization
//! within the call's result set. Backends populate `raw_score` with the
//! native BM25 value for callers that want the unscaled signal.
//! Min-max normalization is per-call (NOT global) — this matches the Phase 2
//! `fuse_rrf` operator's per-branch ranking convention.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::types::Filter;
use crate::error::StorageError;
use crate::hlc::Hlc;

/// BM25 keyword search extension trait.
///
/// Backends opt in by implementing this trait alongside [`StoragePort`](super::port::StoragePort).
/// The Phase 2 [`Keyword::bm25`](https://docs.rs/lunaris-retrieve) operator takes
/// `Arc<dyn KeywordPort>` and dispatches to the backend.
///
/// ## RFC 0001 §3.4 amendment (Wave 2.5A)
///
/// `keyword_search` gains `scope: &Scope` as its first argument after `&self`.
/// Wave 0 froze `StoragePort` with `&Scope` on 8 methods but missed this trait.
/// BM25 keyword search was therefore not scope-isolated at the trait level.
/// Wave 2.5A closes the gap — backends thread `scope` through to the underlying
/// FT index routing (Moon).
#[async_trait]
pub trait KeywordPort: Send + Sync + 'static {
    /// BM25 keyword search.
    ///
    /// * `scope` — the agent/tenant scope; backends MUST restrict results to
    ///   this scope only (RFC 0001 §3.4 amendment, Wave 2.5A).
    /// * `index` is the table / FT index name (e.g., `"chunks"`).
    /// * `query` is the user-supplied query text. Backends MUST escape any
    ///   index-DSL specials (FT escape on Moon).
    /// * `k` is the maximum number of hits to return.
    /// * `filter` narrows the candidate set BEFORE ranking (not after).
    /// * `as_of` is an MVCC bi-temporal snapshot timestamp; `None` means "live".
    ///
    /// Returns hits sorted by descending score. Backends MUST normalize
    /// scores to `[0.0, 1.0]` via per-call min-max within the result set,
    /// preserving the original score in `raw_score`.
    async fn keyword_search(
        &self,
        scope: &crate::scope::Scope,
        index: &str,
        query: &str,
        k: usize,
        filter: Option<&Filter>,
        as_of: Option<Hlc>,
    ) -> Result<Vec<KeywordHit>, StorageError>;
}

/// One keyword search hit.
///
/// `score` is min-max normalized to `[0.0, 1.0]` within the call's result set.
/// `raw_score` carries the backend-native value (raw BM25 on Moon).
/// `metadata` carries the row payload (the `__metadata` field on Moon).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeywordHit {
    pub id: Vec<u8>,
    pub score: f32,
    pub raw_score: f32,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl KeywordHit {
    /// Construct a new hit with explicit raw + normalized scores. Used by
    /// backend impls AND by the test fixtures in `lunaris-retrieve`.
    pub fn new(id: Vec<u8>, score: f32, raw_score: f32, metadata: serde_json::Value) -> Self {
        Self { id, score, raw_score, metadata }
    }
}

/// Min-max normalize a slice of raw scores into `[0.0, 1.0]`.
///
/// When `max == min` (single-hit result OR all hits tied), returns `1.0` for
/// every entry — this preserves the tie semantics and keeps the result valid
/// for downstream RRF fusion.
///
/// Returns a `Vec<f32>` of the same length as `raw`.
pub fn min_max_normalize(raw: &[f32]) -> Vec<f32> {
    if raw.is_empty() {
        return Vec::new();
    }
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for &v in raw {
        if v < min {
            min = v;
        }
        if v > max {
            max = v;
        }
    }
    let span = max - min;
    if span <= f32::EPSILON {
        return vec![1.0; raw.len()];
    }
    raw.iter().map(|&v| (v - min) / span).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_max_empty() {
        assert!(min_max_normalize(&[]).is_empty());
    }

    #[test]
    fn min_max_single_returns_one() {
        let n = min_max_normalize(&[3.5]);
        assert_eq!(n, vec![1.0]);
    }

    #[test]
    fn min_max_tie_returns_one() {
        let n = min_max_normalize(&[2.0, 2.0, 2.0]);
        assert_eq!(n, vec![1.0, 1.0, 1.0]);
    }

    #[test]
    fn min_max_normal_span() {
        // raw = [1.0, 3.0, 5.0]; min=1, max=5, span=4.
        // normalized = [0.0, 0.5, 1.0]
        let n = min_max_normalize(&[1.0, 3.0, 5.0]);
        assert!((n[0] - 0.0).abs() < 1e-6);
        assert!((n[1] - 0.5).abs() < 1e-6);
        assert!((n[2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn keyword_hit_roundtrips_via_serde() {
        let h = KeywordHit::new(b"abc".to_vec(), 0.42, 7.1, serde_json::json!({"text": "hello"}));
        let s = serde_json::to_string(&h).unwrap();
        let back: KeywordHit = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, b"abc".to_vec());
        assert!((back.score - 0.42).abs() < 1e-6);
        assert!((back.raw_score - 7.1).abs() < 1e-4);
    }
}
