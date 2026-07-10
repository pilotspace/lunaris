# Helios ↔ Lunaris Integration Recipe

> **⚠️ v0.6 llama.cpp-only cutover (ADR 2026-07-10).** This recipe was
> written against the v0.4/v0.5 candle stack. The **architecture is still
> the supported pattern** — Helios builds `Arc<dyn Embedder>` /
> `Arc<dyn Reranker>` components and hot-swaps the Lunaris handle via
> `ArcSwap` — but the candle constructors in §3 were deleted. Mapping:
>
> | This doc says | Use instead |
> |---|---|
> | `NativeEmbedder` / `NativeQuantizedEmbedder` (`lunaris_embed_native`) | `lunaris_llamacpp::LlamaCppEmbedder::open(LlamaCppEmbedderOpts { gguf_path, .. })` — granite-r2 Q4_K_M GGUF |
> | `NativeReranker` / `NativeQuantizedReranker` (`lunaris_rerank_native`) | `lunaris_llamacpp::LlamaCppReranker::open(LlamaCppRerankerOpts { gguf_path, .. })` — bge-reranker-v2-m3 Q5_K_M GGUF |
> | features `embedder-gguf` / `reranker-gguf` | feature `llamacpp` (default on the umbrella) |
> | `candle-core` dep + `metal`/`cuda`/`cpu-mkl` candle features | no candle dep; build with umbrella `metal` / `cuda` / `vulkan` |
> | FP16 ↔ Q4 / FP32 ↔ Q5 runtime toggle | collapsed — one GGUF tier per model; the remaining toggles are llamacpp ↔ Ollama-remote ↔ noop |
>
> Output contracts are unchanged (768-d L2-normalized embeddings; sigmoid
> rerank scores ∈ [0, 1]), so §4–§17 (handle swap, config schema,
> hot-reload, CLI, test plan, ops) apply as written modulo constructor
> names. See
> [`docs/migration/0.5-to-0.6-llamacpp-only.md`](../migration/0.5-to-0.6-llamacpp-only.md)
> and the `lunaris-llamacpp` rustdoc for current signatures.


**Purpose:** Embed Lunaris as the memory engine for Helios with **runtime-toggleable** embedder/reranker modes (FP16 ↔ Q4, FP32 ↔ Q5, native ↔ Ollama) — no rebuild, no restart.

**Audience:** Helios engineers integrating against Lunaris v0.4 (commit `79c0d0d` or later).

**Stack assumption:** Helios is a **Rust** service. Pulls `lunaris` as a workspace dep. Reload trigger: **config file hot-reload** + **`helios settings` CLI command**.

---

## 1. What Lunaris gives you

| Component | Where | Notes |
|---|---|---|
| `Lunaris` handle | `lunaris::Lunaris` | Main entry point. `Send + Sync`; cheap to clone via `Arc`. |
| `Embedder` trait | `lunaris_core::embedder::Embedder` | `fn dim() -> usize`, `async fn embed_batch(&[&str]) -> Vec<Vec<f32>>` |
| `Reranker` trait | `lunaris_rerank::Reranker` | `async fn rerank(query, candidates) -> Vec<Hit>`; sigmoid scores ∈ [0, 1] |
| `NativeEmbedder` | `lunaris_embed_native` | granite-r2 FP16, 768-d |
| `NativeQuantizedEmbedder` | `lunaris_embed_native` (feature `embedder-gguf`) | granite-r2 Q4_K_M GGUF, 240 MiB |
| `NativeReranker` | `lunaris_rerank_native` | bge-reranker-v2-m3 FP32 |
| `NativeQuantizedReranker` | `lunaris_rerank_native` (feature `reranker-gguf`) | bge-reranker-v2-m3 Q5_K_M-imatrix GGUF, 446 MiB, **lazy mmap** |
| `OllamaEmbedder` | `lunaris_embed_remote` (feature `embed-remote`) | HTTP escape hatch |
| `NoopReranker` | `lunaris_rerank::NoopReranker` | Identity-rank fallback when no reranker is available |

The contract: as long as you give Lunaris an `Arc<dyn Embedder>` and an `Arc<dyn Reranker>`, **it doesn't care where they came from**. That's how runtime toggling works — Helios builds the components, hands them to Lunaris.

## 2. Cargo dependency

```toml
# crates/helios-memory/Cargo.toml
[dependencies]
lunaris               = { path = "../../vendor/lunaris/crates/lunaris" }
lunaris-core          = { path = "../../vendor/lunaris/crates/lunaris-core" }
lunaris-rerank        = { path = "../../vendor/lunaris/crates/lunaris-rerank" }
lunaris-embed-native  = { path = "../../vendor/lunaris/crates/lunaris-embed-native", features = ["embedder-gguf"] }
lunaris-rerank-native = { path = "../../vendor/lunaris/crates/lunaris-rerank-native", features = ["reranker-gguf"] }
lunaris-embed-remote  = { path = "../../vendor/lunaris/crates/lunaris-embed-remote" }

# Device — compile the one your deployment target needs.
# Apple Silicon dev / staging:  features = ["metal"]
# Linux x86_64 prod:            features = ["cuda"]  (or "cpu-mkl")
# Linux aarch64 prod:           features = []        (NEON default)
candle-core = { version = "0.10", default-features = false, features = ["metal"] }

arc-swap = "1"           # lockless atomic swap of the Lunaris handle
notify   = "8"           # file system watcher
notify-debouncer-mini = "0.7"
tokio    = { version = "1", features = ["full"] }
tracing  = "0.1"
thiserror = "2"
serde    = { version = "1", features = ["derive"] }
serde_yaml = "0.9"       # or toml — pick what matches Helios convention
```

