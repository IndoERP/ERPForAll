// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    thread,
    time::Duration,
};

fn credentials(url: &str, limits: &str) -> Credentials {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("test.key");
    fs::write(&key, "test-only-token\n").unwrap();
    fs::set_permissions(key, fs::Permissions::from_mode(0o600)).unwrap();
    let table = format!("{limits}\n[provider.{PROVIDER}]\nkey_file='test.key'\nbase_url='{url}'");
    Credentials::load(Some(&table.parse().unwrap()), dir.path()).unwrap()
}

// Read the complete HTTP request (including the body) before the fixture writes a response.
fn fixture(response: String) -> (String, thread::JoinHandle<String>) {
    fixture_with(move |stream| stream.write_all(response.as_bytes()).unwrap())
}

fn fixture_with(
    write_reply: impl FnOnce(&mut std::net::TcpStream) + Send + 'static,
) -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let job = thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 4096];
        loop {
            let n = conn.read(&mut buf).unwrap();
            assert!(n > 0, "request ended before its declared length");
            request.extend_from_slice(&buf[..n]);
            let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..end])
                .unwrap()
                .to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            if request.len() >= end + 4 + length {
                break;
            }
        }
        write_reply(&mut conn);
        String::from_utf8(request).unwrap()
    });
    (endpoint, job)
}

#[tokio::test]
async fn authenticates_only_to_the_pinned_base_and_decodes_both_protocols() {
    for (family, body, suffix, auth) in [
        (
            "anthropic",
            ANTHROPIC_REPLY,
            "messages",
            "x-api-key: test-only-token",
        ),
        (
            "openai_compatible",
            OPENAI_REPLY,
            "chat/completions",
            "authorization: bearer test-only-token",
        ),
    ] {
        let raw = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (base, job) = fixture(raw);
        let mut route = route(family);
        route.base_url = base.clone();
        let keys = credentials(&base, "");
        let adapter = route.transport(&keys).unwrap();
        let result = adapter.complete_turn(&request(vec![user()])).await.unwrap();
        assert_eq!(result.model(), "example-model-snapshot");
        assert_eq!(result.usage().total_tokens, 19);
        let sent = job.join().unwrap();
        let (headers, body) = sent.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            headers.lines().next().unwrap(),
            format!("POST /v1/{suffix} HTTP/1.1")
        );
        assert!(headers.to_lowercase().lines().any(|line| line == auth));
        if family == "anthropic" {
            assert!(
                headers
                    .to_lowercase()
                    .lines()
                    .any(|line| line == "anthropic-version: 2023-06-01")
            );
        }
        assert_eq!(serde_json::from_str::<Value>(body).unwrap()["model"], MODEL);
    }
}

#[tokio::test]
async fn raw_error_body_and_location_never_escape_or_get_followed() {
    for (status, kind) in [(302, "unexpected HTTP redirection"), (500, "HTTP 500")] {
        let (base, job) = fixture(format!(
            "HTTP/1.1 {status} Fixture\r\nLocation: http://127.0.0.1:1/private\r\nContent-Length: 6\r\nConnection: close\r\n\r\nsecret"
        ));
        let mut route = route("openai_compatible");
        route.base_url = base.clone();
        let keys = credentials(&base, "");
        let err = route
            .transport(&keys)
            .unwrap()
            .complete_turn(&request(vec![user()]))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Unavailable { .. }));
        assert_eq!(
            err.to_string(),
            format!("provider '{PROVIDER}' service unavailable: {kind}")
        );
        let _sent = job.join().unwrap();
    }
}

#[test]
fn an_untrusted_address_or_privileged_caller_cannot_obtain_a_transport() {
    let mut selected = route("anthropic");
    let original = "https://provider.example.invalid/v1";
    let keys = credentials(original, "");
    for other in [
        "https://other.example.invalid/v1",
        "https://provider.example.invalid/other",
        "http://provider.example.invalid/v1",
    ] {
        selected.base_url = other.into();
        let error = selected.transport(&keys).err().unwrap();
        assert!(matches!(error, ProviderError::InvalidRequest { .. }));
        let reason = if other.starts_with("http:") {
            "unsupported provider endpoint"
        } else {
            "node credential is not bound to the selected provider endpoint"
        };
        assert_eq!(
            error.to_string(),
            format!("provider '{PROVIDER}' invalid request: {reason}")
        );
    }
    selected.base_url = original.into();
    selected.interactive = false;
    assert_eq!(
        selected.transport(&keys).err().unwrap().to_string(),
        format!(
            "provider '{PROVIDER}' invalid request: provider transport requires an unprivileged interactive user"
        )
    );
}

