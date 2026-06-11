//! SessionEnd envelope parsing (session-switch-detect task).
//!
//! Red evidence: before the dispatch lands, `parse` returns
//! `HookEvent::Unknown("SessionEnd")` and the typed assertions fail.

use lunaris_hook::envelope::{HookEvent, parse};

const SESSION_END_JSON: &str = r#"{
  "hook_event_name": "SessionEnd",
  "session_id": "sess-end-1",
  "cwd": "/tmp/test-hook-repo",
  "reason": "logout",
  "event_id": "evt-end-1"
}"#;

#[test]
fn session_end_parses_typed() {
    let ev = parse(SESSION_END_JSON.as_bytes()).expect("valid JSON must parse");
    match ev {
        HookEvent::SessionEnd(p) => {
            assert_eq!(p.session_id, "sess-end-1");
            assert_eq!(p.reason.as_deref(), Some("logout"));
            assert_eq!(p.cwd.as_deref(), Some("/tmp/test-hook-repo"));
        }
        other => panic!("SessionEnd must parse as a typed event, got {other:?}"),
    }
}

#[test]
fn session_end_minimal_fields_tolerated() {
    // Least-sure-flag defense: only the discriminator + session_id are
    // required; every other field is serde-defaulted so a payload-shape
    // mismatch degrades, never crashes.
    let ev = parse(br#"{"hook_event_name":"SessionEnd","session_id":"s2"}"#)
        .expect("minimal SessionEnd must parse");
    match ev {
        HookEvent::SessionEnd(p) => {
            assert_eq!(p.session_id, "s2");
            assert_eq!(p.reason, None);
            assert_eq!(p.cwd, None);
        }
        other => panic!("minimal SessionEnd must parse typed, got {other:?}"),
    }
}

#[test]
fn unknown_kind_still_noop() {
    // Forward-compat must survive the new variant: kinds outside the five
    // known values still land in Unknown.
    let ev = parse(br#"{"hook_event_name":"SomethingNew","session_id":"s"}"#)
        .expect("unknown kind must still parse");
    assert!(matches!(ev, HookEvent::Unknown(k) if k == "SomethingNew"));
}
