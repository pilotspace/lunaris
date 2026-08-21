//! F11 — a per-invocation partition for the storage conformance suite.
//!
//! The suite used to write fixed keys under [`Scope::dev()`], which made it
//! green exactly once per store. `read_as_of::snapshot` asserts its key is
//! absent "before any write"; on the second run the first run's value was
//! still there, and the suite reported a contract violation that had not
//! happened. CI never noticed (fresh Moon per job) and a developer running it
//! twice got a failure that reads like a real backend bug.
//!
//! ## Why the scope alone is not enough
//!
//! Moon namespaces the vector and graph legs by scope on its own — the FT
//! index is `ft_index_name(scope, kind)` and the graph is `graph_key(scope)`.
//! It does **not** namespace KV: `atomic.rs::run_ops` writes `KvPut` keys
//! verbatim, because the documented contract is that the caller already
//! scope-prefixed them (`lunaris_core::keyspace::*`). The conformance suite
//! never did, which is a real violation of that contract and the reason a new
//! scope by itself would have changed nothing for `read_as_of`.
//!
//! So this type owns both halves: a fresh scope for the backend-namespaced
//! legs, and [`SuiteScope::key`] for the KV keys the backend takes literally.
//! Passing it by reference — rather than reading a process-global — is what
//! lets `tests/storage_suite_is_rerunnable.rs` run the whole suite twice in
//! one process and prove each invocation partitioned itself.

use lunaris_core::Scope;

/// A fresh, self-contained partition of the store for one suite invocation.
///
/// Construct once per `run_full_storage_suite` call and thread it down. Never
/// cache one in a `static` — a process-global would make the re-runnability
/// test pass for the wrong reason (one process, one partition, second run
/// colliding exactly as before).
#[derive(Debug, Clone)]
pub struct SuiteScope {
    scope: Scope,
    label: String,
}

impl SuiteScope {
    /// Mint a partition nothing else can be using.
    ///
    /// The label is `conf-<ULID>`: ULID's Crockford alphabet is `[0-9A-Z]`, a
    /// strict subset of the scope alphabet `[A-Za-z0-9_\-.]{1,128}`, so
    /// `Scope::new` cannot reject it and the `expect` below is unreachable
    /// rather than optimistic.
    pub fn fresh() -> Self {
        let label = format!("conf-{}", ulid::Ulid::new());
        let scope = Scope::new(&label).expect("ULID label is inside the scope alphabet");
        Self { scope, label }
    }

    /// The partition key for the backend-namespaced legs (vector, graph).
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// A KV key nothing outside this invocation writes.
    ///
    /// Moon stores `KvPut` keys verbatim, so uniqueness has to be in the bytes
    /// — putting the run label in the scope argument alone would leave every
    /// invocation fighting over `conformance:read_as_of:k1`.
    pub fn key(&self, suffix: &str) -> Vec<u8> {
        format!("conformance:{}:{suffix}", self.label).into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::SuiteScope;

    /// The whole contract in one line: two partitions never overlap. A version
    /// that returned a constant would satisfy every *shape* assertion in this
    /// file, so this is the one that has to exist.
    #[test]
    fn two_partitions_share_no_scope_and_no_key() {
        let a = SuiteScope::fresh();
        let b = SuiteScope::fresh();
        assert_ne!(a.scope().as_str(), b.scope().as_str(), "scopes collided");
        assert_ne!(a.key("read_as_of:k1"), b.key("read_as_of:k1"), "KV keys collided");
    }

    /// `Scope::new` validates; if the label alphabet ever drifts, fail here
    /// rather than in an `expect` inside a live-Moon run.
    #[test]
    fn the_minted_label_is_a_legal_scope() {
        let s = SuiteScope::fresh();
        assert!(s.scope().as_str().starts_with("conf-"));
        assert!(
            lunaris_core::Scope::new(s.scope().as_str()).is_ok(),
            "SuiteScope minted a label Scope::new rejects"
        );
    }

    /// Keys must carry the run label, not just the suffix — otherwise every
    /// invocation writes the same bytes and the scope is decorative.
    #[test]
    fn a_key_carries_the_run_label() {
        let s = SuiteScope::fresh();
        let key = String::from_utf8(s.key("aw:k1")).expect("utf-8");
        assert!(key.contains(s.scope().as_str()), "key {key} does not carry the run label");
        assert!(key.ends_with("aw:k1"), "key {key} lost its suffix");
    }
}
