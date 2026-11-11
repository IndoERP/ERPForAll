//! Round trips counted at the wire (design note).
//!
//! The ORM counts its own statements (`Transaction::query_count`); that says nothing about how
//! many times the client waited for the server. [`WireCounter`] sits between the test pool and
//! the server as a small in-process TCP pass-through and counts **turns**: the number of times
//! the client sent bytes after having received bytes from the server. A statement alone is one
//! turn; two statements sent back to back before the first answer arrives are one turn; a
//! prepare followed by an execute are two. Connection start-up and authentication are not
//! counted (counting starts at the first `ReadyForQuery` of each connection).
//!
//! Nothing is read from the ORM: the figure comes from the bytes that crossed the socket.
//!
//! What a turn is, exactly: a client write after the server sent a *solicited* message since the
//! client's last write. `NoticeResponse`, `NotificationResponse` and `ParameterStatus` on their
//! own are not an answer to anything the client could wait for, so they do not start a turn. Not
//! counted: the `Terminate` a closing connection sends. Known limits: the `FATAL` error that ends
//! a session (`25P04`) is indistinguishable from an answer; a flight split over several client
//! writes (a multi-thread runtime, or more bytes than the socket buffer) can count one turn per
//! write when an answer arrives between the writes; a TLS connection cannot be read, and
//! [`assert_round_trips!`] fails when one crossed the proxy instead of passing with zero.
//!
//! A block starts on a settled wire. A request still unanswered when the block starts (the
//! `Close` that dropping a prepared statement sends without waiting for its answer, say) would
//! make the block's first flight look pipelined behind it: the flight is then part of a turn that
//! was counted before the block, and the block sees one round trip less than it waited for.
//! The proxy therefore keeps the number of requests it forwarded that the server has not answered
//! yet ([`WireCounter::outstanding`]: one per `Query`, `Sync` and `FunctionCall`, each answered by
//! exactly one `ReadyForQuery`), and [`assert_round_trips!`] waits until that number is zero
//! ([`WireCounter::settled`]) before it reads the count. Known limit: a request the client has
//! written but the proxy has not read yet is not visible to it.
//!
//! ```no_run
//! # async fn example() -> erp_testkit::Result<()> {
//! use erp_testkit::{TestDb, WireCounter, assert_round_trips};
//! let db = TestDb::create().await?;
//! let wire = WireCounter::start(&db.config()).await?;
//! let pool = wire.pool(2)?;
//! let client = pool.get().await?;
//! assert_round_trips!(wire, == 1, {
//!     client.batch_execute("SELECT 1").await.expect("select");
//! });
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use deadpool_postgres::Pool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_postgres::Config;
use tokio_postgres::config::Host;

use crate::{Result, TestkitError};

/// The counts the proxy keeps, shared by every connection and every clone of the handle.
struct Counters {
    /// Turns counted.
    turns: AtomicU64,
    /// How many times the counter was reset (a block compares it before and after).
    resets: AtomicU64,
    /// Connections that finished start-up and are being counted.
    armed: AtomicU64,
    /// Connections that negotiated TLS: their bytes cannot be read, so they are not counted.
    opaque: AtomicU64,
    /// Requests forwarded to the server and not answered yet, over all connections.
    outstanding: AtomicU64,
    /// Woken when `outstanding` drops to zero ([`WireCounter::settled`]).
    settle: Notify,
    /// `ReadyForQuery` messages that answered no request the proxy saw: its framing of the
    /// client's stream is wrong, so `outstanding` cannot be trusted.
    unmatched: AtomicU64,
    /// Client messages since the start or the last reset, one slot per [`MessageKind`].
    messages: [AtomicU64; MessageKind::COUNT],
    /// The turn count at the last [`WireCounter::response_sent`] since the last reset, or
    /// [`NO_SNAPSHOT`].
    at_response: AtomicU64,
    /// [`WireCounter::response_sent`] calls since the last reset.
    responses: AtomicU64,
}

/// [`Counters::at_response`] before any [`WireCounter::response_sent`] call.
const NO_SNAPSHOT: u64 = u64::MAX;

