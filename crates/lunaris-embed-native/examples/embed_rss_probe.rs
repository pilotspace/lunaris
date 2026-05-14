//! `embed_rss_probe` — single-purpose binary that loads either the FP16 or
//! Q4 embedder, runs 50 embeds, and exits. Exists so `tests/peak_rss.rs` can
//! wrap it under `/usr/bin/time -l` and compare peak resident set size
//! between paths in **separate processes** (running both in one process
//! double-counts because the FP16 + Q4 weights stay resident simultaneously).
//!
//! Build with the `embedder-gguf` feature when probing the Q4 path:
//!
//! ```bash
//! cargo build --release -p lunaris-embed-native \
//!     --features embedder-gguf --example embed_rss_probe
//! ```
//!
//! Usage:
//!
//! ```bash
//! ./target/release/examples/embed_rss_probe fp16
//! ./target/release/examples/embed_rss_probe q4
//! ```
//!
//! Env vars (mirror the integration test convention):
//!   GRANITE_R2_WEIGHTS_PATH    (fp16 mode)
//!   GRANITE_R2_TOKENIZER_PATH
//!   GRANITE_R2_CONFIG_PATH
//!   GRANITE_R2_GGUF_PATH       (q4 mode)

use std::path::PathBuf;

use candle_core::Device;
use lunaris_core::Embedder;
use lunaris_embed_native::{NativeEmbedder, NativeEmbedderOpts};

#[cfg(feature = "embedder-gguf")]
use lunaris_embed_native::{NativeQuantizedEmbedder, NativeQuantizedEmbedderOpts};

fn env_path(name: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("env var {name} is required"))
}

// 50 diverse prompts — keep simple; we only care about peak RSS, not quality.
const PROMPTS: &[&str] = &[
    "The quick brown fox jumps over the lazy dog.",
    "Trí tuệ nhân tạo đang thay đổi cách chúng ta làm việc.",
    "fn main() { println!(\"hello\"); }",
    "The mitochondria is the powerhouse of the cell.",
    "SELECT * FROM users WHERE id = 42;",
    "François a écrit un livre sur la cuisine française.",
    "Quantum entanglement defies classical intuition.",
    "Lập trình viên thường uống cà phê đen.",
    "import numpy as np\nx = np.zeros(10)",
    "東京の朝ご飯はとても美味しいです。",
    "The Roman Empire fell in 476 AD.",
    "let x = 5; let y = x + 1;",
    "Khoa học dữ liệu cần kiến thức toán học.",
    "She sells seashells by the seashore.",
    "git commit -m 'fix typo'",
    "Photosynthesis converts light to chemical energy.",
    "Hà Nội là thủ đô của Việt Nam.",
    "for i in range(10): print(i)",
    "Le chat est sur le tapis.",
    "Distributed systems are hard to reason about.",
    "Async Rust avoids data races at compile time.",
    "Người Việt thường ăn phở vào buổi sáng.",
    "function add(a, b) { return a + b; }",
    "Mary had a little lamb.",
    "TCP is reliable; UDP is fast.",
    "Cây cà phê trồng ở Tây Nguyên.",
    "use std::collections::HashMap;",
    "El sol brilla en el cielo azul.",
    "Cryptographic hashes are one-way functions.",
    "Bún chả là món đặc sản của Hà Nội.",
    "pub struct User { name: String }",
    "How does a transformer model work?",
    "Mì Quảng có nước dùng sệt và ít.",
    "println!(\"{}\", 1 + 2);",
    "Birds chirp at dawn.",
    "Lock-free data structures use atomic CAS.",
    "Trà đá miễn phí ở các quán Việt Nam.",
    "async fn fetch_data() -> Result<()> { Ok(()) }",
    "The ocean is full of mysteries.",
    "Vector databases enable semantic search.",
    "Cơm tấm Sài Gòn ngon nhất.",
    "let v: Vec<i32> = vec![1, 2, 3];",
    "Mountains were folded by tectonic forces.",
    "Java has a JIT compiler; Rust has AOT.",
    "Bánh mì kẹp thịt là fast food Việt.",
    "tokio::spawn(async move { /* ... */ });",
    "Light travels at 299 792 458 m/s in vacuum.",
    "Chè ba màu là món tráng miệng phổ biến.",
    "macro_rules! my_macro { () => { 42 }; }",
    "DNA stores information in nucleotide sequences.",
];

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "fp16".to_string());
    let tokenizer = env_path("GRANITE_R2_TOKENIZER_PATH");
    let config = env_path("GRANITE_R2_CONFIG_PATH");

    match mode.as_str() {
        "fp16" => {
            let weights = env_path("GRANITE_R2_WEIGHTS_PATH");
            let e = NativeEmbedder::open(NativeEmbedderOpts {
                weights_path: weights,
                tokenizer_path: tokenizer,
                config_path: config,
                device: Device::Cpu,
            })
            .expect("open fp16");
            for chunk in PROMPTS.chunks(8) {
                let _ = e.embed_batch(chunk).await.expect("fp16 embed");
            }
            eprintln!("[probe] fp16 done — {} prompts", PROMPTS.len());
        }
        "q4" => {
            #[cfg(feature = "embedder-gguf")]
            {
                let gguf = env_path("GRANITE_R2_GGUF_PATH");
                let e = NativeQuantizedEmbedder::open(NativeQuantizedEmbedderOpts {
                    gguf_path: gguf,
                    tokenizer_path: tokenizer,
                    config_path: config,
                    device: Device::Cpu,
                })
                .expect("open q4");
                for chunk in PROMPTS.chunks(8) {
                    let _ = e.embed_batch(chunk).await.expect("q4 embed");
                }
                eprintln!("[probe] q4 done — {} prompts", PROMPTS.len());
            }
            #[cfg(not(feature = "embedder-gguf"))]
            {
                panic!("q4 mode requires --features embedder-gguf");
            }
        }
        other => panic!("unknown mode: {other:?} (use fp16 | q4)"),
    }
}
