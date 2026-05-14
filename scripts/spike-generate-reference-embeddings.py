#!/usr/bin/env python3
"""Generate the reference-embedding fixture for `lunaris-embed-native`.

Output: `crates/lunaris-embed-native/tests/fixtures/reference_embeddings.json`

The fixture is the oracle for the numerical-equivalence integration test.
It contains 100 prompts (35 EN + 35 VI + 30 code) plus their 768-d
sentence-transformers embeddings, produced via the model's standard
`encode(.., normalize_embeddings=True)` pipeline which executes the
`modules.json` sequence (Transformer -> CLS-Pooling -> L2 Normalize).

Requirements (already present in this dev environment as of 2026-05-14):

    pip install "sentence-transformers>=5.0" "huggingface_hub>=1.0" "torch>=2.4"

Cache directory: `~/.cache/lunaris/spike/granite-r2/`.

Reproducibility hooks:
- prompts are sorted by SHA-256 hash so re-runs produce a stable row order.
- model revision SHA is captured at runtime and embedded in the fixture.
- `torch.set_num_threads(1)` + `torch.use_deterministic_algorithms(True,
  warn_only=True)` make the floating-point path as deterministic as PyTorch
  allows on CPU.
"""
from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path

import torch
from huggingface_hub import HfApi, snapshot_download
from sentence_transformers import SentenceTransformer

MODEL_ID = "ibm-granite/granite-embedding-311m-multilingual-r2"
CACHE_DIR = Path.home() / ".cache" / "lunaris" / "spike" / "granite-r2"
REPO_ROOT = Path(__file__).resolve().parent.parent
FIXTURE_PATH = (
    REPO_ROOT
    / "crates"
    / "lunaris-embed-native"
    / "tests"
    / "fixtures"
    / "reference_embeddings.json"
)


# ---- Prompt panel ---------------------------------------------------------
# Hand-curated, intentionally diverse. Counts per the spec:
# 35 EN + 35 VI + 30 code = 100.
EN_PROMPTS = [
    "The cat sat on the mat.",
    "Lunaris is a production-grade agent memory engine.",
    "Quantum entanglement enables faster-than-classical correlations.",
    "Neural networks approximate functions through layered transformations.",
    "Photosynthesis converts sunlight into chemical energy.",
    "The Roman Empire fell in 476 AD when the last emperor was deposed.",
    "Rust's borrow checker enforces memory safety at compile time.",
    "A good cup of coffee starts with freshly roasted beans.",
    "The Pacific Ocean covers more than 30 percent of Earth's surface.",
    "Climate change is driven primarily by human greenhouse gas emissions.",
    "Shakespeare wrote 39 plays and 154 sonnets during his lifetime.",
    "The mitochondrion is the powerhouse of the cell.",
    "Distributed consensus requires either availability or strict consistency.",
    "Vector databases enable semantic search over high-dimensional embeddings.",
    "Mount Everest stands 8,848 meters above sea level.",
    "The honey bee communicates the location of food via the waggle dance.",
    "Cryptography secures communications against adversaries.",
    "Jazz emerged in New Orleans in the early twentieth century.",
    "Sleep deprivation impairs both memory and emotional regulation.",
    "The speed of light in vacuum is approximately 299,792 kilometers per second.",
    "Renewable energy now accounts for a growing share of global electricity.",
    "Penicillin was discovered by Alexander Fleming in 1928.",
    "Recursion solves problems by reducing them to smaller instances.",
    "The Amazon rainforest is home to one in ten known species on Earth.",
    "Tea cultivation began in ancient China over four thousand years ago.",
    "Containerization revolutionized how software is shipped and operated.",
    "The Mona Lisa hangs in the Louvre and is viewed by millions yearly.",
    "Plate tectonics explains the slow movement of Earth's lithosphere.",
    "Open-source software thrives on collaboration and transparency.",
    "The Great Wall of China stretches over thirteen thousand miles.",
    "Dolphins use echolocation to navigate murky waters.",
    "Microservices trade operational complexity for deployment independence.",
    "The Eiffel Tower was built for the 1889 World's Fair in Paris.",
    "Antibodies are Y-shaped proteins produced by the immune system.",
    "A symphony orchestra typically includes strings, brass, woodwinds, and percussion.",
]

