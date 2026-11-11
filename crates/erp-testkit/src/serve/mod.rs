//! The served-path harness (PERF.SERVE-HARNESS,,/D12):
//! allocations per request, heap per idle connection and future sizes of a server under test,
//! counted for the whole process with [`erp_alloc_budget::measure`].
//!
//! - [`ServeProbe`] runs the server future on a current-thread Tokio runtime on its own thread and
//!   hands it a [`ServeContext`]: the bound listener, the [`Quiescence`] hook (off-path work in
//!   progress) and the [`ConnGauge`] its [`GaugedIo`]-wrapped connections report to.
//! - [`RawClient`] drives it from the test thread with buffers allocated up front;
//!   [`client_probe`] proves it allocates nothing.
//! - [`ServeProbe::per_request`] gives allocations per request from windows of `n` and `2n`
//!   requests, each starting and ending on a settled server (no off-path work, every connection
//!   idle), so off-path allocations are inside the window.
//! - [`ServeProbe::per_idle_connection`] gives live heap per idle connection from runs of `c` and
//!   `2c` connections.
//! - [`future_size!`](crate::future_size) checks the size of an unpolled future;
//!   [`ServeProbe::result_size_rule`] checks the result-size rule with [`C_ROW`].
//!
//! ```no_run
//! # fn example() -> Result<(), erp_testkit::serve::ServeError> {
//! use erp_testkit::serve::{GaugedIo, RawClient, ServeContext, ServeProbe, DEFAULT_WARMUP};
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! // a server that answers every read with a fixed response
//! let probe = ServeProbe::start(|ctx: ServeContext| async move {
//!     loop {
//!         let (stream, _) = ctx.listener.accept().await?;
//!         let mut io = GaugedIo::new(stream, &ctx.connections);
//!         tokio::task::spawn_local(async move {
//!             let mut buf = vec![0u8; 4096];
//!             while io.read(&mut buf).await.is_ok_and(|n| n > 0) {
//!                 let wrote = io.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
//!                 if wrote.is_err() { break; }
//!             }
//!         });
//!     }
//! })?;
//! let mut client = RawClient::connect(probe.addr(), &b"GET / HTTP/1.1\r\nhost: x\r\n\r\n"[..], 4096)?;
//! let cost = probe.per_request(&mut client, DEFAULT_WARMUP, 100)?;
//! assert_eq!(cost.exact_blocks_per_request(), Some(0));
//! probe.stop()?;
//! # Ok(())
//! # }
//! ```
//!
//! **One measurement per process.** dhat counts every thread of the process, the server thread
//! included, so a measuring test lives in a test binary declared `harness = false` that lists its
//! tests with `erp_testkit::alloc::harness!` (they run on the main thread, with no libtest thread
//! allocating beside them, lesson; `cargo nextest`, what `scripts/ci.sh` runs, starts one
//! process per test), and the binary declares `#[global_allocator] static A:
//! erp_testkit::alloc::Alloc = erp_testkit::alloc::Alloc;`. dhat records a backtrace per
//! allocation: never time anything inside a measured window.

mod client;
mod measure;
mod probe;

use std::time::Duration;

pub use client::{
    CLIENT_IO_TIMEOUT, RawClient, RequestMethod, Response, ResponseMeta, frame_response,
};
pub use measure::{
    C_ROW, C_ROW_GATE, C_ROW_INTERIM, DEFAULT_WARMUP, PerIdle, PerRequest, ResultSize,
    ResultSizeRule, client_probe, future_size,
};
pub use probe::{
    ConnGauge, GaugedIo, OffPath, Quiescence, SETTLE_TIMEOUT, ServeContext, ServeProbe,
};

