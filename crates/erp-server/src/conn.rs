//! Bounded HTTP/1 connections.
//!
//! Three limits, each with its own clock:
//! - the header deadline runs from the first byte of a request head ([`HeadDeadline`]);
//! - the idle limit bounds the wait for the next request (hyper's header-read timer, which hyper
//!   starts when the connection begins waiting for a head);
//! - the age limit stops a connection taking new requests: the in-flight request finishes, then the
//!   connection closes, with a hard bound [`DRAIN`] later.
//!
//! Observed: hyper 1.12 has one timer (`header_read_timeout`) that starts when the connection waits
//! for a head, so it limits idle time and head time together (`vendor/hyper/src/proto/h1/conn.rs`,
//! `poll_read_head`). Adopted: that timer as the idle limit. Modified: a separate head deadline that
//! starts only when bytes arrive, so an idle keep-alive connection is no longer closed at the head
//! deadline (PERF.SERVER-IDLE-KEEPALIVE).
use axum::Router;
use hyper::service::Service;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    time::{Instant, Sleep},
};

/// Time an old connection gets to finish its in-flight request before it is dropped.
pub const DRAIN: Duration = Duration::from_secs(10);

/// A refused connection limit.
#[derive(Debug, Error)]
#[error("HTTP connection limits must be header 1-60 s < idle 5-600 s < age 10-3600 s")]
pub struct InvalidLimits;

/// Validated connection limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectionLimits {
    header_deadline: Duration,
    idle: Duration,
    max_age: Duration,
}
impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            header_deadline: Duration::from_secs(5),
            idle: Duration::from_secs(30),
            max_age: Duration::from_secs(120),
        }
    }
}
impl ConnectionLimits {
    /// # Errors
    /// A value outside its range, or an order other than header < idle < age.
    pub fn new(header_deadline: u64, idle: u64, max_age: u64) -> Result<Self, InvalidLimits> {
        if !(1..=60).contains(&header_deadline)
            || !(5..=600).contains(&idle)
            || !(10..=3600).contains(&max_age)
            || header_deadline >= idle
            || idle >= max_age
        {
            return Err(InvalidLimits);
        }
        Ok(Self {
            header_deadline: Duration::from_secs(header_deadline),
            idle: Duration::from_secs(idle),
            max_age: Duration::from_secs(max_age),
        })
    }
    /// Time allowed for a request head, from its first byte.
    pub fn header_deadline(&self) -> Duration {
        self.header_deadline
    }
    /// Time an idle keep-alive connection waits for its next request.
    pub fn idle(&self) -> Duration {
        self.idle
    }
    /// Age at which a connection stops taking new requests.
    pub fn max_age(&self) -> Duration {
        self.max_age
    }
}

/// Request counters shared by the IO wrapper and the service wrapper of one connection. The
/// connection waits for a head while every started request has finished.
#[derive(Default)]
struct Phase {
    started: AtomicU64,
    finished: AtomicU64,
}
impl Phase {
    /// `Some(n)` while waiting for the head of request `n`; `None` while a request is handled.
    fn awaiting(&self) -> Option<u64> {
        let started = self.started.load(Ordering::Acquire);
        (self.finished.load(Ordering::Acquire) == started).then_some(started)
    }
}

