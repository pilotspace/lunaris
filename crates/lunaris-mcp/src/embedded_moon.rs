//! In-process Moon server — re-export shim.
//!
//! The launcher implementation was PROMOTED to
//! `lunaris_memory_service::embedded_moon` (2026-07-17, contextd embedded-moon
//! unification) so `lunaris-mcp` and `lunaris-contextd` share ONE definition.
//! This shim preserves every existing `crate::embedded_moon::*` call site
//! (state.rs) unchanged.
//!
//! The transport-generic tests (launch/shutdown, decide_storage DI seam,
//! ServerConfig clap-defaults) moved with the implementation. The tests kept
//! here are the ones coupled to mcp-side wiring: `AppState`, `MemoryProxy`,
//! and the session-pad handover.

#[cfg(feature = "embedded-moon")]
pub(crate) use lunaris_memory_service::embedded_moon::{
    EmbeddedMoonError, EmbeddedMoonGuard, decide_storage_with_launcher, launch_embedded_moon,
};

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "embedded-moon"))]
mod tests {
    use super::*;
    use std::sync::Arc;

    // Round-trip through live Moon via the shared handlers, driven off an mcp
    // AppState. Drive the launcher directly (no embedder probe — use
    // open_with_embedder + StubEmbedder to avoid a GGUF download).
    #[tokio::test]
    async fn embedded_moon_scratchpad_round_trip() {
        use lunaris::Lunaris;
        use lunaris_core::{Scope, StubEmbedder};

        let tmpdir = tempfile::tempdir().unwrap();
        let guard = launch_embedded_moon(tmpdir.path().to_str().unwrap())
            .await
            .expect("embedded Moon must start");
        let url = format!("moon://127.0.0.1:{}", guard.port);

        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder(&url, embedder)
            .await
            .expect("Lunaris::open must succeed with live Moon");

        let scope = Scope::new("test-vuz-roundtrip").unwrap();
        let app_state = crate::state::AppState {
            lunaris: Arc::new(lunaris),
            scope,
            storage_url: url.clone(),
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: None, // guard owned separately for this test
        };

        // write then read (shared handlers; namespace resolved here as None → default)
        let write_params = lunaris_memory_service::scratchpad_write::ScratchpadWriteParams {
            key: "hello".into(),
            value: serde_json::json!("world"),
            namespace: None,
        };
        let write_resp = lunaris_memory_service::scratchpad_write::handle(
            &app_state.lunaris,
            &app_state.scope,
            write_params,
        )
        .await
        .expect("scratchpad_write must succeed");
        assert!(!write_resp.lsn.is_empty());

        let read_params = lunaris_memory_service::scratchpad_read::ScratchpadReadParams {
            key: "hello".into(),
            namespace: None,
        };
        let read_resp = lunaris_memory_service::scratchpad_read::handle(
            &app_state.lunaris,
            &app_state.scope,
            read_params,
        )
        .await
        .expect("scratchpad_read must succeed");

        assert!(read_resp.found, "key written to Moon must be found on read");
        assert_eq!(
            read_resp.value,
            Some(serde_json::json!("world")),
            "read value must match written value"
        );

        // cleanup
        let guard_arc = Arc::new(guard);
        guard_arc.shutdown().await;
    }

