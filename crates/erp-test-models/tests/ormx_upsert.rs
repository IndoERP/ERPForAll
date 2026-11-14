//! Business keys and upsert (
//! ). The access cases run on the real
//! access engine in `ormx_upsert_access.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use erp_registry::{FieldDecl, KeyDecl, KindDecl, ModelDecl, TableObjectDecl, TableObjectKind};
use erp_sdk::{
    Cell, DateTime, Error, ErrorKind, RawVals, RecId, RecordSet, Result, SUPERUSER_ID, Transaction,
    Upsert, UpsertCall, UpsertOutcome, Upserted, Vals, Value,
};
use erp_test_models::base_users;
use erp_test_models::test_scale_upsert::{
    self as scale, ScaleCompany, ScaleLocation, ScaleLot, ScaleProduct, ScaleQuant, ScaleSerial,
};
use erp_testkit::{Harness, assert_query_count};
use support::{map, one};

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &scale::ADDON])
        .build()
        .await
        .unwrap()
}

/// A location in each of two companies, `n` products and two lots (the second with a non-ASCII
/// name).
struct World {
    locations: Vec<RecId>,
    products: Vec<RecId>,
    lots: Vec<RecId>,
}

const NON_ASCII_LOT: &str = "Lötnummer-批次-№7";

async fn world(tx: &mut Transaction, products: usize) -> World {
    let env = tx.env();
    let companies = env
        .model::<ScaleCompany>()
        .unwrap()
        .create(
            tx,
            ["C1", "C2"]
                .into_iter()
                .map(|n| Vals::new().set(ScaleCompany::NAME, n))
                .collect(),
        )
        .await
        .unwrap()
        .rec_ids()
        .to_vec();
    let locations = env
        .model::<ScaleLocation>()
        .unwrap()
        .create(
            tx,
            companies
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    Vals::new()
                        .set(ScaleLocation::NAME, format!("L{i}"))
                        .set(ScaleLocation::COMPANY_ID, *c)
                })
                .collect(),
        )
        .await
        .unwrap()
        .rec_ids()
        .to_vec();
    let products = env
        .model::<ScaleProduct>()
        .unwrap()
        .create(
            tx,
            (0..products)
                .map(|i| Vals::new().set(ScaleProduct::NAME, format!("P{i}")))
                .collect(),
        )
        .await
        .unwrap()
        .rec_ids()
        .to_vec();
    let lots = env
        .model::<ScaleLot>()
        .unwrap()
        .create(
            tx,
            ["LOT-1", NON_ASCII_LOT]
                .into_iter()
                .map(|n| Vals::new().set(ScaleLot::NAME, n))
                .collect(),
        )
        .await
        .unwrap()
        .rec_ids()
        .to_vec();
    World {
        locations,
        products,
        lots,
    }
}

fn key_row(product: RecId, location: RecId, lot: Option<RecId>) -> Vals<ScaleQuant> {
    Vals::new()
        .set(ScaleQuant::PRODUCT_ID, product)
        .set(ScaleQuant::LOCATION_ID, location)
        .set(ScaleQuant::LOT_ID, lot)
}

fn quants(tx: &Transaction) -> RecordSet<ScaleQuant> {
    tx.env().model::<ScaleQuant>().unwrap()
}

async fn count(tx: &mut Transaction, sql: &str) -> i64 {
    one(tx, sql).await.unwrap().parse().unwrap()
}

/// The stored `(quantity, row_version)` of a quant.
async fn stored(tx: &mut Transaction, id: RecId) -> (f64, i64) {
    let rows = tx
        .query(
            "SELECT quantity, row_version FROM scale_quant WHERE id = $1",
            &[&id.get()],
        )
        .await
        .unwrap();
    let row = rows.first().expect("the quant exists");
    (row.get::<_, Option<f64>>(0).unwrap_or_default(), row.get(1))
}

fn quant_key(tx: &Transaction) -> erp_registry::BusinessKey {
    tx.registry()
        .model("scale.quant")
        .unwrap()
        .business_key("_quant_key")
        .unwrap()
        .clone()
}

