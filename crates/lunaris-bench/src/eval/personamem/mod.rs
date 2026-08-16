//! PersonaMem — persona-tracking MCQ benchmark over the PRODUCTION
//! ingest/recall path. Harness shell lands in the GREEN commit; this RED
//! commit pins the pure parsing / prompting / scoring contract.

#![forbid(unsafe_code)]

pub(crate) mod dataset;
pub(crate) mod reader;
