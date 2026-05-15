//! O-01-B — one-shot rayon thread-pool init for the candle CPU backend.
//! Mirror of `lunaris-embed-native::rayon_pool` — see that module for the
//! HARDWARE-OPTIMIZATION-ROADMAP §51 rationale. The init is per-process
//! (rayon's global pool); calling either crate's `ensure_physical_core_pool`
//! installs the same singleton, so concurrent embedder + reranker startup
//! does not double-init.

use std::sync::OnceLock;

static INIT: OnceLock<()> = OnceLock::new();

/// Install rayon's global thread pool sized to **physical** core count.
/// Idempotent across threads and across the two native crates.
pub fn ensure_physical_core_pool() {
    INIT.get_or_init(|| {
        let phys = num_cpus::get_physical().max(1);
        match rayon::ThreadPoolBuilder::new().num_threads(phys).build_global() {
            Ok(()) => {
                tracing::info!(
                    backend = "lunaris-rerank-native",
                    physical_cores = phys,
                    "rayon global pool installed at physical-core count"
                );
            }
            Err(e) => {
                tracing::debug!(
                    backend = "lunaris-rerank-native",
                    error = %e,
                    "rayon global pool already initialized; leaving as-is"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_is_idempotent() {
        ensure_physical_core_pool();
        ensure_physical_core_pool();
    }
}
