//! The measurements of perf-serve §3: allocations per request, heap per idle connection, future
//! sizes and the result-size rule.

use std::future::Future;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use erp_alloc_budget::{self as alloc, AllocStats};

use super::ServeError;
use super::client::{self, RawClient};
use super::probe::{SETTLE_TIMEOUT, ServeProbe, wait_until};

/// How long [`ServeProbe::per_idle_connection`] waits for the server's gauge to count the
/// connections the client holds open.
const GAUGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Requests sent before the first measured window (perf-serve §3: default 64), so caches, the
/// date header and lazily built tables are warm.
pub const DEFAULT_WARMUP: usize = 64;

/// The per-fetched-row allocation term the result-size rule uses until PERF.STREAM and
/// PERF.STRINGS merge (G6 item 5 interim): 3 since the raw-row read removed the
/// driver's range vector per row (4 before,:170).
pub const C_ROW_INTERIM: u64 = 3;
/// The G6 gate for the per-fetched-row term: allocations constant in the result size.
pub const C_ROW_GATE: u64 = 0;
/// The value every result-size test takes (perf-serve §3 "one constant"). Moves to
/// [`C_ROW_GATE`] in the commit that merges the last of PERF.STREAM and PERF.STRINGS.
pub const C_ROW: u64 = C_ROW_INTERIM;

/// Allocations per request from two windows of `n` and `2n` requests on one connection
/// ([`ServeProbe::per_request`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PerRequest {
    /// Requests in the first window (the second has twice as many).
    pub n: u64,
    /// The window of `n` requests.
    pub at_n: AllocStats,
    /// The window of `2n` requests.
    pub at_2n: AllocStats,
}

impl PerRequest {
    /// Allocations per request: `(blocks(2n) − blocks(n)) / n`. Negative when the second window
    /// allocated less than the first (a cache filled during the first: warm up longer).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "counts of a test run; f64 is exact below 2^53"
    )]
    pub fn blocks_per_request(&self) -> f64 {
        (self.at_2n.total_blocks as f64 - self.at_n.total_blocks as f64) / self.n as f64
    }

    /// Requested heap bytes per request, the same difference.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "counts of a test run; f64 is exact below 2^53"
    )]
    pub fn bytes_per_request(&self) -> f64 {
        (self.at_2n.total_bytes as f64 - self.at_n.total_bytes as f64) / self.n as f64
    }

    /// The allocations per request when the difference divides evenly by `n`; `None` otherwise
    /// (the server's cost is not the same for every request).
    #[must_use]
    pub fn exact_blocks_per_request(&self) -> Option<u64> {
        exact_per_unit(self.at_n.total_blocks, self.at_2n.total_blocks, self.n)
    }

    /// The fixed part of a window: `blocks(n) − n × per request` (window boundaries, one-time
    /// work). `None` when the per-request cost is not exact.
    #[must_use]
    pub fn fixed_blocks(&self) -> Option<i128> {
        fixed_part(self.at_n.total_blocks, self.at_2n.total_blocks, self.n)
    }
}

/// `(at_2n − at_n) / n` when it divides evenly and does not go negative.
fn exact_per_unit(at_n: u64, at_2n: u64, n: u64) -> Option<u64> {
    let delta = at_2n.checked_sub(at_n)?;
    (n > 0 && delta % n == 0).then(|| delta / n)
}

/// `at_n − n × per unit` for an exact per-unit cost.
fn fixed_part(at_n: u64, at_2n: u64, n: u64) -> Option<i128> {
    let per = exact_per_unit(at_n, at_2n, n)?;
    Some(i128::from(at_n) - i128::from(n) * i128::from(per))
}

/// Heap held per idle connection from two runs with `c` and `2c` connections
/// ([`ServeProbe::per_idle_connection`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PerIdle {
    /// Connections in the first run (the second opens twice as many).
    pub c: u64,
    /// `c` connections opened, one request each, all idle; live heap read at that moment.
    pub at_c: AllocStats,
    /// The same with `2c` connections.
    pub at_2c: AllocStats,
}