**Why these features in particular:** every mode you want to be **runtime-toggleable** must be compiled in. There is no way to flip a Cargo feature at runtime.

## 3. SDK primer — the four constructors you need

### 3.1 FP16 embedder

```rust
use std::path::PathBuf;
use std::sync::Arc;
use candle_core::Device;
use lunaris_embed_native::{NativeEmbedder, NativeEmbedderOpts};
use lunaris_core::embedder::Embedder;

let embedder: Arc<dyn Embedder> = Arc::new(NativeEmbedder::open(NativeEmbedderOpts {
    weights_path:   PathBuf::from("/var/lib/helios/models/granite-r2/model.safetensors"),
    tokenizer_path: PathBuf::from("/var/lib/helios/models/granite-r2/tokenizer.json"),
    config_path:    PathBuf::from("/var/lib/helios/models/granite-r2/config.json"),
    device:         Device::new_metal(0)?,
})?);
```

### 3.2 Q4_K_M embedder (40% RSS reduction vs FP16)

```rust
use lunaris_embed_native::{NativeQuantizedEmbedder, NativeQuantizedEmbedderOpts};

let embedder: Arc<dyn Embedder> = Arc::new(NativeQuantizedEmbedder::open(NativeQuantizedEmbedderOpts {
    gguf_path:      PathBuf::from("/var/lib/helios/models/granite-r2/granite-r2-311m-Q4_K_M.gguf"),
    tokenizer_path: PathBuf::from("/var/lib/helios/models/granite-r2/tokenizer.json"),
    config_path:    PathBuf::from("/var/lib/helios/models/granite-r2/config.json"),
    device:         Device::new_metal(0)?,
})?);
```

### 3.3 FP32 reranker

```rust
use lunaris_rerank_native::{NativeReranker, NativeRerankerOpts};
use lunaris_rerank::Reranker;

let reranker: Arc<dyn Reranker> = Arc::new(NativeReranker::open(NativeRerankerOpts {
    weights_path:   PathBuf::from("/var/lib/helios/models/bge-reranker/model.safetensors"),
    tokenizer_path: PathBuf::from("/var/lib/helios/models/bge-reranker/tokenizer.json"),
    config_path:    PathBuf::from("/var/lib/helios/models/bge-reranker/config.json"),
    device:         Device::new_metal(0)?,
})?);
```

### 3.4 Q5_K_M-imatrix reranker (446 MiB, **lazy-mmap on first `rerank()` call**)

```rust
use lunaris_rerank_native::{NativeQuantizedReranker, NativeQuantizedRerankerOpts};

let reranker: Arc<dyn Reranker> = Arc::new(NativeQuantizedReranker::open(NativeQuantizedRerankerOpts {
    gguf_path:      PathBuf::from("/var/lib/helios/models/bge-reranker/bge-reranker-v2-m3-Q5_K_M-imatrix.gguf"),
    tokenizer_path: PathBuf::from("/var/lib/helios/models/bge-reranker/tokenizer.json"),
    config_path:    PathBuf::from("/var/lib/helios/models/bge-reranker/config.json"),
    device:         Device::new_metal(0)?,
})?);
```

### 3.5 Ollama HTTP embedder (escape hatch)

```rust
use lunaris_embed_remote::{OllamaEmbedder, OllamaEmbedderOpts};

let embedder: Arc<dyn Embedder> = Arc::new(OllamaEmbedder::new(OllamaEmbedderOpts {
    endpoint: "http://internal-ollama:11434".to_string(),
    model:    "nomic-embed-text".to_string(),
    dim:      768,
})?);
```

### 3.6 Noop reranker (recall-only, no rerank pass)

```rust
use lunaris_rerank::NoopReranker;
let reranker: Arc<dyn Reranker> = Arc::new(NoopReranker::default());
```

## 4. Lunaris handle — open, swap, query

```rust
use lunaris::Lunaris;

// Open with explicit embedder, then optionally chain reranker.
let lunaris = Lunaris::open_with_embedder(
    "redis://moon:6379",
    embedder.clone(),
).await?.with_reranker(reranker.clone());

// Query. Cheap clone — readers don't block writers.
let hits = lunaris.recall(query).await?;

// Hot-swap embedder (consumes self; dim-safe).
let lunaris = lunaris.try_with_embedder(new_embedder)?;
//                       ^^^^^^^^^^^^^^^^^^ returns Err on dim mismatch (N-04 D2)

// Hot-swap reranker (consumes self; no dim guardrail — score-space).
let lunaris = lunaris.with_reranker(new_reranker);
```

**Note**: `try_with_embedder` and `with_reranker` consume `self` and return a new handle. To keep a long-lived shared handle, wrap in `Arc<ArcSwap<Lunaris>>` (next section).

## 5. Recommended Helios architecture

```
crates/helios-memory/
├── Cargo.toml
├── src/
│   ├── lib.rs                   pub Memory { handle: Arc<ArcSwap<Lunaris>> }
│   ├── config.rs                MemoryConfig + validate()
│   ├── builder.rs               build_components(&MemoryConfig) -> (Arc<dyn Embedder>, Arc<dyn Reranker>)
│   ├── reload.rs                reload(memory, new_cfg) -> ReloadOutcome
│   └── error.rs                 HeliosMemoryError
└── tests/
    ├── reload_fp16_to_q4.rs
    ├── reload_dim_mismatch.rs
    ├── reload_during_concurrent_queries.rs
    └── reload_q5_first_call_warmup.rs

crates/helios-config/
└── src/watcher.rs               spawn_watcher(path) -> mpsc::Receiver<MemoryConfig>

crates/helios-cli/
└── src/bin/settings.rs          `helios settings memory {reload, show, validate}`
```

