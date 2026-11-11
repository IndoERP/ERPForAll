//! the opening of a transaction is one round trip, measured on the wire
//! (§3, §6). `BEGIN`, the `SET LOCAL` limits, the signal read and the
//! principal resolver's statement leave in one flight. Single-threaded runtime on purpose: the
//! futures of a flight are polled together, so their requests share one write.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use support::currency_fixture;

mod support;

use std::future::Future;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use erp_fields::{Condition, ErrorKind};
use erp_orm::{
    AccessPolicy, BeginOptions, BoxFuture, Orm, OrmConfig, Principal, PrincipalBuilder,
    PrincipalHint, PrincipalResolver, Resolution, ResolverStatement, Signal, StaticPolicy,
    Timeouts, Transaction, UserId,
};
use erp_sql::param::{Param, Scalar};
use erp_test_models::{base_users, test_orm};
use erp_testkit::tokio_postgres::Row;
use erp_testkit::{Harness, WireCounter, assert_round_trips};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

async fn harness(config: OrmConfig) -> Harness {
    Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .config(config)
        .count_round_trips()
        .build()
        .await
        .expect("harness")
}

fn wire(h: &Harness) -> WireCounter {
    h.wire().expect("built with count_round_trips").clone()
}

/// Run one throwaway transaction so the pooled connection exists and the next `begin` reuses it.
async fn warm(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    tx.query("SELECT 1", &[]).await.unwrap();
    tx.commit().await.unwrap();
}

/// The `setting` of `pg_settings` as the server reports it in this transaction (milliseconds).
async fn setting(tx: &mut Transaction, name: &str) -> i64 {
    let rows = tx
        .query(
            "SELECT setting::int8 FROM pg_settings WHERE name = $1::text",
            &[&name],
        )
        .await
        .unwrap();
    rows[0].get(0)
}

/// Budget (no spec case): opening a read/write transaction is one round trip, on a connection
/// that has not run a statement of this test yet and on one that has.
#[tokio::test]
async fn write_start_is_one_round_trip() {
    let h = harness(OrmConfig::default()).await;
    let wire = wire(&h);
    // begin_opts checks the connection out, `open` is the opening flight
    let mut cold = h.begin().await.unwrap();
    wire.reset();
    assert_round_trips!(wire, == 1, {
        cold.open().await.unwrap();
    });
    cold.commit().await.unwrap();
    let mut warm = h.begin().await.unwrap();
    assert_round_trips!(wire, == 1, {
        warm.open().await.unwrap();
    });
    warm.commit().await.unwrap();
}

#[tokio::test]
async fn read_start_is_one_round_trip() {
    let h = harness(OrmConfig::default()).await;
    let wire = wire(&h);
    warm(&h).await;
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).read_only("res.users.read"))
        .await
        .unwrap();
    assert!(tx.intent().is_read());
    wire.reset();
    assert_round_trips!(wire, == 1, {
        tx.open().await.unwrap();
    });
    tx.rollback().await.unwrap();
}

/// The first statement after the start adds exactly its own round trip (warm: prepared).
#[tokio::test]
async fn first_statement_after_start_adds_its_own() {
    let h = harness(OrmConfig::default()).await;
    let wire = wire(&h);
    warm(&h).await;
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    wire.reset();
    let rows = assert_round_trips!(wire, == 1, { tx.query("SELECT 1", &[]).await.unwrap() });
    assert_eq!(rows.len(), 1);
    tx.rollback().await.unwrap();
}

/// With signals on, the generation read rides in the same flight: still one round trip, on a
/// connection that never prepared the read (it is an unnamed statement, no Parse round trip).
#[tokio::test]
async fn start_with_signal_is_one_round_trip() {
    let h = harness(OrmConfig::default()).await;
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let baseline = {
        let client = h.db().client().await.unwrap();
        Signal::read(&client).await.unwrap()
    };
    let wire = wire(&h);
    let orm = Orm::builder(Arc::clone(h.registry()), wire.pool(2).unwrap())
        .policy(StaticPolicy::AllowAll)
        .signal(Signal::new(baseline))
        .build()
        .unwrap();
    // a fresh pool: its first connection has prepared nothing
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    wire.reset();
    assert_round_trips!(wire, == 1, {
        tx.open().await.unwrap();
    });
    assert!(tx.began_generations().is_some(), "the read was applied");
    assert_eq!(
        tx.query_count(),
        0,
        "the signal read is not a statement of the transaction"
    );
    tx.rollback().await.unwrap();
}

