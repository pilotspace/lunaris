//! Anthropic `/v1/messages` request/response shim.
//!
//! Uses tool-use with `input_schema` to constrain JSON output when the
//! caller supplies a `JsonSchema` constraint. With `None` or `Gbnf`
//! constraints, falls back to a plain message and lets the caller
//! post-process.

use lunaris_core::{LunarisError, StorageError};

use crate::{GenOpts, SchemaConstraint};

const URL: &str = "https://api.anthropic.com/v1/messages";

pub(super) fn build_request(
    model: &str,
    api_key: &str,
    prompt: &str,
    constraint: SchemaConstraint<'_>,
    opts: &GenOpts,
) -> (String, serde_json::Value, Vec<(&'static str, String)>) {
    // max_tokens is required for /v1/messages. The caller's GenOpts
    // value already accounts for the worst-case output the upstream
    // schema can produce.
    let max_tokens = opts.max_tokens;

    let body = match constraint {
        SchemaConstraint::JsonSchema(schema) => serde_json::json!({
            "model": model,
            "max_tokens": max_tokens,
            "temperature": opts.temperature,
            "tools": [{
                "name": "emit_structured",
                "description": "Emit the requested structured payload.",
                "input_schema": schema,
            }],
            "tool_choice": {"type": "tool", "name": "emit_structured"},
            "messages": [{"role": "user", "content": prompt}]
        }),
        SchemaConstraint::None | SchemaConstraint::Gbnf(_) => serde_json::json!({
            "model": model,
            "max_tokens": max_tokens,
            "temperature": opts.temperature,
            "messages": [{"role": "user", "content": prompt}]
        }),
    };
    let headers =
        vec![("x-api-key", api_key.to_string()), ("anthropic-version", "2023-06-01".to_string())];
    (URL.to_string(), body, headers)
}

pub(super) fn decode_response(body: &str) -> Result<String, LunarisError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("cloud-api (anthropic) parse: {e}")))
    })?;
    let content = v["content"].as_array().ok_or_else(|| {
        LunarisError::Storage(StorageError::Backend(format!(
            "cloud-api (anthropic) missing content: {body}"
        )))
    })?;

    // Prefer tool_use input (already a JSON object); fall back to text.
    if let Some(tool_input) =
        content.iter().find(|c| c["type"] == "tool_use").and_then(|c| c.get("input"))
    {
        return serde_json::to_string(tool_input).map_err(|e| {
            LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api (anthropic) tool_use serialize: {e}"
            )))
        });
    }
    if let Some(text) =
        content.iter().find(|c| c["type"] == "text").and_then(|c| c["text"].as_str())
    {
        return Ok(text.to_string());
    }
    Err(LunarisError::Storage(StorageError::Backend(format!(
        "cloud-api (anthropic) returned wrong shape: {body}"
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_tool_use_input() {
        let body = r#"{
            "content": [
                {"type": "tool_use", "input": {"winner_id":"01HX","loser_id":"01HY"}}
            ]
        }"#;
        let out = decode_response(body).unwrap();
        assert!(out.contains("\"winner_id\":\"01HX\""), "got: {out}");
    }

    #[test]
    fn decodes_plain_text() {
        let body = r#"{
            "content": [
                {"type": "text", "text": "hello world"}
            ]
        }"#;
        let out = decode_response(body).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn missing_content_is_typed_error() {
        let err = decode_response("{}").unwrap_err();
        assert!(err.to_string().contains("missing content"));
    }

    #[test]
    fn build_request_with_schema_emits_tool_use() {
        let opts = GenOpts::default();
        let schema = serde_json::json!({"type":"object","properties":{"x":{"type":"string"}}});
        let (url, body, headers) = build_request(
            "claude-3-5-haiku-latest",
            "dummy",
            "hi",
            SchemaConstraint::JsonSchema(&schema),
            &opts,
        );
        assert_eq!(url, URL);
        assert_eq!(body["model"], "claude-3-5-haiku-latest");
        assert!(body["tools"].is_array());
        assert_eq!(body["tool_choice"]["name"], "emit_structured");
        assert!(headers.iter().any(|(k, _)| *k == "x-api-key"));
        assert!(headers.iter().any(|(k, _)| *k == "anthropic-version"));
    }
}
