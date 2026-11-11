//! Transactions managed by a module operation (`Transaction::manage_operation`, review A1),
//! `Orm::rebuild` keeping its policy, and a failed `ROLLBACK` reported with the error it follows
//! (review A12).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::sync::Arc;
use std::time::Duration;

use erp_orm::CommitError;
use erp_registry::RegistryBuilder;
use erp_sdk::{Context, Error, ErrorKind, Orm, SUPERUSER_ID, StaticPolicy};
use erp_test_models::base_users;
use erp_testkit::TestDb;
use support::one;

fn registry() -> Arc<erp_registry::Registry> {
    Arc::new(RegistryBuilder::new([&base_users::ADDON]).build().unwrap())
}

fn orm_on(db: &TestDb) -> Orm {
    Orm::builder(registry(), db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

/// A hook cannot end the operation's transaction: every call that ends it or moves it to another
/// ORM is refused with a `Programming` error naming the call, and the transaction stays open and
/// usable. Only the holder of the control commits (the single `manage_operation`).
#[tokio::test]
async fn a_managed_transaction_refuses_commit_rollback_and_switch() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let other = orm.rebuild(registry()).build().unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("CREATE TABLE managed_probe (v int)")
        .await
        .unwrap();
    let control = tx
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();

    let message = |call: &str| {
        format!(
            "{call}: this transaction belongs to a module operation, which commits or rolls it \
             back itself; a hook, data loader or model init must not end it (nothing was \
             committed or rolled back)"
        )
    };
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), message("Transaction::commit"));
    let err = tx.rollback().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), message("Transaction::rollback"));
    let err = tx.switch_orm(&other).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), message("Transaction::switch_orm"));
    match tx.commit_pipelined(None).await.unwrap_err() {
        CommitError::RolledBack(err) => {
            assert_eq!(err.kind(), ErrorKind::Programming);
            assert_eq!(err.message(), message("Transaction::commit_pipelined"));
        }
        other => panic!("expected RolledBack, got {other:?}"),
    }
    let err = tx
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Transaction::manage_operation: this transaction is managed by a module operation already"
    );

    // none of that ended it: the table is still there, and the control of another transaction
    // does not fit this one
    assert_eq!(
        one(&mut tx, "SELECT to_regclass('managed_probe')::text")
            .await
            .as_deref(),
        Some("managed_probe")
    );
    let mut stranger = orm.begin(SUPERUSER_ID).await.unwrap();
    let foreign = stranger
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    let err = tx.operation_commit(&foreign).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Transaction::operation_commit: the operation control belongs to another transaction"
    );
    stranger.operation_rollback(&foreign).await.unwrap();

    tx.operation_switch_orm(&control, &other).await.unwrap();
    tx.operation_commit(&control).await.unwrap();
    drop(tx);
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        one(&mut check, "SELECT to_regclass('managed_probe')::text")
            .await
            .as_deref(),
        Some("managed_probe"),
        "the holder's commit persisted"
    );
    check.rollback().await.unwrap();
    drop(check);
    db.cleanup().await.unwrap();
}

/// The holder's rollback undoes the work, and the refusals did not.
#[tokio::test]
async fn the_holder_can_roll_back() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("CREATE TABLE managed_rolled_back (v int)")
        .await
        .unwrap();
    let control = tx
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    assert!(tx.commit().await.is_err());
    tx.operation_rollback(&control).await.unwrap();
    drop(tx);
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        one(
            &mut check,
            "SELECT to_regclass('managed_rolled_back')::text"
        )
        .await,
        None
    );
    check.rollback().await.unwrap();
    drop(check);
    db.cleanup().await.unwrap();
}

/// A rebuilt ORM keeps the policy it was carried: asking for another one fails at `build`.
#[tokio::test]
async fn a_rebuilt_builder_refuses_another_policy() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let err = orm
        .rebuild(registry())
        .policy(StaticPolicy::DenyAll)
        .build()
        .err()
        .expect("the carried policy cannot be replaced");
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "OrmBuilder::policy on a builder made by Orm::rebuild: a rebuilt ORM keeps the access \
         policy of the ORM it came from; build a new ORM with Orm::builder to use another policy"
    );
    // without the call it builds, with the carried policy
    assert!(orm.rebuild(registry()).build().is_ok());
    db.cleanup().await.unwrap();
}

