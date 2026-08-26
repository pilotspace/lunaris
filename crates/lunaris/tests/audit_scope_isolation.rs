//! W4.6 — audit events must be published under the scope that produced them.
//!
//! ## The defect
//!
//! `lunaris_core::audit::Publisher::publish` is the ONE publish path in the
//! workspace with no `scope` parameter. Every other caller —
//! `lunaris-hook`'s embed-promotion, `scratchpad_consolidate`, the conformance
//! harness — threads a real scope into `StoragePort::publish`. The audit impl
//! hardcodes `Scope::dev()`:
//!
//! ```text
//! impl Publisher for Arc<dyn StoragePort> {
//!     async fn publish(&self, topic, partition, payload) -> ... {
//!         // scope-dev-allowed: audit-publish-trait-surface
//!         StoragePort::publish(self.as_ref(), &Scope::dev(), topic, partition, payload)
//! ```
//!
//! Moon namespaces queue topics per scope (`mq_topic` renders
//! `lunaris:{scope}:{name}`), so this has two consequences, and the second is
//! the one that makes it a governance defect rather than a tidiness one:
//!
//! 1. **A tenant's own audit trail is empty.** `ScopedLunaris::forget` runs
//!    under scope A, but its receipt is published to `Scope::dev()`'s topic.
//!    An operator subscribing to A's audit stream — the only stream they are
//!    entitled to read — sees nothing, for an operation that definitely
//!    happened.
//! 2. **Every tenant's audit events land in one shared partition.** Whoever
//!    can read the `dev` scope reads everybody's forget receipts, consolidator
//!    promotions and verifier arbitrations.
//!
//! ## Why this test drives `ScopedLunaris::forget` and not the trait
//!
//! `forget_scoped` already receives the scope and drops it at the audit call,
//! so a unit test against `Publisher` would prove the trait is scope-less —
//! which is visible by reading it. What needs proving is that the PRODUCTION
//! path, holding a real scope, still fails to use it. Two distinct real scopes
//! are used rather than comparing against `Scope::dev()`, so the assertion
//! does not depend on introspecting the migration crutch it is removing.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use lunaris::{ForgetTarget, Lunaris, ScopeSpec};
use lunaris_core::audit::AUDIT_TOPIC;
use lunaris_core::{Scope, StoragePort};

/// How long to wait for an audit event that SHOULD arrive. `subscribe` polls
/// `MQ POP` roughly three times a second, so this is ~15 polls of headroom.
const EXPECT_WITHIN: Duration = Duration::from_secs(5);
/// How long to wait before concluding an event will NOT arrive. Shorter on
/// purpose — this bound is paid on every run, and a receipt that has not shown
/// up in ~6 polls is not in flight.
const ABSENT_AFTER: Duration = Duration::from_secs(2);

fn moon_url() -> Option<String> {
    match std::env::var("MOON_URL").ok().filter(|s| !s.trim().is_empty()) {
        Some(u) => Some(u),
        None => {
            lunaris_test_harness::strict_skip::note_unavailable(
                "MOON_URL unset — audit_scope_isolation needs a live Moon",
            );
            None
        }
    }
}

/// Drain one message from `scope`'s audit topic, or `None` on timeout.
///
/// NOT `queue_depth`: that method is documented as reporting dead-letter depth
/// (`MQ DLQLEN`), and `publish` creates every topic with `MAXDELIVERY 0`, which
/// disables DLQ routing — so it returns 0 for a healthy and an empty topic
/// alike (see `lunaris-storage-moon::queue::queue_length`). Reading the stream
/// is the only observation that distinguishes them.
async fn next_audit(
    storage: &Arc<dyn StoragePort>,
    scope: &Scope,
    wait: Duration,
) -> Option<Vec<u8>> {
    let mut stream = storage
        .subscribe(scope, "w46-audit-probe", AUDIT_TOPIC, 0)
        .await
        .expect("subscribe to the audit topic must succeed");
    match tokio::time::timeout(wait, stream.next()).await {
        Ok(Some(Ok(msg))) => Some(msg.payload.to_vec()),
        Ok(Some(Err(e))) => panic!("audit subscribe stream errored: {e}"),
        Ok(None) => None,
        Err(_) => None,
    }
}

#[tokio::test]
async fn a_forget_receipt_is_audited_under_the_scope_that_produced_it() {
    let Some(url) = moon_url() else { return };

    let lunaris = Arc::new(Lunaris::open(&url).await.expect("open"));
    let scope_a = Scope::new(format!("w46a{}", ulid::Ulid::new())).expect("scope a");
    let scope_b = Scope::new(format!("w46b{}", ulid::Ulid::new())).expect("scope b");
    let storage: Arc<dyn StoragePort> = lunaris.storage();

    // Instrument self-check FIRST. A scope whose audit topic was never written
    // reads exactly like a scope whose receipt went somewhere else, so prove
    // the observation can see a message at all before trusting its silence.
    // Without this the assertions below could not pass however the production
    // code is fixed — which is how the first draft of this test, built on
    // `queue_depth`, failed.
    let probe = Scope::new(format!("w46p{}", ulid::Ulid::new())).expect("probe scope");
    storage
        .publish(&probe, AUDIT_TOPIC, 0, bytes::Bytes::from_static(b"{\"probe\":true}"))
        .await
        .expect("publish to the audit topic must succeed");
    let seen = next_audit(&storage, &probe, EXPECT_WITHIN).await;
    assert_eq!(
        seen.as_deref(),
        Some(&b"{\"probe\":true}"[..]),
        "measurement is broken, not the code under test: a direct scoped publish to a scope's \
         audit topic was not readable back from that same topic"
    );

    // A dry-run forget in scope A. Dry-run on purpose: it still publishes a
    // receipt (`preview: true`) but writes nothing, so the test cannot be
    // confused by deletion side effects.
    let request = ForgetTarget::Scope(ScopeSpec::BySource("w46/never-written".into())).dry_run();
    lunaris.scoped(scope_a.clone()).forget(request).await.expect("dry-run forget must succeed");

    let on_b = next_audit(&storage, &scope_b, ABSENT_AFTER).await;
    assert!(
        on_b.is_none(),
        "an uninvolved scope received scope A's audit receipt: {:?}",
        on_b.map(|b| String::from_utf8_lossy(&b).into_owned())
    );

    let on_a = next_audit(&storage, &scope_a, EXPECT_WITHIN).await;
    let payload = on_a.expect(
        "the forget ran under scope A but produced NO audit event on A's topic. \
         `Publisher::publish` takes no scope and hardcodes `Scope::dev()`, so the receipt went \
         to a partition this tenant cannot read — their own audit trail is empty for an \
         operation that definitely happened, and everybody else's receipts are piled into that \
         same shared partition.",
    );
    let json: serde_json::Value =
        serde_json::from_slice(&payload).expect("the audit payload must be a JSON AuditEvent");
    assert_eq!(
        json.get("kind").and_then(|v| v.as_str()),
        Some("Forget"),
        "scope A's audit topic carried something other than the forget receipt: {json}"
    );
}
