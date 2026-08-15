//! 0.6.2 P0-3 — readiness probing with a storage WRITE canary.
//!
//! ## Why PING is not enough
//!
//! `/healthz` asks the backend "are you there?" — a TCP connect plus a Moon
//! `PING`. Both of the production wedges this repo has hit (the MA1 write-stall,
//! the dashtable recovery crash-loop) answered PING happily while refusing every
//! write. The load balancer kept routing traffic into a store that could not
//! accept a byte, and no probe we shipped could see it.
//!
//! `/readyz` therefore *writes*: a fixed reserved key in a reserved scope is
//! `KvPut` then `KvDelete`-d through the ordinary `StoragePort` path, bounded
//! by [`CANARY_TIMEOUT`]. A backend that accepts connections but stalls writes
//! fails the canary and drops out of the load balancer — which is the correct
//! remedy. Liveness stays green on purpose: restarting the process does not
//! un-wedge the backend, it just adds a cold start to the incident.
//!
//! ## Why the probe is cached
//!
//! Readiness is polled — by Kubernetes, by an LB, sometimes by a flapping
//! reconcile loop. Uncached, `/readyz` would convert probe traffic into write
//! traffic at whatever rate the caller chose. A result younger than
//! [`CACHE_TTL`] is served from cache, and the cache is guarded by a
//! `tokio::sync::Mutex` so concurrent probes single-flight: at most one canary
//! is ever in flight, and the callers queued behind it get its fresh result.
//! (A `tokio` mutex, deliberately — this guard IS held across an `.await`, which
//! is exactly what `parking_lot` must never do.)
//!
//! ## Why the embedder check does not run inference
//!
//! `Embedder::dim()` is a structural check that never pulls weights. Running a
//! real `embed_batch` on a probe would be actively harmful: a wedged ggml pool
//! cannot be cancelled from async Rust (see `lunaris-hook/src/watchdog.rs`), so
//! an inference-based probe would itself wedge, on a timer, forever.

use std::time::{Duration, Instant};

use lunaris::Lunaris;
use lunaris_core::Scope;
use lunaris_core::storage::types::WriteOp;

use crate::metrics::metrics;

/// Budget for the PING and for the write canary, each. Deliberately far below
/// the default 30 s request budget: a probe that hangs is a probe that lies.
pub const CANARY_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum age of a readiness result served from cache.
pub const CACHE_TTL: Duration = Duration::from_secs(5);

/// Reserved partition for internal health traffic. Valid under the `Scope`
/// alphabet (`[A-Za-z0-9_\-.]{1,128}`) and cannot collide with a tenant scope
/// minted from a bearer token unless an operator deliberately names a tenant
/// `__health__`.
const HEALTH_SCOPE: &str = "__health__";

/// Reserved canary key. Deliberately NOT built with
/// `lunaris_core::keyspace::*_key`: those mint `lunaris:{scope}:{kind}:{ulid}`
/// for *primitives*, and a ULID suffix would make every probe write a NEW key.
/// The canary must be one fixed, self-overwriting key so a probe loop leaves no
/// residue.
const CANARY_KEY: &[u8] = b"lunaris:__health__:canary";

/// Outcome of one component check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Component answered within budget.
    Ok,
    /// Component answered with an error.
    Error,
    /// Component did not answer within budget — the wedge signature.
    Timeout,
}

impl CheckStatus {
    fn is_ok(self) -> bool {
        matches!(self, CheckStatus::Ok)
    }
}

/// Per-component readiness detail, serialized under `checks` in the response.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Checks {
    /// Storage liveness (`StoragePort::health_check` — Moon `PING`).
    pub ping: CheckStatus,
    /// Storage WRITE canary (`KvPut` + `KvDelete` on the reserved key).
    pub canary: CheckStatus,
    /// Embedder is configured with a usable dimension.
    pub embedder: CheckStatus,
}

/// A readiness verdict plus its component breakdown.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ReadyReport {
    pub ready: bool,
    pub checks: Checks,
}

impl ReadyReport {
    fn from_checks(checks: Checks) -> Self {
        let ready = checks.ping.is_ok() && checks.canary.is_ok() && checks.embedder.is_ok();
        Self { ready, checks }
    }
}