## 6. Helios config schema

```yaml
# helios.yaml (excerpt)
memory:
  store_url: "redis://moon:6379"

  embedder:
    mode: q4              # fp16 | q4 | ollama
    # Required when mode = fp16 or q4 — points to the model directory.
    # Lunaris cache convention: ~/.cache/lunaris/models/native/granite-r2/
    dir: /var/lib/helios/models/granite-r2
    # Required when mode = q4 — the GGUF file inside `dir`.
    gguf_filename: granite-r2-311m-Q4_K_M.gguf
    # Required when mode = ollama.
    ollama_url:    null
    ollama_model:  null

  reranker:
    mode: q5              # fp32 | q5 | off
    dir: /var/lib/helios/models/bge-reranker-v2-m3
    gguf_filename: bge-reranker-v2-m3-Q5_K_M-imatrix.gguf

  # auto | cpu | metal | cuda — `auto` (default) uses Lunaris's built-in
  # `select_device()` which auto-upgrades to CUDA → Metal based on compiled-in
  # features and runtime init. Explicit values force the choice.
  device: auto
```

### 6.1 Config Rust types

```rust
// crates/helios-memory/src/config.rs
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryConfig {
    pub store_url: String,
    pub embedder:  EmbedderConfig,
    pub reranker:  RerankerConfig,
    pub device:    DeviceConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum EmbedderConfig {
    Fp16   { dir: PathBuf },
    Q4     { dir: PathBuf, gguf_filename: String },
    Ollama { url: String, model: String, dim: usize },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum RerankerConfig {
    Fp32 { dir: PathBuf },
    Q5   { dir: PathBuf, gguf_filename: String },
    Off,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceConfig {
    /// Pass `Device::Cpu` to Lunaris; `lunaris_embed_native::device_select`
    /// will auto-upgrade to CUDA → Metal based on compiled-in features and
    /// runtime init success. Logged at INFO.
    #[default] Auto,
    /// Force CPU. Note: today this maps to `Device::Cpu` and will still be
    /// auto-upgraded by `select_device()`. Truly-CPU-only requires a Lunaris
    /// patch (~20 LOC) to disable the upgrade path.
    Cpu,
    /// Force Metal. Bypasses auto-detect; fails fast if Metal isn't available.
    Metal,
    /// Force CUDA. Bypasses auto-detect; fails fast if CUDA isn't available.
    Cuda,
}

impl MemoryConfig {
    pub fn validate(&self) -> Result<(), HeliosMemoryError> {
        // Verify file existence per mode; verify device compiled in;
        // verify Ollama URL parses as URL; etc.
        // Fail-fast BEFORE any swap.
    }

    pub fn from_file(path: &Path) -> Result<Self, HeliosMemoryError> { /* serde_yaml */ }
}
```

### 6.2 Disabled-by-default — Helios boots with Noop, operator enables at runtime

**Recommended posture for prod**: Helios starts with `NoopEmbedder` (768-d zero vectors) and `NoopReranker` (identity-rank). The handle is fully functional — `recall()` returns results, just useless ones — and the operator explicitly enables real models via config reload or CLI when ready.

**Why disabled-by-default**:
- Helios can boot without weights staged on the host (no fail-fast at startup).
- ~1.6 GiB RSS savings on idle instances.
- Air-gapped dev environments work out of the box.
- Cost-conscious: only pay model load when memory is actually being used.
- Operator explicitly opts in to the embedder cost — no surprise prod incidents from a missing weights file.

**Trade-off**: recall results are garbage until enabled. Helios should refuse memory-dependent requests (or warn-log) when in noop mode. Health endpoint must reflect this state.

#### Updated config types

```rust
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum EmbedderConfig {
    /// NoopEmbedder (768-d zeros). Default. Recall returns garbage results;
    /// memory engine is "disabled" from a quality standpoint but functional.
    #[default] Disabled,
    Fp16   { dir: PathBuf },
    Q4     { dir: PathBuf, gguf_filename: String },
    Ollama { url: String, model: String, dim: usize },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum RerankerConfig {
    /// NoopReranker (identity rank). Default. Recall path still works; just
    /// no rerank pass.
    #[default] Off,
    Fp32 { dir: PathBuf },
    Q5   { dir: PathBuf, gguf_filename: String },
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            store_url: "redis://localhost:6379".to_string(),
            embedder:  EmbedderConfig::default(),  // Disabled
            reranker:  RerankerConfig::default(),  // Off
            device:    DeviceConfig::default(),    // Auto
        }
    }
}
```

#### Minimal helios.yaml (everything defaulted)

```yaml
memory:
  store_url: "redis://moon:6379"
  # embedder, reranker, device all default — noop embedder, noop reranker, auto device
```

Helios starts in seconds, no model weights needed, no fail-fast. Health endpoint reports `memory.embedder.mode = "disabled"`.

#### Builder addition

