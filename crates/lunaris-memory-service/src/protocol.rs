//! Wire protocol shared by the two client surfaces (contextd-mcp-merge).
//!
//! `MemoryRequest` / `MemoryResponse` are the socket contract between the thin
//! `lunaris-mcp` proxy and the warm `lunaris-contextd` daemon. They live HERE
//! (the transport-neutral shared crate), not in either client, so neither peer
//! depends on the other — and [`dispatch`] is the SINGLE variant→handler map
//! both the contextd socket path and the mcp direct-open fallback call, so the
//! two cannot diverge.
//!
//! Framing is the caller's concern (one JSON request/response, connection-per-
//! call); this module only defines the shapes and the pure dispatch.

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::Scope;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ServiceError;

/// Default contextd socket, mirrored by both peers. Override with
/// `LUNARIS_CONTEXTD_SOCKET`. Kept here so the mcp proxy and the daemon agree
/// on one path without either depending on the other's crate.
pub const CONTEXTD_SOCKET_ENV: &str = "LUNARIS_CONTEXTD_SOCKET";

/// contextd multiplexes hook ops and memory ops on ONE socket via an
/// internally-tagged `type` field on its `ContextRequest` enum. A
/// `MemoryRequest` therefore crosses the wire as
/// `{"type":"memory", <flattened MemoryRequest>}` — the bare `MemoryRequest`
/// JSON PLUS this discriminator. These two constants + [`encode_socket_request`]
/// keep the framing here so the mcp proxy can frame a request WITHOUT depending
/// on `lunaris-hook` (which owns `ContextRequest`). Kept in lock-step by the
/// wire round-trip test in `lunaris-hook` (`proxy_frame_decodes_as_context_request`).
pub const SOCKET_KIND_FIELD: &str = "type";
/// The `ContextRequest` `type` value that selects the memory-op channel.
pub const SOCKET_KIND_MEMORY: &str = "memory";

/// Frame a [`MemoryRequest`] for the contextd unix socket.
///
/// Emits the request's own fields plus the `type: "memory"` discriminator
/// contextd's `ContextRequest` decode requires. WITHOUT this tag contextd
/// rejects the frame (`missing field \`type\``) and closes the connection — the
/// mcp proxy then reads an empty reply, trips its breaker, and silently falls
/// back to Direct, defeating the warm-engine path this whole design exists for.
pub fn encode_socket_request(req: &MemoryRequest) -> Result<Vec<u8>, serde_json::Error> {
    let mut v = serde_json::to_value(req)?;
    // `MemoryRequest` always serializes to a JSON object (it is itself an
    // internally-tagged enum), so this insert is total; the fallback re-encodes
    // the untagged value rather than panicking on the theoretical non-object.
    if let Value::Object(map) = &mut v {
        map.insert(SOCKET_KIND_FIELD.to_owned(), Value::String(SOCKET_KIND_MEMORY.to_owned()));
    }
    serde_json::to_vec(&v)
}

