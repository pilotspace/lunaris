#!/usr/bin/env python3
"""Generate the reference-score fixture for `lunaris-rerank-native`'s
numerical-equivalence integration test.

Runs `sentence_transformers.CrossEncoder("BAAI/bge-reranker-v2-m3")` over a
hand-curated 100-pair panel (25 each: en_to_en, en_to_vi, en_to_code,
vi_to_vi), applies the sigmoid activation to the raw logits, and writes
`crates/lunaris-rerank-native/tests/fixtures/reference_scores.json`.

## Why sigmoid?

`CrossEncoder.predict()` for bge-reranker-v2-m3 returns raw logits by default.
Lunaris v0.4 standardises on the sigmoid-bounded score (∈ [0, 1]) so that
downstream RRF fusion / score thresholding sees a value in the same band as
cosine similarity from the embedder. The Rust `NativeReranker.score`
applies the same sigmoid; the fixture must match.

## Usage

  pip install sentence-transformers==3.* torch
  python3 scripts/spike-generate-reference-rerank-scores.py

## Idempotency

The model is downloaded once into `~/.cache/lunaris/spike/bge-reranker-v2-m3/`
(via the HF Hub cache; we just set `HF_HOME`). Re-running the script regenerates
the JSON deterministically — `CrossEncoder.predict` is deterministic on CPU
fp32 across the same `sentence-transformers` patch version.

## Env vars exported on success

After a successful run the script prints the three env vars the Rust IT test
needs:

  export BGE_RERANKER_WEIGHTS_PATH=...
  export BGE_RERANKER_TOKENIZER_PATH=...
  export BGE_RERANKER_CONFIG_PATH=...
"""

from __future__ import annotations

import json
import math
import os
import sys
from pathlib import Path

# Pin HF cache to ~/.cache/lunaris/spike so the spike doesn't pollute the
# system-wide HF cache. The IT then reads from the snapshot subdir.
SPIKE_CACHE = Path.home() / ".cache" / "lunaris" / "spike"
SPIKE_CACHE.mkdir(parents=True, exist_ok=True)
os.environ.setdefault("HF_HOME", str(SPIKE_CACHE))

REPO_ROOT = Path(__file__).resolve().parent.parent
FIXTURE_PATH = (
    REPO_ROOT
    / "crates"
    / "lunaris-rerank-native"
    / "tests"
    / "fixtures"
    / "reference_scores.json"
)

MODEL_ID = "BAAI/bge-reranker-v2-m3"


# ---------------------------------------------------------------------------
# Hand-curated 100-pair panel
# ---------------------------------------------------------------------------
# Authoring rules:
# - 25 en_to_en: English query ↔ English doc. Mix relevant + irrelevant so
#   sigmoid scores span [low, high]; this drives the variance > 0.01 check.
# - 25 en_to_vi: English query ↔ Vietnamese doc (cross-lingual relevance).
# - 25 en_to_code: English query ↔ code snippet (Python / Rust / SQL).
# - 25 vi_to_vi: Vietnamese query ↔ Vietnamese doc. Include diacritic-heavy
#   strings so NFC normalization gets exercised end-to-end.

