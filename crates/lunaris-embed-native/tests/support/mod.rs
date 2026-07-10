//! Shared test-support: a minimal capturing `tracing_subscriber::Layer` that
//! records span names + fields so `tests/span_emission.rs` can assert the
//! `lunaris.embed.*` instrumentation (see
//! `docs/design/quantized-inference-extractor-reranker.md` §4b) actually
//! fires — without depending on an external crate like `tracing-test`.
//!
//! Not a `tests/*.rs` file itself (no `#[test]`s here) — `tests/support/mod.rs`
//! is a module included via `mod support;`, NOT compiled as its own harness
//! binary (cargo only auto-discovers top-level `tests/*.rs` files).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// One closed span's name + recorded fields (stringified — good enough for
/// assertions like "field X was recorded" / "field X equals N").
#[derive(Debug, Clone, Default)]
pub struct CapturedSpan {
    pub name: &'static str,
    pub fields: HashMap<String, String>,
}

/// `tracing_subscriber::Layer` that appends a [`CapturedSpan`] to a shared
/// `Vec` every time an instrumented span closes. Field values recorded both
/// at span-creation (`on_new_span`) and via `Span::record` after the fact
/// (`on_record`) are merged — mirrors how the production code records
/// `tracing::field::Empty` placeholders then fills them in post-hoc.
#[derive(Clone, Default)]
pub struct CapturingLayer {
    captured: Arc<Mutex<Vec<CapturedSpan>>>,
}

impl CapturingLayer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of every span closed so far, in close order.
    pub fn captured(&self) -> Vec<CapturedSpan> {
        self.captured.lock().expect("capturing layer mutex poisoned").clone()
    }
}

struct FieldVisitor(HashMap<String, String>);

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_string(), format!("{value:?}"));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

impl<S> Layer<S> for CapturingLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor(HashMap::new());
        attrs.record(&mut visitor);
        if let Some(span_ref) = ctx.span(id) {
            span_ref.extensions_mut().insert(visitor.0);
        }
    }

    fn on_record(&self, id: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span_ref) = ctx.span(id) else { return };
        let mut extensions = span_ref.extensions_mut();
        let Some(fields) = extensions.get_mut::<HashMap<String, String>>() else { return };
        let mut visitor = FieldVisitor(std::mem::take(fields));
        values.record(&mut visitor);
        *fields = visitor.0;
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        let Some(span_ref) = ctx.span(&id) else { return };
        let name = span_ref.name();
        let fields =
            span_ref.extensions().get::<HashMap<String, String>>().cloned().unwrap_or_default();
        self.captured.lock().expect("capturing layer mutex poisoned").push(CapturedSpan {
            name,
            fields,
        });
    }
}
