//! PERF.STREAM-WINDOW review round 5 (M3, M4, L3): the ORM reads raw SQL as PostgreSQL does
//! only while the connection reads strings as it was opened (`standard_conforming_strings = on`,
//! `client_encoding = UTF8`). The pool refuses a connection that starts otherwise; a change the
//! server reports later (whatever SQL caused it) makes the rest of the transaction unknown and
//! closes the connection instead of returning it to the pool.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;
use std::time::Duration;

use erp_fields::ErrorKind;
use erp_orm::{BeginOptions, Orm, SUPERUSER_ID, StaticPolicy, Transaction};
use erp_test_models::base_users;
use erp_testkit::Harness;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

/// One backend message: tag, length (itself included), body.
fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let len = i32::try_from(body.len() + 4).unwrap();
    let mut out = vec![tag];
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// A `ParameterStatus` report.
fn parameter_status(name: &str, value: &str) -> Vec<u8> {
    let mut body = name.as_bytes().to_vec();
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    message(b'S', &body)
}

/// One frontend message after the startup: its tag.
async fn read_tag(server: &mut DuplexStream) -> u8 {
    let tag = server.read_u8().await.unwrap();
    let len = usize::try_from(server.read_i32().await.unwrap()).unwrap();
    let mut body = vec![0; len - 4];
    server.read_exact(&mut body).await.unwrap();
    tag
}

/// Read one extended-protocol request up to and including its `Sync`.
async fn read_until_sync(server: &mut DuplexStream) {
    while read_tag(server).await != b'S' {}
}

/// Write `first`, wait, then write `then`: the report arrives in a later read than the command's
/// completion, as it can on a real network.
async fn write_split(server: &mut DuplexStream, first: &[u8], then: &[u8]) {
    server.write_all(first).await.unwrap();
    server.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    server.write_all(then).await.unwrap();
}

/// A server that reports the two settings at startup, then answers three requests: a simple
/// `SET` whose report of a new `client_encoding` comes after its completion; an extended `SET`
/// (prepared, then executed) whose report of a new `standard_conforming_strings` comes the same
/// way; and a simple `SET` followed by a report of another setting and one repeating the value
/// already reported.
async fn serve_reports(mut server: DuplexStream) {
    let len = usize::try_from(server.read_i32().await.unwrap()).unwrap();
    let mut startup = vec![0; len - 4];
    server.read_exact(&mut startup).await.unwrap();
    let mut out = message(b'R', &0i32.to_be_bytes());
    out.extend(parameter_status("client_encoding", "UTF8"));
    out.extend(parameter_status("standard_conforming_strings", "on"));
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    // 1: simple query
    assert_eq!(read_tag(&mut server).await, b'Q');
    let mut then = parameter_status("client_encoding", "SJIS");
    then.extend(message(b'Z', b"I"));
    write_split(&mut server, &message(b'C', b"SET\0"), &then).await;
    // 2: the preparation, its execution, and the close of the statement when it is dropped
    read_until_sync(&mut server).await;
    let mut out = message(b'1', b"");
    out.extend(message(b't', &0i16.to_be_bytes()));
    out.extend(message(b'n', b""));
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    read_until_sync(&mut server).await;
    let mut first = message(b'2', b"");
    first.extend(message(b'C', b"SET\0"));
    let mut then = parameter_status("standard_conforming_strings", "off");
    then.extend(message(b'Z', b"I"));
    write_split(&mut server, &first, &then).await;
    read_until_sync(&mut server).await;
    let mut out = message(b'3', b"");
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    // 3: no new value of either setting
    assert_eq!(read_tag(&mut server).await, b'Q');
    let mut out = message(b'C', b"SET\0");
    out.extend(parameter_status("application_name", "erp-test"));
    out.extend(parameter_status("client_encoding", "SJIS"));
    out.extend(message(b'Z', b"I"));
    server.write_all(&out).await.unwrap();
    assert_eq!(read_tag(&mut server).await, b'X', "the client terminates");
}