```rust
async fn build_embedder(cfg: &EmbedderConfig, device: &Device)
    -> Result<Arc<dyn Embedder>, HeliosMemoryError>
{
    use lunaris_core::{NoopEmbedder, NOOP_DEFAULT_DIM};
    let cfg = cfg.clone();
    let device = device.clone();
    tokio::task::spawn_blocking(move || -> Result<Arc<dyn Embedder>, HeliosMemoryError> {
        Ok(match cfg {
            EmbedderConfig::Disabled => {
                tracing::warn!(target: "helios::memory", "embedder DISABLED — recall results will be garbage until enabled");
                Arc::new(NoopEmbedder::new(NOOP_DEFAULT_DIM))   // 768-d, matches granite-r2
            },
            EmbedderConfig::Fp16 { dir } => /* ... as before ... */,
            EmbedderConfig::Q4   { dir, gguf_filename } => /* ... */,
            EmbedderConfig::Ollama { url, model, dim } => /* ... */,
        })
    }).await?
}

async fn build_reranker(cfg: &RerankerConfig, device: &Device)
    -> Result<Arc<dyn Reranker>, HeliosMemoryError>
{
    use lunaris_rerank::NoopReranker;
    let cfg = cfg.clone();
    let device = device.clone();
    tokio::task::spawn_blocking(move || -> Result<Arc<dyn Reranker>, HeliosMemoryError> {
        Ok(match cfg {
            RerankerConfig::Off  => Arc::new(NoopReranker::default()),  // no warn — recall-only is supported
            RerankerConfig::Fp32 { dir } => /* ... as before ... */,
            RerankerConfig::Q5   { dir, gguf_filename } => /* ... */,
        })
    }).await?
}
```

**Critical dim contract**: `NOOP_DEFAULT_DIM = 768` (from `lunaris_core::NOOP_DEFAULT_DIM`) which matches granite-r2's 768-d. This means flipping `Disabled → Fp16/Q4/Ollama-768d` at runtime via `try_with_embedder` **does not fail the dim guardrail**. If you ever default to a non-768-d noop, you'll be unable to enable a real embedder without a full re-open.

### 6.3 Runtime enable guide — flipping from Disabled → real

There are three ways to enable the embedder/reranker at runtime. Use whichever fits your operational posture.

#### Method 1 — Edit `helios.yaml`, file-watcher picks it up

```yaml
# helios.yaml — operator edits in place
memory:
  store_url: "redis://moon:6379"
  embedder:
    mode: q4                                          # was: disabled
    dir: /var/lib/helios/models/granite-r2
    gguf_filename: granite-r2-311m-Q4_K_M.gguf
  reranker:
    mode: q5                                          # was: off
    dir: /var/lib/helios/models/bge-reranker-v2-m3
    gguf_filename: bge-reranker-v2-m3-Q5_K_M-imatrix.gguf
```

Save the file. The `notify-debouncer-mini` watcher (§10) picks it up within 500ms, validates the new config, builds the components off the hot path, and atomic-swaps. `tracing::info!` logs the reload outcome.

#### Method 2 — `helios settings memory enable` CLI

For ad-hoc enables without touching the config file (useful in incident response or one-off staging spins):

```bash
# Enable Q4 embedder
helios settings memory enable embedder \
  --mode q4 \
  --dir /var/lib/helios/models/granite-r2 \
  --gguf granite-r2-311m-Q4_K_M.gguf

# Enable Q5 reranker
helios settings memory enable reranker \
  --mode q5 \
  --dir /var/lib/helios/models/bge-reranker-v2-m3 \
  --gguf bge-reranker-v2-m3-Q5_K_M-imatrix.gguf

# Disable (go back to noop)
helios settings memory disable embedder
helios settings memory disable reranker
```

CLI handler:

```rust
// crates/helios-cli/src/bin/settings.rs (extended)
#[derive(Subcommand)]
enum MemoryCmd {
    Reload,
    Show,
    Validate { path: PathBuf },
    /// Enable embedder or reranker in the live config.
    Enable {
        component: Component,
        #[arg(long)] mode: String,
        #[arg(long)] dir:  Option<PathBuf>,
        #[arg(long)] gguf: Option<String>,
        #[arg(long)] url:  Option<String>,
        #[arg(long)] model: Option<String>,
        #[arg(long)] dim:   Option<usize>,
    },
    /// Disable component (revert to noop).
    Disable { component: Component },
}

#[derive(clap::ValueEnum, Clone)]
enum Component { Embedder, Reranker }
```

Daemon side: take current config, patch the relevant field, run the standard reload path. The CLI doesn't bypass validation.

#### Method 3 — Programmatic API (Helios admin endpoints, internal services)

```rust
use helios_memory::{Memory, EmbedderConfig, RerankerConfig};
use std::path::PathBuf;

// In an admin HTTP handler:
async fn enable_q4_embedder(memory: Arc<Memory>) -> Result<(), HeliosError> {
    let mut new_cfg = (*memory.current_config()).clone();
    new_cfg.embedder = EmbedderConfig::Q4 {
        dir: PathBuf::from("/var/lib/helios/models/granite-r2"),
        gguf_filename: "granite-r2-311m-Q4_K_M.gguf".to_string(),
    };
    reload::reload(&memory, new_cfg).await?;
    Ok(())
}
```

#### Reload-time behavior on first enable

| Step | Time | Notes |
|---|---|---|
| Config validation | ~µs | File-exist + URL-parse + device-compiled-in |
| Build new embedder (Q4, off hot path) | ~0.5-1.5 s | spawn_blocking; reader queries continue on noop handle |
| Build new reranker (Q5, off hot path) | ~0.3-0.8 s | OnceCell construction; GGUF NOT mmap'd yet (N-04 D1 lazy) |
| `try_with_embedder` dim check | ~ns | Noop 768d → real 768d → passes |
| Atomic swap (`ArcSwap::store`) | ~ns | In-flight queries finish on noop handle (returning garbage results from the last 0.5-1.5s); new queries see real handle |
| Background reranker warm-up | ~50-200 ms | Synthetic rerank call; mmaps the 446 MiB Q5 GGUF |

**During the swap window** (between starting `build_embedder` and `ArcSwap::store`), readers still get noop results. That's fine for `Disabled → enabled` because they were already getting noop. For `Q4 → Fp16` or `Fp32 → Q5` swaps in a working memory engine, you might see ~1 second of stale-but-functional results. Almost never observable.

