//! W4.2 — human-readable memory artifacts: the persona / scenario view.
//!
//! `memory.recall` answers a question. It cannot answer "what do you actually
//! know about me?", and that question is the one that made the curation gap
//! visible in the first place: a store holding 233k episodes could not produce
//! a page a human would read. `memory.profile` renders the scope's captured
//! knowledge as prose, grouped by kind.
//!
//! Two properties carry most of the value here.
//!
//! **An empty profile must say so.** A blank document and "this store has
//! never had anything curated into it" are the same bytes but opposite
//! meanings, and the blank one reads as "nothing to report" when it means
//! "nothing was ever captured". The empty rendering names the tool that fixes
//! it.
//!
//! **Telemetry must not appear.** The profile is a knowledge artifact. A
//! store where 91.6% of episodes are `lunaris:tool_call` envelopes would
//! otherwise render 91.6% shell commands and look busy while saying nothing.

use std::sync::Arc;

use lunaris::EpisodeBuilder;
use lunaris_core::{Scope, StubEmbedder};
use lunaris_memory_service::profile::{self, ProfileParams};
use lunaris_memory_service::remember::{self, RememberKind, RememberParams};
use lunaris_test_harness::{TestEngine, open_test_engine_with_embedder};

async fn make_engine(scope_name: &str) -> (TestEngine, Scope) {
    let embedder = Arc::new(StubEmbedder::new(768));
    let lunaris = open_test_engine_with_embedder(embedder).await;
    let scope = Scope::new(scope_name).unwrap();
    (lunaris, scope)
}

async fn remember_one(
    lunaris: &TestEngine,
    scope: &Scope,
    kind: RememberKind,
    content: &str,
    why: &str,
) {
    remember::handle(
        lunaris,
        scope,
        RememberParams {
            kind,
            content: content.to_owned(),
            why: Some(why.to_owned()),
            tags: None,
            dedupe_key: None,
        },
    )
    .await
    .expect("remember");
}

/// An empty scope renders an honest, actionable page — never a blank one.
#[tokio::test]
async fn an_empty_scope_says_nothing_was_captured_rather_than_rendering_blank() {
    let (lunaris, scope) = make_engine("test-profile-empty").await;

    let out = profile::handle(&lunaris, &scope, ProfileParams::default())
        .await
        .expect("profile an empty scope");

    assert_eq!(out.total, 0, "an empty scope has captured nothing");
    assert!(
        !out.markdown.trim().is_empty(),
        "an empty profile rendered blank — indistinguishable from a failed read"
    );
    assert!(
        out.markdown.contains("memory.remember"),
        "an empty profile must name the tool that fills it, or it reports a problem \
         without its remedy: {}",
        out.markdown
    );
}

/// Captured memories appear under their kind, in prose, with their rationale.
#[tokio::test]
async fn every_kind_gets_its_own_readable_section() {
    let (lunaris, scope) = make_engine("test-profile-sections").await;

    remember_one(
        &lunaris,
        &scope,
        RememberKind::Decision,
        "embeddings live in Moon",
        "one round trip instead of three",
    )
    .await;
    remember_one(
        &lunaris,
        &scope,
        RememberKind::Fix,
        "recall returned nothing",
        "the scope was never threaded through",
    )
    .await;
    remember_one(
        &lunaris,
        &scope,
        RememberKind::Preference,
        "red/green TDD on every feature",
        "Tin asked for it directly",
    )
    .await;
    remember_one(
        &lunaris,
        &scope,
        RememberKind::Constraint,
        "recall must stay under 25ms at 100k docs",
        "it is the core contract",
    )
    .await;

    let out = profile::handle(&lunaris, &scope, ProfileParams::default()).await.expect("profile");

    assert_eq!(out.total, 4, "all four captures must be counted: {:?}", out.counts);

    for probe in [
        "embeddings live in Moon",
        "one round trip instead of three",
        "the scope was never threaded through",
        "red/green TDD on every feature",
        "recall must stay under 25ms",
    ] {
        assert!(out.markdown.contains(probe), "profile lost {probe:?}:\n{}", out.markdown);
    }

    // Grouped, not one flat list: a reader scanning for "what does it think I
    // prefer" must find a place to look.
    for heading in ["Decisions", "Fixes", "Preferences", "Constraints"] {
        assert!(out.markdown.contains(heading), "no {heading} section:\n{}", out.markdown);
    }
    assert!(
        !out.markdown.contains('{'),
        "the profile rendered a JSON envelope — the exact failure that made 87k community \
         summaries unreadable:\n{}",
        out.markdown
    );
}

/// Raw telemetry is substrate. It is never knowledge, so it never appears here.
#[tokio::test]
async fn raw_telemetry_never_appears_in_the_profile() {
    let (lunaris, scope) = make_engine("test-profile-no-telemetry").await;
    let scoped = lunaris.scoped(scope.clone());

    for source in ["lunaris:tool_call:post", "lunaris:pre_tool_use"] {
        scoped
            .ingest(EpisodeBuilder::new(
                source,
                r#"{"tool":"Bash","command":"ls -la /srv/zephyr","exit":0}"#,
            ))
            .await
            .expect("seed telemetry");
    }
    remember_one(
        &lunaris,
        &scope,
        RememberKind::Fix,
        "the gateway needs draining before rollover",
        "connections dropped otherwise",
    )
    .await;

    let out = profile::handle(&lunaris, &scope, ProfileParams::default()).await.expect("profile");

    assert_eq!(out.total, 1, "only the captured fix counts as knowledge: {:?}", out.counts);
    assert!(
        !out.markdown.contains("ls -la"),
        "a shell command reached the knowledge artifact:\n{}",
        out.markdown
    );
    assert!(
        out.markdown.contains("needs draining before rollover"),
        "the captured fix must be present — otherwise this passes because the profile is \
         empty, not because telemetry was filtered:\n{}",
        out.markdown
    );
}