#[tokio::test]
async fn spec_ormx_001_new_keys_insert_in_one_statement() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 1000).await;
    tx.flush().await.unwrap();
    let quantities: Vec<f64> = (0..1000).map(|i| f64::from(i) * 0.5 + 1.0).collect();
    let rows: Vec<Vals<ScaleQuant>> = (0..1000)
        .map(|i| {
            let lot = (i == 7).then_some(w.lots[1]);
            key_row(w.products[i], w.locations[0], lot).set(ScaleQuant::QUANTITY, quantities[i])
        })
        .collect();
    let model = quants(&tx);
    tx.capture_statements(true);
    let out: Upserted<ScaleQuant> = assert_query_count!(tx, >= 1, <= 1, {
        model
            .upsert(
                &mut tx,
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows(rows)
                    .accumulate(ScaleQuant::QUANTITY),
            )
            .await
            .unwrap()
    });
    // the statement's structure (design note §3.3), arbiter named from the registry
    let sql = tx.captured().last().unwrap().clone();
    tx.capture_statements(false);
    let constraint = format!("ON CONFLICT ON CONSTRAINT \"{}\"", quant_key(&tx).full_name);
    for part in [
        "WITH ORDINALITY",
        constraint.as_str(),
        "DO UPDATE SET",
        "RETURNING WITH (OLD AS",
        "UNION ALL",
        "NOT EXISTS",
    ] {
        assert!(sql.contains(part), "{part} missing from {sql}");
    }
    // 1,000 entries in input order, all inserted at version 1, ids as stored
    assert_eq!(out.rows().len(), 1000);
    let stored_ids = tx
        .query(
            "SELECT product_id, id, quantity, lot_id FROM scale_quant",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(stored_ids.len(), 1000);
    let by_product: std::collections::HashMap<i64, (i64, f64, Option<i64>)> = stored_ids
        .iter()
        .map(|r| (r.get(0), (r.get(1), r.get(2), r.get(3))))
        .collect();
    for (i, row) in out.rows().iter().enumerate() {
        assert_eq!(row.outcome(), UpsertOutcome::Inserted, "row {i}");
        assert_eq!(row.previous_version(), None, "row {i}");
        assert_eq!(row.version(), Some(1), "row {i}");
        let (id, quantity, lot) = by_product[&w.products[i].get()];
        assert_eq!(row.id().get(), id, "row {i}");
        assert_eq!(quantity, quantities[i], "row {i}");
        assert_eq!(lot, (i == 7).then_some(w.lots[1].get()), "row {i}");
    }
    // the non-ASCII lot round-trips through its record
    let lot_name = tx
        .env()
        .model::<ScaleLot>()
        .unwrap()
        .browse([w.lots[1].get()])
        .unwrap()
        .get(&mut tx, ScaleLot::NAME)
        .await
        .unwrap();
    assert_eq!(lot_name.as_deref(), Some(NON_ASCII_LOT));
    assert_eq!(out.records().len(), 1000);
    // an empty batch sends nothing and returns nothing
    let empty = assert_query_count!(tx, <= 0, {
        model
            .upsert(&mut tx, Upsert::on(ScaleQuant::QUANT_KEY))
            .await
            .unwrap()
    });
    assert!(empty.rows().is_empty());
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_002_existing_keys_update_only_what_differs() {
    let h = harness().await;
    let d1 = DateTime::parse_odoo("2026-01-05 08:00:00").unwrap();
    let d2 = DateTime::parse_odoo("2026-02-07 09:30:00").unwrap();
    let (w, ids) = h
        .run(async |tx| {
            let w = world(tx, 3).await;
            let rows: Vec<Vals<ScaleQuant>> = (0..3)
                .map(|i| key_row(w.products[i], w.locations[0], None).set(ScaleQuant::IN_DATE, d1))
                .collect();
            let out = quants(tx)
                .upsert(tx, Upsert::on(ScaleQuant::QUANT_KEY).rows(rows))
                .await?;
            Ok((w, out.into_result().ids()))
        })
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let written_before: Vec<Option<String>> = {
        let mut out = Vec::new();
        for id in &ids {
            out.push(
                one(
                    &mut tx,
                    &format!("SELECT write_date::text FROM scale_quant WHERE id = {id}"),
                )
                .await,
            );
        }
        out
    };
    let rows: Vec<Vals<ScaleQuant>> = (0..3)
        .map(|i| {
            let date = if i == 0 { d2 } else { d1 };
            key_row(w.products[i], w.locations[0], None).set(ScaleQuant::IN_DATE, date)
        })
        .collect();
    let model = quants(&tx);
    let out = assert_query_count!(tx, >= 1, <= 1, {
        model
            .upsert(
                &mut tx,
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows(rows)
                    .update([ScaleQuant::IN_DATE]),
            )
            .await
            .unwrap()
    });
    let r = out.rows();
    assert_eq!(
        (r[0].outcome(), r[0].previous_version(), r[0].version()),
        (UpsertOutcome::Updated, Some(1), Some(2))
    );
    for row in &r[1..] {
        assert_eq!(
            (row.outcome(), row.previous_version(), row.version()),
            (UpsertOutcome::Unchanged, Some(1), Some(1))
        );
    }
    assert_eq!(out.into_result().ids(), ids);
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    for (i, id) in ids.iter().enumerate() {
        let (_, version) = stored(&mut tx, *id).await;
        let write_date = one(
            &mut tx,
            &format!("SELECT write_date::text FROM scale_quant WHERE id = {id}"),
        )
        .await;
        let in_date = quants(&tx)
            .browse([id.get()])
            .unwrap()
            .get(&mut tx, ScaleQuant::IN_DATE)
            .await
            .unwrap();
        let expected_date = if i == 0 { d2 } else { d1 };
        assert_eq!(in_date, Some(expected_date));
        if i == 0 {
            assert_eq!(version, 2);
            assert_ne!(write_date, written_before[i], "the updated row was written");
        } else {
            assert_eq!(version, 1);
            assert_eq!(
                write_date, written_before[i],
                "unchanged rows keep write_date"
            );
        }
    }
    // get-or-create on existing keys: every row unchanged, nothing written
    let rows: Vec<Vals<ScaleQuant>> = (0..3)
        .map(|i| key_row(w.products[i], w.locations[0], None))
        .collect();
    let out = quants(&tx)
        .upsert(&mut tx, Upsert::on(ScaleQuant::QUANT_KEY).rows(rows))
        .await
        .unwrap();
    assert!(
        out.rows()
            .iter()
            .all(|r| r.outcome() == UpsertOutcome::Unchanged)
    );
    assert_eq!(out.into_result().ids(), ids);
    let versions: Vec<i64> = {
        let mut v = Vec::new();
        for id in &ids {
            v.push(stored(&mut tx, *id).await.1);
        }
        v
    };
    assert_eq!(versions, vec![2, 1, 1]);
    tx.commit().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// One `+delta` on (product, location, no lot), through `upsert`.
async fn add(
    tx: &mut Transaction,
    product: RecId,
    location: RecId,
    delta: f64,
) -> Result<Upserted<ScaleQuant>> {
    quants(tx)
        .upsert(
            tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key_row(product, location, None).set(ScaleQuant::QUANTITY, delta)])
                .accumulate(ScaleQuant::QUANTITY),
        )
        .await
}