/// Engine-op request mirroring the `memory.*` MCP tools.
///
/// The six stateless engine ops PLUS the four scratchpad ops and the session
/// handover cross the socket — only `list_scopes` (registry) stays mcp-local.
/// Each variant carries an explicit `scope` (trusted local-peer model, §3
/// FROZEN @ v1: the 0700 user socket is the trust boundary) plus the tool's own
/// wire DTO as `params`.
///
/// Scratchpad `namespace` is NOT a top-level field: it rides `params.namespace`,
/// pre-resolved session-aware by the mcp caller before dispatch (the sessions.json
/// marker is client-machine state; contextd has no marker). `ScratchpadHandover`
/// carries no params — it drains the WHOLE scope (`consolidate_unfiltered`).
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum MemoryRequest {
    Ingest {
        scope: String,
        params: crate::ingest::IngestParams,
    },
    Recall {
        scope: String,
        params: crate::recall::RecallParams,
    },
    Forget {
        scope: String,
        params: crate::forget::ForgetParams,
    },
    Remember {
        scope: String,
        params: crate::remember::RememberParams,
    },
    Profile {
        scope: String,
        params: crate::profile::ProfileParams,
    },
    RecordDecision {
        scope: String,
        params: crate::record_decision::RecordDecisionParams,
    },
    RecordEdit {
        scope: String,
        params: crate::record_edit::RecordEditParams,
    },
    Feedback {
        scope: String,
        params: crate::feedback::FeedbackParams,
    },
    Status {
        scope: String,
    },
    /// Maintenance: remove `vec` fields that cannot be KNN candidates (F22).
    /// Deliberately absent from the MCP tool roster and the HTTP routes — see
    /// `crate::repair_vectors`.
    RepairVectors {
        scope: String,
        params: crate::repair_vectors::RepairVectorsParams,
    },
    ScratchpadWrite {
        scope: String,
        params: crate::scratchpad_write::ScratchpadWriteParams,
    },
    ScratchpadRead {
        scope: String,
        params: crate::scratchpad_read::ScratchpadReadParams,
    },
    ScratchpadGrep {
        scope: String,
        params: crate::scratchpad_grep::ScratchpadGrepParams,
    },
    ScratchpadConsolidate {
        scope: String,
        params: crate::scratchpad_consolidate::ScratchpadConsolidateParams,
    },
    ScratchpadHandover {
        scope: String,
    },
    VerifyAgenda {
        scope: String,
        params: crate::verify_agenda::VerifyAgendaParams,
    },
    Resolve {
        scope: String,
        params: crate::resolve::ResolveParams,
    },
    DreamAgenda {
        scope: String,
        params: crate::dream_agenda::DreamAgendaParams,
    },
    Distill {
        scope: String,
        params: crate::distill::DistillParams,
    },
}

impl MemoryRequest {
    /// Borrow the peer-supplied scope string. Every variant carries `scope`, so
    /// this never returns empty by construction — but an empty *string* is
    /// still rejected by `Scope::new` downstream (a `scope_required` fault).
    pub fn scope(&self) -> &str {
        match self {
            MemoryRequest::Ingest { scope, .. }
            | MemoryRequest::Recall { scope, .. }
            | MemoryRequest::Forget { scope, .. }
            | MemoryRequest::Remember { scope, .. }
            | MemoryRequest::Profile { scope, .. }
            | MemoryRequest::RecordDecision { scope, .. }
            | MemoryRequest::RecordEdit { scope, .. }
            | MemoryRequest::Feedback { scope, .. }
            | MemoryRequest::Status { scope, .. }
            | MemoryRequest::RepairVectors { scope, .. }
            | MemoryRequest::ScratchpadWrite { scope, .. }
            | MemoryRequest::ScratchpadRead { scope, .. }
            | MemoryRequest::ScratchpadGrep { scope, .. }
            | MemoryRequest::ScratchpadConsolidate { scope, .. }
            | MemoryRequest::ScratchpadHandover { scope, .. }
            | MemoryRequest::VerifyAgenda { scope, .. }
            | MemoryRequest::Resolve { scope, .. }
            | MemoryRequest::DreamAgenda { scope, .. }
            | MemoryRequest::Distill { scope, .. } => scope,
        }
    }

