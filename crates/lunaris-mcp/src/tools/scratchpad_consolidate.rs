//! `memory.scratchpad_consolidate` — Moon-backed integration tests.
//!
//! scratchpad-proxiable task: the handler + DTOs + the whole-scope handover
//! drain moved to the transport-neutral `lunaris_memory_service` crate
//! (`scratchpad_consolidate` + `handover`). The mcp `#[tool]` method now routes
//! through the proxy like the engine ops.
//!
//! What stays HERE are the guard/timeout/wired tests that need a REAL native
//! queue — they require the `embedded-moon` feature and construct `AppState`
//! via the production `bootstrap_inner` path (so `set_consolidator` installs
//! ActR), then call the SHARED handler against `state.lunaris` / `state.scope`.
//! The non-Moon guard-1 + outputSchema-root tests live with the handler in the
//! shared crate.

#[cfg(all(test, feature = "embedded-moon"))]
mod tests {
    use std::time::Duration;

    use lunaris_memory_service::scratchpad_consolidate::{
        ScratchpadConsolidateParams, handle, handle_inner,
    };

    use crate::state::AppState;

    /// embedded-moon fixture — queue_native=true, unique data dir per test.
    /// enable() is NOT called here — pipeline.is_enabled() stays false.
    /// The caller MUST hold the returned TempDir for the test's duration.
    async fn fresh_state_moon(scope_name: &str, data_dir: &str) -> AppState {
        // PRODUCTION bootstrap path (set_consolidator called, ActR installed).
        // skip_probe=true avoids requiring real GGUF weights in CI.
        AppState::bootstrap_inner(Some(scope_name), None, true, Some(data_dir)).await.unwrap()
    }

    /// Guard 2: embedded-moon + pipeline.enable() → worker_conflict.
    #[tokio::test]
    async fn guard_bg_worker_enabled_returns_worker_conflict() {
        let _tmpdir = tempfile::tempdir().unwrap(); // must outlive state
        let state = fresh_state_moon("test-cons-bgworker", _tmpdir.path().to_str().unwrap()).await;
        // Force-enable the pipeline — makes is_enabled() == true, spawns worker.
        state.lunaris.consolidator_pipeline().enable();
        let resp =
            handle(&state.lunaris, &state.scope, ScratchpadConsolidateParams { namespace: None })
                .await
                .unwrap();
        assert_eq!(
            resp.status, "worker_conflict",
            "enabled pipeline must return worker_conflict; got: {resp:?}"
        );
    }

    /// Guard 3: embedded-moon + 1ms injectable timeout → timeout fires.
    ///
    /// Seed one message so the topic exists; after it's consumed the next poll
    /// blocks on an existing-but-empty topic (the path that sleeps / times out).
    /// The outer 1ms timeout fires well before the 50ms inner cap or Moon's
    /// 250ms idle sleep.
    #[tokio::test]
    async fn guard_timeout_fires_within_wall_clock_bound() {
        use lunaris_consolidate::{CONSOLIDATE_TOPIC, ConsolidateEvent};
        use ulid::Ulid;

        let _tmpdir = tempfile::tempdir().unwrap(); // must outlive state
        let state = fresh_state_moon("test-cons-timeout", _tmpdir.path().to_str().unwrap()).await;
        let storage = state.lunaris.storage();

        let ev = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 1000,
            lsn_counter: 1,
            source: "scratchpad/timeout-probe".into(),
        };
        let payload = serde_json::to_vec(&ev).unwrap();
        storage.publish(&state.scope, CONSOLIDATE_TOPIC, 0, payload.into()).await.unwrap();

        let start = std::time::Instant::now();
        let resp = handle_inner(
            &state.lunaris,
            &state.scope,
            ScratchpadConsolidateParams { namespace: None },
            Duration::from_millis(1),
        )
        .await
        .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(resp.status, "timeout", "1ms timeout must fire; got: {resp:?}");
        assert!(elapsed < Duration::from_secs(6), "wall-clock must be < 6s; got: {elapsed:?}");
    }

    /// WIRED: seed an epoch-aged ConsolidateEvent under the REAL server scope,
    /// call the shared handler, assert a non-empty report AND audit emission.
    /// Discriminating for the scope-dev fix: the event is seeded under
    /// `state.scope` (not Scope::dev()), so it is only consumed if the drain
    /// subscribes under the real scope.
    #[tokio::test]
    async fn seed_aged_event_produces_non_empty_report() {
        use futures::StreamExt;
        use lunaris_consolidate::{CONSOLIDATE_TOPIC, ConsolidateEvent};
        use lunaris_core::Scope;
        use lunaris_core::audit::AUDIT_TOPIC;
        use ulid::Ulid;

        let _tmpdir = tempfile::tempdir().unwrap(); // must outlive state
        let state = fresh_state_moon("test-cons-wired", _tmpdir.path().to_str().unwrap()).await;
        let storage = state.lunaris.storage();

        // lsn_wall_ms=1000 (epoch) → ACT-R activation deeply negative → archives.
        let ev = ConsolidateEvent {
            kind: "ingest_committed".into(),
            episode_id: Ulid::new(),
            lsn_wall_ms: 1000,
            lsn_counter: 1,
            source: "scratchpad/test".into(),
        };
        let payload = serde_json::to_vec(&ev).unwrap();
        storage.publish(&state.scope, CONSOLIDATE_TOPIC, 0, payload.into()).await.unwrap();

        let resp =
            handle(&state.lunaris, &state.scope, ScratchpadConsolidateParams { namespace: None })
                .await
                .unwrap();

        assert_eq!(resp.status, "ok", "expected ok status; got: {resp:?}");
        let (promotions, archives) = (resp.promotions, resp.archives);
        assert!(
            promotions + archives > 0,
            "seeding under real scope must produce non-empty report (scope-dev fix check); \
             promotions={promotions}, archives={archives}"
        );

        // Audit readback: Publisher emits under Scope::dev() (Wave-1E known debt).
        let mut audit_stream =
            storage.subscribe(&Scope::dev(), "audit-drain-test", AUDIT_TOPIC, 0).await.unwrap();
        let deadline = tokio::time::sleep(Duration::from_secs(2));
        tokio::pin!(deadline);
        let mut found_audit = false;
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                msg = audit_stream.next() => {
                    match msg {
                        Some(Ok(m)) => {
                            if let Ok(
                                lunaris_core::audit::AuditEvent::ConsolidatorArchive { .. }
                                | lunaris_core::audit::AuditEvent::ConsolidatorPromotion { .. },
                            ) = serde_json::from_slice::<lunaris_core::audit::AuditEvent>(&m.payload)
                            {
                                found_audit = true;
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
        assert!(found_audit, "a consolidator audit event must be emitted");
    }
}
