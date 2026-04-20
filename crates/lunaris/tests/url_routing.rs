//! URL-scheme routing tests for `lunaris::open(url)`.

use lunaris::*;

#[tokio::test]
async fn moon_url_returns_handle_with_moon_capabilities() {
    let h = lunaris::open("moon://localhost:6390").await.expect("moon URL should parse");
    let cap = h.capabilities();
    assert!(cap.bi_temporal_native, "Moon should report bi_temporal_native=true");
    assert!(cap.graph_native);
    assert!(cap.rerank_native, "Moon should report rerank_native=true");
    assert!(cap.queue_native);
    assert_eq!(cap.max_vector_dim, 768, "Moon profile pinned to 768d");
}

#[tokio::test]
async fn postgres_url_returns_handle_with_postgres_capabilities() {
    let h = lunaris::open("postgres://localhost/lunaris").await.expect("postgres URL should parse");
    let cap = h.capabilities();
    assert!(!cap.bi_temporal_native, "Postgres bi-temporal is emulated");
    assert!(cap.graph_native, "AGE provides graph");
    assert!(!cap.rerank_native, "no native cross-encoder on Postgres");
    assert!(cap.queue_native, "pgmq provides queue");
    assert_eq!(cap.max_vector_dim, 1536);
}

#[tokio::test]
async fn rejects_unknown_scheme() {
    // We can't use `.expect_err()` here — `Arc<dyn StoragePort>` does not implement
    // `Debug`, so `Result::expect_err` (which formats the Ok variant on failure)
    // won't compile. Match the result by hand instead.
    match lunaris::open("rediss://foo").await {
        Ok(_) => panic!("rediss must be rejected"),
        Err(e) => {
            let s = e.to_string();
            assert!(s.contains("storage"), "error must surface as a Storage variant: {s}");
            assert!(s.contains("rediss"), "error must mention the offending scheme: {s}");
        }
    }
}

#[tokio::test]
async fn rejects_garbage_url() {
    match lunaris::open("not a url").await {
        Ok(_) => panic!("garbage must be rejected"),
        Err(e) => assert!(e.to_string().contains("storage")),
    }
}

#[tokio::test]
async fn skeleton_io_methods_return_not_supported() {
    let h = lunaris::open("moon://localhost:6390").await.unwrap();
    let r = h.atomic_write(&[]).await;
    assert!(
        matches!(r, Err(StorageError::NotSupported(_))),
        "Phase 1 skeleton must return NotSupported, got {r:?}"
    );
}
