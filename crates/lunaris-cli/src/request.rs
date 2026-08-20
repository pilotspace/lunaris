//! Argument parsing and `MemoryRequest` construction.
//!
//! This module is deliberately pure: parsed arguments in, a
//! [`MemoryRequest`] out, no I/O. That keeps the mapping unit-testable
//! without a Moon, and — more importantly — it makes it structurally
//! impossible for the CLI to answer a question on its own. Every subcommand
//! must produce a `MemoryRequest` that
//! [`lunaris_memory_service::protocol::dispatch`] executes, which is the
//! identical function `lunaris-contextd` runs and the `lunaris-mcp` proxy
//! falls back to.
//!
//! That constraint is the whole point of this crate existing. Before GA-1
//! there were three divergent recall pipelines (MCP dropped fact legs under
//! `with_root`, the hook ran `hybrid_root` fact legs un-gated, HTTP/SDK was
//! vector-only) because each surface planned its own retrieval. A fourth
//! surface that opened storage itself would be a fourth divergence; a fourth
//! surface built on `dispatch` is an instrument for proving the other three
//! agree.

use clap::{Parser, Subcommand};
use lunaris_memory_service::protocol::MemoryRequest;

/// Default recall breadth. Matches `RecallParams`' own `default_k` so the CLI
/// cannot quietly measure a different retrieval than the other surfaces.
pub(crate) const DEFAULT_K: usize = 5;

#[derive(Parser, Debug)]
#[command(
    name = "lunaris",
    version,
    about = "Inspect and operate a running Lunaris memory store",
    long_about = "Talks to a running `lunaris-contextd` over its unix socket, \
                  falling back to opening the store directly when the daemon \
                  is unreachable. Both paths execute the same shared dispatch \
                  the MCP server and Claude Code hook use."
)]
pub(crate) struct Cli {
    /// Partition scope to operate on.
    #[arg(long, global = true, env = "LUNARIS_SCOPE")]
    pub(crate) scope: Option<String>,

    /// Emit the raw JSON response instead of a human-readable rendering.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Search the store and print the hits the production recall path returns.
    Recall {
        /// Natural-language query.
        query: String,

        /// Maximum number of hits.
        #[arg(long, short, default_value_t = DEFAULT_K)]
        k: usize,

        /// Return raw stored bytes instead of the curated snippet.
        #[arg(long)]
        raw: bool,

        /// Bi-temporal as-of time (RFC-3339). Defaults to latest.
        ///
        /// NOTE: Moon answers historical KV reads with `NotSupported` (0.6.2
        /// bi-temporal ruling) — search-path AS_OF works, hydrate does not.
        #[arg(long)]
        as_of: Option<String>,
    },

    /// Report backend capabilities and queue health for the scope.
    Status,

    /// Preview or commit deletion of episodes.
    ///
    /// Previews by default, matching the MCP surface (`dry_run` defaults to
    /// TRUE there since PR #94) rather than `lunaris-server`'s HTTP DTO, which
    /// defaults to committing. An interactive CLI is the surface where an
    /// accidental purge is most likely and least recoverable, so it follows
    /// the safer of the two existing conventions.
    Forget {
        /// Forget every episode whose source starts with this prefix.
        #[arg(long, conflicts_with = "episode_id")]
        source_prefix: Option<String>,

        /// Forget a single episode by ULID.
        #[arg(long, conflicts_with = "source_prefix")]
        episode_id: Option<String>,

        /// Actually delete. Without this the command only reports what would go.
        #[arg(long)]
        commit: bool,
    },
}

/// Why a set of arguments cannot become a request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RequestError {
    /// No scope on the command line and none in the environment.
    MissingScope,
    /// `forget` was given neither selector.
    MissingForgetTarget,
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingScope => write!(
                f,
                "no scope given: pass --scope or set LUNARIS_SCOPE. The CLI \
                 deliberately has no default scope — guessing one would read \
                 or delete the wrong partition"
            ),
            Self::MissingForgetTarget => write!(
                f,
                "forget needs a target: pass --source-prefix or --episode-id. \
                 An empty prefix would match every episode in the scope and is \
                 rejected as a footgun"
            ),
        }
    }
}