EN_TO_EN: list[tuple[str, str]] = [
    ("how to sort a list in python", "Use the built-in sorted() function or list.sort() method."),
    ("how to sort a list in python", "The capital of France is Paris."),
    ("what causes rain", "Rain forms when water vapor in clouds condenses and falls as precipitation."),
    ("what causes rain", "The Roman Empire fell in 476 AD."),
    ("benefits of regular exercise", "Regular exercise improves cardiovascular health, mood, and longevity."),
    ("benefits of regular exercise", "Photosynthesis converts sunlight into chemical energy."),
    ("difference between TCP and UDP", "TCP is connection-oriented and reliable; UDP is connectionless and faster."),
    ("difference between TCP and UDP", "The mitochondrion is the powerhouse of the cell."),
    ("what is machine learning", "Machine learning trains statistical models to make predictions from data."),
    ("what is machine learning", "The Pacific Ocean is the largest ocean on Earth."),
    ("how does encryption work", "Encryption transforms plaintext into ciphertext using a key, reversible only with the matching key."),
    ("how does encryption work", "Shakespeare wrote 37 plays during his career."),
    ("best practices for password security", "Use long unique passwords stored in a password manager and enable 2FA."),
    ("best practices for password security", "Mount Everest is the tallest mountain above sea level."),
    ("what is a database index", "A database index is a data structure that speeds up row lookup at the cost of write overhead."),
    ("what is a database index", "Bananas contain potassium and dietary fiber."),
    ("how to write a unit test", "Write a function that exercises one behavior, asserts the expected output, and runs in isolation."),
    ("how to write a unit test", "The Great Wall of China is over 13,000 miles long."),
    ("symptoms of the flu", "Common flu symptoms include fever, body aches, cough, and fatigue."),
    ("symptoms of the flu", "Saturn has prominent rings made of ice and rock."),
    ("what is a vector database", "A vector database indexes high-dimensional embeddings for approximate nearest-neighbor search."),
    ("what is a vector database", "Bread is made by fermenting flour and water with yeast."),
    ("how to deploy a web app", "Build the app, containerize it, push the image to a registry, and roll it out to a cluster."),
    ("how to deploy a web app", "Whales are marine mammals, not fish."),
    ("explain transformer architecture", "Transformers use self-attention layers to weight token relevance, replacing recurrence with parallelism."),
]

EN_TO_VI: list[tuple[str, str]] = [
    ("what is the capital of vietnam", "Hà Nội là thủ đô của Việt Nam."),
    ("what is the capital of vietnam", "Phở là món ăn truyền thống của Việt Nam."),
    ("traditional vietnamese coffee", "Cà phê sữa đá là thức uống truyền thống nổi tiếng của Việt Nam."),
    ("traditional vietnamese coffee", "Vịnh Hạ Long là di sản thế giới được UNESCO công nhận."),
    ("vietnamese pho recipe", "Phở bò được nấu từ nước dùng xương bò, bánh phở, thịt bò và rau thơm."),
    ("vietnamese pho recipe", "Hồ Hoàn Kiếm nằm ở trung tâm Hà Nội."),
    ("ha long bay travel", "Vịnh Hạ Long có hàng nghìn đảo đá vôi và là điểm du lịch nổi tiếng."),
    ("ha long bay travel", "Bánh mì Việt Nam là sự kết hợp giữa ẩm thực Pháp và Việt."),
    ("vietnamese new year traditions", "Tết Nguyên Đán là dịp lễ quan trọng nhất, mọi người sum họp gia đình và lì xì."),
    ("vietnamese new year traditions", "Sài Gòn là tên cũ của Thành phố Hồ Chí Minh."),
    ("learning vietnamese language", "Tiếng Việt sử dụng bảng chữ cái Latin với các dấu thanh điệu."),
    ("learning vietnamese language", "Áo dài là trang phục truyền thống của phụ nữ Việt Nam."),
    ("how to make banh mi", "Bánh mì Việt Nam gồm vỏ bánh giòn, pa-tê, thịt nguội, rau thơm và đồ chua."),
    ("how to make banh mi", "Sông Mê Kông chảy qua đồng bằng Nam Bộ."),
    ("vietnam war history", "Chiến tranh Việt Nam kết thúc năm 1975 với sự thống nhất đất nước."),
    ("vietnam war history", "Bún chả là món ăn nổi tiếng của Hà Nội."),
    ("rice farming in vietnam", "Đồng bằng sông Cửu Long là vựa lúa lớn nhất Việt Nam."),
    ("rice farming in vietnam", "Hồ Tây là hồ lớn nhất ở Hà Nội."),
    ("ao dai traditional dress", "Áo dài là biểu tượng của vẻ đẹp và văn hóa truyền thống Việt Nam."),
    ("ao dai traditional dress", "Phở bò là món ăn sáng phổ biến tại Việt Nam."),
    ("vietnamese coffee culture", "Văn hóa cà phê Việt Nam rất đa dạng với cà phê phin, cà phê trứng và cà phê muối."),
    ("vietnamese coffee culture", "Sông Hồng chảy qua thủ đô Hà Nội."),
    ("mekong delta tourism", "Du lịch đồng bằng sông Cửu Long mang đến trải nghiệm chợ nổi và vườn trái cây."),
    ("mekong delta tourism", "Tiếng Việt có sáu thanh điệu khác nhau."),
    ("vietnamese silk craft", "Lụa Hà Đông và lụa Vạn Phúc là những làng nghề dệt lụa truyền thống nổi tiếng."),
]

