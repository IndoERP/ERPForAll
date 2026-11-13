//! hyper 1.12 with the buffer-provider hook (ERPForAll patch, perf-serve §4.1):
//! the patch's behaviour tests and its measurements against the same services as the stock
//! figures (perf-serve §12). Run under nextest (one process per test).
//!
//! Behaviour, under hyper's default write strategy, Flatten (`writev(false)`) and
//! `pipeline_flush(true)`: an idle connection holds no buffer; a pipelined second request still
//! parses, also when a body read carries the start of the next head; a buffer is never given
//! back with bytes in it; heads larger than a lent buffer still parse; a large body is read into
//! lent buffers directly, not through the stack buffer.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support_hyper;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use erp_testkit::serve::{self, RawClient, RequestMethod, ResponseMeta};
use support_hyper::{
    GET_STATIC, HTTP_BLOCKS_CEILING, IDLE_TARGET_BYTES, RESPONSE_CAPACITY, Service, Setup,
    TestPool, echo_request_with, per_idle, per_request, start,
};

/// Bytes a measured figure may move before the pinned value must be updated: one small block.
const BYTES_MARGIN: f64 = 64.0;

/// What was measured, per platform (review F4: the tests pin the measured cost, so a change in
/// either direction is seen and recorded; the G6 ceiling is asserted as well).
/// `(blocks,
/// bytes)` per request.
fn measured_per_request(setup: &Setup) -> Option<(u64, f64)> {
    let linux = match (setup.service, setup.timer) {
        (Service::Static4k, false) => (2, 656.0),
        (Service::Static4k, true) => (3, 768.0),
        (Service::JsonEcho, false) => (4, 1_008.0),
        (Service::JsonEcho, true) => (5, 1_120.0),
    };
    let macos = match (setup.service, setup.timer) {
        (Service::Static4k, false) => (2, 656.0),
        (Service::Static4k, true) => (3, 768.0),
        (Service::JsonEcho, false) => (5, 1_080.0),
        (Service::JsonEcho, true) => (6, 1_192.0),
    };
    on_platform(linux, macos)
}

/// Heap per idle connection, measured as above; `None` for a setup or platform not measured.
fn measured_idle_bytes(setup: &Setup) -> Option<f64> {
    let linux = match (setup.service, setup.timer) {
        (Service::Static4k, false) => Some(1_776.0),
        (Service::JsonEcho, true) => Some(2_696.0),
        _ => None,
    };
    let macos = match (setup.service, setup.timer) {
        (Service::Static4k, false) => Some(1_840.0),
        (Service::JsonEcho, true) => Some(2_760.0),
        _ => None,
    };
    on_platform(linux, macos).flatten()
}

/// The value for the platform the test runs on; `None` on a platform without measurements.
fn on_platform<T>(linux: T, macos: T) -> Option<T> {
    if cfg!(target_os = "linux") {
        Some(linux)
    } else if cfg!(target_os = "macos") {
        Some(macos)
    } else {
        None
    }
}

/// The measured cost, exactly in blocks and within [`BYTES_MARGIN`] in bytes, and within G6's
/// HTTP ceiling.
fn assert_measured_per_request(setup: &Setup) {
    let cost = per_request(setup);
    let blocks = cost
        .exact_blocks_per_request()
        .expect("the same cost for every request");
    let bytes = cost.bytes_per_request();
    let (want_blocks, want_bytes) = measured_per_request(setup)
        .expect("no measured values for this platform: measure it and record them here");
    assert_eq!(
        blocks,
        want_blocks,
        "{}: blocks per request moved from the measured {want_blocks}; record the new figure",
        setup.label()
    );
    assert!(
        (bytes - want_bytes).abs() <= BYTES_MARGIN,
        "{}: {bytes} B per request, measured {want_bytes} B (margin {BYTES_MARGIN} B)",
        setup.label()
    );
    assert!(
        blocks <= HTTP_BLOCKS_CEILING,
        "{blocks} blocks per request over the HTTP ceiling of {HTTP_BLOCKS_CEILING}"
    );
}

