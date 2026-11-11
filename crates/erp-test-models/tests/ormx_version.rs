//! Row versions, expected versions and write hooks (
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use erp_sdk::{
    Base, BoxFuture, Cell, ErrorKind, RawVals, RecId, RecordSet, Result, SUPERUSER_ID, Transaction,
    Upsert, UpsertOutcome, Vals, Value, WriteBatch, WriteHook,
};
use erp_test_models::base_users;
use erp_test_models::test_scale_upsert::hooks::{self, QuantHooks};
use erp_test_models::test_scale_upsert::{
    self as scale, ScaleLocation, ScaleProduct, ScaleQuant, ScaleSerial,
};
use erp_testkit::{Harness, assert_query_count};
use support::one;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &scale::ADDON, &hooks::ADDON])
        .build()
        .await
        .unwrap()
}

/// A location and `n` quants (quantity `i`), created through `create`.
async fn quants(tx: &mut Transaction, n: usize) -> (RecId, RecordSet<ScaleQuant>) {
    let env = tx.env();
    let location = env
        .model::<ScaleLocation>()
        .unwrap()
        .create(tx, vec![Vals::new().set(ScaleLocation::NAME, "Stock")])
        .await
        .unwrap()
        .rec_ids()[0];
    let products = env
        .model::<ScaleProduct>()
        .unwrap()
        .create(
            tx,
            (0..n)
                .map(|i| Vals::new().set(ScaleProduct::NAME, format!("P{i}")))
                .collect(),
        )
        .await
        .unwrap();
    let rows = products
        .rec_ids()
        .iter()
        .enumerate()
        .map(|(i, p)| {
            Vals::new()
                .set(ScaleQuant::PRODUCT_ID, *p)
                .set(ScaleQuant::LOCATION_ID, location)
                .set(ScaleQuant::QUANTITY, i as f64)
        })
        .collect();
    let created = env
        .model::<ScaleQuant>()
        .unwrap()
        .create(tx, rows)
        .await
        .unwrap();
    (location, created)
}

async fn stored_version(tx: &mut Transaction, id: RecId) -> i64 {
    one(
        tx,
        &format!("SELECT row_version::text FROM scale_quant WHERE id = {id}"),
    )
    .await
    .unwrap()
    .parse()
    .unwrap()
}

fn note(text: &str) -> Vals<ScaleQuant> {
    Vals::new().set(ScaleQuant::NOTE, text)
}

