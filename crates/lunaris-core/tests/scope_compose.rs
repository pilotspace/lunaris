//! RED suite for `multi-level-memory-categories` — the partition primitive.
//!
//! These tests pin the load-bearing security guarantee of the feature:
//! caller-supplied user/agent/session ids compose onto the JWT-bound base
//! scope by *narrowing* it (always a strict prefix-child), and can NEVER
//! escape to a sibling or parent partition. The `compose_never_escapes_base`
//! property test is the freeze's flagged invariant.
//!
//! RED reason (compile error, the right reason): `Scope::child`,
//! `compose_levels`, and `MemoryLevel` do not exist yet. The contract
//! (TASK.md §3, FROZEN @ v1) introduces them.
//!
//! Tests target PUBLIC API only (`Scope::new` / `Scope::child` /
//! `compose_levels` / `MemoryLevel`), so they live in an external integration
//! file — the `src/scope.rs` BUILD edits never touch this snapshot.

#![forbid(unsafe_code)]

use lunaris_core::scope::compose_levels;
use lunaris_core::{MemoryLevel, Scope};

/// Valid level-id segments per the contract alphabet `[A-Za-z0-9_-]`
/// (NO `.` — `.` is the composition separator).
const VALID_IDS: &[&str] = &["alice", "agent_7", "run-42", "A", "x9", "user_id-007"];

/// Adversarial / hostile ids that must be *rejected or narrowed*, NEVER
/// allowed to climb out of the base partition.
const ADVERSARIAL_IDS: &[&str] = &[
    "..",         // parent-traversal shape
    ".x",         // leading separator
    "x.",         // trailing separator
    "a.b",        // embedded separator (would forge an extra level)
    "",           // empty
    "u-evil",     // pre-tagged to look like a user level
    "a/b",        // path separator
    "x:y",        // keyspace delimiter — must never reach a KV key
    "../../etc",  // classic traversal
    "evil scope", // whitespace
    "✗",          // non-ascii
];

// ---------------------------------------------------------------------------
// Scope::child
// ---------------------------------------------------------------------------

#[test]
fn scope_child_composes_and_keeps_prefix() {
    let base = Scope::new("tenantA").expect("valid base");
    for id in VALID_IDS {
        let child = base.child(id).unwrap_or_else(|e| panic!("child({id:?}) should compose: {e}"));
        assert!(
            child.as_str().starts_with(base.as_str()),
            "child {:?} must keep base {:?} as a prefix",
            child.as_str(),
            base.as_str()
        );
        // A child is strictly deeper: base + "." + segment.
        assert_eq!(child.as_str(), format!("tenantA.{id}"), "exact composed form");
        assert!(
            child.as_str().len() > base.as_str().len(),
            "a child is strictly longer than its base"
        );
    }
}

#[test]
fn scope_child_rejects_dotted_or_colon_segment() {
    let base = Scope::new("tenantA").expect("valid base");
    for bad in ["al.ice", "al:ice", "a/b", "", "has space"] {
        assert!(
            base.child(bad).is_err(),
            "child({bad:?}) must be rejected — `.`/`:`/`/`/empty/space are not valid segment chars"
        );
    }
    // Base is a value type; rejecting a child must not mutate or affect it.
    assert_eq!(base.as_str(), "tenantA");
}

// ---------------------------------------------------------------------------
// compose_levels
// ---------------------------------------------------------------------------

#[test]
fn compose_levels_all_none_is_base_clone() {
    let base = Scope::new("tenantA").expect("valid base");
    let composed = compose_levels(&base, None, None, None).expect("all-None composes");
    assert_eq!(composed.as_str(), base.as_str(), "all-None must be back-compat: base unchanged");
}

#[test]
fn compose_levels_canonical_order_and_tags() {
    let base = Scope::new("tenantA").expect("valid base");
    // Canonical order: user (u-) → agent (a-) → session (s-).
    let composed = compose_levels(&base, Some("alice"), Some("planner"), Some("run42"))
        .expect("valid level ids compose");
    assert_eq!(
        composed.as_str(),
        "tenantA.u-alice.a-planner.s-run42",
        "levels compose in canonical order with u-/a-/s- tags"
    );
    assert!(composed.as_str().starts_with(base.as_str()), "composed keeps base prefix");

    // Partial composition skips absent levels but keeps order + prefix.
    let only_session =
        compose_levels(&base, None, None, Some("run42")).expect("session-only composes");
    assert_eq!(only_session.as_str(), "tenantA.s-run42");
    assert!(only_session.as_str().starts_with(base.as_str()));
}