/// Reads `res_users` for the subject; counts what it was given.
struct CountingResolver {
    finished_rows: Arc<AtomicUsize>,
}

impl PrincipalResolver for CountingResolver {
    fn statement(&self, hint: &PrincipalHint) -> erp_fields::Result<ResolverStatement> {
        Ok(ResolverStatement::new(
            "SELECT id FROM res_users WHERE id = $1::int8",
            vec![Param::Scalar(Scalar::Int8(hint.subject().get()))],
        ))
    }

    fn finish(
        &self,
        hint: &PrincipalHint,
        rows: &[Row],
        policy: &dyn AccessPolicy,
    ) -> erp_fields::Result<Principal> {
        self.finished_rows.store(rows.len(), Ordering::SeqCst);
        let resolution = Resolution::in_id_order(policy.groups(&[])?, Vec::new(), NonZeroU64::MIN)?;
        Ok(PrincipalBuilder::user(hint.subject())
            .resolution(resolution)
            .build()?)
    }

    fn re_resolve<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _subjects: &'a [UserId],
    ) -> BoxFuture<'a, erp_fields::Result<Vec<Resolution>>> {
        Box::pin(async { Err(erp_fields::Error::programming("not used here")) })
    }
}

/// The principal resolver's statement is pipelined with `BEGIN`: opening for a user principal is
/// one round trip in total, the resolved principal is in place before `begin_opts` returns, and
/// the statement counts in `query_count` as before.
#[tokio::test]
async fn start_resolving_a_principal_is_one_round_trip() {
    let seen = Arc::new(AtomicUsize::new(usize::MAX));
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .resolver(CountingResolver {
            finished_rows: Arc::clone(&seen),
        })
        .count_round_trips()
        .build()
        .await
        .unwrap();
    {
        let mut tx = h.begin().await.unwrap();
        tx.batch("INSERT INTO res_users (id, name, login) VALUES (2, 'u', 'u2')")
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let wire = wire(&h);
    let user = UserId::new(2).unwrap();
    // a connection is checked out (and the pool hook has run) before the measured span
    let mut first = h.orm().begin(user).await.unwrap();
    first.commit().await.unwrap();
    // a committed transaction keeps its connection until dropped
    drop(first);
    wire.reset();
    let tx = assert_round_trips!(wire, == 1, { h.orm().begin(user).await.unwrap() });
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the resolver was given the user's row"
    );
    assert_eq!(tx.env().principal().subject(), user);
    assert_eq!(
        tx.query_count(),
        1,
        "the resolver's statement is a statement of the transaction"
    );
}

/// The limits arrive with `BEGIN`: the server reports what the config (or the per-transaction
/// override) says, in milliseconds.
#[tokio::test]
async fn limits_are_set_local_at_start() {
    let config = OrmConfig {
        timeouts: Timeouts::new(ms(1_500), ms(20_000), ms(900_000)).unwrap(),
        ..OrmConfig::default()
    };
    let h = harness(config.clone()).await;
    let mut tx = h.begin().await.unwrap();
    for (name, limit) in [
        ("lock_timeout", config.timeouts.lock()),
        ("statement_timeout", config.timeouts.statement()),
        ("transaction_timeout", config.timeouts.transaction()),
    ] {
        assert_eq!(
            setting(&mut tx, name).await,
            i64::try_from(limit.as_millis()).unwrap(),
            "{name}"
        );
    }
    tx.rollback().await.unwrap();

    let own = Timeouts::new(ms(700), ms(8_000), ms(60_000)).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    assert_eq!(setting(&mut tx, "lock_timeout").await, 700);
    assert_eq!(setting(&mut tx, "statement_timeout").await, 8_000);
    assert_eq!(setting(&mut tx, "transaction_timeout").await, 60_000);
    tx.commit().await.unwrap();
    // SET LOCAL: nothing is left on the pooled connection
    let mut after = h.begin().await.unwrap();
    assert_eq!(setting(&mut after, "lock_timeout").await, 1_500);
}

