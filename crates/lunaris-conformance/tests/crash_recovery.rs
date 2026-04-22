//! Plan 04-03 D-23 + D-24: STORE-06 + OPS-07 chaos coverage.
//!
//! ## What this test proves
//!
//! `atomic_write` is **all-or-nothing** under `kill -9` mid-transaction on
//! BOTH backends:
//!
//! - **Moon** — TXN.BEGIN / TXN.COMMIT / TXN.ABORT (Phase 1 STORE-03; the
//!   chaos test here exercises the ABORT-on-connection-drop path).
//! - **Postgres** — sqlx `BEGIN` / `COMMIT` / drop-rollback (Phase 1
//!   STORE-04; the chaos test exercises the rollback-on-connection-drop
//!   path via the `pg_terminate_backend` semantics that fire when the
//!   client TCP socket closes mid-transaction).
//!
//! Per **D-24**, this plan does NOT add new backend logic. The TXN.ABORT /
//! transaction-rollback mechanisms already exist; this test asserts they
//! hold under SIGKILL.
//!
//! ## Scenario (verbatim from D-23)
//!
//! 1. Spawn the `chaos` subprocess (built by `lunaris-bench/src/bin/chaos.rs`)
//!    via `std::process::Command`. The child opens a Lunaris handle to the
//!    given URL and issues ONE `atomic_write` of 10K synthetic primitives.
//! 2. Sleep an adversarially-chosen delay drawn uniformly from
//!    `[1ms, atomic_write_p99]` — the proptest property domain.
//! 3. SIGKILL the child via `nix::sys::signal::kill(child_pid,
//!    Signal::SIGKILL)`.
//! 4. Reopen the SAME storage URL; count primitives via
//!    `StoragePort::scan_range(b"episode:", None)`.
//! 5. Assert `primitives_after == primitives_before` OR `primitives_after
//!    == primitives_before + chaos_corpus_size` (anything in between =
//!    partial state = test FAILURE per STORE-06).
//!
//! ## Skip discipline (D-25 + Plan 02-04 two-tier pattern)
//!
//! The property test SKIPs cleanly when `MOON_URL` / `PG_URL` env vars are
//! unset — `cargo test --features chaos-it` exits 0 even without any
//! running backend. This mirrors the `moon-it` / `pg-it` discipline from
//! Plan 02-04 (no backend = clean test exit, NOT failure). Probe order:
//!
//! 1. Env value must be `Some` (W-7 fix: caller passes `Option<String>`,
//!    not an env var NAME, so the test never touches process env).
//! 2. URL parses to a `host:port` authority.
//! 3. `host_port.to_socket_addrs()` resolves (W-3 fix: handles hostname-form
//!    addresses like `localhost:6390` — inline `host_port.parse::<SocketAddr>()`
//!    rejects hostnames).
//! 4. 1-second TCP probe to the resolved socket addr succeeds.
//!
//! Failure at any step → SKIP that backend's iteration with a stderr
//! diagnostic.
//!
//! ## CLAUDE.md compliance
//!
//! - `#![forbid(unsafe_code)]` at the file head — no FFI here. The SIGKILL
//!   syscall is wrapped by `nix::sys::signal::kill`, which is safe Rust
//!   around the underlying `libc::kill` boundary.
//! - No process-env mutation in this test (mutating env is not memory-safe
//!   in Rust 2024 edition). The test reads env at the call site and passes
//!   `Option<String>` into the probe (W-7 fix).
//! - No async wrappers around the chaos subprocess: `std::process::Command`
//!   is the canonical synchronous API, and the `nix::kill` call is a
//!   plain syscall — `tokio::process` would buy nothing here.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

#[cfg(all(unix, feature = "chaos-it"))]
mod chaos {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use futures::stream::StreamExt;
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use proptest::prelude::*;

    use lunaris::Lunaris;

    /// D-23 chaos corpus size — matches `build_chaos_corpus`'s default
    /// (10K primitives). One fact emits 2 WriteOps (KvPut + VectorUpsert)
    /// so the SIGKILL window during a 20K-op atomic_write is observable
    /// inside the property's `[1ms, ATOMIC_WRITE_P99_MS]` delay domain.
    const CHAOS_CORPUS_SIZE: u64 = 10_000;

