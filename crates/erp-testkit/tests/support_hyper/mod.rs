//! hyper servers for the served-path measurements (perf-serve §3 table): a `service_fn`
//! returning a 4 KiB static body and a JSON echo of a 2 KiB body, with or without a Tokio timer
//! (header read timeout), with or without a buffer provider (perf-serve §4.1).
//! Shared by `alloc_serve_hyper.rs` (stock) and `alloc_serve_hyper_provider.rs` (patched).
#![allow(
    dead_code,
    reason = "each test binary uses a part of the support module"
)]

use std::convert::Infallible;
use std::io;
use std::ops::Deref;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use erp_testkit::serve::{
    DEFAULT_WARMUP, GaugedIo, PerIdle, PerRequest, RawClient, ServeContext, ServeError, ServeProbe,
};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{CONTENT_TYPE, HeaderValue};
use hyper::server::conn::http1::{self, BufProvider};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub static BODY_4K: [u8; 4096] = [b'x'; 4096];

/// The request for the static service: a browser-like head of five headers.
pub const GET_STATIC: &[u8] = b"GET /static HTTP/1.1\r\nhost: bench\r\nuser-agent: erp-harness\r\n\
accept: */*\r\naccept-encoding: identity\r\nconnection: keep-alive\r\n\r\n";

/// Requests measured per window (the second window sends twice as many).
pub const N: usize = 200;
/// Connections of the first idle run (the second opens twice as many). Each connection is two
/// descriptors in this process (client and server end): 2 × 64 stays well under macOS's default
/// limit of 256 open files.
pub const IDLE_C: usize = 32;
/// G6 item 5: the HTTP layer's allocations per request (head parse, route match, body
/// channel, response head, write), steady state, whole process.
pub const HTTP_BLOCKS_CEILING: u64 = 8;
/// G6 item 3: heap per idle keep-alive connection, the gate.
pub const IDLE_GATE_BYTES: f64 = 16.0 * 1024.0;
/// G6 item 3: heap per idle keep-alive connection, the exit target.
pub const IDLE_TARGET_BYTES: f64 = 4.0 * 1024.0;

/// The client's response buffer.
pub const RESPONSE_CAPACITY: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Static4k,
    JsonEcho,
}

#[derive(Debug, Clone)]
pub struct Setup {
    pub service: Service,
    /// A Tokio timer with a 30 s header read timeout (hyper's default timeout needs a timer to
    /// take effect; `erp-server` needs it for `http.header_read_secs`).
    pub timer: bool,
    /// Lend buffers from this provider (the patched hyper's hook).
    pub provider: Option<Arc<TestPool>>,
    /// `writev(false)`: hyper's Flatten write strategy, bodies copied into the head buffer.
    pub flatten: bool,
    /// `pipeline_flush(true)`: hyper holds the flush while pipelined requests are buffered.
    pub pipeline_flush: bool,
    /// Count the server's reads on every connection.
    pub reads: Option<Arc<ReadCounter>>,
}

impl Setup {
    pub fn stock(service: Service, timer: bool) -> Self {
        Self {
            service,
            timer,
            provider: None,
            flatten: false,
            pipeline_flush: false,
            reads: None,
        }
    }

    pub fn lent(service: Service, timer: bool) -> Self {
        Self {
            provider: Some(TestPool::new()),
            ..Self::stock(service, timer)
        }
    }

    #[must_use]
    pub fn flattened(self) -> Self {
        Self {
            flatten: true,
            ..self
        }
    }

    #[must_use]
    pub fn with_pipeline_flush(self) -> Self {
        Self {
            pipeline_flush: true,
            ..self
        }
    }

    #[must_use]
    pub fn counting_reads(self) -> Self {
        Self {
            reads: Some(Arc::new(ReadCounter::default())),
            ..self
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{:?}{}{}{}{}",
            self.service,
            if self.timer { "+timer" } else { "" },
            if self.provider.is_some() {
                "+provider"
            } else {
                ""
            },
            if self.flatten { "+flatten" } else { "" },
            if self.pipeline_flush {
                "+pipeline_flush"
            } else {
                ""
            }
        )
    }
}

