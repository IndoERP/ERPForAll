//! S4a review round: poisoning (E1/E2), unassigned stored computes (E3) and the review items.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use std::sync::Arc;

use erp_sdk::{ErrorKind, OrmCacheKey, OrmConfig, RecordSet, Transaction, Value};
use erp_test_models::test_compute::{
    self as tc, AsyncLen, Cascade, ComputeUnassigned, Constrained, Counted, Creator, CtxKey,
    Diamond, PerfBase, PrecomputeReads, Shouting,
};
use erp_test_models::test_orm::Schema;
use erp_test_models::{base_users, test_orm};
use erp_testkit::{Harness, assert_query_count};
use support::one;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_orm::ADDON, &tc::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .expect("harness")
}

fn s(v: Option<Arc<str>>) -> Option<String> {
    v.map(|s| s.to_string())
}

fn tagged<M: erp_sdk::Model>(rs: &RecordSet<M>, tag: &str) -> RecordSet<M> {
    rs.with_context([("log", Value::str(tag))])
}

/// Poll `fut` exactly once, then drop it (a cancelled operation); true when it was pending.
async fn poll_once<F: std::future::Future>(fut: F) -> bool {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| std::task::Poll::Ready(fut.as_mut().poll(cx).is_pending())).await
}

async fn count(tx: &mut Transaction, sql: &str) -> String {
    one(tx, sql).await.unwrap_or_default()
}

/// E1: a constraint failing in `create` poisons the transaction; nothing reaches the database.
#[tokio::test]
async fn e1_constraint_failure_in_create_refuses_commit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Constrained>().unwrap();
    let err = model
        .create(&mut tx, vec![model.vals().set(Constrained::NAME, "bad")])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(tx.is_poisoned());
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "upper may not be BAD");
    assert!(tx.commit().await.is_err(), "still refused");
    tx.rollback().await.unwrap();
    assert!(!tx.is_poisoned());
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM test_orm_constrained").await,
        "0"
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// E1: a constraint failing in `write` poisons the transaction; the bad value is never committed.
#[tokio::test]
async fn e1_constraint_failure_in_write_refuses_commit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Constrained>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Constrained::NAME, "ok")])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let err = rec
        .set(&mut tx, Constrained::NOTE, "bad")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        tx.commit().await.unwrap_err().message(),
        "note may not be bad"
    );
    tx.rollback().await.unwrap();
    let sql = format!(
        "SELECT coalesce(note, 'NULL') FROM test_orm_constrained WHERE id = {}",
        rec.ids()[0]
    );
    assert_eq!(count(&mut tx, &sql).await, "NULL");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// E1: a constraint failing after a recompute at flush poisons too (extended).
#[tokio::test]
async fn e1_constraint_failure_at_flush_refuses_commit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Constrained>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Constrained::NAME, "ok")])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rec.set(&mut tx, Constrained::NAME, "bad").await.unwrap();
    assert_eq!(tx.flush().await.unwrap_err().kind(), ErrorKind::Validation);
    assert_eq!(tx.commit().await.unwrap_err().kind(), ErrorKind::Validation);
    tx.rollback().await.unwrap();
    let sql = format!(
        "SELECT upper FROM test_orm_constrained WHERE id = {}",
        rec.ids()[0]
    );
    assert_eq!(count(&mut tx, &sql).await, "OK");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// E1: conversion errors poison too (S1's `Char(size)` check).
#[tokio::test]
async fn e1_conversion_error_refuses_commit() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let schema = tx.env().model::<Schema>().unwrap();
    let rec = schema
        .create(&mut tx, vec![schema.vals().set(Schema::REQUIRED, "r")])
        .await
        .unwrap();
    let err = rec.set(&mut tx, Schema::SIZE, "abcd").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(tx.commit().await.unwrap_err().kind(), ErrorKind::Validation);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// E1: the savepoint a validation error happened in is rolled back with it, and the transaction
