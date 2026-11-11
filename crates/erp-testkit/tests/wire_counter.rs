//! The wire counter counts what crosses the socket (design note §1). Single-threaded
//! runtime on purpose: two futures polled together put their requests on the wire in one flight.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_testkit::{TestDb, WireCounter, assert_round_trips};

async fn setup() -> (TestDb, WireCounter, deadpool_postgres::Pool) {
    let db = TestDb::create().await.expect("test database");
    let wire = WireCounter::start(&db.config())
        .await
        .expect("wire counter");
    let pool = wire.pool(2).expect("pool through the proxy");
    (db, wire, pool)
}

#[tokio::test]
async fn one_simple_statement_is_one_round_trip() {
    let (db, wire, pool) = setup().await;
    let conn = pool.get().await.expect("connection through the proxy");
    wire.reset();
    assert_round_trips!(wire, == 1, {
        conn.batch_execute("SELECT 1").await.expect("select");
    });
    drop(conn);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn two_sequential_statements_are_two_round_trips() {
    let (db, wire, pool) = setup().await;
    let conn = pool.get().await.expect("connection through the proxy");
    wire.reset();
    assert_round_trips!(wire, == 2, {
        conn.batch_execute("SELECT 1").await.expect("first");
        conn.batch_execute("SELECT 2").await.expect("second");
    });
    drop(conn);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn two_futures_on_one_client_are_one_round_trip() {
    let (db, wire, pool) = setup().await;
    let conn = pool.get().await.expect("connection through the proxy");
    wire.reset();
    assert_round_trips!(wire, == 1, {
        let (a, b) = tokio::join!(conn.batch_execute("SELECT 1"), conn.batch_execute("SELECT 2"));
        a.expect("first");
        b.expect("second");
    });
    drop(conn);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn a_prepare_miss_adds_one_round_trip_and_a_hit_adds_none() {
    let (db, wire, pool) = setup().await;
    let conn = pool.get().await.expect("connection through the proxy");
    wire.reset();
    let miss = assert_round_trips!(wire, == 2, {
        let statement = conn.prepare_cached("SELECT $1::int8").await.expect("prepare");
        conn.query(&statement, &[&7_i64]).await.expect("execute")
    });
    let hit = assert_round_trips!(wire, == 1, {
        let statement = conn.prepare_cached("SELECT $1::int8").await.expect("cached");
        conn.query(&statement, &[&9_i64]).await.expect("execute")
    });
    // the answers are the server's, not echoed by the counter
    assert_eq!(miss[0].get::<_, i64>(0), 7);
    assert_eq!(hit[0].get::<_, i64>(0), 9);
    drop(conn);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
async fn reset_starts_from_zero_and_clones_share_the_count() {
    let (db, wire, pool) = setup().await;
    let conn = pool.get().await.expect("connection through the proxy");
    let twin = wire.clone();
    wire.reset();
    conn.batch_execute("SELECT 1").await.expect("select");
    assert_eq!(twin.count(), wire.count());
    assert!(wire.count() >= 1);
    twin.reset();
    assert_eq!(wire.count(), 0);
    drop(conn);
    db.cleanup().await.expect("cleanup");
}

#[tokio::test]
#[should_panic(expected = "round-trip budget exceeded: 2 round trips, budget 1")]
async fn the_macro_fails_when_over_budget() {
    let (_db, wire, pool) = setup().await;
    let conn = pool.get().await.expect("connection through the proxy");
    wire.reset();
    assert_round_trips!(wire, <= 1, {
        conn.batch_execute("SELECT 1").await.expect("first");
        conn.batch_execute("SELECT 2").await.expect("second");
    });
}

// ----- the rules of what counts, against a fake backend ------------------------------------------
//
// A scripted server on a local port stands in for PostgreSQL so the test decides exactly what the
// client sees and when.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn message(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// A counter in front of a fake backend; returns the counter, a raw client connected through the
/// proxy and the backend's side of that connection.
async fn fake() -> (WireCounter, TcpStream, TcpStream) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    let mut upstream = tokio_postgres::Config::new();
    upstream.host("127.0.0.1").port(port);
    let wire = WireCounter::start(&upstream).await.expect("wire counter");
    let local = wire.config().get_ports()[0];
    let client = TcpStream::connect(("127.0.0.1", local))
        .await
        .expect("client");
    let (backend, _) = listener.accept().await.expect("backend side");
    (wire, client, backend)
}

async fn read_some(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = vec![0u8; 1024];
    let n = stream.read(&mut buf).await.expect("read");
    buf.truncate(n);
    buf
}

/// Start-up over: the backend sent `ReadyForQuery` and the client has it.
async fn start_up(client: &mut TcpStream, backend: &mut TcpStream) {
    client
        .write_all(&[0, 0, 0, 8, 0, 3, 0, 0])
        .await
        .expect("startup");
    read_some(backend).await;
    backend
        .write_all(&message(b'Z', b"I"))
        .await
        .expect("ready");
    assert_eq!(read_some(client).await, message(b'Z', b"I"));
}

#[tokio::test]
async fn a_notice_or_a_notification_is_not_an_answer() {
    let (wire, mut client, mut backend) = fake().await;
    start_up(&mut client, &mut backend).await;
    wire.reset();
    // the query is a turn
    client
        .write_all(&message(b'Q', b"SELECT 1\0"))
        .await
        .unwrap();
    read_some(&mut backend).await;
    assert_eq!(wire.count(), 1);
    // the server speaks on its own; the client writes again without waiting for any answer
    let own = [
        message(b'A', b"\0\0\0\x01c\0p\0"),
        message(b'N', b"SNOTICE\0\0"),
        message(b'S', b"TimeZone\0UTC\0"),
    ]
    .concat();
    for part in [&own[..7], &own[7..]] {
        backend.write_all(part).await.unwrap();
    }
    let mut seen = vec![0u8; own.len()];
    client.read_exact(&mut seen).await.unwrap();
    assert_eq!(seen, own);
    client
        .write_all(&message(b'Q', b"SELECT 2\0"))
        .await
        .unwrap();
    read_some(&mut backend).await;
    assert_eq!(
        wire.count(),
        1,
        "nothing was answered, so the second write is the same turn"
    );
    // an answer makes the next write a turn
    backend.write_all(&message(b'Z', b"I")).await.unwrap();
    let mut ready = [0u8; 6];
    client.read_exact(&mut ready).await.unwrap();
    client
        .write_all(&message(b'Q', b"SELECT 3\0"))
        .await
        .unwrap();
    read_some(&mut backend).await;
    assert_eq!(wire.count(), 2);
}

#[tokio::test]
async fn the_terminate_of_a_closing_connection_is_not_a_round_trip() {
    let (wire, mut client, mut backend) = fake().await;
    start_up(&mut client, &mut backend).await;
    wire.reset();
    client.write_all(&[b'X', 0, 0, 0, 4]).await.unwrap();
    assert_eq!(read_some(&mut backend).await, [b'X', 0, 0, 0, 4]);
    assert_eq!(wire.count(), 0);
}

#[tokio::test]
async fn a_refused_ssl_request_leaves_the_connection_countable() {
    let (wire, mut client, mut backend) = fake().await;
    client
        .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .await
        .unwrap();
    read_some(&mut backend).await;
    backend.write_all(b"N").await.unwrap();
    assert_eq!(read_some(&mut client).await, b"N");
    client.write_all(&[0, 0, 0, 8, 0, 3, 0, 0]).await.unwrap();
    read_some(&mut backend).await;
    backend.write_all(&message(b'Z', b"I")).await.unwrap();
    read_some(&mut client).await;
    assert_eq!((wire.armed(), wire.uncounted()), (1, 0));
    client
        .write_all(&message(b'Q', b"SELECT 1\0"))
        .await
        .unwrap();
    read_some(&mut backend).await;
    assert_eq!(wire.count(), 1);
}

#[tokio::test]
async fn an_accepted_ssl_request_makes_the_connection_uncountable() {
    let (wire, mut client, mut backend) = fake().await;
    client
        .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .await
        .unwrap();
    read_some(&mut backend).await;
    backend.write_all(b"S").await.unwrap();
    assert_eq!(read_some(&mut client).await, b"S");
    assert_eq!((wire.armed(), wire.uncounted()), (0, 1));
}

#[tokio::test]
#[should_panic(expected = "1 connection(s) through the wire counter use TLS, which it cannot read")]
async fn the_macro_fails_when_a_connection_cannot_be_counted() {
    let (wire, mut client, mut backend) = fake().await;
    client
        .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
        .await
        .unwrap();
    read_some(&mut backend).await;
    backend.write_all(b"S").await.unwrap();
    read_some(&mut client).await;
    assert_round_trips!(wire, <= 5, {});
}

#[tokio::test]
#[should_panic(expected = "no connection through the wire counter finished start-up")]
async fn the_macro_fails_when_nothing_was_ever_counted() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let mut upstream = tokio_postgres::Config::new();
    upstream
        .host("127.0.0.1")
        .port(listener.local_addr().unwrap().port());
    let wire = WireCounter::start(&upstream).await.expect("wire counter");
    assert_round_trips!(wire, <= 5, {});
}

#[tokio::test]
#[should_panic(expected = "the WireCounter was reset inside an assert_round_trips! block")]
async fn the_macro_fails_clearly_when_the_block_resets_the_counter() {
    let (wire, mut client, mut backend) = fake().await;
    start_up(&mut client, &mut backend).await;
    query_answered(&mut client, &mut backend).await;
    assert_round_trips!(wire, <= 5, {
        wire.reset();
    });
}

#[tokio::test]
#[should_panic(expected = "the WireCounter was reset inside an assert_round_trips! block")]
async fn the_macro_notices_a_reset_even_when_the_count_climbs_back() {
    let (wire, mut client, mut backend) = fake().await;
    start_up(&mut client, &mut backend).await;
    for _ in 0..2 {
        query_answered(&mut client, &mut backend).await;
    }
    assert_round_trips!(wire, <= 5, {
        wire.reset();
        for _ in 0..2 {
            query_answered(&mut client, &mut backend).await;
        }
    });
}

/// One `Query` the backend answers with `ReadyForQuery`, which the client reads.
async fn query_answered(client: &mut TcpStream, backend: &mut TcpStream) {
    client
        .write_all(&message(b'Q', b"SELECT 1\0"))
        .await
        .unwrap();
    read_some(backend).await;
    backend.write_all(&message(b'Z', b"I")).await.unwrap();
    let mut ready = [0u8; 6];
    client.read_exact(&mut ready).await.unwrap();
}

/// Poll `future` once, without waiting.
fn poll_once<F: std::future::Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    std::future::Future::poll(
        future,
        &mut std::task::Context::from_waker(std::task::Waker::noop()),
    )
}

/// Read exactly `n` bytes.
async fn read_n(stream: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).await.unwrap();
    buf
}

