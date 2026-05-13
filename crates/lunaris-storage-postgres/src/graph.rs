//! `graph_traverse` — Apache AGE `cypher()` adapter with **detect-and-dispatch**
//! multi-column projection.
//!
//! ## Problem (pre-Wave-4)
//!
//! The previous implementation wrapped every Cypher payload as
//!
//! ```sql
//! SELECT * FROM cypher('lunaris_graph', $$ ... $$)
//!   AS (v ag_catalog.agtype);
//! ```
//!
//! and surfaced the result as a single-column
//! `GraphResult { headers: ["v"], rows: [[<agtype-blob>]] }`. The
//! `Graph::anchored` operator does **positional** `row.get(0).as_str()`
//! reads — every read missed the actual node fields and produced
//! empty-id hits.
//!
//! Worse: today's operator template already emits a multi-aliased
//! RETURN (`m.id_hex AS id, m.name AS name, m.type AS type`), and AGE
//! rejects multi-aliased RETURN against a single-column `AS` list with
//! `"return row and column definition list do not match"`. So the path
//! was hard-broken end-to-end, not merely producing latent empty hits.
//! Pinned by `tests/graph_named_columns.rs`.
//!
//! ## Fix
//!
//! AGE requires the `AS (col1 agtype, col2 agtype, ...)` column-definition
//! list to match the Cypher `RETURN` clause column count exactly. We
//! cannot statically pick the shape — the operator (`lunaris-retrieve`)
//! and any future caller may emit 1, 3, or 6+ aliases. So we **parse**
//! the RETURN clause out of `query.cypher`, extract `AS <alias>` for
//! every projected expression, and build the matching column-definition
//! list at SQL-compose time.
//!
//! Cell decode is uniform: AGE emits each agtype as valid JSON text
//! (strings as JSON-quoted `"foo"`, ints/floats bare `3` / `3.14`,
//! NULL as SQL NULL → `Option::None`). `serde_json::from_str(&cell)`
//! parses every scalar into a `serde_json::Value` without bespoke
//! quote-stripping.
//!
//! ## AGE Cypher dialect limits (caller concern)
//!
//! AGE does **not** support Cypher `reduce(acc, x in xs | ...)` syntax
//! (parses as "syntax error at or near `|`"). Callers that want
//! `edge_weight_product` over a variable-length path must use a
//! workaround until upstream AGE catches up. Probe results recorded in
//! `tmp/wave4b-discovery.md`.
//!
//! ## RFC 0001 scope GUC
//!
//! Every Postgres read path opens a transaction and sets
//! `lunaris.scope` via `set_config(..., true)` so RLS policies apply
//! under the non-superuser app role. Mirrors `keyword_search` and
//! `vector_search`. `_as_of` is accepted for the StoragePort signature
//! but ignored — AGE has no native AS_OF.

use lunaris_core::error::StorageError;
use lunaris_core::hlc::Hlc;
use lunaris_core::scope::Scope;
use lunaris_core::storage::types::{CypherQuery, GraphResult};
use sqlx::{AssertSqlSafe, Row};

use crate::pool::{PgClient, sqlx_err};