#[tokio::test]
async fn statement_timeout_gives_the_typed_error_naming_the_limit() {
    let h = harness(OrmConfig::default()).await;
    let own = Timeouts::new(ms(100), ms(200), ms(60_000)).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    let error = tx.query("SELECT pg_sleep(5)", &[]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.condition(), Some(Condition::StatementTimeout));
    assert_eq!(error.sqlstate(), Some("57014"));
    assert_eq!(
        error.message(),
        "canceling statement due to statement timeout (statement_timeout was 200 ms when the \
         transaction began)"
    );
    // the transaction is aborted like after any server error, the connection is kept
    let next = tx.query("SELECT 1", &[]).await.unwrap_err();
    assert_eq!(
        next.message(),
        "current transaction is aborted after an earlier error (canceling statement due to \
         statement timeout); roll back first"
    );
    assert!(!tx.is_released());
}

#[tokio::test]
async fn lock_timeout_gives_the_typed_error_naming_the_limit() {
    let h = harness(OrmConfig::default()).await;
    let mut holder = h.begin().await.unwrap();
    holder
        .execute("UPDATE res_users SET name = name WHERE id = 1", &[])
        .await
        .unwrap();
    let own = Timeouts::new(ms(200), ms(30_000), ms(60_000)).unwrap();
    let mut waiter = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    let error = waiter
        .execute("UPDATE res_users SET name = name WHERE id = 1", &[])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Lock);
    assert_eq!(error.condition(), Some(Condition::LockTimeout));
    assert_eq!(error.sqlstate(), Some("55P03"));
    assert_eq!(
        error.message(),
        "canceling statement due to lock timeout (lock_timeout was 200 ms when the transaction \
         began)"
    );
    holder.rollback().await.unwrap();
}

/// The backstop ends the session (`25P04`): the transaction is released, the connection is not
/// returned to the pool, and the next transaction works on a new one. Wide margins: the shared CI
/// host can be slow, and the statements must not hit their own limit or the backstop early.
#[tokio::test]
async fn transaction_timeout_ends_the_session_and_recycles_the_connection() {
    let h = harness(OrmConfig::default()).await;
    let own = Timeouts::new(ms(500), ms(1_900), ms(2_000)).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    // each statement is within statement_timeout; together they pass the backstop
    tx.query("SELECT pg_sleep(1.2)", &[]).await.unwrap();
    let error = tx.query("SELECT pg_sleep(1.2)", &[]).await.unwrap_err();
    assert_eq!(error.condition(), Some(Condition::TransactionTimeout));
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("25P04"));
    assert_eq!(
        error.message(),
        "terminating connection due to transaction timeout (transaction_timeout was 2000 ms when \
         the transaction began); the connection was discarded, split the work into shorter \
         transactions"
    );
    assert!(tx.is_released());
    let mut next = h.begin().await.unwrap();
    assert_eq!(next.query("SELECT 1", &[]).await.unwrap().len(), 1);
    next.rollback().await.unwrap();
}

/// The backstop fires while the transaction is idle (the forgotten transaction it exists for):
/// no statement is waiting for the server's `25P04`, so the next statement only sees a closed
/// connection. It is still typed, with the closed connection as the cause. The wait is on the
/// client and far past the limit (1 s limit, 2.5 s wait) for a slow host.
#[tokio::test]
async fn the_backstop_firing_while_idle_is_typed_on_the_next_statement() {
    let h = harness(OrmConfig::default()).await;
    let own = Timeouts::new(ms(200), ms(400), ms(1_000)).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    tx.open().await.unwrap();
    tokio::time::sleep(ms(2_500)).await;
    let error = tx.query("SELECT 1", &[]).await.unwrap_err();
    assert_eq!(error.condition(), Some(Condition::TransactionTimeout));
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), None, "the server's error was never seen");
    let prefix = "the connection was closed after the transaction had been open for ";
    let suffix = " ms, past the transaction_timeout backstop (the likely cause; transaction_timeout \
                  was 1000 ms when the transaction began); the connection was discarded, split \
                  the work into shorter transactions";
    let message = error.message();
    let open_for: u64 = message
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
        .unwrap_or_else(|| panic!("unexpected message: {message}"))
        .parse()
        .unwrap();
    assert!(open_for >= 2_500, "{open_for} ms");
    assert!(
        std::error::Error::source(&error).is_some(),
        "the closed connection is the cause"
    );
    assert!(tx.is_released());
    let mut next = h.begin().await.unwrap();
    assert_eq!(next.query("SELECT 1", &[]).await.unwrap().len(), 1);
    next.rollback().await.unwrap();
}