#[tokio::test]
async fn spec_ormx_011_every_orm_update_bumps_the_version() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (location, created) = quants(&mut tx, 1).await;
    tx.flush().await.unwrap();
    let q = created.at(0).unwrap();
    let id = q.rec_ids()[0];
    // readable like write_date, through the typed and the dynamic surface
    assert_eq!(q.get(&mut tx, Base::ROW_VERSION).await.unwrap(), 1);
    assert_eq!(stored_version(&mut tx, id).await, 1);

    // a write: one UPDATE, as on a model without versions
    q.write(&mut tx, note("a")).await.unwrap();
    assert_query_count!(tx, >= 1, <= 1, { tx.flush().await.unwrap() });
    assert_eq!(stored_version(&mut tx, id).await, 2);
    assert_eq!(q.get(&mut tx, Base::ROW_VERSION).await.unwrap(), 2);
    // the same write on the unversioned model is one UPDATE too
    let serial = tx
        .env()
        .model::<ScaleSerial>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(ScaleSerial::CODE, "S")
                    .set(ScaleSerial::NUMBER, 1),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    serial
        .write(&mut tx, Vals::new().set(ScaleSerial::HITS, 1))
        .await
        .unwrap();
    assert_query_count!(tx, >= 1, <= 1, { tx.flush().await.unwrap() });

    // a stored compute recomputing the row (the location is renamed)
    tx.env()
        .model::<ScaleLocation>()
        .unwrap()
        .browse([location.get()])
        .unwrap()
        .write(&mut tx, Vals::new().set(ScaleLocation::NAME, "Renamed"))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        q.get(&mut tx, QuantHooks::LOCATION_NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Renamed")
    );
    assert_eq!(stored_version(&mut tx, id).await, 3);

    // an upsert that updates it, then one that leaves it unchanged (on a field no compute
    // depends on, so no recompute adds an UPDATE of its own)
    let product = q
        .get(&mut tx, ScaleQuant::PRODUCT_ID)
        .await
        .unwrap()
        .unwrap();
    let key = || {
        Vals::<ScaleQuant>::new()
            .set(ScaleQuant::PRODUCT_ID, product)
            .set(ScaleQuant::LOCATION_ID, location)
            .set(ScaleQuant::LOT_ID, None::<RecId>)
    };
    let model = tx.env().model::<ScaleQuant>().unwrap();
    let out = assert_query_count!(tx, >= 1, <= 1, {
        model
            .upsert(
                &mut tx,
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows([key().set(ScaleQuant::MOVE_COUNT, 1)])
                    .accumulate(ScaleQuant::MOVE_COUNT),
            )
            .await
            .unwrap()
    });
    assert_eq!(
        (
            out.rows()[0].outcome(),
            out.rows()[0].previous_version(),
            out.rows()[0].version()
        ),
        (UpsertOutcome::Updated, Some(3), Some(4))
    );
    let out = model
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key().set(ScaleQuant::MOVE_COUNT, 0)])
                .accumulate(ScaleQuant::MOVE_COUNT),
        )
        .await
        .unwrap();
    assert_eq!(
        (out.rows()[0].outcome(), out.rows()[0].version()),
        (UpsertOutcome::Unchanged, Some(4))
    );
    assert_eq!(stored_version(&mut tx, id).await, 4);
    assert_eq!(q.get(&mut tx, Base::ROW_VERSION).await.unwrap(), 4);
    let read = q.as_records().values(&mut tx, "row_version").await.unwrap();
    assert_eq!(read, vec![Value::Int(4)]);

    // never written by callers; never copied
    let spec = tx.registry().model("scale.quant").unwrap().clone();
    let message = format!(
        "'{}.{}' is set by the server and cannot be written",
        spec.name,
        erp_registry::ROW_VERSION
    );
    let err = q
        .as_records()
        .write(&mut tx, RawVals::new().set_cell("row_version", Cell::Id(9)))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Programming, message.as_str())
    );
    let err = q
        .as_records()
        .write_values(&mut tx, &support::map(&[("row_version", Value::Int(9))]))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Programming, message.as_str())
    );
    let err = model
        .create(&mut tx, vec![key().set(Base::ROW_VERSION, 5)])
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Programming, message.as_str())
    );
    assert!(!spec.field(erp_registry::ROW_VERSION).unwrap().copy);
    assert_eq!(stored_version(&mut tx, id).await, 4);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_012_raw_sql_bumps_through_the_trigger() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, created) = quants(&mut tx, 1).await;
    tx.commit().await.unwrap();
    let id = created.rec_ids()[0];
    let trigger = erp_sql::version::trigger_name("scale_quant");
    let mut tx = h.begin().await.unwrap();
    let definition = one(
        &mut tx,
        &format!(
            "SELECT pg_get_triggerdef(oid) FROM pg_trigger WHERE tgname = '{trigger}' AND \
             tgrelid = 'scale_quant'::regclass"
        ),
    )
    .await
    .unwrap();
    for part in [
        "BEFORE UPDATE ON",
        "FOR EACH ROW",
        "WHEN ((new.row_version IS DISTINCT FROM (old.row_version + 1)))",
        "EXECUTE FUNCTION erp_row_version_bump()",
    ] {
        assert!(
            definition.contains(part),
            "{part} missing from {definition}"
        );
    }
    let raw = [
        "UPDATE scale_quant SET quantity = 9 WHERE id = $1",
        "UPDATE scale_quant SET quantity = 10, row_version = 50 WHERE id = $1",
        "UPDATE scale_quant SET quantity = quantity WHERE id = $1",
    ];
    let mut versions = Vec::new();
    for sql in raw {
        tx.execute(sql, &[&id.get()]).await.unwrap();
        versions.push(stored_version(&mut tx, id).await);
    }
    assert_eq!(versions, vec![2, 3, 4], "each raw UPDATE bumps by one");
    // dropping the trigger by hand: the next sync re-creates it, a sync after that does nothing
    tx.batch(&format!("DROP TRIGGER \"{trigger}\" ON scale_quant"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let created_ddl = erp_sql::version::bump_trigger("scale_quant", false);
    assert!(
        tx.captured().iter().any(|s| s.contains(&created_ddl[0])),
        "the sync re-created the trigger: {:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    assert!(
        !tx.captured().iter().any(|s| s.contains("TRIGGER")),
        "nothing to re-create: {:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    tx.execute(raw[0], &[&id.get()]).await.unwrap();
    assert_eq!(stored_version(&mut tx, id).await, 5);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_013_stale_version_conflicts_deleted_record_is_missing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, created) = quants(&mut tx, 3).await;
    tx.commit().await.unwrap();
    let ids: Vec<RecId> = created.rec_ids().to_vec();
    let (q1, q2, q3) = (ids[0], ids[1], ids[2]);
    let spec = h.registry().model("scale.quant").unwrap().clone();
    // q1 at version 3
    h.run(async |tx| {
        let q = tx.env().model::<ScaleQuant>()?.browse([q1.get()])?;
        q.write(tx, note("v2")).await?;
        tx.flush().await?;
        q.write(tx, note("v3")).await?;
        Ok(())
    })
    .await
    .unwrap();

    // a stale expectation: Conflict, not retried, nothing written, one classification SELECT
    let attempts = AtomicUsize::new(0);
    let plain = AtomicUsize::new(0);
    let err = h
        .run(async |tx| {
            attempts.fetch_add(1, Ordering::SeqCst);
            let q = tx.env().model::<ScaleQuant>()?.browse([q1.get()])?;
            q.expect_versions(tx, [(q1, 2)]).await?;
            q.write(tx, note("lost")).await?;
            let before = tx.query_count();
            let result = tx.flush().await;
            plain.store(
                usize::try_from(tx.query_count() - before).unwrap(),
                Ordering::SeqCst,
            );
            result
        })
        .await
        .unwrap_err();
    let erp_testkit::HarnessError::Orm(err) = err else {
        panic!("unexpected harness error: {err}");
    };
    assert_eq!(err.kind(), ErrorKind::Conflict);
    assert_eq!(
        err.message(),
        format!(
            "Record '{}' ({}) {q1} was changed by someone else since you read it (version 2, \
             now 3). Reload it and apply your change again.",
            spec.description, spec.name
        )
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "Conflict is never retried"
    );
    assert_eq!(
        plain.load(Ordering::SeqCst),
        2,
        "the UPDATE and one classification SELECT"
    );
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            &format!("SELECT note FROM scale_quant WHERE id = {q1}")
        )
        .await
        .as_deref(),
        Some("v3")
    );
    assert_eq!(stored_version(&mut tx, q1).await, 3);

    // a matching expectation adds nothing; a second flush of the record does not re-check
    let q = tx
        .env()
        .model::<ScaleQuant>()
        .unwrap()
        .browse([q1.get()])
        .unwrap();
    q.expect_versions(&mut tx, [(q1, 3)]).await.unwrap();
    q.write(&mut tx, note("v4")).await.unwrap();
    assert_query_count!(tx, >= 1, <= 1, { tx.flush().await.unwrap() });
    q.write(&mut tx, note("v5")).await.unwrap();
    assert_query_count!(tx, >= 1, <= 1, { tx.flush().await.unwrap() });
    assert_eq!(stored_version(&mut tx, q1).await, 5);

    // several stale records at once
    let both = tx
        .env()
        .model::<ScaleQuant>()
        .unwrap()
        .browse([q1.get(), q3.get()])
        .unwrap();
    both.expect_versions(&mut tx, [(q1, 1), (q3, 7)])
        .await
        .unwrap();
    both.write(&mut tx, note("x")).await.unwrap();
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Conflict);
    assert_eq!(
        err.message(),
        format!(
            "Records '{}' ({}) {q1} (version 1, now 5), {q3} (version 7, now 1) were changed by \
             someone else since you read them. Reload them and apply your changes again.",
            spec.description, spec.name
        )
    );
    tx.rollback().await.unwrap();

    // a record deleted after it was read: Missing
    let mut tx = h.begin().await.unwrap();
    let q = tx
        .env()
        .model::<ScaleQuant>()
        .unwrap()
        .browse([q2.get()])
        .unwrap();
    q.expect_versions(&mut tx, [(q2, 1)]).await.unwrap();
    tx.execute("DELETE FROM scale_quant WHERE id = $1", &[&q2.get()])
        .await
        .unwrap();
    q.write(&mut tx, note("gone")).await.unwrap();
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert_eq!(
        err.message(),
        format!(
            "Record does not exist or has been deleted.\n(Record: {}({q2},), User: {SUPERUSER_ID})",
            spec.name
        )
    );
    assert!(
        tx.is_poisoned(),
        "the error follows a flush UPDATE (review SF-N1)"
    );
    tx.rollback().await.unwrap();

    // a model without row versions, and invalid expectations
    let mut tx = h.begin().await.unwrap();
    let serials = tx.env().model::<ScaleSerial>().unwrap();
    let err = serials
        .expect_versions(&mut tx, [(q1, 1)])
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "'scale.serial' has no row version (declare row_version = true)"
        )
    );
    let model = tx.env().model::<ScaleQuant>().unwrap();
    let err = model.expect_versions(&mut tx, [(q1, 0)]).await.unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            format!(
                "expect_versions on 'scale.quant': record {q1} cannot have version 0 (row \
                 versions start at 1)"
            )
            .as_str()
        )
    );
    let err = model
        .expect_versions(&mut tx, [(q1, 2), (q1, 3)])
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            format!("expect_versions on 'scale.quant': record {q1} is given versions 2 and 3")
                .as_str()
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `row_version` is opt-in per model (decided 2026-10-16 after the release-mode
/// measurement): a model without `row_version = true` gets no
/// column and no trigger, and `expect_versions` on it fails loudly naming the declaration.
#[tokio::test]
async fn a_model_without_the_declaration_has_no_row_version() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let column = |table: &'static str| {
        format!(
            "SELECT count(*)::text FROM information_schema.columns WHERE table_schema = \
             current_schema() AND table_name = '{table}' AND column_name = 'row_version'"
        )
    };
    let trigger = |table: &'static str| {
        format!(
            "SELECT count(*)::text FROM pg_trigger WHERE tgrelid = '{table}'::regclass AND \
             NOT tgisinternal AND tgname = '{}'",
            erp_sql::version::trigger_name(table)
        )
    };
    // the declared model has both, the other neither
    assert_eq!(
        one(&mut tx, &column("scale_quant")).await.unwrap(),
        "1",
        "scale.quant declares row_version = true"
    );
    assert_eq!(one(&mut tx, &trigger("scale_quant")).await.unwrap(), "1");
    assert_eq!(one(&mut tx, &column("scale_serial")).await.unwrap(), "0");
    assert_eq!(one(&mut tx, &trigger("scale_serial")).await.unwrap(), "0");
    let registry = tx.registry().clone();
    assert!(
        registry
            .model("scale.serial")
            .unwrap()
            .row_version
            .is_none()
    );
    assert!(registry.model("scale.quant").unwrap().row_version.is_some());
    let serials = tx.env().model::<ScaleSerial>().unwrap();
    let err = serials
        .expect_versions(&mut tx, [(RecId::from_real(1).unwrap(), 1)])
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "'scale.serial' has no row version (declare row_version = true)"
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_014_a_concurrent_bump_is_a_conflict_not_a_lost_update() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, created) = quants(&mut tx, 1).await;
    tx.commit().await.unwrap();
    let q = created.rec_ids()[0];
    let spec = h.registry().model("scale.quant").unwrap().clone();
    let orm = h.orm().clone();
    let attempts = AtomicUsize::new(0);
    let errors: Mutex<Vec<(ErrorKind, Option<String>)>> = Mutex::new(Vec::new());
    let err = h
        .run(async |tx| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            let rec = tx.env().model::<ScaleQuant>()?.browse([q.get()])?;
            // A read the record at version 1
            rec.expect_versions(tx, [(q, 1)]).await?;
            rec.get(tx, ScaleQuant::NOTE).await?;
            if attempt == 1 {
                // B writes and commits it (version 2) before A flushes
                let mut b = orm.begin(SUPERUSER_ID).await?;
                b.env()
                    .model::<ScaleQuant>()?
                    .browse([q.get()])?
                    .write(&mut b, note("B"))
                    .await?;
                b.commit().await?;
            }
            rec.write(tx, note("A")).await?;
            let result = tx.flush().await;
            if let Err(e) = &result {
                errors
                    .lock()
                    .unwrap()
                    .push((e.kind(), e.sqlstate().map(str::to_owned)));
            }
            result
        })
        .await
        .unwrap_err();
    let erp_testkit::HarnessError::Orm(err) = err else {
        panic!("unexpected harness error: {err}");
    };
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        *errors.lock().unwrap(),
        vec![
            (ErrorKind::Concurrency, Some("40001".to_owned())),
            (ErrorKind::Conflict, None)
        ]
    );
    assert_eq!(err.kind(), ErrorKind::Conflict);
    assert_eq!(
        err.message(),
        format!(
            "Record '{}' ({}) {q} was changed by someone else since you read it (version 1, now \
             2). Reload it and apply your change again.",
            spec.description, spec.name
        )
    );
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            &format!("SELECT note FROM scale_quant WHERE id = {q}")
        )
        .await
        .as_deref(),
        Some("B")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Every batch the hook received.
