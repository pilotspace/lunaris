//! MiniMax `/v1/text/chatcompletion_v2` request/response shim.
//!
//! OpenAI-compatible `choices[0].message.content` response shape, confirmed
//! against the live `api.minimax.io` production endpoint (LongMemEval
//! graph-pipeline prototype, 2026-07 -- see `tmp/route_shim.py`'s
//! `via_minimax`, which this mirrors). No `response_format`/JSON-schema
//! constraint field is sent: MiniMax's chat-completions endpoint does not
//! document support for one, unlike OpenAI's. Callers needing a shape
//! constraint must embed it in the prompt text itself (same convention as
//! the Ollama backend).

use lunaris_core::{LunarisError, StorageError};

use crate::{GenOpts, SchemaConstraint};

const URL: &str = "https://api.minimax.io/v1/text/chatcompletion_v2";

pub(super) fn build_request(
    model: &str,
    api_key: &str,
    prompt: &str,
    constraint: SchemaConstraint<'_>,
    opts: &GenOpts,
) -> (String, serde_json::Value, Vec<(&'static str, String)>) {
    // No response_format field -- see module doc. The constraint is accepted
    // for signature parity with the other providers but unused.
    let _ = constraint;
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": opts.max_tokens,
        "temperature": opts.temperature,
    });
    let headers = vec![("authorization", format!("Bearer {api_key}"))];
    (URL.to_string(), body, headers)
}

pub(super) fn decode_response(body: &str) -> Result<String, LunarisError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("cloud-api (minimax) parse: {e}")))
    })?;
    let content = v["choices"]
        .as_array()
        .and_then(|cs| cs.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api (minimax) returned wrong shape: {body}"
            )))
        })?;
    Ok(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_first_choice_content() {
        let body = r#"{"choices": [{"message": {"content": "hello"}}]}"#;
        assert_eq!(decode_response(body).unwrap(), "hello");
    }

    #[test]
    fn missing_choices_is_typed_error() {
        let err = decode_response("{}").unwrap_err();
        assert!(err.to_string().contains("returned wrong shape"));
    }

    #[test]
    fn build_request_targets_the_minimax_endpoint() {
        let opts = GenOpts::default();
        let (url, body, headers) =
            build_request("MiniMax-M3", "sk-dummy", "hi", SchemaConstraint::None, &opts);
        assert_eq!(url, URL);
        assert_eq!(body["model"], "MiniMax-M3");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert!(body.get("response_format").is_none());
        assert!(headers.iter().any(|(k, v)| *k == "authorization" && v == "Bearer sk-dummy"));
    }
}