/// A role or database default for `transaction_timeout` (here a session option, which arms the
/// timer the same way) must not replace the ORM's backstop. PostgreSQL 18 arms the timer when the
/// transaction starts and ignores a positive `SET LOCAL` while it runs; the ORM sets 0 first and
/// then its value, which starts a new timer.
#[tokio::test]
async fn a_role_default_for_transaction_timeout_does_not_replace_the_backstop() {
    let h = harness(OrmConfig::default()).await;
    let mut config = h.db().config();
    config.options("-c transaction_timeout=300ms");
    // the server's behaviour, with no ORM involved: the positive SET LOCAL does not move the timer
    let raw_pool = erp_orm::pool::create_pool(config.clone(), 1).unwrap();
    let raw = raw_pool.get().await.unwrap();
    raw.batch_execute("BEGIN; SET LOCAL transaction_timeout = 5000")
        .await
        .unwrap();
    let error = raw.batch_execute("SELECT pg_sleep(1.0)").await.unwrap_err();
    assert_eq!(
        error.as_db_error().map(|db| db.code().code()),
        Some("25P04"),
        "PostgreSQL ignored SET LOCAL transaction_timeout = 5000 under a running 300 ms timer"
    );
    drop(raw);
    // through the ORM (its own pool, so the dead connection above is not reused): the default
    // backstop (1 h) applies, a 1 s statement is fine
    let pool = erp_orm::pool::create_pool(config, 2).unwrap();
    let orm = Orm::builder(Arc::clone(h.registry()), pool)
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(setting(&mut tx, "transaction_timeout").await, 3_600_000);
    tx.query("SELECT pg_sleep(1.0)", &[]).await.unwrap();
    tx.commit().await.unwrap();
}

/// The routines PostgreSQL 18 reports for the three errors the classifier tells apart. They are
/// not translated, so the classification holds in any `lc_messages`; a rename in a new server
/// version fails here instead of silently turning the typed errors off.
#[tokio::test]
async fn postgresql_reports_the_routines_the_classifier_relies_on() {
    let h = harness(OrmConfig::default()).await;
    let holder = h.db().client().await.unwrap();
    holder
        .batch_execute("BEGIN; UPDATE res_users SET name = name WHERE id = 1")
        .await
        .unwrap();
    let routine_of = |error: &erp_testkit::tokio_postgres::Error| {
        let db = error.as_db_error().expect("a server error");
        (db.code().code().to_owned(), db.routine().map(str::to_owned))
    };
    let waiter = h.db().client().await.unwrap();
    let lock = waiter
        .batch_execute(
            "BEGIN; SET LOCAL lock_timeout = 100; UPDATE res_users SET name = name WHERE id = 1",
        )
        .await
        .unwrap_err();
    assert_eq!(
        routine_of(&lock),
        ("55P03".to_owned(), Some("ProcessInterrupts".to_owned()))
    );
    waiter.batch_execute("ROLLBACK").await.unwrap();
    let nowait = waiter
        .batch_execute("BEGIN; SELECT 1 FROM res_users WHERE id = 1 FOR UPDATE NOWAIT")
        .await
        .unwrap_err();
    assert_eq!(
        routine_of(&nowait),
        ("55P03".to_owned(), Some("heap_lock_tuple".to_owned()))
    );
    waiter.batch_execute("ROLLBACK").await.unwrap();
    let statement = waiter
        .batch_execute("BEGIN; SET LOCAL statement_timeout = 100; SELECT pg_sleep(2)")
        .await
        .unwrap_err();
    assert_eq!(
        routine_of(&statement),
        ("57014".to_owned(), Some("ProcessInterrupts".to_owned()))
    );
    holder.batch_execute("ROLLBACK").await.unwrap();
}

/// A lock wait that is cut by `NOWAIT` is not the lock limit: it keeps its own error.
#[tokio::test]
async fn nowait_keeps_its_own_error() {
    let h = harness(OrmConfig::default()).await;
    let mut holder = h.begin().await.unwrap();
    holder
        .execute("UPDATE res_users SET name = name WHERE id = 1", &[])
        .await
        .unwrap();
    let mut other = h.begin().await.unwrap();
    let error = other
        .query(
            "SELECT id FROM res_users WHERE id = 1 FOR UPDATE NOWAIT",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Lock);
    assert_eq!(error.condition(), None);
    assert_eq!(error.sqlstate(), Some("55P03"));
    assert_eq!(
        error.message(),
        "could not obtain lock on row in relation \"res_users\""
    );
    holder.rollback().await.unwrap();
}

