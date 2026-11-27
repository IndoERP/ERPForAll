// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, reason = "controlled local issuer fixture")]
use super::*;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread;

fn key_set(kid: &str) -> String {
    json!({"keys":[
        {"kty":"RSA", "use":"enc", "alg":"RSA-OAEP","kid":"provider-encryption"},
        {"kty":"OKP","use":"sig","alg":"EdDSA","crv":"Ed25519","kid":kid,"x":URL_SAFE_NO_PAD.encode([5;32])}
    ]}).to_string()
}

struct Fixture {
    port: u16,
    response: Arc<StdMutex<String>>,
    requests: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = Arc::new(StdMutex::new(String::new()));
        let requests = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let (reply, calls, stop) = (response.clone(), requests.clone(), stopped.clone());
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                calls.fetch_add(1, Ordering::SeqCst);
                let _ = stream.write_all(reply.lock().unwrap().as_bytes());
            }
        });
        Self {
            port,
            response,
            requests,
            stopped,
            handle: Some(handle),
        }
    }
    fn url(&self) -> Url {
        Url::parse(&format!("http://127.0.0.1:{}/certs", self.port)).unwrap()
    }
    fn reply(&self, status: &str, body: &str) {
        *self.response.lock().unwrap() = format!(
            "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _wake = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
    }
}

#[tokio::test]
async fn remote_rotation_coalesces_unknown_kids_and_expires_last_good_keys() {
    let fixture = Fixture::new();
    let store = KeyStore::Remote(Box::new(RemoteKeys::new(
        fixture.url(),
        Duration::from_secs(30),
    )));
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    assert_eq!(store.diagnostics().await.status, "not_loaded");
    fixture.reply("200 OK", &key_set("first"));
    assert!(store.select(&http, "first").await.is_ok());
    assert_eq!(store.diagnostics().await.key_count, 1);

    fixture.reply("200 OK", &key_set("second"));
    let (a, b, c) = tokio::join!(
        store.select(&http, "second"),
        store.select(&http, "second"),
        store.select(&http, "second")
    );
    assert!(a.is_ok() && b.is_ok() && c.is_ok());
    assert!(store.select(&http, "first").await.is_err());
    assert!(store.select(&http, "unknown").await.is_err());
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 2);

    let KeyStore::Remote(remote) = &store else {
        unreachable!()
    };
    remote.cache.write().await.unknown_refresh = None;
    fixture.reply("503 Unavailable", "");
    assert!(store.select(&http, "third").await.is_err());
    assert!(store.select(&http, "second").await.is_ok());
    assert_eq!(store.diagnostics().await.status, "refresh_failed");
    remote.cache.write().await.success = Some(Instant::now() - Duration::from_secs(31));
    assert!(store.select(&http, "second").await.is_err());
    assert_eq!(store.diagnostics().await.status, "expired");
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 3);

    fixture.reply("200 OK", &key_set("third"));
    remote.cache.write().await.last_failure = Some(Instant::now() - COOLDOWN);
    let (a, b) = tokio::join!(store.select(&http, "third"), store.select(&http, "third"));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 4);
    let diag = store.diagnostics().await;
    assert_eq!(diag.status, "ready");
    assert!(diag.last_success_unix.is_some() && diag.last_attempt_unix.is_some());
}

#[test]
fn key_endpoint_and_policy_are_pinned_and_bounded() {
    let issuer = Url::parse("https://accounts.example.test/erp").unwrap();
    let new = |url, ttl_seconds| KeyStore::new(&SigningKeys::Remote { url, ttl_seconds }, &issuer);
    assert!(new("https://accounts.example.test/certs", 30).is_ok());
    let long_path = format!("https://accounts.example.test/{}", "a".repeat(2048));
    assert!(new(&long_path, 300).is_err());
    for (url, ttl) in [
        ("http://accounts.example.test/certs", 300),
        ("https://different.example.test/certs", 300),
        ("https://accounts.example.test:8443/certs", 300),
        ("https://accounts.example.test/certs?redirect", 300),
        ("https://accounts.example.test/certs", 29),
        ("https://accounts.example.test/certs", 3601),
    ] {
        assert!(new(url, ttl).is_err());
    }
    let keys: serde_json::Value = serde_json::from_str(&key_set("one")).unwrap();
    let key = &keys["keys"][1];
    assert!(parse_keys(json!({"keys":[key,key]}).to_string().as_bytes()).is_err());
    assert!(parse_keys(&vec![b' '; MAX_RESPONSE + 1]).is_err());
}

#[tokio::test]
async fn key_refresh_refuses_redirects_and_excessive_bodies() {
    let fixture = Fixture::new();
    let store = KeyStore::Remote(Box::new(RemoteKeys::new(
        fixture.url(),
        Duration::from_secs(60),
    )));
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let KeyStore::Remote(remote) = &store else {
        unreachable!()
    };
    *fixture.response.lock().unwrap() = format!(
        "HTTP/1.1 302 Found\r\nLocation: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        fixture.url()
    );
    assert!(store.select(&http, "test").await.is_err());
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);
    remote.cache.write().await.last_failure = Some(Instant::now() - COOLDOWN);
    fixture.reply("200 OK", &" ".repeat(MAX_RESPONSE + 1));
    assert!(store.select(&http, "test").await.is_err());
    assert_eq!(store.diagnostics().await.key_count, 0);
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 2);
}