    /// A short op label for logs/metrics (no scope or payload).
    pub fn op(&self) -> &'static str {
        match self {
            MemoryRequest::Ingest { .. } => "ingest",
            MemoryRequest::Recall { .. } => "recall",
            MemoryRequest::Forget { .. } => "forget",
            MemoryRequest::Remember { .. } => "remember",
            MemoryRequest::Profile { .. } => "profile",
            MemoryRequest::RecordDecision { .. } => "record_decision",
            MemoryRequest::RecordEdit { .. } => "record_edit",
            MemoryRequest::Feedback { .. } => "feedback",
            MemoryRequest::Status { .. } => "status",
            MemoryRequest::RepairVectors { .. } => "repair_vectors",
            MemoryRequest::ScratchpadWrite { .. } => "scratchpad_write",
            MemoryRequest::ScratchpadRead { .. } => "scratchpad_read",
            MemoryRequest::ScratchpadGrep { .. } => "scratchpad_grep",
            MemoryRequest::ScratchpadConsolidate { .. } => "scratchpad_consolidate",
            MemoryRequest::ScratchpadHandover { .. } => "scratchpad_handover",
            MemoryRequest::VerifyAgenda { .. } => "verify_agenda",
            MemoryRequest::Resolve { .. } => "resolve",
            MemoryRequest::DreamAgenda { .. } => "dream_agenda",
            MemoryRequest::Distill { .. } => "distill",
        }
    }

    /// True when this op needs the embedder staged before a direct-open call.
    /// `recall`, `scratchpad_read`, and `scratchpad_grep` all run a fused
    /// Vector+Keyword plan; write/consolidate/handover ride ingest or the MQ
    /// drain and never embed. The socket path never needs this — contextd's
    /// warm handle already resolved the resident embedder.
    pub fn needs_embedder(&self) -> bool {
        matches!(
            self,
            MemoryRequest::Recall { .. }
                | MemoryRequest::ScratchpadRead { .. }
                | MemoryRequest::ScratchpadGrep { .. }
        )
    }
}

/// Engine-op response: the tool's own DTO as JSON on success, a tool-native
/// error `code` (plus a human `message`) on failure. The mcp proxy parses this
/// off the socket and re-wraps `data` into the matching rmcp `Json<T>` DTO, or
/// maps `code` back to an `rmcp::ErrorData`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MemoryResponse {
    Ok {
        data: Value,
        /// The storage URL the daemon actually served this op against.
        ///
        /// Split-routing containment (task #20). The socket route and the mcp
        /// proxy's Direct fallback derive their store from two INDEPENDENT
        /// sources — contextd resolves `LUNARIS_STORE_URL` /
        /// `~/.lunaris/contextd-moon.url` in its own process env, while the mcp
        /// engine is opened at boot from `--storage` / `LUNARIS_MCP_STORAGE`.
        /// Without this field nothing can tell whether falling back to Direct
        /// continues the same op stream or silently starts a second one in
        /// another Moon.
        ///
        /// `Option` + `#[serde(default)]` keeps the wire additive: a daemon
        /// older than this field simply reports `None`, which the proxy reads
        /// as "unknown" and treats exactly as it did before — an unknown store
        /// is the pre-existing status quo, not a reason to refuse.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        store: Option<String>,
    },
    Err {
        code: String,
        message: String,
    },
}

impl MemoryResponse {
    /// Success carrying the store the op was served against.
    #[must_use]
    pub fn ok(data: Value, store: impl Into<String>) -> Self {
        Self::Ok { data, store: Some(store.into()) }
    }
}