/// The typed limit errors do not read the server's message: they hold under a server language
/// other than English. Runs when the cluster has such a locale installed, else says why not.
#[tokio::test]
async fn typed_limits_hold_under_a_non_english_server_language() {
    let h = harness(OrmConfig::default()).await;
    let own = Timeouts::new(ms(200), ms(300), ms(60_000)).unwrap();
    let mut chosen = None;
    for locale in [
        "de_DE.utf8",
        "fr_FR.utf8",
        "es_ES.utf8",
        "id_ID.utf8",
        "ja_JP.utf8",
    ] {
        let mut probe = h.begin().await.unwrap();
        let usable = probe
            .query(
                "SELECT set_config('lc_messages', $1::text, true)",
                &[&locale],
            )
            .await
            .is_ok();
        drop(probe);
        if usable {
            chosen = Some(locale);
            break;
        }
    }
    let Some(locale) = chosen else {
        eprintln!(
            "skipped: no non-English locale is installed on the test cluster; the classifier is \
             unit-tested on the error fields (timeouts.rs) and the routines are pinned by \
             postgresql_reports_the_routines_the_classifier_relies_on"
        );
        return;
    };
    let mut holder = h.begin().await.unwrap();
    holder
        .execute("UPDATE res_users SET name = name WHERE id = 1", &[])
        .await
        .unwrap();
    let mut waiter = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    waiter
        .query(
            "SELECT set_config('lc_messages', $1::text, true)",
            &[&locale],
        )
        .await
        .unwrap();
    let lock = waiter
        .execute("UPDATE res_users SET name = name WHERE id = 1", &[])
        .await
        .unwrap_err();
    assert_eq!(lock.condition(), Some(Condition::LockTimeout), "{lock:?}");
    holder.rollback().await.unwrap();
    let mut slow = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    slow.query(
        "SELECT set_config('lc_messages', $1::text, true)",
        &[&locale],
    )
    .await
    .unwrap();
    let statement = slow.query("SELECT pg_sleep(5)", &[]).await.unwrap_err();
    assert_eq!(
        statement.condition(),
        Some(Condition::StatementTimeout),
        "{statement:?}"
    );
}

/// The backstop fires during `COMMIT` itself (a deferred trigger that runs long; the statement
/// limit does not cover deferred triggers, the backstop does): the error is typed like the one of
/// any other statement.
#[tokio::test]
async fn the_backstop_firing_during_commit_is_typed() {
    let h = harness(OrmConfig::default()).await;
    slow_commit_probe(&h).await;
    let own = Timeouts::new(ms(100), ms(800), ms(1_000)).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    tx.execute("INSERT INTO rtt_commit_probe VALUES (1)", &[])
        .await
        .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.condition(), Some(Condition::TransactionTimeout));
    assert_eq!(error.sqlstate(), Some("25P04"));
    assert_eq!(
        error.message(),
        "terminating connection due to transaction timeout (transaction_timeout was 1000 ms when \
         the transaction began); the connection was discarded, split the work into shorter \
         transactions"
    );
    assert!(tx.is_released());
}

/// The backstop fired while the transaction was idle and the next thing is `COMMIT`: the
/// connection is closed, the error is typed (and the transaction is not reported committed).
#[tokio::test]
async fn commit_after_an_idle_backstop_is_typed() {
    let h = harness(OrmConfig::default()).await;
    let own = Timeouts::new(ms(100), ms(400), ms(1_000)).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    tx.open().await.unwrap();
    tokio::time::sleep(ms(2_500)).await;
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.condition(), Some(Condition::TransactionTimeout));
    assert_eq!(error.kind(), ErrorKind::Database);
    assert!(
        error
            .message()
            .starts_with("the connection was closed after the transaction had been open for "),
        "{}",
        error.message()
    );
    assert!(tx.is_released());
}