/// The measured idle heap within [`BYTES_MARGIN`], and within the idle target of G6 item 3 (4 KiB;
/// the gate is 16 KiB).
fn assert_measured_idle(setup: &Setup) {
    let idle = per_idle(setup);
    let bytes = idle.bytes_per_connection();
    let want = measured_idle_bytes(setup).expect(
        "no idle measurement recorded for this setup and platform: measure it, record it here",
    );
    assert!(
        (bytes - want).abs() <= BYTES_MARGIN,
        "{}: {bytes} B per idle connection, measured {want} B (margin {BYTES_MARGIN} B)",
        setup.label()
    );
    assert!(
        bytes <= IDLE_TARGET_BYTES,
        "{bytes} B per idle connection over the {IDLE_TARGET_BYTES} B target"
    );
}

#[global_allocator]
static ALLOC: erp_testkit::alloc::Alloc = erp_testkit::alloc::Alloc;

erp_testkit::alloc::harness!(
    an_idle_connection_holds_no_buffer,
    a_pipelined_second_request_still_parses,
    a_body_read_that_carries_the_next_head_parses,
    heads_and_bodies_larger_than_a_lent_buffer_parse,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    a_large_body_is_read_into_lent_buffers_directly,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    provider_hyper_static_4k_per_request,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    provider_hyper_static_4k_per_request_with_timer,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    provider_hyper_json_echo_per_request,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    provider_hyper_json_echo_per_request_with_timer,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    provider_hyper_static_4k_idle_connection,
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    provider_hyper_json_echo_idle_connection_with_timer,
    provider_hyper_services_return_bodies,
    a_large_body_is_echoed_and_buffers_returned,
);

/// How long a test's client waits for a response before it fails (a hang fails the test instead
/// of stalling CI).
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a test waits for the server to have read what was written.
const SERVER_WAIT: Duration = Duration::from_secs(5);

/// Wait until `done` holds, failing after [`SERVER_WAIT`] with `what`.
fn wait_until(label: &str, what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < SERVER_WAIT,
            "{label}: {what} did not happen within {SERVER_WAIT:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn pool_of(setup: &Setup) -> Arc<TestPool> {
    setup.provider.clone().expect("a provider setup")
}

/// The write strategies every behaviour test runs under, each with its own pool: hyper's default
/// (vectored writes, Queue), Flatten (`writev(false)`: bodies are copied into the lent head
/// buffer) and `pipeline_flush(true)` (the flush waits while pipelined requests are buffered).
fn variants(service: Service, timer: bool) -> [Setup; 3] {
    [
        Setup::lent(service, timer),
        Setup::lent(service, timer).flattened(),
        Setup::lent(service, timer).with_pipeline_flush(),
    ]
}

/// Read `count` responses from `stream`, in order.
fn read_responses(stream: &mut TcpStream, count: usize) -> Vec<(ResponseMeta, Vec<u8>)> {
    let mut received = Vec::new();
    let mut out = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    while out.len() < count {
        while let Some(meta) =
            serve::frame_response(&received, RequestMethod::Other).expect("frames")
        {
            let wire: Vec<u8> = received.drain(..meta.wire_len).collect();
            let body = wire[meta.head_len..].to_vec();
            out.push((meta, body));
            if out.len() == count {
                break;
            }
        }
        if out.len() == count {
            break;
        }
        let n = stream.read(&mut chunk).expect("read");
        assert!(n > 0, "connection closed after {} responses", out.len());
        received.extend_from_slice(&chunk[..n]);
    }
    assert!(
        received.is_empty(),
        "{} bytes after the last response",
        received.len()
    );
    out
}

/// After its response is flushed, a keep-alive connection gives both buffers back: with one
/// connection after many requests, and with eight connections idle at once.
fn an_idle_connection_holds_no_buffer() {
    for setup in [Service::Static4k, Service::JsonEcho]
        .into_iter()
        .flat_map(|service| variants(service, true))
    {
        let service = setup.service;
        let label = setup.label();
        let pool = pool_of(&setup);
        let probe = start(&setup);
        let mut client = RawClient::connect(
            probe.addr(),
            support_hyper::request_for(service),
            RESPONSE_CAPACITY,
        )
        .expect("client");
        for _ in 0..50 {
            let response = client.send().expect("response");
            assert_eq!(response.status(), 200);
        }
        probe.settle().expect("settles");
        assert_eq!(
            pool.held(),
            (0, 0),
            "{label}: buffers held by an idle connection"
        );
        let mut more: Vec<RawClient> = (0..8)
            .map(|_| {
                RawClient::connect(
                    probe.addr(),
                    support_hyper::request_for(service),
                    RESPONSE_CAPACITY,
                )
                .expect("client")
            })
            .collect();
        for other in &mut more {
            assert_eq!(other.send().expect("response").status(), 200);
        }
        probe.settle().expect("settles");
        assert_eq!(probe.connections().open(), 9);
        assert_eq!(
            pool.held(),
            (0, 0),
            "{label}: buffers held by nine idle connections"
        );
        assert_eq!(pool.nonempty(), 0, "{label}");
        drop((client, more));
        probe.stop().expect("server stops cleanly");
    }
}