impl PerIdle {
    /// Live heap bytes per idle connection: `(end_bytes(2c) − end_bytes(c)) / c`.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "counts of a test run; f64 is exact below 2^53"
    )]
    pub fn bytes_per_connection(&self) -> f64 {
        (self.at_2c.end_bytes as f64 - self.at_c.end_bytes as f64) / self.c as f64
    }

    /// Live heap blocks per idle connection, the same difference.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "counts of a test run; f64 is exact below 2^53"
    )]
    pub fn blocks_per_connection(&self) -> f64 {
        (self.at_2c.end_blocks as f64 - self.at_c.end_blocks as f64) / self.c as f64
    }

    /// Allocations made per connection to accept it and serve its one request (live or not).
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "counts of a test run; f64 is exact below 2^53"
    )]
    pub fn allocations_per_connection(&self) -> f64 {
        (self.at_2c.total_blocks as f64 - self.at_c.total_blocks as f64) / self.c as f64
    }
}

impl ServeProbe {
    /// Measure exactly `n` requests on `client` in one window that starts and ends on a settled
    /// server ([`ServeProbe::settle`]: no off-path work pending, every connection idle), so
    /// off-path work of the window is inside it. Whole process, every thread (dhat).
    ///
    /// The server must wrap its connections in [`super::GaugedIo`]: settling compares idle with
    /// open connections, and an ungauged server reports 0 of 0 (settled at once, before its
    /// off-path work ends). The window therefore requires exactly `client`'s connection on the
    /// gauge (one open connection) before and after the requests.
    ///
    /// **The caller holds no other connection to the server** while it measures: the harness
    /// compares the gauge with the connections it counts (`==`), so a connection it does not know
    /// about fails the measurement instead of hiding one the server does not report. The
    /// connection must also stay open: a response with `Connection: close` is refused.
    ///
    /// # Errors
    /// [`ServeError::ConnectionsNotGauged`] when the server does not report exactly one open
    /// connection within 5 s, [`ServeError::ConnectionNotKeptOpen`] when it closes the connection
    /// after a request, the client's errors, [`ServeError::UnexpectedStatus`] for a status
    /// outside 2xx/3xx (or other than the one the client expects, [`RawClient::expect_status`]),
    /// [`ServeError::Timeout`] when the server does not settle,
    /// [`ServeError::Alloc`].
    pub fn measure_window(
        &self,
        client: &mut RawClient,
        n: usize,
    ) -> Result<AllocStats, ServeError> {
        self.window(client, n, 1)
    }

    /// [`ServeProbe::measure_window`] for a caller that holds exactly `clients` connections to the
    /// server: the gauge must count that many before and after the requests.
    fn window(
        &self,
        client: &mut RawClient,
        n: usize,
        clients: u64,
    ) -> Result<AllocStats, ServeError> {
        // the clients are connected; the server may not have accepted them yet
        self.await_gauged(clients)?;
        self.settle()?;
        let (stats, outcome) = alloc::measure(|| -> Result<(), ServeError> {
            for _ in 0..n {
                // a reconnect would put the server's per-connection work in the window
                send_kept_open(client, "a measured window")?;
            }
            // the responses came over connections the server has open: they must be on the gauge
            // (waiting allocates nothing; it returns at once when they are)
            self.await_gauged(clients)?;
            self.settle()
        })?;
        outcome.map_err(|error| self.explain(error))?;
        Ok(stats)
    }

    /// Wait up to `GAUGE_TIMEOUT` (5 s) until the gauge counts exactly `expected` open connections:
    /// the harness knows every connection to the server (the callers' contract), so more is as
    /// wrong as fewer (a connection wrapped twice, or one the caller did not declare). Allocates
    /// nothing.
    fn await_gauged(&self, expected: u64) -> Result<(), ServeError> {
        let gauge = self.connections();
        wait_until(
            GAUGE_TIMEOUT,
            "the client connections to be open on the server",
            || gauge.open() == expected,
        )
        .map_err(|_| ServeError::ConnectionsNotGauged {
            open: gauge.open(),
            expected,
            waited: GAUGE_TIMEOUT,
        })
    }