struct Cached {
    at: Instant,
    report: ReadyReport,
}

/// Rate-limited readiness prober. One per process, shared via [`crate::AppState`].
#[derive(Default)]
pub struct Readiness {
    cache: tokio::sync::Mutex<Option<Cached>>,
}

impl Readiness {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the current readiness verdict, probing at most once per
    /// [`CACHE_TTL`].
    pub async fn check(&self, lunaris: &Lunaris) -> ReadyReport {
        // Single-flight: whoever holds the guard runs the canary; everyone
        // else re-reads the (now fresh) cache after acquiring it.
        let mut guard = self.cache.lock().await;
        if let Some(cached) = guard.as_ref()
            && cached.at.elapsed() < CACHE_TTL
        {
            return cached.report;
        }
        let report = probe(lunaris).await;
        metrics().ready.set(i64::from(report.ready));
        *guard = Some(Cached { at: Instant::now(), report });
        report
    }
}

/// Run every component check. Each is independently bounded — a stalled PING
/// must not eat the canary's budget.
async fn probe(lunaris: &Lunaris) -> ReadyReport {
    let storage = lunaris.storage();

    let ping = match tokio::time::timeout(CANARY_TIMEOUT, storage.health_check()).await {
        Ok(Ok(())) => CheckStatus::Ok,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "readyz: storage ping failed");
            CheckStatus::Error
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = CANARY_TIMEOUT.as_secs(),
                "readyz: storage ping timed out"
            );
            CheckStatus::Timeout
        }
    };

    let scope = Scope::new(HEALTH_SCOPE).expect("HEALTH_SCOPE is a compile-time-valid scope");
    let canary = {
        // A nonce value so a successful put is distinguishable in a backend
        // dump, and so two servers probing the same store do not look idempotent.
        let value = ulid::Ulid::new().to_string().into_bytes();
        let put = WriteOp::KvPut { key: CANARY_KEY.to_vec(), value };
        let del = WriteOp::KvDelete { key: CANARY_KEY.to_vec() };
        let storage = storage.clone();
        let scope = scope.clone();
        let write = async move {
            storage.atomic_write(&scope, std::slice::from_ref(&put)).await?;
            storage.atomic_write(&scope, std::slice::from_ref(&del)).await
        };
        match tokio::time::timeout(CANARY_TIMEOUT, write).await {
            Ok(Ok(_lsn)) => CheckStatus::Ok,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "readyz: write canary rejected");
                CheckStatus::Error
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = CANARY_TIMEOUT.as_secs(),
                    "readyz: write canary timed out — backend accepts connections but stalls \
                     writes (the wedge signature)"
                );
                CheckStatus::Timeout
            }
        }
    };

    // Structural only — see the module docs on why a probe must never run
    // inference.
    let embedder = if lunaris.embedder().dim() > 0 { CheckStatus::Ok } else { CheckStatus::Error };

    ReadyReport::from_checks(Checks { ping, canary, embedder })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_scope_is_a_valid_scope() {
        assert!(Scope::new(HEALTH_SCOPE).is_ok());
    }

    #[test]
    fn canary_key_is_scoped_and_fixed() {
        let key = String::from_utf8(CANARY_KEY.to_vec()).expect("ascii");
        assert!(key.starts_with("lunaris:__health__:"), "canary must live in the reserved scope");
        assert!(!key.contains("01"), "canary key must be FIXED, never ULID-suffixed");
    }

    #[test]
    fn ready_requires_every_check() {
        let all_ok =
            Checks { ping: CheckStatus::Ok, canary: CheckStatus::Ok, embedder: CheckStatus::Ok };
        assert!(ReadyReport::from_checks(all_ok).ready);
        for broken in [
            Checks { canary: CheckStatus::Timeout, ..all_ok },
            Checks { ping: CheckStatus::Error, ..all_ok },
            Checks { embedder: CheckStatus::Error, ..all_ok },
        ] {
            assert!(!ReadyReport::from_checks(broken).ready, "{broken:?} must not be ready");
        }
    }
}