/// `ROLLBACK` failing after the work's error is not lost: the error returned keeps the work's kind
/// and message first, adds the rollback's, and the work's error stays the source.
#[tokio::test]
async fn a_failed_rollback_is_added_to_the_error_it_follows() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let admin = db.pool().get().await.unwrap();
    let err = orm
        .run(SUPERUSER_ID, Context::new(), async |tx| {
            let pid: i32 = tx
                .query("SELECT pg_backend_pid()", &[])
                .await?
                .first()
                .unwrap()
                .get(0);
            // end the session behind the transaction's back, and wait until it is gone
            admin
                .query_one("SELECT pg_terminate_backend($1)", &[&pid])
                .await
                .unwrap();
            let mut gone = false;
            for _ in 0..500 {
                let n: i64 = admin
                    .query_one(
                        "SELECT count(*) FROM pg_stat_activity WHERE pid = $1",
                        &[&pid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if n == 0 {
                    gone = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(gone, "backend {pid} still alive");
            Err::<(), _>(Error::user("the work failed"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert!(
        err.message().starts_with(
            "the work failed (and ROLLBACK failed, the connection was closed instead (which \
             rolls the transaction back on the server): "
        ),
        "{}",
        err.message()
    );
    let source = std::error::Error::source(&err).expect("the work's error is the source");
    assert_eq!(source.to_string(), "the work failed");
    drop(admin);
    db.cleanup().await.unwrap();
}

// ---- S1: a rebuilt ORM keeps the access configuration; switch_orm is for System ------------

/// A resolver that is never asked: it only has to exist to be passed to the builder.
struct NeverAsked;

impl erp_orm::PrincipalResolver for NeverAsked {
    fn statement(
        &self,
        _hint: &erp_orm::PrincipalHint,
    ) -> erp_sdk::Result<erp_orm::ResolverStatement> {
        Err(Error::programming("never asked"))
    }

    fn finish(
        &self,
        _hint: &erp_orm::PrincipalHint,
        _rows: &[erp_testkit::tokio_postgres::Row],
        _policy: &dyn erp_orm::AccessPolicy,
    ) -> erp_sdk::Result<erp_orm::Principal> {
        Err(Error::programming("never asked"))
    }

    fn re_resolve<'a>(
        &'a self,
        _tx: &'a mut erp_sdk::Transaction,
        _subjects: &'a [erp_orm::UserId],
    ) -> erp_orm::BoxFuture<'a, erp_sdk::Result<Vec<erp_orm::Resolution>>> {
        Box::pin(async { Err(Error::programming("never asked")) })
    }
}

fn hook<'a>(
    _tx: &'a mut erp_sdk::Transaction,
    _batch: &'a erp_orm::WriteBatch,
) -> erp_orm::BoxFuture<'a, erp_sdk::Result<()>> {
    Box::pin(async { Ok(()) })
}

static SOURCE: erp_orm::PolicySourceWrite =
    erp_orm::PolicySourceWrite::new("res.users", erp_orm::SourceKind::Membership);

/// Each override of the access configuration on a rebuilt builder is refused at `build` with a
/// `Programming` error naming the call (S1).
#[tokio::test]
async fn a_rebuilt_builder_refuses_every_access_override() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let expect = |call: &str, err: Error| {
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "{call} on a builder made by Orm::rebuild: a rebuilt ORM keeps the access \
                 configuration (policy, resolver, policy sources, write hooks, signal) of the \
                 ORM it came from; build a new ORM with Orm::builder to use another"
            )
        );
    };
    expect(
        "OrmBuilder::resolver",
        orm.rebuild(registry())
            .resolver(NeverAsked)
            .build()
            .err()
            .expect("refused"),
    );
    expect(
        "OrmBuilder::trust_caller_resolutions",
        orm.rebuild(registry())
            .trust_caller_resolutions()
            .build()
            .err()
            .expect("refused"),
    );
    expect(
        "OrmBuilder::policy_sources",
        orm.rebuild(registry())
            .policy_sources([&SOURCE])
            .build()
            .err()
            .expect("refused"),
    );
    expect(
        "OrmBuilder::write_hook",
        orm.rebuild(registry())
            .write_hook(erp_orm::WriteHook::new("res.users", &["name"], hook))
            .build()
            .err()
            .expect("refused"),
    );
    expect(
        "OrmBuilder::signal",
        orm.rebuild(registry())
            .signal(erp_orm::Signal::new(erp_orm::Generations::default()))
            .build()
            .err()
            .expect("refused"),
    );
    // the same calls on a fresh builder are not refused for being overrides
    assert!(
        Orm::builder(registry(), db.pool().clone())
            .policy(StaticPolicy::AllowAll)
            .policy_sources([&SOURCE])
            .build()
            .is_ok()
    );
    db.cleanup().await.unwrap();
}

/// A transaction of a user principal cannot move onto another registry: `switch_orm` and
/// `operation_switch_orm` refuse it with `Access` and nothing is switched (S1).
#[tokio::test]
async fn switch_orm_refuses_a_user_transaction() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let stage = orm.rebuild(registry()).build().unwrap();
    let mut tx = orm.begin(erp_orm::UserId::new(2).unwrap()).await.unwrap();
    let err = tx.switch_orm(&stage).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "Transaction::switch_orm: only a transaction of the System principal with full \
         elevation may do this (this one acts as user 2)"
    );
    assert!(
        Arc::ptr_eq(tx.registry(), orm.registry()),
        "nothing switched"
    );
    let control = tx
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    let err = tx.operation_switch_orm(&control, &stage).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert!(
        Arc::ptr_eq(tx.registry(), orm.registry()),
        "nothing switched"
    );
    tx.operation_rollback(&control).await.unwrap();
    drop(tx);
    // the System principal still can
    let mut system = orm.begin(SUPERUSER_ID).await.unwrap();
    system.switch_orm(&stage).await.unwrap();
    system.rollback().await.unwrap();
    drop(system);
    db.cleanup().await.unwrap();
}