    /// Allocations per request (perf-serve §3 `per_request`): sends `warmup` requests, then
    /// measures a window of `n` and one of `2n`; per request = the difference divided by `n`,
    /// which cancels the fixed part of a window.
    ///
    /// `client`'s connection is the only one the caller holds to the server and stays open (as
    /// for [`ServeProbe::measure_window`]).
    ///
    /// Run it in a test binary of its own under nextest (one process per test): every thread of
    /// the process is counted.
    ///
    /// # Errors
    /// [`ServeError::BadArgument`] for `n == 0`, [`ServeError::ConnectionNotKeptOpen`] when the
    /// server closes the connection during the warm-up, otherwise as
    /// [`ServeProbe::measure_window`].
    pub fn per_request(
        &self,
        client: &mut RawClient,
        warmup: usize,
        n: usize,
    ) -> Result<PerRequest, ServeError> {
        if n == 0 {
            return Err(ServeError::BadArgument("per_request needs n > 0"));
        }
        let twice = n
            .checked_mul(2)
            .ok_or(ServeError::BadArgument("2n overflows"))?;
        for _ in 0..warmup {
            send_kept_open(client, "the warm-up")?;
        }
        let at_n = self.measure_window(client, n)?;
        let at_2n = self.measure_window(client, twice)?;
        Ok(PerRequest {
            n: to_u64(n),
            at_n,
            at_2n,
        })
    }

    /// Heap per idle connection (perf-serve §3 `per_idle_connection`): opens `c`, then `2c`
    /// connections, sends `request` once on each, waits until the server reports all of them idle
    /// (the [`super::ConnGauge`] its [`super::GaugedIo`] keeps) and reads the live heap; per
    /// connection = the difference divided by `c`. A first run of `c` connections, not measured,
    /// warms the runtime. The server must wrap its connections in [`super::GaugedIo`].
    ///
    /// The caller holds no connection to the server while it runs: each run waits until the
    /// server has dropped every earlier connection, then expects exactly its own on the gauge.
    ///
    /// Every connection is two descriptors in the test process (the client's end and the
    /// server's): keep `4c` under the open-file limit (256 by default on macOS).
    ///
    /// # Errors
    /// [`ServeError::BadArgument`] for `c == 0` or an empty request,
    /// [`ServeError::ConnectionsNotGauged`] when the server does not report exactly the run's
    /// connections, [`ServeError::ConnectionNotKeptOpen`] when it closes one after its response,
    /// the client's errors, [`ServeError::Timeout`], [`ServeError::Alloc`].
    pub fn per_idle_connection(
        &self,
        request: &[u8],
        c: usize,
        response_capacity: usize,
    ) -> Result<PerIdle, ServeError> {
        if c == 0 {
            return Err(ServeError::BadArgument("per_idle_connection needs c > 0"));
        }
        if request.is_empty() {
            return Err(ServeError::BadArgument("the request is empty"));
        }
        let twice = c
            .checked_mul(2)
            .ok_or(ServeError::BadArgument("2c overflows"))?;
        let mut streams: Vec<TcpStream> = Vec::with_capacity(twice);
        let mut buffer = vec![0u8; response_capacity.max(1)];
        self.idle_run(request, c, &mut streams, &mut buffer)?;
        let at_c = self.idle_run(request, c, &mut streams, &mut buffer)?;
        let at_2c = self.idle_run(request, twice, &mut streams, &mut buffer)?;
        Ok(PerIdle {
            c: to_u64(c),
            at_c,
            at_2c,
        })
    }