/// Reads with a deadline that starts at the first byte of a request head and ends when the
/// request reaches the service.
struct HeadDeadline<T> {
    inner: T,
    phase: Arc<Phase>,
    limit: Duration,
    /// The request whose head the timer belongs to, and the timer.
    timer: Option<(u64, Pin<Box<Sleep>>)>,
}
impl<T: AsyncRead + Unpin> AsyncRead for HeadDeadline<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let awaiting = this.phase.awaiting();
        if this
            .timer
            .as_ref()
            .is_some_and(|(request, _)| awaiting != Some(*request))
        {
            this.timer = None;
        }
        if let Some((_, sleep)) = this.timer.as_mut()
            && sleep.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP request head deadline expired",
            )));
        }
        let before = buf.filled().len();
        let read = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let (Some(request), None, Poll::Ready(Ok(()))) = (awaiting, &this.timer, &read)
            && buf.filled().len() > before
        {
            let mut sleep = Box::pin(tokio::time::sleep(this.limit));
            // Register the waker so the deadline fires even if no more bytes arrive.
            let _ = sleep.as_mut().poll(cx);
            this.timer = Some((request, sleep));
        }
        read
    }
}
impl<T: AsyncWrite + Unpin> AsyncWrite for HeadDeadline<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Counts requests in and out of the service, so [`HeadDeadline`] knows when a head is awaited.
#[derive(Clone)]
struct Tracked<S> {
    inner: S,
    phase: Arc<Phase>,
}
/// Marks a request finished when its response future completes or is dropped.
struct Finished(Arc<Phase>);
impl Drop for Finished {
    fn drop(&mut self) {
        self.0.finished.fetch_add(1, Ordering::AcqRel);
    }
}
impl<S, R> Service<R> for Tracked<S>
where
    S: Service<R>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;
    fn call(&self, request: R) -> Self::Future {
        self.phase.started.fetch_add(1, Ordering::AcqRel);
        let finished = Finished(Arc::clone(&self.phase));
        let response = self.inner.call(request);
        Box::pin(async move {
            let _finished = finished;
            response.await
        })
    }
}

/// Serve one accepted connection under `limits`.
pub async fn serve(socket: TcpStream, router: Router, limits: ConnectionLimits) {
    serve_until(socket, router, limits, std::future::pending()).await;
}

/// Serve one connection until its age limit or an explicit server shutdown.
/// The current request may finish; keep-alive admission stops immediately. The caller may
/// impose a shorter whole-server drain deadline by dropping this future.
pub async fn serve_until(
    socket: TcpStream,
    router: Router,
    limits: ConnectionLimits,
    shutdown: impl Future<Output = ()>,
) {
    serve_draining(socket, router, limits, shutdown, DRAIN).await;
}

pub(crate) async fn serve_draining(
    socket: TcpStream,
    router: Router,
    limits: ConnectionLimits,
    shutdown: impl Future<Output = ()>,
    shutdown_drain: Duration,
) {
    let phase = Arc::new(Phase::default());
    let io = HeadDeadline {
        inner: socket,
        phase: Arc::clone(&phase),
        limit: limits.header_deadline,
        timer: None,
    };
    let service = Tracked {
        inner: hyper_util::service::TowerToHyperService::new(router),
        phase,
    };
    let mut builder = hyper::server::conn::http1::Builder::new();
    builder
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(limits.idle)
        .max_headers(64);
    let connection = builder.serve_connection(hyper_util::rt::TokioIo::new(io), service);
    tokio::pin!(connection);
    let opened = Instant::now();
    let age = tokio::time::sleep_until(opened + limits.max_age);
    tokio::pin!(age, shutdown);
    let drain = tokio::select! {
        result = connection.as_mut() => {
            if result.is_err() { tracing::debug!("HTTP client connection ended with protocol or I/O error"); }
            return;
        },
        () = &mut age => DRAIN,
        () = &mut shutdown => shutdown_drain,
    };
    connection.as_mut().graceful_shutdown();
    let result = match tokio::time::timeout(drain, connection.as_mut()).await {
        Ok(result) => result,
        Err(_) => {
            tracing::debug!("HTTP connection drain expired");
            return;
        }
    };
    if result.is_err() {
        tracing::debug!("HTTP client connection ended with protocol or I/O error");
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "functional shutdown regression"
)]
mod shutdown_tests {
    use super::*;
    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn configured_shutdown_deadline_drops_stalled_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(1);
        let router = Router::new().route(
            "/",
            get(move || {
                let started = started_tx.clone();
                async move {
                    started.send(()).await.unwrap();
                    std::future::pending::<String>().await
                }
            }),
        );
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            serve_draining(
                socket,
                router,
                ConnectionLimits::default(),
                async {
                    stop_rx.await.unwrap();
                },
                Duration::from_millis(50),
            )
            .await;
        });
        let mut client = TcpStream::connect(address).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
            .await
            .unwrap()
            .unwrap();
        stop_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        let mut byte = [0];
        assert_eq!(client.read(&mut byte).await.unwrap(), 0);
    }
}