EN_TO_CODE: list[tuple[str, str]] = [
    (
        "python sort list",
        "def sort_numbers(xs):\n    return sorted(xs)\n",
    ),
    (
        "python sort list",
        "SELECT * FROM users WHERE created_at > NOW() - INTERVAL '7 days';",
    ),
    (
        "rust async function",
        "async fn fetch_url(url: &str) -> Result<String, reqwest::Error> {\n    reqwest::get(url).await?.text().await\n}\n",
    ),
    (
        "rust async function",
        "import numpy as np\narr = np.zeros((10, 10))",
    ),
    (
        "sql group by example",
        "SELECT category, COUNT(*) AS n FROM products GROUP BY category ORDER BY n DESC;",
    ),
    (
        "sql group by example",
        "fn main() { println!(\"Hello, world!\"); }",
    ),
    (
        "javascript fetch api",
        "const res = await fetch('/api/users');\nconst json = await res.json();",
    ),
    (
        "javascript fetch api",
        "CREATE INDEX idx_users_email ON users(email);",
    ),
    (
        "python read csv file",
        "import csv\nwith open('data.csv') as f:\n    reader = csv.DictReader(f)\n    rows = list(reader)\n",
    ),
    (
        "python read csv file",
        "let v: Vec<i32> = (0..10).collect();",
    ),
    (
        "rust error handling result",
        "fn parse(s: &str) -> Result<i32, std::num::ParseIntError> {\n    s.parse::<i32>()\n}\n",
    ),
    (
        "rust error handling result",
        "df.groupby('category').agg({'price': 'mean'})",
    ),
    (
        "regex match email",
        "import re\npattern = r'^[\\w._%+-]+@[\\w.-]+\\.[A-Za-z]{2,}$'\nre.match(pattern, addr)",
    ),
    (
        "regex match email",
        "git checkout -b feature/login",
    ),
    (
        "docker run container",
        "docker run -d --name web -p 8080:80 nginx:alpine",
    ),
    (
        "docker run container",
        "console.log('hello');",
    ),
    (
        "kubernetes deployment yaml",
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  replicas: 3",
    ),
    (
        "kubernetes deployment yaml",
        "for i in range(10): print(i)",
    ),
    (
        "python list comprehension",
        "squares = [x * x for x in range(10) if x % 2 == 0]",
    ),
    (
        "python list comprehension",
        "SELECT version();",
    ),
    (
        "rust hashmap insert",
        "use std::collections::HashMap;\nlet mut m: HashMap<&str, i32> = HashMap::new();\nm.insert(\"a\", 1);",
    ),
    (
        "rust hashmap insert",
        "<html><body>Hello</body></html>",
    ),
    (
        "git rebase interactive",
        "git rebase -i HEAD~3  # squash and reorder the last three commits",
    ),
    (
        "git rebase interactive",
        "function add(a, b) { return a + b; }",
    ),
    (
        "tokio spawn task",
        "tokio::spawn(async move {\n    process(payload).await;\n});",
    ),
]