/// commits.
#[tokio::test]
async fn e1_rolled_back_savepoint_clears_the_poison() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Constrained>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Constrained::NAME, "ok")])
        .await
        .unwrap();
    let r = rec.clone();
    let err = tx
        .savepoint(async |tx: &mut Transaction| r.set(tx, Constrained::NOTE, "bad").await)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(!tx.is_poisoned());
    rec.set(&mut tx, Constrained::NOTE, "fine").await.unwrap();
    tx.commit().await.unwrap();
    let sql = format!(
        "SELECT note FROM test_orm_constrained WHERE id = {}",
        rec.ids()[0]
    );
    assert_eq!(count(&mut tx, &sql).await, "fine");
    // a poisoned transaction cannot open a savepoint
    let err = rec
        .set(&mut tx, Constrained::NOTE, "bad")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    let err = tx
        .savepoint(async |_tx: &mut Transaction| Ok(()))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// E2: an operation cancelled in the middle of a compute poisons the transaction; only rollback
/// recovers it.
#[tokio::test]
async fn e2_cancelled_compute_poisons_until_rollback() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(PerfBase::VALUE, 1)])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    one(&mut tx, "SELECT 'open'::text").await; // open the database transaction
    // with YIELD_KEY, the async compute of `value_ctx` suspends once inside the compute: the
    // first poll stops there
    let keyed = rec.with_context([("key", Value::Int(i64::from(tc::YIELD_KEY)))]);
    let pending = keyed.get(&mut tx, PerfBase::VALUE_CTX);
    assert!(
        poll_once(pending).await,
        "the read must still be pending after one poll"
    );
    assert!(tx.is_poisoned());
    let err = keyed.get(&mut tx, PerfBase::VALUE_CTX).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("cancelled"), "{err}");
    assert_eq!(
        tx.commit().await.unwrap_err().kind(),
        ErrorKind::Programming
    );
    assert_eq!(tx.flush().await.unwrap_err().kind(), ErrorKind::Programming);
    tx.rollback().await.unwrap();
    assert!(!tx.is_poisoned());
    assert_eq!(
        keyed.get(&mut tx, PerfBase::VALUE_CTX).await.unwrap(),
        tc::YIELD_KEY
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// E2: the same for a write cancelled in an inverse, and for a savepoint.
#[tokio::test]
async fn e2_cancelled_write_and_savepoint_poison() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Cascade>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Cascade::FOO, "a")])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    one(&mut tx, "SELECT 'open'::text").await; // open the database transaction
    let r = rec.clone();
    let pending = tx.savepoint(async move |tx: &mut Transaction| {
        r.get(tx, Cascade::BAZ).await?;
        Ok(())
    });
    assert!(poll_once(pending).await, "pending after one poll");
    assert!(tx.is_poisoned());
    let err = tx
        .savepoint(async |_tx: &mut Transaction| Ok(()))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    tx.rollback().await.unwrap();
    rec.set(&mut tx, Cascade::FOO, "ok").await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// E3: an unassigned stored compute keeps the record's current value; new records get NULL.
#[tokio::test]
async fn e3_unassigned_stored_compute_keeps_the_current_value() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<ComputeUnassigned>().unwrap();
    let rec = model
        .create(
            &mut tx,
            vec![model.vals().set(ComputeUnassigned::FOO, "assign")],
        )
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, ComputeUnassigned::BARS).await.unwrap()).as_deref(),
        Some("assign")
    );
    rec.set(&mut tx, ComputeUnassigned::FOO, "x").await.unwrap();
    assert_eq!(
        s(rec.get(&mut tx, ComputeUnassigned::BARS).await.unwrap()).as_deref(),
        Some("assign")
    );
    tx.commit().await.unwrap();
    let sql = format!(
        "SELECT bars FROM test_orm_compute_unassigned WHERE id = {}",
        rec.ids()[0]
    );
    assert_eq!(count(&mut tx, &sql).await, "assign");
    let new = model
        .new_record(&mut tx, model.vals().set(ComputeUnassigned::FOO, "x"))
        .await
        .unwrap();
    assert_eq!(
        new.get(&mut tx, ComputeUnassigned::BARS).await.unwrap(),
        None
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 3: computes may depend on the built-in `display_name`.
#[tokio::test]
async fn display_name_has_dependents() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        tx.field_depends("test_orm.shouting", "display_name")
            .unwrap(),
        ["name"]
    );
    let model = tx.env().model::<Shouting>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(Shouting::NAME, "joe")])
        .await
        .unwrap();
    assert_eq!(
        s(rec.get(&mut tx, Shouting::SHOUT).await.unwrap()).as_deref(),
        Some("JOE")
    );
    assert_eq!(
        s(rec.get(&mut tx, Shouting::LABEL).await.unwrap()).as_deref(),
        Some("<joe>")
    );
    rec.set(&mut tx, Shouting::NAME, "ann").await.unwrap();
    assert!(!tx.is_cached(&rec, "shout").unwrap());
    assert_eq!(
        tx.records_to_compute(&rec, "label").unwrap(),
        *rec.as_records()
    );
    assert_eq!(
        s(rec.get(&mut tx, Shouting::SHOUT).await.unwrap()).as_deref(),
        Some("ANN")
    );
    tx.commit().await.unwrap();
    let sql = format!(
        "SELECT label FROM test_orm_shouting WHERE id = {}",
        rec.ids()[0]
    );
    assert_eq!(count(&mut tx, &sql).await, "<ann>");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 5: a compute reading a field that cannot be precomputed is not precomputed, and