/// Two requests written in one segment are both answered, in order, with the right bodies; so
/// are a request whose body arrives in a second write and a chunked request body.
fn a_pipelined_second_request_still_parses() {
    for setup in variants(Service::JsonEcho, true) {
        pipelined_requests_parse(&setup);
    }
}

fn pipelined_requests_parse(setup: &Setup) {
    let label = setup.label();
    let pool = pool_of(setup);
    let probe = start(setup);
    let mut stream = TcpStream::connect(probe.addr()).expect("connect");
    stream.set_nodelay(true).expect("nodelay");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("read timeout");
    // two keep-alive requests in one write
    let mut both = echo_request_with(b'A');
    both.extend_from_slice(&echo_request_with(b'B'));
    stream.write_all(&both).expect("write");
    let answers = read_responses(&mut stream, 2);
    for ((meta, body), fill) in answers.iter().zip(*b"AB") {
        assert_eq!(meta.status, 200);
        assert_eq!(body.len(), 2048);
        assert_eq!(body[100], fill, "{label}: responses out of order");
    }
    // a body that arrives after the head, in a separate write
    let split = echo_request_with(b'C');
    let head_end = split
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("head")
        + 4;
    stream.write_all(&split[..head_end]).expect("head");
    std::thread::sleep(std::time::Duration::from_millis(20));
    stream.write_all(&split[head_end..]).expect("body");
    let answer = read_responses(&mut stream, 1);
    assert_eq!(answer[0].1[100], b'C');
    // a chunked request body, then a pipelined GET-like echo behind it
    let mut chunked = b"POST /echo HTTP/1.1\r\nhost: bench\r\ntransfer-encoding: chunked\r\n\r\n\
        5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"
        .to_vec();
    chunked.extend_from_slice(&echo_request_with(b'D'));
    stream.write_all(&chunked).expect("write");
    let answers = read_responses(&mut stream, 2);
    assert_eq!(answers[0].1, b"hello world", "{label}");
    assert_eq!(answers[1].1[100], b'D', "{label}");
    probe.settle().expect("settles");
    assert_eq!(pool.held(), (0, 0), "{label}");
    assert_eq!(
        pool.nonempty(),
        0,
        "{label}: a buffer came back with bytes in it"
    );
    drop(stream);
    probe.stop().expect("server stops cleanly");
}

/// Review D4: a body read into a lent buffer that also carries the start of the next pipelined
/// head. The head arrives alone (the body read then waits on an empty buffer); the body arrives
/// together with the first half of the next request's head, so the bytes after the body stay in
/// the lent buffer while the first answer is produced; the rest of the second request follows.
/// Both are answered in order and every buffer is back with the pool afterwards.
///
/// Review R2-1: the server's reads are counted, so the shape is proven rather than hoped for
/// under load: the first read is the head alone, the second is exactly the body and the half
/// head (one read), and the rest is written only after it.
fn a_body_read_that_carries_the_next_head_parses() {
    for setup in variants(Service::JsonEcho, true) {
        body_then_next_head(&setup.counting_reads());
    }
}

fn head_end(request: &[u8]) -> usize {
    request
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a head")
        + 4
}