impl Default for Counters {
    fn default() -> Self {
        Self {
            turns: AtomicU64::new(0),
            resets: AtomicU64::new(0),
            armed: AtomicU64::new(0),
            opaque: AtomicU64::new(0),
            outstanding: AtomicU64::new(0),
            settle: Notify::new(),
            unmatched: AtomicU64::new(0),
            messages: std::array::from_fn(|_| AtomicU64::new(0)),
            at_response: AtomicU64::new(NO_SNAPSHOT),
            responses: AtomicU64::new(0),
        }
    }
}

/// The client messages [`WireCounter::messages`] counts by type (PostgreSQL protocol 3 type
/// bytes, frontend side).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageKind {
    Parse,
    Bind,
    Describe,
    Execute,
    Sync,
    Query,
    Close,
    Flush,
}

impl MessageKind {
    const COUNT: usize = 8;

    /// The kind of a frontend message type byte; `None` for the types not counted (`F`
    /// function call, `X` terminate, copy messages).
    fn of(type_byte: u8) -> Option<Self> {
        Some(match type_byte {
            b'P' => Self::Parse,
            b'B' => Self::Bind,
            b'D' => Self::Describe,
            b'E' => Self::Execute,
            b'S' => Self::Sync,
            b'Q' => Self::Query,
            b'C' => Self::Close,
            b'H' => Self::Flush,
            _ => return None,
        })
    }

    fn slot(self) -> usize {
        self as usize
    }
}

/// Client messages of each type that crossed a [`WireCounter`] since its start or its last
/// [`WireCounter::reset`], over all connections, start-up excluded (counting starts at each
/// connection's first `ReadyForQuery`, like the turns).
///
/// A named prepare is one `Parse` (plus the `Describe` and `Sync` tokio-postgres sends with it);
/// running a prepared statement is a `Bind`, an `Execute` and a `Sync`; a simple query is one
/// `Query`. Tests use it to assert, for example, that no `Parse` crosses the wire for statements
/// that should already be prepared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MessageCounts {
    /// `Parse` (`P`): a statement prepared, named or unnamed.
    pub parse: u64,
    /// `Bind` (`B`).
    pub bind: u64,
    /// `Describe` (`D`), of a statement or a portal.
    pub describe: u64,
    /// `Execute` (`E`).
    pub execute: u64,
    /// `Sync` (`S`).
    pub sync: u64,
    /// `Query` (`Q`): a simple-protocol query string.
    pub query: u64,
    /// `Close` (`C`), of a statement or a portal.
    pub close: u64,
    /// `Flush` (`H`).
    pub flush: u64,
}

impl Counters {
    /// `n` requests of `link` were answered (or will never be: its connection ended).
    fn answered(&self, link: &Link, n: u64) {
        let had = link
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |had| {
                Some(had.saturating_sub(n))
            })
            .unwrap_or_else(|had| had);
        let taken = had.min(n);
        if taken < n {
            self.unmatched.fetch_add(n - taken, Ordering::AcqRel);
        }
        if taken > 0 && self.outstanding.fetch_sub(taken, Ordering::AcqRel) == taken {
            self.settle.notify_waiters();
        }
    }
}

/// How long [`assert_round_trips!`] waits for the requests in flight before its block to be
/// answered before it fails.
const SETTLE_LIMIT: Duration = Duration::from_secs(60);

/// A handle on the round trips crossing a [`WireCounter`] proxy. Clones share the count.
#[derive(Clone)]
pub struct WireCounter {
    counters: Arc<Counters>,
    local: SocketAddr,
    upstream: Arc<Config>,
    // aborts the accept loop when the last handle goes
    _guard: Arc<AcceptGuard>,
}

struct AcceptGuard(tokio::task::AbortHandle);

impl Drop for AcceptGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl std::fmt::Debug for WireCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // the upstream config carries the password: never printed
        f.debug_struct("WireCounter")
            .field("listen", &self.local)
            .field("round_trips", &self.count())
            .finish_non_exhaustive()
    }
}

/// What the two directions of one proxied connection tell each other.
#[derive(Default)]
struct Link {
    /// The first `ReadyForQuery` was seen: start-up is over, turns count from here.
    started: AtomicBool,
    /// A solicited server message arrived since the client last sent.
    server_spoke: AtomicBool,
    /// The client opened with an `SSLRequest`: the server's one-byte reply is not a message.
    awaiting_ssl_reply: AtomicBool,
    /// This connection's share of [`Counters::outstanding`].
    outstanding: AtomicU64,
}