#[tokio::test]
async fn spec_ormx_003_accumulate_is_atomic_under_concurrency() {
    let h = harness().await;
    let w = h
        .run(async |tx| {
            let w = world(tx, 2).await;
            add(tx, w.products[0], w.locations[0], 0.0).await?;
            Ok(w)
        })
        .await
        .unwrap();
    let (product, location) = (w.products[0], w.locations[0]);

    // an existing quant: a concurrent +5 commits after this unit of work took its snapshot; its
    // own +5 hits 40001 and the unit of work is retried
    for (first, second) in [(5.0, 5.0), (5.0, -7.0)] {
        let reset = h
            .run(async |tx| {
                tx.execute(
                    "UPDATE scale_quant SET quantity = 0 WHERE product_id = $1",
                    &[&product.get()],
                )
                .await
            })
            .await
            .unwrap();
        assert_eq!(reset, 1);
        let attempts = AtomicUsize::new(0);
        let orm = h.orm().clone();
        let theirs: Mutex<Option<UpsertOutcome>> = Mutex::new(None);
        let ours = h
            .run(async |tx| {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                tx.query("SELECT 1", &[]).await?; // takes the snapshot
                if attempt == 1 {
                    let mut other = orm.begin(SUPERUSER_ID).await?;
                    let out = add(&mut other, product, location, first).await?;
                    *theirs.lock().unwrap() = Some(out.rows()[0].outcome());
                    other.commit().await?;
                }
                let out = add(tx, product, location, second).await?;
                Ok(out.rows()[0].outcome())
            })
            .await
            .unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "40001, then a retry");
        assert_eq!(*theirs.lock().unwrap(), Some(UpsertOutcome::Updated));
        assert_eq!(ours, UpsertOutcome::Updated);
        let mut tx = h.begin().await.unwrap();
        let rows = tx
            .query(
                "SELECT quantity FROM scale_quant WHERE product_id = $1",
                &[&product.get()],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "one quant per key");
        assert_eq!(rows[0].get::<_, f64>(0), first + second);
        tx.rollback().await.unwrap();
    }

    // a new key inserted by two transactions at once: the second waits for the first's
    // uncommitted row, then fails with 40001 (the row is not in its snapshot); the retried unit
    // of work updates it
    let new_product = w.products[1];
    let (mut a, _) = h.begin_pair().await.unwrap();
    let a_out = add(&mut a, new_product, location, 5.0).await.unwrap();
    assert_eq!(a_out.rows()[0].outcome(), UpsertOutcome::Inserted);
    let a_slot = Mutex::new(Some(a));
    let attempts = AtomicUsize::new(0);
    let ours = h
        .run(async |tx| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt > 1 {
                return Ok(add(tx, new_product, location, 5.0).await?.rows()[0].outcome());
            }
            let pid = erp_testkit::backend_pid(tx)
                .await
                .map_err(|e| Error::programming(e.to_string()))?;
            let mut a = a_slot.lock().unwrap().take().unwrap();
            let (ours, committed) = tokio::join!(add(tx, new_product, location, 5.0), async {
                erp_testkit::wait_until_blocked(&h, pid, Duration::from_secs(20))
                    .await
                    .map_err(|e| Error::programming(e.to_string()))?;
                a.commit().await
            });
            committed?;
            let err = ours.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Concurrency);
            assert_eq!(err.sqlstate(), Some("40001"));
            Err(err)
        })
        .await
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(ours, UpsertOutcome::Updated);
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT quantity FROM scale_quant WHERE product_id = $1",
            &[&new_product.get()],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "no duplicate quant");
    assert_eq!(rows[0].get::<_, f64>(0), 10.0);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_004_accumulate_floor_never_stores_below_the_floor() {
    let h = harness().await;
    let w = h
        .run(async |tx| {
            let w = world(tx, 4).await;
            let l = w.locations[0];
            quants(tx)
                .upsert(
                    tx,
                    Upsert::on(ScaleQuant::QUANT_KEY).rows([
                        key_row(w.products[0], l, None).set(ScaleQuant::RESERVED_QUANTITY, 3.0),
                        key_row(w.products[1], l, None).set(ScaleQuant::RESERVED_QUANTITY, 3.0),
                        key_row(w.products[2], l, None).set(ScaleQuant::MOVE_COUNT, i32::MAX),
                    ]),
                )
                .await?;
            Ok(w)
        })
        .await
        .unwrap();
    let l = w.locations[0];
    let spec = h.registry().model("scale.quant").unwrap().clone();

    // -5 on 3 stores GREATEST(0, 3 - 5); -2 on 3 stores 1
    let mut tx = h.begin().await.unwrap();
    let model = quants(&tx);
    let out = model
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([
                    key_row(w.products[0], l, None).set(ScaleQuant::RESERVED_QUANTITY, -5.0),
                    key_row(w.products[1], l, None).set(ScaleQuant::RESERVED_QUANTITY, -2.0),
                ])
                .accumulate_floor(ScaleQuant::RESERVED_QUANTITY, 0.0),
        )
        .await
        .unwrap();
    assert!(
        out.rows()
            .iter()
            .all(|r| r.outcome() == UpsertOutcome::Updated)
    );
    let expected = vec![(3.0f64 - 5.0).max(0.0), (3.0f64 - 2.0).max(0.0)];
    let cached = out
        .records()
        .mapped(&mut tx, ScaleQuant::RESERVED_QUANTITY)
        .await
        .unwrap();
    assert_eq!(cached, expected);
    tx.invalidate_all().await.unwrap();
    let stored_values = out
        .records()
        .mapped(&mut tx, ScaleQuant::RESERVED_QUANTITY)
        .await
        .unwrap();
    assert_eq!(stored_values, expected);
    tx.rollback().await.unwrap();

    // an integer whose accumulated value leaves int4: Validation naming the field
    let field = spec.field("move_count").unwrap();
    let message = format!(
        "The database refused the change: the new value of '{}' ({}) on '{}' is out of range",
        field.string, field.name, spec.description
    );
    // ... in the batch (two rows of one key): refused before any statement
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let err = assert_query_count!(tx, <= 0, {
        model
            .upsert(
                &mut tx,
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows([
                        key_row(w.products[3], l, None).set(ScaleQuant::MOVE_COUNT, i32::MAX),
                        key_row(w.products[3], l, None).set(ScaleQuant::MOVE_COUNT, 1),
                    ])
                    .accumulate(ScaleQuant::MOVE_COUNT),
            )
            .await
            .unwrap_err()
    });
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Validation, message.as_str())
    );
    tx.rollback().await.unwrap();
    // ... in the database (`i32::MAX` stored, `+ 1`: SQLSTATE 22003)
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let err = model
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key_row(w.products[2], l, None).set(ScaleQuant::MOVE_COUNT, 1)])
                .accumulate(ScaleQuant::MOVE_COUNT),
        )
        .await
        .unwrap_err();
    // the model has a row version: `row_version + 1` could overflow as well, and the server does
    // not say which number did, so the error names the candidates instead of blaming one
    assert_eq!(
        (err.kind(), err.message(), err.sqlstate()),
        (
            ErrorKind::Database,
            format!(
                "a number of the upsert on '{}' is out of range in the database: the accumulated \
                 '{}' ({}) or the row version (row_version); the statement does not say which",
                spec.description, field.string, field.name
            )
            .as_str(),
            Some("22003")
        )
    );
    tx.rollback().await.unwrap();
    // without a row version the accumulated field is the only candidate: Validation naming it
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let serials = tx.env().model::<ScaleSerial>().unwrap();
    let serial_row = |hits: i32| {
        Vals::<ScaleSerial>::new()
            .set(ScaleSerial::CODE, "O")
            .set(ScaleSerial::NUMBER, 1)
            .set(ScaleSerial::HITS, hits)
    };
    serials
        .upsert(
            &mut tx,
            Upsert::on(ScaleSerial::CODE_KEY).rows([serial_row(i32::MAX)]),
        )
        .await
        .unwrap();
    let err = serials
        .upsert(
            &mut tx,
            Upsert::on(ScaleSerial::CODE_KEY)
                .rows([serial_row(1)])
                .accumulate(ScaleSerial::HITS),
        )
        .await
        .unwrap_err();
    let serial_spec = h.registry().model("scale.serial").unwrap();
    let hits = serial_spec.field("hits").unwrap();
    assert_eq!(
        (err.kind(), err.message(), err.sqlstate()),
        (
            ErrorKind::Validation,
            format!(
                "The database refused the change: the new value of '{}' ({}) on '{}' is out of \
                 range",
                hits.string, hits.name, serial_spec.description
            )
            .as_str(),
            Some("22003")
        )
    );
    tx.rollback().await.unwrap();
    // a negative delta on an existing row is no overflow
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let out = model
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key_row(w.products[2], l, None).set(ScaleQuant::MOVE_COUNT, -1)])
                .accumulate(ScaleQuant::MOVE_COUNT),
        )
        .await
        .unwrap();
    assert_eq!(
        out.records()
            .get(&mut tx, ScaleQuant::MOVE_COUNT)
            .await
            .unwrap(),
        i32::MAX - 1
    );
    tx.rollback().await.unwrap();

    // a new row below the floor fails the call: nothing can be committed
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let err = model
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key_row(w.products[3], l, None).set(ScaleQuant::RESERVED_QUANTITY, -1.0)])
                .accumulate_floor(ScaleQuant::RESERVED_QUANTITY, 0.0),
        )
        .await
        .unwrap_err();
    let reserved = spec.field("reserved_quantity").unwrap();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The database refused the change: a new '{}' record would start with '{}' ({}) = \
             {} below its floor {}",
            spec.description,
            reserved.string,
            reserved.name,
            Value::Float(-1.0),
            Value::Float(0.0)
        )
    );
    assert!(tx.is_poisoned());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_005_duplicate_keys_in_one_call_are_merged() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 999).await;
    tx.flush().await.unwrap();
    let l = w.locations[0];
    let date = |i: i64| DateTime::from_micros(1_700_000_000_000_000 + i * 1_000_000);
    // 1,000 rows of which 2 share a key: rows 10 and 20 (product 10)
    let product_of = |i: usize| {
        if i == 20 {
            w.products[10]
        } else {
            w.products[i.min(998)]
        }
    };
    let rows: Vec<Vals<ScaleQuant>> = (0..1000usize)
        .map(|i| {
            key_row(product_of(i), l, None)
                .set(ScaleQuant::QUANTITY, i as f64)
                .set(ScaleQuant::IN_DATE, date(i as i64))
        })
        .collect();
    // rows 998 and 999 both use product 998 too: the 1,000 rows hold 998 distinct keys
    let model = quants(&tx);
    let out = assert_query_count!(tx, >= 1, <= 1, {
        model
            .upsert(
                &mut tx,
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows(rows)
                    .update([ScaleQuant::IN_DATE])
                    .accumulate(ScaleQuant::QUANTITY),
            )
            .await
            .unwrap()
    });
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM scale_quant").await,
        998
    );
    let r = out.rows();
    assert_eq!(r.len(), 1000);
    assert_eq!(
        r[10].id(),
        r[20].id(),
        "duplicates map to the merged record"
    );
    assert_eq!(r[998].id(), r[999].id());
    assert!(r.iter().all(|row| row.outcome() == UpsertOutcome::Inserted));
    let merged = model.browse([r[10].id().get()]).unwrap();
    assert_eq!(
        merged.get(&mut tx, ScaleQuant::QUANTITY).await.unwrap(),
        10.0 + 20.0
    );
    assert_eq!(
        merged.get(&mut tx, ScaleQuant::IN_DATE).await.unwrap(),
        Some(date(20)),
        "the last duplicate's in_date"
    );
    tx.invalidate_all().await.unwrap();
    let (quantity, _) = stored(&mut tx, r[998].id()).await;
    assert_eq!(quantity, 998.0 + 999.0);

    // 1,000 rows of one key on an existing quant: one row, summed, last in_date
    let rows: Vec<Vals<ScaleQuant>> = (0..1000)
        .map(|i| {
            key_row(w.products[0], l, None)
                .set(ScaleQuant::QUANTITY, 1.0)
                .set(ScaleQuant::IN_DATE, date(5000 + i))
        })
        .collect();
    let out = assert_query_count!(tx, >= 1, <= 1, {
        model
            .upsert(
                &mut tx,
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows(rows)
                    .update([ScaleQuant::IN_DATE])
                    .accumulate(ScaleQuant::QUANTITY),
            )
            .await
            .unwrap()
    });
    let first = out.rows()[0];
    assert!(out.rows().iter().all(|row| *row == first));
    assert_eq!(first.outcome(), UpsertOutcome::Updated);
    tx.invalidate_all().await.unwrap();
    let (quantity, version) = stored(&mut tx, first.id()).await;
    assert_eq!(quantity, 0.0 + 1000.0);
    assert_eq!(version, 2);
    let rec = model.browse([first.id().get()]).unwrap();
    assert_eq!(
        rec.get(&mut tx, ScaleQuant::IN_DATE).await.unwrap(),
        Some(date(5000 + 999))
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// An addon that redeclares `_quant_key` with `nulls_distinct = true`.
static NULLS_DISTINCT: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "test_scale_nulls_distinct",
    depends: &["test_scale_upsert"],
    exact_only: false,
};