/// The server's reads on every connection of a run: calls that returned bytes, the bytes, and
/// the largest single read.
#[derive(Debug, Default)]
pub struct ReadCounter {
    pub reads: AtomicU64,
    pub bytes: AtomicU64,
    pub largest: AtomicU64,
}

impl ReadCounter {
    /// `(reads, bytes, largest read)`.
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.reads.load(Ordering::Acquire),
            self.bytes.load(Ordering::Acquire),
            self.largest.load(Ordering::Acquire),
        )
    }
}

/// A stream that counts its reads into a [`ReadCounter`]. Allocates nothing.
pub struct CountingIo<T> {
    inner: T,
    counter: Arc<ReadCounter>,
}

impl<T: AsyncRead + Unpin> AsyncRead for CountingIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = (buf.filled().len() - before) as u64;
        if n > 0 {
            // bytes last: a reader that waits for a byte count (Acquire) then sees the reads and
            // the largest read that came with those bytes
            self.counter.reads.fetch_add(1, Ordering::AcqRel);
            self.counter.largest.fetch_max(n, Ordering::AcqRel);
            self.counter.bytes.fetch_add(n, Ordering::AcqRel);
        }
        polled
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for CountingIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
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

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// The largest buffer the test pool keeps; larger ones given back (a read buffer grown for a large
/// body, a Flatten head buffer that held a body) are dropped, as `BufProvider`'s contract asks.
pub const POOL_KEEP_MAX: usize = 64 * 1024;

/// A buffer provider for the tests: a locked free list with capacity reserved up front, and
/// counters that say how many buffers the connections hold and whether a buffer came back with
/// bytes in it. `erp-server`'s provider is the per-thread cache.
#[derive(Debug)]
pub struct TestPool {
    reads: Mutex<Vec<BytesMut>>,
    writes: Mutex<Vec<Vec<u8>>>,
    /// Read buffers taken minus given back: held by connections now.
    pub reads_out: AtomicI64,
    /// Head buffers taken minus given back.
    pub writes_out: AtomicI64,
    /// Read buffers taken in all.
    pub read_takes: AtomicU64,
    /// Buffers given back that were not empty (hyper's contract says never).
    pub nonempty_gives: AtomicU64,
    /// Buffers the pool had to allocate.
    pub fresh: AtomicU64,
    /// Buffers given back above [`POOL_KEEP_MAX`] and dropped.
    pub dropped: AtomicU64,
}

impl TestPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            reads: Mutex::new(Vec::with_capacity(1024)),
            writes: Mutex::new(Vec::with_capacity(1024)),
            reads_out: AtomicI64::new(0),
            writes_out: AtomicI64::new(0),
            read_takes: AtomicU64::new(0),
            nonempty_gives: AtomicU64::new(0),
            fresh: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        })
    }

    pub fn held(&self) -> (i64, i64) {
        (
            self.reads_out.load(Ordering::Acquire),
            self.writes_out.load(Ordering::Acquire),
        )
    }

    pub fn nonempty(&self) -> u64 {
        self.nonempty_gives.load(Ordering::Acquire)
    }
}

impl BufProvider for TestPool {
    fn take_read(&self, min: usize) -> BytesMut {
        self.reads_out.fetch_add(1, Ordering::AcqRel);
        self.read_takes.fetch_add(1, Ordering::AcqRel);
        let cached = self.reads.lock().expect("pool lock").pop();
        if let Some(mut buf) = cached {
            // a remnant whose allocation is still shared with a live request is dropped
            if buf.capacity() >= min || buf.try_reclaim(min) {
                return buf;
            }
        }
        self.fresh.fetch_add(1, Ordering::AcqRel);
        BytesMut::with_capacity(min.max(8192))
    }

    fn give_read(&self, buf: BytesMut) {
        self.reads_out.fetch_sub(1, Ordering::AcqRel);
        if !buf.is_empty() {
            self.nonempty_gives.fetch_add(1, Ordering::AcqRel);
        }
        if buf.capacity() > POOL_KEEP_MAX {
            self.dropped.fetch_add(1, Ordering::AcqRel);
            return;
        }
        self.reads.lock().expect("pool lock").push(buf);
    }