/// `precompute = false` opts out.
#[tokio::test]
async fn precompute_respects_reads_and_explicit_false() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PrecomputeReads>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(PrecomputeReads::NAME, "n")])
        .await
        .unwrap();
    let mut to_compute = tx.fields_to_compute();
    to_compute.sort();
    assert_eq!(
        to_compute,
        [
            "test_orm.precompute.reads.p",
            "test_orm.precompute.reads.q",
            "test_orm.precompute.reads.s"
        ]
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        s(rec.get(&mut tx, PrecomputeReads::P).await.unwrap()).as_deref(),
        Some("p:n!")
    );
    assert_eq!(
        s(rec.get(&mut tx, PrecomputeReads::Q).await.unwrap()).as_deref(),
        Some("q:n")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 6: dependents of `write_date` follow the value the flush sets;:
/// rows written only by a recompute get the flushing environment's uid.
#[tokio::test]
async fn write_date_dependents_and_comp_069_write_uid() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    let rec = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    tx.commit().await.unwrap();
    let before = rec.get(&mut tx, PerfBase::WRITE_DAY).await.unwrap();
    assert!(before.is_some());
    let before_stamp = rec.get(&mut tx, erp_sdk::Base::WRITE_DATE).await.unwrap();
    rec.set(&mut tx, PerfBase::VALUE, 3).await.unwrap();
    tx.flush().await.unwrap();
    assert!(!tx.is_cached(&rec, "write_day").unwrap());
    let after = rec.get(&mut tx, PerfBase::WRITE_DAY).await.unwrap();
    let stamp = rec.get(&mut tx, erp_sdk::Base::WRITE_DATE).await.unwrap();
    assert_eq!(after.as_deref(), stamp.map(|d| d.to_string()).as_deref());
    assert_ne!(stamp, before_stamp, "the flush set a new write_date");
    tx.commit().await.unwrap();

    let users = tx.env().model::<erp_sdk::ResUsers>().unwrap();
    let demo = users
        .create(
            &mut tx,
            vec![users.vals().set(erp_sdk::ResUsers::NAME, "Demo")],
        )
        .await
        .unwrap();
    let creators = tx.env().model::<Creator>().unwrap();
    let rec = creators
        .with_user(demo.ids()[0])
        .create(&mut tx, vec![creators.vals().set(Creator::NAME, "x")])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let sql = format!(
        "SELECT write_uid::text FROM test_orm_compute_creator WHERE id = {}",
        rec.ids()[0]
    );
    assert_eq!(
        count(&mut tx, &sql).await,
        "1",
        "flushed by the transaction's uid"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 9: a malformed company context is an error, never the "no company" key.
#[tokio::test]
async fn malformed_company_context_is_an_error() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let env = tx
        .env()
        .with_context([("allowed_company_ids", Value::str("1,2"))]);
    // the context's own Validation error, kept by the environment (review F2)
    let context_error = env.context().allowed_company_ids().unwrap_err();
    let err = tx.cache_key(&env, "test_orm.ctx_key", "all").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), context_error.message());
    let model = tx.env().model::<CtxKey>().unwrap();
    let rec = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let err = rec
        .with_env(env)
        .get(&mut tx, CtxKey::ALL)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), context_error.message());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 11: a diamond is walked once per field, and computed in dependency order.