#[test]
fn literal_remote_destinations_require_public_addresses_and_clean_https_urls() {
    use crate::transport::trusted_base;
    for endpoint in [
        "https://10.0.0.4/v1",
        "https://100.64.0.4/v1",
        "https://169.254.169.254/v1",
        "https://192.0.2.4/v1",
        "https://127.0.0.1/v1",
        "https://[::ffff:127.0.0.1]/v1",
        "https://[fc00::1]/v1",
        "https://[fe80::1]/v1",
        "https://[2001:db8::1]/v1",
        "https://provider.example.invalid:0/v1",
        "https://operator@provider.example.invalid/v1",
        "https://provider.example.invalid/v1?token=fixture-marker",
        "https://provider.example.invalid/v1#fixture-marker",
    ] {
        assert!(trusted_base(endpoint).is_none());
    }
    for endpoint in [
        "https://93.184.215.14/v1",
        "https://[2606:4700:4700::1111]/v1",
        "https://provider.example.invalid/v1",
        "http://127.0.0.1:11434/v1",
        "http://[::1]:11434/v1",
    ] {
        assert!(trusted_base(endpoint).is_some());
    }
}

#[tokio::test]
async fn dns_resolved_loopback_is_refused_and_exchange_errors_are_redacted() {
    // A hostname must pass the checked resolver. The numeric loopback exception exists only
    // for an explicitly configured HTTP local provider.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut selected = route("openai_compatible");
    selected.base_url = format!(
        "https://localhost:{}/v1",
        listener.local_addr().unwrap().port()
    );
    let keys = credentials(&selected.base_url, "");
    let error = selected
        .transport(&keys)
        .unwrap()
        .complete_turn(&request(vec![user()]))
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::Unavailable { .. }));
    assert_eq!(
        error.to_string(),
        format!("provider '{PROVIDER}' service unavailable: could not complete the HTTP exchange")
    );
    let debug = format!("{keys:?} {error:?}");
    assert!(!debug.contains("test-only-token"));
    assert!(!debug.contains("localhost"));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
}

#[tokio::test]
async fn chunked_response_is_limited_even_without_a_content_length() {
    let (base, job) = fixture(format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n400\r\n{}\r\n1\r\ny\r\n0\r\n\r\n",
        "x".repeat(1024)
    ));
    let mut selected = route("openai_compatible");
    selected.base_url = base.clone();
    let keys = credentials(&base, "max_response_bytes=1024");
    let error = selected
        .transport(&keys)
        .unwrap()
        .complete_turn(&request(vec![user()]))
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::InvalidResponse { .. }));
    assert_eq!(
        error.to_string(),
        format!("provider '{PROVIDER}' invalid response: response exceeds configured byte limit")
    );
    let _sent = job.join().unwrap();
}

#[tokio::test]
async fn incomplete_response_after_headers_is_not_replayed() {
    let (base, job) = fixture(
        "HTTP/1.1 200 OK\r\nContent-Length: 500\r\nConnection: close\r\n\r\npartial".into(),
    );
    let mut selected = route("openai_compatible");
    selected.base_url = base.clone();
    let keys = credentials(&base, "");
    let error = selected
        .transport(&keys)
        .unwrap()
        .complete_turn(&request(vec![user()]))
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::Unavailable { .. }));
    assert_eq!(
        error.to_string(),
        format!("provider '{PROVIDER}' service unavailable: incomplete response body")
    );
    let _sent = job.join().unwrap();
}

#[tokio::test]
async fn deadline_also_covers_an_unfinished_response_body() {
    let (release, hold) = std::sync::mpsc::channel();
    let (base, job) = fixture_with(move |conn| {
        conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nx")
            .unwrap();
        // Keep the body incomplete until the caller has observed the deadline.
        hold.recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
    });
    let mut selected = route("openai_compatible");
    selected.base_url = base.clone();
    let keys = credentials(&base, "timeout_ms=250");
    let error = selected
        .transport(&keys)
        .unwrap()
        .complete_turn(&request(vec![user()]))
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::Unavailable { .. }));
    assert_eq!(
        error.to_string(),
        format!("provider '{PROVIDER}' service unavailable: request deadline expired")
    );
    release.send(()).unwrap();
    let _sent = job.join().unwrap();
}