/// Gives back the unanswered requests of a connection when it ends, so a dead connection does
/// not keep the wire busy forever.
struct Hangup {
    link: Arc<Link>,
    counters: Arc<Counters>,
}

impl Drop for Hangup {
    fn drop(&mut self) {
        let left = self.link.outstanding.load(Ordering::Acquire);
        self.counters.answered(&self.link, left);
    }
}

/// The client messages the server answers with one `ReadyForQuery` each: `Query`, `Sync` and
/// `FunctionCall`.
fn is_request(kind: u8) -> bool {
    matches!(kind, b'Q' | b'S' | b'F')
}

/// The protocol code of an `SSLRequest` or `GSSENCRequest` (length 8, then the code), the
/// untyped message a client may open with.
fn is_encryption_request(first_write: &[u8]) -> bool {
    first_write.len() == 8
        && first_write[..4] == [0, 0, 0, 8]
        && (first_write[4..] == [0x04, 0xd2, 0x16, 0x2f]
            || first_write[4..] == [0x04, 0xd2, 0x16, 0x30])
}

/// The `Terminate` message a connection sends when it closes.
fn is_terminate(write: &[u8]) -> bool {
    write == [b'X', 0, 0, 0, 4]
}

impl WireCounter {
    /// Listen on a free local port and forward every connection to the server `upstream` names
    /// (the first TCP host and port of the configuration).
    ///
    /// # Errors
    /// `Configuration` when `upstream` names no TCP host or the listener cannot bind.
    pub async fn start(upstream: &Config) -> Result<Self> {
        let host = match upstream.get_hosts() {
            [Host::Tcp(host), ..] => host.clone(),
            _ => {
                return Err(TestkitError::Configuration(
                    "the wire counter forwards to a TCP host; the configuration names none".into(),
                ));
            }
        };
        // no port in the configuration means the PostgreSQL default, as for libpq
        let port = upstream.get_ports().first().copied().unwrap_or(5432);
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| TestkitError::Configuration(format!("wire counter cannot listen: {e}")))?;
        let local = listener
            .local_addr()
            .map_err(|e| TestkitError::Configuration(format!("wire counter address: {e}")))?;
        let counters = Arc::new(Counters::default());
        let accept = {
            let counters = Arc::clone(&counters);
            tokio::spawn(async move {
                loop {
                    let client = match listener.accept().await {
                        Ok((client, _)) => client,
                        Err(error) => {
                            // an accept error (descriptors exhausted) ends the forwarding for
                            // the rest of the test: say so instead of failing it silently
                            eprintln!(
                                "erp-testkit wire counter: accept failed, no more connections are forwarded: {error}"
                            );
                            return;
                        }
                    };
                    let counters = Arc::clone(&counters);
                    let target = (host.clone(), port);
                    tokio::spawn(async move {
                        if let Err(error) = forward(client, target, counters).await {
                            eprintln!("erp-testkit wire counter: connection ended: {error}");
                        }
                    });
                }
            })
        };
        Ok(Self {
            counters,
            local,
            upstream: Arc::new(upstream.clone()),
            _guard: Arc::new(AcceptGuard(accept.abort_handle())),
        })
    }

    /// The configuration of the upstream with host and port replaced by the proxy's; every
    /// other setting (user, password, database, `options`, application name, timeouts,
    /// keepalives, TLS mode, channel binding) is the upstream's, so a proxied pool behaves like
    /// the direct one. `target_session_attrs` and `load_balance_hosts` are not copied: the proxy
    /// forwards to the single host it was started with, so they have nothing to select.
    pub fn config(&self) -> Config {
        let up = &*self.upstream;
        let mut config = Config::new();
        config.host("127.0.0.1").port(self.local.port());
        if let Some(user) = up.get_user() {
            config.user(user);
        }
        if let Some(password) = up.get_password() {
            config.password(password);
        }
        if let Some(dbname) = up.get_dbname() {
            config.dbname(dbname);
        }
        if let Some(options) = up.get_options() {
            config.options(options);
        }
        if let Some(name) = up.get_application_name() {
            config.application_name(name);
        }
        config.ssl_mode(up.get_ssl_mode());
        config.ssl_negotiation(up.get_ssl_negotiation());
        if let Some(timeout) = up.get_connect_timeout() {
            config.connect_timeout(*timeout);
        }
        if let Some(timeout) = up.get_tcp_user_timeout() {
            config.tcp_user_timeout(*timeout);
        }
        config.keepalives(up.get_keepalives());
        config.keepalives_idle(up.get_keepalives_idle());
        if let Some(interval) = up.get_keepalives_interval() {
            config.keepalives_interval(interval);
        }
        if let Some(retries) = up.get_keepalives_retries() {
            config.keepalives_retries(retries);
        }
        config.channel_binding(up.get_channel_binding());
        config
    }

    /// A pool (with the PostgreSQL version gate) whose connections cross this proxy.
    ///
    /// # Errors
    /// The pool errors of [`erp_orm::pool::create_pool`].
    pub fn pool(&self, max_size: usize) -> Result<Pool> {
        Ok(erp_orm::pool::create_pool(self.config(), max_size)?)
    }

    /// Round trips counted since the start or the last [`WireCounter::reset`], over all
    /// connections.
    pub fn count(&self) -> u64 {
        self.counters.turns.load(Ordering::Acquire)
    }

    /// How many times [`WireCounter::reset`] was called (for [`assert_round_trips!`]).
    #[doc(hidden)]
    pub fn reset_generation(&self) -> u64 {
        self.counters.resets.load(Ordering::Acquire)
    }

    /// Start counting from zero: turns, [`WireCounter::messages`] and the response-sent snapshot
    /// ([`WireCounter::turns_at_response`]).
    pub fn reset(&self) {
        self.counters.resets.fetch_add(1, Ordering::AcqRel);
        self.counters.turns.store(0, Ordering::Release);
        for slot in &self.counters.messages {
            slot.store(0, Ordering::Release);
        }
        self.counters
            .at_response
            .store(NO_SNAPSHOT, Ordering::Release);
        self.counters.responses.store(0, Ordering::Release);
    }

    /// Client messages per type since the start or the last [`WireCounter::reset`], over all
    /// connections. Counted when the proxy reads them, before it forwards them, so a message the
    /// server has answered is always in the count.
    pub fn messages(&self) -> MessageCounts {
        let get = |kind: MessageKind| self.counters.messages[kind.slot()].load(Ordering::Acquire);
        MessageCounts {
            parse: get(MessageKind::Parse),
            bind: get(MessageKind::Bind),
            describe: get(MessageKind::Describe),
            execute: get(MessageKind::Execute),
            sync: get(MessageKind::Sync),
            query: get(MessageKind::Query),
            close: get(MessageKind::Close),
            flush: get(MessageKind::Flush),
        }
    }

    /// The "response sent" hook: the server under test calls it when the last byte of its
    /// response has been written, and the counter keeps the number of turns counted (since the
    /// last [`WireCounter::reset`]) at that moment. A test then asserts the round trips before the
    /// answer ([`WireCounter::turns_at_response`]) separately from the total ([`WireCounter::count`]):
    /// a read's `COMMIT` sent after the response is in the total, not in the snapshot.
    ///
    /// Each call replaces the snapshot; [`WireCounter::responses_sent`] says how many calls there
    /// were. Known limit: a flight the client wrote before the call but the proxy had not read yet
    /// is counted after the snapshot (the same limit as [`WireCounter::outstanding`]).
    pub fn response_sent(&self) {
        let turns = self.count();
        self.counters.at_response.store(turns, Ordering::Release);
        self.counters.responses.fetch_add(1, Ordering::AcqRel);
    }

    /// The turn count at the last [`WireCounter::response_sent`] since the last reset; `None`
    /// when the hook was not called since then.
    pub fn turns_at_response(&self) -> Option<u64> {
        match self.counters.at_response.load(Ordering::Acquire) {
            NO_SNAPSHOT => None,
            turns => Some(turns),
        }
    }

    /// [`WireCounter::response_sent`] calls since the last reset.
    pub fn responses_sent(&self) -> u64 {
        self.counters.responses.load(Ordering::Acquire)
    }

    /// Connections that finished start-up and are being counted.
    pub fn armed(&self) -> u64 {
        self.counters.armed.load(Ordering::Acquire)
    }

    /// Connections that use TLS: their turns cannot be counted.
    pub fn uncounted(&self) -> u64 {
        self.counters.opaque.load(Ordering::Acquire)
    }

    /// Requests (`Query`, `Sync`, `FunctionCall`) the proxy forwarded to the server whose
    /// `ReadyForQuery` has not come back yet, over all connections. Start-up is not counted, and
    /// a connection that ends takes its unanswered requests with it.
    pub fn outstanding(&self) -> u64 {
        self.counters.outstanding.load(Ordering::Acquire)
    }

    /// Wait until every request the proxy forwarded has been answered
    /// ([`WireCounter::outstanding`] is zero). Returns at once when it is.
    pub async fn settled(&self) {
        loop {
            // registered before the check, so a drop to zero between the two is not missed
            let mut wake = std::pin::pin!(self.counters.settle.notified());
            wake.as_mut().enable();
            if self.outstanding() == 0 {
                return;
            }
            wake.await;
        }
    }

    /// [`WireCounter::settled`] for [`assert_round_trips!`], bounded.
    ///
    /// # Panics
    /// When requests are still unanswered after a minute, or when the proxy lost track of the
    /// requests of a connection (an answer it cannot match).
    #[doc(hidden)]
    pub async fn settle_before_block(&self) {
        let settled = tokio::time::timeout(SETTLE_LIMIT, self.settled()).await;
        assert!(
            settled.is_ok(),
            "{} request(s) through the wire counter are still unanswered after {:?}: the \
             round-trip block cannot start on a settled wire",
            self.outstanding(),
            SETTLE_LIMIT
        );
        self.assert_in_step();
    }

    /// # Panics
    /// When a `ReadyForQuery` answered no request the proxy saw.
    #[track_caller]
    fn assert_in_step(&self) {
        let unmatched = self.counters.unmatched.load(Ordering::Acquire);
        assert!(
            unmatched == 0,
            "the wire counter saw {unmatched} ReadyForQuery message(s) that answered no request it \
             saw: it lost track of the client's messages"
        );
    }

    /// The round trips since `before` (a [`WireCounter::count`] taken earlier), for
    /// [`assert_round_trips!`].
    ///
    /// # Panics
    /// When the counter was reset since `before` (`generation` is [`WireCounter::reset_generation`]
    /// taken with it), when a connection through the proxy cannot be
    /// counted (TLS), or when no connection finished start-up so nothing was counted: a budget
    /// must not pass because the counter saw nothing.
    #[doc(hidden)]
    #[track_caller]
    pub fn used_since(&self, before: u64, generation: u64) -> u64 {
        assert!(
            self.uncounted() == 0,
            "{} connection(s) through the wire counter use TLS, which it cannot read: round trips \
             cannot be counted",
            self.uncounted()
        );
        assert!(
            self.armed() > 0,
            "no connection through the wire counter finished start-up: the budget would pass \
             without counting anything"
        );
        self.assert_in_step();
        let now = self.count();
        assert!(
            self.reset_generation() == generation && now >= before,
            "the WireCounter was reset inside an assert_round_trips! block"
        );
        now - before
    }
}

