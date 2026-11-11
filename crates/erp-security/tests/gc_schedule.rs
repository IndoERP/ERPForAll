//! The scheduled collector against the actual base authentication models.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "regression assertions"
)]
#[cfg(feature = "test-support")]
use erp_fields::Error;
use erp_fields::ErrorKind;
use erp_security::auth::gc_schedule::{GcRun, GcSchedule};
use erp_security::auth::{Auth, AuthConfig, HashPool};
use erp_testkit::Harness;
use std::time::Duration;

async fn fixture() -> (Harness, deadpool_postgres::Pool) {
    fixture_with_config(erp_orm::OrmConfig::default()).await
}

async fn fixture_with_config(config: erp_orm::OrmConfig) -> (Harness, deadpool_postgres::Pool) {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .require_core_tables(erp_security::auth::core_tables())
        .config(config)
        .build()
        .await
        .unwrap();
    let pool = erp_orm::pool::create_pool(h.db().config(), 1).unwrap();
    (h, pool)
}

#[tokio::test]
async fn scheduled_run_uses_the_real_auth_collector() {
    let (h, pool) = fixture().await;
    let mut tx = h.begin().await.unwrap();
    erp_orm::model_hooks::call_init(&mut tx, "ir.config_parameter")
        .await
        .unwrap();
    erp_orm::model_hooks::call_init(&mut tx, "res.users")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let auth = Auth::new(h.orm().clone(), AuthConfig::DEFAULT, HashPool::global())
        .await
        .unwrap();
    let c = h.db().client().await.unwrap();
    c.execute("INSERT INTO erp_login_throttle(kind,key_hash,failures,last_failure_at) VALUES ('address',sha256('scheduler-test'::bytea),1,clock_timestamp()-interval '30 days')", &[]).await.unwrap();
    let result = GcSchedule::new(pool).run(&auth).await.unwrap();
    match result {
        GcRun::Collected(report) => assert_eq!(report.throttle_rows, 1),
        other => panic!("expected actual collection, got {other:?}"),
    }
    let count: i64 = c
        .query_one("SELECT count(*) FROM erp_login_throttle", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn stale_or_expired_lease_cannot_enter_a_destructive_batch() {
    let (h, _pool) = fixture().await;
    let c = h.db().client().await.unwrap();
    c.batch_execute("UPDATE erp_auth_gc_state SET lease_token=7, lease_until=clock_timestamp()-interval '1 second'").await.unwrap();
    let mut tx = h.begin().await.unwrap();
    error(
        erp_security::auth::test_support::gc_fence(&mut tx, 7)
            .await
            .unwrap_err(),
        ErrorKind::Concurrency,
        "authentication collection lease expired or was superseded",
    );
    tx.rollback().await.unwrap();
    c.batch_execute("UPDATE erp_auth_gc_state SET lease_token=8, lease_until=clock_timestamp()+interval '1 minute'").await.unwrap();
    let mut tx = h.begin().await.unwrap();
    error(
        erp_security::auth::test_support::gc_fence(&mut tx, 7)
            .await
            .unwrap_err(),
        ErrorKind::Concurrency,
        "authentication collection lease expired or was superseded",
    );
    tx.rollback().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    erp_security::auth::test_support::gc_fence(&mut tx, 8)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}
#[cfg(feature = "test-support")]
fn error(e: Error, kind: ErrorKind, message: &str) {
    assert_eq!(e.kind(), kind);
    assert_eq!(e.message(), message);
}

#[tokio::test]
async fn real_batch_timeout_preserves_prior_sweep_progress_under_a_held_fence() {
    // Keep this fixture's PostgreSQL wait longer than the scheduler's real 30-second
    // deadline, so cancellation leaves a genuine backend holding the fenced transaction.
    let (h, pool) = fixture_with_config(erp_orm::OrmConfig {
        timeouts: erp_orm::Timeouts::new(
            Duration::from_secs(45),
            Duration::from_secs(90),
            Duration::from_secs(120),
        )
        .unwrap(),
        ..erp_orm::OrmConfig::default()
    })
    .await;
    let mut setup = h.begin().await.unwrap();
    erp_orm::model_hooks::call_init(&mut setup, "ir.config_parameter")
        .await
        .unwrap();
    erp_orm::model_hooks::call_init(&mut setup, "res.users")
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let auth = Auth::new(h.orm().clone(), AuthConfig::DEFAULT, HashPool::global())
        .await
        .unwrap();
    let schedule = GcSchedule::new(pool);
    let c = h.db().client().await.unwrap();
    c.batch_execute("UPDATE erp_auth_gc_state SET page=91; INSERT INTO erp_login_throttle(kind,key_hash,failures,last_failure_at) VALUES ('address',sha256('blocked-real-batch'::bytea),1,clock_timestamp()-interval '30 days'); CREATE FUNCTION stall_gc_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(981431); RETURN OLD; END; $$; CREATE TRIGGER stall_gc_delete BEFORE DELETE ON erp_login_throttle FOR EACH ROW EXECUTE FUNCTION stall_gc_delete()")
        .await.unwrap();
    let blocker = h.db().client().await.unwrap();
    blocker
        .query_one("SELECT pg_advisory_lock(981431)", &[])
        .await
        .unwrap();
    let running = schedule.run(&auth);
    let observed = async {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let waiting: bool = c.query_one("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory' AND pid<>pg_backend_pid())", &[]).await.unwrap().get(0);
                if waiting { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
    };
    let (result, ()) = tokio::join!(running, observed);
    let failure = result.unwrap_err();
    assert_eq!(failure.kind(), ErrorKind::ResourceExhausted);
    // Finish also fails while the cancelled backend retains the fence. Its secondary
    // cause can be the PostgreSQL lock/statement deadline or the client control deadline,
    // depending on host scheduling; the original typed deadline must remain first.
    assert!(
        failure
            .message()
            .starts_with("authentication collection run exceeded its deadline (and ")
    );
    // Read without acquiring the stalled fence: only work committed before the timed-out
    // throttle DELETE may have advanced this cursor. The empty heap was fully swept.
    let page: i64 = c
        .query_one("SELECT page FROM erp_auth_gc_state WHERE singleton", &[])
        .await
        .unwrap()
        .get(0);
    let unlocked: bool = blocker
        .query_one("SELECT pg_advisory_unlock(981431)", &[])
        .await
        .unwrap()
        .get(0);
    assert!(unlocked, "fixture must release its owned advisory lock");
    // Await rollback of the cancelled real transaction, rather than relying on a sleep.
    c.batch_execute("SET lock_timeout='5s'; BEGIN; SELECT singleton FROM erp_auth_gc_state WHERE singleton FOR UPDATE; ROLLBACK; DROP TRIGGER stall_gc_delete ON erp_login_throttle; DROP FUNCTION stall_gc_delete()")
        .await.unwrap();
    let remaining: i64 = c
        .query_one("SELECT count(*) FROM erp_login_throttle", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        page, 0,
        "completed idle sweep must remain durable when finish cannot obtain the cancelled backend's fence"
    );
    assert_eq!(
        remaining, 1,
        "the blocked throttle delete must not have committed"
    );
}

#[tokio::test]
async fn lock_stopped_collection_remains_due_for_another_attempt() {
    let (h, pool) = fixture().await;
    let mut setup = h.begin().await.unwrap();
    erp_orm::model_hooks::call_init(&mut setup, "ir.config_parameter")
        .await
        .unwrap();
    erp_orm::model_hooks::call_init(&mut setup, "res.users")
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let auth = Auth::new(h.orm().clone(), AuthConfig::DEFAULT, HashPool::global())
        .await
        .unwrap();
    let blocker = h.db().client().await.unwrap();
    let lock = erp_security::auth::gc::GC_LOCK;
    blocker
        .query_one("SELECT pg_advisory_lock($1, $2)", &[&lock.0, &lock.1])
        .await
        .unwrap();
    let result = GcSchedule::new(pool).run(&auth).await;
    let unlocked: bool = blocker
        .query_one("SELECT pg_advisory_unlock($1, $2)", &[&lock.0, &lock.1])
        .await
        .unwrap()
        .get(0);
    assert!(unlocked, "fixture must release its owned collector lock");
    match result.unwrap() {
        GcRun::Collected(report) => assert_eq!(
            report.stopped_by_lock,
            Some(erp_security::auth::gc::GcPhase::Expired)
        ),
        other => panic!("expected lock-stopped collection, got {other:?}"),
    }
    let due: bool = blocker
        .query_one(
            "SELECT next_run_at <= clock_timestamp() FROM erp_auth_gc_state WHERE singleton",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        due,
        "an advisory-lock-stopped run is not a completed interval"
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn checkpoint_is_atomic_with_batch_effects_and_refuses_stale_or_expired_epochs() {
    let (h, _pool) = fixture().await;
    let c = h.db().client().await.unwrap();
    c.batch_execute("CREATE TABLE gc_checkpoint_effect(id bigint PRIMARY KEY); UPDATE erp_auth_gc_state SET lease_token=7, lease_until=clock_timestamp()+interval '30 seconds'").await.unwrap();
    let assert_saved = async |page: i64, effects: i64| {
        let row = c.query_one("SELECT page, (SELECT count(*) FROM gc_checkpoint_effect) FROM erp_auth_gc_state WHERE singleton", &[]).await.unwrap();
        assert_eq!(row.get::<_, i64>(0), page);
        assert_eq!(row.get::<_, i64>(1), effects);
    };
    let mut tx = h.begin().await.unwrap();
    erp_security::auth::test_support::gc_fence(&mut tx, 7)
        .await
        .unwrap();
    tx.execute("INSERT INTO gc_checkpoint_effect(id) VALUES (1)", &[])
        .await
        .unwrap();
    erp_security::auth::test_support::gc_checkpoint(
        &mut tx,
        7,
        erp_security::auth::gc::GcCursor::at(123),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_saved(0, 0).await;

    let mut tx = h.begin().await.unwrap();
    erp_security::auth::test_support::gc_fence(&mut tx, 7)
        .await
        .unwrap();
    tx.execute("INSERT INTO gc_checkpoint_effect(id) VALUES (1)", &[])
        .await
        .unwrap();
    erp_security::auth::test_support::gc_checkpoint(
        &mut tx,
        7,
        erp_security::auth::gc::GcCursor::at(124),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_saved(124, 1).await;

    c.batch_execute("UPDATE erp_auth_gc_state SET lease_token=8")
        .await
        .unwrap();
    for token in [7, 8] {
        if token == 8 {
            c.batch_execute("UPDATE erp_auth_gc_state SET lease_until='-infinity'")
                .await
                .unwrap();
        }
        let mut tx = h.begin().await.unwrap();
        tx.execute("INSERT INTO gc_checkpoint_effect(id) VALUES (2)", &[])
            .await
            .unwrap();
        error(
            erp_security::auth::test_support::gc_checkpoint(
                &mut tx,
                token,
                erp_security::auth::gc::GcCursor::at(125),
            )
            .await
            .unwrap_err(),
            ErrorKind::Concurrency,
            "authentication collection lease expired or was superseded",
        );
        tx.rollback().await.unwrap();
        assert_saved(124, 1).await;
    }
}