#### Health endpoint integration

Helios's health/readiness probe should reflect memory state:

```json
GET /healthz
{
  "memory": {
    "embedder": { "mode": "disabled", "ready": false },
    "reranker": { "mode": "off",      "ready": true },
    "device":   "metal"
  }
}
```

When `embedder.mode = "disabled"`, return HTTP 503 from `/readyz` so traffic doesn't hit Helios until the operator has enabled it. `/livez` (process alive) stays 200.

#### Auto-enable on weights-detected (optional)

If you want Helios to auto-enable when the operator drops model weights into the configured cache dir, add a periodic check in the watcher loop:

```rust
async fn auto_enable_check(memory: &Memory) {
    let cfg = memory.current_config();
    if matches!(cfg.embedder, EmbedderConfig::Disabled) {
        let canonical_q4 = PathBuf::from("/var/lib/helios/models/granite-r2/granite-r2-311m-Q4_K_M.gguf");
        if canonical_q4.exists() {
            let mut new_cfg = (*cfg).clone();
            new_cfg.embedder = EmbedderConfig::Q4 {
                dir: PathBuf::from("/var/lib/helios/models/granite-r2"),
                gguf_filename: "granite-r2-311m-Q4_K_M.gguf".to_string(),
            };
            let _ = reload::reload(memory, new_cfg).await;
        }
    }
}
```

Run every 30s in a background task. Opt-in (config flag `memory.auto_enable_on_weights = true`). Useful for k8s pod replacement where weights mount via PVC after pod start.

## 7. Component builder

```rust
// crates/helios-memory/src/builder.rs
use std::sync::Arc;
use candle_core::Device;
use lunaris_core::embedder::Embedder;
use lunaris_rerank::Reranker;

pub async fn build_components(cfg: &MemoryConfig)
    -> Result<(Arc<dyn Embedder>, Arc<dyn Reranker>), HeliosMemoryError>
{
    // `Auto` and `Cpu` both pass Device::Cpu to Lunaris, which then runs
    // `select_device()` (auto-upgrade to CUDA → Metal based on compiled-in
    // features). `Metal`/`Cuda` bypass auto-detect — fail-fast if unavailable.
    let device = match cfg.device {
        DeviceConfig::Auto | DeviceConfig::Cpu => Device::Cpu,
        DeviceConfig::Metal                    => Device::new_metal(0)?,
        DeviceConfig::Cuda                     => Device::new_cuda(0)?,
    };

    let embedder = build_embedder(&cfg.embedder, &device).await?;
    let reranker = build_reranker(&cfg.reranker, &device).await?;

    Ok((embedder, reranker))
}

async fn build_embedder(cfg: &EmbedderConfig, device: &Device)
    -> Result<Arc<dyn Embedder>, HeliosMemoryError>
{
    // CPU-heavy work — wrap in spawn_blocking so we don't stall the runtime.
    let cfg = cfg.clone();
    let device = device.clone();
    tokio::task::spawn_blocking(move || -> Result<Arc<dyn Embedder>, HeliosMemoryError> {
        Ok(match cfg {
            EmbedderConfig::Fp16 { dir } => Arc::new(NativeEmbedder::open(NativeEmbedderOpts {
                weights_path:   dir.join("model.safetensors"),
                tokenizer_path: dir.join("tokenizer.json"),
                config_path:    dir.join("config.json"),
                device,
            })?),
            EmbedderConfig::Q4 { dir, gguf_filename } => {
                Arc::new(NativeQuantizedEmbedder::open(NativeQuantizedEmbedderOpts {
                    gguf_path:      dir.join(gguf_filename),
                    tokenizer_path: dir.join("tokenizer.json"),
                    config_path:    dir.join("config.json"),
                    device,
                })?)
            },
            EmbedderConfig::Ollama { url, model, dim } => {
                Arc::new(OllamaEmbedder::new(OllamaEmbedderOpts { endpoint: url, model, dim })?)
            },
        })
    }).await?
}

async fn build_reranker(cfg: &RerankerConfig, device: &Device)
    -> Result<Arc<dyn Reranker>, HeliosMemoryError>
{
    let cfg = cfg.clone();
    let device = device.clone();
    tokio::task::spawn_blocking(move || -> Result<Arc<dyn Reranker>, HeliosMemoryError> {
        Ok(match cfg {
            RerankerConfig::Fp32 { dir } => Arc::new(NativeReranker::open(NativeRerankerOpts {
                weights_path:   dir.join("model.safetensors"),
                tokenizer_path: dir.join("tokenizer.json"),
                config_path:    dir.join("config.json"),
                device,
            })?),
            RerankerConfig::Q5 { dir, gguf_filename } => {
                Arc::new(NativeQuantizedReranker::open(NativeQuantizedRerankerOpts {
                    gguf_path:      dir.join(gguf_filename),
                    tokenizer_path: dir.join("tokenizer.json"),
                    config_path:    dir.join("config.json"),
                    device,
                })?)
            },
            RerankerConfig::Off => Arc::new(NoopReranker::default()),
        })
    }).await?
}
```

## 8. Memory handle with `ArcSwap`

```rust
// crates/helios-memory/src/lib.rs
use arc_swap::ArcSwap;
use std::sync::Arc;
use lunaris::Lunaris;

pub struct Memory {
    handle:      Arc<ArcSwap<Lunaris>>,
    current_cfg: Arc<ArcSwap<MemoryConfig>>,
}

impl Memory {
    pub async fn open(cfg: MemoryConfig) -> Result<Self, HeliosMemoryError> {
        cfg.validate()?;
        let (embedder, reranker) = builder::build_components(&cfg).await?;
        let lunaris = Lunaris::open_with_embedder(&cfg.store_url, embedder).await?
            .with_reranker(reranker);
        Ok(Self {
            handle:      Arc::new(ArcSwap::from_pointee(lunaris)),
            current_cfg: Arc::new(ArcSwap::from_pointee(cfg)),
        })
    }

    /// Lockless read for query handlers.
    pub fn load(&self) -> Arc<Lunaris> {
        self.handle.load_full()
    }

    pub fn current_config(&self) -> Arc<MemoryConfig> {
        self.current_cfg.load_full()
    }
}
```

