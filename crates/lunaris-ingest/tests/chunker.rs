//! Chunker integration tests — INGEST-01 invariants.

use lunaris_ingest::{ChunkDraft, chunk_markdown, est_token_count};

const TWELVE_KB_FIXTURE: &str = include_str!("fixtures/12kb_doc.md");

#[test]
fn chunks_a_simple_doc() {
    // target=2 overlap=0 forces frequent emits so we can verify heading_path
    // tracking on a tiny doc.
    let md = "# A\nfoo bar baz\n\n## B\nqux quux corge\n";
    let chunks = chunk_markdown(md, 2, 0);
    assert!(!chunks.is_empty(), "non-empty doc must produce chunks");
    // The first chunk's heading path is established the moment we emit; with
    // target=2 we emit very early so the first chunk should be under H1 only.
    assert_eq!(
        chunks[0].heading_path,
        vec!["A".to_string()],
        "first chunk should be inside the first H1"
    );
    // At least one chunk should record the nested H1 -> H2 lineage.
    let nested = chunks
        .iter()
        .any(|c| c.heading_path == vec!["A".to_string(), "B".to_string()]);
    assert!(
        nested,
        "expected at least one chunk under the H1 -> H2 lineage; got: {:#?}",
        chunks.iter().map(|c| &c.heading_path).collect::<Vec<_>>()
    );
}

#[test]
fn twelve_kb_markdown_yields_4_to_8_chunks() {
    let bytes = TWELVE_KB_FIXTURE.len();
    assert!(
        (10_000..=14_000).contains(&bytes),
        "fixture must be 10–14 KB to satisfy the planning contract; got {bytes}"
    );
    let chunks = chunk_markdown(TWELVE_KB_FIXTURE, 500, 100);
    let n = chunks.len();
    assert!(
        (4..=8).contains(&n),
        "12 KB fixture must yield 4–8 chunks at target=500 overlap=100; got {n}"
    );
    // Every chunk should carry a non-empty heading path (the fixture has H1 + H2 + H3 throughout).
    for (i, c) in chunks.iter().enumerate() {
        assert!(
            !c.heading_path.is_empty(),
            "chunk {i} has empty heading_path; text starts: {}",
            &c.text.chars().take(80).collect::<String>()
        );
    }
    // Offsets are monotonic from 0.
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(c.offset, i as u32, "offsets must be monotonic from 0");
    }
}

#[test]
fn overlap_tail_is_carried() {
    // target=10 overlap=3 — the chunker should emit at least 2 chunks for a doc
    // long enough; the second chunk's text should start with the last 3 words
    // of the first chunk's text.
    let body: String = (0..40)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let chunks = chunk_markdown(&body, 10, 3);
    assert!(chunks.len() >= 2, "expected at least 2 chunks, got {}", chunks.len());
    let first_overlap = &chunks[0].overlap_tail;
    let first_overlap_words: Vec<&str> = first_overlap.split_whitespace().collect();
    assert_eq!(
        first_overlap_words.len(),
        3,
        "overlap_tail should hold exactly 3 words; got {first_overlap_words:?}"
    );
    let second_text = &chunks[1].text;
    assert!(
        second_text.starts_with(first_overlap),
        "second chunk should start with the first chunk's overlap_tail.\n  overlap_tail: {first_overlap}\n  second_text: {second_text}"
    );
}

#[test]
fn token_estimate_matches_heuristic() {
    // Sanity: the chunker's emit threshold uses the same heuristic Plan 02-04
    // will reuse for the bench fixture. 100 words × 1.3 = 130.
    let s = (0..100).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
    assert_eq!(est_token_count(&s), 130);
}

#[test]
fn chunks_preserve_ordering_in_complex_doc() {
    let chunks: Vec<ChunkDraft> = chunk_markdown(TWELVE_KB_FIXTURE, 500, 100);
    // Find the indices of two known heading_path chains and assert ordering.
    let storage_idx = chunks.iter().position(|c| {
        c.heading_path
            .iter()
            .any(|h| h.contains("Storage Contracts"))
    });
    let recall_idx = chunks
        .iter()
        .position(|c| c.heading_path.iter().any(|h| h.contains("Recall Hot Path")));
    if let (Some(s), Some(r)) = (storage_idx, recall_idx) {
        assert!(
            s < r,
            "Storage Contracts section should chunk before Recall Hot Path"
        );
    }
}