/// The same for the statement sent together with `COMMIT` (`commit_pipelined`): it is the last
/// write of a request and the usual place to wait for a lock.
#[tokio::test]
async fn a_statement_timeout_on_the_final_statement_is_typed() {
    let h = harness(OrmConfig::default()).await;
    let own = Timeouts::new(ms(100), ms(300), ms(60_000)).unwrap();
    let mut tx = h
        .orm()
        .begin_opts(BeginOptions::new(erp_orm::SUPERUSER_ID).timeouts(own))
        .await
        .unwrap();
    tx.open().await.unwrap();
    let no_params: [(
        &(dyn erp_testkit::tokio_postgres::types::ToSql + Sync),
        erp_testkit::tokio_postgres::types::Type,
    ); 0] = [];
    let last = erp_orm::FinalStatement::new("SELECT pg_sleep(3)", &no_params);
    let Err(erp_orm::CommitError::RolledBack(error)) = tx.commit_pipelined(Some(last)).await else {
        panic!("a timed-out final statement is a definite rollback");
    };
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.condition(), Some(Condition::StatementTimeout));
    assert_eq!(error.sqlstate(), Some("57014"));
    assert_eq!(
        error.message(),
        "canceling statement due to statement timeout (statement_timeout was 300 ms when the \
         transaction began)"
    );
}

/// Budget (no spec case): the last statement and `COMMIT` leave together, so a pipelined commit
/// with a final statement costs one round trip and counts the statement once.
#[tokio::test]
async fn a_pipelined_commit_with_a_final_statement_is_one_round_trip() {
    let h = harness(OrmConfig::default()).await;
    let wire = wire(&h);
    warm(&h).await;
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    let no_params: [(
        &(dyn erp_testkit::tokio_postgres::types::ToSql + Sync),
        erp_testkit::tokio_postgres::types::Type,
    ); 0] = [];
    let last = erp_orm::FinalStatement::new("SELECT 1", &no_params);
    let before = tx.query_count();
    wire.reset();
    assert_round_trips!(wire, == 1, {
        tx.commit_pipelined(Some(last)).await.unwrap();
    });
    assert_eq!(tx.query_count(), before + 1);
}

/// A final statement the driver refuses before sending (a parameter of the wrong type) must not
/// be followed by `COMMIT`: the earlier writes roll back, the caller gets a definite
/// `RolledBack` (a retry with an idempotency record as the final statement is then safe), and
/// the connection is usable afterwards.
#[tokio::test]
async fn a_final_statement_refused_by_the_driver_rolls_back_the_earlier_writes() {
    let h = harness(OrmConfig::default()).await;
    let mut setup = h.begin().await.unwrap();
    setup
        .batch("CREATE TABLE rtt_final_probe (id int)")
        .await
        .unwrap();
    setup.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.execute("INSERT INTO rtt_final_probe VALUES (1)", &[])
        .await
        .unwrap();
    let wrong: i64 = 1;
    let params = [(
        &wrong as &(dyn erp_testkit::tokio_postgres::types::ToSql + Sync),
        erp_testkit::tokio_postgres::types::Type::TEXT,
    )];
    let last = erp_orm::FinalStatement::new("SELECT $1::text", &params);
    let before = tx.query_count();
    let Err(erp_orm::CommitError::RolledBack(error)) = tx.commit_pipelined(Some(last)).await else {
        panic!("a final statement that never left the process is a definite rollback");
    };
    assert_eq!(
        tx.query_count(),
        before,
        "a statement that never left the process is not counted"
    );
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(
        error.message(),
        "statement failed in the database client: error serializing parameter 0"
    );
    assert_eq!(
        error.sqlstate(),
        None,
        "a client-side refusal has no sqlstate"
    );
    assert!(!tx.is_released(), "the transaction is reusable");
    tx.query("SELECT 1", &[]).await.unwrap();

    let mut check = h.begin().await.unwrap();
    let rows = check
        .query("SELECT id FROM rtt_final_probe", &[])
        .await
        .unwrap();
    assert!(rows.is_empty(), "the earlier write must not be committed");
    check.rollback().await.unwrap();
}