    fn take_write(&self) -> Vec<u8> {
        self.writes_out.fetch_add(1, Ordering::AcqRel);
        let cached = self.writes.lock().expect("pool lock").pop();
        cached.unwrap_or_else(|| {
            self.fresh.fetch_add(1, Ordering::AcqRel);
            Vec::with_capacity(8192)
        })
    }

    fn give_write(&self, buf: Vec<u8>) {
        self.writes_out.fetch_sub(1, Ordering::AcqRel);
        if !buf.is_empty() {
            self.nonempty_gives.fetch_add(1, Ordering::AcqRel);
        }
        if buf.capacity() > POOL_KEEP_MAX {
            self.dropped.fetch_add(1, Ordering::AcqRel);
            return;
        }
        self.writes.lock().expect("pool lock").push(buf);
    }
}

pub fn echo_request_with(fill: u8) -> Vec<u8> {
    let mut body = br#"{"jsonrpc":"2.0","params":{"data":""#.to_vec();
    let tail = br#""}}"#;
    body.resize(2048 - tail.len(), fill);
    body.extend_from_slice(tail);
    assert_eq!(body.len(), 2048);
    let mut request = format!(
        "POST /echo HTTP/1.1\r\nhost: bench\r\nuser-agent: erp-harness\r\n\
         content-type: application/json\r\naccept: application/json\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    request
}

pub fn request_for(service: Service) -> Vec<u8> {
    match service {
        Service::Static4k => GET_STATIC.to_vec(),
        Service::JsonEcho => echo_request_with(b'a'),
    }
}

pub fn body_len_of(service: Service) -> usize {
    match service {
        Service::Static4k => 4096,
        Service::JsonEcho => 2048,
    }
}

async fn static_4k(_request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(Response::new(Full::new(Bytes::from_static(&BODY_4K))))
}

async fn json_echo(request: Request<Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let body = request.into_body().collect().await?.to_bytes();
    let mut response = Response::new(Full::new(body));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(response)
}

pub fn builder(setup: &Setup) -> http1::Builder {
    let mut builder = http1::Builder::new();
    if setup.flatten {
        builder.writev(false);
    }
    if setup.pipeline_flush {
        builder.pipeline_flush(true);
    }
    if setup.timer {
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_secs(30));
    }
    if let Some(provider) = &setup.provider {
        let provider: Arc<dyn BufProvider> = provider.clone();
        builder.buffer_provider(provider);
    }
    builder
}

/// A [`ServeProbe`] running a hyper server, with the count of connections that ended with an
/// error: [`HyperProbe::stop`] fails the test when any did (a measurement over a failing
/// connection is not the intended work).
pub struct HyperProbe {
    probe: ServeProbe,
    errors: Arc<AtomicU64>,
}

impl Deref for HyperProbe {
    type Target = ServeProbe;

    fn deref(&self) -> &ServeProbe {
        &self.probe
    }
}

impl HyperProbe {
    /// Stop the server; panics when a hyper connection ended with an error.
    ///
    /// # Errors
    /// [`ServeProbe::stop`]'s.
    pub fn stop(self) -> Result<(), ServeError> {
        self.probe.stop()?;
        let errors = self.errors.load(Ordering::Acquire);
        assert_eq!(
            errors, 0,
            "{errors} hyper connection(s) ended with an error (printed above)"
        );
        Ok(())
    }
}

pub fn start(setup: &Setup) -> HyperProbe {
    let setup = setup.clone();
    let errors = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&errors);
    let probe = ServeProbe::start(move |ctx: ServeContext| async move {
        let ServeContext {
            listener,
            connections,
            ..
        } = ctx;
        let builder = Rc::new(builder(&setup));
        loop {
            let (stream, _) = listener.accept().await?;
            stream.set_nodelay(true)?;
            let builder = Rc::clone(&builder);
            let service = setup.service;
            let errors = Arc::clone(&counted);
            match &setup.reads {
                None => {
                    let io = TokioIo::new(GaugedIo::new(stream, &connections));
                    tokio::task::spawn_local(serve(builder, io, service, errors));
                }
                Some(counter) => {
                    let counting = CountingIo {
                        inner: stream,
                        counter: Arc::clone(counter),
                    };
                    let io = TokioIo::new(GaugedIo::new(counting, &connections));
                    tokio::task::spawn_local(serve(builder, io, service, errors));
                }
            }
        }
    })
    .expect("probe starts");
    HyperProbe { probe, errors }
}

