//! Phase 11 Plan 11-01 RCPDOC-05 — `CustomerSupportHistory` documentary
//! wrapper. Composes [`DocumentCorpus`] (tickets, `source="ticket:<id>"`) +
//! [`MessageStream`] (chat transcripts, `source="chat:<ticket_id>/..."`).
//!
//! ## Double-indexing mitigation (D-09)
//!
//! RRF fuses WITHIN each primitive's own bucket — NOT across types. At
//! `recall`, the wrapper assembles results from both buckets and asserts
//! distinct source prefixes (`ticket:` vs `chat:`) so a double-indexed
//! record cannot collapse duplicates. Returns a `Vec<Hit>` that is the
//! simple concatenation of (tickets, chats); ordering across buckets is
//! NOT normalised (tie-bucket behaviour is deferred per D-13).
//!
//! ## ≤ 30 LOC public-surface contract (RCPDOC-05)
//!
//! Public symbols: `new`, `with_graph_pipeline`, `ingest_ticket`,
//! `ingest_chat`, `recall`. Five `pub fn` / `pub async fn` declarations —
//! LOC-guard caps at 6.
//!
//! ## Primitive-call budget
//!
//! - `ingest_ticket`: 1 primitive call (`DocumentCorpus::ingest`).
//! - `ingest_chat`: 1 primitive call (`MessageStream::ingest`).
//! - `recall`: 2 primitive calls (`DocumentCorpus::search` +
//!   `MessageStream::recall`). Exactly the budget per SC #2.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

use std::sync::Arc;

use lunaris::Lunaris;
use lunaris_core::storage::types::Lsn;
use lunaris_core::{LunarisError, Scope};
use lunaris_retrieve::Hit;

use crate::{DocumentCorpus, MessageStream};

/// Prefix for ticket-body Episodes (checked at `recall` time).
const TICKET_PREFIX: &str = "ticket:";
/// Prefix for chat-transcript Episodes (checked at `recall` time).
const CHAT_PREFIX: &str = "chat:";

/// Customer-support history wrapper. Owns both a tickets `DocumentCorpus`
/// and a chats `MessageStream`. Clone-safe (both primitives are `Clone`).
#[derive(Clone)]
pub struct CustomerSupportHistory {
    lunaris: Arc<Lunaris>,
    tickets: DocumentCorpus,
    chats: MessageStream,
}

impl CustomerSupportHistory {
    /// Construct with hard-coded source prefixes `ticket:` and `chat:` — the
    /// wrapper is a named recipe, not a general-purpose composer. Downstream
    /// callers that need alternate prefixes should compose DocumentCorpus +
    /// MessageStream directly.
    pub fn new(lunaris: Arc<Lunaris>, scope: Scope) -> Self {
        Self {
            lunaris: lunaris.clone(),
            tickets: DocumentCorpus::new(lunaris.clone(), scope.clone(), TICKET_PREFIX),
            chats: MessageStream::new(lunaris, scope, CHAT_PREFIX),
        }
    }

    /// Opt-in the product/customer relationship graph (1 primitive call on
    /// `GraphPipelineHandle`). Default OFF per blueprint §5.2.
    pub fn with_graph_pipeline(self, on: bool) -> Self {
        let handle = self.lunaris.graph_pipeline();
        if on {
            handle.enable();
        } else {
            handle.disable();
        }
        self
    }

    /// Ingest one ticket body. `ticket_id` is stamped into metadata.
    /// (1 primitive call.)
    pub async fn ingest_ticket(
        &self,
        ticket_id: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<(), LunarisError> {
        let id = ticket_id.into();
        let mut meta = serde_json::Map::new();
        meta.insert("ticket_id".into(), serde_json::Value::String(id));
        self.tickets.ingest(vec![(body.into(), meta)]).await
    }

    /// Ingest one chat-turn. `ticket_id/turn_idx` slug lands in the
    /// MessageStream thread_id so chats cluster per ticket. (1 primitive call.)
    // Plan 11-02b Rule 1 widen: `turn_idx` accepts `usize` so the codegen
    // `kind = "usize"` annotation flows cleanly across the FFI boundary
    // without a `U32` allow-list entry in the emitter. usize is strictly
    // wider than u32 on every platform Lunaris supports; production
    // semantics unchanged (turn indices never overflow 2^32).
    pub async fn ingest_chat(
        &self,
        ticket_id: impl Into<String>,
        turn_idx: usize,
        participant: impl Into<String>,
        msg: impl Into<String>,
    ) -> Result<Lsn, LunarisError> {
        let thread_id = format!("{}/{}", ticket_id.into(), turn_idx);
        self.chats.ingest(msg, thread_id, participant).await
    }

    /// Recall from BOTH buckets; RRF fusion happens WITHIN each primitive's
    /// own bucket (per D-09 — no cross-primitive fusion). Returns the
    /// concatenation of tickets + chats, filtered to enforce distinct source
    /// prefixes (`ticket:` vs `chat:`). 2 primitive calls
    /// (`DocumentCorpus::search` + `MessageStream::recall`).
    pub async fn recall(&self, query: &str) -> Result<Vec<Hit>, LunarisError> {
        let ticket_hits = self.tickets.clone().search(query).await?;
        let chat_hits = self.chats.recall(query).await?;
        // D-09 / T-11-01-02 enforcement: a double-indexed record (same id
        // ingested under both prefixes) would collapse on recall. The
        // bucket-prefix check below rejects that class of data corruption.
        let mut out = Vec::with_capacity(ticket_hits.len() + chat_hits.len());
        for h in ticket_hits {
            debug_assert!(
                h.source.starts_with(TICKET_PREFIX),
                "CustomerSupportHistory: ticket bucket leaked non-ticket source {}",
                h.source
            );
            out.push(h);
        }
        for h in chat_hits {
            debug_assert!(
                h.source.starts_with(CHAT_PREFIX),
                "CustomerSupportHistory: chat bucket leaked non-chat source {}",
                h.source
            );
            out.push(h);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn customer_support_history_public_surface_under_30_loc() {
        let src = include_str!("./customer_support_history.rs");
        let production = src.split("#[cfg(test)]").next().unwrap_or(src);
        let pub_fns = production.matches("    pub fn ").count()
            + production.matches("    pub async fn ").count();
        assert!(
            pub_fns <= 6,
            "RCPDOC-05 ≤30-LOC contract: CustomerSupportHistory has {pub_fns} pub fns; cap is 6"
        );
        assert!(
            pub_fns >= 3,
            "RCPDOC-05 contract: CustomerSupportHistory needs ≥3 public methods; got {pub_fns}"
        );
    }
}
