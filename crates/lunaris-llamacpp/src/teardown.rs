//! Process-wide inference teardown registry (exit-time Metal safety).
//!
//! ggml-metal's device table is destroyed by a C++ static destructor during
//! `exit()` (`__cxa_finalize`), and `ggml_metal_rsets_free` ASSERTS that its
//! residency sets are empty — i.e. that every Metal buffer (model weights,
//! compute buffers) was freed first. Host runtimes do not guarantee that:
//! CPython/Node may leak binding objects at interpreter shutdown, and the
//! SDK handle registry is process-global — so a worker that finished all of
//! its work cleanly still ABORTS inside `exit()` (SIGABRT, exit 134; proven
//! live 2026-07-17 at `ggml-metal-device.m:622` in a Python worker).
//!
//! Every engine (`LlamaCppEmbedder`, `LlamaCppReranker`) parks its heavy
//! state behind a takeable [`EngineCell`] registered here; bindings call
//! [`shutdown_all_inference`] from the host's atexit hook — which runs
//! BEFORE `exit()`'s static destructors — deterministically freeing every
//! model, warm context, and worker thread. After teardown an engine returns
//! its typed `Closed` error instead of encoding.

use std::sync::{Arc, Mutex, Weak};

/// One engine's takeable heavy state (`T` = the engine's `Inner`).
pub(crate) struct EngineCell<T: Send + Sync + 'static>(Mutex<Option<Arc<T>>>);

impl<T: Send + Sync + 'static> EngineCell<T> {
    /// Wrap `inner` and register the cell for process-wide teardown.
    pub(crate) fn new(inner: Arc<T>) -> Arc<Self> {
        let cell = Arc::new(Self(Mutex::new(Some(inner))));
        register(Arc::clone(&cell) as Arc<dyn Teardown>);
        cell
    }

    /// Clone the live inner, or `None` once teardown has run.
    pub(crate) fn get(&self) -> Option<Arc<T>> {
        self.0.lock().expect("engine cell poisoned").clone()
    }
}

/// Object-safe "free your heavy state" hook, one impl for all engine types.
trait Teardown: Send + Sync {
    /// Take + drop the heavy state; `true` when something was freed.
    fn teardown(&self) -> bool;
}

impl<T: Send + Sync + 'static> Teardown for EngineCell<T> {
    fn teardown(&self) -> bool {
        let taken = self.0.lock().expect("engine cell poisoned").take();
        // `taken` drops at the end of this fn, OUTSIDE the cell lock. When
        // it is the last `Arc<Inner>`, the engine's worker thread is joined
        // here (`EncodeWorker::drop`) — in-flight encodes finish first
        // because they hold their own `Arc<Inner>` clone.
        taken.is_some()
    }
}

/// `std::sync` (not parking_lot) to match this crate's plain-sync worker
/// discipline; never held across an await (the crate has no async locks).
static REGISTRY: Mutex<Vec<Weak<dyn Teardown>>> = Mutex::new(Vec::new());

fn register(cell: Arc<dyn Teardown>) {
    let mut reg = REGISTRY.lock().expect("teardown registry poisoned");
    reg.retain(|w| w.strong_count() > 0);
    reg.push(Arc::downgrade(&cell));
}

/// Free every live inference engine: model weights, warm context, worker
/// thread. Idempotent; returns how many engines this call actually freed.
///
/// Bindings register this with the host runtime's atexit hook (Python
/// `atexit`, which runs before C++ static destructors) so the process never
/// reaches ggml-metal's `rsets`-empty assertion with live buffers.
pub fn shutdown_all_inference() -> usize {
    let live: Vec<Arc<dyn Teardown>> = {
        let mut reg = REGISTRY.lock().expect("teardown registry poisoned");
        let live = reg.iter().filter_map(Weak::upgrade).collect();
        reg.clear();
        live
    };
    // Teardown OUTSIDE the registry lock: dropping joins worker threads.
    live.into_iter().filter(|t| t.teardown()).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FlagOnDrop(Arc<AtomicBool>);
    impl Drop for FlagOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// One combined test: the registry is process-global, so parallel test
    /// threads would race each other's register/shutdown counts.
    #[test]
    fn shutdown_frees_registered_engines_then_reports_closed() {
        let dropped = Arc::new(AtomicBool::new(false));
        let cell = EngineCell::new(Arc::new(FlagOnDrop(Arc::clone(&dropped))));
        assert!(cell.get().is_some(), "cell must serve inner before teardown");

        // An already-dropped engine must not linger as a freeable entry.
        {
            let _gone = EngineCell::new(Arc::new(FlagOnDrop(Arc::new(AtomicBool::new(false)))));
        }

        let freed = shutdown_all_inference();
        assert_eq!(freed, 1, "exactly the one live engine must be freed");
        assert!(dropped.load(Ordering::SeqCst), "inner must actually drop");
        assert!(cell.get().is_none(), "post-teardown reads must see Closed");

        // Idempotent: nothing left to free.
        assert_eq!(shutdown_all_inference(), 0);
    }
}