/// The key is claimed once per process (review D2): the second claim names why it fails.
#[test]
fn the_operation_key_can_be_claimed_once() {
    let _key = erp_orm::OperationKey::claim().unwrap();
    let err = erp_orm::OperationKey::claim().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "OperationKey::claim: the operation key was claimed already; the module system takes it \
         once per process, before any other code"
    );
}

#[tokio::test]
async fn managed_classified_commit_preserves_unknown_and_does_not_run_hooks() {
    for (fault, persisted) in [
        (erp_orm::CommitFault::ConnectionClosedBeforeCommit, false),
        (erp_orm::CommitFault::AnswerLostAfterCommit, true),
    ] {
        let db = TestDb::create().await.unwrap();
        let orm = orm_on(&db);
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        tx.batch("CREATE TABLE classified_probe (id bigint)")
            .await
            .unwrap();
        let control = tx
            .manage_operation(&erp_orm::OperationKey::for_tests())
            .unwrap();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let capture = Arc::clone(&ran);
        tx.after_commit(move || capture.store(true, std::sync::atomic::Ordering::SeqCst));
        tx.inject_commit_fault(fault);
        let error = tx.operation_commit_classified(&control).await.unwrap_err();
        assert!(matches!(error, CommitError::Unknown(_)), "{error:?}");
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        drop(tx);
        let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
        assert_eq!(
            one(&mut check, "SELECT to_regclass('classified_probe')::text")
                .await
                .is_some(),
            persisted
        );
        check.rollback().await.unwrap();
        drop(check);
        db.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn managed_classified_commit_refuses_foreign_control_and_commits_with_holder() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("CREATE TABLE classified_holder (id bigint)")
        .await
        .unwrap();
    let control = tx
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    let mut stranger = orm.begin(SUPERUSER_ID).await.unwrap();
    let foreign = stranger
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    let error = tx.operation_commit_classified(&foreign).await.unwrap_err();
    match error {
        CommitError::RolledBack(error) => {
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "Transaction::operation_commit_classified: the operation control belongs to another transaction"
            );
        }
        other => panic!("expected foreign-control refusal, got {other:?}"),
    }
    stranger.operation_rollback(&foreign).await.unwrap();
    tx.operation_commit_classified(&control).await.unwrap();
    drop(tx);
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    assert!(
        one(&mut check, "SELECT to_regclass('classified_holder')::text")
            .await
            .is_some()
    );
    check.rollback().await.unwrap();
    drop(check);
    drop(stranger);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn managed_classified_commit_preserves_server_confirmed_rollback() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("CREATE TABLE classified_rejected (id bigint); CREATE FUNCTION reject_classified_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'managed commit refused'; END; $$; CREATE CONSTRAINT TRIGGER reject_classified_commit AFTER INSERT ON classified_rejected DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION reject_classified_commit(); INSERT INTO classified_rejected VALUES(1)").await.unwrap();
    let control = tx
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let capture = Arc::clone(&ran);
    tx.after_commit(move || capture.store(true, std::sync::atomic::Ordering::SeqCst));
    match tx.operation_commit_classified(&control).await.unwrap_err() {
        CommitError::RolledBack(error) => {
            assert_eq!(error.kind(), ErrorKind::Database);
            assert_eq!(error.message(), "managed commit refused");
            assert_eq!(error.sqlstate(), Some("P0001"));
        }
        other => panic!("expected confirmed rollback, got {other:?}"),
    }
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    drop(tx);
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        one(
            &mut check,
            "SELECT to_regclass('classified_rejected')::text"
        )
        .await,
        None
    );
    check.rollback().await.unwrap();
    drop(check);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn managed_classified_commit_preserves_aborted_transaction_rollback() {
    let db = TestDb::create().await.unwrap();
    let orm = orm_on(&db);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("CREATE TABLE classified_aborted (id bigint)")
        .await
        .unwrap();
    let control = tx
        .manage_operation(&erp_orm::OperationKey::for_tests())
        .unwrap();
    let original = tx.query("SELECT 1 / 0", &[]).await.unwrap_err();
    assert_eq!(original.kind(), ErrorKind::Database);
    assert_eq!(original.message(), "division by zero");
    match tx.operation_commit_classified(&control).await.unwrap_err() {
        CommitError::RolledBack(error) => {
            assert_eq!(error.kind(), ErrorKind::Database);
            assert_eq!(error.sqlstate(), Some("22012"));
            assert_eq!(
                error.message(),
                "transaction rolled back: an earlier statement failed (division by zero)"
            );
        }
        other => panic!("expected pre-commit rollback, got {other:?}"),
    }
    drop(tx);
    let mut check = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(
        one(&mut check, "SELECT to_regclass('classified_aborted')::text").await,
        None
    );
    check.rollback().await.unwrap();
    drop(check);
    db.cleanup().await.unwrap();
}