#[tokio::test]
async fn diamond_dependencies_are_visited_once_in_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tagged(&tx.env().model::<Diamond>().unwrap(), "diamond");
    let rec = model
        .create(&mut tx, vec![model.vals().set(Diamond::A, "x")])
        .await
        .unwrap();
    assert_eq!(tc::take_log("diamond"), ["b", "c", "d"], "precomputed");
    rec.set(&mut tx, Diamond::A, "y").await.unwrap();
    assert_eq!(tx.records_to_compute(&rec, "d").unwrap(), *rec.as_records());
    assert_eq!(
        s(rec.get(&mut tx, Diamond::D).await.unwrap()).as_deref(),
        Some("yb+yc")
    );
    assert_eq!(tc::take_log("diamond"), ["b", "c", "d"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// most `recompute_batch` records, for stored and non-stored fields and at create.
#[tokio::test]
async fn comp_025_recompute_in_bounded_batches() {
    let h = Harness::builder([&tc::ADDON])
        .config(OrmConfig {
            recompute_batch: 1000,
            ..OrmConfig::default()
        })
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tagged(&tx.env().model::<PerfBase>().unwrap(), "c025");
    let vals = (0..2500)
        .map(|i| model.vals().set(PerfBase::VALUE, i))
        .collect();
    let recs = model.create(&mut tx, vals).await.unwrap();
    assert_eq!(
        tc::take_log("c025"),
        ["value_pc x1000", "value_pc x1000", "value_pc x500"]
    );
    recs.set(&mut tx, PerfBase::VALUE, -1).await.unwrap();
    // pull recompute in the records' (tagged) environment
    recs.mapped(&mut tx, PerfBase::VALUE_PC).await.unwrap();
    assert_eq!(
        tc::take_log("c025"),
        ["value_pc x1000", "value_pc x1000", "value_pc x500"]
    );
    let counted = tagged(&tx.env().model::<Counted>().unwrap(), "c025n");
    let vals = (0..2500)
        .map(|i| counted.vals().set(Counted::N, i))
        .collect();
    let crecs = counted.create(&mut tx, vals).await.unwrap();
    crecs.mapped(&mut tx, Counted::DOUBLE).await.unwrap();
    assert_eq!(
        tc::take_log("c025n"),
        ["double x1000", "double x1000", "double x500"]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 13: an async stored compute that yields the same value writes nothing.
#[tokio::test]
async fn async_compute_unchanged_result_writes_nothing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<AsyncLen>().unwrap();
    let rec = model
        .create(&mut tx, vec![model.vals().set(AsyncLen::NAME, "abc")])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rec.set(&mut tx, AsyncLen::NAME, "xyz").await.unwrap();
    tx.capture_statements(true);
    tx.flush().await.unwrap();
    let updates = tx.captured().join("\n");
    assert!(!updates.contains("\"len\" ="), "{updates}");
    assert_eq!(rec.get(&mut tx, AsyncLen::LEN).await.unwrap(), 3);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 15: values computed by a transaction that read a cache before another
/// transaction invalidated it are not published.
#[tokio::test]
async fn ormcache_stale_values_are_not_published() {
    let h = harness().await;
    let key = OrmCacheKey::new("m", "f", vec![Value::Int(1)]);
    let mut t1 = h.begin().await.unwrap();
    let mut t2 = h.begin().await.unwrap();
    assert_eq!(t1.ormcache_get("default", &key).await.unwrap(), None);
    t1.ormcache_add("default", key.clone(), Value::str("stale"))
        .await
        .unwrap();
    t2.invalidate_ormcache("default");
    t2.commit().await.unwrap();
    t1.commit().await.unwrap();
    let mut t3 = h.begin().await.unwrap();
    assert_eq!(t3.ormcache_get("default", &key).await.unwrap(), None);
    t3.ormcache_add("default", key.clone(), Value::str("fresh"))
        .await
        .unwrap();
    t3.commit().await.unwrap();
    let mut t4 = h.begin().await.unwrap();
    assert_eq!(
        t4.ormcache_get("default", &key).await.unwrap(),
        Some(Value::str("fresh"))
    );
    drop((t1, t2, t3, t4));
    h.cleanup().await.unwrap();
}

/// Review item 15: commit refuses while a savepoint is open.
#[tokio::test]
async fn commit_inside_a_savepoint_is_refused() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .savepoint(async |tx: &mut Transaction| tx.commit().await)
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Pending savepoints not released, cannot commit!"
    );
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 17: the futures of the transaction-scoped APIs are `Send`.
#[tokio::test]
async fn transaction_scope_futures_are_send() {
    fn assert_send<T: Send>(t: T) -> T {
        t
    }
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<Cascade>().unwrap();
    let rec = model.create(&mut tx, vec![model.vals()]).await.unwrap();
    let records = rec.as_records().clone();
    assert_send(tx.protecting(&["baz"], &records, async |_tx: &mut Transaction| Ok(())))
        .await
        .unwrap();
    assert_send(tx.savepoint(async |_tx: &mut Transaction| Ok(())))
        .await
        .unwrap();
    let key = OrmCacheKey::new("m", "f", vec![]);
    assert_send(tx.ormcache("default", key, async |_tx: &mut Transaction| {
        Ok(Value::Int(1))
    }))
    .await
    .unwrap();
    assert_send(tx.flush()).await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// S3/M3): a search flushes what its WHERE clause reads, recomputing pending stored computes
/// first, and only those fields.
#[tokio::test]
async fn spec_comp_058_search_flushes_and_recomputes_what_it_reads() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    let recs = model
        .create(
            &mut tx,
            vec![
                model
                    .vals()
                    .set(PerfBase::NAME, "a")
                    .set(PerfBase::VALUE, 1),
                model
                    .vals()
                    .set(PerfBase::NAME, "b")
                    .set(PerfBase::VALUE, 2),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let (r1, r2) = (recs.at(0).unwrap(), recs.at(1).unwrap());
    recs.mapped(&mut tx, PerfBase::VALUE_PC).await.unwrap();
    // a pending stored compute in the domain: recomputed, written, then searched
    r1.set(&mut tx, PerfBase::VALUE, 42).await.unwrap();
    r2.set(&mut tx, PerfBase::NAME, "other").await.unwrap();
    tx.capture_statements(true);
    let found = assert_query_count!(tx, >= 2, <= 2, {
        model
            .search(&mut tx, &support::dom(erp_sdk::serde_json::json!([["value_pc", "=", 0.42]])))
            .await
            .unwrap()
    });
    assert_eq!(found.ids(), r1.ids());
    let log = tx.captured().to_vec();
    assert!(
        log[0].starts_with("UPDATE") && log[0].contains("\"value_pc\""),
        "{log:?}"
    );
    assert!(
        !log[0].contains("\"name\" ="),
        "only what the search reads: {log:?}"
    );
    assert!(log[1].starts_with("SELECT"), "{log:?}");
    assert!(
        tx.is_dirty(&r2, "name").unwrap(),
        "other fields keep their pending writes"
    );
    // a pending plain write in the domain
    let found = assert_query_count!(tx, >= 2, <= 2, {
        model
            .search(&mut tx, &support::dom(erp_sdk::serde_json::json!([["name", "=", "other"]])))
            .await
            .unwrap()
    });
    assert_eq!(found.ids(), r2.ids());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A budget check that the RETURNING-free flush still reports deleted rows with one extra query
/// only on that error path (review item 19).
#[tokio::test]
async fn flush_reports_deleted_rows_without_returning() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<PerfBase>().unwrap();
    let recs = model
        .create(&mut tx, vec![model.vals(), model.vals()])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    recs.set(&mut tx, PerfBase::NAME, "x").await.unwrap();
    tx.capture_statements(true);
    assert_query_count!(tx, >= 1, <= 1, { tx.flush().await.unwrap() });
    assert!(
        !tx.captured()[0].contains("RETURNING"),
        "{:?}",
        tx.captured()
    );
    let gone = recs.ids()[1];
    tx.execute(
        &format!("DELETE FROM test_performance_base WHERE id = {gone}"),
        &[],
    )
    .await
    .unwrap();
    recs.set(&mut tx, PerfBase::NAME, "y").await.unwrap();
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert!(
        err.message()
            .contains(&format!("test_performance.base({gone},)")),
        "{err}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