async fn forward(
    client: TcpStream,
    target: (String, u16),
    counters: Arc<Counters>,
) -> std::io::Result<()> {
    let server = TcpStream::connect((target.0.as_str(), target.1)).await?;
    client.set_nodelay(true)?;
    server.set_nodelay(true)?;
    let link = Arc::new(Link::default());
    let _hangup = Hangup {
        link: Arc::clone(&link),
        counters: Arc::clone(&counters),
    };
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();
    let upstream = {
        let link = Arc::clone(&link);
        let counters = Arc::clone(&counters);
        async move {
            let mut buf = vec![0u8; 64 * 1024];
            let mut first = true;
            // the client's messages after start-up: a chunk read once start-up is over holds no
            // start-up bytes (the server answered them all before its first ReadyForQuery), so
            // the framing starts on a message boundary
            let mut messages = Frames::default();
            loop {
                let n = client_read.read(&mut buf).await?;
                if n == 0 {
                    return server_write.shutdown().await;
                }
                if std::mem::take(&mut first) && is_encryption_request(&buf[..n]) {
                    link.awaiting_ssl_reply.store(true, Ordering::Release);
                }
                // counted before the bytes go on: the server cannot answer a turn, or a request,
                // not yet counted
                let started = link.started.load(Ordering::Acquire);
                if started
                    && !is_terminate(&buf[..n])
                    && link.server_spoke.swap(false, Ordering::AcqRel)
                {
                    counters.turns.fetch_add(1, Ordering::AcqRel);
                }
                if started {
                    let requests = tally_client(&mut messages, &buf[..n], &counters.messages);
                    if requests > 0 {
                        link.outstanding.fetch_add(requests, Ordering::AcqRel);
                        counters.outstanding.fetch_add(requests, Ordering::AcqRel);
                    }
                }
                server_write.write_all(&buf[..n]).await?;
            }
        }
    };
    let downstream = async move {
        let mut buf = vec![0u8; 64 * 1024];
        let mut frames = Frames::default();
        loop {
            let n = server_read.read(&mut buf).await?;
            if n == 0 {
                return client_write.shutdown().await;
            }
            let mut bytes = &buf[..n];
            if link.awaiting_ssl_reply.swap(false, Ordering::AcqRel) {
                // the one-byte answer to an SSLRequest: `N` means plain text goes on, anything
                // else (`S`) means the rest is encrypted and cannot be read
                if bytes[0] == b'N' {
                    bytes = &bytes[1..];
                } else {
                    counters.opaque.fetch_add(1, Ordering::AcqRel);
                    frames.give_up();
                }
            }
            let fed = frames.feed(bytes);
            if fed.ready_for_query_first {
                link.started.store(true, Ordering::Release);
                counters.armed.fetch_add(1, Ordering::AcqRel);
            }
            if fed.solicited {
                // set before the client can see the bytes
                link.server_spoke.store(true, Ordering::Release);
            }
            if fed.answers > 0 {
                // before the client can see them too: a client that has its answer finds the
                // wire settled
                counters.answered(&link, fed.answers);
            }
            client_write.write_all(&buf[..n]).await?;
        }
    };
    let (up, down) = tokio::join!(upstream, downstream);
    up.and(down)
}