#[tokio::test]
async fn a_request_is_outstanding_until_its_ready_for_query() {
    let (wire, mut client, mut backend) = fake().await;
    start_up(&mut client, &mut backend).await;
    assert_eq!(wire.outstanding(), 0, "start-up is not a request");
    // Parse + Sync and a Query, split inside a header: two requests (Parse is not one)
    let (parse, sync) = (message(b'P', b"\0SELECT 1\0\0\0"), message(b'S', b""));
    let flight = [parse.clone(), sync.clone(), message(b'Q', b"SELECT 2\0")].concat();
    // two bytes into the Query's header
    let split = parse.len() + sync.len() + 2;
    for part in [&flight[..split], &flight[split..]] {
        client.write_all(part).await.unwrap();
        let seen = read_n(&mut backend, part.len()).await;
        assert_eq!(seen, part);
    }
    assert_eq!(wire.outstanding(), 2);
    let settled = wire.settled();
    let mut settled = std::pin::pin!(settled);
    assert!(
        poll_once(settled.as_mut()).is_pending(),
        "two requests are unanswered"
    );
    // the Sync's answer, then the Query's (a notice and a row before it are not answers)
    let first = [message(b'1', b""), message(b'Z', b"I")].concat();
    backend.write_all(&first).await.unwrap();
    assert_eq!(read_n(&mut client, first.len()).await, first);
    assert_eq!(wire.outstanding(), 1);
    assert!(
        poll_once(settled.as_mut()).is_pending(),
        "one is unanswered"
    );
    let second = [
        message(b'N', b"SNOTICE\0\0"),
        message(b'C', b"SELECT 1\0"),
        message(b'Z', b"I"),
    ]
    .concat();
    backend.write_all(&second).await.unwrap();
    assert_eq!(read_n(&mut client, second.len()).await, second);
    assert_eq!(wire.outstanding(), 0);
    assert!(
        poll_once(settled.as_mut()).is_ready(),
        "every request answered"
    );
}

