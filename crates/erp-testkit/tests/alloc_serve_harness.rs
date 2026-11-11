//! Self-tests of the served-path harness (PERF.SERVE-HARNESS, perf-serve §3 "Exit"): the client
//! probe, a known allocation of 1 block per request detected exactly, an idle connection holding a
//! known 8 KiB detected within 1 %, an off-path task allocating 1 block counted after the
//! quiescence hook, the result-size rule, and the errors that keep a measurement from passing
//! without counting.
//!
//! Each test measures the whole process: run under nextest (one process per test).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use erp_testkit::serve::{
    self, C_ROW_GATE, ConnGauge, DEFAULT_WARMUP, GaugedIo, OffPath, Quiescence, RawClient,
    ResultSizeRule, ServeContext, ServeError, ServeProbe,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;

#[global_allocator]
static ALLOC: erp_testkit::alloc::Alloc = erp_testkit::alloc::Alloc;

erp_testkit::alloc::harness!(
    client_probe_allocates_nothing,
    a_known_block_per_request_is_detected_exactly,
    an_idle_connection_holding_8_kib_is_detected_within_1_percent,
    an_off_path_block_is_counted_after_the_quiescence_hook,
    the_result_size_rule_detects_per_row_allocations,
    an_ungauged_server_fails_the_idle_measurement,
    an_ungauged_server_fails_the_request_measurements,
    a_server_gauging_one_of_two_connections_fails,
    a_server_gauging_a_connection_twice_fails,
    a_server_closing_the_connection_fails_the_window,
    a_server_closing_the_connection_fails_the_warm_up,
    an_error_status_fails_the_measurement,
    an_expected_refusal_status_is_measured,
    a_failing_server_is_reported,
    the_gauge_tracks_idle_connections,
    a_connection_closed_mid_response_is_an_error
);

const GET: &[u8] = b"GET / HTTP/1.1\r\nhost: harness\r\n\r\n";
const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
const FAIL: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n";
const OK_CLOSE: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok";
const CANNED_LENGTH: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 11\r\nx-a: b\r\n\r\nhello world";
const CANNED_CHUNKED: &[u8] =
    b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";

/// What the test server does besides answering.
#[derive(Debug, Clone, Copy, Default)]
struct Mode {
    /// Blocks allocated (and freed) per request.
    per_request_blocks: usize,
    /// Blocks per "row": one row per `r` in the request path (`GET /rrr`).
    per_row_blocks: usize,
    /// Bytes each connection holds for its whole life, idle included.
    idle_hold: usize,
    /// Off-path work after each response: a background task allocates one block after a delay,
    /// under a quiescence guard taken before the response is written.
    off_path: bool,
    /// Wrap connections in `GaugedIo` (off: a server the idle measurement cannot read).
    ungauged: bool,
    /// Wrap only the first accepted connection in `GaugedIo` (1 of 2 gauged).
    gauge_first_only: bool,
    /// Wrap every connection in `GaugedIo` twice (each counted as two).
    gauge_twice: bool,
    /// Answer with `Connection: close` and close after the response.
    close: bool,
    /// Answer 500.
    fail: bool,
}

/// Off-path delay: long enough that a window ending at the response would miss the allocation.
const OFF_PATH_DELAY: Duration = Duration::from_millis(20);

fn start(mode: Mode) -> ServeProbe {
    ServeProbe::start(move |ctx: ServeContext| async move {
        let ServeContext {
            listener,
            quiet,
            connections,
            ..
        } = ctx;
        let work = Rc::new(OffPathQueue {
            guards: RefCell::new(Vec::with_capacity(4096)),
            wake: Notify::new(),
        });
        if mode.off_path {
            tokio::task::spawn_local(off_path_worker(Rc::clone(&work)));
        }
        let mut accepted = 0usize;
        loop {
            let (stream, _) = listener.accept().await?;
            stream.set_nodelay(true)?;
            accepted += 1;
            let work = Rc::clone(&work);
            let quiet = quiet.clone();
            if mode.ungauged || (mode.gauge_first_only && accepted > 1) {
                tokio::task::spawn_local(connection(stream, mode, quiet, work));
            } else if mode.gauge_twice {
                let io = GaugedIo::new(GaugedIo::new(stream, &connections), &connections);
                tokio::task::spawn_local(connection(io, mode, quiet, work));
            } else {
                let io = GaugedIo::new(stream, &connections);
                tokio::task::spawn_local(connection(io, mode, quiet, work));
            }
        }
    })
    .expect("probe starts")
}

struct OffPathQueue {
    /// Guards of work not done yet; capacity reserved up front so a push does not allocate.
    guards: RefCell<Vec<OffPath>>,
    wake: Notify,
}