/// Why a served-path measurement could not be made, or failed its rule.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// A harness function was called with an argument it cannot use.
    #[error("bad harness argument: {0}")]
    BadArgument(&'static str),
    /// A socket operation of the client failed.
    #[error("client I/O failed while {during}")]
    Io {
        /// The step that failed.
        during: &'static str,
        /// The socket error.
        #[source]
        source: std::io::Error,
    },
    /// The client has no connection (an earlier error closed it and reconnecting failed).
    #[error("the client is not connected")]
    NotConnected,
    /// The server closed the connection before the response was complete.
    #[error("the server closed the connection after {received} bytes of an incomplete response")]
    ConnectionClosed {
        /// Bytes of the response received.
        received: usize,
    },
    /// The server answered with `Connection: close` where the measurement needs the connection
    /// kept open (a measured window, its warm-up, an idle-connection run): the client would
    /// reconnect and the measurement would include, or miss, the server's per-connection work.
    #[error(
        "the server closed the connection after a response during {during}: the measurement \
         needs its connections kept open"
    )]
    ConnectionNotKeptOpen {
        /// The step that sent the request.
        during: &'static str,
    },
    /// The response does not fit the client's fixed buffer (the buffer never grows: growing would
    /// allocate inside the measured window).
    #[error("the response does not fit the client's {capacity}-byte buffer")]
    ResponseTooLarge {
        /// The buffer's size.
        capacity: usize,
    },
    /// The response breaks HTTP/1.1 framing rules the client checks.
    #[error("malformed response: {0}")]
    MalformedResponse(&'static str),
    /// The response uses a framing the client does not read.
    #[error("unsupported response framing: {0}")]
    UnsupportedFraming(&'static str),
    /// Bytes arrived after the end of the response (the harness never pipelines).
    #[error("{extra} bytes arrived after the end of the response")]
    TrailingBytes {
        /// Bytes after the response.
        extra: usize,
    },
    /// The response status is not 2xx or 3xx, or not the status the client expects
    /// ([`RawClient::expect_status`]): the measured work is not the intended one.
    #[error(
        "the server answered with status {status}{}",
        .expected.map_or_else(String::new, |want| format!(", not the expected {want}"))
    )]
    UnexpectedStatus {
        /// The status code.
        status: u16,
        /// The status the client expects; `None`: any 2xx or 3xx.
        expected: Option<u16>,
    },
    /// A wait did not finish in time.
    #[error("timed out after {after:?} waiting for {waiting_for}")]
    Timeout {
        /// What was awaited.
        waiting_for: &'static str,
        /// The limit.
        after: Duration,
    },
    /// The server did not report exactly the open connections the harness holds within the time
    /// limit: it does not wrap each of its connections once in a [`GaugedIo`] (idleness cannot be
    /// read), it is slow to accept, it closed a client's connection, or the caller holds a
    /// connection the harness does not count.
    #[error(
        "the server reports {open} open connections within {waited:?}, expected {expected}: \
         either it does not wrap every accepted connection exactly once in \
         erp_testkit::serve::GaugedIo with the context's gauge, it is slow to accept, it closed \
         a client's connection, or the caller holds connections the harness does not count"
    )]
    ConnectionsNotGauged {
        /// Connections the gauge counts.
        open: u64,
        /// Connections the harness holds open (the gauge must count exactly these).
        expected: u64,
        /// How long the harness waited for them.
        waited: Duration,
    },
    /// The server thread could not be started.
    #[error("the server under test could not start")]
    ServerStart {
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// The server thread ended: its future returned, failed or panicked.
    #[error("the server under test stopped: {reason}")]
    ServerStopped {
        /// What ended it, or how it was noticed.
        reason: String,
        /// The server future's error, or the client error seen while the server had stopped.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },
    /// The client probe counted allocations: the client is not allocation-free and every
    /// measurement it drives would be wrong.
    #[error(
        "the raw client allocated {blocks} blocks ({bytes} bytes) against a server that does \
         no work: measurements through it are not exact"
    )]
    ClientAllocated {
        /// Blocks counted.
        blocks: u64,
        /// Bytes counted.
        bytes: u64,
    },
    /// The result-size rule failed (perf-serve §3).
    #[error(
        "result-size rule broken: request {request} at the large size allocated {blocks} \
         blocks, allowed {allowed} ({blocks_at_s} at the small size + c_row {c_row} × \
         {added_rows} added rows)"
    )]
    ResultSizeRule {
        /// 2 or 3: which request at the large size.
        request: usize,
        /// Its blocks.
        blocks: u64,
        /// The allowance.
        allowed: u64,
        /// Blocks of the request at the small size.
        blocks_at_s: u64,
        /// The per-row term used.
        c_row: u64,
        /// Rows added between the two sizes.
        added_rows: u64,
    },
    /// The allocation counter failed.
    #[error(transparent)]
    Alloc(#[from] erp_alloc_budget::AllocError),
}
