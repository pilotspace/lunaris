//! Bench fixtures — the canonical inputs each bench reads.
//!
//! ## Why re-export instead of copy?
//!
//! The 12 KB markdown fixture lives at
//! `crates/lunaris-ingest/tests/fixtures/12kb_doc.md` (Plan 02-01 produced it
//! with hand-tuned heading depth so it lands 4–8 chunks at target=500 /
//! overlap=100). Duplicating the bytes here would give us TWO sources of truth
//! that drift apart. Instead we `include_str!` the fixture from its canonical
//! location, so any chunk-count assertion in the benches stays consistent with
//! the chunker test in `lunaris-ingest`.
//!
//! ## Fixture statistics (Plan 02-01 SUMMARY)
//!
//! - Bytes: 10277 (~10.0 KiB)
//! - Words: 1538
//! - Chunk count at target=500, overlap=100: 4–8 (validated by
//!   `lunaris-ingest::tests::chunker::twelve_kb_markdown_yields_4_to_8_chunks`)
//! - Heading depth: 3 levels (`#`, `##`, `###`) so heading_path lineage is
//!   non-trivial.

/// Returns the contents of the canonical 12 KB markdown fixture.
///
/// Used by `benches/ingest_hot_path.rs` to drive the ingest p50/p99 measurement
/// against a payload that matches the blueprint §4.1 latency budget table
/// (which assumes 12 KB markdown input with 5 chunks × 32-batch embed).
pub fn twelve_kb_markdown_fixture() -> &'static str {
    include_str!("../../lunaris-ingest/tests/fixtures/12kb_doc.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_resolves_at_compile_time() {
        // Compile-time `include_str!` would fail the build if the path is wrong;
        // this test asserts the bytes also match the Plan 02-01 size budget so
        // a future move of the fixture doesn't silently change bench inputs.
        let s = twelve_kb_markdown_fixture();
        assert!(
            s.len() >= 9_000 && s.len() <= 11_000,
            "fixture size out of band: {} bytes (expected 10277 ± 1 KiB per Plan 02-01)",
            s.len()
        );
    }

    #[test]
    fn fixture_has_three_heading_levels() {
        // Heading-path lineage matters for the chunker; if a future edit
        // flattens the doc to a single H1 we lose the heading-path coverage.
        // Accept H1 either at the start of the file OR preceded by a newline.
        let s = twelve_kb_markdown_fixture();
        assert!(s.starts_with("# ") || s.contains("\n# "), "missing H1");
        assert!(s.contains("\n## "), "missing H2");
        assert!(s.contains("\n### "), "missing H3");
    }
}