fn body_then_next_head(setup: &Setup) {
    let label = setup.label();
    let pool = pool_of(setup);
    let counter = setup.reads.clone().expect("a counting setup");
    // waits are on bytes, so a split read reaches the exact assertions below and is named there
    let bytes = || counter.bytes.load(Ordering::Acquire);
    let probe = start(setup);
    let mut stream = TcpStream::connect(probe.addr()).expect("connect");
    stream.set_nodelay(true).expect("nodelay");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("read timeout");
    let first = echo_request_with(b'E');
    let second = echo_request_with(b'F');
    let first_head = head_end(&first);
    let half = head_end(&second) / 2;
    stream.write_all(&first[..first_head]).expect("first head");
    let head_bytes = first_head as u64;
    wait_until(&label, "the server reading the first head", || {
        bytes() >= head_bytes
    });
    let (reads, read, largest) = counter.snapshot();
    assert_eq!(
        (reads, read, largest),
        (1, head_bytes, head_bytes),
        "{label}: the first head ({head_bytes} B) arrived in {reads} reads, largest {largest} B, \
         {read} B read"
    );
    let mut body_and_half_head = first[first_head..].to_vec();
    body_and_half_head.extend_from_slice(&second[..half]);
    stream
        .write_all(&body_and_half_head)
        .expect("body and half a head");
    let carried = body_and_half_head.len() as u64;
    assert_eq!(carried, 2048 + half as u64);
    wait_until(
        &label,
        "the server reading the body and half a head",
        || bytes() >= head_bytes + carried,
    );
    let (reads, read, largest) = counter.snapshot();
    assert_eq!(
        (reads - 1, read - head_bytes, largest),
        (1, carried, carried),
        "{label}: the body and the next head's first half ({carried} B) arrived in {} reads, \
         largest {largest} B, {} B read",
        reads - 1,
        read - head_bytes
    );
    stream.write_all(&second[half..]).expect("the rest");
    let answers = read_responses(&mut stream, 2);
    for ((meta, body), fill) in answers.iter().zip(*b"EF") {
        assert_eq!(meta.status, 200, "{label}");
        assert_eq!(body.len(), 2048, "{label}");
        assert_eq!(body[100], fill, "{label}: wrong body or order");
    }
    probe.settle().expect("settles");
    assert_eq!(
        pool.held(),
        (0, 0),
        "{label}: buffers held after both answers"
    );
    assert_eq!(pool.nonempty(), 0, "{label}");
    drop(stream);
    probe.stop().expect("server stops cleanly");
}

/// A request head larger than a lent buffer (12 KiB of headers) grows the buffer and parses; a
/// 64 KiB body is echoed whole. No buffer comes back with bytes in it.
fn heads_and_bodies_larger_than_a_lent_buffer_parse() {
    for setup in variants(Service::JsonEcho, false) {
        large_heads_and_bodies_parse(&setup);
    }
}