VI_TO_VI: list[tuple[str, str]] = [
    ("phở bò ngon nhất ở đâu", "Phở Thìn ở Lò Đúc là một trong những quán phở bò nổi tiếng nhất Hà Nội."),
    ("phở bò ngon nhất ở đâu", "Cà phê sữa đá là thức uống phổ biến vào buổi sáng."),
    ("học tiếng anh hiệu quả", "Để học tiếng Anh hiệu quả nên luyện nghe nói hàng ngày và đọc nhiều tài liệu."),
    ("học tiếng anh hiệu quả", "Vịnh Hạ Long có nhiều hang động đẹp."),
    ("cách nấu phở gà", "Để nấu phở gà ngon cần ninh xương gà với gừng, hành nướng và gia vị thuốc bắc."),
    ("cách nấu phở gà", "Áo dài là biểu tượng văn hóa Việt Nam."),
    ("du lịch đà lạt", "Đà Lạt nổi tiếng với khí hậu mát mẻ quanh năm và những đồi thông xanh."),
    ("du lịch đà lạt", "Phở là món ăn truyền thống của miền Bắc."),
    ("văn hóa trà việt nam", "Trà sen Tây Hồ là một trong những loại trà truyền thống quý hiếm của Việt Nam."),
    ("văn hóa trà việt nam", "Sài Gòn có nhiều quán cà phê hiện đại."),
    ("kinh nghiệm phượt sapa", "Đi phượt Sa Pa nên chuẩn bị áo ấm, giày leo núi và lên kế hoạch trekking các bản làng."),
    ("kinh nghiệm phượt sapa", "Bánh mì Việt Nam có lớp vỏ giòn rụm."),
    ("cách làm bánh chưng ngày tết", "Bánh chưng được gói bằng lá dong, nhân đậu xanh, thịt mỡ và nếp."),
    ("cách làm bánh chưng ngày tết", "Hồ Gươm nằm ở trung tâm Hà Nội."),
    ("lịch sử thủ đô hà nội", "Hà Nội có lịch sử hơn một nghìn năm, được vua Lý Thái Tổ chọn làm kinh đô năm 1010."),
    ("lịch sử thủ đô hà nội", "Cà phê trứng là đặc sản Hà Nội."),
    ("học lập trình python", "Để bắt đầu học Python nên làm quen với cú pháp, biến, vòng lặp và hàm cơ bản."),
    ("học lập trình python", "Vịnh Hạ Long là di sản thế giới."),
    ("văn hóa áo dài việt nam", "Áo dài Việt Nam có nhiều kiểu dáng, phổ biến nhất là áo dài truyền thống cổ cao."),
    ("văn hóa áo dài việt nam", "Phở bò là món ăn quốc dân của Việt Nam."),
    ("ẩm thực miền tây nam bộ", "Ẩm thực miền Tây nổi bật với canh chua cá lóc, cá kho tộ và lẩu mắm."),
    ("ẩm thực miền tây nam bộ", "Bún chả Hà Nội rất nổi tiếng."),
    ("tết nguyên đán việt nam", "Tết Nguyên Đán là dịp lễ lớn nhất trong năm, với các phong tục lì xì, gói bánh chưng và chúc Tết."),
    ("tết nguyên đán việt nam", "Sông Mê Kông chảy qua nhiều nước."),
    ("nghệ thuật rối nước", "Múa rối nước là loại hình nghệ thuật dân gian độc đáo của Việt Nam, biểu diễn trên mặt nước."),
]