/// The flaky case of `rtt_route::a_routed_read_only_start_is_one_round_trip`, made explicit: a
/// request sent before the block without waiting for its answer (the `Close` of a dropped
/// statement) is answered before the block's flight leaves, so the flight is its own turn. Without
/// the wait the flight would be pipelined behind that request and the block would see 0.
#[tokio::test]
async fn the_block_starts_after_the_answer_to_an_earlier_request() {
    let (wire, mut client, mut backend) = fake().await;
    start_up(&mut client, &mut backend).await;
    let close = [message(b'C', b"Ss1\0"), message(b'S', b"")].concat();
    client.write_all(&close).await.unwrap();
    assert_eq!(read_n(&mut backend, close.len()).await, close);
    assert_eq!(wire.outstanding(), 1);
    wire.reset();
    let query = message(b'Q', b"SELECT 1\0");
    let ready = message(b'Z', b"I");
    let started = std::sync::atomic::AtomicBool::new(false);
    let block = async {
        assert_round_trips!(wire, == 1, {
            started.store(true, std::sync::atomic::Ordering::SeqCst);
            client.write_all(&query).await.unwrap();
            // the close's answer and the query's
            read_n(&mut client, 2 * ready.len() + 5).await
        })
    };
    let mut block = std::pin::pin!(block);
    assert!(poll_once(block.as_mut()).is_pending());
    assert!(
        !started.load(std::sync::atomic::Ordering::SeqCst),
        "the block must not start while a request through the proxy is unanswered"
    );
    let server = async {
        backend
            .write_all(&[message(b'3', b""), ready.clone()].concat())
            .await
            .unwrap();
        assert_eq!(read_n(&mut backend, query.len()).await, query);
        backend.write_all(&ready).await.unwrap();
    };
    let (seen, ()) = tokio::join!(block.as_mut(), server);
    assert_eq!(
        seen,
        [message(b'3', b""), ready.clone(), ready.clone()].concat()
    );
}