/// `scale.quant` with `_quant_key` declared `nulls_distinct = true` by a later declaration.
static NULLS_DISTINCT_KEY: [TableObjectDecl; 1] = [TableObjectDecl {
    name: "_quant_key",
    kind: TableObjectKind::Key(
        KeyDecl::new(
            &["product_id", "location_id", "lot_id"],
            "A quant already exists for this product, location and lot.",
        )
        .nulls_distinct(true),
    ),
}];

/// [`NULLS_DISTINCT_KEY`] as a declaration of the [`NULLS_DISTINCT`] addon.
const fn nulls_distinct_decl() -> ModelDecl {
    ModelDecl::extension("test_scale_nulls_distinct", "scale.quant", &["scale.quant"])
        .table_objects(&NULLS_DISTINCT_KEY)
}

static NULLS_DISTINCT_DECL: ModelDecl = nulls_distinct_decl();

#[tokio::test]
async fn spec_ormx_006_null_key_parts_conflict() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 1).await;
    let (p, l) = (w.products[0], w.locations[0]);
    let model = quants(&tx);
    let first = model
        .create(
            &mut tx,
            vec![key_row(p, l, None).set(ScaleQuant::QUANTITY, 2.0)],
        )
        .await
        .unwrap();
    let out = add(&mut tx, p, l, 1.0).await.unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Updated);
    assert_eq!(out.rows()[0].id(), first.rec_ids()[0]);
    assert_eq!(
        count(&mut tx, "SELECT count(*)::text FROM scale_quant").await,
        1
    );
    // a plain create of the same (P, L, NULL) violates the key
    let message = format!(
        "The database refused the change: {}",
        quant_key(&tx).message
    );
    let err = tx
        .savepoint(async |tx| {
            quants(tx)
                .create(tx, vec![key_row(p, l, None)])
                .await
                .map(|_| ())
        })
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Validation, message.as_str())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();

    // a key declared nulls_distinct cannot arbitrate, and still refuses duplicates without NULL
    let h = Harness::builder([&base_users::ADDON, &scale::ADDON, &NULLS_DISTINCT])
        .with_decls([nulls_distinct_decl()])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 1).await;
    let (p, l, lot) = (w.products[0], w.locations[0], w.lots[0]);
    let err = assert_query_count!(tx, <= 0, { add(&mut tx, p, l, 1.0).await.unwrap_err() });
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "upsert on 'scale.quant': key '_quant_key' cannot arbitrate an upsert (nulls_distinct)"
    );
    let model = quants(&tx);
    model
        .create(&mut tx, vec![key_row(p, l, None), key_row(p, l, None)])
        .await
        .unwrap();
    model
        .create(&mut tx, vec![key_row(p, l, Some(lot))])
        .await
        .unwrap();
    let err = tx
        .savepoint(async |tx| {
            quants(tx)
                .create(tx, vec![key_row(p, l, Some(lot))])
                .await
                .map(|_| ())
        })
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (ErrorKind::Validation, message.as_str())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A later addon adding the key `_name_key` to `scale.lot`, which has rows by then.
static LATE_KEY: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "test_scale_late_key",
    depends: &["test_scale_upsert"],
    exact_only: false,
};

static LOT_NAME_KEY: [TableObjectDecl; 1] = [TableObjectDecl {
    name: "_name_key",
    kind: TableObjectKind::Key(KeyDecl::new(
        &["name"],
        "A lot with this name exists already.",
    )),
}];

static LOT_NAME_KEY_DECL: ModelDecl =
    ModelDecl::extension("test_scale_late_key", "scale.lot", &["scale.lot"])
        .table_objects(&LOT_NAME_KEY);