#[test]
fn compose_levels_over_128_bytes_rejected() {
    let base = Scope::new("tenantA").expect("valid base");
    // Char-valid but length-blowing id: base(7) + ".u-"(3) + 130 → 140 > 128.
    let long_id = "a".repeat(130);
    assert!(
        compose_levels(&base, Some(&long_id), None, None).is_err(),
        "a composed scope over the 128-byte cap must be rejected"
    );
}

// ---------------------------------------------------------------------------
// SECURITY — the freeze's flagged, load-bearing invariant.
// ---------------------------------------------------------------------------

#[test]
fn compose_never_escapes_base() {
    let base = Scope::new("tenantA").expect("valid base");
    let base_prefix = format!("{}.", base.as_str()); // a real child starts with "tenantA."

    for id in ADVERSARIAL_IDS {
        // Drive the hostile id through every level position. Track how many
        // levels each call composes so the Ok branch can assert no EXTRA level
        // was forged (the discriminating check below).
        for (composed, n_levels) in [
            (compose_levels(&base, Some(id), None, None), 1),
            (compose_levels(&base, None, Some(id), None), 1),
            (compose_levels(&base, None, None, Some(id)), 1),
            // Mixed: a clean id alongside the hostile one — the hostile one
            // must still not let the result climb out.
            (compose_levels(&base, Some("alice"), Some(id), None), 2),
        ] {
            match composed {
                Err(_) => { /* rejected — safe */ }
                Ok(s) => {
                    assert!(
                        s.as_str().starts_with(&base_prefix),
                        "SECURITY: compose with hostile id {id:?} produced {:?}, which is NOT a \
                         strict child of base {:?} — a cross-partition escape",
                        s.as_str(),
                        base.as_str()
                    );
                    // And it must not byte-alias the base itself or a parent.
                    assert_ne!(
                        s.as_str(),
                        base.as_str(),
                        "a non-empty level must deepen the scope"
                    );
                    // DISCRIMINATING (catches a future weakening of
                    // is_valid_segment): the base "tenantA" carries 0 dots and
                    // each level adds EXACTLY one '.' separator. If a '.'-bearing
                    // id ever slipped past validation it would forge an extra
                    // level (e.g. "a.b" → "tenantA.u-a.b", 2 dots for 1 level),
                    // which this equality would catch even though the prefix
                    // assertion above would not.
                    assert_eq!(
                        s.as_str().matches('.').count(),
                        n_levels,
                        "SECURITY: hostile id {id:?} forged an extra level in {:?}",
                        s.as_str()
                    );
                }
            }
        }
    }
}

/// Pin the gate the whole no-escape invariant rests on. If `is_valid_segment`
/// is ever weakened to admit a level separator (`.`), the KV delimiter (`:`),
/// a path char (`/`), whitespace, or the empty string, `compose_levels` could
/// forge a level or byte-alias the `lunaris:{scope}:{kind}:{ulid}` KV format —
/// so assert the rejections (and a few acceptances) directly.
#[test]
fn is_valid_segment_rejects_structural_chars() {
    for bad in ["a.b", "a:b", "a/b", "", "a b", "..", "x.", ".x", "u-a.b", "✗"] {
        assert!(!Scope::is_valid_segment(bad), "{bad:?} must be REJECTED as a segment");
    }
    for ok in ["alice", "bot-1", "run_42", "U-X", "a", "0", "u-x"] {
        assert!(Scope::is_valid_segment(ok), "{ok:?} must be a valid segment");
    }
}

// ---------------------------------------------------------------------------
// MemoryLevel — the canonical tag set (keeps the enum wired into compose).
// ---------------------------------------------------------------------------

#[test]
fn memory_level_tags_are_canonical() {
    assert_eq!(MemoryLevel::User.tag(), "u");
    assert_eq!(MemoryLevel::Agent.tag(), "a");
    assert_eq!(MemoryLevel::Session.tag(), "s");
}