/// Counts the client's messages whose header ends in `bytes` into `messages` (one slot per
/// [`MessageKind`]) and returns how many of them are requests ([`is_request`]).
fn tally_client(
    frames: &mut Frames,
    bytes: &[u8],
    messages: &[AtomicU64; MessageKind::COUNT],
) -> u64 {
    let mut requests = 0;
    frames.each_kind(bytes, |kind| {
        if is_request(kind) {
            requests += 1;
        }
        if let Some(counted) = MessageKind::of(kind) {
            messages[counted.slot()].fetch_add(1, Ordering::AcqRel);
        }
    });
    requests
}

/// What a chunk of the server's stream contained.
#[derive(Default)]
struct Fed {
    /// The first `ReadyForQuery` of the connection ended in this chunk.
    ready_for_query_first: bool,
    /// A message the client could be waiting for: anything but notices, notifications and
    /// parameter reports, which the server sends on its own.
    solicited: bool,
    /// `ReadyForQuery` messages after the first: each answers one request.
    answers: u64,
}

/// Frames of a protocol byte stream after its untyped opening (`type: u8, length: i32 incl.
/// itself, body`): the server's from its first byte, the client's once start-up is over.
#[derive(Default)]
struct Frames {
    header: Vec<u8>,
    /// Bytes of the current message body still to skip.
    skip: usize,
    ready_seen: bool,
    /// The stream is encrypted: it is not framed any more.
    given_up: bool,
}

