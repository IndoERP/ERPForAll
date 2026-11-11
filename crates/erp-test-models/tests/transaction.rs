//! Transaction lifecycle, retries (–5) and runtime safety checks from the S1 review.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::{Context, ErrorKind, Orm, SUPERUSER_ID, StaticPolicy, Value};
use erp_test_models::test_orm::{ModelB, Partner};
use erp_test_models::{base_users, test_orm};
use erp_testkit::{Harness, TestDb};
use support::{map, one};

async fn backend_pid(orm: &Orm) -> String {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let pid = one(&mut tx, "SELECT pg_backend_pid()::text").await.unwrap();
    tx.commit().await.unwrap();
    pid
}

/// A server error aborts the transaction: commit() rolls back and fails; the transaction is usable
/// again afterwards.
#[tokio::test]
async fn commit_after_a_failed_statement_is_an_error() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "kept?")])
        .await
        .unwrap();
    let model_b = tx.env().model::<ModelB>().unwrap();
    let negative = model_b
        .vals()
        .set(ModelB::NAME, "B1")
        .set(ModelB::VALUE, -1);
    let err = model_b.create(&mut tx, vec![negative]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(err.sqlstate(), Some("23514"));
    // rolled back: nothing of the transaction persisted, and the transaction works again
    assert_eq!(
        one(&mut tx, "SELECT count(*)::text FROM test_orm_partner")
            .await
            .as_deref(),
        Some("0")
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A future dropped during BEGIN (or any statement) never returns a connection that may be inside
/// a transaction to the pool: the connection is detached and a new one is used next.
#[tokio::test]
async fn dropped_operation_detaches_the_connection() {
    let db = TestDb::builder().pool_size(1).create().await.unwrap();
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_orm::ADDON])
            .with_decls(crate::currency_fixture::decls())
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let first = backend_pid(&orm).await;
    assert_eq!(
        backend_pid(&orm).await,
        first,
        "pool of one: same connection"
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let pending = tx.query("SELECT pg_sleep(5)", &[]);
    assert!(
        tokio::time::timeout(Duration::from_millis(1), pending)
            .await
            .is_err(),
        "the statement was cut short"
    );
    drop(tx);
    let next = backend_pid(&orm).await;
    assert_ne!(
        next, first,
        "the connection was detached, not returned in a transaction"
    );
    drop(orm);
    db.cleanup().await.unwrap();
}

/// Session state set by code outside the ORM (a `.sql` data file) stays on the connection unless
/// the transaction declares it: a tainted transaction closes its connection instead of returning
/// it, so the next user of the pool sees the default.
#[tokio::test]
async fn a_tainted_connection_does_not_go_back_to_the_pool() {
    let db = TestDb::builder().pool_size(1).create().await.unwrap();
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_orm::ADDON])
            .with_decls(crate::currency_fixture::decls())
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, db.pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let show = "SELECT current_setting('application_name')";
    let default = {
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let value = one(&mut tx, show).await.unwrap();
        tx.commit().await.unwrap();
        value
    };
    // without the declaration the setting leaks to the next user of the connection
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("SET application_name = 'leaked'").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(one(&mut tx, show).await.as_deref(), Some("leaked"));
    tx.batch("RESET application_name").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // with it, the next user gets a fresh connection
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.batch("SET application_name = 'leaked'").await.unwrap();
    tx.taint_connection();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    assert_eq!(one(&mut tx, show).await, Some(default));
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);
    db.cleanup().await.unwrap();
}

