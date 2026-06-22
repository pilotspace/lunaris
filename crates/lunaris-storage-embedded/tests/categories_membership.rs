//! Discriminating REAL-backend test for the `multi-level-memory-categories`
//! frozen-flag mitigation.
//!
//! Categories are stored as a JSON ARRAY on the episode (`metadata.categories
//! = ["blue", ...]`). A recall-side `Filter::Eq { field: "categories", value }`
//! MUST be honored by the embedded SQLite backend as SET-MEMBERSHIP — "is
//! `value` one of the array's elements?" — not whole-array scalar equality.
//!
//! ## RED reason (the right reason)
//!
//! Before BUILD, `filter_to_sqlite` compiles `Eq` to
//! `json_extract(metadata, '$.categories') = 'blue'`. On a JSON array,
//! `json_extract` returns the array text (`["blue"]`), which never equals a
//! scalar — so a categories filter ANNIHILATES the result set (returns zero
//! hits) instead of partitioning it. This is the built-≠-wired trap the §3
//! freeze flag named: a capture-stub test (filter reaches storage) is GREEN
//! while the real backend silently returns the wrong set.
//!
//! After BUILD changes `Eq` to a `json_each(...) EXISTS` membership test, the
//! blue row is returned and the green row excluded — and scalar `Eq` is
//! preserved (a 1-element membership), so existing filters do not regress.

#![allow(clippy::unwrap_used)]

use lunaris_core::scope::Scope;
use lunaris_core::storage::StoragePort as _;
use lunaris_core::storage::types::{Filter, WriteOp};
use lunaris_storage_embedded::EmbeddedStorage;

/// Upsert one vector row whose `categories` metadata is a JSON array.
async fn write_cat(
    storage: &EmbeddedStorage,
    scope: &Scope,
    id: &[u8],
    categories: &[&str],
    embedding: Vec<f32>,
) {
    let cats: Vec<serde_json::Value> =
        categories.iter().map(|c| serde_json::Value::String((*c).to_string())).collect();
    let meta = serde_json::json!({ "categories": cats });
    storage
        .atomic_write(
            scope,
            &[WriteOp::VectorUpsert {
                index: "chunks".to_string(),
                id: id.to_vec(),
                embedding,
                metadata: meta,
            }],
        )
        .await
        .expect("vector upsert persists");
}

/// Two rows under the SAME scope, IDENTICAL embeddings, differing ONLY by the
/// `categories` array. A `categories=["blue"]` `Eq` filter must return EXACTLY
/// the blue row — proving the filter is honored (not dropped → 2, not
/// over-filtered → 0).
#[tokio::test]
async fn categories_eq_filter_honored_as_membership() {
    let storage = EmbeddedStorage::connect("memory://").await.unwrap();
    let scope = Scope::new("ct").unwrap();
    let v = vec![1.0_f32, 0.0, 0.0];

    write_cat(&storage, &scope, b"blue-row", &["blue"], v.clone()).await;
    write_cat(&storage, &scope, b"green-row", &["green"], v.clone()).await;

    // Sanity: with NO filter, both identical-embedding rows match the probe.
    let all = storage.vector_search(&scope, "chunks", &v, 10, None, None, false).await.unwrap();
    assert_eq!(all.len(), 2, "both rows must be present without a filter");

    // The discriminating assertion: Eq on the categories ARRAY = membership.
    let blue = Filter::Eq { field: "categories".into(), value: serde_json::json!("blue") };
    let hits =
        storage.vector_search(&scope, "chunks", &v, 10, Some(&blue), None, false).await.unwrap();
    assert_eq!(
        hits.len(),
        1,
        "categories=['blue'] must return EXACTLY the blue row — filter must be HONORED \
         (dropped → 2, over-filtered → 0)"
    );
    let cats = hits[0].metadata.get("categories").expect("hit metadata carries categories");
    assert_eq!(cats, &serde_json::json!(["blue"]), "the surviving row must be the blue one");
}

/// A member of a MULTI-category array must match — membership, not whole-array
/// equality.
#[tokio::test]
async fn categories_membership_matches_one_of_many() {
    let storage = EmbeddedStorage::connect("memory://").await.unwrap();
    let scope = Scope::new("ct2").unwrap();
    let v = vec![1.0_f32, 0.0, 0.0];

    write_cat(&storage, &scope, b"multi", &["work", "urgent"], v.clone()).await;

    let urgent = Filter::Eq { field: "categories".into(), value: serde_json::json!("urgent") };
    let hits =
        storage.vector_search(&scope, "chunks", &v, 10, Some(&urgent), None, false).await.unwrap();
    assert_eq!(hits.len(), 1, "a member of a multi-category array must match");
}

/// Scalar (non-array) `Eq` must STILL work after the membership change — a
/// regression guard so the fix does not break existing scalar metadata filters.
#[tokio::test]
async fn scalar_eq_still_matches_after_membership_fix() {
    let storage = EmbeddedStorage::connect("memory://").await.unwrap();
    let scope = Scope::new("ct3").unwrap();
    let v = vec![1.0_f32, 0.0, 0.0];

    // `source` is a scalar string field (not an array).
    let meta = serde_json::json!({ "source": "chat" });
    storage
        .atomic_write(
            &scope,
            &[WriteOp::VectorUpsert {
                index: "chunks".to_string(),
                id: b"scalar".to_vec(),
                embedding: v.clone(),
                metadata: meta,
            }],
        )
        .await
        .unwrap();

    let eq = Filter::Eq { field: "source".into(), value: serde_json::json!("chat") };
    let hit =
        storage.vector_search(&scope, "chunks", &v, 10, Some(&eq), None, false).await.unwrap();
    assert_eq!(hit.len(), 1, "scalar Eq must still match exactly");

    let miss = Filter::Eq { field: "source".into(), value: serde_json::json!("nope") };
    let none =
        storage.vector_search(&scope, "chunks", &v, 10, Some(&miss), None, false).await.unwrap();
    assert_eq!(none.len(), 0, "scalar Eq non-match must return nothing");
}