**Query handler usage:**

```rust
async fn handle_query(memory: &Memory, query: &str) -> Result<Vec<Hit>, HeliosError> {
    let lunaris = memory.load();           // Arc::clone, ~1ns
    lunaris.recall(query).await            // reader sees a consistent handle for the whole call
}
```

## 9. Hot-reload

```rust
// crates/helios-memory/src/reload.rs
pub struct ReloadOutcome {
    pub swapped_embedder: bool,
    pub swapped_reranker: bool,
    pub swapped_device:   bool,
    pub took_ms:          u128,
}

pub async fn reload(memory: &Memory, new_cfg: MemoryConfig)
    -> Result<ReloadOutcome, HeliosMemoryError>
{
    let started = std::time::Instant::now();

    // 1. Validate new config BEFORE any mutation.
    new_cfg.validate()?;

    let current = memory.current_config();
    let diff = ConfigDiff::compute(&current, &new_cfg);

    if diff.is_empty() {
        return Ok(ReloadOutcome::noop(started.elapsed().as_millis()));
    }

    // 2. Build new components OFF the hot path (spawn_blocking inside).
    //    If the build fails (bad path, dim mismatch, etc.), we abort here
    //    and the live handle is untouched.
    let (new_embedder, new_reranker) = builder::build_components(&new_cfg).await?;

    // 3. Construct the new Lunaris handle.
    //    `open_with_embedder` reopens the store too; if store_url changed,
    //    this picks up the new URL. If only embedder/reranker changed,
    //    use try_with_embedder + with_reranker on the existing handle for
    //    less churn (see §9.1).
    let new_lunaris = if diff.store_url_changed {
        Lunaris::open_with_embedder(&new_cfg.store_url, new_embedder).await?
            .with_reranker(new_reranker)
    } else {
        // Take a snapshot of the current handle (Arc::clone is fine — Lunaris
        // is internally Arc'd through Inner). Rebuild via the builder methods.
        let current_lunaris: Arc<Lunaris> = memory.load();
        // Lunaris isn't Clone, but we can keep the storage backing by
        // re-opening with the same URL — semantically idempotent and avoids
        // the borrow-checker dance of consuming an Arc<Lunaris>.
        Lunaris::open_with_embedder(&current.store_url, new_embedder).await?
            .with_reranker(new_reranker)
    };

    // 4. Atomic swap. Readers in flight finish on the old handle.
    memory.handle.store(Arc::new(new_lunaris));
    memory.current_cfg.store(Arc::new(new_cfg));

    // 5. Best-effort warm-up for Q5 reranker (avoids latency spike on
    //    the first real rerank call, which would mmap 446 MiB).
    if diff.reranker_changed && matches!(new_cfg.reranker, RerankerConfig::Q5 { .. }) {
        tokio::spawn(warmup_reranker(memory.load()));
    }

    Ok(ReloadOutcome {
        swapped_embedder: diff.embedder_changed,
        swapped_reranker: diff.reranker_changed,
        swapped_device:   diff.device_changed,
        took_ms:          started.elapsed().as_millis(),
    })
}

async fn warmup_reranker(lunaris: Arc<Lunaris>) {
    // Fire a synthetic rerank to materialize the lazy OnceCell.
    let _ = lunaris.reranker().rerank(
        "warmup", vec![/* one synthetic candidate */]
    ).await;
}
```

### 9.1 Per-component swap (alternative — when store_url unchanged)

If you want to avoid re-opening the store (saves a Redis/Postgres reconnect + reuses extractor/verifier/consolidator state), use the per-component swap path. This requires Lunaris exposing the per-field setters publicly, which it does as builder methods that consume `self`. Trick: build a fresh `Lunaris` via `open_with_embedder`, then chain `.with_reranker`. Don't try to mutate the existing `Arc<Lunaris>` in place — that fights the type system.

Alternative pattern for the truly latency-sensitive path: wrap the embedder + reranker in your OWN `ArcSwap`-based proxy `Embedder` / `Reranker` impl. Then swap inside the proxy without touching the Lunaris handle at all. ~50 LOC; recommended only if reload latency is profiled as a problem.

```rust
// Sketch — only build this if profiling demands it.
pub struct SwappableEmbedder { inner: Arc<ArcSwap<Arc<dyn Embedder>>> }
#[async_trait::async_trait]
impl Embedder for SwappableEmbedder {
    fn dim(&self) -> usize { self.inner.load().dim() }
    async fn embed_batch(&self, batch: &[&str]) -> Vec<Vec<f32>> {
        self.inner.load().embed_batch(batch).await
    }
}
```

## 10. Config file watcher

```rust
// crates/helios-config/src/watcher.rs
use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

pub fn spawn_watcher(path: PathBuf) -> mpsc::Receiver<MemoryConfig> {
    let (tx, rx) = mpsc::channel(8);

    tokio::task::spawn_blocking(move || {
        let (debounce_tx, debounce_rx) = std::sync::mpsc::channel();
        let mut debouncer = new_debouncer(Duration::from_millis(500), debounce_tx).unwrap();
        debouncer.watcher().watch(&path, RecursiveMode::NonRecursive).unwrap();

        for events in debounce_rx {
            if events.is_err() { continue; }
            match MemoryConfig::from_file(&path) {
                Ok(cfg) => {
                    if tx.blocking_send(cfg).is_err() { break; }
                },
                Err(e) => tracing::error!(?e, ?path, "config parse failed; keeping old config"),
            }
        }
    });

    rx
}
```

