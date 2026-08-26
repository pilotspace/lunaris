//! W4.3b — the direct-capture write path and its four kinds.
//!
//! The curation-gap census found nothing distilled at any layer: 91.6% of a
//! live store's 233k episodes were raw tool telemetry, and the community tree
//! never built above its leaves. The chosen fix (decision #1) is not "make
//! RAPTOR summarise better" — you cannot summarise `ls -la` into wisdom — it
//! is to capture knowledge DIRECTLY, when it happens, as first-class memory.
//!
//! Decision #3 fixed what counts as worth keeping, and it is exactly four
//! kinds: decisions with their rationale, failures with their fixes, the
//! user's preferences and working style, and project state and constraints.
//! `memory.remember` is the one write path for all four.
//!
//! This test pins the contract an agent depends on: every kind round-trips
//! through the production handlers over a real store, the kind survives on the
//! source so ranking and injection can see it, and the rationale is not lost.

use std::sync::Arc;

use lunaris_core::{Scope, StubEmbedder};
use lunaris_memory_service::recall::{self, RecallParams};
use lunaris_memory_service::remember::{self, RememberKind, RememberParams};
use lunaris_test_harness::{TestEngine, open_test_engine_with_embedder};

async fn make_engine(scope_name: &str) -> (TestEngine, Scope) {
    let embedder = Arc::new(StubEmbedder::new(768));
    let lunaris = open_test_engine_with_embedder(embedder).await;
    let scope = Scope::new(scope_name).unwrap();
    (lunaris, scope)
}

fn params(kind: RememberKind, content: &str, why: Option<&str>) -> RememberParams {
    RememberParams {
        kind,
        content: content.to_owned(),
        why: why.map(str::to_owned),
        tags: None,
        dedupe_key: None,
    }
}

/// All four kinds are writable, and each lands under a source that names it.
///
/// The source matters as much as the content: `injectable_source` and
/// `source_priority` in the hook both read it, so a kind that does not reach
/// the source is a memory that cannot be ranked or filtered as what it is.
#[tokio::test]
async fn every_capture_kind_writes_a_memory_that_names_its_kind() {
    let (lunaris, scope) = make_engine("test-remember-kinds").await;

    let cases = [
        (RememberKind::Decision, "we store embeddings in Moon, not Postgres", "one round trip"),
        (RememberKind::Fix, "recall returned nothing until the scope was threaded", "wrong scope"),
        (RememberKind::Preference, "Tin wants red/green TDD on every feature", "stated directly"),
        (RememberKind::Constraint, "the recall budget is 25ms at 100k docs", "the core contract"),
    ];

    let mut seen = Vec::new();
    for (kind, content, why) in cases {
        let out = remember::handle(&lunaris, &scope, params(kind, content, Some(why)))
            .await
            .unwrap_or_else(|e| panic!("remember({}) failed: {e}", kind.as_str()));

        assert!(!out.was_duplicate, "a fresh {} must not report a dedupe hit", kind.as_str());
        assert_eq!(
            out.source,
            format!("{}:{}", kind.as_str(), scope.as_str()),
            "the source must carry the kind — the hook ranks and filters on it"
        );
        assert!(
            !seen.contains(&out.source),
            "two kinds shared source {} — they would be indistinguishable to the reader",
            out.source
        );
        seen.push(out.source);
    }
    assert_eq!(seen.len(), 4, "all four capture kinds must be writable");
}

/// A captured memory comes back through the ordinary read path, rationale
/// included. A write nobody can read is not a memory.
#[tokio::test]
async fn a_remembered_memory_is_recallable_with_its_rationale() {
    let (lunaris, scope) = make_engine("test-remember-roundtrip").await;

    remember::handle(
        &lunaris,
        &scope,
        params(
            RememberKind::Fix,
            "the zephyr relay dropped connections during rollover",
            Some("the gateway was never drained before the switch"),
        ),
    )
    .await
    .expect("remember a fix");

    let found = recall::handle(
        &lunaris,
        &scope,
        RecallParams {
            query: "zephyr relay rollover".into(),
            k: 5,
            filters: None,
            as_of: None,
            raw: false,
        },
    )
    .await
    .expect("recall");

    let hit = found.hits.first().expect("the remembered fix must be recallable");
    assert!(
        hit.content.contains("zephyr relay dropped connections"),
        "the captured content must survive the round trip: {}",
        hit.content
    );
    assert!(
        hit.content.contains("never drained"),
        "the rationale is the part worth keeping — a fix without its why is a changelog line: {}",
        hit.content
    );
}

/// `dedupe_key` makes capture safe to retry. An agent that re-runs a step must
/// not double-write the lesson it already recorded.
#[tokio::test]
async fn the_same_dedupe_key_writes_once() {
    let (lunaris, scope) = make_engine("test-remember-dedupe").await;

    let mut p = params(RememberKind::Constraint, "MSRV is pinned at 1.94", None);
    p.dedupe_key = Some("msrv-pin".to_owned());
    let first = remember::handle(&lunaris, &scope, p).await.expect("first write");
    assert!(!first.was_duplicate, "the first write is not a duplicate");

    let mut p = params(RememberKind::Constraint, "MSRV is pinned at 1.94", None);
    p.dedupe_key = Some("msrv-pin".to_owned());
    let second = remember::handle(&lunaris, &scope, p).await.expect("second write");
    assert!(second.was_duplicate, "the same dedupe key must not write a second episode");
    assert_eq!(first.lsn, second.lsn, "a dedupe hit returns the prior LSN");
}