static HOOKED: Mutex<Vec<WriteBatch>> = Mutex::new(Vec::new());

fn record_hook<'a>(_tx: &'a mut Transaction, batch: &'a WriteBatch) -> BoxFuture<'a, Result<()>> {
    let batch = batch.clone();
    Box::pin(async move {
        HOOKED
            .lock()
            .map_err(|_| erp_sdk::Error::programming("hook lock poisoned"))?
            .push(batch);
        Ok(())
    })
}

#[tokio::test]
async fn spec_ormx_015_the_flush_update_returns_old_values_for_hooks() {
    let h = Harness::builder([&base_users::ADDON, &scale::ADDON, &hooks::ADDON])
        .write_hook(WriteHook::new("scale.quant", &["quantity"], record_hook))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let (_, created) = quants(&mut tx, 100).await;
    tx.flush().await.unwrap();
    HOOKED.lock().unwrap().clear();
    let old: Vec<f64> = created.mapped(&mut tx, ScaleQuant::QUANTITY).await.unwrap();
    created
        .write(&mut tx, Vals::new().set(ScaleQuant::QUANTITY, 500.0))
        .await
        .unwrap();
    tx.capture_statements(true);
    // the write's own statement count: the UPDATE of quantity (its recompute of
    // quantity_double goes into the same statement)
    assert_query_count!(tx, >= 1, <= 1, { tx.flush().await.unwrap() });
    let hooked_sql = tx.captured().to_vec();
    tx.capture_statements(false);
    assert!(
        hooked_sql
            .iter()
            .any(|s| s.contains("RETURNING WITH (OLD AS")),
        "{hooked_sql:?}"
    );
    let batches = HOOKED.lock().unwrap().clone();
    assert_eq!(batches.len(), 1, "one call for the statement");
    let batch = &batches[0];
    assert_eq!(
        (batch.model(), batch.fields()),
        ("scale.quant", &["quantity"][..])
    );
    assert_eq!(batch.rows().len(), 100);
    let ids: Vec<RecId> = created.rec_ids().to_vec();
    for row in batch.rows() {
        let i = ids.iter().position(|id| *id == row.id()).unwrap();
        assert_eq!(
            row.values(),
            &[(Cell::Float(old[i]), Cell::Float(500.0))][..]
        );
    }
    // an upsert update reaches the hook too
    HOOKED.lock().unwrap().clear();
    let first = created.at(0).unwrap();
    let product = first
        .get(&mut tx, ScaleQuant::PRODUCT_ID)
        .await
        .unwrap()
        .unwrap();
    let location = first
        .get(&mut tx, ScaleQuant::LOCATION_ID)
        .await
        .unwrap()
        .unwrap();
    tx.env()
        .model::<ScaleQuant>()
        .unwrap()
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([Vals::new()
                    .set(ScaleQuant::PRODUCT_ID, product)
                    .set(ScaleQuant::LOCATION_ID, location)
                    .set(ScaleQuant::LOT_ID, None::<RecId>)
                    .set(ScaleQuant::QUANTITY, 1.0)])
                .accumulate(ScaleQuant::QUANTITY),
        )
        .await
        .unwrap();
    let batches = HOOKED.lock().unwrap().clone();
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0].rows()[0].values(),
        &[(Cell::Float(500.0), Cell::Float(501.0))][..]
    );
    // a model nobody hooks keeps the plain UPDATE
    let serial = tx
        .env()
        .model::<ScaleSerial>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(ScaleSerial::CODE, "S")
                    .set(ScaleSerial::NUMBER, 1),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    serial
        .write(&mut tx, Vals::new().set(ScaleSerial::HITS, 3))
        .await
        .unwrap();
    tx.capture_statements(true);
    tx.flush().await.unwrap();
    let plain = tx.captured().to_vec();
    tx.capture_statements(false);
    assert_eq!(plain.len(), 1, "{plain:?}");
    assert!(
        plain[0].starts_with("UPDATE \"scale_serial\" SET"),
        "{plain:?}"
    );
    assert!(
        !plain[0].contains("RETURNING") && !plain[0].contains("row_version"),
        "{plain:?}"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

fn failing_hook<'a>(_tx: &'a mut Transaction, batch: &'a WriteBatch) -> BoxFuture<'a, Result<()>> {
    let rows = batch.rows().len();
    Box::pin(async move {
        // not a Validation error: only the write path's own poisoning keeps it from committing
        Err(erp_sdk::Error::programming(format!(
            "the hook refused {rows} rows"
        )))
    })
}

/// A write hook that fails after the flush `UPDATE` wrote the rows: the error reaches the caller
/// and the transaction cannot commit (fail loudly, design note §4.4).
#[tokio::test]
async fn a_failing_write_hook_poisons_the_transaction() {
    let h = Harness::builder([&base_users::ADDON, &scale::ADDON])
        .write_hook(WriteHook::new("scale.quant", &["note"], failing_hook))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let (_, created) = quants(&mut tx, 2).await;
    tx.flush().await.unwrap();
    created.write(&mut tx, note("x")).await.unwrap();
    let err = tx.flush().await.unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Programming, "the hook refused 2 rows")
    );
    assert!(tx.is_poisoned());
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.message(), "the hook refused 2 rows");
    tx.rollback().await.unwrap();
    // nothing was committed
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM scale_quant WHERE note = 'x'"
        )
        .await
        .as_deref(),
        Some("0")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A later addon turning the row version on for `scale.lot`, which has rows by then.