/// The ONE variant→handler dispatch. Both the contextd socket path and the mcp
/// direct-open fallback call this against a ready engine handle, so the two
/// surfaces execute byte-identical engine logic. Staging (recall only) is the
/// CALLER's concern — see [`MemoryRequest::needs_embedder`].
///
/// `lunaris` is `&Arc<Lunaris>` (not `&Lunaris`): the six engine handlers accept
/// it via deref coercion, and the scratchpad handlers clone the `Arc` to build a
/// `WorkingMemory` (which owns an `Arc<Lunaris>`). Both callers — the mcp
/// direct-open path (`state.lunaris`) and contextd (`handle_for_scope`) — already
/// hold an `Arc<Lunaris>`, so this is a borrow-form detail, not a wire change.
pub async fn dispatch(
    lunaris: &Arc<Lunaris>,
    scope: &Scope,
    request: MemoryRequest,
) -> Result<Value, ServiceError> {
    match request {
        MemoryRequest::Ingest { params, .. } => {
            to_value(crate::ingest::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Recall { params, .. } => {
            to_value(crate::recall::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Forget { params, .. } => {
            to_value(crate::forget::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Remember { params, .. } => {
            to_value(crate::remember::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Profile { params, .. } => {
            to_value(crate::profile::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::RecordDecision { params, .. } => {
            to_value(crate::record_decision::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::RecordEdit { params, .. } => {
            to_value(crate::record_edit::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Feedback { params, .. } => {
            to_value(crate::feedback::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Status { .. } => {
            to_value(crate::status::handle(lunaris, scope, crate::status::StatusParams {}).await?)
        }
        MemoryRequest::RepairVectors { params, .. } => {
            to_value(crate::repair_vectors::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::ScratchpadWrite { params, .. } => {
            to_value(crate::scratchpad_write::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::ScratchpadRead { params, .. } => {
            to_value(crate::scratchpad_read::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::ScratchpadGrep { params, .. } => {
            to_value(crate::scratchpad_grep::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::ScratchpadConsolidate { params, .. } => {
            to_value(crate::scratchpad_consolidate::handle(lunaris, scope, params).await?)
        }
        // Handover is INFALLIBLE (warn-and-continue); it returns a HandoverResponse
        // directly, never a Result — a failed drain must not error the caller.
        MemoryRequest::ScratchpadHandover { .. } => {
            to_value(crate::handover::handle(lunaris, scope).await)
        }
        MemoryRequest::VerifyAgenda { params, .. } => {
            to_value(crate::verify_agenda::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Resolve { params, .. } => {
            to_value(crate::resolve::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::DreamAgenda { params, .. } => {
            to_value(crate::dream_agenda::handle(lunaris, scope, params).await?)
        }
        MemoryRequest::Distill { params, .. } => {
            to_value(crate::distill::handle(lunaris, scope, params).await?)
        }
    }
}

/// Serialize a tool DTO to JSON. Our DTOs are plain structs, so this only fails
/// on a non-finite float (e.g. a NaN score) — surfaced as `InvalidInput` rather
/// than a panic on the socket/proxy path.
fn to_value<T: Serialize>(dto: T) -> Result<Value, ServiceError> {
    serde_json::to_value(dto)
        .map_err(|e| ServiceError::InvalidInput(format!("response serialization failed: {e}")))
}

#[cfg(test)]
mod tests {
    use lunaris_core::{Scope, StubEmbedder};
    use lunaris_test_harness::doubles::PortWithCaps;
    use lunaris_test_harness::{
        TestStorage, TestStore, open_test_engine_with_embedder, open_test_storage,
    };
    use serde_json::json;

    use super::*;

    /// A harness-issued ephemeral Moon.
    ///
    /// `dispatch` takes `&Arc<Lunaris>`, so the deref-transparent `TestEngine`
    /// is split into its parts. The returned [`TestStore`] owns the Moon child
    /// and must be bound for the test's lifetime — hence the third element.
    async fn fresh(scope_name: &str) -> (Arc<Lunaris>, Scope, TestStore) {
        let embedder = Arc::new(StubEmbedder::new(768));
        let (engine, store) = open_test_engine_with_embedder(embedder).await.into_parts();
        let scope = Scope::new(scope_name).unwrap();
        (Arc::new(engine), scope, store)
    }

    /// Same, but the engine's storage DECLARES no native queue. See
    /// `handover::tests` for why that is a capability double and not a second
    /// storage engine.
    async fn fresh_no_queue(scope_name: &str) -> (Arc<Lunaris>, Scope, TestStorage) {
        let storage = open_test_storage().await;
        let engine = Lunaris::with_parts(
            Arc::new(PortWithCaps::without_queue(storage.port())),
            Arc::new(StubEmbedder::new(768)),
            lunaris_core::HlcClock::new(0),
        );
        let scope = Scope::new(scope_name).unwrap();
        (Arc::new(engine), scope, storage)
    }

    /// needs_embedder: recall + scratchpad_read/grep touch vector search;
    /// write/consolidate/handover do not.
    #[test]
    fn needs_embedder_covers_read_and_grep_only() {
        let read = MemoryRequest::ScratchpadRead {
            scope: "s".into(),
            params: crate::scratchpad_read::ScratchpadReadParams {
                key: "k".into(),
                namespace: None,
            },
        };
        let grep = MemoryRequest::ScratchpadGrep {
            scope: "s".into(),
            params: crate::scratchpad_grep::ScratchpadGrepParams {
                pattern: "".into(),
                namespace: None,
            },
        };
        let write = MemoryRequest::ScratchpadWrite {
            scope: "s".into(),
            params: crate::scratchpad_write::ScratchpadWriteParams {
                key: "k".into(),
                value: json!(1),
                namespace: None,
            },
        };
        let consolidate = MemoryRequest::ScratchpadConsolidate {
            scope: "s".into(),
            params: crate::scratchpad_consolidate::ScratchpadConsolidateParams { namespace: None },
        };
        let handover = MemoryRequest::ScratchpadHandover { scope: "s".into() };
        assert!(read.needs_embedder(), "read must stage");
        assert!(grep.needs_embedder(), "grep must stage");
        assert!(!write.needs_embedder(), "write must NOT stage");
        assert!(!consolidate.needs_embedder(), "consolidate must NOT stage");
        assert!(!handover.needs_embedder(), "handover must NOT stage");
    }

    #[test]
    fn op_labels_for_scratchpad_variants() {
        assert_eq!(
            MemoryRequest::ScratchpadHandover { scope: "s".into() }.op(),
            "scratchpad_handover"
        );
    }

    /// scratchpad_write then scratchpad_read, both THROUGH dispatch — proves the
    /// wire variants route to the shared handlers end to end.
    #[tokio::test]
    async fn dispatch_scratchpad_write_then_read_round_trip() {
        let (lunaris, scope, _store) = fresh("test-proto-rt").await;
        let write = MemoryRequest::ScratchpadWrite {
            scope: scope.as_str().into(),
            params: crate::scratchpad_write::ScratchpadWriteParams {
                key: "proto-key".into(),
                value: json!({"v": 7}),
                namespace: None,
            },
        };
        dispatch(&lunaris, &scope, write).await.unwrap();

        let read = MemoryRequest::ScratchpadRead {
            scope: scope.as_str().into(),
            params: crate::scratchpad_read::ScratchpadReadParams {
                key: "proto-key".into(),
                namespace: None,
            },
        };
        let data = dispatch(&lunaris, &scope, read).await.unwrap();
        assert_eq!(data.get("found").and_then(|f| f.as_bool()), Some(true));
        assert_eq!(data.get("value"), Some(&json!({"v": 7})));
    }

    /// Handover via dispatch on a non-native-queue backend returns Ok with an
    /// advisory skip status — never an error.
    ///
    /// **Re-expressed in 0.7.0** (same move as
    /// `handover::tests::handover_on_no_queue_backend_skips`): the assertion
    /// *is* `queue_native == false`, so it is made against a live Moon with
    /// that one bit cleared rather than against a second storage engine.
    #[tokio::test]
    async fn dispatch_handover_on_no_queue_backend_is_ok_and_skips() {
        let (lunaris, scope, _storage) = fresh_no_queue("test-proto-handover").await;
        let req = MemoryRequest::ScratchpadHandover { scope: scope.as_str().into() };
        let data = dispatch(&lunaris, &scope, req).await.unwrap();
        assert_eq!(data.get("status").and_then(|s| s.as_str()), Some("skipped_no_queue"));
    }

    /// deny_unknown_fields on a scratchpad wire params rejects a smuggled scope.
    #[test]
    fn scratchpad_params_reject_smuggled_scope() {
        let raw = json!({"key": "k", "value": 1, "scope": "other"});
        let parsed: Result<crate::scratchpad_write::ScratchpadWriteParams, _> =
            serde_json::from_value(raw);
        assert!(parsed.is_err(), "deny_unknown_fields must reject smuggled scope");
    }
}
