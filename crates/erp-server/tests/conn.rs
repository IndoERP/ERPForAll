#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
//! HTTP/1 connection limits (PERF.SERVER-IDLE-KEEPALIVE): the header
//! deadline runs from a request's first byte, an idle keep-alive connection lives until the idle limit,
//! and an old connection drains gracefully. Real sockets and real time; limits are the smallest the
//! type accepts (header 1 s, idle 5 s, age 10 s).
use axum::{Router, routing::get};
use erp_server::conn::{ConnectionLimits, serve};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::sleep,
};

async fn server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let limits = ConnectionLimits::new(1, 5, 10).unwrap();
    let router = Router::new().route("/", get(|| async { "ok" })).route(
        "/slow",
        get(|| async {
            sleep(Duration::from_secs(2)).await;
            "slow"
        }),
    );
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(serve(socket, router.clone(), limits));
        }
    });
    address
}

/// Read one response; `None` when the server closed the connection before a full response.
async fn response(stream: &mut TcpStream) -> Option<String> {
    let mut data = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let text = String::from_utf8_lossy(&data).into_owned();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .map(|value| value.trim().parse::<usize>().unwrap());
            if length.is_some_and(|length| body.len() >= length) {
                return Some(text);
            }
        }
        match tokio::time::timeout(Duration::from_secs(20), stream.read(&mut chunk)).await {
            Ok(Ok(0) | Err(_)) => return None,
            Ok(Ok(read)) => data.extend_from_slice(&chunk[..read]),
            Err(_) => panic!("no response or close within 20 s"),
        }
    }
}

async fn get_on(stream: &mut TcpStream, path: &str) -> Option<String> {
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return None;
    }
    response(stream).await
}

/// Wait for the server to close an idle connection; returns how long that took.
async fn closed_after(stream: &mut TcpStream) -> Duration {
    let start = Instant::now();
    let mut byte = [0_u8; 1];
    match tokio::time::timeout(Duration::from_secs(20), stream.read(&mut byte)).await {
        Ok(Ok(0) | Err(_)) => start.elapsed(),
        Ok(Ok(_)) => panic!("unexpected data on an idle connection"),
        Err(_) => panic!("connection still open after 20 s"),
    }
}

#[tokio::test]
async fn an_idle_connection_is_reused_after_the_header_deadline() {
    let address = server().await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    assert!(get_on(&mut stream, "/").await.unwrap().ends_with("ok"));
    // Longer than the 1 s header deadline, shorter than the 5 s idle limit.
    sleep(Duration::from_millis(2500)).await;
    let second = get_on(&mut stream, "/").await;
    assert!(
        second.is_some_and(|text| text.ends_with("ok")),
        "the idle connection was closed"
    );
}

#[tokio::test]
async fn the_header_deadline_restarts_with_each_request() {
    let address = server().await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    assert!(get_on(&mut stream, "/").await.is_some());
    sleep(Duration::from_millis(2000)).await;
    // A head sent in two parts within the deadline, after an idle gap longer than the deadline.
    stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
    sleep(Duration::from_millis(500)).await;
    stream.write_all(b"Host: test\r\n\r\n").await.unwrap();
    assert!(
        response(&mut stream)
            .await
            .is_some_and(|text| text.ends_with("ok"))
    );
}

#[tokio::test]
async fn a_slow_request_head_is_closed_at_the_header_deadline() {
    let address = server().await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\n").await.unwrap();
    let waited = closed_after(&mut stream).await;
    assert!(
        waited < Duration::from_millis(3000),
        "closed after {waited:?}"
    );
}

#[tokio::test]
async fn a_slow_handler_is_not_cut_by_the_header_deadline() {
    let address = server().await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    let text = get_on(&mut stream, "/slow").await;
    assert!(text.is_some_and(|text| text.ends_with("slow")));
    assert!(
        get_on(&mut stream, "/").await.is_some(),
        "the connection did not survive a slow handler"
    );
}

#[tokio::test]
async fn an_idle_connection_closes_at_the_idle_limit() {
    let address = server().await;
    let mut stream = TcpStream::connect(address).await.unwrap();
    assert!(get_on(&mut stream, "/").await.is_some());
    let waited = closed_after(&mut stream).await;
    assert!(
        waited >= Duration::from_millis(4500) && waited < Duration::from_millis(8000),
        "closed after {waited:?}"
    );
}

#[tokio::test]
async fn an_old_connection_finishes_its_request_and_then_closes() {
    let address = server().await;
    let start = Instant::now();
    let mut stream = TcpStream::connect(address).await.unwrap();
    // Keep the connection busy until just before the 10 s age, then start a 2 s request across it.
    while start.elapsed() < Duration::from_millis(8500) {
        assert!(get_on(&mut stream, "/").await.is_some());
        sleep(Duration::from_millis(1500)).await;
    }
    let text = get_on(&mut stream, "/slow").await;
    assert!(
        text.is_some_and(|text| text.ends_with("slow")),
        "the in-flight request was cut"
    );
    let waited = closed_after(&mut stream).await;
    assert!(
        waited < Duration::from_millis(3000),
        "closed {waited:?} after the drained request"
    );
}

/// The limits type refuses values outside its ranges and an order other than header < idle < age.
#[test]
fn limits_are_validated() {
    assert!(ConnectionLimits::new(5, 30, 120).is_ok());
    for (header, idle, age) in [
        (0, 30, 120),
        (61, 600, 3600),
        (5, 4, 120),
        (5, 601, 3600),
        (5, 30, 9),
        (5, 30, 3601),
        (30, 30, 120),
        (5, 120, 120),
    ] {
        assert!(
            ConnectionLimits::new(header, idle, age).is_err(),
            "{header}/{idle}/{age}"
        );
    }
    let defaults = ConnectionLimits::default();
    assert_eq!(
        (
            defaults.header_deadline(),
            defaults.idle(),
            defaults.max_age()
        ),
        (
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(120)
        )
    );
}

/// Server shutdown drains the active request and immediately closes idle keep-alive connections.
#[tokio::test]
async fn shutdown_finishes_active_request_and_closes_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(1);
    let (finish_tx, finish_rx) = tokio::sync::watch::channel(false);
    let router = Router::new().route(
        "/",
        get(move || {
            let started = started_tx.clone();
            let mut finish = finish_rx.clone();
            async move {
                started.send(()).await.unwrap();
                finish.wait_for(|done| *done).await.unwrap();
                "drained"
            }
        }),
    );
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        erp_server::conn::serve_until(socket, router, ConnectionLimits::default(), async {
            stop_rx.await.unwrap();
        })
        .await;
    });
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
        .await
        .unwrap()
        .unwrap();
    stop_tx.send(()).unwrap();
    finish_tx.send(true).unwrap();
    assert!(response(&mut stream).await.unwrap().ends_with("drained"));
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
}

#[tokio::test]
async fn shutdown_closes_an_idle_keep_alive_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        erp_server::conn::serve_until(
            socket,
            Router::new().route("/", get(|| async { "ok" })),
            ConnectionLimits::default(),
            async {
                stop_rx.await.unwrap();
            },
        )
        .await;
    });
    let mut stream = TcpStream::connect(address).await.unwrap();
    assert!(get_on(&mut stream, "/").await.unwrap().ends_with("ok"));
    stop_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    let mut byte = [0];
    assert_eq!(stream.read(&mut byte).await.unwrap(), 0);
}