VI_PROMPTS = [
    "Hà Nội là thủ đô của Việt Nam.",
    "Trí tuệ nhân tạo đang thay đổi cách chúng ta làm việc.",
    "Phở là món ăn nổi tiếng nhất của ẩm thực Việt Nam.",
    "Cà phê sữa đá là thức uống được nhiều người yêu thích.",
    "Vịnh Hạ Long được UNESCO công nhận là di sản thiên nhiên thế giới.",
    "Tết Nguyên Đán là dịp lễ quan trọng nhất trong năm.",
    "Học lập trình giúp tư duy logic và giải quyết vấn đề tốt hơn.",
    "Mưa phùn tháng ba làm mọi thứ ẩm ướt và mát mẻ.",
    "Sông Hương chảy qua thành phố Huế thơ mộng.",
    "Áo dài là trang phục truyền thống của phụ nữ Việt Nam.",
    "Việc đọc sách mở rộng tri thức và làm giàu tâm hồn.",
    "Đồng bằng sông Cửu Long là vựa lúa lớn nhất cả nước.",
    "Bún chả Hà Nội nổi tiếng với nước mắm chua ngọt.",
    "Trẻ em cần được học tập trong môi trường an toàn và lành mạnh.",
    "Mỗi quốc gia có nền văn hóa riêng đáng được tôn trọng.",
    "Cây tre tượng trưng cho sức sống bền bỉ của người Việt.",
    "Đường phố Sài Gòn lúc nào cũng nhộn nhịp và đông đúc.",
    "Internet kết nối hàng tỷ người trên khắp hành tinh.",
    "Sức khỏe là tài sản quý giá nhất của mỗi con người.",
    "Bảo vệ môi trường là trách nhiệm chung của toàn xã hội.",
    "Mặt trời mọc ở phía đông và lặn ở phía tây.",
    "Thiền định giúp tinh thần thư giãn và minh mẫn hơn.",
    "Lễ hội đua thuyền truyền thống thường diễn ra vào mùa xuân.",
    "Học ngoại ngữ mở ra nhiều cơ hội nghề nghiệp mới.",
    "Hồ Gươm nằm ở trung tâm Hà Nội, là biểu tượng của thủ đô.",
    "Khoa học và công nghệ là động lực phát triển của quốc gia.",
    "Bánh chưng và bánh tét không thể thiếu trong ngày Tết cổ truyền.",
    "Tre, trúc, nứa là những loại cây gắn liền với làng quê Việt.",
    "Đại học là nơi đào tạo nguồn nhân lực chất lượng cao cho đất nước.",
    "Mỗi mùa xuân đến, hoa đào hoa mai nở rộ khắp nơi.",
    "Tình yêu thương gia đình là nền tảng của hạnh phúc.",
    "Bóng đá là môn thể thao được yêu thích nhất ở Việt Nam.",
    "Làng nghề truyền thống lưu giữ tinh hoa văn hóa dân tộc.",
    "Mạng xã hội kết nối nhưng cũng tiềm ẩn nhiều rủi ro.",
    "Đọc sách mỗi ngày là thói quen rất tốt cho mọi lứa tuổi.",
]

CODE_PROMPTS = [
    "fn fibonacci(n: u64) -> u64 { if n < 2 { n } else { fibonacci(n-1) + fibonacci(n-2) } }",
    "def quicksort(xs):\n    if len(xs) <= 1: return xs\n    p = xs[0]\n    return quicksort([x for x in xs[1:] if x < p]) + [p] + quicksort([x for x in xs[1:] if x >= p])",
    "SELECT user_id, COUNT(*) AS cnt FROM events WHERE created_at > NOW() - INTERVAL '7 days' GROUP BY user_id HAVING COUNT(*) > 10;",
    "const debounce = (fn, ms) => { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; };",
    "impl<T: Clone> Stack<T> { pub fn push(&mut self, v: T) { self.inner.push(v); } pub fn pop(&mut self) -> Option<T> { self.inner.pop() } }",
    "async fn fetch_user(id: u64) -> anyhow::Result<User> { let resp = reqwest::get(format!(\"/users/{id}\")).await?; Ok(resp.json().await?) }",
    "with open('data.txt') as f: lines = [line.strip() for line in f if not line.startswith('#')]",
    "CREATE TABLE orders (id BIGSERIAL PRIMARY KEY, user_id BIGINT NOT NULL, total NUMERIC(12,2), created_at TIMESTAMPTZ DEFAULT NOW());",
    "type Result<T, E = Error> = std::result::Result<T, E>; #[derive(thiserror::Error, Debug)] pub enum Error { #[error(\"oops\")] Oops }",
    "for i, line in enumerate(open('log.txt')):\n    if 'ERROR' in line:\n        print(f'{i}: {line.strip()}')",
    "function memoize(fn) { const cache = new Map(); return (...args) => { const k = JSON.stringify(args); if (!cache.has(k)) cache.set(k, fn(...args)); return cache.get(k); }; }",
    "let xs: Vec<_> = (1..=100).filter(|x| x % 3 == 0 || x % 5 == 0).collect(); let sum: u64 = xs.iter().sum();",
    "import numpy as np\narr = np.linspace(0, 1, 1000)\nresult = np.sin(arr * 2 * np.pi) ** 2",
    "WITH ranked AS (SELECT *, ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY ts DESC) AS rn FROM events) SELECT * FROM ranked WHERE rn = 1;",
    "use tokio::sync::mpsc; let (tx, mut rx) = mpsc::channel::<String>(16); tokio::spawn(async move { while let Some(m) = rx.recv().await { println!(\"{m}\"); } });",
    "class LRUCache:\n    def __init__(self, capacity: int):\n        self.capacity = capacity\n        self.cache = OrderedDict()",
    "git rev-list --count HEAD && git log --oneline -10 && git status --porcelain",
    "match value { Some(x) if x > 0 => println!(\"pos\"), Some(0) => println!(\"zero\"), Some(_) => println!(\"neg\"), None => println!(\"nada\") }",
    "interface User { id: number; name: string; email?: string; createdAt: Date; }",
    "ALTER TABLE users ADD COLUMN last_seen TIMESTAMPTZ; CREATE INDEX idx_users_last_seen ON users (last_seen DESC);",
    "let mut sum = 0u64; for i in 1..=1_000_000 { if i.is_multiple_of(7) { sum += i; } }",
    "data class Point(val x: Double, val y: Double) { fun distance(o: Point) = sqrt((x-o.x).pow(2) + (y-o.y).pow(2)) }",
    "#[derive(serde::Serialize, serde::Deserialize)] pub struct Episode { pub id: ulid::Ulid, pub scope: String, pub text: String }",
    "echo 'export PATH=$HOME/.cargo/bin:$PATH' >> ~/.zshrc && source ~/.zshrc && cargo --version",
    "func handler(w http.ResponseWriter, r *http.Request) { ctx := r.Context(); user, err := auth.From(ctx); if err != nil { http.Error(w, err.Error(), 401); return } }",
    "trait Embedder: Send + Sync { async fn embed_batch(&self, inputs: &[&str]) -> Result<Vec<Vec<f32>>>; fn dim(&self) -> usize; }",
    "useEffect(() => { const id = setInterval(() => setCount(c => c + 1), 1000); return () => clearInterval(id); }, []);",
    "EXPLAIN ANALYZE SELECT u.name FROM users u JOIN orders o ON o.user_id = u.id WHERE o.total > 1000 ORDER BY o.created_at DESC LIMIT 50;",
    "let regex = regex::Regex::new(r\"^\\d{4}-\\d{2}-\\d{2}$\").unwrap(); assert!(regex.is_match(\"2026-05-14\"));",
    "kubectl get pods -n production -o wide | grep -E 'CrashLoopBackOff|Error' | awk '{print $1}'",
]


