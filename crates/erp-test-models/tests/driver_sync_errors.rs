//! TEST.FLAKE-BUILD-TIMEOUT: an error the server raises while it answers the `Sync` of a
//! statement's preparation is the error reported, never a later "current transaction is aborted"
//! (`25P02`) on the next statement.
//!
//! PostgreSQL arms `statement_timeout` at the `Parse` and keeps it armed until the `Sync`. When it
//! fires after the `Describe` has been answered, the error goes out after the row description,
//! in the answer to the `Sync`, and the transaction is aborted. The driver's `prepare` used to
//! stop reading at the row description, so that error was dropped with the rest of the answer:
//! the statement looked prepared, its `Bind` then failed with `25P02`, and the timeout itself was
//! never reported (seen in CI as `Orm::build_object` returning `25P02` instead of `57014`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::ErrorKind;
use erp_orm::Transaction;
use erp_test_models::base_users;
use erp_testkit::Harness;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_postgres::error::SqlState;

/// One backend message: tag, length (itself included), body.
fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let len = i32::try_from(body.len() + 4).unwrap();
    let mut out = vec![tag];
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// One frontend message after the startup: its tag and body.
async fn read_message(server: &mut DuplexStream) -> (u8, Vec<u8>) {
    let tag = server.read_u8().await.unwrap();
    let len = usize::try_from(server.read_i32().await.unwrap()).unwrap();
    let mut body = vec![0; len - 4];
    server.read_exact(&mut body).await.unwrap();
    (tag, body)
}

/// The `ErrorResponse` PostgreSQL sends when `statement_timeout` fires.
fn statement_timeout_error() -> Vec<u8> {
    let mut fields = Vec::new();
    for (code, value) in [
        (b'S', "ERROR"),
        (b'V', "ERROR"),
        (b'C', "57014"),
        (b'M', "canceling statement due to statement timeout"),
        (b'R', "ProcessInterrupts"),
    ] {
        fields.push(code);
        fields.extend_from_slice(value.as_bytes());
        fields.push(0);
    }
    fields.push(0);
    message(b'E', &fields)
}

/// Read the startup message and answer it: authentication done, ready for queries.
async fn startup(server: &mut DuplexStream) {
    // startup message: length, protocol, parameters
    let len = usize::try_from(server.read_i32().await.unwrap()).unwrap();
    let mut startup = vec![0; len - 4];
    server.read_exact(&mut startup).await.unwrap();
    let mut out = message(b'R', &0i32.to_be_bytes());
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
}

/// Read one request up to and including its `Sync`, appending each message's tag to `tags`.
async fn read_request(server: &mut DuplexStream, tags: &mut Vec<u8>) {
    loop {
        let (tag, _) = read_message(server).await;
        tags.push(tag);
        if tag == b'S' {
            return;
        }
    }
}

/// The end of a script: the `Close` and `Sync` the client sends for the statement the server
/// parsed (answered in the failed transaction), its `ROLLBACK` (answered normally) and its
/// `Terminate` when it is dropped.
async fn finish(server: &mut DuplexStream, tags: &mut Vec<u8>) {
    // fail at once, never wait: without the Close the next message is the ROLLBACK, and the
    // script would wait for a Sync that never comes
    let (tag, _) = read_message(server).await;
    tags.push(tag);
    assert_eq!(
        tag, b'C',
        "the client closes the statement the server parsed (tags so far {tags:?})"
    );
    read_request(server, tags).await;
    let mut out = message(b'3', b"");
    out.extend(message(b'Z', b"E"));
    server.write_all(&out).await.unwrap();
    let (tag, body) = read_message(server).await;
    tags.push(tag);
    assert_eq!(
        body, b"ROLLBACK\0",
        "the next request is the client's ROLLBACK"
    );
    let mut out = message(b'C', b"ROLLBACK\0");
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    let (tag, _) = read_message(server).await;
    tags.push(tag);
}

/// A server that answers a preparation's `Parse`, `Describe` and `Sync` with the description
/// and then, for the `Sync`, the statement timeout and `ReadyForQuery` in a failed transaction:
/// the exact answer PostgreSQL gives when the timer fires between the `Describe` and the `Sync`.
async fn serve_timeout_at_sync(mut server: DuplexStream) -> Vec<u8> {
    startup(&mut server).await;
    let mut tags = Vec::new();
    read_request(&mut server, &mut tags).await;
    let mut out = message(b'1', b"");
    out.extend(message(b't', &0i16.to_be_bytes()));
    out.extend(message(b'n', b""));
    out.extend(statement_timeout_error());
    out.extend(message(b'Z', b"E"));
    server.write_all(&out).await.unwrap();
    finish(&mut server, &mut tags).await;
    tags
}

/// A type the driver does not know, so it looks it up with its type query.
const UNKNOWN_TYPE: u32 = 900_001;

/// One field of a `RowDescription`: name, table, column, type, size, modifier, format.
fn field(name: &str, type_oid: u32, size: i16) -> Vec<u8> {
    let mut out = name.as_bytes().to_vec();
    out.push(0);
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&0i16.to_be_bytes());
    out.extend_from_slice(&type_oid.to_be_bytes());
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&(-1i32).to_be_bytes());
    out.extend_from_slice(&0i16.to_be_bytes());
    out
}