static LATE_VERSION: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "test_scale_late_version",
    depends: &["test_scale_upsert"],
    exact_only: false,
};

static LOT_VERSION_DECL: erp_registry::ModelDecl =
    erp_registry::ModelDecl::extension("test_scale_late_version", "scale.lot", &["scale.lot"])
        .row_version(true);

/// The large-table rule (design note §11): the row version of an existing table above
/// `large_table_rows` needs no post-commit step. The column comes with PostgreSQL's fast default
/// (no rewrite, existing rows at version 1, the default dropped again) and the trigger takes a
/// short lock without a scan, both in the schema transaction.
#[tokio::test]
async fn row_version_added_to_a_large_table_needs_no_step() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO scale_lot (name) SELECT 'lot ' || g FROM generate_series(1, 11) g",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let registry = erp_registry::RegistryBuilder::new([
        &base_users::ADDON,
        &scale::ADDON,
        &hooks::ADDON,
        &LATE_VERSION,
    ])
    .with_decls([&LOT_VERSION_DECL])
    .build()
    .unwrap();
    let orm = erp_orm::Orm::builder(std::sync::Arc::new(registry), h.db().pool().clone())
        .policy(erp_orm::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let sync = tx
        .init_models_with(&[], &erp_orm::SchemaSyncOptions::new().large_table_rows(10))
        .await
        .unwrap();
    assert!(sync.steps().is_empty(), "{:?}", sync.steps());
    let ddl = tx.captured().join(";\n");
    for part in [
        "ALTER TABLE \"scale_lot\" ADD COLUMN \"row_version\" int8 DEFAULT 1::int8 NOT NULL",
        "ALTER TABLE \"scale_lot\" ALTER COLUMN \"row_version\" DROP DEFAULT",
        erp_sql::version::bump_trigger("scale_lot", false)[0].as_str(),
    ] {
        assert!(ddl.contains(part), "{part} missing from {ddl}");
    }
    tx.capture_statements(false);
    tx.commit().await.unwrap();
    let versions = |rows: Vec<Vec<Option<String>>>| -> Vec<String> {
        rows.into_iter().map(|r| r[0].clone().unwrap()).collect()
    };
    assert_eq!(
        versions(
            support::rows(
                &mut tx,
                "SELECT DISTINCT row_version::text FROM scale_lot ORDER BY 1"
            )
            .await
        ),
        vec!["1".to_owned()]
    );
    tx.execute(
        "UPDATE scale_lot SET name = name || '!' WHERE name = 'lot 1'",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT row_version::text FROM scale_lot WHERE name = 'lot 1!'"
        )
        .await
        .as_deref(),
        Some("2")
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// Review SF-M1: a flush `UPDATE` with expected versions that skips a row has already written
/// the others, so the `Conflict` poisons the transaction: a caller that catches it cannot commit
/// the partial write.
#[tokio::test]
async fn a_conflict_found_by_the_flush_poisons_the_transaction() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, created) = quants(&mut tx, 2).await;
    tx.commit().await.unwrap();
    let (q1, q2) = (created.rec_ids()[0], created.rec_ids()[1]);
    let mut tx = h.begin().await.unwrap();
    // someone bumps Q1 after it was read; Q2's expectation holds
    tx.execute(
        "UPDATE scale_quant SET quantity = 9 WHERE id = $1",
        &[&q1.get()],
    )
    .await
    .unwrap();
    let both = tx
        .env()
        .model::<ScaleQuant>()
        .unwrap()
        .browse([q1.get(), q2.get()])
        .unwrap();
    both.expect_versions(&mut tx, [(q1, 1), (q2, 1)])
        .await
        .unwrap();
    both.write(&mut tx, note("partial")).await.unwrap();
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Conflict);
    assert!(tx.is_poisoned());
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(
        (commit.kind(), commit.message()),
        (ErrorKind::Conflict, err.message())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review DB-M1: an upsert does not check pending expectations (its own version bump would make
/// them fail later), so it refuses while the model has any, naming `expect_versions`.
#[tokio::test]
async fn an_upsert_refuses_while_expectations_are_pending() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (location, created) = quants(&mut tx, 1).await;
    let q1 = created.rec_ids()[0];
    let product: i64 = one(
        &mut tx,
        &format!("SELECT product_id::text FROM scale_quant WHERE id = {q1}"),
    )
    .await
    .unwrap()
    .parse()
    .unwrap();
    tx.flush().await.unwrap();
    let key = || {
        Vals::<ScaleQuant>::new()
            .set(ScaleQuant::PRODUCT_ID, RecId::from_real(product).unwrap())
            .set(ScaleQuant::LOCATION_ID, location)
            .set(ScaleQuant::LOT_ID, None::<RecId>)
    };
    let model = tx.env().model::<ScaleQuant>().unwrap();
    let one_quant = model.browse([q1.get()]).unwrap();
    one_quant.expect_versions(&mut tx, [(q1, 1)]).await.unwrap();
    let err = model
        .upsert(&mut tx, Upsert::on(ScaleQuant::QUANT_KEY).rows([key()]))
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "upsert on 'scale.quant': expect_versions expectations are pending for this model; \
             write and flush the expected records before upserting (an upsert does not check them, \
             and its own version bump would make them fail later)"
        )
    );
    // consumed by the write that follows them, the upsert goes through
    one_quant.write(&mut tx, note("seen")).await.unwrap();
    tx.flush().await.unwrap();
    let out = model
        .upsert(&mut tx, Upsert::on(ScaleQuant::QUANT_KEY).rows([key()]))
        .await
        .unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Unchanged);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SF-S6: expectations recorded inside a savepoint that rolls back go with it.
