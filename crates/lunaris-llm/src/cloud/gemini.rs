//! Gemini `generateContent` request/response shim.
//!
//! Auth via query-param `?key=<api_key>` (no Authorization header).

use lunaris_core::{LunarisError, StorageError};

use crate::{GenOpts, SchemaConstraint};

pub(super) fn build_request(
    model: &str,
    api_key: &str,
    prompt: &str,
    constraint: SchemaConstraint<'_>,
    opts: &GenOpts,
) -> (String, serde_json::Value, Vec<(&'static str, String)>) {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key={api_key}"
    );
    let generation_config = match constraint {
        SchemaConstraint::JsonSchema(schema) => serde_json::json!({
            "responseMimeType": "application/json",
            "responseSchema": schema,
            "maxOutputTokens": opts.max_tokens,
            "temperature": opts.temperature,
        }),
        SchemaConstraint::None | SchemaConstraint::Gbnf(_) => serde_json::json!({
            "maxOutputTokens": opts.max_tokens,
            "temperature": opts.temperature,
        }),
    };
    let body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": prompt}]}],
        "generationConfig": generation_config,
    });
    (url, body, Vec::new())
}

pub(super) fn decode_response(body: &str) -> Result<String, LunarisError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("cloud-api (gemini) parse: {e}")))
    })?;
    let text = v["candidates"]
        .as_array()
        .and_then(|cs| cs.first())
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|ps| ps.as_array())
        .and_then(|ps| ps.first())
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| {
            LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api (gemini) returned wrong shape: {body}"
            )))
        })?;
    Ok(text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_first_candidate_text() {
        let body = r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "hello"}]
                }
            }]
        }"#;
        assert_eq!(decode_response(body).unwrap(), "hello");
    }

    #[test]
    fn missing_candidates_is_typed_error() {
        let err = decode_response("{}").unwrap_err();
        assert!(err.to_string().contains("returned wrong shape"));
    }

    #[test]
    fn build_request_with_schema_adds_response_schema() {
        let opts = GenOpts::default();
        let schema = serde_json::json!({"type":"object"});
        let (url, body, headers) = build_request(
            "gemini-2.5-flash",
            "dummy-key",
            "hi",
            SchemaConstraint::JsonSchema(&schema),
            &opts,
        );
        assert!(url.contains("gemini-2.5-flash"));
        assert!(url.contains("key=dummy-key"));
        assert_eq!(body["generationConfig"]["responseMimeType"], "application/json");
        assert!(body["generationConfig"]["responseSchema"].is_object());
        assert!(headers.is_empty(), "gemini uses query-param auth");
    }
}
