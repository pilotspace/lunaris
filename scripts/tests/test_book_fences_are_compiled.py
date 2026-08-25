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
DOCS = ROOT / "docs"
HARNESS = ROOT / "crates" / "lunaris-book-tests" / "src" / "lib.rs"

# W4.18 second half: `docs/` pages OUTSIDE `docs/book/src/`. 116 further Rust
# fences lived here and nothing compiled any of them — the larger half, since
# even a wired `mdbook test` would never reach outside the book's own src.
#
# Each prefix below is EXCLUDED, and each exclusion is a decision with a reason.
# Pinning them here is the point: a page that lands under one of these
# directories drops out of coverage silently, and the only defence is that
# widening the list requires editing this literal.
EXCLUDED_PREFIXES = (
    # Historical records. Their Rust is a proposal frozen at authoring time —
    # compiling it would force edits that falsify the record.
    "docs/rfcs/",
    "docs/design/",
    "docs/decisions/",
    "docs/spikes/",
    "docs/planning/",
    # These SHOW the retired API beside its replacement on purpose. A migration
    # guide whose "before" block compiles is a migration guide that is wrong.
    "docs/migration/",
    # Reference material about other repos, plus prose-embedded sketches that
    # name types Lunaris does not define.
    "docs/testing/",
    "docs/integration/helios-memory-engine.md",
)

# Exactly the `docs/`-outside-the-book pages that ARE compiled. Pinned as a set
# rather than a count: a count is satisfied by a swap, and dropping the page
# with the drift in it while adding a page with none reads identical.
EXPECTED_DOCS_PAGES = {
    "docs/MIGRATING-FROM-ZEP.md",
    "docs/guide.md",
    "docs/helios-integration.md",
    "docs/operations/external-moon.md",
    "docs/protocol/conformance.md",
    "docs/release/deprecations.md",
}

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


def harness_includes(harness_text: str):
    """Paths the harness ACTUALLY includes.

    A plain substring search over the file cannot tell `#[doc = include_str!(x)]`
    from `// #[doc = include_str!(x)]`, so commenting a page out reads exactly
    like including it — the mutation that first passed this guard. Parse line by
    line and skip anything commented.
    """
    found = set()
    for line in harness_text.split("\n"):
        stripped = line.strip()
        if stripped.startswith("//"):
            continue
        found.update(re.findall(r'include_str!\("\.\./\.\./\.\./([^"]+)"\)', stripped))
    return found


def docs_pages_with_rust():
    """`docs/**` pages with Rust, minus the book (covered above) and exclusions."""
    out = []
    for p in sorted(DOCS.rglob("*.md")):
        rel = p.relative_to(ROOT).as_posix()
        if rel.startswith("docs/book/"):
            continue
        if any(rel.startswith(pre) for pre in EXCLUDED_PREFIXES):
            continue
        if any(i.startswith("```rust") for i in fenced_blocks(p.read_text())):
            out.append(rel)
    return out


class BookFencesAreCompiled(unittest.TestCase):
    def setUp(self):
        self.harness = HARNESS.read_text()
        self.includes = harness_includes(self.harness)
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

    def test_a_commented_out_include_does_not_count_as_included(self):
        """The mutation that first slipped past this guard, pinned."""
        live = "#[doc = include_str!(\"../../../docs/guide.md\")]"
        self.assertEqual(harness_includes(live), {"docs/guide.md"})
        self.assertEqual(harness_includes("// " + live), set())
        self.assertEqual(harness_includes("  //" + live), set())

    def test_every_page_with_rust_is_in_the_harness(self):
        missing = [
            str(p.relative_to(ROOT))
            for p in self.pages
            if p.relative_to(ROOT).as_posix() not in self.includes
        ]
        self.assertEqual(
            missing,
            [],
            f"these book pages contain Rust that NOTHING compiles: {missing}. Add a "
            f"`#[doc = include_str!(..)] pub mod ..` line to {HARNESS.relative_to(ROOT)}.",
        )

    def test_the_harness_does_not_point_at_pages_that_moved(self):
        for rel in sorted(self.includes):
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

    # --- W4.18 second half: `docs/` outside the book -----------------------

    def test_the_docs_scanner_finds_something_to_reason_about(self):
        """Guard the guard: an over-broad exclusion list would empty the scan."""
        found = docs_pages_with_rust()
        self.assertGreaterEqual(
            len(found), 5, f"only {len(found)} docs/ pages with Rust found: {found}"
        )
        # And prove an exclusion is actually excluding — `docs/migration/` has
        # Rust in it, so an empty EXCLUDED_PREFIXES would surface it here.
        self.assertTrue(
            any(
                p.name.endswith(".md")
                and any(i.startswith("```rust") for i in fenced_blocks(p.read_text()))
                for p in (DOCS / "migration").rglob("*.md")
            ),
            "docs/migration/ has no Rust, so its exclusion no longer proves anything",
        )

    def test_the_compiled_docs_pages_are_exactly_the_pinned_set(self):
        found = set(docs_pages_with_rust())
        self.assertEqual(
            found,
            EXPECTED_DOCS_PAGES,
            "the set of docs/ pages carrying Rust has changed. Add the new page to "
            f"{HARNESS.relative_to(ROOT)} AND to EXPECTED_DOCS_PAGES here, or, if it "
            "genuinely should not be compiled, add it to EXCLUDED_PREFIXES with the "
            "reason. Silence is the failure mode this pins shut.",
        )

    def test_every_docs_page_with_rust_is_in_the_harness(self):
        missing = [rel for rel in docs_pages_with_rust() if rel not in self.includes]
        self.assertEqual(
            missing,
            [],
            f"these docs/ pages contain Rust that NOTHING compiles: {missing}",
        )

    def test_ignored_fences_do_not_creep_in_docs(self):
        """Same ratchet as the book's, over the pages the harness compiles."""
        ignored = [
            rel
            for rel in docs_pages_with_rust()
            for info in fenced_blocks((ROOT / rel).read_text())
            if info.startswith("```rust") and "ignore" in info
        ]
        self.assertEqual(
            ignored,
            [],
            f"`rust,ignore` fences in compiled docs/ pages: {ignored}. `ignore` "
            "suppresses the compile while still looking like a checked example. The "
            "floor here is 0 — unlike the book's, no docs/ fence has earned one.",
        )

    def test_no_untagged_fences_in_docs(self):
        untagged = [
            rel
            for rel in docs_pages_with_rust()
            for info in fenced_blocks((ROOT / rel).read_text())
            if info.strip() == "```"
        ]
        self.assertEqual(
            untagged,
            [],
            f"untagged code fences in {untagged}. rustdoc compiles these as Rust, so "
            "an ASCII diagram or a shell snippet becomes a confusing compile error. "
            "Tag them (```text, ```json, ```bash).",
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