/// One value of a binary `DataRow`, or NULL.
fn value(row: &mut Vec<u8>, bytes: Option<&[u8]>) {
    match bytes {
        Some(bytes) => {
            row.extend_from_slice(&i32::try_from(bytes.len()).unwrap().to_be_bytes());
            row.extend_from_slice(bytes);
        }
        None => row.extend_from_slice(&(-1i32).to_be_bytes()),
    }
}

/// A server whose prepared statement has a parameter of an unknown type: the driver prepares
/// its type query, runs it, and the server sends the type's row and then the statement timeout
/// (the executor stopped after the row), before `ReadyForQuery` in a failed transaction.
async fn serve_timeout_after_the_type_row(mut server: DuplexStream) -> Vec<u8> {
    const NAME: u32 = 19;
    const CHAR: u32 = 18;
    const OID: u32 = 26;
    startup(&mut server).await;
    let mut tags = Vec::new();
    // the caller's statement: one parameter of the unknown type, no columns
    read_request(&mut server, &mut tags).await;
    let mut parameters = 1i16.to_be_bytes().to_vec();
    parameters.extend_from_slice(&UNKNOWN_TYPE.to_be_bytes());
    let mut out = message(b'1', b"");
    out.extend(message(b't', &parameters));
    out.extend(message(b'n', b""));
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    // the driver's type query: one oid parameter, seven columns
    read_request(&mut server, &mut tags).await;
    let mut parameters = 1i16.to_be_bytes().to_vec();
    parameters.extend_from_slice(&OID.to_be_bytes());
    let mut columns = 7i16.to_be_bytes().to_vec();
    for (name, oid, size) in [
        ("typname", NAME, 64),
        ("typtype", CHAR, 1),
        ("typelem", OID, 4),
        ("rngsubtype", OID, 4),
        ("typbasetype", OID, 4),
        ("nspname", NAME, 64),
        ("typrelid", OID, 4),
    ] {
        columns.extend(field(name, oid, size));
    }
    let mut out = message(b'1', b"");
    out.extend(message(b't', &parameters));
    out.extend(message(b'T', &columns));
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    // its execution: the row (binary), then the timeout
    read_request(&mut server, &mut tags).await;
    let mut row = 7i16.to_be_bytes().to_vec();
    value(&mut row, Some(b"mystery"));
    value(&mut row, Some(b"b"));
    value(&mut row, Some(&0u32.to_be_bytes()));
    value(&mut row, None);
    value(&mut row, Some(&0u32.to_be_bytes()));
    value(&mut row, Some(b"public"));
    value(&mut row, Some(&0u32.to_be_bytes()));
    let mut out = message(b'2', b"");
    out.extend(message(b'D', &row));
    out.extend(statement_timeout_error());
    out.extend(message(b'Z', b"E"));
    server.write_all(&out).await.unwrap();
    finish(&mut server, &mut tags).await;
    tags
}

type Driver = tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>;

/// A client on a scripted server: the client, its connection task and the server task (which
/// returns the tags of the messages it read).
async fn scripted<F>(
    serve: impl FnOnce(DuplexStream) -> F,
) -> (
    tokio_postgres::Client,
    Driver,
    tokio::task::JoinHandle<Vec<u8>>,
)
where
    F: std::future::Future<Output = Vec<u8>> + Send + 'static,
{
    let (client_side, server_side) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(serve(server_side));
    let mut config = tokio_postgres::Config::new();
    config
        .user("erp")
        .dbname("erp")
        .ssl_mode(tokio_postgres::config::SslMode::Disable);
    let (client, connection) = config
        .connect_raw(client_side, tokio_postgres::NoTls)
        .await
        .unwrap();
    (client, tokio::spawn(connection), server)
}

/// How long a scripted exchange may take: a script that waits for a message the client never
/// sends fails the test instead of hanging it.
const SCRIPT_GUARD: std::time::Duration = std::time::Duration::from_secs(30);

async fn within_guard<T>(exchange: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(SCRIPT_GUARD, exchange)
        .await
        .expect("the scripted exchange finishes")
}

/// Send the `ROLLBACK`, drop the client and return the tags the server read. The server's own
/// result is checked first, so a script assertion (a message the client did not send) is the
/// failure reported, not the closed connection it leaves behind.
async fn end(
    client: tokio_postgres::Client,
    driver: Driver,
    server: tokio::task::JoinHandle<Vec<u8>>,
) -> Vec<u8> {
    let rolled_back = client.batch_execute("ROLLBACK").await;
    drop(client);
    let tags = server.await.unwrap();
    rolled_back.unwrap();
    driver.await.unwrap().unwrap();
    tags
}

/// The driver's error is the server's statement timeout (SQLSTATE, severity and the routine
/// that raised it; never the message text, which follows the server's `lc_messages`).
fn assert_statement_timeout(error: &tokio_postgres::Error) {
    assert_eq!(error.code(), Some(&SqlState::QUERY_CANCELED));
    let db = error.as_db_error().unwrap();
    assert_eq!(db.severity(), "ERROR");
    assert_eq!(db.routine(), Some("ProcessInterrupts"));
}