fn large_heads_and_bodies_parse(setup: &Setup) {
    let label = setup.label();
    let pool = pool_of(setup);
    let probe = start(setup);
    let mut stream = TcpStream::connect(probe.addr()).expect("connect");
    let big_header = "v".repeat(12 * 1024);
    let body = vec![b'z'; 64 * 1024];
    let mut request = format!(
        "POST /echo HTTP/1.1\r\nhost: bench\r\nx-big: {big_header}\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    stream.write_all(&request).expect("write");
    let answer = read_responses(&mut stream, 1);
    assert_eq!(answer[0].0.status, 200, "{label}");
    assert!(
        answer[0].1 == body,
        "{label}: the 64 KiB body was not echoed whole"
    );
    // and a normal request after it on the same connection
    stream.write_all(GET_STATIC).expect("write");
    let answer = read_responses(&mut stream, 1);
    assert_eq!((answer[0].0.status, answer[0].1.len()), (200, 0), "{label}");
    probe.settle().expect("settles");
    assert_eq!(pool.held(), (0, 0), "{label}");
    assert_eq!(pool.nonempty(), 0, "{label}");
    drop(stream);
    probe.stop().expect("server stops cleanly");
}

/// One request with a 1 MiB body, through `setup`: `(server reads, largest read, read buffers
/// taken)`.
fn large_body_reads(setup: &Setup) -> (u64, u64, u64) {
    const BODY: usize = 1024 * 1024;
    let counter = setup.reads.clone().expect("a counting setup");
    let probe = start(setup);
    let mut stream = TcpStream::connect(probe.addr()).expect("connect");
    let body = vec![b'q'; BODY];
    let mut request =
        format!("POST /echo HTTP/1.1\r\nhost: bench\r\ncontent-length: {BODY}\r\n\r\n")
            .into_bytes();
    request.extend_from_slice(&body);
    stream.write_all(&request).expect("write");
    let answer = read_responses(&mut stream, 1);
    assert_eq!(answer[0].0.status, 200);
    assert!(answer[0].1 == body, "the 1 MiB body was not echoed whole");
    probe.settle().expect("settles");
    let (reads, bytes, largest) = counter.snapshot();
    assert_eq!(bytes, request.len() as u64, "every byte read once");
    let takes = setup.provider.as_ref().map_or(0, |pool| {
        assert_eq!(pool.held(), (0, 0));
        assert_eq!(pool.nonempty(), 0);
        pool.read_takes.load(std::sync::atomic::Ordering::Acquire)
    });
    drop(stream);
    probe.stop().expect("server stops cleanly");
    eprintln!(
        "SERVE-HARNESS-RESULT large_body {}: 1 MiB body, server reads {reads}, largest read \
         {largest} B, read buffers taken {takes}",
        setup.label()
    );
    (reads, largest, takes)
}

/// Review F2: body reads go straight into a lent buffer of hyper's adaptive read size, as stock
/// hyper reads into its own buffer; only reads that wait for a head go through the 8 KiB stack
/// buffer. Before the fix every body read went through the stack buffer once the read buffer
/// emptied: at most 8 KiB per read and one buffer taken (and 8 KiB copied) per read, so at least
/// 128 reads and 128 takes for 1 MiB, and the adaptive size never grew past 16 KiB.
fn a_large_body_is_read_into_lent_buffers_directly() {
    let (stock_reads, stock_largest, _) =
        large_body_reads(&Setup::stock(Service::JsonEcho, false).counting_reads());
    let (reads, largest, takes) =
        large_body_reads(&Setup::lent(Service::JsonEcho, false).counting_reads());
    assert!(
        largest > 8192,
        "no read larger than the 8 KiB stack buffer ({largest} B): body reads still go through it"
    );
    assert!(
        takes < 64,
        "{takes} read buffers taken for 1 MiB: body reads take a buffer per stack read"
    );
    eprintln!(
        "SERVE-HARNESS-RESULT large_body comparison: stock {stock_reads} reads (largest \
         {stock_largest} B), provider {reads} reads (largest {largest} B), {takes} takes"
    );
}

fn provider_hyper_static_4k_per_request() {
    assert_measured_per_request(&Setup::lent(Service::Static4k, false));
}

fn provider_hyper_static_4k_per_request_with_timer() {
    assert_measured_per_request(&Setup::lent(Service::Static4k, true));
}

fn provider_hyper_json_echo_per_request() {
    assert_measured_per_request(&Setup::lent(Service::JsonEcho, false));
}

fn provider_hyper_json_echo_per_request_with_timer() {
    assert_measured_per_request(&Setup::lent(Service::JsonEcho, true));
}

fn provider_hyper_static_4k_idle_connection() {
    assert_measured_idle(&Setup::lent(Service::Static4k, false));
}

fn provider_hyper_json_echo_idle_connection_with_timer() {
    assert_measured_idle(&Setup::lent(Service::JsonEcho, true));
}

fn provider_hyper_services_return_bodies() {
    for service in [Service::Static4k, Service::JsonEcho] {
        for timer in [false, true] {
            support_hyper::assert_service_response(&Setup::lent(service, timer));
        }
    }
}

fn a_large_body_is_echoed_and_buffers_returned() {
    for setup in [
        Setup::stock(Service::JsonEcho, false),
        Setup::lent(Service::JsonEcho, false),
    ] {
        let probe = start(&setup);
        let mut stream = TcpStream::connect(probe.addr()).expect("connect");
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .expect("read timeout");
        let body = vec![b'q'; 1024 * 1024];
        let mut request = format!(
            "POST /echo HTTP/1.1\r\nhost: bench\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(&body);
        stream.write_all(&request).expect("write");
        let answer = read_responses(&mut stream, 1);
        assert_eq!(answer[0].0.status, 200);
        assert_eq!(answer[0].1, body);
        probe.settle().expect("settles");
        if let Some(pool) = &setup.provider {
            assert_eq!(pool.held(), (0, 0));
            assert_eq!(pool.nonempty(), 0);
        }
        drop(stream);
        probe.stop().expect("server stops cleanly");
    }
}
