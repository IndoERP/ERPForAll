// SPDX-License-Identifier: MIT OR Apache-2.0
//! the hook transaction guard (and amendment
//! A3). Inside [`Transaction::hook_scope`] the
//! calls that end the transaction and raw transaction-control SQL are refused before anything is
//! sent; a refusal stays recorded even when the hook catches it, so `finish` fails; dropping the
//! scope restores the caller's state on `?`, a dropped future and an unwind. The savepoint API and
//! ordinary SQL stay available.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::future::{Future, poll_fn};
use std::task::Poll;

use erp_fields::{Error, ErrorKind};
use erp_orm::{CommitError, OperationKey, SavepointOutcome, Transaction};
use erp_test_models::base_users;
use erp_testkit::Harness;
use tokio_postgres::types::Type;

const HOOK: &str = "test.post_login";

/// A harness with a committed probe table, visible from every connection.
async fn world() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("CREATE TABLE tx_hook_probe (id bigserial PRIMARY KEY, note text NOT NULL)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    h
}

async fn write_probe(tx: &mut Transaction, note: &str) {
    let written = tx
        .execute("INSERT INTO tx_hook_probe (note) VALUES ($1)", &[&note])
        .await
        .unwrap();
    assert_eq!(written, 1);
}

async fn probe_count(tx: &mut Transaction, note: &str) -> i64 {
    tx.query(
        "SELECT count(*) FROM tx_hook_probe WHERE note = $1",
        &[&note],
    )
    .await
    .unwrap()[0]
        .get::<_, i64>(0)
}

async fn xact_id(tx: &mut Transaction) -> String {
    tx.query("SELECT pg_current_xact_id()::text", &[])
        .await
        .unwrap()[0]
        .get::<_, String>(0)
}

fn api_message(call: &str, hook: &str) -> String {
    format!(
        "{call}: refused inside the {hook} hook; the framework ends this transaction after the \
         hook (nothing was committed or rolled back)"
    )
}

fn batch_message(hook: &str) -> String {
    format!(
        "Transaction::batch: refused inside the {hook} hook (the simple protocol can carry several \
         statements, a transaction-control one among them); the framework ends this transaction \
         after the hook (nothing was sent)"
    )
}

fn replaced_poison_message(hook: &str) -> String {
    format!(
        "HookScope: the transaction of the {hook} hook scope was replaced (swapped) or a nested \
         scope was forgotten while the scope lived; this transaction must not commit"
    )
}

fn replaced_finish_message(hook: &str) -> String {
    format!(
        "HookScope::finish: the transaction of the {hook} hook scope was replaced (swapped) or a \
         nested scope was forgotten while the scope lived (nothing was committed)"
    )
}

fn sql_message(call: &str, keyword: &str, hook: &str) -> String {
    format!(
        "{call}: transaction control ({keyword}) is refused inside the {hook} hook; the framework \
         ends this transaction after the hook (nothing was sent)"
    )
}

fn assert_programming(error: &Error, message: &str) {
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::Programming, message)
    );
}