impl Frames {
    fn give_up(&mut self) {
        self.given_up = true;
    }

    fn feed(&mut self, bytes: &[u8]) -> Fed {
        let mut fed = Fed::default();
        let mut ready_seen = self.ready_seen;
        self.each_kind(bytes, |kind| {
            if kind == b'Z' {
                if ready_seen {
                    fed.answers += 1;
                } else {
                    ready_seen = true;
                    fed.ready_for_query_first = true;
                }
            }
            // NoticeResponse, NotificationResponse, ParameterStatus
            if !matches!(kind, b'N' | b'A' | b'S') {
                fed.solicited = true;
            }
        });
        self.ready_seen = ready_seen;
        fed
    }

    /// Call `on` with the type byte of every message whose header ends in `bytes`.
    fn each_kind(&mut self, mut bytes: &[u8], mut on: impl FnMut(u8)) {
        if self.given_up {
            return;
        }
        while !bytes.is_empty() {
            if self.skip > 0 {
                let take = self.skip.min(bytes.len());
                self.skip -= take;
                bytes = &bytes[take..];
                continue;
            }
            let need = 5 - self.header.len();
            let take = need.min(bytes.len());
            self.header.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.header.len() < 5 {
                continue;
            }
            let kind = self.header[0];
            let len = u32::from_be_bytes([
                self.header[1],
                self.header[2],
                self.header[3],
                self.header[4],
            ]) as usize;
            self.header.clear();
            on(kind);
            self.skip = len.saturating_sub(4);
        }
    }
}

