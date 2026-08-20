//! Human-readable rendering of dispatch responses.
//!
//! Rendering is intentionally lossy and intentionally NOT a contract: scripts
//! should use `--json`, which prints the dispatch payload verbatim under a
//! `data` key. Keeping the pretty form free of promises means it can improve
//! without breaking anyone, and it removes the temptation to let the CLI
//! reshape data on its way out — reshaping is how a surface starts to diverge.

use serde_json::Value;

use crate::route::Route;

pub(crate) fn render(value: &Value, via: Route) -> String {
    let mut out = String::new();

    if let Some(hits) = value.get("hits").and_then(Value::as_array) {
        if hits.is_empty() {
            out.push_str("no hits\n");
        }
        for (i, hit) in hits.iter().enumerate() {
            let score = hit.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let source = hit.get("source").and_then(Value::as_str).unwrap_or("?");
            let id = hit.get("episode_id").and_then(Value::as_str).unwrap_or("?");
            // `content` is the field name on the recall DTO. Getting this wrong
            // once already produced an empty-looking scratchpad read.
            let text = hit.get("content").and_then(Value::as_str).unwrap_or("").replace('\n', " ");
            out.push_str(&format!("{:>2}. [{score:.3}] {source}  {id}\n    {text}\n", i + 1));
        }
    } else if value.get("status").is_some() && value.get("matched").is_some() {
        let status = value.get("status").and_then(Value::as_str).unwrap_or("?");
        let matched = value.get("matched").and_then(Value::as_u64).unwrap_or(0);
        let removed = value.get("removed").and_then(Value::as_u64).unwrap_or(0);
        out.push_str(&format!("{status}: matched={matched} removed={removed}\n"));
        if status == "preview" {
            out.push_str("nothing was deleted — re-run with --commit to apply\n");
        }
    } else {
        // Status and anything else: pretty JSON beats a bespoke formatter that
        // silently drops fields as the response shape grows.
        out.push_str(&serde_json::to_string_pretty(value).unwrap_or_default());
        out.push('\n');
    }

    out.push_str(&format!("\n(via {})\n", via.as_str()));
    out
}
