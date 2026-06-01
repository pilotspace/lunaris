//! Plan 05-04 — opinionated v0 recipes (helios-rfc §5.3 surface).
//!
//! Per blueprint §7 "opinionation lives here". v0 ships only [`CodingSessionMemory`];
//! the other 9 recipes (`RECIPE-V1-01..11`) ship in v1.
//!
//! See [`coding_session_memory`] for the canonical helios-rfc §5.3 file-tool surface
//! (write / read / edit / grep / ls / forget / time-travel via `as_of`).

#![forbid(unsafe_code)]

pub mod coding_session_memory;

pub use coding_session_memory::{AsOfScratchpad, CodingSessionMemory};

/// Deprecated alias for [`CodingSessionMemory`].
///
/// v0.4 consumers importing `lunaris::HeliosScratchpad` continue to compile;
/// they receive a `#[deprecated]` warning. Remove in v0.7.
#[deprecated(
    since = "0.5.0",
    note = "use CodingSessionMemory; HeliosScratchpad will be removed in v0.7"
)]
pub use coding_session_memory::HeliosScratchpad;
