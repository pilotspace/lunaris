//! Plan 05-03 — `POST /v1/forget` contract (D-21 two-step hard-delete rail).
//!
//! Three test functions:
//! - [`id_target`] — soft `dry_run: true` against a **really-ingested**
//!   `ForgetTarget::Id` returns `200` with a `preview: true` receipt that
//!   matched exactly one row.
//! - [`unknown_id_returns_404`] — W1.1: an `Id` target that resolves to nothing
//!   in the caller's scope is `404`, not a `200` receipt claiming zero rows.
//! - [`two_step_hard_delete`] — proves the D-21 contract:
//!     - `hard: true` WITHOUT prior `dry_run` token → `428 Precondition Required`,
//!     - `dry_run: true` returns the receipt to round-trip,
//!     - `hard: true` with the prior receipt as `confirmation_token` → `200`.
//!
//! ## W1.1 — why these now seed their own episode
//!
//! Until 2026-08-21 every function here targeted a freshly-minted random ULID
//! that had never been ingested. The whole file therefore asserted the shape of
//! a receipt for a delete that could not possibly have happened — it passed
//! identically against the pre-W1.1 handler, which routed through
//! `Scope::dev()` and deleted nothing for anybody. Seeding a real episode first
//! is what turns these into contract tests instead of shape tests.

#![forbid(unsafe_code)]

use reqwest::{Client, StatusCode};
use serde_json::json;
use url::Url;

/// Ingest one episode under the caller's token and return its ULID.
///
/// The ULID is caller-supplied (`POST /v1/ingest` accepts `id`), so the forget
/// tests below can address the exact row they just created.
async fn seed_episode(client: &Client, base: &Url, token: &str) -> anyhow::Result<String> {
    let id = ulid::Ulid::new().to_string();
    let url = base.join("/v1/ingest")?;
    let body = json!({
        "id": id,
        "source": "conformance:protocol-forget",
        "content": "Seed episode for the forget conformance contract.",
        "t_ref": chrono::Utc::now().to_rfc3339(),
        "metadata": {},
    });
    let resp = client.post(url).bearer_auth(token).json(&body).send().await?;
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        status == StatusCode::OK,
        "forget-suite seed ingest expected 200, got {status}; body={payload}"
    );
    Ok(id)
}

/// `dry_run: true` against an episode this suite actually ingested.
///
/// Asserts:
/// - status `200 OK`,
/// - `preview` field is `true`,
/// - `matched == 1` — the row is visible under the caller's own scope. A zero
///   here means the handler is scanning a partition the caller does not own,
///   which is exactly the W1.1 defect.
pub async fn id_target(client: &Client, base: &Url, token: &str) -> anyhow::Result<()> {
    let id = seed_episode(client, base, token).await?;
    let url = base.join("/v1/forget")?;
    let body = json!({
        "target": { "Id": id },
        "dry_run": true,
    });
    let resp = client.post(url).bearer_auth(token).json(&body).send().await?;
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        status == StatusCode::OK,
        "POST /v1/forget dry_run expected 200, got {status}; body={payload}"
    );
    anyhow::ensure!(
        payload.get("preview").and_then(|v| v.as_bool()) == Some(true),
        "dry_run forget receipt should carry preview=true; body={payload}"
    );
    anyhow::ensure!(
        payload.get("matched").and_then(|v| v.as_u64()) == Some(1),
        "dry_run forget of a just-ingested episode must report matched=1 — a 0 means the \
         handler is not using the caller's scope; body={payload}"
    );
    Ok(())
}

/// W1.1 — an `Id` target with no match in the caller's scope is `404`.
///
/// Before W1.1 this returned `200 OK` with `rows_written = 0`, which a client
/// cannot distinguish from a successful delete. Cross-scope ids and
/// never-existed ids share the status on purpose: 404 must not confirm that an
/// id exists inside somebody else's partition.
pub async fn unknown_id_returns_404(
    client: &Client,
    base: &Url,
    token: &str,
) -> anyhow::Result<()> {
    let url = base.join("/v1/forget")?;
    let body = json!({
        "target": { "Id": ulid::Ulid::new().to_string() },
        "dry_run": true,
    });
    let resp = client.post(url).bearer_auth(token).json(&body).send().await?;
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await?;
    anyhow::ensure!(
        status == StatusCode::NOT_FOUND,
        "POST /v1/forget on an unknown Id expected 404, got {status}; body={payload}"
    );
    anyhow::ensure!(
        payload.get("error").and_then(|v| v.as_str()) == Some("not_found"),
        "404 forget body must carry the typed error code; body={payload}"
    );
    Ok(())
}

