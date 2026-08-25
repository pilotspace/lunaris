//! W4.18 — the error-handling pattern the whole cookbook teaches must compile.
//!
//! Found by compiling the book's Rust fences for the first time. Ten pages open
//! the same way:
//!
//! ```text
//! async fn main() -> Result<(), lunaris::LunarisError> {
//!     let lunaris = Lunaris::open("moon://localhost:6380").await?;
//!     let scope   = Scope::new("acme-workspace")?;
//! ```
//!
//! and the second `?` does not compile. `Scope::new` returns
//! `Result<_, ScopeError>`, `LunarisError` had no `From<ScopeError>`, so the
//! reader gets E0277 on the second line of every recipe.
//!
//! This is an API gap rather than a documentation bug. `Result<_,
//! LunarisError>` is the natural signature for a function that talks to
//! Lunaris, `Scope::new` is how you name a partition, and `?` is how Rust
//! composes them. Refusing that combination pushes every caller into a
//! `map_err` that carries no information.
//!
//! `LunarisError` is `#[non_exhaustive]` precisely so a variant can be added
//! without breaking downstream matches — its own doc comment says so.

use lunaris_core::{LunarisError, Scope, ScopeError};

/// The shape every cookbook page uses: one function, one error type, `?` on
/// both a Lunaris call and a `Scope::new`.
fn as_a_reader_would_write_it(name: &str) -> Result<Scope, LunarisError> {
    let scope = Scope::new(name)?;
    Ok(scope)
}

#[test]
fn scope_new_composes_with_question_mark_under_the_umbrella_error() {
    let ok = as_a_reader_would_write_it("acme-workspace").expect("a valid scope must succeed");
    assert_eq!(ok.as_str(), "acme-workspace");
}

#[test]
fn an_invalid_scope_arrives_as_a_scope_variant_not_a_stringly_one() {
    // The conversion has to preserve WHICH thing went wrong. Flattening it into
    // `Backend(String)` would compile and would make every scope typo look like
    // a storage failure — the exact confusion the taxonomy exists to prevent.
    let err = as_a_reader_would_write_it("not a valid scope!")
        .expect_err("a scope with a space and a bang must be rejected");
    assert!(
        matches!(err, LunarisError::Scope(ScopeError::Invalid(_))),
        "expected LunarisError::Scope(ScopeError::Invalid), got {err:?}"
    );
}

#[test]
fn the_rendered_message_still_names_the_scope_that_was_rejected() {
    // An operator reading a log needs the offending string, not just the class.
    let err = as_a_reader_would_write_it("nope!").expect_err("must be rejected");
    let rendered = err.to_string();
    assert!(rendered.contains("nope!"), "message lost the offending value: {rendered}");
    assert!(rendered.contains("scope"), "message does not say what kind of error: {rendered}");
}
