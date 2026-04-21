//! Plan 05-03 — `POST /v1/forget` contract (D-21 two-step hard-delete rail).
//!
//! Two test functions:
//! - [`id_target`] — soft `dry_run: true` against a `ForgetTarget::Id` returns
//!   `200` with `preview: true` receipt.
//! - [`two_step_hard_delete`] — proves the D-21 contract:
//!     - `hard: true` WITHOUT prior `dry_run` token → `428 Precondition Required`,
//!     - `dry_run: true` returns the receipt to round-trip,
//!     - `hard: true` with the prior receipt as `confirmation_token` → `200`.

#![forbid(unsafe_code)]

use reqwest::{Client, StatusCode};
use serde_json::json;
use url::Url;

/// `dry_run: true` against a `ForgetTarget::Id` — preview-only receipt.
///
/// Asserts:
/// - status `200 OK`,
/// - `preview` field is `true`.
pub async fn id_target(client: &Client, base: &Url, token: &str) -> anyhow::Result<()> {
    let url = base.join("/v1/forget")?;
    let body = json!({
        "target": { "Id": ulid::Ulid::new().to_string() },
        "dry_run": true,
    });
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?;
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
    Ok(())
}

/// D-21 two-step hard-delete contract.
///
/// Spec sequence:
///   1. `hard: true` without `confirmation_token` → `428 Precondition Required`
///      (the safety rail per `crates/lunaris/src/forget.rs` Lunaris::forget impl).
///   2. `dry_run: true` returns a `ForgetReceipt { preview: true, ... }`.
///   3. `hard: true` with the prior receipt JSON in `confirmation_token` → `200 OK`
///      and `ForgetReceipt { preview: false }`.
///
/// The `confirmation_token` wire shape is the SERIALIZED prior `ForgetReceipt`
/// JSON (per Plan 05-01 routes/forget.rs rustdoc — `ForgetConfirmation` has a
/// `pub(crate)` inner field so external callers cannot mint the typed token
/// directly; the server reconstructs it from the receipt round-trip).
pub async fn two_step_hard_delete(
    client: &Client,
    base: &Url,
    token: &str,
) -> anyhow::Result<()> {
    let url = base.join("/v1/forget")?;
    let target_id = ulid::Ulid::new().to_string();

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
    Ok(())
}
