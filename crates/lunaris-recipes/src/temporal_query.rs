//! Stub — filled in by Phase 9 Plan 09-04 (TemporalQuery / PRIM-03).
#![allow(dead_code)]
use std::marker::PhantomData;

pub trait SupportsAsOf {}
pub struct TemporalQuery<S>(PhantomData<S>);
