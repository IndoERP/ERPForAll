// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicit whole-unit retry options.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use erp_sdk::{
    BeginOptions, Condition, Context, Error, ErrorKind, OperationIdentity, RetryPolicy, RunOptions,
    SUPERUSER_ID,
};

fn options(attempts: u32) -> RunOptions {
    RunOptions::new(
        BeginOptions::new(SUPERUSER_ID),
        RetryPolicy::new(attempts, Duration::from_millis(1), Duration::from_millis(2)).unwrap(),
    )
    .retry_lock_timeouts()
    .lock_timeout(Duration::from_millis(20))
}

#[tokio::test]
async fn classified_timeout_retries_whole_work_and_keeps_identity() {
    let h = support::orm().await;
    let identity = OperationIdentity::generate().unwrap();
    let opts = RunOptions::new(
        BeginOptions::new(SUPERUSER_ID).operation_identity(identity.clone()),
        RetryPolicy::new(3, Duration::from_millis(1), Duration::from_millis(2)).unwrap(),
    )
    .retry_lock_timeouts();
    let attempts = AtomicUsize::new(0);
    let hooks = Arc::new(AtomicUsize::new(0));
    h.orm()
        .run_with_options(opts, async |tx| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            assert_eq!(tx.env().operation_identity(), Some(&identity));
            // A failed attempt's write and hook must disappear together.
            tx.execute("INSERT INTO test_orm_partner(name) VALUES ('attempt')", &[])
                .await?;
            let hooks = Arc::clone(&hooks);
            tx.after_commit(move || {
                hooks.fetch_add(1, Ordering::SeqCst);
            });
            if attempt < 2 {
                return Err(Error::new(ErrorKind::Lock, "classified timeout")
                    .with_sqlstate(Some("55P03".into()))
                    .with_condition(Condition::LockTimeout));
            }
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(hooks.load(Ordering::SeqCst), 1);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM test_orm_partner WHERE name='attempt'",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn bare_55p03_and_noncontention_errors_are_terminal() {
    let h = support::orm().await;
    for (kind, state, condition) in [
        (ErrorKind::Lock, Some("55P03"), None),
        (
            ErrorKind::Database,
            Some("57014"),
            Some(Condition::StatementTimeout),
        ),
        (ErrorKind::Validation, None, None),
        (ErrorKind::Programming, None, None),
        (ErrorKind::Access, None, None),
    ] {
        let calls = AtomicUsize::new(0);
        let failure = h
            .orm()
            .run_with_options(options(3), async |_tx| {
                calls.fetch_add(1, Ordering::SeqCst);
                let mut error =
                    Error::new(kind, "terminal fixture").with_sqlstate(state.map(str::to_owned));
                if let Some(condition) = condition {
                    error = error.with_condition(condition);
                }
                Err::<(), _>(error)
            })
            .await
            .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(failure.attempts(), 1);
        assert!(!failure.exhausted());
        assert_eq!(failure.error().kind(), kind);
        assert_eq!(failure.error().message(), "terminal fixture");
        assert_eq!(failure.error().sqlstate(), state);
    }
    let calls = AtomicUsize::new(0);
    let error = h
        .orm()
        .run(SUPERUSER_ID, Context::new(), async |_tx| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(
                Error::new(ErrorKind::Lock, "default timeout")
                    .with_sqlstate(Some("55P03".into()))
                    .with_condition(Condition::LockTimeout),
            )
        })
        .await
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(error.kind(), ErrorKind::Lock);
    assert_eq!(error.message(), "default timeout");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn serialization_and_deadlock_exhaustion_preserve_cause_and_rollback() {
    let h = support::orm().await;
    for state in ["40001", "40P01"] {
        let calls = AtomicUsize::new(0);
        let failure = h
            .orm()
            .run_with_options(options(3), async |tx| {
                calls.fetch_add(1, Ordering::SeqCst);
                tx.execute(
                    "INSERT INTO test_orm_partner(name) VALUES ('rolled-back')",
                    &[],
                )
                .await?;
                Err::<(), _>(
                    Error::new(ErrorKind::Concurrency, "contention fixture")
                        .with_sqlstate(Some(state.into())),
                )
            })
            .await
            .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(failure.attempts(), 3);
        assert!(failure.exhausted());
        let error = failure.into_error();
        assert_eq!(error.kind(), ErrorKind::Concurrency);
        assert_eq!(error.message(), "contention fixture");
        assert_eq!(error.sqlstate(), Some(state));
    }
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM test_orm_partner WHERE name='rolled-back'",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn real_lock_timeout_retries_but_nowait_does_not() {
    let h = support::orm().await;
    let mut holder = h.begin().await.unwrap();
    holder
        .execute("SELECT pg_advisory_xact_lock(741239)", &[])
        .await
        .unwrap();
    let attempts = AtomicUsize::new(0);
    let failure = h
        .orm()
        .run_with_options(options(2), async |tx| {
            attempts.fetch_add(1, Ordering::SeqCst);
            tx.query("SELECT pg_advisory_xact_lock(741239)", &[])
                .await?;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(failure.attempts(), 2);
    assert!(failure.exhausted());
    assert_eq!(failure.error().kind(), ErrorKind::Lock);
    assert_eq!(failure.error().sqlstate(), Some("55P03"));
    assert_eq!(failure.error().condition(), Some(Condition::LockTimeout));
    assert_eq!(
        failure.error().message(),
        "canceling statement due to lock timeout (lock_timeout was 20 ms when the transaction began)"
    );
    holder.rollback().await.unwrap();
    drop(holder);

    let mut seed = h.begin().await.unwrap();
    seed.execute(
        "INSERT INTO test_orm_partner(id,name) VALUES (741239,'locked')",
        &[],
    )
    .await
    .unwrap();
    seed.commit().await.unwrap();
    drop(seed);
    let mut holder = h.begin().await.unwrap();
    holder
        .query(
            "SELECT id FROM test_orm_partner WHERE id=741239 FOR UPDATE",
            &[],
        )
        .await
        .unwrap();
    let attempts = AtomicUsize::new(0);
    let failure = h
        .orm()
        .run_with_options(options(2), async |tx| {
            attempts.fetch_add(1, Ordering::SeqCst);
            tx.query(
                "SELECT id FROM test_orm_partner WHERE id=741239 FOR UPDATE NOWAIT",
                &[],
            )
            .await?;
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(!failure.exhausted());
    assert_eq!(failure.error().sqlstate(), Some("55P03"));
    assert_ne!(failure.error().condition(), Some(Condition::LockTimeout));
    assert_eq!(failure.error().kind(), ErrorKind::Lock);
    assert_eq!(
        failure.error().message(),
        "could not obtain lock on row in relation \"test_orm_partner\""
    );
    holder.rollback().await.unwrap();
    drop(holder);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn ambiguous_commit_is_terminal() {
    let h = support::orm().await;
    let calls = AtomicUsize::new(0);
    let failure = h
        .orm()
        .run_with_options(options(3), async |tx| {
            calls.fetch_add(1, Ordering::SeqCst);
            tx.execute(
                "INSERT INTO test_orm_partner(name) VALUES ('committed')",
                &[],
            )
            .await?;
            tx.inject_commit_fault(erp_orm::CommitFault::AnswerLostAfterCommit);
            Ok(())
        })
        .await
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(failure.attempts(), 1);
    assert!(!failure.exhausted());
    assert_eq!(failure.error().kind(), ErrorKind::Database);
    assert_eq!(
        failure.error().message(),
        "database connection closed: injected fault after COMMIT"
    );
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM test_orm_partner WHERE name='committed'",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn incompatible_lock_limit_refuses_before_work() {
    let h = support::orm().await;
    let calls = AtomicUsize::new(0);
    let failure = h
        .orm()
        .run_with_options(
            options(1).lock_timeout(Duration::from_secs(300)),
            async |_tx| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(failure.attempts(), 0);
    assert!(!failure.exhausted());
    assert_eq!(failure.error().kind(), ErrorKind::Programming);
    assert_eq!(
        failure.error().message(),
        "lock_timeout (300000 ms) must be below statement_timeout (300000 ms): the statement limit would always fire first and the lock limit could never apply"
    );
    h.cleanup().await.unwrap();
}

#[test]
fn retry_policy_rejects_invalid_budgets() {
    for (attempts, base, cap) in [(0, 1, 2), (6, 1, 2), (1, 0, 2), (1, 3, 2), (1, 1, 10001)] {
        let error = RetryPolicy::new(
            attempts,
            Duration::from_millis(base),
            Duration::from_millis(cap),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "retry policy requires 1 to 5 attempts and positive whole-millisecond backoffs up to 10000 ms, with base <= cap."
        );
    }
}

#[tokio::test]
async fn cancelled_body_does_not_commit_or_run_hooks() {
    let h = support::orm().await;
    let hooks = Arc::new(AtomicUsize::new(0));
    let (ready, observed) = tokio::sync::oneshot::channel();
    let mut ready = Some(ready);
    {
        let work = h.orm().run_with_options(options(2), async |tx| {
            tx.execute(
                "INSERT INTO test_orm_partner(name) VALUES ('cancelled')",
                &[],
            )
            .await?;
            let hooks = Arc::clone(&hooks);
            tx.after_commit(move || {
                hooks.fetch_add(1, Ordering::SeqCst);
            });
            ready.take().unwrap().send(()).unwrap();
            std::future::pending::<erp_sdk::Result<()>>().await
        });
        tokio::pin!(work);
        tokio::select! { _ = observed => {}, result = &mut work => panic!("unexpected completion: {result:?}") }
    }
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM test_orm_partner WHERE name='cancelled'",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
    assert_eq!(hooks.load(Ordering::SeqCst), 0);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rollback_failure_preserves_primary_error_and_closes_connection() {
    let h = support::orm().await;
    let failure = h
        .orm()
        .run_with_options(options(1), async |tx| {
            tx.execute(
                "INSERT INTO test_orm_partner(name) VALUES ('lost connection')",
                &[],
            )
            .await?;
            let pid: i32 = tx.query("SELECT pg_backend_pid()", &[]).await?[0].get(0);
            let mut terminator = h.orm().begin(SUPERUSER_ID).await?;
            assert!(
                terminator
                    .query("SELECT pg_terminate_backend($1)", &[&pid])
                    .await?[0]
                    .get::<_, bool>(0)
            );
            terminator.commit().await?;
            Err::<(), _>(
                Error::new(ErrorKind::Concurrency, "rollback fixture")
                    .with_sqlstate(Some("40001".into())),
            )
        })
        .await
        .unwrap_err();
    assert_eq!(failure.attempts(), 1);
    assert!(failure.exhausted());
    assert_eq!(failure.error().kind(), ErrorKind::Concurrency);
    assert_eq!(failure.error().sqlstate(), Some("40001"));
    // The nested driver diagnostic depends on whether the fatal packet or socket close wins.
    assert!(
        failure
            .error()
            .message()
            .starts_with("rollback fixture (and "),
        "{failure}"
    );
    assert!(failure.error().message().contains("ROLLBACK"), "{failure}");
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM test_orm_partner WHERE name='lost connection'",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn top_level_runner_refuses_an_outer_unit_before_work() {
    let h = support::orm().await;
    let calls = AtomicUsize::new(0);
    let failure = erp_orm::unit_of_work(async {
        let mut outer = h.begin().await.unwrap();
        outer.query("SELECT 1", &[]).await.unwrap();
        let failure = h
            .orm()
            .run_with_options(options(2).top_level("stock.run_quant_unit"), async |_tx| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap_err();
        outer.rollback().await.unwrap();
        failure
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(failure.attempts(), 0);
    assert_eq!(failure.error().kind(), ErrorKind::Programming);
    assert_eq!(
        failure.error().message(),
        "stock.run_quant_unit cannot run inside another unit of work (including an RPC transaction)."
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn serialization_at_commit_restarts_and_hooks_run_once() {
    let h = support::orm().await;
    let mut setup = h.begin().await.unwrap();
    setup.batch("CREATE SEQUENCE unit_commit_attempt; CREATE FUNCTION unit_commit_fail_once() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF nextval('unit_commit_attempt') = 1 THEN RAISE EXCEPTION 'commit contention fixture' USING ERRCODE='40001'; END IF; RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER unit_commit_failure AFTER INSERT ON test_orm_partner DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION unit_commit_fail_once()")
        .await.unwrap();
    setup.commit().await.unwrap();
    drop(setup);
    let calls = AtomicUsize::new(0);
    let hooks = Arc::new(AtomicUsize::new(0));
    h.orm()
        .run_with_options(options(2), async |tx| {
            calls.fetch_add(1, Ordering::SeqCst);
            tx.execute(
                "INSERT INTO test_orm_partner(name) VALUES ('commit retry')",
                &[],
            )
            .await?;
            let hooks = Arc::clone(&hooks);
            tx.after_commit(move || {
                hooks.fetch_add(1, Ordering::SeqCst);
            });
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(hooks.load(Ordering::SeqCst), 1);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.query(
            "SELECT count(*) FROM test_orm_partner WHERE name='commit retry'",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, i64>(0),
        1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn retry_works_with_a_single_connection_pool() {
    let h = support::orm().await;
    let pool = erp_orm::pool::create_pool(h.db().config(), 1).unwrap();
    let orm = erp_sdk::Orm::builder(Arc::clone(h.registry()), pool.clone())
        .policy(erp_sdk::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    // Four backoffs of up to 1.5 s each. The runner draws every sleep from 0 to the bound, so one
    // sleep may be too short to notice; a slot kept through all four sleeps would make at least
    // one probe below wait past its 250 ms limit, except with a probability below 0.1 %.
    let backoff = Duration::from_millis(1500);
    let opts = RunOptions::new(
        BeginOptions::new(SUPERUSER_ID),
        RetryPolicy::new(5, backoff, backoff).unwrap(),
    );
    let calls = AtomicUsize::new(0);
    let (failed, mut attempts) = tokio::sync::mpsc::unbounded_channel();
    let run = orm.run_with_options(opts, async |tx| {
        tx.query("SELECT 1", &[]).await?;
        let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt < 5 {
            failed.send(attempt).unwrap();
            return Err(Error::new(ErrorKind::Concurrency, "one-slot retry")
                .with_sqlstate(Some("40001".into())));
        }
        Ok(())
    });
    // While the runner sleeps after a failed attempt, the pool's only connection is free.
    let probe = async {
        for expected in 1..5 {
            assert_eq!(attempts.recv().await, Some(expected));
            let connection = tokio::time::timeout(Duration::from_millis(250), pool.get())
                .await
                .unwrap_or_else(|_| panic!("backoff {expected} kept the only connection"))
                .unwrap();
            drop(connection);
        }
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(30), async { tokio::join!(run, probe) })
            .await
            .expect("retry releases the only connection");
    result.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    drop(orm);
    h.cleanup().await.unwrap();
}