#[tokio::test]
async fn a_connection_that_ends_gives_back_its_unanswered_requests() {
    let (wire, mut client, mut backend) = fake().await;
    start_up(&mut client, &mut backend).await;
    let query = message(b'Q', b"SELECT pg_sleep(60)\0");
    client.write_all(&query).await.unwrap();
    assert_eq!(read_n(&mut backend, query.len()).await, query);
    assert_eq!(wire.outstanding(), 1);
    drop(backend);
    drop(client);
    // bounded: a connection that never gives its requests back fails here instead of hanging
    tokio::time::timeout(std::time::Duration::from_secs(10), wire.settled())
        .await
        .expect("the ended connection's request is no longer outstanding");
    assert_eq!(wire.outstanding(), 0);
}

#[tokio::test]
#[should_panic(expected = "ReadyForQuery message(s) that answered no request it saw")]
async fn the_macro_fails_when_an_answer_matches_no_request() {
    let (wire, mut client, mut backend) = fake().await;
    start_up(&mut client, &mut backend).await;
    backend.write_all(&message(b'Z', b"I")).await.unwrap();
    read_n(&mut client, 6).await;
    assert_round_trips!(wire, <= 5, {});
}

#[tokio::test]
async fn the_proxy_keeps_the_upstream_settings() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let mut upstream = tokio_postgres::Config::new();
    upstream
        .host("127.0.0.1")
        .port(listener.local_addr().unwrap().port())
        .user("u")
        .password("p")
        .dbname("d")
        .options("-c lock_timeout=5s")
        .application_name("app")
        .connect_timeout(std::time::Duration::from_secs(3))
        .keepalives(false);
    let wire = WireCounter::start(&upstream).await.expect("wire counter");
    let config = wire.config();
    assert_eq!(config.get_options(), Some("-c lock_timeout=5s"));
    assert_eq!(config.get_application_name(), Some("app"));
    assert_eq!(
        config.get_connect_timeout(),
        Some(&std::time::Duration::from_secs(3))
    );
    assert!(!config.get_keepalives());
    assert_eq!(
        (config.get_user(), config.get_dbname()),
        (Some("u"), Some("d"))
    );
    assert_eq!(config.get_password(), Some(&b"p"[..]));
}