    // scratchpad-handover integration (the discriminating wire-proof):
    // session A writes under its pad -> marker flips to B -> B's first read
    // (a) serves the new per-session pad (empty), (b) triggers the guarded
    // whole-scope handover consolidate (proven by the explicit consolidate
    // afterwards draining ZERO events), and (c) leaves A's pad readable
    // under its explicit namespace (nothing destroyed).
    #[tokio::test]
    async fn embedded_moon_session_handover_rotates_and_drains() {
        use lunaris::Lunaris;
        use lunaris_core::{Scope, StubEmbedder};

        use crate::proxy::MemoryProxy;
        use crate::tools::staging::resolve_namespace_session_aware;

        crate::tools::staging::skip_stage_for_tests();
        let _seam = crate::session_pad::lock_test_seam().await;

        let tmpdir = tempfile::tempdir().unwrap();
        let guard = launch_embedded_moon(tmpdir.path().to_str().unwrap())
            .await
            .expect("embedded Moon must start");
        let url = format!("moon://127.0.0.1:{}", guard.port);

        let embedder = Arc::new(StubEmbedder::new(768));
        let lunaris = Lunaris::open_with_embedder(&url, embedder)
            .await
            .expect("Lunaris::open must succeed with live Moon");
        // Same wiring as production bootstrap: ActR installed, pipeline
        // disabled (guard 2 passes; the handover is the sole consumer).
        lunaris
            .consolidator_pipeline()
            .set_consolidator(
                Arc::new(lunaris::ActRConsolidator::default()) as Arc<dyn lunaris::Consolidator>
            );

        let scope_name = "test-handover-rotation";
        let scope = Scope::new(scope_name).unwrap();
        let app_state = crate::state::AppState {
            lunaris: Arc::new(lunaris),
            scope,
            storage_url: url.clone(),
            #[cfg(feature = "embedded-moon")]
            _embedded_moon: None,
        };

        // Session A active.
        let marker = tmpdir.path().join("sessions.json");
        let write_marker = |session: &str| {
            let body = serde_json::json!({
                scope_name: { "active_session_id": session, "ended": false,
                               "updated_at": "2026-06-11T00:00:00Z" }
            });
            std::fs::write(&marker, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
        };
        write_marker("sess-ha-a");
        crate::session_pad::set_sessions_file_for_tests(Some(marker.clone()));

        // Direct-only proxy: the handover fires THROUGH the proxy but, with no
        // socket, runs on THIS app_state.lunaris — the same engine that holds
        // the pad. This mirrors exactly what the `#[tool]` methods do: resolve
        // the session-aware namespace (which triggers the handover) then call
        // the shared handler with the resolved namespace.
        let proxy = MemoryProxy::direct_only_for_test();

        // A writes two keys under its (default) pad — two consolidate events enqueued.
        for (k, v) in [("plan", "ship the handover"), ("blocker", "none")] {
            let ns = resolve_namespace_session_aware(&proxy, &app_state, None)
                .await
                .expect("session-A namespace resolution must succeed");
            lunaris_memory_service::scratchpad_write::handle(
                &app_state.lunaris,
                &app_state.scope,
                lunaris_memory_service::scratchpad_write::ScratchpadWriteParams {
                    key: k.into(),
                    value: serde_json::json!(v),
                    namespace: Some(ns),
                },
            )
            .await
            .expect("session-A write must succeed");
        }

        // Switch: marker flips to session B.
        write_marker("sess-ha-b");

        // B's first default resolution triggers the whole-scope handover drain
        // (through the proxy → this engine), then serves B's fresh (empty) pad.
        let b_ns = resolve_namespace_session_aware(&proxy, &app_state, None)
            .await
            .expect("session-B namespace resolution (fires handover) must succeed");
        let read_resp = lunaris_memory_service::scratchpad_read::handle(
            &app_state.lunaris,
            &app_state.scope,
            lunaris_memory_service::scratchpad_read::ScratchpadReadParams {
                key: "plan".into(),
                namespace: Some(b_ns),
            },
        )
        .await
        .expect("session-B read must succeed even while handover runs");
        assert!(
            !read_resp.found,
            "session B's fresh pad must not see session A's keys (got {:?})",
            read_resp.value
        );

        // Drain proof: the explicit consolidate now finds NOTHING — the
        // handover already consumed A's pending events. (Without the
        // handover this drains the two enqueued events instead.)
        let consolidate_resp = lunaris_memory_service::scratchpad_consolidate::handle(
            &app_state.lunaris,
            &app_state.scope,
            lunaris_memory_service::scratchpad_consolidate::ScratchpadConsolidateParams {
                namespace: None,
            },
        )
        .await
        .expect("explicit consolidate must succeed");
        assert_eq!(consolidate_resp.status, "ok");
        assert_eq!(
            consolidate_resp.promotions + consolidate_resp.archives,
            0,
            "handover must have drained session A's events before B's pad was served"
        );

        // Contracted: A's facts surface in long-term recall after handover.
        let recall_resp = lunaris_memory_service::recall::handle(
            &app_state.lunaris,
            &app_state.scope,
            lunaris_memory_service::recall::RecallParams {
                query: "ship the handover".into(),
                k: 5,
                filters: Some(lunaris_memory_service::recall::RecallFilters {
                    source_prefix: Some("scratchpad/sess-ha-a/".into()),
                }),
                as_of: None,
                raw: false,
            },
        )
        .await
        .expect("recall over session A's pad must succeed");
        assert!(
            !recall_resp.hits.is_empty(),
            "memory.recall must surface session A's facts after the handover"
        );

        // Nothing destroyed: A's pad stays readable under its explicit namespace.
        let a_read = lunaris_memory_service::scratchpad_read::handle(
            &app_state.lunaris,
            &app_state.scope,
            lunaris_memory_service::scratchpad_read::ScratchpadReadParams {
                key: "plan".into(),
                namespace: Some("scratchpad/sess-ha-a/".into()),
            },
        )
        .await
        .expect("explicit session-A read must succeed");
        assert!(a_read.found, "session A's pad must remain intact after handover");

        crate::session_pad::set_sessions_file_for_tests(None);
        let guard_arc = Arc::new(guard);
        guard_arc.shutdown().await;
    }
}