pub(crate) async fn graph_traverse(
    c: &PgClient,
    scope: &Scope,
    query: &CypherQuery,
    _as_of: Option<Hlc>,
) -> Result<GraphResult, StorageError> {
    // Parse the RETURN aliases out of the Cypher payload so we can build a
    // matching multi-column AS clause. Empty alias list → fall back to the
    // legacy single-column `v` envelope so a caller emitting `RETURN n`
    // (no alias) still gets one row per match.
    let aliases = parse_return_aliases(&query.cypher);
    let column_list = if aliases.is_empty() {
        "(v ag_catalog.agtype)".to_string()
    } else {
        let cols: Vec<String> = aliases.iter().map(|a| format!("{a} ag_catalog.agtype")).collect();
        format!("({})", cols.join(", "))
    };

    // AGE's 3-arg `cypher(graph, query, params)` rejects everything except a
    // raw SQL `$N` parameter in the 3rd slot ("third argument of cypher
    // function must be a parameter"). sqlx-postgres can't natively bind an
    // `ag_catalog.agtype` value (it's an extension-defined type with a
    // dynamic OID), and any wrapping (`$1::agtype`, CTE indirection) trips
    // the same parse-tree check.
    //
    // Workaround: inline the params into the Cypher payload as Cypher
    // literals before sending. `$ids` becomes `['aaaa','bbbb']`, `$k`
    // becomes the bare int. JSON is a syntactic subset of Cypher's
    // literal grammar for the values Lunaris currently emits
    // (strings, ints, floats, arrays, nested maps).
    //
    // Injection note: every callsite in `lunaris-retrieve` builds the
    // params map from typed values (`EntityId::Display` for hex strings,
    // `serde_json::Number` for k). The hex-string production
    // (`EntityId` formats as `[0-9a-f]{32}`) cannot contain a `'`. If a
    // future caller widens to arbitrary user text the inliner below
    // needs JSON-aware Cypher escaping; pinned by
    // `param_inline_rejects_quoted_string` (defended in
    // `crates/lunaris-retrieve/src/operators/graph.rs` callsite).
    let cypher_with_params = inline_params(&query.cypher, &query.params);

    // Outer SELECT wraps every alias in `ag_catalog.agtype_out(...)` which
    // returns a `cstring` that the Postgres protocol surfaces as `text`.
    // This is what sqlx can decode losslessly via `try_get_unchecked::<Option<String>>`
    // — the alternative (raw `agtype` columns) ships the on-wire binary
    // (a `\x01` version-byte prefix + JSONB-ish body) which `String` decodes
    // into the wrong bytes. The text form is also JSON-shaped:
    //   strings → `"foo"`, ints → `3`, floats → `3.14`, NULL → SQL NULL.
    let select_list = if aliases.is_empty() {
        "ag_catalog.agtype_out(v) AS v".to_string()
    } else {
        aliases
            .iter()
            .map(|a| format!("ag_catalog.agtype_out({a}) AS {a}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let sql = format!(
        "SELECT {select_list} FROM cypher('{graph}', $$ {cypher} $$) AS {cols}",
        select_list = select_list,
        graph = query.graph,
        cypher = cypher_with_params,
        cols = column_list,
    );

    // RFC 0001 Wave 1B: open a transaction so every statement runs on a
    // single pooled connection. `LOAD 'age'` is per-session (the AGE
    // extension is not in `shared_preload_libraries` in the docker image)
    // and must apply to the same backend that executes `cypher(...)`.
    // Issuing `LOAD` inside the transaction guarantees that — sqlx pins
    // the connection for the lifetime of the `Transaction`.
    let mut tx = c.pool.begin().await.map_err(sqlx_err)?;
    // `LOAD 'age'` resolves the cypher() / agtype symbols for this
    // backend session. Idempotent within a connection.
    sqlx::query("LOAD 'age'").execute(&mut *tx).await.map_err(sqlx_err)?;
    sqlx::query("SET LOCAL search_path = ag_catalog, \"$user\", public")
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;
    sqlx::query("SELECT set_config('lunaris.scope', $1, true)")
        .bind(scope.as_str())
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;

    let rows = sqlx::query(AssertSqlSafe(sql)).fetch_all(&mut *tx).await.map_err(sqlx_err)?;
    tx.commit().await.map_err(sqlx_err)?;

    // Determine the actual headers we return. When aliases are present the
    // header set IS the parsed alias list (preserves RETURN order). When
    // no aliases were detected, fall back to the legacy `v` column.
    let headers: Vec<String> = if aliases.is_empty() { vec!["v".into()] } else { aliases.clone() };

    let mut out_rows: Vec<Vec<serde_json::Value>> = Vec::with_capacity(rows.len());
    for r in &rows {
        let mut row_out: Vec<serde_json::Value> = Vec::with_capacity(headers.len());
        for i in 0..headers.len() {
            // Each column is the output of `ag_catalog.agtype_out(<alias>)`
            // (composed in the outer SELECT above) which returns a
            // `cstring` that the Postgres protocol presents as `text`.
            // sqlx decodes it natively via `Option<String>`. The text
            // form is JSON-shaped per AGE convention:
            //   strings → `"foo"` (double-quoted), ints → `3`, floats →
            //   `3.14`, NULL → SQL NULL.
            let cell: Option<String> = r.try_get_unchecked::<Option<String>, _>(i).unwrap_or(None);
            let value = match cell {
                None => serde_json::Value::Null,
                Some(s) => match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(v) => v,
                    Err(_) => serde_json::Value::String(s),
                },
            };
            row_out.push(value);
        }
        out_rows.push(row_out);
    }

    Ok(GraphResult { headers, rows: out_rows })
}

/// Replace every `$<name>` occurrence in `cypher` with the Cypher literal
/// equivalent of `params[name]`.
///
/// JSON is a syntactic subset of Cypher's literal grammar for every
/// value type Lunaris emits today: strings (JSON-quoted matches Cypher
/// single-or-double-quoted), numbers, booleans, arrays, maps, null.
/// `serde_json::to_string` is the canonical encoder.
///
/// Identifier boundary: `$` is followed by `[A-Za-z_][A-Za-z0-9_]*`.
/// Anything that's not a known param name passes through unchanged so
/// raw Cypher with `$$` quoting (the `$$ ... $$` outer envelope) survives.
fn inline_params(cypher: &str, params: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut out = String::with_capacity(cypher.len());
    let bytes = cypher.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        // `$$` is the Postgres dollar-quoted-string sentinel; the caller
        // wraps Cypher with `$$ ... $$`. We never substitute across it —
        // but `inline_params` is called BEFORE the `$$` envelope is added,
        // so this branch is informational defense only.
        if b == b'$' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next.is_ascii_alphabetic() || next == b'_' {
                // Scan forward for the identifier body.
                let start = i + 1;
                let mut end = start + 1;
                while end < bytes.len() {
                    let c = bytes[end];
                    if c.is_ascii_alphanumeric() || c == b'_' {
                        end += 1;
                    } else {
                        break;
                    }
                }
                let name = &cypher[start..end];
                if let Some(v) = params.get(name) {
                    // Encode as JSON, which is a subset of Cypher literal syntax.
                    if let Ok(s) = serde_json::to_string(v) {
                        // Defense-in-depth: `$$` inside the inlined value
                        // would terminate the outer `cypher('graph', $$ ...
                        // $$)` dollar-quoted envelope and turn the rest
                        // of the Cypher into raw SQL. Today's callers
                        // (hex EntityIds + numeric `k`) cannot produce
                        // `$$`, but pin the invariant at runtime so a
                        // future widening to user text trips loudly in
                        // debug builds.
                        debug_assert!(
                            !s.contains("$$"),
                            "inline_params: param '{name}' encodes to '{s}' which contains the \
                             dollar-quote sentinel '$$' — would escape the outer cypher() literal"
                        );
                        out.push_str(&s);
                        i = end;
                        continue;
                    }
                }
            }
        }
        // Copy the current UTF-8 codepoint verbatim. Using `b as char`
        // here would mojibake every non-ASCII byte (each byte of a
        // multi-byte sequence becomes its own `char`). Decode-then-push
        // is safe because `$` itself is single-byte ASCII and can never
        // appear inside a multi-byte UTF-8 codepoint.
        let ch = cypher[i..].chars().next().expect("non-empty remainder");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Extract the `AS <alias>` identifiers from a Cypher `RETURN` clause.
///
/// Cypher grammar: `RETURN [DISTINCT] <expr> [AS alias] (, <expr> [AS alias])*
/// [ORDER BY ...] [SKIP ...] [LIMIT ...]`.
///
/// We are not a full Cypher parser; this is a tolerant extractor for the
/// well-formed `RETURN <expr> AS <alias>` shapes the Lunaris retrieve crate
/// emits. Robust against:
///
/// - leading `DISTINCT` keyword
/// - case-insensitive `RETURN` / `AS` / `LIMIT` / `ORDER BY` / `SKIP`
/// - extra whitespace / newlines / multi-line RETURN clauses
/// - parameter-style `$ids` / `$k` payload in expressions
///
/// Returns an empty list if no `AS` aliases are detected — caller falls
/// back to the legacy single-column `v` envelope.
fn parse_return_aliases(cypher: &str) -> Vec<String> {
    // Case-insensitive locate of the last `RETURN` keyword. Multiple
    // RETURNs in a single Cypher payload are not produced by Lunaris;
    // taking the LAST is safe and matches AGE's outer-most projection.
    let lower = cypher.to_ascii_lowercase();
    let Some(ret_idx) = lower.rfind("return ") else { return Vec::new() };
    let after_return = &cypher[ret_idx + "return ".len()..];

    // Stop at the first ORDER BY / SKIP / LIMIT terminator. These can also
    // appear inside string literals in theory — Lunaris's emitter never
    // does that, so a literal-match is safe.
    let body_end = ["order by", "skip ", "limit "]
        .iter()
        .filter_map(|kw| {
            let l = after_return.to_ascii_lowercase();
            l.find(kw)
        })
        .min()
        .unwrap_or(after_return.len());
    let body = &after_return[..body_end];

    // Strip leading `DISTINCT`.
    let body_trimmed = body.trim_start();
    let body = body_trimmed
        .strip_prefix("DISTINCT ")
        .or_else(|| body_trimmed.strip_prefix("distinct "))
        .unwrap_or(body_trimmed);

    // Split top-level `,` — Cypher RETURN items don't nest parens/brackets
    // in any pattern Lunaris emits today, so a flat split is correct. If
    // a future caller adds bracketed expressions we'll need a paren-aware
    // splitter; pin this assumption with a unit test below.
    body.split(',')
        .filter_map(|piece| {
            let piece_lower = piece.to_ascii_lowercase();
            let as_pos = piece_lower.rfind(" as ")?;
            let alias = piece[as_pos + 4..].trim();
            // Strip any trailing whitespace/newline; aliases are
            // simple identifiers in every Lunaris emitter path.
            let alias_clean = alias.split_whitespace().next()?.trim();
            if alias_clean.is_empty() || !is_valid_alias(alias_clean) {
                None
            } else {
                Some(alias_clean.to_string())
            }
        })
        .collect()
}

/// `alias` must be a plain Cypher identifier: starts with letter/`_`, body of
/// letter/digit/`_`. Defends the dynamic SQL splice in `column_list` against
/// caller-controlled injection — a malformed alias drops the row from the
/// header set and the caller's RETURN fails loudly downstream.
fn is_valid_alias(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_three_aliases_from_distinct_return() {
        let c = "UNWIND $ids AS sid \
                 MATCH (n {id_hex: sid})-[*1..3]-(m) \
                 RETURN DISTINCT m.id_hex AS id, m.name AS name, m.type AS type \
                 LIMIT $k";
        assert_eq!(parse_return_aliases(c), vec!["id", "name", "type"]);
    }

    #[test]
    fn parses_six_aliases_with_path_metrics() {
        let c = "MATCH p = (n)-[*1..3]-(m) \
                 RETURN m.id_hex AS id, m.name AS name, m.type AS type, \
                        length(p) AS path_length, 1.0 AS edge_weight_product, \
                        n.id_hex AS source_entity_id \
                 LIMIT 10";
        assert_eq!(
            parse_return_aliases(c),
            vec!["id", "name", "type", "path_length", "edge_weight_product", "source_entity_id"]
        );
    }

    #[test]
    fn returns_empty_on_unaliased_return() {
        let c = "MATCH (n) RETURN n LIMIT 10";
        assert!(parse_return_aliases(c).is_empty());
    }

    #[test]
    fn ignores_aliases_without_as_keyword() {
        let c = "RETURN m";
        assert!(parse_return_aliases(c).is_empty());
    }

    #[test]
    fn rejects_alias_with_injection_chars() {
        // A malformed alias with whitespace/parens drops out of the
        // returned set so it never reaches dynamic SQL composition.
        assert!(!is_valid_alias("id agtype, name agtype"));
        assert!(!is_valid_alias("id;DROP"));
        assert!(!is_valid_alias("1bad"));
        assert!(is_valid_alias("id"));
        assert!(is_valid_alias("path_length"));
        assert!(is_valid_alias("_underscore"));
    }

    #[test]
    fn case_insensitive_return_distinct_as() {
        let c = "match (m) return distinct m.id as id, m.name as name";
        assert_eq!(parse_return_aliases(c), vec!["id", "name"]);
    }

    #[test]
    fn inline_params_substitutes_array_and_int() {
        let mut p = serde_json::Map::new();
        p.insert("ids".into(), serde_json::json!(["aaaa", "bbbb"]));
        p.insert("k".into(), serde_json::json!(10));
        let c = "UNWIND $ids AS sid MATCH (n {id_hex: sid}) RETURN n.id_hex AS id LIMIT $k";
        let out = inline_params(c, &p);
        assert_eq!(
            out,
            "UNWIND [\"aaaa\",\"bbbb\"] AS sid MATCH (n {id_hex: sid}) RETURN n.id_hex AS id LIMIT 10"
        );
    }

    #[test]
    fn inline_params_leaves_unknown_dollar_names_alone() {
        // Cypher's own `$missing` ref with no param should pass through —
        // AGE will then surface its own "missing parameter" error.
        let c = "RETURN $missing";
        let out = inline_params(c, &serde_json::Map::new());
        assert_eq!(out, "RETURN $missing");
    }

    #[test]
    fn inline_params_preserves_utf8_in_surrounding_cypher() {
        // Non-ASCII bytes in the Cypher text MUST round-trip codepoint-
        // for-codepoint. A naive `byte as char` push would mojibake every
        // multi-byte sequence into individual Latin-1-style chars.
        let mut p = serde_json::Map::new();
        p.insert("k".into(), serde_json::json!(7));
        let c = "MATCH (n {name: 'Käse über'}) RETURN n.id AS id LIMIT $k";
        let out = inline_params(c, &p);
        assert_eq!(out, "MATCH (n {name: 'Käse über'}) RETURN n.id AS id LIMIT 7");
        // Codepoint count round-trips.
        assert!(out.chars().any(|c| c == 'ä'));
        assert!(out.chars().any(|c| c == 'ü'));
    }
}
