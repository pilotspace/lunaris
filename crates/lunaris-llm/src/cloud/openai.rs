//! OpenAI `/v1/chat/completions` request/response shim.

use lunaris_core::{LunarisError, StorageError};

use crate::{GenOpts, SchemaConstraint};

const URL: &str = "https://api.openai.com/v1/chat/completions";

pub(super) fn build_request(
    model: &str,
    api_key: &str,
    prompt: &str,
    constraint: SchemaConstraint<'_>,
    opts: &GenOpts,
) -> (String, serde_json::Value, Vec<(&'static str, String)>) {
    let response_format = match constraint {
        SchemaConstraint::JsonSchema(schema) => Some(serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "lunaris_structured",
                "strict": true,
                "schema": schema,
            }
        })),
        SchemaConstraint::None | SchemaConstraint::Gbnf(_) => None,
    };
    let mut body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": opts.max_tokens,
        "temperature": opts.temperature,
    });
    if let Some(rf) = response_format {
        body["response_format"] = rf;
    }
    let headers = vec![("authorization", format!("Bearer {api_key}"))];
    (URL.to_string(), body, headers)
}

pub(super) fn decode_response(body: &str) -> Result<String, LunarisError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        LunarisError::Storage(StorageError::Backend(format!("cloud-api (openai) parse: {e}")))
    })?;
    let content = v["choices"]
        .as_array()
        .and_then(|cs| cs.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .ok_or_else(|| {
            LunarisError::Storage(StorageError::Backend(format!(
                "cloud-api (openai) returned wrong shape: {body}"
            )))
        })?;
    Ok(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_first_choice_content() {
        let body = r#"{
            "choices": [{"message": {"content": "{\"x\":1}"}}]
        }"#;
        assert_eq!(decode_response(body).unwrap(), "{\"x\":1}");
    }

    #[test]
    fn missing_choices_is_typed_error() {
        let err = decode_response("{}").unwrap_err();
        assert!(err.to_string().contains("returned wrong shape"));
    }

    #[test]
    fn build_request_with_schema_adds_response_format() {
        let opts = GenOpts::default();
        let schema = serde_json::json!({"type":"object"});
        let (url, body, headers) = build_request(
            "gpt-4o-mini",
            "sk-dummy",
            "hi",
            SchemaConstraint::JsonSchema(&schema),
            &opts,
        );
        assert_eq!(url, URL);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert!(headers.iter().any(|(k, v)| *k == "authorization" && v == "Bearer sk-dummy"));
    }

    #[test]
    fn build_request_without_schema_omits_response_format() {
        let opts = GenOpts::default();
        let (_, body, _) = build_request("gpt-4o-mini", "sk", "hi", SchemaConstraint::None, &opts);
        assert!(body.get("response_format").is_none());
    }
}