/// An ORM on `h`'s database whose registry adds `decl` of `addon` to the harness's addons.
fn orm_with(
    h: &Harness,
    addon: &'static erp_sdk::AddonDescriptor,
    decl: &'static ModelDecl,
) -> erp_orm::Orm {
    let registry = erp_registry::RegistryBuilder::new([&base_users::ADDON, &scale::ADDON, addon])
        .with_decls([decl])
        .build()
        .unwrap();
    erp_orm::Orm::builder(std::sync::Arc::new(registry), h.db().pool().clone())
        .policy(erp_orm::StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

/// Statements among `captured` that change a table, an index or a constraint's comment.
fn table_ddl(captured: &[String]) -> Vec<String> {
    captured
        .iter()
        .filter(|s| {
            s.contains("ALTER TABLE") || s.contains("INDEX") || s.contains("COMMENT ON CONSTRAINT")
        })
        .cloned()
        .collect()
}

/// The stored definition (comment) of constraint `name`, when the constraint exists.
async fn constraint_comment(tx: &mut Transaction, name: &str) -> Option<Option<String>> {
    let rows = tx
        .query(
            "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint WHERE conname = $1",
            &[&name],
        )
        .await
        .unwrap();
    rows.first().map(|r| r.get(0))
}

/// The large-table rule of schema sync (design note §11): a business key added to an
/// existing table with more rows than `large_table_rows` puts nothing in the transaction (its
/// `ADD CONSTRAINT` would build the unique index under `ACCESS EXCLUSIVE` for the whole scan) and
/// leaves the two post-commit steps of `ormx_large.rs`; at the threshold it is added in the
/// transaction, with no step.
#[tokio::test]
async fn business_key_added_to_a_large_table_puts_nothing_in_the_transaction() {
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
    let orm = orm_with(&h, &LATE_KEY, &LOT_NAME_KEY_DECL);
    let key = orm
        .registry()
        .model("scale.lot")
        .unwrap()
        .business_key("_name_key")
        .unwrap()
        .clone();
    let options = erp_orm::SchemaSyncOptions::new().large_table_rows(10);

    // 11 rows, more than 10: no DDL for the key, two steps
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    assert_eq!(table_ddl(tx.captured()), Vec::<String>::new());
    assert_eq!(
        sync.steps()
            .iter()
            .map(|s| (s.kind(), s.key().to_owned()))
            .collect::<Vec<_>>(),
        vec![
            (
                erp_orm::SchemaStepKind::CreateKeyIndex,
                format!("key_index.{}", key.full_name)
            ),
            (
                erp_orm::SchemaStepKind::AddKeyConstraint,
                format!("key_constraint.{}", key.full_name)
            ),
        ]
    );
    tx.rollback().await.unwrap();
    assert_eq!(constraint_comment(&mut tx, &key.full_name).await, None);
    tx.rollback().await.unwrap();
    drop(tx);

    // 10 rows, not more than 10: added in the transaction, no step left
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("DELETE FROM scale_lot WHERE name = 'lot 11'", &[])
        .await
        .unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    assert!(sync.steps().is_empty(), "{:?}", sync.steps());
    tx.commit().await.unwrap();
    assert_eq!(
        constraint_comment(&mut tx, &key.full_name).await,
        Some(Some(erp_sql::upsert::key_definition(&["name"], false)))
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A business key whose definition changes on a large table is refused (design note §11.1; the
/// refusal's text is asserted in `ormx_large.rs`); the inline synchronization of tests and new
/// databases (`init_models`) re-creates it whatever the size.
#[tokio::test]
async fn business_key_changed_on_a_large_table_is_refused_and_inline_sync_rebuilds_it() {
    let h = harness().await;
    let (full_name, before) = h
        .run(async |tx| {
            let w = world(tx, 11).await;
            quants(tx)
                .upsert(
                    tx,
                    Upsert::on(ScaleQuant::QUANT_KEY)
                        .rows(w.products.iter().map(|p| key_row(*p, w.locations[0], None))),
                )
                .await?;
            let key = quant_key(tx);
            let before = constraint_comment(tx, &key.full_name).await;
            Ok((key.full_name, before))
        })
        .await
        .unwrap();
    assert_eq!(
        before,
        Some(Some(erp_sql::upsert::key_definition(
            &["product_id", "location_id", "lot_id"],
            false
        )))
    );
    let orm = orm_with(&h, &NULLS_DISTINCT, &NULLS_DISTINCT_DECL);
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let err = tx
        .init_models_with(&[], &erp_orm::SchemaSyncOptions::new().large_table_rows(10))
        .await
        .unwrap_err();
    assert_eq!(table_ddl(tx.captured()), Vec::<String>::new());
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert!(
        err.message().starts_with(
            "cannot change business key '_quant_key' of 'scale.quant' during the install: table \
             'scale_quant' has more than 10 rows (large_table_rows), and a changed key has no \
             online replacement"
        ),
        "{}",
        err.message()
    );
    tx.rollback().await.unwrap();
    assert_eq!(constraint_comment(&mut tx, &full_name).await, before);
    tx.rollback().await.unwrap();
    drop(tx);

    // the inline form runs everything in the transaction
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        constraint_comment(&mut tx, &full_name).await,
        Some(Some(erp_sql::upsert::key_definition(
            &["product_id", "location_id", "lot_id"],
            true
        )))
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_007_computed_defaults_must_be_given() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let serials = tx.env().model::<ScaleSerial>().unwrap();
    let code = |c: &str| Vals::<ScaleSerial>::new().set(ScaleSerial::CODE, c);
    let err = assert_query_count!(tx, <= 0, {
        serials
            .upsert(&mut tx, Upsert::on(ScaleSerial::CODE_KEY).rows([code("A")]))
            .await
            .unwrap_err()
    });
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "upsert on 'scale.serial': fields with computed defaults must be given explicitly (an \
         upsert evaluates defaults before it knows which rows are new): number"
    );
    // a context default is computed too
    let with_hits = serials.with_context([("default_hits", Value::Int(4))]);
    let err = with_hits
        .upsert(
            &mut tx,
            Upsert::on(ScaleSerial::CODE_KEY).rows([code("A").set(ScaleSerial::NUMBER, 1)]),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "upsert on 'scale.serial': fields with computed defaults must be given explicitly (an \
         upsert evaluates defaults before it knows which rows are new): hits"
    );
    // with the computed field given: the static default fills inserted rows, no number is drawn
    let label_default = match &tx
        .registry()
        .model("scale.serial")
        .unwrap()
        .field("label")
        .unwrap()
        .default
    {
        Some(erp_registry::FieldDefault::Value(v)) => v.to_value(),
        other => panic!("label has a static default, got {other:?}"),
    };
    let drawn = scale::SERIAL_NUMBERS.load(Ordering::SeqCst);
    let out = serials
        .upsert(
            &mut tx,
            Upsert::on(ScaleSerial::CODE_KEY)
                .rows([code("A").set(ScaleSerial::NUMBER, 7)])
                .update([ScaleSerial::NUMBER]),
        )
        .await
        .unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Inserted);
    let label = out
        .records()
        .get(&mut tx, ScaleSerial::LABEL)
        .await
        .unwrap();
    assert_eq!(label.map(Value::Str), Some(label_default));
    let out = serials
        .upsert(
            &mut tx,
            Upsert::on(ScaleSerial::CODE_KEY)
                .rows([code("A").set(ScaleSerial::NUMBER, 8)])
                .update([ScaleSerial::NUMBER]),
        )
        .await
        .unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Updated);
    assert_eq!(scale::SERIAL_NUMBERS.load(Ordering::SeqCst), drawn);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_009_computes_and_constraints_run_not_overrides() {
    use erp_test_models::test_scale_upsert::hooks::{self, QuantHooks};
    let h = Harness::builder([&base_users::ADDON, &scale::ADDON, &hooks::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 3).await;
    let (l, model) = (w.locations[0], quants(&tx));
    let existing = model
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key_row(w.products[0], l, None).set(ScaleQuant::QUANTITY, 4.0)]),
        )
        .await
        .unwrap()
        .rows()[0]
        .id();
    tx.flush().await.unwrap();
    hooks::CREATE_CALLS.store(0, Ordering::SeqCst);
    hooks::WRITE_CALLS.store(0, Ordering::SeqCst);
    hooks::CHECKED.lock().unwrap().clear();
    let out = assert_query_count!(tx, >= 1, <= 1, {
        model
            .upsert(
                &mut tx,
                Upsert::on(ScaleQuant::QUANT_KEY)
                    .rows([
                        key_row(w.products[0], l, None).set(ScaleQuant::QUANTITY, 2.0),
                        key_row(w.products[1], l, None).set(ScaleQuant::QUANTITY, 3.0),
                    ])
                    .accumulate(ScaleQuant::QUANTITY),
            )
            .await
            .unwrap()
    });
    let ids: Vec<RecId> = out.rows().iter().map(|r| r.id()).collect();
    assert_eq!(ids[0], existing);
    assert_eq!(
        (out.rows()[0].outcome(), out.rows()[1].outcome()),
        (UpsertOutcome::Updated, UpsertOutcome::Inserted)
    );
    // both are recomputed by the next flush
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let doubled = out
        .records()
        .mapped(&mut tx, QuantHooks::QUANTITY_DOUBLE)
        .await
        .unwrap();
    let quantity = out
        .records()
        .mapped(&mut tx, ScaleQuant::QUANTITY)
        .await
        .unwrap();
    assert_eq!(quantity, vec![4.0 + 2.0, 3.0]);
    assert_eq!(
        doubled,
        quantity.iter().map(|q| q * 2.0).collect::<Vec<_>>()
    );
    let mut checked = hooks::CHECKED.lock().unwrap().clone();
    checked.sort_unstable();
    let mut expected: Vec<i64> = ids.iter().map(|i| i.get()).collect();
    expected.sort_unstable();
    assert_eq!(checked, expected, "the constraint ran for both ids");
    assert_eq!(hooks::CREATE_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(hooks::WRITE_CALLS.load(Ordering::SeqCst), 0);

    // a constraint violation fails the upsert and is rolled back with the savepoint
    let err = tx
        .savepoint(async |tx| {
            quants(tx)
                .upsert(
                    tx,
                    Upsert::on(ScaleQuant::QUANT_KEY)
                        .rows([key_row(w.products[2], l, None).set(ScaleQuant::QUANTITY, 2000.0)])
                        .accumulate(ScaleQuant::QUANTITY),
                )
                .await
                .map(|_| ())
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "A quant holds at most 1000 units (got 2000)."
    );
    assert_eq!(
        count(
            &mut tx,
            &format!(
                "SELECT count(*)::text FROM scale_quant WHERE product_id = {}",
                w.products[2]
            )
        )
        .await,
        0
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_010_upsert_is_overridable_with_super() {
    use erp_test_models::test_scale_upsert::override_upsert::{self, QuantMarked};
    let h = Harness::builder([&base_users::ADDON, &scale::ADDON, &override_upsert::ADDON])
        .build()
        .await
        .unwrap();
    let model_id = h.registry().model_id("scale.quant").unwrap();
    let chain: Vec<&str> = h
        .orm()
        .dispatch()
        .implementations(model_id, "upsert")
        .iter()
        .map(|i| i.addon)
        .collect();
    assert_eq!(chain, vec!["test_scale_upsert_override", "base"]);
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 5).await;
    let before = override_upsert::CALLS.load(Ordering::SeqCst);
    let out = quants(&tx)
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows((0..5).map(|i| key_row(w.products[i], w.locations[0], None))),
        )
        .await
        .unwrap();
    assert_eq!(override_upsert::CALLS.load(Ordering::SeqCst), before + 1);
    assert_eq!(override_upsert::LAST_BATCH.load(Ordering::SeqCst), 5);
    assert_eq!(out.records().len(), 5);
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let marked = out
        .records()
        .mapped(&mut tx, QuantMarked::MARKED)
        .await
        .unwrap();
    assert_eq!(marked, vec![true; 5]);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Call checks before any statement (design note §10): unknown key, key field updated,
/// accumulate on a non-numeric field, a row without a key field, a field named twice, rows
/// setting server-set or non-column fields.
#[tokio::test]
async fn upsert_calls_are_checked_before_any_statement() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 1).await;
    tx.flush().await.unwrap();
    let records = tx.env().records("scale.quant").unwrap();
    let key = || {
        RawVals::new()
            .set_cell("product_id", Cell::Id(w.products[0].get()))
            .set_cell("location_id", Cell::Id(w.locations[0].get()))
            .set_cell("lot_id", Cell::Null)
    };
    let stamp = || Cell::DateTime(DateTime::parse_odoo("2026-01-02 03:04:05").unwrap());
    let cases: Vec<(UpsertCall, &str)> = vec![
        (
            UpsertCall::new("_nope").with_rows([key()]),
            "upsert on 'scale.quant': no business key '_nope' (declared keys: _quant_key)",
        ),
        (
            UpsertCall::new("_quant_key")
                .with_rows([key()])
                .with_update(["lot_id"]),
            "upsert on 'scale.quant': key field 'lot_id' cannot be updated on conflict",
        ),
        (
            UpsertCall::new("_quant_key")
                .with_rows([key().set_cell("note", Cell::Str("x".into()))])
                .with_accumulate("note"),
            "upsert on 'scale.quant': accumulate needs an integer, float or monetary field; 'note' \
             is char",
        ),
        (
            UpsertCall::new("_quant_key").with_rows([RawVals::new()
                .set_cell("product_id", Cell::Id(w.products[0].get()))
                .set_cell("location_id", Cell::Id(w.locations[0].get()))]),
            "upsert on 'scale.quant': row 0 does not set key field 'lot_id'",
        ),
        (
            UpsertCall::new("_quant_key")
                .with_rows([key().set_cell("quantity", Cell::Float(1.0))])
                .with_update(["quantity"])
                .with_accumulate("quantity"),
            "upsert on 'scale.quant': field 'quantity' is named twice in update and accumulate",
        ),
        (
            UpsertCall::new("_quant_key")
                .with_rows([key()])
                .with_update(["quantity"]),
            "upsert on 'scale.quant': row 0 does not set updated field 'quantity'",
        ),
        (
            UpsertCall::new("_quant_key")
                .with_rows([key().set_cell("quantity", Cell::Null)])
                .with_accumulate("quantity"),
            "upsert on 'scale.quant': row 0 does not set accumulated field 'quantity'",
        ),
        (
            UpsertCall::new("_quant_key").with_rows([key().set_cell("row_version", Cell::Id(3))]),
            "'scale.quant.row_version' is set by the server and cannot be written",
        ),
        (
            UpsertCall::new("_quant_key").with_rows([key().set_cell("write_uid", Cell::Id(1))]),
            "'scale.quant.write_uid' is set by the server and cannot be written",
        ),
        (
            UpsertCall::new("_quant_key").with_rows([key().set_cell("create_uid", Cell::Id(1))]),
            "'scale.quant.create_uid' is set by the server and cannot be written",
        ),
        (
            UpsertCall::new("_quant_key").with_rows([key().set_cell("create_date", stamp())]),
            "'scale.quant.create_date' is set by the server and cannot be written",
        ),
        (
            UpsertCall::new("_quant_key").with_rows([key().set_cell("write_date", stamp())]),
            "'scale.quant.write_date' is set by the server and cannot be written",
        ),
        (
            UpsertCall::new("_quant_key").with_rows([key().set_cell("id", Cell::Id(1))]),
            "'scale.quant.id' is set by the server and cannot be written",
        ),
        // a NULL is a value too
        (
            UpsertCall::new("_quant_key").with_rows([key().set_cell("create_date", Cell::Null)]),
            "'scale.quant.create_date' is set by the server and cannot be written",
        ),
        // named in update or accumulate instead of a row
        (
            UpsertCall::new("_quant_key")
                .with_rows([key()])
                .with_update(["write_date"]),
            "'scale.quant.write_date' is set by the server and cannot be written",
        ),
        (
            UpsertCall::new("_quant_key")
                .with_rows([key()])
                .with_accumulate("row_version"),
            "'scale.quant.row_version' is set by the server and cannot be written",
        ),
        (
            UpsertCall::new("_quant_key")
                .with_rows([key().set_cell("display_name", Cell::Str("x".into()))]),
            "upsert on 'scale.quant': field 'display_name' is not a column of the model's own \
             table (inherited, x2many or not stored)",
        ),
        (
            UpsertCall::new("_quant_key")
                .with_rows([key().set_cell("reserved_quantity", Cell::Float(1.0))])
                .with_accumulate_floor("reserved_quantity", Cell::Null),
            "upsert on 'scale.quant': the floor of 'reserved_quantity' is empty",
        ),
    ];
    for (call, message) in cases {
        let err =
            assert_query_count!(tx, <= 0, { records.upsert(&mut tx, call).await.unwrap_err() });
        assert_eq!(err.kind(), ErrorKind::Programming, "{message}");
        assert_eq!(err.message(), message);
    }
    // duplicates with accumulate_floor cannot be merged
    let err = records
        .upsert(
            &mut tx,
            UpsertCall::new("_quant_key")
                .with_rows([
                    key().set_cell("reserved_quantity", Cell::Float(1.0)),
                    key().set_cell("reserved_quantity", Cell::Float(2.0)),
                ])
                .with_accumulate_floor("reserved_quantity", Cell::Float(0.0)),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "upsert on 'scale.quant': rows 0 and 1 have the same key; accumulate_floor applies its \
         floor per call, so they cannot be merged into one statement (merge them first, or upsert \
         them in separate calls)"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Models of a test addon declaring their own `create_date` field: `scale.stamp` with log access
/// (the field is server-set by name, as `create` treats it), `scale.ledger` without (an ordinary
/// field).
static STAMPS: erp_sdk::AddonDescriptor = erp_sdk::AddonDescriptor {
    name: "test_scale_stamps",
    depends: &["test_scale_upsert"],
    exact_only: false,
};

static STAMP_FIELDS: [FieldDecl; 2] = [
    FieldDecl::new("code", KindDecl::Char { size: None }).required(true),
    FieldDecl::new("create_date", KindDecl::Datetime).string("Stamped On"),
];

static CODE_KEY: [TableObjectDecl; 1] = [TableObjectDecl {
    name: "_code_key",
    kind: TableObjectKind::Key(KeyDecl::new(&["code"], "The code must be unique.")),
}];

/// Caller values for the audit fields are refused by name on a model with log access, also when
/// the model declares the field itself (its `create` drops them, and the statement sets them);
/// without log access `create_date` is an ordinary field the upsert writes.
#[tokio::test]
async fn upsert_refuses_audit_fields_by_name_under_log_access_only() {
    let h = Harness::builder([&base_users::ADDON, &scale::ADDON, &STAMPS])
        .with_decls([
            ModelDecl::new("test_scale_stamps", "scale.stamp")
                .fields(&STAMP_FIELDS)
                .table_objects(&CODE_KEY),
            ModelDecl::new("test_scale_stamps", "scale.ledger")
                .log_access(false)
                .fields(&STAMP_FIELDS)
                .table_objects(&CODE_KEY),
        ])
        .build()
        .await
        .unwrap();
    let registry = h.registry();
    assert!(
        !registry
            .model("scale.stamp")
            .unwrap()
            .field("create_date")
            .unwrap()
            .magic,
        "the model's own declaration replaces the audit field"
    );
    let stamp = DateTime::parse_odoo("2026-01-02 03:04:05").unwrap();
    let row = |code: &str| {
        RawVals::new()
            .set_cell("code", Cell::Str(code.into()))
            .set_cell("create_date", Cell::DateTime(stamp))
    };
    let mut tx = h.begin().await.unwrap();
    let stamps = tx.env().records("scale.stamp").unwrap();
    let err = assert_query_count!(tx, <= 0, {
        stamps
            .upsert(&mut tx, UpsertCall::new("_code_key").with_rows([row("A")]))
            .await
            .unwrap_err()
    });
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "'scale.stamp.create_date' is set by the server and cannot be written"
        )
    );
    let ledger = tx.env().records("scale.ledger").unwrap();
    let out = ledger
        .upsert(&mut tx, UpsertCall::new("_code_key").with_rows([row("A")]))
        .await
        .unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Inserted);
    assert_eq!(
        one(
            &mut tx,
            &format!(
                "SELECT to_char(create_date, 'YYYY-MM-DD HH24:MI:SS') FROM scale_ledger WHERE id \
                 = {}",
                out.rows()[0].id()
            )
        )
        .await
        .unwrap(),
        "2026-01-02 03:04:05"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The dynamic entry converts values and floors with the one converter.
#[tokio::test]
async fn upsert_values_on_the_dynamic_surface() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 1).await;
    let records = tx.env().records("scale.quant").unwrap();
    let row = |q: f64| {
        map(&[
            ("product_id", Value::Int(w.products[0].get())),
            ("location_id", Value::Int(w.locations[0].get())),
            ("lot_id", Value::False),
            ("quantity", Value::Float(q)),
            ("reserved_quantity", Value::Float(q)),
        ])
    };
    let out = records
        .upsert_values(
            &mut tx,
            "_quant_key",
            &[row(2.0)],
            &[],
            &[("quantity", None)],
        )
        .await
        .unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Inserted);
    let out = records
        .upsert_values(
            &mut tx,
            "_quant_key",
            &[row(-5.0)],
            &[],
            &[
                ("quantity", None),
                ("reserved_quantity", Some(Value::Int(0))),
            ],
        )
        .await
        .unwrap();
    assert_eq!(out.rows()[0].outcome(), UpsertOutcome::Updated);
    let rec = records.browse([out.rows()[0].id().get()]).unwrap();
    assert_eq!(
        rec.values(&mut tx, "quantity").await.unwrap(),
        vec![Value::Float(2.0 - 5.0)]
    );
    assert_eq!(
        rec.values(&mut tx, "reserved_quantity").await.unwrap(),
        vec![Value::Float((2.0f64 - 5.0).max(0.0))]
    );
    let err = records
        .upsert_values(
            &mut tx,
            "_quant_key",
            &[row(1.0)],
            &[],
            &[("nope", Some(Value::Int(0)))],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "upsert on 'scale.quant': no field 'nope'");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// An upsert in a read-only transaction is refused before any statement
/// touches the model, as `create` and `write` are.
#[tokio::test]
async fn an_upsert_in_a_read_only_transaction_is_refused() {
    let h = harness().await;
    let mut tx = h
        .orm()
        .begin_opts(erp_orm::BeginOptions::new(SUPERUSER_ID).read_only("ormx.upsert_check"))
        .await
        .unwrap();
    let one_id = RecId::from_real(1).unwrap();
    tx.capture_statements(true);
    let err = quants(&tx)
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY)
                .rows([key_row(one_id, one_id, None).set(ScaleQuant::QUANTITY, 1.0)]),
        )
        .await
        .unwrap_err();
    tx.capture_statements(false);
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Programming,
            "'ormx.upsert_check' is declared effect = read but tried to upsert 'scale.quant'; \
             declare effect = write (or wider)."
        )
    );
    assert!(
        tx.captured().iter().all(|s| !s.contains("scale_quant")),
        "{:#?}",
        tx.captured()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review DB-S1: a required key field never takes NULL.
#[tokio::test]
async fn a_required_key_field_refuses_null() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 1).await;
    tx.flush().await.unwrap();
    let spec = h.registry().model("scale.quant").unwrap().clone();
    let field = spec.field("product_id").unwrap();
    let err = quants(&tx)
        .upsert(
            &mut tx,
            Upsert::on(ScaleQuant::QUANT_KEY).rows([Vals::<ScaleQuant>::new()
                .set(ScaleQuant::PRODUCT_ID, None::<RecId>)
                .set(ScaleQuant::LOCATION_ID, w.locations[0])
                .set(ScaleQuant::LOT_ID, None::<RecId>)]),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (err.kind(), err.message()),
        (
            ErrorKind::Validation,
            format!(
                "The database refused the change: the key field '{}' ({}) of '{}' is required \
                 and row 0 gives no value for it",
                field.string, field.name, spec.description
            )
            .as_str()
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review DB-S2: the statements take the rows sorted by key, so concurrent calls with the same
/// keys in another input order lock the rows in the same order; the result is still in input
/// order.
#[tokio::test]
async fn rows_are_sent_sorted_by_key_and_returned_in_input_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let serials = tx.env().model::<ScaleSerial>().unwrap();
    let row = |c: &str, n: i32| {
        Vals::<ScaleSerial>::new()
            .set(ScaleSerial::CODE, c)
            .set(ScaleSerial::NUMBER, n)
    };
    let out = serials
        .upsert(
            &mut tx,
            Upsert::on(ScaleSerial::CODE_KEY).rows([row("B", 2), row("A", 1), row("C", 3)]),
        )
        .await
        .unwrap();
    let ids: Vec<i64> = out.rows().iter().map(|r| r.id().get()).collect();
    assert!(
        ids[1] < ids[0] && ids[0] < ids[2],
        "A (input 1) was inserted before B (input 0) and C: {ids:?}"
    );
    let stored = tx
        .query("SELECT code FROM scale_serial ORDER BY id", &[])
        .await
        .unwrap();
    let codes: Vec<String> = stored.iter().map(|r| r.get(0)).collect();
    assert_eq!(codes, ["A", "B", "C"]);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review RS-S6: the builder is `Debug` and `Clone`.
#[test]
fn the_upsert_builder_is_debug_and_clone() {
    let call = Upsert::<ScaleSerial>::on(ScaleSerial::CODE_KEY)
        .rows([Vals::<ScaleSerial>::new().set(ScaleSerial::CODE, "A")]);
    let text = format!("{:?}", call.clone());
    assert!(
        text.starts_with("Upsert { call: UpsertCall { key: \"_code_key\""),
        "{text}"
    );
}

/// Review SEC-S2 (round 2): a readonly stored computed field (declared `precompute`, or not: a
/// stored compute is precomputed by default) and a stored related field are always computed; a
/// caller value is refused in rows, `update` and `accumulate`.
#[tokio::test]
async fn readonly_stored_computed_and_related_fields_are_refused() {
    use erp_test_models::test_scale_upsert::hooks;
    use erp_test_models::test_scale_upsert::override_upsert;
    let h = Harness::builder([
        &base_users::ADDON,
        &scale::ADDON,
        &hooks::ADDON,
        &override_upsert::ADDON,
    ])
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let w = world(&mut tx, 1).await;
    tx.flush().await.unwrap();
    let key = || {
        RawVals::new()
            .set_cell("product_id", Cell::Id(w.products[0].get()))
            .set_cell("location_id", Cell::Id(w.locations[0].get()))
            .set_cell("lot_id", Cell::Null)
    };
    let records = tx.env().records("scale.quant").unwrap();
    let cases = [
        // declared precompute
        ("double_quantity", Cell::Float(5.0)),
        // stored compute, precompute unset
        ("quantity_double", Cell::Float(5.0)),
        // stored related
        ("location_label", Cell::Str("x".into())),
    ];
    for (name, cell) in cases {
        let message = format!(
            "upsert on 'scale.quant': field '{name}' is a readonly stored computed or related \
             field: it is always computed and a caller value is refused"
        );
        let in_row = UpsertCall::new("_quant_key").with_rows([key().set_cell(name, cell.clone())]);
        let in_update = UpsertCall::new("_quant_key")
            .with_rows([key().set_cell(name, cell.clone())])
            .with_update([name]);
        for (label, call) in [("row", in_row), ("update", in_update)] {
            let err = records.upsert(&mut tx, call).await.unwrap_err();
            assert_eq!(
                (err.kind(), err.message()),
                (ErrorKind::Programming, message.as_str()),
                "{name} in {label}"
            );
        }
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