#[tokio::test]
async fn expectations_of_a_rolled_back_savepoint_are_dropped() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, created) = quants(&mut tx, 1).await;
    tx.commit().await.unwrap();
    let q1 = created.rec_ids()[0];
    let mut tx = h.begin().await.unwrap();
    // the record moved on (version 2); an expectation of version 1 would be stale
    tx.execute(
        "UPDATE scale_quant SET quantity = 9 WHERE id = $1",
        &[&q1.get()],
    )
    .await
    .unwrap();
    let model = tx.env().model::<ScaleQuant>().unwrap();
    let one_quant = model.browse([q1.get()]).unwrap();
    let err = tx
        .savepoint(async |tx| {
            one_quant.expect_versions(tx, [(q1, 1)]).await?;
            Err::<(), _>(erp_sdk::Error::validation("undo"))
        })
        .await
        .unwrap_err();
    assert_eq!(err.message(), "undo");
    one_quant.write(&mut tx, note("fresh")).await.unwrap();
    tx.flush().await.unwrap();
    assert_eq!(stored_version(&mut tx, q1).await, 3);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SF-S5: a write hook that fails after an upsert updated rows poisons the transaction,
/// whatever the caller does with the error.
#[tokio::test]
async fn a_failing_write_hook_after_an_upsert_poisons_the_transaction() {
    let h = Harness::builder([&base_users::ADDON, &scale::ADDON])
        .write_hook(WriteHook::new("scale.quant", &["note"], failing_hook))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let (location, created) = quants(&mut tx, 1).await;
    let q1 = created.rec_ids()[0];
    let product: i64 = one(
        &mut tx,
        &format!("SELECT product_id::text FROM scale_quant WHERE id = {q1}"),
    )
    .await
    .unwrap()
    .parse()
    .unwrap();
    tx.flush().await.unwrap();
    let err = tx
        .env()
        .model::<ScaleQuant>()
        .unwrap()
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([Vals::<ScaleQuant>::new()
                    .set(ScaleQuant::PRODUCT_ID, RecId::from_real(product).unwrap())
                    .set(ScaleQuant::LOCATION_ID, location)
                    .set(ScaleQuant::LOT_ID, None::<RecId>)
                    .set(ScaleQuant::NOTE, "x")])
                .update([ScaleQuant::NOTE]),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Programming, "the hook refused 1 rows")
    );
    assert!(tx.is_poisoned());
    let commit = tx.commit().await.unwrap_err();
    assert_eq!(commit.message(), "the hook refused 1 rows");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review DB-S4: the row version keeps a database default of 1, so a raw `INSERT` that leaves
