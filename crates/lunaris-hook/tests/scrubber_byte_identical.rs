//! HOOK-04 CI gate — scrubber_byte_identical.
//!
//! Verifies that the built-in secret scrubbers (ENV_KEY, AWS_KEY, GH_TOKEN,
//! SSH_KEY, JWT — plus the extended set: API_KEY, SLACK_TOKEN, GITLAB_PAT,
//! GCP_KEY, KV_SECRET, BEARER) produce deterministic, byte-identical output
//! across two independent runs against the known-secrets fixture.
//!
//! Also tests TOML overlay (custom patterns add on top, built-ins always run)
//! and TOML parse-failure fallback (warn log, no panic, built-ins continue).

use lunaris_hook::scrub::ScrubEngine;

// ── built-in tests ──────────────────────────────────────────────────────────

#[test]
fn test1_env_var_redaction() {
    let mut content = "MY_API_KEY=sk_live_abc123\n".to_string();
    let engine = ScrubEngine::new();
    let count = engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:ENV_KEY>"),
        "expected ENV_KEY redaction, got: {:?}",
        content
    );
    assert!(count > 0, "expected at least one redaction");
}

#[test]
fn test2_aws_key_redaction() {
    let mut content = "AKIAIOSFODNN7EXAMPLE".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:AWS_KEY>"),
        "expected AWS_KEY redaction, got: {:?}",
        content
    );
}

#[test]
fn test3_github_token_ghp() {
    let mut content = "ghp_16C7e42F292c6912E7710c838347Ae178B4a".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:GH_TOKEN>"),
        "expected GH_TOKEN redaction for ghp_, got: {:?}",
        content
    );
}

#[test]
fn test4_github_token_gho() {
    let mut content = "gho_16C7e42F292c6912E7710c838347Ae178B4a".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:GH_TOKEN>"),
        "expected GH_TOKEN redaction for gho_, got: {:?}",
        content
    );
}

#[test]
fn test5_github_token_ghu() {
    let mut content = "ghu_16C7e42F292c6912E7710c838347Ae178B4a".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:GH_TOKEN>"),
        "expected GH_TOKEN redaction for ghu_, got: {:?}",
        content
    );
}

#[test]
fn test6_ssh_private_key_header() {
    let mut content = "-----BEGIN RSA PRIVATE KEY-----".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:SSH_KEY>"),
        "expected SSH_KEY redaction, got: {:?}",
        content
    );
}

#[test]
fn test7_jwt_token() {
    let mut content =
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0ZXN0In0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
            .to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(content.contains("<REDACTED:JWT>"), "expected JWT redaction, got: {:?}", content);
}

#[test]
fn test8_byte_identical_across_two_runs() {
    let fixture = include_str!("fixtures/known_secrets.txt");

    let engine = ScrubEngine::new();

    let mut first = fixture.to_string();
    engine.apply(&mut first);

    let mut second = fixture.to_string();
    engine.apply(&mut second);

    assert_eq!(first, second, "scrubber output must be byte-identical across two independent runs");

    // Also assert all 5 kinds are redacted in the fixture output.
    assert!(first.contains("<REDACTED:ENV_KEY>"), "fixture missing ENV_KEY redaction");
    assert!(first.contains("<REDACTED:AWS_KEY>"), "fixture missing AWS_KEY redaction");
    assert!(first.contains("<REDACTED:GH_TOKEN>"), "fixture missing GH_TOKEN redaction");
    assert!(first.contains("<REDACTED:SSH_KEY>"), "fixture missing SSH_KEY redaction");
    assert!(first.contains("<REDACTED:JWT>"), "fixture missing JWT redaction");
}

// ── TOML overlay tests ───────────────────────────────────────────────────────

#[test]
fn test9_custom_toml_pattern_adds_on_top_of_builtins() {
    use std::io::Write;
    let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
    writeln!(
        tmpfile,
        r#"
[scrubbers.custom]
patterns = [
  {{ name = "int_secret", pattern = "INT_SECRET_[A-Z0-9]{{8}}", redact_as = "<REDACTED:INTERNAL>" }},
]
"#
    )
    .unwrap();

    let engine = ScrubEngine::from_toml_path(tmpfile.path());

    // Custom pattern fires.
    let mut custom_content = "INT_SECRET_ABCD1234".to_string();
    engine.apply(&mut custom_content);
    assert!(
        custom_content.contains("<REDACTED:INTERNAL>"),
        "custom TOML pattern did not fire, got: {:?}",
        custom_content
    );

    // Built-ins still fire.
    let mut aws_content = "AKIAIOSFODNN7EXAMPLE".to_string();
    engine.apply(&mut aws_content);
    assert!(
        aws_content.contains("<REDACTED:AWS_KEY>"),
        "built-in AWS_KEY pattern missing when custom TOML loaded, got: {:?}",
        aws_content
    );
}

