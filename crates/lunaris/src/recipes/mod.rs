//! Plan 05-04 — opinionated v0 recipes (helios-rfc §5.3 surface).
//!
//! Per blueprint §7 "opinionation lives here". v0 ships only [`HeliosScratchpad`];
//! the other 9 recipes (`RECIPE-V1-01..11`) ship in v1.
//!
//! See [`helios_scratchpad`] for the canonical helios-rfc §5.3 file-tool surface
//! (write / read / edit / grep / ls / forget / time-travel via `as_of`).

#![forbid(unsafe_code)]

pub mod helios_scratchpad;

pub use helios_scratchpad::{AsOfScratchpad, HeliosScratchpad};