/// How the server's accept loop treats a failed `accept()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AcceptFailure {
    /// The peer went away before the handshake completed; accept the next connection.
    Skip,
    /// The process or host is out of descriptors, socket buffers or memory. Pausing new
    /// connections for [`ACCEPT_PAUSE`] lets existing connections finish and free resources;
    /// stopping the server here would let any client that holds enough idle sockets end it.
    Pause,
    /// The listener itself failed; the server stops and reports the error.
    Fatal,
}

/// How long the accept loop stops taking new connections after an [`AcceptFailure::Pause`].
pub const ACCEPT_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

/// Classify an `accept()` error (review M1).
#[must_use]
pub fn accept_failure(error: &std::io::Error) -> AcceptFailure {
    use std::io::ErrorKind;
    if matches!(
        error.kind(),
        ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionRefused
            | ErrorKind::Interrupted
    ) {
        return AcceptFailure::Skip;
    }
    // Pending network errors that Linux accept(2) reports for the new connection, not the
    // listener; its manual says to retry them like EAGAIN.
    #[cfg(target_os = "linux")]
    const PEER_ERRORS: &[i32] = &[
        libc::ENETDOWN,
        libc::EPROTO,
        libc::ENOPROTOOPT,
        libc::EHOSTDOWN,
        libc::ENONET,
        libc::EHOSTUNREACH,
        libc::ENETUNREACH,
    ];
    #[cfg(not(target_os = "linux"))]
    const PEER_ERRORS: &[i32] = &[
        libc::ENETDOWN,
        libc::EPROTO,
        libc::ENOPROTOOPT,
        libc::EHOSTDOWN,
        libc::EHOSTUNREACH,
        libc::ENETUNREACH,
    ];
    if error
        .raw_os_error()
        .is_some_and(|code| PEER_ERRORS.contains(&code))
    {
        return AcceptFailure::Skip;
    }
    match error.raw_os_error() {
        Some(code)
            if code == libc::EMFILE
                || code == libc::ENFILE
                || code == libc::ENOBUFS
                || code == libc::ENOMEM =>
        {
            AcceptFailure::Pause
        }
        _ => AcceptFailure::Fatal,
    }
}

#[cfg(test)]
mod accept_failure_tests {
    use super::{AcceptFailure, accept_failure};
    use std::io::{Error, ErrorKind};

    #[test]
    fn descriptor_and_buffer_exhaustion_pause_instead_of_stopping() {
        for code in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert_eq!(
                accept_failure(&Error::from_raw_os_error(code)),
                AcceptFailure::Pause,
                "errno {code}"
            );
        }
    }

    #[test]
    fn aborted_handshakes_are_skipped() {
        for kind in [
            ErrorKind::ConnectionAborted,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionRefused,
            ErrorKind::Interrupted,
        ] {
            assert_eq!(
                accept_failure(&Error::from(kind)),
                AcceptFailure::Skip,
                "{kind:?}"
            );
        }
        assert_eq!(
            accept_failure(&Error::from_raw_os_error(libc::ECONNABORTED)),
            AcceptFailure::Skip
        );
    }

    #[test]
    fn pending_network_errors_of_the_new_connection_are_skipped() {
        for code in [
            libc::ENETDOWN,
            libc::EPROTO,
            libc::ENOPROTOOPT,
            libc::EHOSTDOWN,
            libc::EHOSTUNREACH,
            libc::ENETUNREACH,
        ] {
            assert_eq!(
                accept_failure(&Error::from_raw_os_error(code)),
                AcceptFailure::Skip,
                "errno {code}"
            );
        }
        #[cfg(target_os = "linux")]
        assert_eq!(
            accept_failure(&Error::from_raw_os_error(libc::ENONET)),
            AcceptFailure::Skip
        );
    }

    #[test]
    fn other_listener_errors_stay_fatal() {
        for code in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert_eq!(
                accept_failure(&Error::from_raw_os_error(code)),
                AcceptFailure::Fatal,
                "errno {code}"
            );
        }
        assert_eq!(
            accept_failure(&Error::other("listener closed")),
            AcceptFailure::Fatal
        );
    }
}
