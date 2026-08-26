//! The GGUF model catalogue — one place that names the artifacts.
//!
//! This module is the single source of truth for *which* weights Lunaris runs
//! on: the mirror URL, the pinned SHA-256, the filename the engine looks for,
//! and the directory it looks in. It carries no HTTP, no hashing and no
//! progress bar, so it costs nothing to depend on — every crate that needs to
//! name a model can, whether or not it can download one. The downloading half
//! lives in [`crate::model_staging`], behind the optional `model-staging`
//! feature.
//!
//! # Why one place
//!
//! Until W0.7 the URL and digest were duplicated across two full staging
//! implementations and three CI workflows, held together by "keep in sync"
//! comments. The failure that shape produces is quiet: one copy re-pins the
//! mirror, and the MCP server and `lunaris try` stage *different* weights
//! under the same filename, so every comparison between them silently stops
//! meaning anything. `crates/lunaris-core/tests/model_catalogue.rs` pins the
//! invariant — exactly one source file may name a digest, and every workflow
//! literal must equal this catalogue's.
//!
//! # Model sources (verified 2026-05-24 via HF API `/api/models/{repo}/tree/main`)
//!
//! | Kind | HF repo | SHA-256 (LFS oid) | Size |
//! |---|---|---|---|
//! | Embedder Q4_K_M | `mykor/granite-embedding-311m-multilingual-r2-GGUF` | `58d27f63…` | 253 MB |
//! | Reranker Q5_K_M | `gpustack/bge-reranker-v2-m3-GGUF` | `1a212007…` | 468 MB |
//!
//! Files are saved under the locked dot-separated quant name from the brief,
//! NOT the mirror's hyphen-separated name: the engine looks up by the locked
//! name, and consistency beats provenance-mirroring.

use std::path::{Path, PathBuf};

/// The GGUF models Lunaris can stage on first use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ModelKind {
    /// `ibm-granite/granite-embedding-311m-multilingual-r2` — Q4_K_M GGUF.
    ///
    /// Mirrored from `mykor/granite-embedding-311m-multilingual-r2-GGUF`
    /// (community conversion; the upstream official repo has no GGUF
    /// artifacts). LFS sha256 verified via the HF API on 2026-05-24.
    EmbedderGraniteQ4KM,

    /// `BAAI/bge-reranker-v2-m3` — Q5_K_M GGUF.
    ///
    /// Mirrored from `gpustack/bge-reranker-v2-m3-GGUF` (11k downloads;
    /// highest-quality community conversion). LFS sha256 verified via the HF
    /// API on 2026-05-24.
    RerankerBgeV2M3Q5KM,
}

impl ModelKind {
    /// The locked filename under [`models_dir`].
    ///
    /// This is what `Lunaris::open` looks for when `LUNARIS_EMBEDDER_GGUF` /
    /// `LUNARIS_RERANKER_GGUF` are unset, so a stager that writes a different
    /// name has done nothing at all.
    #[must_use]
    pub fn filename(self) -> &'static str {
        match self {
            Self::EmbedderGraniteQ4KM => "granite-embedding-311m-multilingual-r2.Q4_K_M.gguf",
            Self::RerankerBgeV2M3Q5KM => "bge-reranker-v2-m3.Q5_K_M.gguf",
        }
    }

    /// Canonical HuggingFace `resolve/main/` URL for the GGUF file.
    ///
    /// These stable URLs redirect to HF's xet/CDN infrastructure; a client
    /// with the default redirect policy follows them.
    #[must_use]
    pub fn url(self) -> &'static str {
        match self {
            Self::EmbedderGraniteQ4KM => {
                "https://huggingface.co/mykor/granite-embedding-311m-multilingual-r2-GGUF/resolve/main/granite-embedding-311M-multilingual-r2-Q4_K_M.gguf"
            }
            Self::RerankerBgeV2M3Q5KM => {
                "https://huggingface.co/gpustack/bge-reranker-v2-m3-GGUF/resolve/main/bge-reranker-v2-m3-Q5_K_M.gguf"
            }
        }
    }

    /// Expected SHA-256 hex digest (the HF Git-LFS `oid sha256:HEX`).
    ///
    /// A mismatch on freshly downloaded bytes is an integrity failure, not a
    /// bug: the mirror changed content.
    #[must_use]
    pub fn sha256(self) -> &'static str {
        match self {
            Self::EmbedderGraniteQ4KM => {
                "58d27f63e69ccf7abce27bf6b35bb0edebc3a1c05ad4a3165acaba1cdca107c0"
            }
            Self::RerankerBgeV2M3Q5KM => {
                "1a212007526c7083627eed92b39dd4472e90ff1374a03fb068733378220813ef"
            }
        }
    }

    /// Approximate download size in MB.
    ///
    /// Used only to set expectations in prose before a progress bar appears —
    /// a number the user sees before the wait is what turns a hang into a wait.
    #[must_use]
    pub fn size_mb(self) -> u64 {
        match self {
            Self::EmbedderGraniteQ4KM => 253,
            Self::RerankerBgeV2M3Q5KM => 468,
        }
    }

    /// Short human-readable name for progress messages.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::EmbedderGraniteQ4KM => "granite-embedding-311m Q4_K_M",
            Self::RerankerBgeV2M3Q5KM => "bge-reranker-v2-m3 Q5_K_M",
        }
    }

    /// The environment variable an operator sets to point at their own
    /// weights for this slot, bypassing staging entirely.
    #[must_use]
    pub fn env_override(self) -> &'static str {
        match self {
            Self::EmbedderGraniteQ4KM => "LUNARIS_EMBEDDER_GGUF",
            Self::RerankerBgeV2M3Q5KM => "LUNARIS_RERANKER_GGUF",
        }
    }
}