def assert_panel_shape() -> list[str]:
    assert len(EN_PROMPTS) == 35, f"EN count {len(EN_PROMPTS)} != 35"
    assert len(VI_PROMPTS) == 35, f"VI count {len(VI_PROMPTS)} != 35"
    assert len(CODE_PROMPTS) == 30, f"CODE count {len(CODE_PROMPTS)} != 30"
    all_prompts = EN_PROMPTS + VI_PROMPTS + CODE_PROMPTS
    assert len(all_prompts) == 100
    # Deterministic ordering — sort by sha256 so re-runs produce an
    # identical fixture file.
    return sorted(all_prompts, key=lambda s: hashlib.sha256(s.encode("utf-8")).hexdigest())


def main() -> int:
    torch.set_num_threads(1)
    try:
        torch.use_deterministic_algorithms(True, warn_only=True)
    except Exception:  # noqa: BLE001
        pass

    prompts = assert_panel_shape()
    print(f"[spike] {len(prompts)} prompts ready; downloading {MODEL_ID}…", flush=True)

    snap_dir = snapshot_download(repo_id=MODEL_ID, cache_dir=str(CACHE_DIR))
    print(f"[spike] model cached at: {snap_dir}", flush=True)

    api = HfApi()
    info = api.model_info(MODEL_ID)
    revision = info.sha or "unknown"

    print("[spike] loading SentenceTransformer (cpu, fp32)…", flush=True)
    model = SentenceTransformer(
        snap_dir,
        device="cpu",
        model_kwargs={"torch_dtype": torch.float32},
        trust_remote_code=False,
    )
    # Put the model in inference mode (disables dropout etc.).
    model = model.train(False)

    print("[spike] encoding panel…", flush=True)
    with torch.no_grad():
        embs = model.encode(
            prompts,
            batch_size=8,
            convert_to_numpy=True,
            normalize_embeddings=True,
            show_progress_bar=True,
        )

    assert embs.shape == (100, 768), f"unexpected shape {embs.shape}"

    fixture = {
        "schema_version": 1,
        "model_id": MODEL_ID,
        "revision": revision,
        "torch_version": torch.__version__,
        "device": "cpu",
        "dtype": "float32",
        "normalize_embeddings": True,
        "pooling": "cls",
        "dim": 768,
        "count": len(prompts),
        "language_panel": {"en": 35, "vi": 35, "code": 30},
        "rows": [
            {"prompt": p, "embedding": embs[i].astype(float).tolist()}
            for i, p in enumerate(prompts)
        ],
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with open(FIXTURE_PATH, "w", encoding="utf-8") as f:
        json.dump(fixture, f, ensure_ascii=False)
    size_mb = FIXTURE_PATH.stat().st_size / (1024 * 1024)
    print(f"[spike] wrote {FIXTURE_PATH} ({size_mb:.2f} MB)", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
