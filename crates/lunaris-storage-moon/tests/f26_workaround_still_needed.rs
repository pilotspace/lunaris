//! F26 reverse-ratchet — fire when the vendored Moon GAINS the parser fix.
//!
//! `render_valid_time_filter` encodes an exclusive upper bound as `hi - 1`
//! instead of the grammar's `(hi`, because Moon's KNN-prefilter parser cannot
//! read a `(`-prefixed bound and silently drops the WHOLE filter when it
//! fails — returning more rows than asked for, with no error. See F26 and
//! `knn_prefilter_is_never_silently_dropped.rs`.
//!
//! The fix landed upstream on `main` (2026-08-23, moon#648): the parser now
//! does `strip_prefix('(')`, and an unparseable filter maps to
//! `FilterParse::Invalid` rather than falling through unfiltered. It is in NO
//! RELEASE — measured `strip_prefix('(')` counts in that file: v0.8.6 = 0,
//! v0.8.7 = 0, main = 1.
//!
//! So the workaround must stay for now. The hazard is that it stays FOREVER:
//! once `vendor/moon` is bumped past the fix, nothing would ever tell anyone
//! the `hi - 1` dance is obsolete, and a well-documented workaround stops
//! attracting attention precisely because it is well documented.
//!
//! This test asserts the vendored parser still LACKS the fix. It is expected
//! to pass today and to FAIL the moment someone bumps the submodule to a Moon
//! that carries it — at which point the failure message is the to-do list.
//!
//! Deliberately NOT behind `moon-it`: it inspects vendored source, starts no
//! server, and `lunaris-storage-moon` cannot compile without `vendor/moon`
//! anyway (moondb is a path dependency there), so the file is always present
//! wherever this test builds. A skip-if-missing branch would make it a check
//! that reports nothing.

use std::path::PathBuf;

fn vendored_parser() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/moon/src/command/vector_search/ft_search/parse.rs")
}

#[test]
fn the_vendored_knn_prefilter_parser_still_lacks_the_exclusive_bound_fix() {
    let path = vendored_parser();
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the vendored Moon parser at {}: {e}\n\
             This is not a skip condition: lunaris-storage-moon depends on \
             moondb by path from vendor/moon, so if the crate compiled, the \
             submodule is checked out. A missing file here means the tree is \
             in a state this test cannot reason about.",
            path.display()
        )
    });

    // The upstream fix is identifiable by the exclusive-bound strip in the
    // KNN-prefilter parser. Match on the decision (does it understand `(`?),
    // not on a version string — a version can be bumped without the fix, and
    // the fix reached `main` long before any release carried it.
    let has_exclusive_bounds = src.contains("strip_prefix('(')");

    assert!(
        !has_exclusive_bounds,
        "vendor/moon's KNN-prefilter parser now understands `(`-prefixed \
         exclusive bounds, so F26's workaround is obsolete.\n\
         \n\
         This test failing is GOOD NEWS and a to-do list:\n\
           1. Confirm the silent-degradation half too — `parse_filter_string` \
             returning None must map to `FilterParse::Invalid`, not fall \
             through to an unfiltered search. The `(` fix alone is the small \
             half.\n\
          2. Retire the `hi - 1` encoding in `render_valid_time_filter` in \
             favour of `(hi`, and drop the explanatory comment block.\n\
          3. Re-point `knn_prefilter_is_never_silently_dropped.rs` at a \
             `(`-bounded filter, so it proves the new path instead of the \
             workaround.\n\
          4. Delete this test.\n\
         \n\
         Parser: {}",
        path.display()
    );
}
