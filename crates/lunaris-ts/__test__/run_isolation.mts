/**
 * Per-run isolation helpers, shared by every parity suite that writes into a
 * live Moon.
 *
 * `runTag` keeps two runs' ROWS distinguishable. `runWindowOffsetMs` keeps two
 * runs' rows from competing for the same top-k. A suite that filters on valid
 * time needs both — see the note on `runWindowOffsetMs`.
 */

/**
 * A per-run discriminator for source prefixes.
 *
 * Any suite writing under a fixed prefix is only correct against a FRESH
 * backend: it re-ingests its fixtures on top of the previous run's, and an
 * exact-count assertion then reads double. CI gets away with it because
 * runners are fresh; a developer running twice does not (F34 — the Python
 * twin of `documentary_parity.spec.mts` read 12 events where it asserts 6,
 * every one duplicated, and this file's four prefixes had the same shape).
 *
 * Not a ULID — pulling a dependency in for this would be the only reason
 * these files need one. Time + entropy is enough to keep two runs against the
 * same Moon from sharing a prefix, which is all this is for.
 *
 * `randomBytes`, not `Math.random()`: this value ends up inside a scope-ish
 * identifier, and CodeQL's `js/insecure-randomness` rule flags PRNG output
 * reaching one (high severity). The rule is arguably over-firing on a test
 * discriminator, but the CSPRNG costs nothing here and a muzzled alert is
 * worse than a one-line change.
 *
 * It lives in its own module rather than in each spec so there is ONE
 * definition to find when the next suite needs it.
 */
import crypto from "node:crypto";

export function runTag(): string {
  return `${Date.now().toString(36)}${crypto.randomBytes(4).toString("hex")}`;
}

/**
 * A per-run shift for any fixture whose test filters on VALID TIME.
 *
 * `runTag` is not enough on its own. A per-run source prefix keeps two runs'
 * rows distinguishable, but the recipes filter that prefix in memory AFTER a
 * global `top(30)` — while `.between()` pushes `@valid_time:[lo hi]` down to
 * Moon. So every run's rows land in the SAME window and compete for the same
 * 30 slots. Measured: run 2 of the timeline scenario returned 12, run 5
 * returned exactly 30 (the cap), and once both language suites piled onto the
 * same window a later run returned 5 of its own 6 — an UNDER-return, which
 * reads like a product bug rather than a dirty store.
 *
 * Shifting the window per run is what actually isolates: Moon's own numeric
 * filter then excludes the other runs, so the top-30 never sees them.
 *
 * Drawn from entropy, not from the clock. A time-derived offset looks tidier
 * but two suites that start in the same second — which is exactly what a
 * `vitest run` over both spec files or a CI matrix does — would land on the
 * same window and reproduce the bug this exists to prevent. The shift is a
 * uniform translation, so it changes no assertion's meaning; the only thing at
 * stake is collision probability, and 1e5 seven-day slots puts that at ~1e-5
 * per pair of runs. A collision just re-creates the old failure, loudly.
 */
export function runWindowOffsetMs(): number {
  // `crypto.randomInt`, not `randomBytes(4) % 100_000`: a modulo of a 32-bit
  // value by a non-power-of-two biases the low slots, and CodeQL's
  // `js/biased-cryptographic-random` flags it high-severity. The bias is
  // irrelevant to a test discriminator, but the unbiased API is the same one
  // line — and `secrets.randbelow`, which the Python twin uses, already
  // rejection-samples. Matching them costs nothing and leaves no alert to
  // explain away.
  return crypto.randomInt(0, 100_000) * 7 * 86_400_000;
}