/// Review round 5, M3 (the driver patch, amendment A2): the count of new
/// `client_encoding` and `standard_conforming_strings` reports includes the reports of a request
/// as soon as the request returns, also when the report reaches the client after the command's
/// completion (the server sends it before `ReadyForQuery`, and the driver hands the request its
/// `ReadyForQuery` only after it applied the report). The values at startup are the baseline;
/// another setting, or a report of the value already known, does not count.
#[tokio::test]
async fn the_driver_counts_a_new_string_reading_before_the_request_returns() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let (client_side, server_side) = tokio::io::duplex(64 * 1024);
        let server = tokio::spawn(serve_reports(server_side));
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
        assert_eq!(client.string_reading_changes(), 0);
        client
            .batch_execute("SET client_encoding = 'SJIS'")
            .await
            .unwrap();
        assert_eq!(client.string_reading_changes(), 1);
        client
            .execute("SET standard_conforming_strings = off", &[])
            .await
            .unwrap();
        assert_eq!(client.string_reading_changes(), 2);
        client
            .batch_execute("SET application_name = 'erp-test'")
            .await
            .unwrap();
        assert_eq!(client.string_reading_changes(), 2);
        drop(client);
        server.await.unwrap();
        driver.await.unwrap().unwrap();
    })
    .await
    .expect("the scripted exchange finishes");
}

/// The same against PostgreSQL: every change of either setting is reported, whatever SQL makes
/// it (`SET LOCAL`, dynamic SQL in a `DO` block, `set_config` through the extended protocol,
/// `RESET ALL`, and the end of a transaction undoing its `SET LOCAL`s); a `SET` to the value the
/// setting has, or of another setting, is not.
#[tokio::test]
async fn postgres_reports_every_change_of_string_reading() {
    let h = harness().await;
    let client = h.db().client().await.unwrap();
    let mut count = client.string_reading_changes();
    for (sql, reports) in [
        ("BEGIN", 0),
        ("SET LOCAL standard_conforming_strings = off", 1),
        (
            "DO $$ BEGIN EXECUTE 'SET LOCAL client_' || 'encoding = ''SQL_ASCII'''; END $$",
            1,
        ),
        ("SET LOCAL client_encoding = 'SQL_ASCII'", 0),
        ("SET LOCAL application_name = 'erp-test'", 0),
        ("COMMIT", 2),
        ("SET client_encoding = 'unicode'", 0),
    ] {
        client.batch_execute(sql).await.unwrap();
        let now = client.string_reading_changes();
        assert_eq!(now - count, reports, "{sql}");
        count = now;
    }
    client
        .query_one(
            "SELECT set_config('standard_conforming_strings', 'off', false)",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(client.string_reading_changes(), count + 1);
    client.batch_execute("RESET ALL").await.unwrap();
    assert_eq!(client.string_reading_changes(), count + 2);
    drop(client);
    h.cleanup().await.unwrap();
}

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .expect("harness")
}

/// An ORM on the harness database with a pool of one connection, so every transaction gets the
/// connection the previous one gave back, if it gave it back.
fn single_connection_orm(h: &Harness) -> Orm {
    let pool = erp_orm::pool::create_pool(h.db().config(), 1).unwrap();
    Orm::builder(Arc::clone(h.registry()), pool)
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

async fn backend_pid(tx: &mut Transaction) -> i32 {
    tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0)
}

