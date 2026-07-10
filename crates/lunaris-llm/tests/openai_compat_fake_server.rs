//! A3 acceptance (llama.cpp-only cutover) — the generic OpenAI-compatible
//! URL transport. Extractor/verifier go remote-only: cloud mux + ONE
//! `openai-compat` backend covering Ollama `/v1`, llama-server, vLLM, and
//! LM Studio.
//!
//! The discriminating proof is a fake local `/v1/chat/completions` server:
//! `CloudBackend` must round-trip a generation through an ARBITRARY base
//! URL, hit the exact joined path, and send NO `authorization` header when
//! the key is empty (local servers are typically unauthenticated).

#![cfg(feature = "cloud-api")]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::mpsc;

use lunaris_llm::{
    CloudBackend, CloudBackendOpts, CloudProvider, GenOpts, LlmBackend, SchemaConstraint,
};

/// One-shot fake OpenAI-compatible server: accepts a single connection,
/// captures the request head (request line + headers), replies with a
/// canned chat-completions body.
fn spawn_fake_server(response_content: &str) -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
    let port = listener.local_addr().expect("local addr").port();
    let (tx, rx) = mpsc::channel();
    let body = format!(r#"{{"choices":[{{"message":{{"content":"{response_content}"}}}}]}}"#);
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = stream.read(&mut tmp).expect("read request");
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                let content_length = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim())
                    })
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                if buf.len() - (pos + 4) >= content_length {
                    let _ = tx.send(head);
                    break;
                }
            }
            if n == 0 {
                return; // client hung up before completing the request
            }
        }
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).expect("write response");
    });
    (port, rx)
}

#[tokio::test]
async fn generate_round_trips_through_arbitrary_base_url_without_auth() {
    let (port, rx) = spawn_fake_server("hello from openai-compat");

    let backend = CloudBackend::new(CloudBackendOpts {
        provider: CloudProvider::OpenAiCompat,
        model: "qwen3:4b".into(),
        api_key: String::new(), // local servers need no key
        max_retries: 0,
        base_url: Some(format!("http://127.0.0.1:{port}/v1")),
    })
    .expect("openai-compat backend with empty key must construct");

    let out = backend
        .generate("say hi", SchemaConstraint::None, GenOpts::default())
        .await
        .expect("generate through the fake server");
    assert_eq!(out, "hello from openai-compat");

    let head = rx.recv().expect("server captured the request");
    let request_line = head.lines().next().unwrap_or_default();
    assert!(
        request_line.starts_with("POST /v1/chat/completions"),
        "must hit the joined base-URL path, got: {request_line}"
    );
    assert!(
        !head.lines().any(|l| l.to_ascii_lowercase().starts_with("authorization:")),
        "empty key must send NO authorization header, got:\n{head}"
    );
}

#[tokio::test]
async fn bearer_header_sent_when_key_is_configured() {
    let (port, rx) = spawn_fake_server("ok");

    let backend = CloudBackend::new(CloudBackendOpts {
        provider: CloudProvider::OpenAiCompat,
        model: "m".into(),
        api_key: "sk-local-test".into(),
        max_retries: 0,
        base_url: Some(format!("http://127.0.0.1:{port}/v1")),
    })
    .expect("construct");

    backend.generate("hi", SchemaConstraint::None, GenOpts::default()).await.expect("generate");

    let head = rx.recv().expect("captured");
    assert!(
        head.lines().any(|l| l.to_ascii_lowercase() == "authorization: bearer sk-local-test"),
        "configured key must arrive as a Bearer header, got:\n{head}"
    );
}