/// Where staged GGUFs live: `$HOME/.lunaris/models/`.
///
/// `$HOME` is read from the environment **first**, with [`dirs::home_dir`] as
/// the fallback, because that is the order the engine's own lookup uses. The
/// two must agree: resolving the home directory differently here would stage
/// bytes into a directory `Lunaris::open` never consults, which reads exactly
/// like a successful download of a model that then does nothing.
///
/// Returns `None` only when neither source yields a home directory.
///
/// # Relocating the directory
///
/// `LUNARIS_MODELS_DIR` (and its predecessor `LUNARIS_MCP_MODELS_DIR`)
/// override the result outright. Because `Lunaris::open` now resolves its
/// staged-artifact default through this same function, the override moves the
/// staging target and the engine's lookup **together** — which is the point.
/// Before W0.7 only the MCP stager read `LUNARIS_MCP_MODELS_DIR`, so setting
/// it downloaded into a directory the engine did not consult: a successful
/// 253 MB download of a model that then did nothing.
///
/// An operator who wants one specific file, rather than a different directory,
/// points [`ModelKind::env_override`] at it instead.
#[must_use]
pub fn models_dir() -> Option<PathBuf> {
    for var in ["LUNARIS_MODELS_DIR", "LUNARIS_MCP_MODELS_DIR"] {
        if let Some(dir) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(dir));
        }
    }
    home_dir().map(|h| h.join(".lunaris").join("models"))
}

/// The full path a staged `kind` occupies, or `None` with no home directory.
///
/// Existence is **not** checked — callers that need "is it there?" stat it
/// themselves, and callers that need "put it there" pass it to the stager.
#[must_use]
pub fn staged_path(kind: ModelKind) -> Option<PathBuf> {
    models_dir().map(|d| d.join(kind.filename()))
}

/// [`staged_path`] against an explicit home directory — the pure core of the
/// path rule, so it can be asserted without mutating process environment.
#[must_use]
pub fn staged_path_in(home: &Path, kind: ModelKind) -> PathBuf {
    home.join(".lunaris").join("models").join(kind.filename())
}

/// The home directory, resolved the way every Lunaris path resolves it:
/// the `HOME` environment variable first, [`dirs::home_dir`] as the fallback.
///
/// The order matters and is not arbitrary. `Lunaris::open` reads `$HOME`, so
/// anything that resolves a home directory differently — `dirs` alone is the
/// tempting one — writes to a place the engine will not read under `sudo`,
/// under `launchd`, or in a container. That failure is silent on both sides.
#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(dirs::home_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_digest_is_a_full_lowercase_sha256() {
        for kind in [ModelKind::EmbedderGraniteQ4KM, ModelKind::RerankerBgeV2M3Q5KM] {
            let d = kind.sha256();
            assert_eq!(d.len(), 64, "{kind:?} digest is not 64 hex chars: {d}");
            assert!(
                d.chars().all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()),
                "{kind:?} digest must be lowercase hex: {d}"
            );
        }
    }

    #[test]
    fn the_url_ends_in_a_gguf_and_the_two_kinds_never_collide() {
        let e = ModelKind::EmbedderGraniteQ4KM;
        let r = ModelKind::RerankerBgeV2M3Q5KM;
        for kind in [e, r] {
            assert!(kind.url().ends_with(".gguf"), "{kind:?} url does not end in .gguf");
            assert!(kind.filename().ends_with(".gguf"));
        }
        assert_ne!(e.filename(), r.filename());
        assert_ne!(e.sha256(), r.sha256());
        assert_ne!(e.url(), r.url());
        assert_ne!(e.env_override(), r.env_override());
    }

    #[test]
    fn staged_path_in_is_home_relative_and_carries_the_locked_name() {
        let p = staged_path_in(Path::new("/home/x"), ModelKind::EmbedderGraniteQ4KM);
        assert!(p.starts_with("/home/x/.lunaris/models"));
        assert_eq!(
            p.file_name().unwrap(),
            ModelKind::EmbedderGraniteQ4KM.filename(),
            "the staged file must carry the locked name, not the mirror's"
        );
    }
}