#[test]
fn test10_no_toml_still_redacts_builtins() {
    // ScrubEngine::new() should redact all 5 kinds without any TOML.
    let engine = ScrubEngine::new();

    let cases = [
        ("MY_KEY=somevalue", "ENV_KEY"),
        ("AKIAIOSFODNN7EXAMPLE", "AWS_KEY"),
        ("ghp_16C7e42F292c6912E7710c838347Ae178B4a", "GH_TOKEN"),
        ("-----BEGIN EC PRIVATE KEY-----", "SSH_KEY"),
        ("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1In0.abc123def456ghi", "JWT"),
    ];

    for (secret, kind) in cases {
        let mut content = secret.to_string();
        engine.apply(&mut content);
        let expected = format!("<REDACTED:{}>", kind);
        assert!(
            content.contains(&expected),
            "built-in {} pattern missing without TOML, got: {:?}",
            kind,
            content
        );
    }
}

#[test]
fn test11_malformed_toml_fallback_no_panic() {
    use std::io::Write;
    let mut tmpfile = tempfile::NamedTempFile::new().unwrap();
    // Deliberately malformed TOML.
    writeln!(tmpfile, "not valid toml === [[[").unwrap();

    // Must not panic — falls back to built-ins only.
    let engine = ScrubEngine::from_toml_path(tmpfile.path());

    let mut content = "AKIAIOSFODNN7EXAMPLE".to_string();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:AWS_KEY>"),
        "built-in patterns must work after TOML parse failure, got: {:?}",
        content
    );
}

// ── extended built-ins (ADD task scrub-and-curation-hardening, FROZEN @ v1) ──
// 2026-07-14 deep-test finding #3: sk-ant/password=/xoxb/glpat/AIza/bearer
// secrets crossed the scrubber verbatim. One test per new kind + the two
// contracted rejects (benign words survive).

#[test]
fn test12_api_key_sk_ant() {
    let mut content = "export key sk-ant-api03-AbCd1234EfGh5678IjKl".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:API_KEY>"),
        "expected API_KEY redaction for sk-ant-, got: {:?}",
        content
    );
    assert!(!content.contains("sk-ant-"), "raw sk-ant key must not survive, got: {:?}", content);
}

#[test]
fn test13_api_key_sk_generic() {
    let mut content = "sk-proj-AbCdEfGh12345678IjKl".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:API_KEY>"),
        "expected API_KEY redaction for generic sk-, got: {:?}",
        content
    );
}

#[test]
fn test14_slack_token() {
    let mut content = "xoxb-FAKEFAKE00-Kp3fq9AbCdEf12345678".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:SLACK_TOKEN>"),
        "expected SLACK_TOKEN redaction, got: {:?}",
        content
    );
}

#[test]
fn test15_gitlab_pat() {
    let mut content = "glpat-AbCdEfGhIjKlMnOpQrSt".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:GITLAB_PAT>"),
        "expected GITLAB_PAT redaction, got: {:?}",
        content
    );
}

#[test]
fn test16_gcp_key() {
    let mut content = "AIzaSyA1bC2dE3fG4hI5jK6lM7nO8pQ9rS0tU1v".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(
        content.contains("<REDACTED:GCP_KEY>"),
        "expected GCP_KEY redaction, got: {:?}",
        content
    );
}

#[test]
fn test17_kv_secret_lowercase_and_json() {
    let engine = ScrubEngine::new();

    let mut assignment = "password=hunter22".to_string();
    engine.apply(&mut assignment);
    assert!(
        assignment.contains("<REDACTED:KV_SECRET>"),
        "expected KV_SECRET redaction for lowercase password=, got: {:?}",
        assignment
    );

    let mut json_form = r#""token": "abcd1234""#.to_string();
    engine.apply(&mut json_form);
    assert!(
        json_form.contains("<REDACTED:KV_SECRET>"),
        "expected KV_SECRET redaction for JSON token field, got: {:?}",
        json_form
    );
}

#[test]
fn test18_bearer_header() {
    let mut content = "Authorization: Bearer AbCdEf123456789012345".to_string();
    let engine = ScrubEngine::new();
    engine.apply(&mut content);
    assert!(content.contains("<REDACTED:BEARER>"), "expected BEARER redaction, got: {:?}", content);
}

#[test]
fn test19_benign_token_words_survive() {
    let engine = ScrubEngine::new();

    let mut config = "max_tokens: 4096".to_string();
    engine.apply(&mut config);
    assert_eq!(config, "max_tokens: 4096", "benign config word must survive");

    let mut prose = "the bearer of news arrived".to_string();
    engine.apply(&mut prose);
    assert_eq!(prose, "the bearer of news arrived", "bearer as prose must survive");
}