/// The driver contract the ORM relies on, without timing: a preparation whose `Sync` is answered
/// with an error fails with that error, and the statement the server parsed is closed (the tags
/// show the client's `Close` and `Sync`). Fails on the released `tokio-postgres` 0.7.18, whose
/// `prepare` returns the statement and drops the error. The test also sends a `ROLLBACK` after
/// it and checks the answer; that part cannot tell the released code from the fixed one (the
/// driver hands each request the answers up to its own `ReadyForQuery` either way), it only shows
/// the connection is still usable.
#[tokio::test]
async fn a_preparation_reports_the_error_the_server_raises_at_its_sync() {
    let tags = within_guard(async {
        let (client, driver, server) = scripted(serve_timeout_at_sync).await;
        let error = client.prepare("SELECT 1").await.unwrap_err();
        assert_statement_timeout(&error);
        end(client, driver, server).await
    })
    .await;
    assert_eq!(tags, b"PDSCSQX".to_vec());
}

/// The same for the driver's type lookup during a preparation: an error after the type's row is
/// returned (the lookup reads its answer to `ReadyForQuery`), and the caller's statement, already
/// parsed, is closed. Fails on the released driver, whose lookup takes the row and drops the
/// rest of the answer.
#[tokio::test]
async fn a_type_lookup_reports_the_error_the_server_raises_after_the_row() {
    let tags = within_guard(async {
        let (client, driver, server) = scripted(serve_timeout_after_the_type_row).await;
        let error = client.prepare("SELECT $1").await.unwrap_err();
        assert_statement_timeout(&error);
        end(client, driver, server).await
    })
    .await;
    // the caller's preparation, the type query's preparation and execution, the Close of the
    // caller's statement, the ROLLBACK and the Terminate
    assert_eq!(tags, b"PDSPDSBESCSQX".to_vec());
}

/// Named statements on the server that the ORM's statement cache does not hold, on the
/// transaction's connection.
async fn statements_not_cached(tx: &mut Transaction) -> i64 {
    let rows = tx
        .query("SELECT count(*) FROM pg_prepared_statements", &[])
        .await
        .unwrap();
    let on_server: i64 = rows[0].get(0);
    on_server - i64::try_from(tx.prepared_statements()).unwrap()
}

/// Rounds of the stress test below. Each prepares a new statement (its text is unique), so every
/// round goes through `Parse`, `Describe` and `Sync` under a 1 ms limit.
const ROUNDS: usize = 300;

/// The same through the ORM against the server: with a 1 ms `statement_timeout`, the catalog
/// statement of schema sync (planned in about that time) is stopped in its `Parse`, its
/// `Describe`, at its `Sync` or in its execution, depending on the load. Wherever it is stopped,
/// the error is the timeout (`57014`); `25P02` would mean an earlier error was lost. The test sets
/// the limit with `set_config` behind the ORM's back, so the ORM does not claim it as the
/// transaction's own limit (no condition); `Orm::build_object` tells the transaction, and its test
/// checks the condition. No statement a failed preparation parsed stays on the connection: after
/// the rounds the server holds no more named statements beyond the ORM's cache than before.
/// Timing dependent: on the released driver it fails only when a round's timer fires between the
/// `Describe` and the `Sync` (seen under two parallel CI runs); the scripted tests above are the
/// deterministic proof. The leak check is weak on its own (without the `Close` it caught the leak
/// in 1 of 10 runs). Known window: each round's `ROLLBACK` also runs under the 1 ms limit, and a
/// timer that fires before the server finishes it would fail `rollback()` here; not seen in 160
/// runs of 300 rounds.
#[tokio::test]
async fn a_statement_timeout_during_a_preparation_is_the_error_reported() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness");
    let tables = vec!["res_users".to_owned(), "res_partner".to_owned()];
    let mut tx = h.begin().await.unwrap();
    let not_cached_before = statements_not_cached(&mut tx).await;
    tx.rollback().await.unwrap();
    let mut stopped = 0usize;
    for round in 0..ROUNDS {
        tx.query("SELECT set_config('statement_timeout', '1ms', true)", &[])
            .await
            .unwrap();
        let sql = format!("/* round {round} */ {}", erp_sql::ddl::CATALOG_QUERY);
        match tx.query(&sql, &[&tables]).await {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::Database, "round {round}");
                assert_eq!(
                    error.sqlstate(),
                    Some("57014"),
                    "round {round}: the statement timeout must be the error reported: {error}"
                );
                assert_eq!(error.condition(), None, "round {round}");
                stopped += 1;
            }
        }
        tx.rollback().await.unwrap();
    }
    // the limit was exercised (the catalog statement takes longer than 1 ms to run)
    assert!(stopped > 0, "no round was stopped by the 1 ms limit");
    assert_eq!(
        statements_not_cached(&mut tx).await,
        not_cached_before,
        "a statement parsed by a failed preparation stayed on the connection"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