async fn serve<I>(builder: Rc<http1::Builder>, io: I, service: Service, errors: Arc<AtomicU64>)
where
    I: hyper::rt::Read + hyper::rt::Write + Unpin + 'static,
{
    let served = match service {
        Service::Static4k => builder.serve_connection(io, service_fn(static_4k)).await,
        Service::JsonEcho => builder.serve_connection(io, service_fn(json_echo)).await,
    };
    if let Err(error) = served {
        errors.fetch_add(1, Ordering::AcqRel);
        eprintln!("hyper connection ended with an error: {error}");
    }
}

pub fn per_request(setup: &Setup) -> PerRequest {
    let probe = start(setup);
    let mut client =
        RawClient::connect(probe.addr(), request_for(setup.service), RESPONSE_CAPACITY)
            .expect("client");
    let response = client.send().expect("first response");
    assert_eq!(
        (response.status(), response.meta().body_len),
        (200, body_len_of(setup.service))
    );
    let cost = probe
        .per_request(&mut client, DEFAULT_WARMUP, N)
        .expect("measured");
    drop(client);
    probe.stop().expect("server stops cleanly");
    eprintln!(
        "SERVE-HARNESS-RESULT per_request {}: blocks/request {:.3} (exact {:?}), bytes/request \
         {:.1}, fixed {:?}, windows {:?} / {:?}",
        setup.label(),
        cost.blocks_per_request(),
        cost.exact_blocks_per_request(),
        cost.bytes_per_request(),
        cost.fixed_blocks(),
        cost.at_n,
        cost.at_2n
    );
    if let Some(pool) = &setup.provider {
        assert_eq!(pool.nonempty(), 0, "a buffer came back with bytes in it");
    }
    cost
}

pub fn per_idle(setup: &Setup) -> PerIdle {
    let probe = start(setup);
    let idle = probe
        .per_idle_connection(&request_for(setup.service), IDLE_C, RESPONSE_CAPACITY)
        .expect("idle measured");
    probe.stop().expect("server stops cleanly");
    eprintln!(
        "SERVE-HARNESS-RESULT per_idle {}: bytes/idle connection {:.1}, live blocks/connection \
         {:.2}, allocations/connection {:.2}, runs {:?} / {:?}",
        setup.label(),
        idle.bytes_per_connection(),
        idle.blocks_per_connection(),
        idle.allocations_per_connection(),
        idle.at_c,
        idle.at_2c
    );
    if let Some(pool) = &setup.provider {
        assert_eq!(pool.nonempty(), 0, "a buffer came back with bytes in it");
    }
    idle
}

/// Exercise the real service and buffer ownership without a measurement window.
pub fn assert_service_response(setup: &Setup) {
    let request = request_for(setup.service);
    let expected = match setup.service {
        Service::Static4k => BODY_4K.to_vec(),
        Service::JsonEcho => {
            let head_end = request
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("head")
                + 4;
            request[head_end..].to_vec()
        }
    };
    let probe = start(setup);
    let mut client = RawClient::connect(probe.addr(), request, RESPONSE_CAPACITY).expect("client");
    for _ in 0..2 {
        let response = client.send().expect("response");
        assert_eq!(response.status(), 200);
        assert_eq!(response.content().expect("body"), expected.as_slice());
    }
    probe.settle().expect("settles");
    assert_eq!(probe.connections().open(), 1);
    if let Some(pool) = &setup.provider {
        assert_eq!(pool.held(), (0, 0));
        assert_eq!(pool.nonempty(), 0);
    }
    drop(client);
    probe.stop().expect("server stops cleanly");
}
