//! Integration tests for `lunaris_hook::filter` (HOOK-03).
//!
//! Covers all three filter layers: path-glob deny, event-kind filter, and
//! content-size truncation. Tests 7 and 10 mutate process environment; a
//! module-level `parking_lot::Mutex` serialises those tests so parallel
//! test threads do not race on env state.
//!
//! Edition 2024 made `std::env::set_var` / `remove_var` `unsafe`; the
//! `#[allow(unused_unsafe)]` attribute is NOT used — each call site has an
//! explicit `unsafe` block with a safety comment.

use lunaris_hook::filter::{FilterPolicy, FilterVerdict, TruncatedPayload};

use lunaris_hook::envelope::{HookEvent, PreToolUsePayload};
use parking_lot::Mutex;

/// All env-mutating tests acquire this lock first so they do not race.
static ENV_LOCK: Mutex<()> = Mutex::new(());

// ── helpers ────────────────────────────────────────────────────────────────

fn pre_tool_use_with_path(tool_name: &str, path: &str) -> HookEvent {
    HookEvent::PreToolUse(PreToolUsePayload {
        hook_event_name: "PreToolUse".to_owned(),
        session_id: "sess-test".to_owned(),
        transcript_path: None,
        cwd: "/tmp".to_owned(),
        tool_name: tool_name.to_owned(),
        tool_input: serde_json::json!({ "path": path }),
        event_id: None,
        timestamp: None,
    })
}

fn pre_tool_use_no_path(tool_name: &str) -> HookEvent {
    HookEvent::PreToolUse(PreToolUsePayload {
        hook_event_name: "PreToolUse".to_owned(),
        session_id: "sess-test".to_owned(),
        transcript_path: None,
        cwd: "/tmp".to_owned(),
        tool_name: tool_name.to_owned(),
        tool_input: serde_json::json!({}),
        event_id: None,
        timestamp: None,
    })
}

// ── path deny layer ────────────────────────────────────────────────────────

/// Test 1: `.env` path is denied by built-in deny list.
#[test]
fn test1_path_deny_dotenv() {
    let policy = FilterPolicy::from_env().expect("policy");
    let event = pre_tool_use_with_path("Edit", ".env");
    assert_eq!(policy.apply(&event), FilterVerdict::Deny);
}

/// Test 2: `secrets/api.pem` matches built-in `**/*.pem` deny.
#[test]
fn test2_path_deny_pem_glob() {
    let policy = FilterPolicy::from_env().expect("policy");
    let event = pre_tool_use_with_path("Edit", "secrets/api.pem");
    assert_eq!(policy.apply(&event), FilterVerdict::Deny);
}

/// Test 3: `/home/user/.ssh/id_rsa` matches built-in `**/id_rsa*` deny.
#[test]
fn test3_path_deny_id_rsa() {
    let policy = FilterPolicy::from_env().expect("policy");
    let event = pre_tool_use_with_path("Edit", "/home/user/.ssh/id_rsa");
    assert_eq!(policy.apply(&event), FilterVerdict::Deny);
}

/// Test 4: `.git/config` matches built-in `**/.git/**` deny.
#[test]
fn test4_path_deny_git_dir() {
    let policy = FilterPolicy::from_env().expect("policy");
    let event = pre_tool_use_with_path("Edit", ".git/config");
    assert_eq!(policy.apply(&event), FilterVerdict::Deny);
}

// ── kind filter layer ──────────────────────────────────────────────────────

/// Test 5: PreToolUse with tool_name="Read" passes default kind filter.
#[test]
fn test5_kind_read_default_allow() {
    let policy = FilterPolicy::from_env().expect("policy");
    let event = pre_tool_use_no_path("Read");
    assert_eq!(policy.apply(&event), FilterVerdict::Allow);
}

/// Test 6: PreToolUse with tool_name="Edit" passes default kind filter.
#[test]
fn test6_kind_edit_default_allow() {
    let policy = FilterPolicy::from_env().expect("policy");
    let event = pre_tool_use_no_path("Edit");
    assert_eq!(policy.apply(&event), FilterVerdict::Allow);
}

