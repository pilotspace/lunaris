#!/usr/bin/env python3
"""W4.18 — every Rust fence in the book must be reachable by the compiler.

The book's 54 Rust examples had never been compiled. `mdbook test` cannot do it
(it forwards only `-L`, and Rust 2018+ needs `--extern`), so
`crates/lunaris-book-tests` pulls each page in with `#[doc = include_str!(..)]`
and lets rustdoc compile the fences against a real dependency graph. CI's
existing `cargo test --doc --workspace` step runs them by workspace membership.

Compiling them for the first time found real drift, including
`Graph::anchored(vec![alice], 2)` in three places when the seeds have been
`Vec<(EntityId, f32)>` for several releases — the identical bug W4.11 had
already fixed in the rustdoc fences, still live in the book because nothing
compiled the book.

That coverage is one forgotten edit from lapsing, in three different ways, so
each gets an assertion:

  1. A new page with Rust in it that nobody adds to `lib.rs` is invisible.
  2. A fence quietly marked `ignore` stops being checked while still looking
     like a checked example. The count is a shrink-only ratchet.
  3. An UNTAGGED ``` fence is compiled as Rust by both rustdoc and mdbook. The
     book's are diagrams and JSON; one containing Rust-ish text would either
     fail confusingly or pass without anyone intending it to be checked.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BOOK = ROOT / "docs" / "book" / "src"
HARNESS = ROOT / "crates" / "lunaris-book-tests" / "src" / "lib.rs"

# Two fences are deliberately `ignore`d. This is a CEILING that may only fall.
# `<=` would read as a budget for more; the count is pinned exactly, and the
# message says what to do if a third is genuinely justified.
EXPECTED_IGNORED = 2


def fenced_blocks(text: str):
    """Yield each fence's info string. Handles fences nested in blockquotes."""
    info, inside = None, False
    for line in text.split("\n"):
        stripped = line.lstrip("> ").rstrip()
        if not stripped.startswith("```"):
            continue
        if not inside:
            inside, info = True, stripped
            yield info
        else:
            inside = False


def pages_with_rust():
    out = []
    for p in sorted(BOOK.rglob("*.md")):
        text = p.read_text()
        if any(i.startswith("```rust") for i in fenced_blocks(text)):
            out.append(p)
    return out


class BookFencesAreCompiled(unittest.TestCase):
    def setUp(self):
        self.harness = HARNESS.read_text()
        self.pages = pages_with_rust()

    def test_the_scanner_finds_the_book_it_reasons_about(self):
        """Guard the guard: an empty scan would pass every assertion below."""
        self.assertTrue(BOOK.is_dir(), f"{BOOK} is missing")
        self.assertGreaterEqual(
            len(self.pages), 15, f"only {len(self.pages)} pages with Rust found"
        )
        # And prove the fence scanner sees a blockquoted fence, which is the
        # shape that slipped past the first pass over this book.
        self.assertIn("```rust", list(fenced_blocks("> ```rust\n> let x = 1;\n> ```")))

    def test_every_page_with_rust_is_in_the_harness(self):
        missing = [
            str(p.relative_to(ROOT))
            for p in self.pages
            if f'include_str!("../../../{p.relative_to(ROOT).as_posix()}")' not in self.harness
        ]
        self.assertEqual(
            missing,
            [],
            f"these book pages contain Rust that NOTHING compiles: {missing}. Add a "
            f"`#[doc = include_str!(..)] pub mod ..` line to {HARNESS.relative_to(ROOT)}.",
        )

    def test_the_harness_does_not_point_at_pages_that_moved(self):
        for rel in re.findall(r'include_str!\("\.\./\.\./\.\./([^"]+)"\)', self.harness):
            self.assertTrue(
                (ROOT / rel).is_file(),
                f"{HARNESS.relative_to(ROOT)} includes {rel}, which does not exist",
            )

    def test_ignored_fences_do_not_creep(self):
        ignored = []
        for p in sorted(BOOK.rglob("*.md")):
            for info in fenced_blocks(p.read_text()):
                if info.startswith("```rust") and "ignore" in info:
                    ignored.append(str(p.relative_to(BOOK)))
        self.assertEqual(
            len(ignored),
            EXPECTED_IGNORED,
            f"the book has {len(ignored)} `rust,ignore` fences ({ignored}), expected "
            f"{EXPECTED_IGNORED}. `ignore` suppresses the compile while still looking "
            "like a checked example. If a new one is genuinely right (a fence that "
            "mirrors a declaration rather than calling it), lower or raise this number "
            "deliberately and say why in the fence.",
        )

    def test_no_untagged_fences(self):
        """An untagged ``` fence is compiled as Rust by rustdoc AND mdbook."""
        untagged = []
        for p in sorted(BOOK.rglob("*.md")):
            for info in fenced_blocks(p.read_text()):
                if info.strip() == "```":
                    untagged.append(str(p.relative_to(BOOK)))
        self.assertEqual(
            untagged,
            [],
            f"untagged code fences in {untagged}. Both rustdoc and mdbook treat these "
            "as Rust. Tag them (```text, ```json, ```bash) so what is checked is what "
            "was meant to be checked.",
        )


if __name__ == "__main__":
    unittest.main(verbosity=2)