def main() -> int:
    try:
        from sentence_transformers import CrossEncoder  # type: ignore
    except ImportError as e:
        print(
            f"[error] sentence-transformers not installed ({e}). Install with:\n"
            f"  pip install sentence-transformers==3.* torch",
            file=sys.stderr,
        )
        return 1

    # Sanity-check the panel counts before doing any GPU work.
    assert len(EN_TO_EN) == 25, f"en_to_en has {len(EN_TO_EN)}, expected 25"
    assert len(EN_TO_VI) == 25, f"en_to_vi has {len(EN_TO_VI)}, expected 25"
    assert len(EN_TO_CODE) == 25, f"en_to_code has {len(EN_TO_CODE)}, expected 25"
    assert len(VI_TO_VI) == 25, f"vi_to_vi has {len(VI_TO_VI)}, expected 25"

    pairs: list[dict] = []

    def add(category: str, prefix: str, group: list[tuple[str, str]]) -> None:
        for i, (q, d) in enumerate(group):
            pairs.append(
                {
                    "id": f"{prefix}-{i + 1:03d}",
                    "query": q,
                    "doc": d,
                    "category": category,
                }
            )

    add("en_to_en", "en-en", EN_TO_EN)
    add("en_to_vi", "en-vi", EN_TO_VI)
    add("en_to_code", "en-code", EN_TO_CODE)
    add("vi_to_vi", "vi-vi", VI_TO_VI)

    assert len(pairs) == 100, f"expected 100 pairs, got {len(pairs)}"

    print(f"[info] loading CrossEncoder({MODEL_ID}) — HF_HOME={os.environ['HF_HOME']}")
    model = CrossEncoder(MODEL_ID, device="cpu", max_length=512)

    inputs = [(p["query"], p["doc"]) for p in pairs]
    print(f"[info] predicting {len(inputs)} pairs (batch_size=8) ...")
    raw_logits = model.predict(inputs, batch_size=8, show_progress_bar=True)

    # Apply sigmoid to map raw logits → [0, 1]. Matches the Rust
    # `NativeReranker::score` activation.
    sigmoid = lambda x: 1.0 / (1.0 + math.exp(-float(x)))
    for p, logit in zip(pairs, raw_logits):
        p["reference_score"] = round(sigmoid(logit), 6)

    fixture = {
        "schema_version": 1,
        "model": MODEL_ID,
        "activation": "sigmoid",
        "max_seq": 512,
        "device": "cpu",
        "dtype": "fp32",
        "generator": "scripts/spike-generate-reference-rerank-scores.py",
        # `populated: true` indicates `reference_score` is filled for every
        # pair. The committed seed fixture ships with `populated: false` and
        # `reference_score: null` for every pair (so the schema + panel are
        # version-controlled but the live numbers come from this script);
        # running this script flips it to `populated: true` with real scores.
        "populated": True,
        "pairs": pairs,
    }

    FIXTURE_PATH.parent.mkdir(parents=True, exist_ok=True)
    with FIXTURE_PATH.open("w", encoding="utf-8") as f:
        json.dump(fixture, f, ensure_ascii=False, indent=2)
        f.write("\n")
    print(f"[ok] wrote {FIXTURE_PATH} ({FIXTURE_PATH.stat().st_size} bytes)")

    scores = [p["reference_score"] for p in pairs]
    print(
        f"[stats] min={min(scores):.4f} max={max(scores):.4f} "
        f"mean={sum(scores) / len(scores):.4f}"
    )

    # Print the env vars the Rust IT needs. The exact filenames depend on the
    # HF Hub snapshot layout; we glob to discover them.
    snapshot_root = SPIKE_CACHE / "hub"
    candidates = list(snapshot_root.glob("**/model.safetensors"))
    if candidates:
        weights = candidates[0]
        model_dir = weights.parent
        tokenizer = model_dir / "tokenizer.json"
        config = model_dir / "config.json"
        print("\n[next] export these env vars before running the Rust IT:")
        print(f"  export BGE_RERANKER_WEIGHTS_PATH={weights}")
        print(f"  export BGE_RERANKER_TOKENIZER_PATH={tokenizer}")
        print(f"  export BGE_RERANKER_CONFIG_PATH={config}")
        print(
            "\nThen:\n"
            "  cargo test -p lunaris-rerank-native --features reranker-it --release \\\n"
            "    --test numerical_equivalence -- --nocapture"
        )
    else:
        print(
            "[warn] could not auto-discover model.safetensors under HF_HOME; "
            f"locate it manually under {SPIKE_CACHE} and export the three BGE_RERANKER_*_PATH env vars."
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