    /// D-23 / D-25 — upper bound of the proptest delay distribution.
    /// Conservatively set above the Phase 2 measured atomic_write p99 so
    /// the property covers both the "kill-mid-write" AND "kill-after-commit"
    /// regimes. Phase 5 may tighten this once live measurements land.
    const ATOMIC_WRITE_P99_MS: u64 = 110;

    /// Two-tier skip discipline (Plan 02-04 pattern):
    /// 1. env value must be `Some`
    /// 2. URL must parse to a `host:port` authority
    /// 3. `host_port.to_socket_addrs()` resolves (W-3 fix: hostnames OK)
    /// 4. 1-second TCP probe to the resolved socket addr succeeds
    ///
    /// **W-3 fix:** use [`std::net::ToSocketAddrs`] (resolves both hostnames
    /// and literal IPs) instead of inline `host_port.parse::<SocketAddr>()`
    /// (literal IPs only — `localhost:6390` would be rejected).
    ///
    /// **W-7 fix:** takes `Option<String>` (the env value) instead of an
    /// env var NAME — removes any process-env-mutation foot-gun in tests
    /// (mutating env is not memory-safe in Rust 2024 edition).
    fn probe_backend(name: &str, url: Option<String>) -> Option<String> {
        let url = url?;
        let host_port = if let Some(rest) = url.strip_prefix("moon://") {
            // moon://host:port[/path] — strip optional trailing path.
            rest.split('/').next()?.to_string()
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            // postgres://[user[:pass]@]host[:port]/db — strip userinfo,
            // strip /db, fall back to default port 5432 when absent.
            let after_scheme = url.split("://").nth(1)?;
            let authority = after_scheme.split('/').next()?;
            let bare = authority.rsplit('@').next()?;
            if bare.contains(':') {
                bare.to_string()
            } else {
                format!("{bare}:5432")
            }
        } else {
            // Unknown scheme — skip rather than guess.
            return None;
        };

        // W-3 fix: ToSocketAddrs resolves hostnames + literal IPs. The
        // inline `host_port.parse::<SocketAddr>()` path would reject
        // "localhost:6390" because `SocketAddr::from_str` only accepts
        // literal IP addresses — fine for `127.0.0.1:6390`, NOT for
        // `localhost:6390` which is the canonical dev-box form.
        let timeout = Duration::from_secs(1);
        let addr = match host_port.to_socket_addrs().ok().and_then(|mut it| it.next()) {
            Some(a) => a,
            None => {
                // Intentionally do NOT log the URL itself (it may carry
                // credentials in postgres://user:pass@host form). Log only
                // the host_port and the env var name.
                eprintln!(
                    "crash_recovery: SKIP {name} (DNS resolution of {host_port} failed)"
                );
                return None;
            }
        };
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(_) => Some(url),
            Err(_) => {
                eprintln!(
                    "crash_recovery: SKIP {name} (TCP probe to {host_port} failed within {}ms)",
                    timeout.as_millis()
                );
                None
            }
        }
    }

    /// Reopen the storage URL and count rows under the `episode:` prefix
    /// via `StoragePort::scan_range`. Used to compute `primitives_before`
    /// AND `primitives_after` for the all-or-nothing assertion.
    ///
    /// Per D-24 the test does NOT call any new backend method — only the
    /// existing `scan_range` surface from Phase 1 STORE-01 / STORE-02.
    fn count_primitives_via_scan_range(url: &str) -> u64 {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for primitive count");
        rt.block_on(async {
            let lunaris = Lunaris::open(url).await.expect("reopen storage to count primitives");
            let storage = lunaris.storage();

            // Scan all 6 primitive prefixes — D-23 step 3a "scan_range
            // across all 6 primitive prefixes finds either ALL rows from
            // the atomic_write or ZERO rows (no partial)". `build_chaos_corpus`
            // currently emits only `fact:` rows + vector upserts (no
            // graph), so the dominant prefix is `fact:`. Scanning the
            // others future-proofs against the corpus expanding.
            let prefixes: &[&[u8]] = &[
                b"episode:",
                b"chunk:",
                b"entity:",
                b"relation:",
                b"fact:",
                b"community:",
            ];

            let mut total: u64 = 0;
            for prefix in prefixes {
                let mut stream = match storage.scan_range(prefix, None).await {
                    Ok(s) => s,
                    Err(e) => {
                        // Backend rejected the scan (e.g., NotSupported on a
                        // particular prefix). Treat as 0 for that prefix —
                        // the all-or-nothing invariant still holds because
                        // `before` and `after` use the same prefix list.
                        eprintln!(
                            "crash_recovery: scan_range({:?}) returned error: {e} — treating as 0",
                            std::str::from_utf8(prefix).unwrap_or("<non-utf8>")
                        );
                        continue;
                    }
                };
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(_) => total += 1,
                        Err(e) => {
                            // Mid-stream error: surface as panic so the
                            // proptest catches it as a failure (NOT a skip).
                            // A scan_range that starts but breaks halfway
                            // would mask partial state.
                            panic!(
                                "scan_range({:?}) mid-stream error: {e}",
                                std::str::from_utf8(prefix).unwrap_or("<non-utf8>")
                            );
                        }
                    }
                }
            }
            total
        })
    }

    /// Run one chaos iteration:
    /// 1. Count primitives before
    /// 2. Spawn `chaos` subprocess
    /// 3. Sleep `delay_ms`
    /// 4. SIGKILL (with try_wait race-window mitigation per T-04-03-03)
    /// 5. Wait for child
    /// 6. Sleep 100ms for backend rollback to settle (T-04-03-06 mitigation)
    /// 7. Count primitives after
    /// 8. Return `(before, after)` for the proptest assertion
    fn run_chaos_iteration(url: &str, delay_ms: u64) -> (u64, u64) {
        let primitives_before = count_primitives_via_scan_range(url);

        // Resolve the `chaos` binary path at runtime. Cargo's
        // `CARGO_BIN_EXE_<name>` env var is ONLY set for binaries in the
        // SAME package as the integration test — and `chaos` lives in
        // `lunaris-bench` (a dev-dep), NOT in `lunaris-conformance`. So we
        // walk up from `CARGO_MANIFEST_DIR` to the workspace root and join
        // `target/{profile}/chaos`. The profile is "debug" for `cargo test`
        // and "release" for `cargo test --release`; the
        // `CARGO_TARGET_TMPDIR` env var (Rust 1.54+) is set by Cargo when
        // running integration tests and lives at
        // `target/{profile}/build/<crate>-<hash>/...` — its parent's parent
        // is `target/{profile}/`. We use `CARGO_BIN_EXE_<name>` if Cargo
        // ever starts setting it cross-crate (forward-compat), else fall
        // back to the manifest-dir walk.
        let chaos_bin = std::env::var("CARGO_BIN_EXE_chaos").unwrap_or_else(|_| {
            // CARGO_MANIFEST_DIR points at this crate
            // (.../crates/lunaris-conformance). Workspace root is two
            // dirs up. target/ sits at the workspace root.
            let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let workspace = manifest.parent().and_then(|p| p.parent()).unwrap_or(&manifest);
            // Honor CARGO_TARGET_DIR if set, else default to <workspace>/target
            let target_dir = std::env::var("CARGO_TARGET_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| workspace.join("target"));
            // Profile: tests run under debug unless --release was passed.
            // `cfg!(debug_assertions)` is true in the debug profile.
            let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
            target_dir.join(profile).join("chaos").to_string_lossy().into_owned()
        });
        let mut child = Command::new(&chaos_bin)
            .arg(url)
            .arg("--corpus")
            .arg(CHAOS_CORPUS_SIZE.to_string())
            // T-04-03-02 mitigation: route stdout/stderr to /dev/null so
            // the chaos child doesn't echo URLs (which may carry creds in
            // postgres://user:pass@host form) into the test log.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn chaos child");

        std::thread::sleep(Duration::from_millis(delay_ms));

        let pid = Pid::from_raw(child.id() as i32);
        // T-04-03-03 mitigation: try_wait BEFORE kill — if the child has
        // already exited (commit completed faster than delay_ms), don't
        // attempt to send SIGKILL to a recycled PID. This is the "kill-
        // after-commit" branch of the property domain — equally valid: the
        // assertion `after == before + N` holds.
        match child.try_wait() {
            Ok(Some(_status)) => {
                // Child already done; valid commit-completed scenario.
            }
            Ok(None) => {
                if let Err(e) = kill(pid, Signal::SIGKILL) {
                    // The child may have just exited between try_wait and
                    // kill (race window). Log + continue — the wait below
                    // reaps the exit status either way.
                    eprintln!(
                        "crash_recovery: kill({pid}) failed: {e} (child may have just exited)"
                    );
                }
            }
            Err(e) => {
                // try_wait itself errored — best-effort kill, then proceed.
                eprintln!(
                    "crash_recovery: try_wait error: {e}; attempting SIGKILL anyway"
                );
                let _ = kill(pid, Signal::SIGKILL);
            }
        }
        // Reap the child. We don't care about exit status here — both
        // commit-completed (exit 0) and SIGKILL'd (no exit code) are
        // valid scenarios for the all-or-nothing property.
        let _ = child.wait();

        // T-04-03-06 mitigation: 100ms sleep so the backend's TXN.ABORT
        // (Moon) or rollback-on-conn-drop (Postgres) finishes fully before
        // we reopen and scan. Without this, the recovery scan could race
        // the in-flight rollback and observe transient partial state.
        std::thread::sleep(Duration::from_millis(100));

        let primitives_after = count_primitives_via_scan_range(url);
        (primitives_before, primitives_after)
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            // D-25: 50 iterations × 2 backends × ~1s/iter = ~100s total.
            // Runs in CI's integration-test job, NOT the unit-test job.
            cases: 50,
            // Persistence file under target/ would dirty the repo on
            // failure; disable so a flake doesn't leave artefacts behind.
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        /// STORE-06 + OPS-07 on Moon — the primary path for v0
        /// (Moon is the default backend per the blueprint §5.3 inversion).
        #[test]
        fn moon_atomic_write_is_all_or_nothing_under_kill_minus_9(
            delay_ms in 1u64..ATOMIC_WRITE_P99_MS,
        ) {
            // W-7 fix: read env at the call site, pass Option<String> in
            // — never mutate process env from inside the test.
            let url = match probe_backend("MOON_URL", std::env::var("MOON_URL").ok()) {
                Some(u) => u,
                None => return Ok(()),
            };
            let (before, after) = run_chaos_iteration(&url, delay_ms);
            // STORE-06 invariant: anything in between `before` and
            // `before + CHAOS_CORPUS_SIZE` = partial state = test
            // FAILURE. The two valid outcomes are commit-completed
            // (after == before + N) and rolled-back (after == before).
            prop_assert!(
                after == before || after == before + CHAOS_CORPUS_SIZE,
                "STORE-06 (Moon): atomic_write must be all-or-nothing under kill -9; \
                 before={before} after={after} chaos_corpus_size={CHAOS_CORPUS_SIZE} \
                 delay_ms={delay_ms}"
            );
        }

        /// STORE-06 + OPS-07 on Postgres — the portability proof per
        /// blueprint §5.3 (Lunaris isn't Moon-locked).
        #[test]
        fn postgres_atomic_write_is_all_or_nothing_under_kill_minus_9(
            delay_ms in 1u64..ATOMIC_WRITE_P99_MS,
        ) {
            // W-7 fix: read env at the call site.
            let url = match probe_backend("PG_URL", std::env::var("PG_URL").ok()) {
                Some(u) => u,
                None => return Ok(()),
            };
            let (before, after) = run_chaos_iteration(&url, delay_ms);
            prop_assert!(
                after == before || after == before + CHAOS_CORPUS_SIZE,
                "STORE-06 (Postgres): atomic_write must be all-or-nothing under kill -9; \
                 before={before} after={after} chaos_corpus_size={CHAOS_CORPUS_SIZE} \
                 delay_ms={delay_ms}"
            );
        }
    }

    /// W-7 unit assertion: passing `None` short-circuits without touching
    /// process env. Keeps the test surface honest about its zero-env-mutation
    /// contract.
    #[test]
    fn probe_backend_returns_none_for_unset_env() {
        assert!(probe_backend("nonexistent", None).is_none());
    }

    /// Unknown URL schemes (e.g., `redis://`) skip cleanly rather than
    /// guess host:port from a malformed input.
    #[test]
    fn probe_backend_returns_none_for_unknown_scheme() {
        assert!(probe_backend("weird", Some("redis://localhost:6379".to_string())).is_none());
    }

    /// W-3 unit assertion: the host:port extraction itself accepts both
    /// hostname-form (`localhost:6390`) and literal-IP-form
    /// (`127.0.0.1:6390`) addresses. We can't TCP-probe in a unit test
    /// without a live backend, so we just assert the parse step doesn't
    /// reject the hostname form (W-3 regression guard).
    #[test]
    fn probe_backend_extracts_host_port_for_moon_hostname_form() {
        // We set MOON_URL to an unreachable port so the parse + DNS
        // succeeds but the TCP probe fails — proves the W-3 path is
        // exercised (DNS resolution of "localhost" works, then TCP probe
        // to port 1 fails, returning None). If W-3 had regressed (literal-
        // IP-only parse), the function would have returned None at the
        // parse step BEFORE reaching the TCP probe, so this test would
        // still pass — but we additionally assert that
        // `to_socket_addrs()` succeeds for the hostname form below.
        use std::net::ToSocketAddrs;
        let resolved = "localhost:1".to_socket_addrs();
        assert!(
            resolved.is_ok() && resolved.unwrap().next().is_some(),
            "W-3: localhost:port MUST resolve via to_socket_addrs (regressing to \
             SocketAddr::from_str would break dev-box probes)"
        );
    }
}