/// the column out works; it is the one ORM column with a default.
#[tokio::test]
async fn the_row_version_keeps_its_database_default() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let default = one(
        &mut tx,
        "SELECT column_default FROM information_schema.columns WHERE table_schema = \
         current_schema() AND table_name = 'scale_quant' AND column_name = 'row_version'",
    )
    .await;
    assert_eq!(default.as_deref(), Some("1"));
    let (location, _) = quants(&mut tx, 1).await;
    let product = tx
        .env()
        .model::<ScaleProduct>()
        .unwrap()
        .create(
            &mut tx,
            vec![Vals::new().set(ScaleProduct::NAME, "raw insert")],
        )
        .await
        .unwrap()
        .rec_ids()[0];
    tx.flush().await.unwrap();
    let version = one(
        &mut tx,
        &format!(
            "INSERT INTO scale_quant (product_id, location_id) VALUES ({product}, {location}) \
             RETURNING row_version::text"
        ),
    )
    .await;
    assert_eq!(version.as_deref(), Some("1"));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SF-N1 (count path): a model without a row version has no row-returning flush; a row
/// deleted behind the ORM's back makes the `UPDATE` short, which poisons the transaction because
/// the rest of the chunk was written.
#[tokio::test]
async fn a_short_update_without_versions_poisons_the_transaction() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let serials = tx.env().model::<ScaleSerial>().unwrap();
    let created = serials
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(ScaleSerial::CODE, "A")
                    .set(ScaleSerial::NUMBER, 1),
                Vals::new()
                    .set(ScaleSerial::CODE, "B")
                    .set(ScaleSerial::NUMBER, 2),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let (a, b) = (created.rec_ids()[0], created.rec_ids()[1]);
    tx.execute("DELETE FROM scale_serial WHERE id = $1", &[&b.get()])
        .await
        .unwrap();
    created
        .write(&mut tx, Vals::new().set(ScaleSerial::HITS, 7))
        .await
        .unwrap();
    let err = tx.flush().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Missing);
    assert!(tx.is_poisoned(), "A was written, B was not: {a} / {b}");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review SF-N2: with one key per statement, a failure in the second statement poisons the
