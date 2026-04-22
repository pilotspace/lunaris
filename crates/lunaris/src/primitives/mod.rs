//! Phase 10/11/12 Option-A relocation — primitives that must live in the
//! umbrella `lunaris` crate because Phase 12 `HeliosScratchpad` (also in
//! `lunaris`) composes over them. Previously these lived in
//! `lunaris-recipes`, but `lunaris-recipes → lunaris` dependency direction
//! made the reverse import impossible. Moving them down (crate-wise) breaks
//! the cycle without touching the public Phase 9 fluent API — the recipes
//! crate re-exports these names so `use lunaris_recipes::WorkingMemory;`
//! keeps compiling.
//!
//! Phase 13 proper lunaris-primitives crate extraction will subsume this
//! module.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

pub mod working_memory;

pub use working_memory::WorkingMemory;