    /// One run of [`ServeProbe::per_idle_connection`]: `k` connections measured, then closed.
    fn idle_run(
        &self,
        request: &[u8],
        k: usize,
        streams: &mut Vec<TcpStream>,
        buffer: &mut [u8],
    ) -> Result<AllocStats, ServeError> {
        let gauge = self.connections();
        let method = client::RequestMethod::of_request(request);
        wait_until(
            SETTLE_TIMEOUT,
            "the server to drop every earlier connection",
            || gauge.open() == 0,
        )?;
        self.settle()?;
        let (stats, outcome) = alloc::measure(|| -> Result<(), ServeError> {
            for _ in 0..k {
                let mut stream = client::connect(self.addr())?;
                stream.write_all(request).map_err(|source| ServeError::Io {
                    during: "writing the request",
                    source,
                })?;
                let meta = client::read_response(&mut stream, buffer, method)?;
                expect_status(meta.status, None)?;
                if meta.close {
                    // nothing would stay idle
                    return Err(ServeError::ConnectionNotKeptOpen {
                        during: "an idle-connection run",
                    });
                }
                streams.push(stream);
            }
            // accepting is immediate on loopback; a server that never reports is found quickly
            self.await_gauged(to_u64(k))?;
            self.settle()
        })?;
        // closed outside the window; the vector keeps its capacity for the next run
        streams.clear();
        outcome.map_err(|error| self.explain(error))?;
        Ok(stats)
    }

    /// A client-side error seen while the server thread had already ended is reported as the
    /// server stopping (a reset connection is the symptom, not the cause).
    fn explain(&self, error: ServeError) -> ServeError {
        match (&error, self.ensure_running()) {
            (ServeError::ServerStopped { .. }, _) | (_, Ok(())) => error,
            (_, Err(_)) => ServeError::ServerStopped {
                reason: "the server thread ended during the measurement (the source is what the \
                         client saw; ServeProbe::stop() returns the server's own error)"
                    .into(),
                source: Some(Box::new(error)),
            },
        }
    }

    /// The result-size rule (perf-serve §3): the same request at `s` rows (`small`, measured
    /// once, warm) and at `4s` rows (`large`: its first request warms the larger sizes and is not
    /// counted; its second and third are measured one by one). Each of those two may allocate at
    /// most `c_row` × the added fetched rows more than the request at `s` rows.
    ///
    /// Both connections (`small`'s and `large`'s) must stay open for the whole rule, and the
    /// caller holds no other connection to the server: every window expects exactly these two on
    /// the gauge.
    ///
    /// # Errors
    /// [`ServeError::ResultSizeRule`] when a request at `4s` rows exceeds the rule,
    /// [`ServeError::ConnectionsNotGauged`] when the server does not report exactly two open
    /// connections, [`ServeError::ConnectionNotKeptOpen`] when it closes one (warm-up included),
    /// otherwise as [`ServeProbe::measure_window`].
    pub fn result_size_rule(
        &self,
        rule: ResultSizeRule,
        small: (&mut RawClient, u64),
        large: (&mut RawClient, u64),
    ) -> Result<ResultSize, ServeError> {
        let (small_client, small_rows) = small;
        let (large_client, large_rows) = large;
        if large_rows < small_rows {
            return Err(ServeError::BadArgument(
                "the large request must fetch at least as many rows as the small one",
            ));
        }
        for _ in 0..DEFAULT_WARMUP {
            send_kept_open(small_client, "the warm-up")?;
        }
        // both clients are open for the whole rule: the gauge must count both, and only them
        let at_s = self.window(small_client, 1, 2)?;
        send_kept_open(large_client, "the warm-up")?;
        let second = self.window(large_client, 1, 2)?;
        let third = self.window(large_client, 1, 2)?;
        let result = ResultSize {
            small_rows,
            large_rows,
            at_s,
            at_4s: [second, third],
        };
        rule.check(&result)?;
        Ok(result)
    }
}

/// The per-fetched-row term of the result-size rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultSizeRule {
    c_row: u64,
}

