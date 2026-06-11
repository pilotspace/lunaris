//! ADD task `mq-typed-client` — static contract invariant.
//!
//! §3 CONTRACT (FROZEN @ v1): "FORBIDDEN: dotted MQ.PUSH / MQ.POP
//! (server-unhandled) and any redis::cmd(\"MQ\") in this module."
//!
//! This scan runs on plain `cargo test` (no `moon-it` feature, no live Moon)
//! so the invariant is CI-pinned the same way INGEST-04 is grep-pinned.

const QUEUE_RS: &str = include_str!("../src/queue.rs");

/// Zero raw `redis::cmd("MQ")` call sites may remain in queue.rs — the typed
/// `moondb::MqClient` is the only sanctioned MQ surface.
#[test]
fn queue_rs_uses_only_typed_mq_client() {
    let raw_mq_sites: Vec<usize> = QUEUE_RS
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains("redis::cmd(\"MQ\")") || l.contains("cmd(\"MQ\")"))
        .map(|(n, _)| n + 1)
        .collect();
    assert!(
        raw_mq_sites.is_empty(),
        "queue.rs still issues raw RESP MQ commands at lines {raw_mq_sites:?}; \
         the mq-typed-client contract requires the typed moondb MqClient"
    );
}

/// Dotted MQ command spellings are server-unhandled (verified 2026-06-09,
/// P-C spike) and must never appear.
#[test]
fn queue_rs_never_uses_dotted_mq_commands() {
    assert!(
        !QUEUE_RS.contains("MQ.PUSH") && !QUEUE_RS.contains("MQ.POP"),
        "queue.rs references dotted MQ.PUSH/MQ.POP — Moon's dispatcher does not handle them"
    );
}
