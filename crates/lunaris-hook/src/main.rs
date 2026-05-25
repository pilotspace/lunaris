//! RED STUB — exits 73 unconditionally so integration tests fail.
//!
//! GREEN task replaces this with the full stdin-read → parse → ingest → exit pipeline.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, unreachable_pub)]

fn main() {
    // RED: unconditional exit 73 so integration tests asserting exit 0 fail.
    std::process::exit(73);
}