/// A final statement that fails on the server makes `COMMIT` answer `ROLLBACK`; the hooks
/// registered for that attempt must not survive into the next one on the same transaction.
#[tokio::test]
async fn after_commit_hooks_do_not_survive_a_server_side_final_statement_failure() {
    let h = harness(OrmConfig::default()).await;
    let mut setup = h.begin().await.unwrap();
    setup
        .batch("CREATE TABLE rtt_hook_probe (id int PRIMARY KEY); INSERT INTO rtt_hook_probe VALUES (1)")
        .await
        .unwrap();
    setup.commit().await.unwrap();

    let ran = Arc::new(AtomicUsize::new(0));
    let mut tx = h.begin().await.unwrap();
    tx.open().await.unwrap();
    let hook_ran = Arc::clone(&ran);
    tx.after_commit(move || {
        hook_ran.fetch_add(1, Ordering::SeqCst);
    });
    let no_params: [(
        &(dyn erp_testkit::tokio_postgres::types::ToSql + Sync),
        erp_testkit::tokio_postgres::types::Type,
    ); 0] = [];
    let last = erp_orm::FinalStatement::new("INSERT INTO rtt_hook_probe VALUES (1)", &no_params);
    let Err(erp_orm::CommitError::RolledBack(error)) = tx.commit_pipelined(Some(last)).await else {
        panic!("a unique violation in the final statement is a definite rollback");
    };
    assert_eq!(error.sqlstate(), Some("23505"));

    tx.query("SELECT 1", &[]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "the hook belonged to the rolled-back attempt"
    );
}

/// A table whose rows fire a deferred constraint trigger that sleeps 3 s at `COMMIT`.
async fn slow_commit_probe(h: &Harness) {
    let mut setup = h.begin().await.unwrap();
    setup
        .batch(
            "CREATE TABLE rtt_commit_probe (id int); \
             CREATE FUNCTION rtt_slow() RETURNS trigger LANGUAGE plpgsql AS \
               $$ BEGIN PERFORM pg_sleep(3); RETURN NULL; END $$; \
             CREATE CONSTRAINT TRIGGER rtt_slow AFTER INSERT ON rtt_commit_probe \
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION rtt_slow()",
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
}

/// A flight where one statement fails on the client (a parameter of the wrong type never leaves
/// the process) and another on the server: the server's error is reported and the transaction is
/// aborted, so a following commit fails instead of reporting success for a rolled-back
/// transaction.
#[tokio::test]
async fn a_client_error_in_a_flight_does_not_hide_a_server_abort() {
    let h = harness(OrmConfig::default()).await;
    let mut tx = h.begin().await.unwrap();
    tx.execute("UPDATE res_users SET name = name WHERE id = 1", &[])
        .await
        .unwrap();
    let wrong_type = [Param::Scalar(Scalar::Int8(1))];
    let no_params: &[Param] = &[];
    let error = tx
        .flight_for_tests(
            &[
                // $1 is text, the parameter is an int8: refused by the driver before it is sent
                ("SELECT $1::text", &wrong_type, true),
                ("SELECT 1 / (SELECT 0)", no_params, true),
            ],
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("22012"));
    assert_eq!(error.message(), "division by zero");
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.kind(), ErrorKind::Database);
    assert_eq!(
        commit.message(),
        "transaction rolled back: an earlier statement failed (division by zero)"
    );
    // the client error alone leaves the transaction as it was
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .flight_for_tests(&[("SELECT $1::text", &wrong_type, true)], None)
        .await
        .unwrap_err();
    assert_eq!(error.sqlstate(), None);
    assert_eq!(tx.query("SELECT 1", &[]).await.unwrap().len(), 1);
    tx.rollback().await.unwrap();
}

/// A start future dropped after its flight left (a deadline): the transaction is released, a
/// later use fails loudly, and the connection (whose state is unknown) is not returned to the
/// pool.
#[tokio::test]
async fn a_dropped_start_releases_the_transaction_and_its_connection() {
    let h = harness(OrmConfig::default()).await;
    let pid_of = |mut tx: Transaction| async move {
        let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await.unwrap()[0].get(0);
        tx.rollback().await.unwrap();
        pid
    };
    let first = pid_of(h.begin().await.unwrap()).await;
    let mut tx = h.begin().await.unwrap();
    {
        let mut start = Box::pin(tx.open());
        let polled =
            std::future::poll_fn(|cx| std::task::Poll::Ready(start.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "the flight is on the wire");
    }
    let error = tx.query("SELECT 1", &[]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the transaction can no longer be used: its connection was released after a failure or a \
         dropped operation"
    );
    assert!(tx.is_released());
    drop(tx);
    let second = pid_of(h.begin().await.unwrap()).await;
    assert_ne!(first, second, "the connection was not reused");
}