async fn off_path_worker(work: Rc<OffPathQueue>) {
    loop {
        work.wake.notified().await;
        tokio::time::sleep(OFF_PATH_DELAY).await;
        loop {
            let guard = work.guards.borrow_mut().pop();
            let Some(guard) = guard else { break };
            std::hint::black_box(Box::new(7u64));
            drop(guard);
        }
    }
}

async fn connection<T: AsyncRead + AsyncWrite + Unpin>(
    mut io: T,
    mode: Mode,
    quiet: Quiescence,
    work: Rc<OffPathQueue>,
) {
    let mut buffer = vec![0u8; 8192];
    let held = vec![1u8; mode.idle_hold];
    let mut filled = 0;
    loop {
        let n = match io.read(&mut buffer[filled..]).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        filled += n;
        if !buffer[..filled].ends_with(b"\r\n\r\n") {
            continue;
        }
        let rows = rows_of(&buffer[..filled]);
        for _ in 0..mode.per_request_blocks + rows * mode.per_row_blocks {
            std::hint::black_box(Box::new(0u64));
        }
        if mode.off_path {
            // taken before the response is written: the harness contract
            let guard = quiet.begin();
            work.guards.borrow_mut().push(guard);
            work.wake.notify_one();
        }
        let response = match (mode.fail, mode.close) {
            (true, _) => FAIL,
            (false, true) => OK_CLOSE,
            (false, false) => OK,
        };
        if io.write_all(response).await.is_err() || io.flush().await.is_err() || mode.close {
            break;
        }
        filled = 0;
    }
    std::hint::black_box(held);
}

/// One row per `r` in the request target.
fn rows_of(head: &[u8]) -> usize {
    let line = head.split(|&b| b == b'\r').next().unwrap_or(head);
    line.iter()
        .skip_while(|&&b| b != b'/')
        .take_while(|&&b| b != b' ')
        .filter(|&&b| b == b'r')
        .count()
}

fn client(probe: &ServeProbe, request: &[u8]) -> RawClient {
    RawClient::connect(probe.addr(), request, 4096).expect("client connects")
}

fn per_request(mode: Mode) -> serve::PerRequest {
    let probe = start(mode);
    let mut client = client(&probe, GET);
    let cost = probe
        .per_request(&mut client, DEFAULT_WARMUP, 50)
        .expect("measured");
    drop(client);
    probe.stop().expect("server stops cleanly");
    cost
}

/// The client allocates nothing for `Content-Length` and chunked responses, against a server
/// that does no work: every count the harness reports is the server's.
fn client_probe_allocates_nothing() {
    for canned in [CANNED_LENGTH, CANNED_CHUNKED] {
        let stats = serve::client_probe(canned, 200).expect("the client allocates nothing");
        assert_eq!(stats.total_blocks, 0);
        assert_eq!(stats.total_bytes, 0);
    }
}

/// A server that allocates one more block per request is measured at exactly one more block per
/// request, and the per-request cost of both is exact (the same for every request).
fn a_known_block_per_request_is_detected_exactly() {
    let base = per_request(Mode::default());
    let plus_one = per_request(Mode {
        per_request_blocks: 1,
        ..Mode::default()
    });
    let base_blocks = base.exact_blocks_per_request().expect("exact base cost");
    let plus_blocks = plus_one.exact_blocks_per_request().expect("exact cost");
    assert_eq!(plus_blocks, base_blocks + 1, "{base:?} {plus_one:?}");
    // the base server and the client allocate nothing per request
    assert_eq!(base_blocks, 0, "{base:?}");
    // eight bytes per request: one Box<u64>
    assert!(
        (plus_one.bytes_per_request() - base.bytes_per_request() - 8.0).abs() < f64::EPSILON,
        "{base:?} {plus_one:?}"
    );
}

/// A connection that holds a known 8 KiB while idle is measured within 1 % of 8 KiB more per
/// idle connection than the same server without it.
fn an_idle_connection_holding_8_kib_is_detected_within_1_percent() {
    let measure = |mode| {
        let probe = start(mode);
        let idle = probe
            .per_idle_connection(GET, 16, 4096)
            .expect("idle measured");
        probe.stop().expect("server stops cleanly");
        idle
    };
    let base = measure(Mode::default());
    let holding = measure(Mode {
        idle_hold: 8192,
        ..Mode::default()
    });
    let extra = holding.bytes_per_connection() - base.bytes_per_connection();
    assert!(
        (extra - 8192.0).abs() <= 8192.0 * 0.01,
        "extra {extra} bytes per idle connection; base {base:?}, holding {holding:?}"
    );
    // the per-connection figure counts the connection's own buffer (8 KiB) and the task
    assert!(
        base.bytes_per_connection() >= 8192.0,
        "base {} bytes per idle connection",
        base.bytes_per_connection()
    );
}