impl ResultSizeRule {
    /// The rule with `c_row` allocations allowed per added fetched row ([`C_ROW`] in tests).
    #[must_use]
    pub const fn new(c_row: u64) -> Self {
        Self { c_row }
    }

    /// The allocations allowed per added fetched row.
    #[must_use]
    pub const fn c_row(&self) -> u64 {
        self.c_row
    }

    /// Check measured requests against the rule.
    ///
    /// # Errors
    /// [`ServeError::ResultSizeRule`] for the first request at `4s` over the allowance, and
    /// [`ServeError::BadArgument`] when the allowance overflows.
    pub fn check(&self, measured: &ResultSize) -> Result<(), ServeError> {
        self.check_blocks(
            (measured.small_rows, measured.large_rows),
            measured.at_s.total_blocks,
            [
                measured.at_4s[0].total_blocks,
                measured.at_4s[1].total_blocks,
            ],
        )
    }

    /// [`ResultSizeRule::check`] on block counts: `rows` = (small, large), `at_s` the blocks of
    /// the request at the small size, `at_4s` those of the second and third at the large size.
    fn check_blocks(&self, rows: (u64, u64), at_s: u64, at_4s: [u64; 2]) -> Result<(), ServeError> {
        let added = rows.1.checked_sub(rows.0).ok_or(ServeError::BadArgument(
            "the large request fetches fewer rows",
        ))?;
        let allowed = self
            .c_row
            .checked_mul(added)
            .and_then(|extra| extra.checked_add(at_s))
            .ok_or(ServeError::BadArgument(
                "the result-size allowance overflows",
            ))?;
        for (index, &blocks) in at_4s.iter().enumerate() {
            if blocks > allowed {
                return Err(ServeError::ResultSizeRule {
                    request: index + 2,
                    blocks,
                    allowed,
                    blocks_at_s: at_s,
                    c_row: self.c_row,
                    added_rows: added,
                });
            }
        }
        Ok(())
    }
}

/// The measurements behind a [`ResultSizeRule`] check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResultSize {
    /// Rows the small request fetches (`s`).
    pub small_rows: u64,
    /// Rows the large request fetches (`4s`).
    pub large_rows: u64,
    /// One request at `s` rows, warm.
    pub at_s: AllocStats,
    /// The second and the third request at `4s` rows.
    pub at_4s: [AllocStats; 2],
}

impl ResultSize {
    /// Build a result from measurements taken elsewhere (for a check without a probe).
    #[must_use]
    pub const fn new(
        small_rows: u64,
        large_rows: u64,
        at_s: AllocStats,
        at_4s: [AllocStats; 2],
    ) -> Self {
        Self {
            small_rows,
            large_rows,
            at_s,
            at_4s,
        }
    }
}

/// The size of a constructed, never polled future (perf-serve §3 `future_size!`). Taking it by
/// reference with a `Future` bound makes the compiler refuse anything that is not a future.
#[must_use]
pub fn future_size<F: Future>(future: &F) -> usize {
    std::mem::size_of_val(future)
}

/// Assert the size of a constructed (not polled) future against a ceiling and evaluate to the
/// size: `future_size!(make_future(), <= 4096)`. The future is dropped unpolled.
#[macro_export]
macro_rules! future_size {
    ($future:expr, <= $max:expr) => {{
        let __future = $future;
        let __size = $crate::serve::future_size(&__future);
        let __max: usize = $max;
        ::std::mem::drop(__future);
        assert!(
            __size <= __max,
            "future size {} bytes exceeds the ceiling of {} bytes",
            __size,
            __max
        );
        __size
    }};
}

