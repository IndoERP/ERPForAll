//! The driver's counts of response batches, delivered and parked (amendment A3, lesson
//! ): what the allocation tests measure the driver's per-batch blocks against. A batch is
//! cut from the whole messages read so far (up to the end of the read, or a `ReadyForQuery`), so
//! the delivered count follows how the server's bytes arrived, and the parked count how far the
//! reader lags, not the result.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use erp_orm::ResponseBatches;
use erp_test_models::base_users;

use erp_testkit::Harness;
use futures_core::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::oneshot;

// Process snapshots require every other connection in this binary to be quiescent.
static PROCESS_WINDOW: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One backend message: tag, length (itself included), body.
fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let len = i32::try_from(body.len() + 4).unwrap();
    let mut out = vec![tag];
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// One frontend message after the startup: its tag.
async fn read_tag(server: &mut DuplexStream) -> u8 {
    let tag = server.read_u8().await.unwrap();
    let len = usize::try_from(server.read_i32().await.unwrap()).unwrap();
    let mut body = vec![0; len - 4];
    server.read_exact(&mut body).await.unwrap();
    tag
}

/// Wait until the client has counted `batches` batches: the server writes its next part only
/// once the client has read the previous one, so two parts never arrive in one read whatever the
/// host's load.
async fn until_counted(client: &tokio_postgres::Client, batches: u64) {
    while client.response_batches() < batches {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(client.response_batches(), batches);
}

/// A server that answers three simple queries: the first in one write; the second in three
/// writes of whole messages, each read before the next is written; the third in two writes that
/// split the command's completion in the middle (one batch whether or not the client reads the
/// first part on its own, since it completes no message).
async fn serve_in_parts(
    mut server: DuplexStream,
    client: oneshot::Receiver<Arc<tokio_postgres::Client>>,
) {
    let len = usize::try_from(server.read_i32().await.unwrap()).unwrap();
    let mut startup = vec![0; len - 4];
    server.read_exact(&mut startup).await.unwrap();
    let mut out = message(b'R', &0i32.to_be_bytes());
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    let client = client.await.unwrap();
    let complete = message(b'C', b"SELECT 0\0");
    let ready = message(b'Z', b"I");
    // 1: one write
    assert_eq!(read_tag(&mut server).await, b'Q');
    server
        .write_all(&[complete.clone(), ready.clone()].concat())
        .await
        .unwrap();
    // 2: three writes of whole messages
    assert_eq!(read_tag(&mut server).await, b'Q');
    server.write_all(&message(b'I', b"")).await.unwrap();
    until_counted(&client, 2).await;
    server.write_all(&complete).await.unwrap();
    until_counted(&client, 3).await;
    // let go before the third answer, so the test holds the last handle once it has it
    drop(client);
    server.write_all(&ready).await.unwrap();
    // 3: the completion split across two writes
    assert_eq!(read_tag(&mut server).await, b'Q');
    let (head, tail) = complete.split_at(3);
    server.write_all(head).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    server.write_all(&[tail, &ready].concat()).await.unwrap();
    assert_eq!(read_tag(&mut server).await, b'X', "the client terminates");
}

/// The count grows by one per read that completed messages for a request: one for an answer in
/// one write, three for an answer in three writes read one by one, and one for an answer whose
/// first write completed no message. The startup is not counted.
#[tokio::test]
async fn the_driver_counts_one_batch_per_read_that_completed_messages() {
    let _exclusive = PROCESS_WINDOW.lock().await;
    // Two distinct connection lifetimes, including their destruction, share the same history.
    let start = ResponseBatches::process_wide();
    for connection in 1..=2 {
        split_exchange().await;
        let window = ResponseBatches::process_wide().since(start).unwrap();
        assert_eq!(window.delivered(), connection * 5);
    }
}

async fn split_exchange() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let (give_client, take_client) = oneshot::channel();
        let server = tokio::spawn(serve_in_parts(server_side, take_client));
        let mut config = tokio_postgres::Config::new();
        config
            .user("erp")
            .dbname("erp")
            .ssl_mode(tokio_postgres::config::SslMode::Disable);
        let (client, connection) = config
            .connect_raw(client_side, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        let client = Arc::new(client);
        assert_eq!(client.response_batches(), 0);
        give_client.send(Arc::clone(&client)).unwrap();
        let process_start = ResponseBatches::process_wide();
        let mut seen = Vec::new();
        for _ in 0..3 {
            client.batch_execute("SELECT 0").await.unwrap();
            seen.push(client.response_batches());
        }
        assert_eq!(seen, [1, 4, 5]);
        let process = ResponseBatches::process_wide()
            .since(process_start)
            .unwrap();
        assert_eq!(process.delivered(), client.response_batches());
        assert_eq!(process.parked(), client.parked_batches());
        // the server let go of its handle before it wrote the third answer
        let client =
            Arc::try_unwrap(client).unwrap_or_else(|_| panic!("the server still holds the client"));
        drop(client);
        server.await.unwrap();
        driver.await.unwrap().unwrap();
    })
    .await
    .expect("the scripted exchange finishes");
}