/// a serialization failure re-runs the unit of work; hooks run once, after the final
/// commit.
#[tokio::test]
async fn run_retries_serialization_failures() {
    let h = support::orm().await;
    let pid = h
        .run(async |tx| {
            let rec = tx.env().records("test_orm.partner")?;
            Ok(rec
                .create_values(tx, &[map(&[("number", Value::Int(1))])])
                .await?
                .ids()[0])
        })
        .await
        .unwrap();
    let attempts = AtomicUsize::new(0);
    let hooks = Arc::new(AtomicUsize::new(0));
    let orm = h.orm().clone();
    h.run(async |tx| {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
        let rec = tx.env().records("test_orm.partner")?.browse([pid])?;
        rec.value(tx, "number").await?; // takes the snapshot
        if attempt == 1 {
            // another transaction changes the row and commits
            let mut other = orm.begin(SUPERUSER_ID).await?;
            let theirs = other.env().records("test_orm.partner")?.browse([pid])?;
            theirs
                .set_value(&mut other, "number", Value::Int(7))
                .await?;
            other.commit().await?;
        }
        rec.set_value(tx, "number", Value::Int(42)).await?;
        tx.flush().await?;
        let counter = Arc::clone(&hooks);
        tx.after_commit(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "first attempt hit 40001, second succeeded"
    );
    assert_eq!(
        hooks.load(Ordering::SeqCst),
        1,
        "after-commit hook ran once"
    );
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            &format!("SELECT number::text FROM test_orm_partner WHERE id = {pid}")
        )
        .await
        .as_deref(),
        Some("42")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Other errors are not retried, and hooks of a failed unit of work never run.
#[tokio::test]
async fn run_does_not_retry_other_errors() {
    let h = support::orm().await;
    let attempts = AtomicUsize::new(0);
    let hooks = Arc::new(AtomicUsize::new(0));
    let err = h
        .run(async |tx| {
            attempts.fetch_add(1, Ordering::SeqCst);
            let counter = Arc::clone(&hooks);
            tx.after_commit(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            });
            let model = tx.env().model::<ModelB>()?;
            let negative = model.vals().set(ModelB::NAME, "B1").set(ModelB::VALUE, -1);
            model.create(tx, vec![negative]).await?;
            Ok(())
        })
        .await
        .unwrap_err();
    let erp_testkit::HarnessError::Orm(err) = err else {
        panic!("unexpected harness error: {err}");
    };
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message().contains("The value must be positive"),
        "{err}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.load(Ordering::SeqCst), 0);
    h.cleanup().await.unwrap();
}

static OTHER: AddonDescriptor = AddonDescriptor {
    name: "test_other_registry",
    depends: &["base"],
    exact_only: false,
};

/// Records of another registry cannot be used with a transaction (their ids would be resolved
/// against the wrong metadata).
#[tokio::test]
async fn records_of_another_registry_are_refused() {
    let h = support::orm().await;
    let other = Arc::new(
        RegistryBuilder::new([&OTHER])
            .with_decls([&*Box::leak(Box::new(
                ModelDecl::new("test_other_registry", "test_orm.partner")
                    .fields(const { &[FieldDecl::char("name")] }),
            ))])
            .build()
            .unwrap(),
    );
    let foreign_orm = Orm::builder(other, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let foreign = foreign_orm
        .env(SUPERUSER_ID, Context::new())
        .records("test_orm.partner")
        .unwrap()
        .browse([1])
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let err = foreign.value(&mut tx, "name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("another registry"), "{err}");
    let err = foreign
        .write_values(&mut tx, &map(&[("name", Value::str("x"))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert!(err.message().contains("another registry"), "{err}");
    drop(tx);
    drop(foreign_orm);
    h.cleanup().await.unwrap();
}

static EPOCH: AddonDescriptor = AddonDescriptor {
    name: "test_epoch",
    depends: &["base"],
    exact_only: false,
};

/// After DDL on one connection, another pooled connection drops its prepared statements before
/// reusing them: a cached plan whose result type changed would fail ("cached plan must not change
/// result type").
#[tokio::test]
async fn schema_changes_invalidate_prepared_statements_pool_wide() {
    let v1 = ModelDecl::new("test_epoch", "x.epoch")
        .fields(const { &[FieldDecl::char("code").size(5)] });
    let h = Harness::builder([&EPOCH])
        .with_decls([v1])
        .build()
        .await
        .unwrap();
    let orm = h.orm().clone();
    // A prepares the ORM's read of `code` (varchar) on its connection
    let mut a = orm.begin(SUPERUSER_ID).await.unwrap();
    let rec = a
        .env()
        .records("x.epoch")
        .unwrap()
        .create_values(&mut a, &[map(&[("code", Value::str("abc"))])])
        .await
        .unwrap();
    a.invalidate_all().await.unwrap();
    assert_eq!(rec.value(&mut a, "code").await.unwrap(), Value::str("abc"));
    a.commit().await.unwrap();
    // B, on a second connection while A still holds the first, changes the result type
    let mut b = orm.begin(SUPERUSER_ID).await.unwrap();
    b.batch("ALTER TABLE x_epoch ALTER COLUMN code TYPE text")
        .await
        .unwrap();
    b.commit().await.unwrap();
    drop(b);
    drop(a); // the first connection, with its old prepared statement, goes back to the pool
    // read through both pooled connections at once
    let mut c1 = orm.begin(SUPERUSER_ID).await.unwrap();
    let mut c2 = orm.begin(SUPERUSER_ID).await.unwrap();
    for c in [&mut c1, &mut c2] {
        let mine = rec.with_env(c.env());
        assert_eq!(mine.value(c, "code").await.unwrap(), Value::str("abc"));
    }
    drop((c1, c2));
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Change the column type of `x_epoch.code` and read it back (preparing the read against the
/// changed shape), then fail.
async fn alter_and_read(
    tx: &mut erp_orm::Transaction,
    rec: &erp_orm::Records,
) -> erp_fields::Result<()> {
    tx.batch("ALTER TABLE x_epoch ALTER COLUMN code TYPE text")
        .await?;
    tx.invalidate_all().await?;
    assert_eq!(rec.value(tx, "code").await?, Value::str("abc"));
    Err(erp_fields::Error::user("undo"))
}

/// Statements prepared after DDL inside a transaction describe the changed table; when the
/// transaction rolls back (whole or to a savepoint), the table is back to its old shape, so the
/// connection drops them (lead review H on; "cached plan must not change result type"
/// otherwise). The same transaction object keeps its connection across rollbacks.
#[tokio::test]
async fn rollback_after_schema_change_drops_prepared_statements() {
    let v1 = ModelDecl::new("test_epoch", "x.epoch")
        .fields(const { &[FieldDecl::char("code").size(5)] });
    let h = Harness::builder([&EPOCH])
        .with_decls([v1])
        .build()
        .await
        .unwrap();
    let orm = h.orm().clone();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rec = tx
        .env()
        .records("x.epoch")
        .unwrap()
        .create_values(&mut tx, &[map(&[("code", Value::str("abc"))])])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for savepoint in [false, true] {
        tx.invalidate_all().await.unwrap();
        assert_eq!(rec.value(&mut tx, "code").await.unwrap(), Value::str("abc"));
        let err = if savepoint {
            tx.savepoint(async |tx| alter_and_read(tx, &rec).await)
                .await
                .unwrap_err()
        } else {
            let err = alter_and_read(&mut tx, &rec).await.unwrap_err();
            tx.rollback().await.unwrap();
            err
        };
        assert_eq!(err.message(), "undo");
        tx.invalidate_all().await.unwrap();
        assert_eq!(
            rec.value(&mut tx, "code").await.unwrap(),
            Value::str("abc"),
            "savepoint={savepoint}"
        );
        tx.rollback().await.unwrap();
    }
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// The public ORM futures are `Send` (one task owns a transaction, futures move
/// between worker threads).
#[tokio::test]
async fn public_futures_are_send() {
    fn assert_send<T: Send>(_: &T) {}
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let records = partners.as_records().clone();
    assert_send(&partners.create(&mut tx, vec![]));
    assert_send(&partners.get(&mut tx, Partner::NAME));
    assert_send(&partners.mapped(&mut tx, Partner::NAME));
    assert_send(&partners.write(&mut tx, partners.vals()));
    assert_send(&records.read(&mut tx, &["name"]));
    assert_send(&records.value(&mut tx, "name"));
    assert_send(&tx.flush());
    assert_send(&tx.commit());
    assert_send(&tx.rollback());
    assert_send(&tx.init_models(&[]));
    assert_send(&h.orm().begin(SUPERUSER_ID));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A harness dropped without `cleanup()` (a panicking test) still drops its database.
#[tokio::test]
async fn harness_drop_removes_the_database() {
    let h = support::orm().await;
    let name = h.db().name().to_owned();
    let cluster = h.db().cluster().clone();
    drop(h);
    assert!(!cluster.database_exists(&name).await.unwrap());
}

#[tokio::test]
async fn spec_comp_057_after_commit_hooks() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();

    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran_clone = std::sync::Arc::clone(&ran);
    tx.after_commit(move || {
        ran_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    // cr.flush() does not run after_commit hooks
    tx.flush().await.unwrap();
    assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));

    // cr.commit() runs after_commit hooks
    tx.commit().await.unwrap();
    assert!(ran.load(std::sync::atomic::Ordering::SeqCst));

    // cr.rollback() drops after_commit hooks without running
    let mut tx = h.begin().await.unwrap();
    let ran_rollback = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran_rollback_clone = std::sync::Arc::clone(&ran_rollback);
    tx.after_commit(move || {
        ran_rollback_clone.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    tx.rollback().await.unwrap();
    assert!(!ran_rollback.load(std::sync::atomic::Ordering::SeqCst));

    drop(tx);
    h.cleanup().await.unwrap();
}