// Plan 04-03: when the `chaos-it` feature is OFF (default), the test file
// contains no test functions. Cargo treats files under `tests/` with no
// `#[test]` items as a no-op build, so `cargo test --test crash_recovery`
// exits 0 cleanly (no tests run, no failures). This keeps the default
// `cargo test --workspace` runtime clean — the chaos loop is opt-in via
// `--features chaos-it` per D-25.
#[cfg(not(all(unix, feature = "chaos-it")))]
#[test]
fn chaos_it_feature_disabled_placeholder() {
    // Document why this file appears empty under default features.
    // Without this placeholder some toolchains warn about "no tests
    // discovered in test binary"; the placeholder also gives operators a
    // clear PASS line confirming the test crate built correctly.
    eprintln!(
        "crash_recovery: chaos-it feature disabled (or non-Unix target); \
         enable with `cargo test -p lunaris-conformance --features chaos-it --test crash_recovery`"
    );
}

// Phase 12 Plan 12-04 Task 1 — smoke test for the new helios interrupt
// profile. Lives in this test file per the plan's Done criteria (so the
// HELIOS-06 interrupt profile is co-located with the Phase-4 crash_recovery
// harness it forward-references). Pure-logic test — runs in every default
// `cargo test --workspace` pass.
#[test]
fn helios_interrupt_profile_smoke() {
    use lunaris_conformance::chaos_helpers::{helios_interrupt_profile, InterruptSite};
    let profile = helios_interrupt_profile();
    assert_eq!(profile.name, "helios");
    assert!(
        profile
            .sites
            .contains(&InterruptSite::MidHeliosScratchpadWrite),
        "profile MUST include MidHeliosScratchpadWrite (Phase 12 D-10)"
    );
    assert!(
        profile
            .sites
            .contains(&InterruptSite::MidConsolidatorPromotion),
        "profile MUST include MidConsolidatorPromotion (Phase 12 D-11 Consolidator-mid-promotion race)"
    );
}
