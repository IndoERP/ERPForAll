//! [`ServeProbe`]: a server under test on its own thread, with the hooks the harness reads.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use super::ServeError;

/// How long the harness waits for the server to become quiescent or idle before it fails.
pub const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The off-path work a server still has to finish: every read `COMMIT` answered, the touch queue
/// flushed, the pool idle (perf-serve §3). The server takes an [`OffPath`] guard **before it writes
/// the response** that triggers the work and drops it when the work is done; the harness reads
/// the counts only once no guard is held, so off-path allocations land in the measured window.
///
/// Clones share the count; cloning and [`Quiescence::begin`] do not allocate.
#[derive(Debug, Clone, Default)]
pub struct Quiescence(Arc<AtomicU64>);

impl Quiescence {
    /// A new hook with nothing pending.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Off-path work starts; it ends when the guard is dropped.
    #[must_use = "the work ends when the guard is dropped"]
    pub fn begin(&self) -> OffPath {
        self.0.fetch_add(1, Ordering::AcqRel);
        OffPath(self.clone())
    }

    /// Guards currently held.
    #[must_use]
    pub fn pending(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    /// Block the calling thread until no guard is held.
    ///
    /// # Errors
    /// [`ServeError::Timeout`] when work is still pending after `limit`.
    pub fn wait(&self, limit: Duration) -> Result<(), ServeError> {
        wait_until(limit, "off-path work to finish (quiescence hook)", || {
            self.pending() == 0
        })
    }
}

/// One piece of off-path work in progress ([`Quiescence::begin`]).
#[derive(Debug)]
pub struct OffPath(Quiescence);

impl Drop for OffPath {
    fn drop(&mut self) {
        (self.0).0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Connections of the server under test: how many are open and how many are idle (served a
/// response, flushed it, and are waiting for the next request). Kept by [`GaugedIo`]; clones
/// share the counts.
#[derive(Debug, Clone, Default)]
pub struct ConnGauge(Arc<GaugeCounts>);

#[derive(Debug, Default)]
struct GaugeCounts {
    open: AtomicU64,
    idle: AtomicU64,
}

impl ConnGauge {
    /// A gauge with no connection.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Connections wrapped in a [`GaugedIo`] and not dropped yet.
    #[must_use]
    pub fn open(&self) -> u64 {
        self.0.open.load(Ordering::Acquire)
    }

    /// Open connections that are idle.
    #[must_use]
    pub fn idle(&self) -> u64 {
        self.0.idle.load(Ordering::Acquire)
    }
}

/// A connection's IO wrapped so the [`ConnGauge`] knows when it is idle.
///
/// A connection is idle when it owes nothing and waits for input: every byte of input it received
/// has been followed by a write and a successful flush (or it has received nothing yet), nothing
/// written is unflushed, and its last `poll_read` returned `Pending`. That is the server's own view
/// of "waiting for the next request with nothing to send", read from the IO calls, with no hook
/// inside the HTTP library. Known limits:
/// - a server that never calls `poll_flush` after writing never becomes idle (the harness then
///   times out rather than reading early);
/// - a server that reads input and writes nothing after it (it drains the rest of a request body
///   after its response was flushed, or discards bytes it does not answer) never becomes idle
///   either: input sets "owes", and only a write and a flush clear it. The harness times out
///   rather than reading early; measure such a server with requests whose bodies it reads before
///   answering;
/// - an interim response (`100 Continue`) followed by a wait for the body reads as idle.
///
/// Wrapping allocates nothing (the gauge handle is a reference count).
#[derive(Debug)]
pub struct GaugedIo<T> {
    io: T,
    gauge: ConnGauge,
    /// The last `poll_read` returned `Pending`.
    read_pending: bool,
    /// Input arrived and no write followed by a flush has happened since.
    owes: bool,
    /// Bytes were written since input last arrived.
    wrote_since_input: bool,
    /// Bytes were written since the last successful flush.
    unflushed: bool,
    /// This connection is counted in [`ConnGauge::idle`].
    idle: bool,
}

impl<T> GaugedIo<T> {
    /// Wrap `io`, counting it as open in `gauge` until it is dropped.
    pub fn new(io: T, gauge: &ConnGauge) -> Self {
        gauge.0.open.fetch_add(1, Ordering::AcqRel);
        Self {
            io,
            gauge: gauge.clone(),
            read_pending: false,
            owes: false,
            wrote_since_input: false,
            unflushed: false,
            idle: false,
        }
    }

    /// The wrapped IO.
    pub fn get_ref(&self) -> &T {
        &self.io
    }

    /// Recompute idleness from the state and update the gauge on a change.
    fn update(&mut self) {
        let idle = self.read_pending && !self.owes && !self.unflushed;
        if self.idle != idle {
            self.idle = idle;
            if idle {
                self.gauge.0.idle.fetch_add(1, Ordering::AcqRel);
            } else {
                self.gauge.0.idle.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    fn wrote(&mut self, n: usize) {
        if n > 0 {
            self.wrote_since_input = true;
            self.unflushed = true;
            self.update();
        }
    }
}

impl<T> Drop for GaugedIo<T> {
    fn drop(&mut self) {
        self.read_pending = false;
        self.update();
        self.gauge.0.open.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for GaugedIo<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.io).poll_read(cx, buf);
        match &polled {
            Poll::Pending => this.read_pending = true,
            Poll::Ready(Ok(())) if buf.filled().len() > before => {
                this.read_pending = false;
                this.owes = true;
                this.wrote_since_input = false;
            }
            // end of input or an error: the connection is closing, not waiting
            Poll::Ready(_) => this.read_pending = false,
        }
        this.update();
        polled
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for GaugedIo<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = polled {
            this.wrote(n);
        }
        polled
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = polled {
            this.wrote(n);
        }
        polled
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.io).poll_flush(cx);
        if let Poll::Ready(Ok(())) = polled {
            this.unflushed = false;
            if this.wrote_since_input {
                this.owes = false;
            }
            this.update();
        }
        polled
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

/// What the server future receives from [`ServeProbe::start`].
#[derive(Debug)]
#[non_exhaustive]
pub struct ServeContext {
    /// Bound to a free port on 127.0.0.1; the probe's address.
    pub listener: TcpListener,
    /// The quiescence hook: take a guard for every piece of off-path work.
    pub quiet: Quiescence,
    /// Wrap every accepted connection in a [`GaugedIo`] with this gauge, exactly once. The
    /// measurements compare it with the connections the harness holds (`==`): while one runs,
    /// the test holds no connection to the server other than the ones it passed in.
    pub connections: ConnGauge,
}

/// A server under test, run on a current-thread Tokio runtime (with a `LocalSet`, so
/// `spawn_local` works) on its own thread, and the hooks the harness reads.
///
/// Stop it with [`ServeProbe::stop`] to see the server's own error; dropping it stops it too and
/// reports an error only on stderr.
#[derive(Debug)]
pub struct ServeProbe {
    addr: SocketAddr,
    quiet: Quiescence,
    connections: ConnGauge,
    shutdown: Arc<Notify>,
    /// Wakes the sync task on the server thread ([`ServeProbe::settle`]).
    ping: Arc<Notify>,
    /// Pings the sync task has answered.
    acks: Arc<AtomicU64>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl ServeProbe {
    /// Start the server future `serve` builds from its [`ServeContext`]. Returns once the
    /// listener is bound.
    ///
    /// # Errors
    /// [`ServeError::ServerStart`] when the runtime cannot be built or the listener cannot bind.
    pub fn start<F, Fut>(serve: F) -> Result<Self, ServeError>
    where
        F: FnOnce(ServeContext) -> Fut + Send + 'static,
        Fut: Future<Output = io::Result<()>> + 'static,
    {
        let quiet = Quiescence::new();
        let connections = ConnGauge::new();
        let shutdown = Arc::new(Notify::new());
        let ping = Arc::new(Notify::new());
        let acks = Arc::new(AtomicU64::new(0));
        let (bound_tx, bound_rx) = std::sync::mpsc::sync_channel::<io::Result<SocketAddr>>(1);
        let thread = {
            let quiet = quiet.clone();
            let connections = connections.clone();
            let shutdown = Arc::clone(&shutdown);
            let ping = Arc::clone(&ping);
            let acks = Arc::clone(&acks);
            std::thread::Builder::new()
                .name("serve-probe".into())
                .spawn(move || -> io::Result<()> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    // Each `bound_tx.send` below can only fail when `start` has stopped waiting
                    // for it, which it never does before it has a message; the error itself
                    // travels in the message and in the thread's result.
                    let runtime = match runtime {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            let message = error.to_string();
                            let _sent = bound_tx.send(Err(error));
                            return Err(io::Error::other(message));
                        }
                    };
                    let local = tokio::task::LocalSet::new();
                    local.block_on(&runtime, async move {
                        let listener = match TcpListener::bind(("127.0.0.1", 0)).await {
                            Ok(listener) => listener,
                            Err(error) => {
                                let message = error.to_string();
                                let _sent = bound_tx.send(Err(error));
                                return Err(io::Error::other(message));
                            }
                        };
                        let addr = listener.local_addr();
                        let addr = match addr {
                            Ok(addr) => addr,
                            Err(error) => {
                                let message = error.to_string();
                                let _sent = bound_tx.send(Err(error));
                                return Err(io::Error::other(message));
                            }
                        };
                        // the receiver waits for this message; it can only be gone if start()
                        // itself unwound, and then nobody reads the probe anyway
                        let _sent = bound_tx.send(Ok(addr));
                        // answers settle()'s pings: when it runs, the poll that was in progress on
                        // this thread when the ping was sent has returned
                        tokio::task::spawn_local(async move {
                            loop {
                                ping.notified().await;
                                acks.fetch_add(1, Ordering::AcqRel);
                            }
                        });
                        let context = ServeContext {
                            listener,
                            quiet,
                            connections,
                        };
                        tokio::select! {
                            served = serve(context) => served,
                            () = shutdown.notified() => Ok(()),
                        }
                    })
                })
                .map_err(|source| ServeError::ServerStart { source })?
        };
        let addr = match bound_rx.recv() {
            Ok(Ok(addr)) => addr,
            Ok(Err(source)) => {
                // the thread returns the same error it sent; `source` is the one reported
                let _joined = thread.join();
                return Err(ServeError::ServerStart { source });
            }
            Err(_) => {
                return Err(stopped(thread.join()));
            }
        };
        Ok(Self {
            addr,
            quiet,
            connections,
            shutdown,
            ping,
            acks,
            thread: Some(thread),
        })
    }

    /// The server's address.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The quiescence hook shared with the server.
    #[must_use]
    pub fn quiescence(&self) -> &Quiescence {
        &self.quiet
    }

    /// The connection gauge shared with the server.
    #[must_use]
    pub fn connections(&self) -> &ConnGauge {
        &self.connections
    }

    /// Fails when the server thread has ended (its future returned or panicked).
    ///
    /// # Errors
    /// [`ServeError::ServerStopped`].
    pub fn ensure_running(&self) -> Result<(), ServeError> {
        match &self.thread {
            Some(thread) if !thread.is_finished() => Ok(()),
            _ => Err(ServeError::ServerStopped {
                reason: "the server thread has ended before the measurement finished".into(),
                source: None,
            }),
        }
    }

    /// Block until no off-path work is pending and every open connection the server wrapped in
    /// a [`GaugedIo`] is idle, and the server thread has finished the poll that got it there: the
    /// boundary every measured window starts and ends on. Allocates nothing.
    ///
    /// The last step sends a ping to a task on the server thread and waits for its answer: the
    /// runtime runs one task at a time, so when the ping task runs, the connection task that set
    /// the last idle flag has returned from its poll (and freed what that poll freed).
    ///
    /// # Errors
    /// [`ServeError::Timeout`] after [`SETTLE_TIMEOUT`], [`ServeError::ServerStopped`].
    pub fn settle(&self) -> Result<(), ServeError> {
        let start = Instant::now();
        loop {
            self.ensure_running()?;
            let left = SETTLE_TIMEOUT.saturating_sub(start.elapsed());
            self.quiet.wait(left)?;
            wait_until(left, "every open connection to be idle", || {
                self.connections.idle() == self.connections.open()
            })?;
            let before = self.acks.load(Ordering::Acquire);
            self.ping.notify_one();
            wait_until(left, "the server thread to answer the settle ping", || {
                self.acks.load(Ordering::Acquire) > before
            })?;
            if self.quiet.pending() == 0 && self.connections.idle() == self.connections.open() {
                return Ok(());
            }
            if start.elapsed() >= SETTLE_TIMEOUT {
                return Err(ServeError::Timeout {
                    waiting_for: "the server to stay settled across a ping",
                    after: SETTLE_TIMEOUT,
                });
            }
        }
    }

    /// Stop the server and return its outcome.
    ///
    /// # Errors
    /// [`ServeError::ServerStopped`] when the server future failed or panicked.
    pub fn stop(mut self) -> Result<(), ServeError> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> Result<(), ServeError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        self.shutdown.notify_one();
        match thread.join() {
            Ok(Ok(())) => Ok(()),
            joined => Err(stopped(joined)),
        }
    }
}

impl Drop for ServeProbe {
    fn drop(&mut self) {
        if let Err(error) = self.stop_inner() {
            // Drop cannot return it; `stop()` is the channel that does
            match std::error::Error::source(&error) {
                Some(source) => eprintln!("erp-testkit serve probe: {error}: {source}"),
                None => eprintln!("erp-testkit serve probe: {error}"),
            }
        }
    }
}

fn stopped(joined: std::thread::Result<io::Result<()>>) -> ServeError {
    let (reason, source): (String, Option<Box<dyn std::error::Error + Send + Sync>>) = match joined
    {
        Ok(Ok(())) => (
            "the server future returned before the probe was stopped".into(),
            None,
        ),
        Ok(Err(error)) => ("the server future failed".into(), Some(Box::new(error))),
        Err(panic) => {
            let text = panic
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned());
            let reason = match text {
                Some(text) => format!("the server thread panicked: {text}"),
                None => "the server thread panicked".into(),
            };
            (reason, None)
        }
    };
    ServeError::ServerStopped { reason, source }
}

/// Poll `done` on the calling thread until it holds or `limit` passes. Sleeping and reading the
/// clock allocate nothing, so this is safe inside a measured window.
pub(crate) fn wait_until(
    limit: Duration,
    waiting_for: &'static str,
    mut done: impl FnMut() -> bool,
) -> Result<(), ServeError> {
    let start = Instant::now();
    let mut pause = Duration::from_micros(10);
    loop {
        if done() {
            return Ok(());
        }
        if start.elapsed() >= limit {
            return Err(ServeError::Timeout {
                waiting_for,
                after: limit,
            });
        }
        std::thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_millis(1));
    }
}