**Helios main loop:**

```rust
let memory = Memory::open(initial_cfg).await?;
let mut config_rx = helios_config::watcher::spawn_watcher(PathBuf::from("helios.yaml"));

tokio::spawn({
    let memory = memory.clone();   // Memory contains only Arcs
    async move {
        while let Some(new_cfg) = config_rx.recv().await {
            match reload::reload(&memory, new_cfg).await {
                Ok(outcome) => tracing::info!(?outcome, "memory reload OK"),
                Err(e)      => tracing::error!(?e,      "memory reload FAILED; keeping old"),
            }
        }
    }
});
```

## 11. `helios settings` CLI

```rust
// crates/helios-cli/src/bin/settings.rs
use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Cli {
    /// Helios admin socket. Default: $XDG_RUNTIME_DIR/helios.sock
    #[arg(long)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Reload memory backend from disk config
    MemoryReload,
    /// Print current memory config (live)
    MemoryShow,
    /// Validate a config file without applying it
    MemoryValidate { path: PathBuf },
}

// Talks to the daemon via Unix socket. Daemon side dispatches MemoryReload to
// reload::reload(&memory, MemoryConfig::from_file(...)).
```

Recommended IPC: Unix socket + `tokio::net::UnixStream` + a tiny JSON-RPC. Don't shell out to HTTP for this — Unix socket has tighter ACL story for an admin command.

## 12. Edge cases

| Case | Mitigation |
|---|---|
| **Dim mismatch on embedder swap** | `try_with_embedder` returns `Err(StorageError::Backend("embedder dim N != store dim M; ..."))`. Helios should: (a) keep the old handle (no swap occurred), (b) emit metric `helios_memory_reload_failed_total{reason="dim_mismatch"}`, (c) surface in `helios settings memory show`. |
| **Q5 reranker first-call lazy mmap** | Spawn the `warmup_reranker` task post-swap (§9 step 5). Measure p99 latency for 30s after reload to confirm no spike. |
| **In-flight queries during swap** | `ArcSwap::load_full()` returns an `Arc` snapshot; queries hold that snapshot for their whole duration. The swap doesn't tear state. New queries see the new handle. |
| **Config validation failure** | Validate BEFORE building components. Builder failures (file not found, bad GGUF SHA) abort cleanly without touching the live handle. |
| **Ollama URL unreachable** | `OllamaEmbedder::new` does NOT ping the endpoint — failures surface at first `embed_batch` call. Add a Helios-side health probe: post-build, call `embed_batch(&["health probe"])` once. If it fails, abort the swap. |
| **Device hot-swap (CPU↔Metal↔CUDA)** | Heavy — requires re-loading weights onto the new device. Supported, but pay the full open cost (~1-3 s for FP16 weights). Document this in operator runbook so they know reload-with-device-change is slower. |
| **Reload during another reload** | Wrap `reload::reload` in a `tokio::sync::Mutex<()>` so concurrent reloads serialize. The mutex is held only for the duration of `reload` itself — readers are never blocked. |
| **GGUF file missing on switch to Q4/Q5** | Builder fails at file-open. Keep old handle. Emit alert. |

## 13. Test plan (red/green TDD)

```rust
#[tokio::test]
async fn reload_fp16_to_q4_succeeds() {
    let memory = open_with_fp16().await;
    let q4_cfg = config_with_q4();
    let outcome = reload(&memory, q4_cfg).await.unwrap();
    assert!(outcome.swapped_embedder);
    assert_recall_still_works(&memory).await;
}

#[tokio::test]
async fn reload_dim_mismatch_keeps_old_handle() {
    let memory = open_with_fp16().await;  // 768-d
    let bad_cfg = config_with_synthetic_512d_embedder();
    let err = reload(&memory, bad_cfg).await.unwrap_err();
    assert!(matches!(err, HeliosMemoryError::DimMismatch { .. }));
    assert_recall_still_works(&memory).await;
}

#[tokio::test]
async fn reload_during_concurrent_queries_no_torn_state() {
    let memory = open_with_fp16().await;
    let handles: Vec<_> = (0..100).map(|_| {
        let m = memory.clone();
        tokio::spawn(async move { m.load().recall("test").await })
    }).collect();

    // Reload mid-flight
    reload(&memory, config_with_q4()).await.unwrap();

    // All 100 queries finish without panic.
    for h in handles { h.await.unwrap().unwrap(); }
}

#[tokio::test]
async fn reload_q5_first_call_warmup() {
    let memory = open_with_fp32().await;
    reload(&memory, config_with_q5()).await.unwrap();

    // Warmup task should have spawned. Wait for it.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // First user-issued rerank should now be fast — no lazy-mmap spike.
    let t0 = Instant::now();
    let _ = memory.load().recall_with_rerank("test").await;
    assert!(t0.elapsed() < Duration::from_millis(100));
}
```

## 14. Deployment recipes per environment

### 14.1 Local dev (Apple Silicon)
```yaml
memory:
  store_url: "redis://localhost:6379"
  embedder: { mode: fp16, dir: ~/.cache/lunaris/models/native/granite-r2 }
  reranker: { mode: fp32, dir: ~/.cache/lunaris/models/native/bge-reranker-v2-m3 }
  device:   metal
```
Stage weights once: `cargo run -p lunaris-bench --bin stage-models`.