/// A server that answers one simple query whose reader is slow: a row description and two rows,
/// each in a write of its own, written only once the previous one was delivered, except the last
/// row, which arrives while the response channel is full; then the completion.
async fn serve_to_a_slow_reader(
    mut server: DuplexStream,
    client: oneshot::Receiver<Arc<tokio_postgres::Client>>,
    third_written: oneshot::Sender<()>,
) {
    let len = usize::try_from(server.read_i32().await.unwrap()).unwrap();
    let mut startup = vec![0; len - 4];
    server.read_exact(&mut startup).await.unwrap();
    let mut out = message(b'R', &0i32.to_be_bytes());
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    let client = client.await.unwrap();
    assert_eq!(read_tag(&mut server).await, b'Q');
    // one text column `a`: name, table oid, column number, type oid 25 (text), size -1,
    // modifier -1, text format
    let mut description = 1i16.to_be_bytes().to_vec();
    description.extend_from_slice(b"a\0");
    description.extend_from_slice(&0i32.to_be_bytes());
    description.extend_from_slice(&0i16.to_be_bytes());
    description.extend_from_slice(&25i32.to_be_bytes());
    description.extend_from_slice(&(-1i16).to_be_bytes());
    description.extend_from_slice(&(-1i32).to_be_bytes());
    description.extend_from_slice(&0i16.to_be_bytes());
    let mut row = 1i16.to_be_bytes().to_vec();
    row.extend_from_slice(&1i32.to_be_bytes());
    row.push(b'x');
    server
        .write_all(&message(b'T', &description))
        .await
        .unwrap();
    until_counted(&client, 1).await;
    server.write_all(&message(b'D', &row)).await.unwrap();
    // the channel (`channel(1)`, one sender) now holds two batches: the connection is parked
    until_counted(&client, 2).await;
    assert_eq!(client.parked_batches(), 1, "the second delivery parked");
    server.write_all(&message(b'D', &row)).await.unwrap();
    third_written.send(()).unwrap();
    // delivered once the reader takes the first batch
    until_counted(&client, 3).await;
    drop(client);
    let mut end = message(b'C', b"SELECT 2\0");
    end.extend(message(b'Z', b"I"));
    server.write_all(&end).await.unwrap();
    assert_eq!(read_tag(&mut server).await, b'X', "the client terminates");
}

/// One step of a result stream.
async fn next_message(
    stream: &mut Pin<Box<tokio_postgres::SimpleQueryStream>>,
) -> Option<tokio_postgres::SimpleQueryMessage> {
    std::future::poll_fn(|cx| stream.as_mut().poll_next(cx))
        .await
        .map(|message| message.unwrap())
}