/// Review round 5, L3: a connection on which the server reported a new
/// `standard_conforming_strings` or `client_encoding` is closed when its transaction ends, never
/// given to the next transaction; one without such a report is given back and reused. The report
/// alone decides: the statement that caused it is the transaction's last raw statement (seen only
/// when the transaction ends), or one whose text names neither setting (a `DO` block).
#[tokio::test]
async fn a_connection_whose_strings_read_differently_is_closed_not_reused() {
    let h = harness().await;
    let orm = single_connection_orm(&h);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let mut pid = backend_pid(&mut tx).await;
    tx.commit().await.unwrap();
    drop(tx);
    for (change, commit) in [
        ("SET LOCAL application_name = 'erp-test'", true),
        ("SET LOCAL standard_conforming_strings = on", true),
    ] {
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(backend_pid(&mut tx).await, pid, "reused before {change}");
        tx.execute(change, &[]).await.unwrap();
        if commit {
            tx.commit().await.unwrap();
        }
        drop(tx);
    }
    for (change, commit) in [
        ("SET LOCAL standard_conforming_strings = off", true),
        ("SET LOCAL standard_conforming_strings = off", false),
        (
            "DO $$ BEGIN EXECUTE 'SET LOCAL client_' || 'encoding = ''SQL_ASCII'''; END $$",
            true,
        ),
        ("SET client_encoding = 'SQL_ASCII'", true),
    ] {
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(backend_pid(&mut tx).await, pid, "before {change}");
        tx.execute(change, &[]).await.unwrap();
        if commit {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
        drop(tx);
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let next = backend_pid(&mut tx).await;
        assert_ne!(
            next, pid,
            "after {change} (commit: {commit}) the connection is not reused"
        );
        let encoding: String = tx.query("SHOW client_encoding", &[]).await.unwrap()[0].get(0);
        assert_eq!(encoding, "UTF8");
        tx.rollback().await.unwrap();
        drop(tx);
        pid = next;
    }
    h.cleanup().await.unwrap();
}

/// Review round 5, M3: in a read-intent transaction, after the server reported a new string
/// reading (here from a function, which no SQL text shows), raw SQL is refused: the ORM can no
/// longer read it with certainty.
#[tokio::test]
async fn a_read_transaction_refuses_raw_sql_after_a_new_string_reading() {
    let h = harness().await;
    let mut setup = h.begin().await.unwrap();
    setup
        .batch(
            "CREATE FUNCTION erp_test_scs_off() RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN SET LOCAL standard_conforming_strings = off; RETURN 1; END $$",
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(SUPERUSER_ID).read_only("res.users"))
        .await
        .unwrap();
    assert_eq!(tx.query("SELECT 1", &[]).await.unwrap().len(), 1);
    tx.query("SELECT erp_test_scs_off()", &[]).await.unwrap();
    let error = tx.query("SELECT 1", &[]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "'res.users' is declared effect = read but the server reported a new \
         standard_conforming_strings or client_encoding on this connection, so the ORM can no \
         longer read raw SQL with certainty; declare effect = write (or wider)."
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Review round 5, M4: the pool refuses a connection whose `standard_conforming_strings` is off
/// (here a role's default) with an error naming the setting, at checkout and through the ORM.
#[tokio::test]
async fn the_pool_refuses_a_connection_whose_strings_read_differently() {
    let h = harness().await;
    let config = h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    h.db()
        .client()
        .await
        .unwrap()
        .batch_execute(&format!(
            "ALTER ROLE \"{role}\" SET standard_conforming_strings = off"
        ))
        .await
        .unwrap();
    // the wording is pinned by `pool::tests::session_settings_that_read_strings_differently_are_refused`
    let expected = erp_orm::pool::UnsupportedSessionSetting::check("off", "UTF8")
        .unwrap_err()
        .to_string();
    let expected = expected.as_str();
    let pool = erp_orm::pool::create_pool(config, 1).unwrap();
    let error = pool.get().await.unwrap_err();
    assert!(error.to_string().contains(expected), "{error}");
    let orm = Orm::builder(Arc::clone(h.registry()), pool)
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let error = match orm.begin(SUPERUSER_ID).await {
        Ok(mut tx) => tx.query("SELECT 1", &[]).await.unwrap_err(),
        Err(error) => error,
    };
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), expected);
    h.cleanup().await.unwrap();
}

/// Review round 6, L1: a flight's statements are read before the flight is sent, and a statement
/// planned per call (`Plan::PerCall`) is parsed only after the one before it completed. The
/// third element of each `flight_for_tests` tuple chooses the plan policy (`true`:
/// `PlanPolicy::Cached`, `false`: `PlanPolicy::PerCall`, run as `Plan::PerCall`); every statement
/// here passes `PER_CALL`. When the server reports a new string reading during a flight, the
/// flight itself counts as unknown once it returns (every model's tick moves), and so does every
/// later raw statement.
#[tokio::test]
async fn a_flight_that_changes_string_reading_counts_as_unknown() {
    // `flight_for_tests`: `false` selects `PlanPolicy::PerCall`
    const PER_CALL: bool = false;
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let tick = tx.every_model_tick_for_tests();
    tx.flight_for_tests(
        &[("SELECT 1", &[], PER_CALL), ("SELECT 2", &[], PER_CALL)],
        None,
    )
    .await
    .unwrap();
    assert_eq!(tx.every_model_tick_for_tests(), tick, "a flight of reads");
    tx.flight_for_tests(
        &[
            (
                "SELECT set_config('standard_conforming_strings', 'off', true)",
                &[],
                PER_CALL,
            ),
            ("SELECT 1", &[], PER_CALL),
        ],
        None,
    )
    .await
    .unwrap();
    assert!(
        tx.every_model_tick_for_tests() > tick,
        "the flight that changed the string reading is unknown"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Review round 6, L2: the count a transaction compares with is the connection's since it was
/// opened (the pool refused it if it opened with other settings), not the count when the
/// transaction got it. A connection a user of the pool outside a transaction changed is not
/// trusted by the next transaction: its raw statements are unknown and the connection is closed
/// when the transaction ends.
#[tokio::test]
async fn a_connection_changed_outside_a_transaction_is_not_trusted() {
    let h = harness().await;
    let pool = erp_orm::pool::create_pool(h.db().config(), 1).unwrap();
    let orm = Orm::builder(Arc::clone(h.registry()), pool.clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let client = pool.get().await.unwrap();
    client
        .batch_execute("SET standard_conforming_strings = off")
        .await
        .unwrap();
    drop(client);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let pid = backend_pid(&mut tx).await;
    let tick = tx.every_model_tick_for_tests();
    tx.query("SELECT 1", &[]).await.unwrap();
    assert!(
        tx.every_model_tick_for_tests() > tick,
        "a raw statement on a changed connection is unknown"
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_ne!(
        backend_pid(&mut tx).await,
        pid,
        "the changed connection was closed"
    );
    let reading: String = tx
        .query("SHOW standard_conforming_strings", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(reading, "on");
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

/// Review round 6, L4: `client_encoding` cannot open as anything but `UTF8` through this driver:
/// it sends `client_encoding=UTF8` in its startup packet (first; `options` comes last), a setting
/// in the startup packet outranks a role's or database's default, and the server applies it
/// after `options` in its startup processing, so it wins over an `options` value too. Neither a
/// role default nor an `options` string changes it. The pool still checks `client_encoding`, as
/// a guard: the ordering is the server's behaviour, and something that runs after startup (a
/// login event trigger, a connection pooler between) could change it.
#[tokio::test]
async fn client_encoding_opens_as_utf8_whatever_the_defaults() {
    let h = harness().await;
    let mut config = h.read_only_role().await.unwrap();
    let role = config.get_user().unwrap().to_owned();
    h.db()
        .client()
        .await
        .unwrap()
        .batch_execute(&format!(
            "ALTER ROLE \"{role}\" SET client_encoding = 'SQL_ASCII'"
        ))
        .await
        .unwrap();
    config.options("-c client_encoding=LATIN1");
    let pool = erp_orm::pool::create_pool(config, 1).unwrap();
    let client = pool.get().await.unwrap();
    let encoding: String = client
        .query_one("SHOW client_encoding", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(encoding, "UTF8");
    assert_eq!(client.string_reading_changes(), 0);
    drop(client);
    h.cleanup().await.unwrap();
}