### 14.2 Staging (Apple Silicon, low-RSS)
```yaml
memory:
  embedder: { mode: q4, dir: ..., gguf_filename: granite-r2-311m-Q4_K_M.gguf }
  reranker: { mode: q5, dir: ..., gguf_filename: bge-reranker-v2-m3-Q5_K_M-imatrix.gguf }
  device:   metal
```
~1.6 GiB total RSS after first rerank. Trades 40-60ms latency for memory.

### 14.3 Prod (CUDA Linux)
```yaml
memory:
  embedder: { mode: fp16, dir: /var/lib/helios/models/granite-r2 }
  reranker: { mode: fp32, dir: /var/lib/helios/models/bge-reranker-v2-m3 }
  device:   cuda
```
Maximum perf. Embed p50 ≤ 3 ms, rerank p50 ≤ 25 ms (per O-01 gate).

### 14.4 Air-gap (no GPU, can't host weights)
```yaml
memory:
  embedder: { mode: ollama, url: "http://internal-ollama:11434", model: "nomic-embed-text", dim: 768 }
  reranker: { mode: off }
  device:   cpu
```
Routes embed through your existing Ollama deployment; runs recall-only (no rerank). `tracing::warn!` logs this is the unsupported path.

### 14.5 Canary rollout
Run two Helios instances. A: current config. B: new config. Load-balance 5% → 25% → 100% to B as metrics confirm equivalence. Lunaris is stateless across modes — no schema migration; the store sees the same 768-d vectors regardless of embedder mode (granite-r2 FP16 and Q4 produce numerically-equivalent vectors per N-01.5 drift gate).

## 15. Operational checklist

### 15.1 Before first reload in prod
- [ ] Confirm binary compiled with all needed features: `cargo tree -p helios-memory --features metal,cuda,cpu-mkl 2>/dev/null` should not error.
- [ ] Stage weights: run `lunaris stage-models --cache-dir /var/lib/helios/models/native`.
- [ ] Verify SHA-256 of GGUF artifacts matches the canonical values:
  - granite-r2-311m-Q4_K_M.gguf: `0768a38b0bc9900e89bb15ae0b6ea2ca7db130759e0eca226119610aedf5e276`
  - bge-reranker-v2-m3-Q5_K_M-imatrix.gguf: `6cdcc566200dba69553a89a9d59ff6d631e33969bc9367eff6914919f7722a1c`
- [ ] Run `helios settings memory validate <new_config>` first.

### 15.2 Smoke test post-reload
- [ ] `helios settings memory show` returns the new mode
- [ ] `helios admin recall --query "smoke test"` returns ≥1 hit
- [ ] p99 latency stays under gate for 60s (check Prometheus / equivalent)
- [ ] `helios_memory_reload_failed_total` did not increment

### 15.3 Rollback
- [ ] Edit `helios.yaml` back to previous mode
- [ ] Watcher picks it up → reload OK
- [ ] If watcher is broken: `helios settings memory reload --path <old_helios.yaml>`
- [ ] In extremis: `kill -HUP $pid` (if SIGHUP handler installed) — currently not in scope, add only if you need it

## 16. Metrics to wire

```
helios_memory_reload_attempts_total{result="ok|failed"}
helios_memory_reload_failed_total{reason="dim_mismatch|file_missing|build_failed|other"}
helios_memory_reload_duration_seconds
helios_memory_current_mode{component="embedder|reranker", mode="..."}
helios_memory_first_rerank_latency_seconds_post_reload     # detect missing warmup
```

## 17. Carry from Lunaris v0.4 (don't re-discover)

- **Lazy reranker (N-04 D1)**: the Q5 reranker GGUF mmap defers to first `rerank()` call. Plan for the spike in `rerank` latency post-swap OR call the warmup helper.
- **Dim guardrail (N-04 D2)**: `try_with_embedder` fails hard on mismatch. `with_embedder` (infallible) only `tracing::warn!`s. **Always prefer the fallible variant in Helios.**
- **No mid-process Cargo feature flips**: compile every mode you need to toggle.
- **MLX backend** is a v0.5+ followup (O-02 spike GREEN at 8.74× perf but port deferred). Don't plan around it for v0.4.
- **CUDA / aarch64 perf gates** are TBD until O-03 self-hosted runners are provisioned. Apple Silicon (Metal) is the most validated path.

## 18. References

- Lunaris v0.4 migration: `docs/migration/0.3-to-0.4-native-default.md`
- Native-default `Lunaris::open()` rustdoc: `crates/lunaris/src/handle.rs:155`
- N-04 lazy reranker SUMMARY: `.planning/phases/N-04-tightening/SUMMARY.md`
- N-03 cutover SUMMARY: `.planning/phases/N-03-cutover/SUMMARY.md`
- Stage models CLI: `crates/lunaris-bench/src/bin/stage_models.rs`
- Hardware perf gates: `.planning/milestones/v0.4-NATIVE-SINGLE-BACKEND/HARDWARE-OPTIMIZATION-ROADMAP.md`
- Perf-gate CI workflow: `.github/workflows/perf-gates.yml`

## 19. Open questions for Helios team

1. **Where does Helios source its `helios.yaml` from?** Local file, ConfigMap, Vault? Determines watcher implementation.
2. **Does Helios already have an admin socket / RPC convention?** If yes, plug the `MemoryReload` command into it. If no, recommend Unix socket + JSON-RPC.
3. **Per-tenant memory mode?** Currently the design is single-tenant per process. If you need per-request mode selection, that's a different design — talk to Lunaris team before building it.
4. **Reload SLA?** Current design: ~1-3 seconds for FP16 swap, ~0.5 seconds for Q4. If <100ms required, you'd need the proxy pattern from §9.1.