/// A batch that finds its request's channel full waits in the connection and counts once, when it
/// is delivered, not when it is read (review rust-9); every delivery into a channel that holds a
/// batch already counts as parked, and the parked connection resumes once the reader takes a batch
/// (the parked count's check polls with the connection's own context, so it cannot leave a waker
/// that is not the connection's: a lost wake-up would hang here, and the timeout fails it).
#[tokio::test]
async fn a_batch_that_waits_for_room_counts_once_when_delivered() {
    let _exclusive = PROCESS_WINDOW.lock().await;
    let process_start = ResponseBatches::process_wide();
    tokio::time::timeout(Duration::from_secs(30), async {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let (give_client, take_client) = oneshot::channel();
        let (third_written, third_was_written) = oneshot::channel();
        let server = tokio::spawn(serve_to_a_slow_reader(
            server_side,
            take_client,
            third_written,
        ));
        let mut config = tokio_postgres::Config::new();
        config
            .user("erp")
            .dbname("erp")
            .ssl_mode(tokio_postgres::config::SslMode::Disable);
        let (client, connection) = config
            .connect_raw(client_side, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        let client = Arc::new(client);
        give_client.send(Arc::clone(&client)).unwrap();
        let mut stream = Box::pin(client.simple_query_raw("SELECT 'x'").await.unwrap());
        third_was_written.await.unwrap();
        // time for the connection to read the third batch and find the channel full
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            client.response_batches(),
            2,
            "a waiting batch is not counted"
        );
        assert_eq!(client.parked_batches(), 1, "nor counted as parked yet");
        let process = ResponseBatches::process_wide()
            .since(process_start)
            .unwrap();
        assert_eq!((process.delivered(), process.parked()), (2, 1));
        assert!(matches!(
            next_message(&mut stream).await,
            Some(tokio_postgres::SimpleQueryMessage::RowDescription(_))
        ));
        until_counted(&client, 3).await;
        // the waiting batch went into a channel that still held the first row: it parked again
        assert_eq!(client.parked_batches(), 2, "the resumed delivery parked");
        // the completion arrives while the channel is full again: not counted until delivered
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(client.response_batches(), 3);
        assert_eq!(client.parked_batches(), 2);
        let mut rows = 0;
        while let Some(message) = next_message(&mut stream).await {
            match message {
                tokio_postgres::SimpleQueryMessage::Row(row) => {
                    assert_eq!(row.get(0), Some("x"));
                    rows += 1;
                }
                tokio_postgres::SimpleQueryMessage::CommandComplete(n) => assert_eq!(n, 2),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(rows, 2);
        assert_eq!(client.response_batches(), 4, "each batch counted once");
        // the reader took both rows before it yielded, so the completion went into an empty
        // channel: no park
        assert_eq!(
            client.parked_batches(),
            2,
            "a delivery into an empty channel does not park"
        );
        let process = ResponseBatches::process_wide()
            .since(process_start)
            .unwrap();
        assert_eq!((process.delivered(), process.parked()), (4, 2));
        drop(stream);
        let client =
            Arc::try_unwrap(client).unwrap_or_else(|_| panic!("the server still holds the client"));
        drop(client);
        server.await.unwrap();
        driver.await.unwrap().unwrap();
    })
    .await
    .expect("the scripted exchange finishes");
}

/// Against PostgreSQL through the ORM: at least one batch per statement, and a result of about
/// 1 MB in batches no larger than a read buffer and one row.
#[tokio::test]
async fn a_transaction_reports_its_connection_s_batches() {
    let _exclusive = PROCESS_WINDOW.lock().await;
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness");
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    let start = tx
        .driver_response_batches()
        .expect("an open transaction has a connection");
    tx.query("SELECT 1", &[]).await.unwrap();
    let after_one = tx.driver_response_batches().unwrap();
    assert_eq!(ResponseBatches::process_wide().since(after_one), None);
    assert_eq!(after_one.since(ResponseBatches::process_wide()), None);
    let first = after_one
        .since(start)
        .expect("one connection: the counts only grow");
    assert!(first.delivered() >= 1, "{start:?} -> {after_one:?}");
    let rows = tx
        .query(
            "SELECT repeat('x', 1000) FROM generate_series(1, 1000)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1_000);
    let batches = tx
        .driver_response_batches()
        .unwrap()
        .since(after_one)
        .expect("one connection: the counts only grow");
    // 1,000 rows of 1,011 wire bytes: a batch holds at most an 8 KiB read buffer and the row that
    // overflowed it
    assert!(
        batches.delivered() >= 1_011_000 / (8_192 + 1_011),
        "{batches:?}"
    );
    assert!(batches.parked() <= batches.delivered(), "{batches:?}");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