/// transaction, because the first is written.
#[tokio::test]
#[allow(
    clippy::field_reassign_with_default,
    reason = "OrmConfig is non_exhaustive"
)]
async fn a_failing_second_chunk_poisons_the_transaction() {
    // OrmConfig is #[non_exhaustive]: a struct literal is not allowed outside its crate
    let mut config = erp_orm::OrmConfig::default();
    config.insert_batch = 1;
    let h = Harness::builder([&base_users::ADDON, &scale::ADDON])
        .config(config)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let (location, created) = quants(&mut tx, 2).await;
    tx.flush().await.unwrap();
    // product ids 1 and 2 on a fresh database: the keys sort 1 before 2
    let products: Vec<i64> = tx
        .query(
            "SELECT product_id FROM scale_quant ORDER BY product_id",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    tx.execute(
        "UPDATE scale_quant SET move_count = 2147483647 WHERE product_id = $1",
        &[&products[1]],
    )
    .await
    .unwrap();
    let _ = created;
    let row = |p: i64| {
        Vals::<ScaleQuant>::new()
            .set(ScaleQuant::PRODUCT_ID, RecId::from_real(p).unwrap())
            .set(ScaleQuant::LOCATION_ID, location)
            .set(ScaleQuant::LOT_ID, None::<RecId>)
            .set(ScaleQuant::MOVE_COUNT, 1)
    };
    let err = tx
        .env()
        .model::<ScaleQuant>()
        .unwrap()
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([row(products[0]), row(products[1])])
                .accumulate(ScaleQuant::MOVE_COUNT),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(err.sqlstate(), Some("22003"));
    assert!(tx.is_poisoned());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