/// An off-path task that allocates one block 20 ms after the response is inside the measured
/// window because the window ends only when the quiescence hook says the work is done.
fn an_off_path_block_is_counted_after_the_quiescence_hook() {
    let window = |mode| {
        let probe = start(mode);
        let mut client = client(&probe, GET);
        for _ in 0..DEFAULT_WARMUP {
            client.send().expect("warm-up");
        }
        let start = std::time::Instant::now();
        let stats = probe.measure_window(&mut client, 1).expect("measured");
        let took = start.elapsed();
        drop(client);
        probe.stop().expect("server stops cleanly");
        (stats, took)
    };
    let (base, _) = window(Mode::default());
    let (off_path, took) = window(Mode {
        off_path: true,
        ..Mode::default()
    });
    assert_eq!(
        off_path.total_blocks,
        base.total_blocks + 1,
        "base {base:?}, off-path {off_path:?}"
    );
    // the window waited for the work: it lasted at least the off-path delay
    assert!(took >= OFF_PATH_DELAY, "window took {took:?}");
    // and per request in steady state too
    let base = per_request(Mode::default());
    let off_path = per_request(Mode {
        off_path: true,
        ..Mode::default()
    });
    assert_eq!(
        off_path.exact_blocks_per_request(),
        base.exact_blocks_per_request().map(|b| b + 1),
        "{base:?} {off_path:?}"
    );
}

/// The result-size rule passes a server whose allocations grow by `c_row` per row and refuses
/// one that grows faster; at the gate (`c_row` = 0) only constant allocations pass.
fn the_result_size_rule_detects_per_row_allocations() {
    let small_request = b"GET /rrrrrrrrrr HTTP/1.1\r\nhost: harness\r\n\r\n".as_slice();
    let large: Vec<u8> = {
        let mut r = b"GET /".to_vec();
        r.extend(std::iter::repeat_n(b'r', 40));
        r.extend_from_slice(b" HTTP/1.1\r\nhost: harness\r\n\r\n");
        r
    };
    let run = |mode, rule| {
        let probe = start(mode);
        let mut small = client(&probe, small_request);
        let mut big = client(&probe, &large);
        let outcome = probe.result_size_rule(rule, (&mut small, 10), (&mut big, 40));
        drop((small, big));
        probe.stop().expect("server stops cleanly");
        outcome
    };
    let per_row = Mode {
        per_row_blocks: 1,
        ..Mode::default()
    };
    let measured = run(per_row, ResultSizeRule::new(1)).expect("1 block per row fits c_row 1");
    assert_eq!(
        measured.at_4s.map(|s| s.total_blocks),
        [measured.at_s.total_blocks + 30; 2],
        "{measured:?}"
    );
    let refused = run(per_row, ResultSizeRule::new(C_ROW_GATE)).expect_err("refused at the gate");
    assert!(
        matches!(
            refused,
            ServeError::ResultSizeRule {
                request: 2,
                c_row: 0,
                added_rows: 30,
                ..
            }
        ),
        "{refused}"
    );
    run(Mode::default(), ResultSizeRule::new(C_ROW_GATE))
        .expect("constant allocations pass the gate");
}

/// A server that does not report its connections cannot pass an idle measurement by counting
/// nothing.
fn an_ungauged_server_fails_the_idle_measurement() {
    let probe = start(Mode {
        ungauged: true,
        ..Mode::default()
    });
    let error = probe
        .per_idle_connection(GET, 4, 4096)
        .expect_err("cannot measure idleness");
    assert_eq!(error.to_string(), not_gauged(0, 4));
    probe.stop().expect("server stops cleanly");
}

/// The same for a request window: an ungauged server reports 0 idle of 0 open, which would settle
/// at once and end a window before the server's off-path work. `per_request` and the result-size
/// rule refuse it instead of counting a window that may have missed allocations.
fn an_ungauged_server_fails_the_request_measurements() {
    let probe = start(Mode {
        ungauged: true,
        off_path: true,
        ..Mode::default()
    });
    let mut first = client(&probe, GET);
    let error = probe
        .per_request(&mut first, 4, 8)
        .expect_err("per_request refuses an ungauged server");
    assert!(
        matches!(
            error,
            ServeError::ConnectionsNotGauged {
                open: 0,
                expected: 1,
                waited,
            } if waited == Duration::from_secs(5)
        ),
        "{error}"
    );
    assert_eq!(error.to_string(), not_gauged(0, 1));
    let mut second = client(&probe, GET);
    let error = probe
        .result_size_rule(ResultSizeRule::new(1), (&mut first, 1), (&mut second, 1))
        .expect_err("the result-size rule refuses an ungauged server");
    // the rule holds both clients open, so it expects both on the gauge
    assert_eq!(error.to_string(), not_gauged(0, 2));
    drop((first, second));
    probe.stop().expect("server stops cleanly");
}