/// Test 7: LUNARIS_HOOK_INCLUDE="**/*.rs" overrides kind filter for Read on .rs path.
#[test]
fn test7_kind_include_override_allows_read_rs() {
    let _guard = ENV_LOCK.lock();
    // SAFETY: serialised by ENV_LOCK; no other thread is reading or writing
    // LUNARIS_HOOK_INCLUDE during this block.
    unsafe { std::env::set_var("LUNARIS_HOOK_INCLUDE", "**/*.rs") };
    let policy = FilterPolicy::from_env().expect("policy");
    // SAFETY: same lock guard scope.
    unsafe { std::env::remove_var("LUNARIS_HOOK_INCLUDE") };

    let event = pre_tool_use_with_path("Read", "src/main.rs");
    assert_eq!(
        policy.apply(&event),
        FilterVerdict::Allow,
        "Read of a .rs path should be allowed when LUNARIS_HOOK_INCLUDE=**/*.rs"
    );
}

// ── content truncation layer ───────────────────────────────────────────────

/// Test 8: payload exceeding 128 KiB is truncated with truncated_bytes > 0.
#[test]
fn test8_content_truncation_large_payload() {
    // 200 KiB of 'A' bytes.
    let content: String = "A".repeat(200 * 1024);
    let result: TruncatedPayload = FilterPolicy::truncate_payload(&content);

    assert!(result.truncated_bytes > 0, "truncated_bytes must be > 0 for large payload");
    assert!(result.content.contains("…elided…"), "truncated content must contain elision marker");
    // Head: 64 KiB; tail: 32 KiB; elided: 200 KiB - 64 KiB - 32 KiB = 104 KiB.
    assert_eq!(
        result.truncated_bytes,
        (200 * 1024 - 64 * 1024 - 32 * 1024) as u64,
        "truncated_bytes must equal elided byte count"
    );
}

/// Test 9: payload under 128 KiB is returned unchanged with truncated_bytes=0.
#[test]
fn test9_content_no_truncation_small_payload() {
    let content = "hello lunaris".to_owned();
    let result: TruncatedPayload = FilterPolicy::truncate_payload(&content);

    assert_eq!(result.truncated_bytes, 0, "no truncation for small payload");
    assert_eq!(result.content, content, "content must be unchanged");
}

// ── LUNARIS_HOOK_EXCLUDE ───────────────────────────────────────────────────

/// Test 10: LUNARIS_HOOK_EXCLUDE="**/build/**" denies a path under build/.
#[test]
fn test10_exclude_env_denies_build_path() {
    let _guard = ENV_LOCK.lock();
    // SAFETY: serialised by ENV_LOCK.
    unsafe { std::env::set_var("LUNARIS_HOOK_EXCLUDE", "**/build/**") };
    let policy = FilterPolicy::from_env().expect("policy");
    // SAFETY: same lock guard scope.
    unsafe { std::env::remove_var("LUNARIS_HOOK_EXCLUDE") };

    let event = pre_tool_use_with_path("Edit", "target/build/output.o");
    assert_eq!(policy.apply(&event), FilterVerdict::Deny);
}

// ── built-in deny wins over explicit include ───────────────────────────────

/// Test 11: LUNARIS_HOOK_INCLUDE="**/.env" does NOT re-allow a .env path.
///
/// Built-in deny list ALWAYS wins; user INCLUDE cannot override it.
#[test]
fn test11_builtin_deny_wins_over_explicit_include() {
    let _guard = ENV_LOCK.lock();
    // SAFETY: serialised by ENV_LOCK.
    unsafe { std::env::set_var("LUNARIS_HOOK_INCLUDE", "**/.env") };
    let policy = FilterPolicy::from_env().expect("policy");
    // SAFETY: same lock guard scope.
    unsafe { std::env::remove_var("LUNARIS_HOOK_INCLUDE") };

    let event = pre_tool_use_with_path("Edit", ".env");
    assert_eq!(
        policy.apply(&event),
        FilterVerdict::Deny,
        "built-in deny must win even when LUNARIS_HOOK_INCLUDE explicitly includes .env"
    );
}