/// Proves [`RawClient`] allocates nothing (perf-serve §3 client probe): a canned server on a plain
/// thread that allocates nothing per request answers `requests` requests (after a warm-up) inside
/// a measured window, for each of the given canned responses. Returns the window's figures.
///
/// # Errors
/// [`ServeError::ClientAllocated`] when the window counted any allocation; the client's errors.
pub fn client_probe(canned: &'static [u8], requests: usize) -> Result<AllocStats, ServeError> {
    let server = CannedServer::start(canned)?;
    let mut client = RawClient::connect(
        server.addr(),
        &b"GET / HTTP/1.1\r\nhost: probe\r\n\r\n"[..],
        64 * 1024,
    )?;
    for _ in 0..DEFAULT_WARMUP {
        client.send()?;
    }
    let (stats, outcome) = alloc::measure(|| -> Result<(), ServeError> {
        for _ in 0..requests {
            let response = client.send()?;
            if response.meta().wire_len != canned.len() {
                return Err(ServeError::BadArgument(
                    "the client framed the canned response at the wrong length",
                ));
            }
        }
        Ok(())
    })?;
    outcome?;
    drop(client);
    server.stop()?;
    if stats.total_blocks > 0 {
        return Err(ServeError::ClientAllocated {
            blocks: stats.total_blocks,
            bytes: stats.total_bytes,
        });
    }
    Ok(stats)
}

/// A blocking server on a plain thread that answers every request head with the same bytes and
/// allocates nothing per request: the "no server work" side of [`client_probe`].
struct CannedServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<std::io::Result<()>>>,
}

impl CannedServer {
    fn start(canned: &'static [u8]) -> Result<Self, ServeError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|source| ServeError::ServerStart { source })?;
        let addr = listener
            .local_addr()
            .map_err(|source| ServeError::ServerStart { source })?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("canned-server".into())
                .spawn(move || serve_canned(&listener, canned, &stop))
                .map_err(|source| ServeError::ServerStart { source })?
        };
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    fn stop(mut self) -> Result<(), ServeError> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> Result<(), ServeError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        self.stop.store(true, Ordering::Release);
        // wakes the blocking accept; a failure means the thread is already gone
        let _woken = TcpStream::connect(self.addr);
        match thread.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(ServeError::ServerStopped {
                reason: "the canned server failed".into(),
                source: Some(Box::new(error)),
            }),
            Err(_) => Err(ServeError::ServerStopped {
                reason: "the canned server panicked".into(),
                source: None,
            }),
        }
    }
}

impl Drop for CannedServer {
    fn drop(&mut self) {
        if let Err(error) = self.stop_inner() {
            match std::error::Error::source(&error) {
                Some(source) => eprintln!("erp-testkit canned server: {error}: {source}"),
                None => eprintln!("erp-testkit canned server: {error}"),
            }
        }
    }
}