/// Review R2-6: a server that gauges only the first of two connections reports one open
/// connection while the harness holds two; the result-size rule and the idle measurement refuse
/// it instead of reading idleness from half the connections.
fn a_server_gauging_one_of_two_connections_fails() {
    let probe = start(Mode {
        gauge_first_only: true,
        ..Mode::default()
    });
    let mut first = client(&probe, GET);
    let mut second = client(&probe, GET);
    let error = probe
        .result_size_rule(ResultSizeRule::new(1), (&mut first, 1), (&mut second, 1))
        .expect_err("one of two connections gauged");
    assert!(
        matches!(
            error,
            ServeError::ConnectionsNotGauged {
                open: 1,
                expected: 2,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(error.to_string(), not_gauged(1, 2));
    drop((first, second));
    probe.stop().expect("server stops cleanly");
}

/// Review R2-6: the harness knows every connection, so it compares with `==`: a server that
/// counts a connection twice (two `GaugedIo` around one stream) is refused, where an "at least"
/// comparison would pass it and settle on a gauge that does not match the connections.
fn a_server_gauging_a_connection_twice_fails() {
    let probe = start(Mode {
        gauge_twice: true,
        ..Mode::default()
    });
    let mut client = client(&probe, GET);
    let error = probe
        .per_request(&mut client, 1, 1)
        .expect_err("one connection counted twice");
    assert_eq!(error.to_string(), not_gauged(2, 1));
    drop(client);
    let error = probe
        .per_idle_connection(GET, 2, 4096)
        .expect_err("each connection counted twice");
    assert_eq!(error.to_string(), not_gauged(4, 2));
    probe.stop().expect("server stops cleanly");
}

/// Review R2-2, R3-3: a window needs its connection open throughout; a server that answers with
/// `Connection: close` is refused with its own error, not measured across a reconnect.
fn a_server_closing_the_connection_fails_the_window() {
    let probe = start(Mode {
        close: true,
        ..Mode::default()
    });
    let mut client = client(&probe, GET);
    let error = probe
        .measure_window(&mut client, 1)
        .expect_err("the connection closed");
    assert!(
        matches!(
            error,
            ServeError::ConnectionNotKeptOpen {
                during: "a measured window"
            }
        ),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "the server closed the connection after a response during a measured window: the \
         measurement needs its connections kept open"
    );
    drop(client);
    probe.stop().expect("server stops cleanly");
}

/// Review R3-4: the warm-up of `per_request` and of the result-size rule refuses a closing
/// server too, before any window starts (otherwise the client reconnects and the first window
/// waits for a gauge that counts the new connection late); so does an idle-connection run.
fn a_server_closing_the_connection_fails_the_warm_up() {
    let probe = start(Mode {
        close: true,
        ..Mode::default()
    });
    let warm_up = |error: ServeError| {
        assert!(
            matches!(
                error,
                ServeError::ConnectionNotKeptOpen {
                    during: "the warm-up"
                }
            ),
            "{error}"
        );
    };
    let mut first = client(&probe, GET);
    warm_up(
        probe
            .per_request(&mut first, 1, 1)
            .expect_err("closed in the warm-up"),
    );
    let mut first = client(&probe, GET);
    let mut second = client(&probe, GET);
    warm_up(
        probe
            .result_size_rule(ResultSizeRule::new(1), (&mut first, 1), (&mut second, 1))
            .expect_err("closed in the warm-up"),
    );
    drop((first, second));
    // and an idle-connection run: nothing would stay idle
    let error = probe
        .per_idle_connection(GET, 2, 4096)
        .expect_err("closed after the response");
    assert!(
        matches!(
            error,
            ServeError::ConnectionNotKeptOpen {
                during: "an idle-connection run"
            }
        ),
        "{error}"
    );
    probe.stop().expect("server stops cleanly");
}

/// The message of [`ServeError::ConnectionsNotGauged`] after the harness's 5 s wait.
fn not_gauged(open: u64, expected: u64) -> String {
    format!(
        "the server reports {open} open connections within 5s, expected {expected}: either it \
         does not wrap every accepted connection exactly once in erp_testkit::serve::GaugedIo \
         with the context's gauge, it is slow to accept, it closed a client's connection, or \
         the caller holds connections the harness does not count"
    )
}

/// A failing response is not measured as if it were the intended work.
fn an_error_status_fails_the_measurement() {
    let probe = start(Mode {
        fail: true,
        ..Mode::default()
    });
    let mut client = client(&probe, GET);
    let error = probe
        .per_request(&mut client, 1, 1)
        .expect_err("500 refused");
    assert_eq!(error.to_string(), "the server answered with status 500");
    let error = probe
        .per_request(&mut client, 0, 0)
        .expect_err("n = 0 refused");
    assert_eq!(
        error.to_string(),
        "bad harness argument: per_request needs n > 0"
    );
    drop(client);
    probe.stop().expect("server stops cleanly");
}

/// A refusal path (a 429 or 503 the server is meant to send) is measured when the client names
/// the status it expects: exactly that status passes and is counted like any other work; any
/// other status, a success included, still fails the measurement.
fn an_expected_refusal_status_is_measured() {
    let probe = start(Mode {
        fail: true,
        per_request_blocks: 1,
        ..Mode::default()
    });
    let mut raw = client(&probe, GET);
    raw.expect_status(500);
    assert_eq!(raw.expected_status(), Some(500));
    let cost = probe
        .per_request(&mut raw, DEFAULT_WARMUP, 50)
        .expect("the expected 500 is measured");
    assert_eq!(cost.exact_blocks_per_request(), Some(1), "{cost:?}");
    raw.expect_status(503);
    let error = probe
        .per_request(&mut raw, 1, 1)
        .expect_err("500 is not the expected 503");
    assert_eq!(
        error.to_string(),
        "the server answered with status 500, not the expected 503"
    );
    drop(raw);
    probe.stop().expect("server stops cleanly");

    let probe = start(Mode::default());
    let mut raw = client(&probe, GET);
    raw.expect_status(503);
    let error = probe
        .per_request(&mut raw, 1, 1)
        .expect_err("a success is not the expected refusal");
    assert_eq!(
        error.to_string(),
        "the server answered with status 200, not the expected 503"
    );
    drop(raw);
    probe.stop().expect("server stops cleanly");
}

/// A server future that fails is reported by `stop`, not lost.
fn a_failing_server_is_reported() {
    let probe = ServeProbe::start(|_ctx: ServeContext| async {
        Err::<(), _>(std::io::Error::other("listener broke"))
    })
    .expect("probe starts");
    let gauge: &ConnGauge = probe.connections();
    assert_eq!((gauge.open(), gauge.idle()), (0, 0));
    // the thread ends on its own; wait for it
    let waited = std::time::Instant::now();
    while probe.ensure_running().is_ok() {
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "server did not stop"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        probe.ensure_running().expect_err("stopped").to_string(),
        "the server under test stopped: the server thread has ended before the measurement finished"
    );
    let error = probe.stop().expect_err("failure reported");
    assert_eq!(
        error.to_string(),
        "the server under test stopped: the server future failed"
    );
    assert_eq!(
        std::error::Error::source(&error).map(ToString::to_string),
        Some("listener broke".to_string()),
        "the server's own error is the source"
    );
}

/// The gauge follows a connection through busy, idle and closed.
fn the_gauge_tracks_idle_connections() {
    let probe = start(Mode::default());
    let gauge = probe.connections().clone();
    let mut client = client(&probe, GET);
    client.send().expect("first request");
    probe.settle().expect("settles");
    assert_eq!((gauge.open(), gauge.idle()), (1, 1));
    drop(client);
    let waited = std::time::Instant::now();
    while gauge.open() > 0 {
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "connection not dropped"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(gauge.idle(), 0);
    probe.stop().expect("server stops cleanly");
}

/// A dropped stream on the server side is seen by the client as an incomplete response.
fn a_connection_closed_mid_response_is_an_error() {
    let probe = ServeProbe::start(|ctx: ServeContext| async move {
        loop {
            let (mut stream, _) = ctx.listener.accept().await?;
            let mut buf = [0u8; 1024];
            let _n = stream.read(&mut buf).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 10\r\n\r\nabc")
                .await?;
            drop::<TcpStream>(stream);
        }
    })
    .expect("probe starts");
    let mut client = client(&probe, GET);
    let error = client.send().expect_err("incomplete");
    assert!(
        matches!(error, ServeError::ConnectionClosed { received } if received > 0),
        "{error}"
    );
    drop(client);
    probe.stop().expect("server stops cleanly");
}