impl Cli {
    /// Build the wire request this invocation represents.
    pub(crate) fn to_request(&self) -> Result<MemoryRequest, RequestError> {
        let scope = self
            .scope
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or(RequestError::MissingScope)?
            .to_string();

        Ok(match &self.command {
            Command::Recall { query, k, raw, as_of } => MemoryRequest::Recall {
                scope,
                params: lunaris_memory_service::recall::RecallParams {
                    query: query.clone(),
                    k: *k,
                    filters: None,
                    as_of: as_of.clone(),
                    raw: *raw,
                },
            },

            Command::Status => MemoryRequest::Status { scope },

            Command::Forget { source_prefix, episode_id, commit } => {
                if source_prefix.is_none() && episode_id.is_none() {
                    return Err(RequestError::MissingForgetTarget);
                }
                MemoryRequest::Forget {
                    scope,
                    params: lunaris_memory_service::forget::ForgetParams {
                        target: lunaris_memory_service::forget::ForgetTarget {
                            source_prefix: source_prefix.clone(),
                            episode_id: episode_id.clone(),
                        },
                        dry_run: !*commit,
                    },
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse an argv the way a shell would hand it over.
    fn cli(args: &[&str]) -> Cli {
        let mut argv = vec!["lunaris"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).expect("args should parse")
    }

    #[test]
    fn recall_maps_to_a_recall_request_at_the_shared_default_breadth() {
        let req = cli(&["--scope", "acme", "recall", "why did we pick moon"])
            .to_request()
            .expect("should build");

        match req {
            MemoryRequest::Recall { scope, params } => {
                assert_eq!(scope, "acme");
                assert_eq!(params.query, "why did we pick moon");
                assert_eq!(
                    params.k, DEFAULT_K,
                    "the CLI must default to the same k as RecallParams' own \
                     default; a surface that quietly measures a different \
                     breadth is not comparable with the others, which is the \
                     whole reason this crate exists"
                );
                assert!(!params.raw, "curated snippets by default, like every other surface");
                assert!(params.as_of.is_none());
            }
            other => panic!("expected Recall, got {other:?}"),
        }
    }

    /// Pins the CLI default against `RecallParams`' own default. If either side
    /// moves, this fails — which is the point: they must move together.
    #[test]
    fn cli_default_k_equals_the_recall_params_default() {
        let from_params: lunaris_memory_service::recall::RecallParams =
            serde_json::from_value(serde_json::json!({ "query": "x" }))
                .expect("RecallParams should deserialize with only `query`");
        assert_eq!(
            DEFAULT_K, from_params.k,
            "lunaris-cli's DEFAULT_K drifted from RecallParams' serde default"
        );
    }

    #[test]
    fn status_maps_to_a_status_request() {
        match cli(&["--scope", "acme", "status"]).to_request().expect("should build") {
            MemoryRequest::Status { scope } => assert_eq!(scope, "acme"),
            other => panic!("expected Status, got {other:?}"),
        }
    }

    /// The MCP surface previews by default (PR #94, `dry_run` defaults TRUE);
    /// `lunaris-server`'s HTTP DTO commits by default. A CLI is where an
    /// accidental purge is most likely and least recoverable, so it follows the
    /// safer of the two conventions — and must not acquire the other by drift.
    #[test]
    fn forget_previews_unless_commit_is_explicit() {
        let preview = cli(&["--scope", "acme", "forget", "--source-prefix", "lunaris:tool_call"])
            .to_request()
            .expect("should build");
        match preview {
            MemoryRequest::Forget { params, .. } => assert!(
                params.dry_run,
                "forget MUST default to preview; a CLI that deletes on the happy \
                 path turns a typo into data loss"
            ),
            other => panic!("expected Forget, got {other:?}"),
        }

        let committed =
            cli(&["--scope", "acme", "forget", "--source-prefix", "lunaris:tool_call", "--commit"])
                .to_request()
                .expect("should build");
        match committed {
            MemoryRequest::Forget { params, .. } => assert!(!params.dry_run),
            other => panic!("expected Forget, got {other:?}"),
        }
    }

    #[test]
    fn forget_without_a_selector_is_refused() {
        let err = cli(&["--scope", "acme", "forget"]).to_request().unwrap_err();
        assert_eq!(
            err,
            RequestError::MissingForgetTarget,
            "an empty target would match every episode in the scope"
        );
    }

    /// No default scope, ever. The store is partitioned by scope, and a guessed
    /// partition means reading — or deleting — someone else's memories.
    #[test]
    fn a_missing_scope_is_an_error_not_a_guess() {
        let err = Cli::try_parse_from(["lunaris", "status"])
            .expect("args parse; scope is optional at the clap layer")
            .to_request()
            .unwrap_err();
        assert_eq!(err, RequestError::MissingScope);
    }

    #[test]
    fn a_blank_scope_does_not_pass_as_a_scope() {
        let err = cli(&["--scope", "   ", "status"]).to_request().unwrap_err();
        assert_eq!(
            err,
            RequestError::MissingScope,
            "whitespace must not slip through as a scope — Scope::new would \
             reject it downstream, but failing here names the actual problem"
        );
    }
}
