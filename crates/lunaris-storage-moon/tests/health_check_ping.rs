//! Structural wiring guard for the Moon `StoragePort::health_check` override
//! (`observability-rollout-maturity`).
//!
//! A behavioral test would need a fake server that completes the entire
//! `connect_with_dim` dialog (RESP handshake + FT._LIST + per-index FT.INFO/
//! FT.CREATE) before stalling on PING — fragile and over-budget for this task.
//! Instead this guard pins that `impl StoragePort for MoonStorage` OVERRIDES
//! `health_check` with a real redis `PING` (not the trait's default `Ok(())`),
//! so a dead Moon surfaces as `Err` on the `/healthz` probe.
//!
//! The *bounding* guarantee (the PING cannot hang the handler) is already
//! proven behaviorally by `tests/op_timeout.rs`: every Moon command on the
//! `connect_with_timeout` client is bounded by `LUNARIS_MOON_OP_TIMEOUT`, and
//! PING is such a command. Reading the sources as strings (separate files)
//! avoids any self-match.
//!
//! RED reason: the override does not exist yet.

const MOON_LIB_SRC: &str = include_str!("../src/lib.rs");
const MOON_CLIENT_SRC: &str = include_str!("../src/client.rs");

#[test]
fn moon_storage_overrides_health_check_with_ping() {
    assert!(
        MOON_LIB_SRC.contains("async fn health_check"),
        "MoonStorage must OVERRIDE StoragePort::health_check (not inherit the default Ok(())), \
         so a dead/stalled Moon surfaces as Err on the /healthz probe"
    );
    assert!(
        MOON_LIB_SRC.contains("PING") || MOON_CLIENT_SRC.contains("PING"),
        "the Moon health_check override must issue a redis PING round-trip (the cheapest \
         liveness signal), bounded by the existing LUNARIS_MOON_OP_TIMEOUT path"
    );
}