/// Run a block and check that it took exactly (`== n`) or at most (`<= n`) that many round trips
/// on the wire (counted by a [`WireCounter`], never by the ORM). `>= m, <= n` gives a range.
/// Evaluates to the block's value. The block's connection must already be checked out of the
/// pool and warmed (a prepared-statement miss is a round trip of its own, on purpose). Must be
/// used in an async context: before the block runs it waits until every request through the
/// proxy has been answered ([`WireCounter::settled`]), so a request left in flight before the
/// block cannot absorb the block's first flight into a turn counted earlier.
#[macro_export]
macro_rules! assert_round_trips {
    ($wire:expr, == $n:expr, $body:block) => {
        $crate::assert_round_trips!($wire, >= $n, <= $n, $body)
    };
    ($wire:expr, <= $n:expr, $body:block) => {
        $crate::assert_round_trips!($wire, >= 0, <= $n, $body)
    };
    ($wire:expr, >= $m:expr, <= $n:expr, $body:block) => {{
        $wire.settle_before_block().await;
        let __generation = $wire.reset_generation();
        let __before = $wire.count();
        let __value = $body;
        let __used = $wire.used_since(__before, __generation);
        let (__min, __max): (u64, u64) = ($m, $n);
        assert!(
            __used <= __max,
            "round-trip budget exceeded: {} round trips, budget {}",
            __used,
            __max
        );
        assert!(
            __used >= __min,
            "expected at least {} round trips, saw {}",
            __min,
            __used
        );
        __value
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn ready_for_query_is_found_across_split_chunks() {
        let mut stream = message(b'R', &[0, 0, 0, 0]);
        stream.extend(message(b'S', b"server_version\x0018.1\x00"));
        stream.extend(message(b'Z', b"I"));
        for split in 0..stream.len() {
            let mut frames = Frames::default();
            let first = frames.feed(&stream[..split]).ready_for_query_first;
            let second = frames.feed(&stream[split..]).ready_for_query_first;
            assert!(
                first ^ second,
                "ReadyForQuery seen exactly once when split at {split}"
            );
        }
    }

    #[test]
    fn nothing_found_without_ready_for_query() {
        let mut frames = Frames::default();
        assert!(
            !frames
                .feed(&message(b'R', &[0, 0, 0, 3]))
                .ready_for_query_first
        );
    }

    #[test]
    fn only_the_first_ready_for_query_arms() {
        let mut frames = Frames::default();
        assert!(frames.feed(&message(b'Z', b"I")).ready_for_query_first);
        assert!(!frames.feed(&message(b'Z', b"I")).ready_for_query_first);
    }

    #[test]
    fn unsolicited_messages_do_not_start_a_turn() {
        for kind in b"NAS".iter().copied() {
            let mut frames = Frames::default();
            assert!(
                !frames.feed(&message(kind, b"x\x00")).solicited,
                "{} is sent on the server's own",
                kind as char
            );
        }
        for kind in b"CTDEZ12t".iter().copied() {
            let mut frames = Frames::default();
            assert!(
                frames.feed(&message(kind, b"")).solicited,
                "{} answers the client",
                kind as char
            );
        }
        // an answer that comes with a notice is an answer
        let mut mixed = message(b'N', b"x\x00");
        mixed.extend(message(b'C', b"SELECT 1\x00"));
        assert!(Frames::default().feed(&mixed).solicited);
    }

    fn counts_of(messages: &[AtomicU64; MessageKind::COUNT]) -> Vec<(MessageKind, u64)> {
        ALL_KINDS
            .iter()
            .map(|&kind| (kind, messages[kind.slot()].load(Ordering::Acquire)))
            .filter(|&(_, n)| n > 0)
            .collect()
    }

    const ALL_KINDS: [MessageKind; MessageKind::COUNT] = [
        MessageKind::Parse,
        MessageKind::Bind,
        MessageKind::Describe,
        MessageKind::Execute,
        MessageKind::Sync,
        MessageKind::Query,
        MessageKind::Close,
        MessageKind::Flush,
    ];

    #[test]
    fn every_counted_kind_has_its_own_slot() {
        let mut slots: Vec<usize> = ALL_KINDS.iter().map(|k| k.slot()).collect();
        slots.sort_unstable();
        slots.dedup();
        assert_eq!(slots, (0..MessageKind::COUNT).collect::<Vec<_>>());
        for (byte, kind) in [
            (b'P', MessageKind::Parse),
            (b'B', MessageKind::Bind),
            (b'D', MessageKind::Describe),
            (b'E', MessageKind::Execute),
            (b'S', MessageKind::Sync),
            (b'Q', MessageKind::Query),
            (b'C', MessageKind::Close),
            (b'H', MessageKind::Flush),
        ] {
            assert_eq!(MessageKind::of(byte), Some(kind), "{}", byte as char);
        }
        // function call, terminate, copy data/done/fail, a server-only type: not counted
        for byte in *b"FXdcfZT" {
            assert_eq!(MessageKind::of(byte), None, "{}", byte as char);
        }
    }

    /// A prepare (`Parse`, `Describe`, `Sync`) then an execute (`Bind`, `Execute`, `Sync`) then
    /// a simple query, as tokio-postgres writes them, split at every byte: every message is
    /// counted once by type, and the requests are the two `Sync` and the `Query`.
    #[test]
    fn client_messages_are_counted_by_type_across_split_chunks() {
        let mut stream = message(b'P', b"s1\x00SELECT $1::int8\x00\x00\x00");
        stream.extend(message(b'D', b"Ss1\x00"));
        stream.extend(message(b'S', b""));
        stream.extend(message(b'B', b"\x00s1\x00\x00\x00\x00\x01\x00\x00\x00\x01"));
        stream.extend(message(b'E', b"\x00\x00\x00\x00\x00"));
        stream.extend(message(b'S', b""));
        stream.extend(message(b'Q', b"COMMIT\x00"));
        for split in 0..=stream.len() {
            let messages: [AtomicU64; MessageKind::COUNT] =
                std::array::from_fn(|_| AtomicU64::new(0));
            let mut frames = Frames::default();
            let requests = tally_client(&mut frames, &stream[..split], &messages)
                + tally_client(&mut frames, &stream[split..], &messages);
            assert_eq!(requests, 3, "split at {split}");
            assert_eq!(
                counts_of(&messages),
                vec![
                    (MessageKind::Parse, 1),
                    (MessageKind::Bind, 1),
                    (MessageKind::Describe, 1),
                    (MessageKind::Execute, 1),
                    (MessageKind::Sync, 2),
                    (MessageKind::Query, 1),
                ],
                "split at {split}"
            );
        }
    }

    /// A message body that contains type bytes (`P`, `S` inside the SQL text) is skipped, not
    /// counted: only headers are read.
    #[test]
    fn type_bytes_inside_a_body_are_not_counted() {
        let stream = message(b'Q', b"SELECT 'PBSEQ'\x00");
        let messages: [AtomicU64; MessageKind::COUNT] = std::array::from_fn(|_| AtomicU64::new(0));
        let requests = tally_client(&mut Frames::default(), &stream, &messages);
        assert_eq!(requests, 1);
        assert_eq!(counts_of(&messages), vec![(MessageKind::Query, 1)]);
    }

    #[test]
    fn a_fresh_counter_has_no_response_snapshot() {
        assert_eq!(
            Counters::default().at_response.load(Ordering::Acquire),
            NO_SNAPSHOT
        );
    }

    #[test]
    fn terminate_and_encryption_requests_are_recognised() {
        assert!(is_terminate(&[b'X', 0, 0, 0, 4]));
        assert!(!is_terminate(&[b'Q', 0, 0, 0, 4]));
        assert!(!is_terminate(&[b'X', 0, 0, 0, 4, b'Q']));
        assert!(is_encryption_request(&[0, 0, 0, 8, 4, 0xd2, 0x16, 0x2f]));
        assert!(!is_encryption_request(&[0, 0, 0, 8, 0, 3, 0, 0]));
    }
}