/// PERF.SERVE-HARNESS (perf-serve §3): the message-type counts of a known prepare and execute.
/// tokio-postgres prepares with `Parse`, `Describe`, `Sync` and runs a prepared statement with
/// `Bind`, `Execute`, `Sync`; a cached statement sends no second `Parse`; a simple query is one
/// `Query`. The figures come from the bytes the proxy read.
#[tokio::test]
async fn messages_are_counted_by_type_for_a_known_prepare() {
    let (db, wire, pool) = setup().await;
    let conn = pool.get().await.expect("connection through the proxy");
    // A statement dropped while the connection was set up queues a `Close` + `Sync` that goes
    // out with the next request; one round trip first puts it on the wire before the reset.
    conn.batch_execute("SELECT 0").await.expect("warm");
    wire.settled().await;
    wire.reset();
    let statement = conn
        .prepare_cached("SELECT $1::int8")
        .await
        .expect("prepare");
    let after_prepare = wire.messages();
    assert_eq!(
        (
            after_prepare.parse,
            after_prepare.describe,
            after_prepare.sync,
            after_prepare.bind,
            after_prepare.execute,
            after_prepare.query
        ),
        (1, 1, 1, 0, 0, 0),
        "{after_prepare:?}"
    );
    let rows = conn.query(&statement, &[&5_i64]).await.expect("execute");
    assert_eq!(rows[0].get::<_, i64>(0), 5);
    // the cache hit sends no Parse: the warm-statement assertion the served path uses
    let again = conn
        .prepare_cached("SELECT $1::int8")
        .await
        .expect("cached");
    conn.query(&again, &[&6_i64]).await.expect("execute");
    conn.batch_execute("SELECT 1").await.expect("simple query");
    let total = wire.messages();
    assert_eq!(
        (
            total.parse,
            total.describe,
            total.sync,
            total.bind,
            total.execute,
            total.query
        ),
        (1, 1, 3, 2, 2, 1),
        "{total:?}"
    );
    // reset zeroes every type
    wire.reset();
    assert_eq!(wire.messages(), erp_testkit::MessageCounts::default());
    drop(conn);
    db.cleanup().await.expect("cleanup");
}

/// PERF.SERVE-HARNESS (perf-serve §3): the response-sent snapshot of a known two-flight
/// request. The "server" answers after its first flight and sends the second one (the read's
/// `COMMIT` in the real path) after the answer: the snapshot holds the round trips before the
/// answer, the total holds both.
#[tokio::test]
async fn the_response_sent_snapshot_separates_turns_before_the_answer() {
    let (db, wire, pool) = setup().await;
    let conn = pool.get().await.expect("connection through the proxy");
    conn.batch_execute("SELECT 0").await.expect("warm");
    wire.settled().await;
    wire.reset();
    assert_eq!(wire.turns_at_response(), None);
    assert_eq!(wire.responses_sent(), 0);
    conn.batch_execute("BEGIN; SELECT 1")
        .await
        .expect("first flight");
    wire.response_sent();
    conn.batch_execute("COMMIT").await.expect("second flight");
    wire.settled().await;
    assert_eq!(wire.turns_at_response(), Some(1));
    assert_eq!(wire.count(), 2);
    assert_eq!(wire.responses_sent(), 1);
    // a second response replaces the snapshot and is counted
    wire.response_sent();
    assert_eq!(wire.turns_at_response(), Some(2));
    assert_eq!(wire.responses_sent(), 2);
    // reset clears the snapshot with the count
    wire.reset();
    assert_eq!(wire.turns_at_response(), None);
    assert_eq!(wire.responses_sent(), 0);
    drop(conn);
    db.cleanup().await.expect("cleanup");
}