/// D-21 two-step hard-delete contract.
///
/// Spec sequence:
///   1. `hard: true` without `confirmation_token` → `428 Precondition Required`
///      (the safety rail per `crates/lunaris/src/forget.rs` forget_scoped impl).
///   2. `dry_run: true` returns a `ForgetReceipt { preview: true, ... }`.
///   3. `hard: true` with the prior receipt JSON in `confirmation_token` → `200 OK`
///      and `ForgetReceipt { preview: false, rows_deleted: 1 }`.
///
/// The `confirmation_token` wire shape is the SERIALIZED prior `ForgetReceipt`
/// JSON (per Plan 05-01 routes/forget.rs rustdoc — `ForgetConfirmation` has a
/// `pub(crate)` inner field so external callers cannot mint the typed token
/// directly; the server reconstructs it from the receipt round-trip).
///
/// W1.1: the target is a real ingested episode, so step 3 asserts an actual
/// deletion (`rows_deleted == 1`) rather than the shape of an empty receipt.
pub async fn two_step_hard_delete(client: &Client, base: &Url, token: &str) -> anyhow::Result<()> {
    let url = base.join("/v1/forget")?;
    let target_id = seed_episode(client, base, token).await?;

    // Step 1 — hard without confirmation_token → 428.
    let no_token_resp = client
        .post(url.clone())
        .bearer_auth(token)
        .json(&json!({
            "target": { "Id": target_id.clone() },
            "hard": true,
        }))
        .send()
        .await?;
    anyhow::ensure!(
        no_token_resp.status() == StatusCode::PRECONDITION_REQUIRED,
        "hard-without-token expected 428, got {}",
        no_token_resp.status()
    );

    // Step 2 — dry_run returns preview receipt for round-trip.
    let dry_resp = client
        .post(url.clone())
        .bearer_auth(token)
        .json(&json!({
            "target": { "Id": target_id.clone() },
            "dry_run": true,
        }))
        .send()
        .await?;
    anyhow::ensure!(
        dry_resp.status() == StatusCode::OK,
        "dry_run step expected 200, got {}",
        dry_resp.status()
    );
    let dry_receipt: serde_json::Value = dry_resp.json().await?;
    anyhow::ensure!(
        dry_receipt.get("preview").and_then(|v| v.as_bool()) == Some(true),
        "dry_run step receipt missing preview=true; receipt={dry_receipt}"
    );

    // Step 3 — hard with serialized prior receipt as confirmation_token → 200.
    // The handler deserializes this back into ForgetReceipt + calls
    // confirm_hard_forget to mint the typed ForgetConfirmation token.
    let confirmation_token = serde_json::to_string(&dry_receipt)?;
    let hard_resp = client
        .post(url)
        .bearer_auth(token)
        .json(&json!({
            "target": { "Id": target_id },
            "hard": true,
            "confirmation_token": confirmation_token,
        }))
        .send()
        .await?;
    let status = hard_resp.status();
    let payload: serde_json::Value = hard_resp.json().await?;
    anyhow::ensure!(
        status == StatusCode::OK,
        "hard-with-token expected 200, got {status}; body={payload}"
    );
    anyhow::ensure!(
        payload.get("preview").and_then(|v| v.as_bool()) == Some(false),
        "hard-delete receipt should carry preview=false; body={payload}"
    );
    anyhow::ensure!(
        payload.get("rows_deleted").and_then(|v| v.as_u64()) == Some(1),
        "hard-delete of a just-ingested episode must report rows_deleted=1 — a 0 means the \
         D-21 flow completed without removing anything; body={payload}"
    );
    Ok(())
}