#[tokio::test]
async fn api_calls_that_end_the_transaction_are_refused() {
    let h = world().await;
    let mut other = h.begin().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    write_probe(&mut tx, "api").await;
    let before = xact_id(&mut tx).await;
    {
        let mut scope = tx.hook_scope(HOOK);
        let sent = scope.query_count();
        let error = scope.commit().await.unwrap_err();
        assert_programming(&error, &api_message("Transaction::commit", HOOK));
        assert_eq!(scope.query_count(), sent, "commit sent a statement");
        let error = scope.rollback().await.unwrap_err();
        assert_programming(&error, &api_message("Transaction::rollback", HOOK));
        assert_eq!(scope.query_count(), sent, "rollback sent a statement");
        let Err(CommitError::RolledBack(error)) = scope.commit_pipelined(None).await else {
            panic!("commit_pipelined inside a hook scope was not refused as rolled back");
        };
        assert_programming(&error, &api_message("Transaction::commit_pipelined", HOOK));
        assert_eq!(
            scope.query_count(),
            sent,
            "commit_pipelined sent a statement"
        );
        // the exact message tells the refusal from any other outcome of the switch, and nothing
        // is sent (a switch flushes first)
        let error = scope.switch_orm(h.orm()).await.unwrap_err();
        assert_programming(&error, &api_message("Transaction::switch_orm", HOOK));
        assert_eq!(scope.query_count(), sent, "switch_orm sent a statement");
        let error = scope.batch("SELECT 1").await.unwrap_err();
        assert_programming(&error, &batch_message(HOOK));
        assert_eq!(scope.query_count(), sent, "batch sent a statement");

        // the same transaction, the write still in it
        assert_eq!(xact_id(&mut scope).await, before);
        assert_eq!(probe_count(&mut scope, "api").await, 1);
        // and still invisible to another connection
        assert_eq!(probe_count(&mut other, "api").await, 0);

        let error = scope.finish().unwrap_err();
        assert_programming(&error, &api_message("Transaction::commit", HOOK));
    }
    assert_eq!(xact_id(&mut tx).await, before);
    tx.rollback().await.unwrap();
    other.rollback().await.unwrap();
    drop((tx, other));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn raw_transaction_control_is_refused_before_it_is_sent() {
    // (statement, the keyword the refusal names)
    const CASES: &[(&str, &str)] = &[
        ("COMMIT", "COMMIT"),
        ("commit", "COMMIT"),
        ("END", "END"),
        ("ABORT", "ABORT"),
        ("ROLLBACK", "ROLLBACK"),
        ("ROLLBACK TO SAVEPOINT x", "ROLLBACK"),
        ("START TRANSACTION", "START"),
        ("BEGIN", "BEGIN"),
        ("SAVEPOINT x", "SAVEPOINT"),
        ("RELEASE x", "RELEASE"),
        ("PREPARE TRANSACTION 'x'", "PREPARE"),
        ("prepare transaction 'x'", "PREPARE"),
        ("COMMIT PREPARED 'x'", "COMMIT"),
        ("ROLLBACK PREPARED 'x'", "ROLLBACK"),
        ("/* c */ COMMIT", "COMMIT"),
        ("-- c\nCOMMIT", "COMMIT"),
        // the word scanner skips punctuation, so the first word of a leading `;` is the keyword
        (";COMMIT", "COMMIT"),
    ];
    let h = world().await;
    let mut other = h.begin().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    write_probe(&mut tx, "raw").await;
    let before = xact_id(&mut tx).await;
    {
        let mut scope = tx.hook_scope(HOOK);
        for (sql, keyword) in CASES {
            let sent = scope.query_count();
            let error = scope.query(sql, &[]).await.unwrap_err();
            assert_programming(&error, &sql_message("Transaction::query", keyword, HOOK));
            let error = scope.execute(sql, &[]).await.unwrap_err();
            assert_programming(&error, &sql_message("Transaction::execute", keyword, HOOK));
            let error = scope.query_typed(sql, &[]).await.unwrap_err();
            assert_programming(
                &error,
                &sql_message("Transaction::query_typed", keyword, HOOK),
            );
            assert_eq!(scope.query_count(), sent, "{sql:?} was sent");
        }
        // a typed statement with parameters is checked the same way
        let sent = scope.query_count();
        let error = scope
            .query_typed("COMMIT", &[(&1i64, Type::INT8)])
            .await
            .unwrap_err();
        assert_programming(
            &error,
            &sql_message("Transaction::query_typed", "COMMIT", HOOK),
        );
        assert_eq!(scope.query_count(), sent);
        // the streamed path is checked the same way
        for (sql, keyword) in CASES {
            let error = scope
                .query_each(sql, &[], |_| panic!("{sql:?} returned a row"))
                .await
                .unwrap_err();
            assert_programming(
                &error,
                &sql_message("Transaction::query_each", keyword, HOOK),
            );
        }
        assert_eq!(scope.query_count(), sent);

        assert_eq!(xact_id(&mut scope).await, before);
        assert_eq!(probe_count(&mut scope, "raw").await, 1);
        assert_eq!(probe_count(&mut other, "raw").await, 0);
        let error = scope.finish().unwrap_err();
        assert_programming(&error, &sql_message("Transaction::query", "COMMIT", HOOK));
    }
    tx.rollback().await.unwrap();
    other.rollback().await.unwrap();
    drop((tx, other));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn prepare_of_a_statement_is_not_refused() {
    let h = world().await;
    let mut tx = h.begin().await.unwrap();
    {
        let mut scope = tx.hook_scope(HOOK);
        scope
            .execute("PREPARE tx_hook_p AS SELECT 1", &[])
            .await
            .unwrap();
        // a prepared statement outlives the transaction on the pooled connection
        scope.execute("DEALLOCATE tx_hook_p", &[]).await.unwrap();
        scope.finish().unwrap();
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn multi_statement_text_is_refused_by_the_server() {
    const SQL: &str = "SELECT 1; COMMIT";
    let h = world().await;
    for call in ["query", "execute", "query_typed"] {
        // a fresh transaction each time: the server error aborts it
        let mut tx = h.begin().await.unwrap();
        write_probe(&mut tx, call).await;
        {
            let mut scope = tx.hook_scope(HOOK);
            let error = match call {
                "query" => scope.query(SQL, &[]).await.map(|_| ()),
                "execute" => scope.execute(SQL, &[]).await.map(|_| ()),
                _ => scope.query_typed(SQL, &[]).await.map(|_| ()),
            }
            .unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::Database,
                "{call}: {}",
                error.message()
            );
            assert_eq!(
                error.sqlstate(),
                Some("42601"),
                "{call}: {}",
                error.message()
            );
            // the guard refused nothing, so the scope finishes cleanly
            scope.finish().unwrap();
        }
        // the transaction is aborted now: a new observer (a fresh REPEATABLE READ snapshot taken
        // after the refusal) looks, then another one after the rollback
        let mut other = h.begin().await.unwrap();
        assert_eq!(probe_count(&mut other, call).await, 0, "{call}");
        other.rollback().await.unwrap();
        tx.rollback().await.unwrap();
        let mut other = h.begin().await.unwrap();
        assert_eq!(probe_count(&mut other, call).await, 0, "{call}");
        other.rollback().await.unwrap();
    }
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn do_block_that_commits_is_refused_by_the_server() {
    let h = world().await;
    let mut other = h.begin().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    write_probe(&mut tx, "do").await;
    {
        let mut scope = tx.hook_scope(HOOK);
        let error = scope
            .execute("DO $$BEGIN COMMIT; END$$", &[])
            .await
            .unwrap_err();
        // PostgreSQL refuses transaction control in a DO block run inside a transaction block
        // (invalid_transaction_termination); dberror maps a state it does not name to Database
        assert_eq!(error.kind(), ErrorKind::Database, "{}", error.message());
        assert_eq!(error.sqlstate(), Some("2D000"), "{}", error.message());
        scope.finish().unwrap();
    }
    assert_eq!(probe_count(&mut other, "do").await, 0);
    tx.rollback().await.unwrap();
    assert_eq!(probe_count(&mut other, "do").await, 0);
    other.rollback().await.unwrap();
    drop((tx, other));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn savepoints_work_inside_a_hook_scope() {
    let h = world().await;
    let mut tx = h.begin().await.unwrap();
    {
        let mut scope = tx.hook_scope(HOOK);
        scope
            .savepoint(async |t: &mut Transaction| {
                write_probe(t, "kept").await;
                Ok(())
            })
            .await
            .unwrap();
        let outcome = scope
            .savepoint_outcome(async |t: &mut Transaction| {
                write_probe(t, "undone").await;
                Err::<(), _>(Error::validation("undo the savepoint"))
            })
            .await;
        let SavepointOutcome::RolledBack(error) = outcome else {
            panic!("the failing savepoint was not rolled back cleanly");
        };
        assert_eq!(
            (error.kind(), error.message()),
            (ErrorKind::Validation, "undo the savepoint")
        );
        assert_eq!(probe_count(&mut scope, "kept").await, 1);
        assert_eq!(probe_count(&mut scope, "undone").await, 0);
        scope.finish().unwrap();
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn dropping_the_scope_restores_statements_and_a_dropped_refusal_blocks_the_commit() {
    let h = world().await;
    let mut tx = h.begin().await.unwrap();
    write_probe(&mut tx, "restore").await;

    // `?` out of an async block
    let error = async {
        let mut scope = tx.hook_scope(HOOK);
        scope.commit().await?;
        Ok::<(), Error>(())
    }
    .await
    .unwrap_err();
    assert_programming(&error, &api_message("Transaction::commit", HOOK));
    tx.batch("SELECT 1").await.unwrap();
    tx.execute("SELECT 1", &[]).await.unwrap();

    // a pending future dropped while it owns the live scope
    {
        let mut future = Box::pin(async {
            let mut scope = tx.hook_scope(HOOK);
            let error = scope.batch("SELECT 1").await.unwrap_err();
            assert_programming(&error, &batch_message(HOOK));
            std::future::pending::<()>().await;
        });
        poll_fn(|cx| {
            assert!(future.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }
    tx.batch("SELECT 1").await.unwrap();

    // an unwind while the scope lives
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _scope = tx.hook_scope(HOOK);
        panic!("test unwind while a hook scope is active");
    }));
    assert!(panicked.is_err());
    tx.batch("SELECT 1").await.unwrap();

    // a new scope starts clean: the earlier refusals belong to the dropped scopes
    tx.hook_scope(HOOK).finish().unwrap();

    // but the first refusal that was dropped without `finish` never reached the framework, so this
    // transaction does not commit: the caller that saw the error rolls it back
    assert!(tx.is_poisoned());
    let error = tx.commit().await.unwrap_err();
    assert_programming(
        &error,
        &format!(
            "HookScope: the {HOOK} hook scope was dropped without finish after a refusal ({}); \
             this transaction must not commit",
            api_message("Transaction::commit", HOOK)
        ),
    );
    drop(tx);
    let mut check = h.begin().await.unwrap();
    assert_eq!(probe_count(&mut check, "restore").await, 0);
    check.rollback().await.unwrap();
    drop(check);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caught_refusals_stay_sticky_and_nested_scopes_carry_them_out() {
    let h = world().await;
    let mut tx = h.begin().await.unwrap();

    // a clean scope finishes
    {
        let mut scope = tx.hook_scope(HOOK);
        write_probe(&mut scope, "sticky").await;
        assert_eq!(probe_count(&mut scope, "sticky").await, 1);
        scope.finish().unwrap();
    }

    // a hook that catches its refusals still fails the scope, with the first one
    {
        let mut scope = tx.hook_scope(HOOK);
        let caught = scope.commit().await.is_err();
        assert!(caught);
        let caught = scope.execute("ROLLBACK", &[]).await.is_err();
        assert!(caught);
        // ordinary work goes on after a caught refusal
        assert_eq!(probe_count(&mut scope, "sticky").await, 1);
        let error = scope.finish().unwrap_err();
        assert_programming(&error, &api_message("Transaction::commit", HOOK));
    }

    // nested: the inner scope names its own hook, and its refusal reaches the outer finish
    {
        let mut outer = tx.hook_scope("test.outer");
        {
            let mut inner = outer.hook_scope("test.inner");
            let error = inner.rollback().await.unwrap_err();
            assert_programming(&error, &api_message("Transaction::rollback", "test.inner"));
            // the inner owner never calls finish
        }
        // the outer hook is in force again
        let error = outer.query("END", &[]).await.unwrap_err();
        assert_programming(
            &error,
            &sql_message("Transaction::query", "END", "test.outer"),
        );
        let error = outer.finish().unwrap_err();
        assert_programming(&error, &api_message("Transaction::rollback", "test.inner"));
    }

    // nested and clean on both levels
    {
        let mut outer = tx.hook_scope("test.outer");
        {
            let mut inner = outer.hook_scope("test.inner");
            assert_eq!(probe_count(&mut inner, "sticky").await, 1);
            inner.finish().unwrap();
        }
        outer.finish().unwrap();
    }

    // an inner refusal reported by the inner finish still reaches the outer one
    {
        let mut outer = tx.hook_scope("test.outer");
        {
            let mut inner = outer.hook_scope("test.inner");
            let error = inner.batch("SELECT 1").await.unwrap_err();
            assert_programming(&error, &batch_message("test.inner"));
            let error = inner.finish().unwrap_err();
            assert_programming(&error, &batch_message("test.inner"));
        }
        let error = outer.finish().unwrap_err();
        assert_programming(&error, &batch_message("test.inner"));
    }

    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_transaction_swapped_into_the_scope_is_poisoned() {
    let h = world().await;
    let mut login = h.begin().await.unwrap();
    let mut own = h.begin().await.unwrap();
    {
        let mut scope = login.hook_scope(HOOK);
        std::mem::swap(&mut *scope, &mut own);
        // the scope now lends the swapped-in transaction: its finish fails
        let error = scope.finish().unwrap_err();
        assert_programming(&error, &replaced_finish_message(HOOK));
    }
    // `login` holds the swapped-in transaction: poisoned when the scope dropped
    assert!(login.is_poisoned());
    let error = login.commit().await.unwrap_err();
    assert_programming(&error, &replaced_poison_message(HOOK));
    // `own` holds the guarded transaction, whose guard was left in place
    let error = own.commit().await.unwrap_err();
    assert_programming(&error, &api_message("Transaction::commit", HOOK));
    drop((login, own));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_forgotten_nested_scope_fails_the_outer_finish() {
    let h = world().await;
    let mut tx = h.begin().await.unwrap();
    {
        let mut outer = tx.hook_scope("test.outer");
        let mut inner = outer.hook_scope("test.inner");
        let error = inner.rollback().await.unwrap_err();
        assert_programming(&error, &api_message("Transaction::rollback", "test.inner"));
        std::mem::forget(inner);
        let error = outer.finish().unwrap_err();
        assert_programming(&error, &replaced_finish_message("test.outer"));
    }
    assert!(tx.is_poisoned());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_refusal_dropped_without_finish_poisons_the_transaction() {
    let h = world().await;
    let mut tx = h.begin().await.unwrap();
    {
        let mut scope = tx.hook_scope(HOOK);
        // the hook catches the refusal and forgets to call finish
        let _ = scope.commit().await.unwrap_err();
    }
    assert!(tx.is_poisoned());
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert!(
        error.message().contains("dropped without finish"),
        "{}",
        error.message()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn manage_operation_is_refused_inside_a_hook_scope() {
    let h = world().await;
    let mut tx = h.begin().await.unwrap();
    let key = OperationKey::for_tests();
    {
        let mut scope = tx.hook_scope(HOOK);
        let error = scope.manage_operation(&key).unwrap_err();
        assert_programming(&error, &api_message("Transaction::manage_operation", HOOK));
        assert!(!scope.is_operation_managed());
        let error = scope.finish().unwrap_err();
        assert_programming(&error, &api_message("Transaction::manage_operation", HOOK));
    }
    // outside the scope the module system can still take the transaction
    let control = tx.manage_operation(&key).unwrap();
    assert!(tx.is_operation_managed());
    tx.operation_rollback(&control).await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
