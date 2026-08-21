//! The built-in sample corpus `lunaris try` ingests.
//!
//! Six short observations, shaped like the things an agent actually
//! accumulates: a decision with its reasoning, an operational constraint, an
//! incident, a person, a convention and a plan. That mix is deliberate — a
//! trial corpus of interchangeable lorem paragraphs proves the pipe runs but
//! teaches nothing about what a memory store is FOR, and the first minute is
//! the only minute where that lesson is cheap to give.
//!
//! Each entry is under 300 characters so it is one chunk, one embedding and
//! one hydrate — the trial's wall-clock budget is dominated by the model, and
//! the corpus must not add to it.
//!
//! Every entry carries a stable [`SampleMemory::dedupe_key`]. That is what
//! makes the durable `~/.lunaris/try` data directory safe to keep: a second
//! `lunaris try` returns the prior LSN for each sample instead of writing a
//! second copy, so the store cannot grow without bound no matter how many
//! times someone runs the command.

/// One built-in observation.
pub(crate) struct SampleMemory {
    /// Logical origin, rendered in the hit list so the output looks like a
    /// real store rather than a flat list of strings.
    pub(crate) source: &'static str,
    /// The observation text.
    pub(crate) content: &'static str,
    /// Stable idempotency key — see the module docs.
    pub(crate) dedupe_key: &'static str,
}

/// The corpus. Order is not meaningful; retrieval decides what comes back.
pub(crate) const SAMPLES: &[SampleMemory] = &[
    SampleMemory {
        source: "sample/decisions",
        content: "We chose Moon over Postgres for the memory substrate because recall has to \
                  stay under 25 ms at a hundred thousand documents per scope, and Moon answers \
                  vector search, BM25 and graph traversal in one round trip instead of three.",
        dedupe_key: "lunaris-try/sample/decisions/moon-over-postgres",
    },
    SampleMemory {
        source: "sample/decisions",
        // Deliberately says "one shard" rather than the literal CLI flag: the
        // markdown chunker runs pulldown_cmark with ENABLE_SMART_PUNCTUATION,
        // which rewrites `--` to an en dash (documented in
        // `lunaris/tests/working_memory_roundtrip.rs`). Real and worth fixing,
        // but the front door is not the place to demonstrate it.
        content: "Lunaris runs against a single-shard Moon. A sharded Moon rejects the \
                  multi-key transaction the ingest path commits, so every deployment pins \
                  itself to one shard until cross-shard transactions land.",
        dedupe_key: "lunaris-try/sample/decisions/single-shard",
    },
    SampleMemory {
        source: "sample/incidents",
        content: "The 2 a.m. page in March was a wedged Moon: an append-only-file rewrite \
                  never completed and every recall timed out. BGREWRITEAOF recovered it where \
                  BGSAVE wrote nothing, and a liveness probe now catches it in seconds.",
        dedupe_key: "lunaris-try/sample/incidents/wedged-moon",
    },
    SampleMemory {
        source: "sample/people",
        content: "Priya owns the retrieval DSL. Ask her before adding a leg to the production \
                  root: three surfaces once drifted apart because each one planned its own \
                  recall, and unifying them cost a week.",
        dedupe_key: "lunaris-try/sample/people/priya-retrieval-dsl",
    },
    SampleMemory {
        source: "sample/conventions",
        content: "Never hold a lock across an await. Snapshot under read() or write(), drop the \
                  guard, then await. This is the rule that keeps the ingest path from \
                  deadlocking under concurrent load.",
        dedupe_key: "lunaris-try/sample/conventions/no-lock-across-await",
    },
    SampleMemory {
        source: "sample/roadmap",
        content: "Next quarter we publish two named operating points: a fast path with the \
                  reranker off at roughly 20 ms, and a quality path with it on. An unlabelled \
                  latency number is the defect this is meant to kill.",
        dedupe_key: "lunaris-try/sample/roadmap/two-operating-points",
    },
];

/// The question `lunaris try` asks when the caller supplies none.
///
/// Chosen so the corpus has an obvious right answer a human can grade at a
/// glance: if the Moon-over-Postgres memory is not near the top, the reader
/// learns something true about the ranking without needing a benchmark.
pub(crate) const DEFAULT_QUERY: &str = "why did we pick Moon instead of Postgres";

/// Recall breadth for the trial: five of six, so the output demonstrates that
/// retrieval RANKS rather than dumps. A trial that returns the whole corpus
/// would look identical to `cat`.
pub(crate) const DEFAULT_K: usize = 5;

#[cfg(test)]
mod tests {
    use super::*;

    /// Dedupe keys are the only thing standing between a durable trial dir and
    /// unbounded growth. A copy-paste collision would silently drop a sample.
    #[test]
    fn dedupe_keys_are_unique() {
        let mut keys: Vec<&str> = SAMPLES.iter().map(|s| s.dedupe_key).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "duplicate dedupe_key in the sample corpus: {keys:?}");
    }

    /// The trial must rank, not dump — see [`DEFAULT_K`].
    #[test]
    fn default_k_is_smaller_than_the_corpus() {
        assert!(
            DEFAULT_K < SAMPLES.len(),
            "k ({DEFAULT_K}) must be below the corpus size ({}) or the demo cannot show \
             ranking at all",
            SAMPLES.len()
        );
    }

    /// Each entry must fit in one chunk and in the 260-char curated snippet the
    /// recall DTO returns, or the rendered hit is a truncation and the reader
    /// cannot tell whether retrieval worked.
    #[test]
    fn every_sample_fits_a_single_rendered_snippet() {
        for s in SAMPLES {
            assert!(
                s.content.len() <= 260,
                "{}: {} chars — the recall DTO curates to 260, so this would render \
                 truncated",
                s.dedupe_key,
                s.content.len()
            );
        }
    }
}