fn serve_canned(listener: &TcpListener, canned: &[u8], stop: &AtomicBool) -> std::io::Result<()> {
    use std::io::Read;
    let mut buffer = vec![0u8; 16 * 1024];
    loop {
        let (mut stream, _) = listener.accept()?;
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        stream.set_nodelay(true)?;
        let mut filled = 0;
        loop {
            let n = stream.read(&mut buffer[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
            if buffer[..filled].ends_with(b"\r\n\r\n") {
                stream.write_all(canned)?;
                filled = 0;
            } else if filled == buffer.len() {
                return Err(std::io::Error::other("a request head larger than 16 KiB"));
            }
        }
    }
}

/// Send `client`'s request once and require a 2xx/3xx answer on a connection the server keeps
/// open. The client reconnects after a `Connection: close`, so a server that closes would be
/// measured across reconnects, with the gauge counting connections late. Allocates nothing.
fn send_kept_open(client: &mut RawClient, during: &'static str) -> Result<(), ServeError> {
    let expected = client.expected_status();
    let meta = client.send()?.meta();
    expect_status(meta.status, expected)?;
    if meta.close {
        return Err(ServeError::ConnectionNotKeptOpen { during });
    }
    Ok(())
}

/// `status` is `expected` when the client names one, else a 2xx or 3xx status.
fn expect_status(status: u16, expected: Option<u16>) -> Result<(), ServeError> {
    let accepted = match expected {
        Some(want) => status == want,
        None => (200..400).contains(&status),
    };
    if accepted {
        Ok(())
    } else {
        Err(ServeError::UnexpectedStatus { status, expected })
    }
}

fn to_u64(n: usize) -> u64 {
    // usize is at most 64 bits on every target the workspace builds for
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn the_rule_allows_c_row_per_added_row_and_no_more() {
        let rule = ResultSizeRule::new(4);
        assert_eq!(
            rule.check_blocks((10, 40), 100, [220, 219])
                .map_err(|e| e.to_string()),
            Ok(())
        );
        assert_eq!(
            rule.check_blocks((10, 40), 100, [220, 221])
                .unwrap_err()
                .to_string(),
            "result-size rule broken: request 3 at the large size allocated 221 blocks, \
             allowed 220 (100 at the small size + c_row 4 × 30 added rows)"
        );
        // at the gate the blocks must not grow at all
        let gate = ResultSizeRule::new(C_ROW_GATE);
        assert_eq!(
            gate.check_blocks((10, 40), 100, [100, 100])
                .map_err(|e| e.to_string()),
            Ok(())
        );
        assert_eq!(
            gate.check_blocks((10, 40), 100, [101, 100])
                .unwrap_err()
                .to_string(),
            "result-size rule broken: request 2 at the large size allocated 101 blocks, \
             allowed 100 (100 at the small size + c_row 0 × 30 added rows)"
        );
        // equal sizes: no added rows, no allowance
        assert_eq!(
            ResultSizeRule::new(4)
                .check_blocks((10, 10), 5, [5, 6])
                .unwrap_err()
                .to_string(),
            "result-size rule broken: request 3 at the large size allocated 6 blocks, \
             allowed 5 (5 at the small size + c_row 4 × 0 added rows)"
        );
    }

    #[test]
    fn the_rule_refuses_impossible_inputs() {
        assert_eq!(
            ResultSizeRule::new(u64::MAX)
                .check_blocks((10, 40), 1, [1, 1])
                .unwrap_err()
                .to_string(),
            "bad harness argument: the result-size allowance overflows"
        );
        assert_eq!(
            ResultSizeRule::new(1)
                .check_blocks((40, 10), 1, [1, 1])
                .unwrap_err()
                .to_string(),
            "bad harness argument: the large request fetches fewer rows"
        );
    }

    #[test]
    fn per_unit_costs_divide_the_difference() {
        assert_eq!(exact_per_unit(35, 65, 10), Some(3));
        assert_eq!(fixed_part(35, 65, 10), Some(5));
        assert_eq!(exact_per_unit(35, 66, 10), None);
        assert_eq!(fixed_part(35, 66, 10), None);
        // the second window allocating less is not a cost per unit
        assert_eq!(exact_per_unit(35, 25, 10), None);
        assert_eq!(exact_per_unit(0, 0, 0), None);
        assert_eq!(exact_per_unit(u64::MAX - 1, u64::MAX, 1), Some(1));
        assert_eq!(
            fixed_part(u64::MAX, u64::MAX, u64::MAX),
            Some(i128::from(u64::MAX))
        );
    }

    #[test]
    fn the_c_row_constant_is_the_interim_value_until_the_gate() {
        assert_eq!(C_ROW, C_ROW_INTERIM);
        assert_eq!((C_ROW_INTERIM, C_ROW_GATE), (3, 0));
    }

    #[test]
    fn future_size_reports_the_unpolled_state() {
        let small = async {};
        let big = async {
            let buffer = [0u8; 512];
            std::future::ready(()).await;
            std::hint::black_box(buffer);
        };
        assert!(future_size(&small) < 8);
        assert!(future_size(&big) >= 512);
        let size = crate::future_size!(async {}, <= 8);
        assert!(size <= 8);
    }

    #[test]
    #[should_panic(expected = "future size")]
    fn future_size_macro_fails_over_the_ceiling() {
        let _size = crate::future_size!(
            async {
                let buffer = [0u8; 512];
                std::future::ready(()).await;
                std::hint::black_box(buffer);
            },
            <= 64
        );
    }
}
