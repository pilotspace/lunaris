//! TokenCounter trait and implementations — CHUNK-01.
//!
//! ## Design
//!
//! [`TokenCounter`] is a trait with two impls:
//! - [`BpeTokenCounter`]: real BPE round-trip via `tokenizers::Tokenizer::from_file`.
//! - [`SurrogateTokenCounter`]: v0 heuristic `ceil(words * 1.3)`, kept as the
//!   documented degraded path and fallback.
//!
//! [`make_token_counter`] returns `Arc<dyn TokenCounter + Send + Sync>`:
//! - `Some(path)` where the file exists and parses → `BpeTokenCounter`
//! - `Some(path)` where load fails → `tracing::warn!` + `SurrogateTokenCounter`
//! - `None` → `SurrogateTokenCounter`
//!
//! The surrogate is NOT deleted. `est_token_count` in `mod.rs` remains
//! the canonical surrogate entry point for the pipeline default path.

use std::path::Path;
use std::sync::Arc;

/// Count the number of BPE tokens in a text string.
pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str) -> u32;
}

// ---------------------------------------------------------------------------
// SurrogateTokenCounter
// ---------------------------------------------------------------------------

/// v0 BPE surrogate: `ceil(words * 1.3)`. Kept as the documented degraded
/// path and fallback when a real tokenizer is unavailable.
#[derive(Clone, Debug, Default)]
pub struct SurrogateTokenCounter;

impl TokenCounter for SurrogateTokenCounter {
    #[inline]
    fn count(&self, text: &str) -> u32 {
        let words = text.split_whitespace().count();
        (words as f32 * 1.3).ceil() as u32
    }
}

// ---------------------------------------------------------------------------
// BpeTokenCounter
// ---------------------------------------------------------------------------

/// Real BPE round-trip counter backed by a HuggingFace `tokenizers::Tokenizer`
/// loaded from a file.
pub struct BpeTokenCounter {
    tokenizer: tokenizers::Tokenizer,
}

impl BpeTokenCounter {
    /// Load a tokenizer from `path`. Returns an error if the file is missing
    /// or the JSON is malformed.
    pub fn try_new(path: &Path) -> Result<Self, String> {
        tokenizers::Tokenizer::from_file(path).map(|tokenizer| Self { tokenizer }).map_err(|e| {
            format!("BpeTokenCounter: failed to load tokenizer from {}: {}", path.display(), e)
        })
    }
}

impl TokenCounter for BpeTokenCounter {
    fn count(&self, text: &str) -> u32 {
        // add_special_tokens=false: we only count content tokens, not BOS/EOS.
        match self.tokenizer.encode(text, false) {
            Ok(enc) => enc.len() as u32,
            Err(_) => {
                // Encoding failure is rare; degrade to surrogate silently.
                SurrogateTokenCounter.count(text)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

/// Create a [`TokenCounter`] from an optional tokenizer file path.
///
/// - `Some(path)` where load succeeds → [`BpeTokenCounter`]
/// - `Some(path)` where load fails → logs `tracing::warn!` and returns [`SurrogateTokenCounter`]
/// - `None` → [`SurrogateTokenCounter`]
pub fn make_token_counter(path: Option<&Path>) -> Arc<dyn TokenCounter + Send + Sync> {
    match path {
        Some(p) => match BpeTokenCounter::try_new(p) {
            Ok(bpe) => {
                tracing::debug!(path = %p.display(), "BpeTokenCounter loaded");
                Arc::new(bpe)
            }
            Err(e) => {
                tracing::warn!(err = %e, "BpeTokenCounter init failed; falling back to SurrogateTokenCounter");
                Arc::new(SurrogateTokenCounter)
            }
        },
        None => Arc::new(SurrogateTokenCounter),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Path to the committed test fixture.
    fn fixture_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/test_tokenizer.json")
    }

    #[test]
    fn surrogate_counter_matches_est_token_count() {
        let counter = SurrogateTokenCounter;
        // 10 words × 1.3 = 13.0 → ceil = 13
        let text = "one two three four five six seven eight nine ten";
        let surrogate_result = counter.count(text);
        // Must match est_token_count from mod.rs (same formula)
        let expected = (text.split_whitespace().count() as f32 * 1.3).ceil() as u32;
        assert_eq!(surrogate_result, expected);
        assert_eq!(surrogate_result, 13);
    }

    #[test]
    fn bpe_counter_try_new_fails_on_missing_file() {
        let bad_path = std::path::Path::new("/nonexistent/path/tokenizer.json");
        assert!(BpeTokenCounter::try_new(bad_path).is_err());
    }

    #[test]
    fn bpe_count_via_fixture() {
        let path = fixture_path();
        let counter = BpeTokenCounter::try_new(&path).expect("fixture must load");
        let text = "hello world";
        let count = counter.count(text);
        // BPE count must be > 0 for non-empty input
        assert!(count > 0, "BPE count must be > 0 for non-empty input");
    }

    #[test]
    fn make_token_counter_good_path() {
        let path = fixture_path();
        let counter = make_token_counter(Some(&path));
        // Good path returns a BpeTokenCounter; we verify it gives a count > 0
        assert!(counter.count("hello world") > 0);
    }

    #[test]
    fn make_token_counter_bad_path_falls_back_to_surrogate() {
        let bad = std::path::Path::new("/nonexistent/tokenizer.json");
        let counter = make_token_counter(Some(bad));
        // Fallback to surrogate: "hello world" = 2 words × 1.3 = 2.6 → ceil = 3
        let expected = (2_f32 * 1.3).ceil() as u32;
        assert_eq!(counter.count("hello world"), expected);
    }

    #[test]
    fn make_token_counter_none_returns_surrogate() {
        let counter = make_token_counter(None);
        // surrogate: 10 words × 1.3 = 13
        let text = "one two three four five six seven eight nine ten";
        assert_eq!(counter.count(text), 13);
    }

    /// UAT: BPE count from the production granite tokenizer differs from surrogate.
    /// Requires GRANITE_R2_TOKENIZER_PATH to point to a valid tokenizer.json.
    #[test]
    #[ignore]
    fn bpe_counter_differs_from_surrogate_for_known_input() {
        let path = std::env::var("GRANITE_R2_TOKENIZER_PATH")
            .expect("GRANITE_R2_TOKENIZER_PATH must be set for this UAT test");
        let bpe = BpeTokenCounter::try_new(std::path::Path::new(&path))
            .expect("granite tokenizer must load");
        let surrogate = SurrogateTokenCounter;
        let text = "The quick brown fox jumps over the lazy dog near the riverbank.";
        let bpe_count = bpe.count(text);
        let surrogate_count = surrogate.count(text);
        assert_ne!(
            bpe_count, surrogate_count,
            "BPE and surrogate must differ for English prose (bpe={bpe_count}, surrogate={surrogate_count})"
        );
        // BPE should be in a reasonable range (not wildly off from surrogate)
        let ratio = bpe_count as f64 / surrogate_count as f64;
        assert!(
            (0.5..=2.0).contains(&ratio),
            "BPE/surrogate ratio {ratio:.2} out of expected range [0.5, 2.0]"
        );
    }
}
