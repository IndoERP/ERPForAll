//! Atomic reference boundary integration. Stock consumers remain M7 work.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration assertions"
)]

use erp_addon_uom::references::register;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

async fn fixture() -> Harness {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    // This fixture exercises the installed SQL, but is not a module-lifecycle proof.
    tx.batch(include_str!("../src/guard_sql.sql"))
        .await
        .unwrap();
    tx.execute("INSERT INTO uom_reference_fence(singleton) VALUES (1)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    h
}

async fn unit(tx: &mut Transaction, name: &str, parent: Option<i64>, factor: i64) -> i64 {
    let mut vals = RawVals::new()
        .set_cell("name", Cell::Str(name.into()))
        .set_cell("relative_factor", Cell::Decimal(Decimal::from(factor)));
    if let Some(parent) = parent {
        vals = vals.set_cell("relative_uom_id", Cell::Id(parent));
    }
    tx.env()
        .records("uom.uom")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}

#[tokio::test]
async fn committed_reference_protects_direct_and_ancestor_ratios() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Root", None, 1).await;
    let pack = unit(&mut tx, "Pack", Some(root), 6).await;
    let carton = unit(&mut tx, "Carton", Some(pack), 4).await;
    let env = tx.env();
    let identities = register(&env, &mut tx, &[carton, carton]).await.unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].id(), carton);
    assert_eq!(identities[0].root(), root);
    assert_eq!(identities[0].factor(), Decimal::from(24));
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    for id in [carton, pack] {
        let error = tx
            .savepoint(async |tx| {
                tx.env()
                    .records("uom.uom")?
                    .browse([id])?
                    .write(
                        tx,
                        RawVals::new().set_cell("relative_factor", Cell::Decimal(Decimal::from(5))),
                    )
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "A referenced unit of measure would change. Create a new unit instead."
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn registration_rollback_removes_new_markers_and_repeat_use_is_idempotent() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    // A real SQL business-row fixture, not a delivered stock consumer.
    tx.batch(
        "CREATE TABLE guard_consumer_fixture(uom_id bigint PRIMARY KEY REFERENCES uom_uom(id))",
    )
    .await
    .unwrap();
    let root = unit(&mut tx, "Root", None, 1).await;
    let pack = unit(&mut tx, "Pack", Some(root), 6).await;
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    register(&env, &mut tx, &[pack]).await.unwrap();
    tx.execute(
        "INSERT INTO guard_consumer_fixture(uom_id) VALUES($1)",
        &[&pack],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let count: i64 = tx
        .query("SELECT count(*) FROM uom_reference_use", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    assert_eq!(
        tx.query("SELECT count(*) FROM guard_consumer_fixture", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
    let env = tx.env();
    let error = tx
        .savepoint(async |tx| {
            register(&env, tx, &[pack]).await?;
            tx.execute(
                "INSERT INTO guard_consumer_fixture(uom_id) VALUES($1)",
                &[&pack],
            )
            .await?;
            Err::<(), _>(Error::user("consumer fixture refused"))
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
    assert_eq!(error.message(), "consumer fixture refused");
    let count: i64 = tx
        .query("SELECT count(*) FROM uom_reference_use", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 0);
    assert_eq!(
        tx.query("SELECT count(*) FROM guard_consumer_fixture", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
    let row = tx
        .query(
            "SELECT relative_uom_id::bigint,factor::text FROM uom_uom WHERE id=$1",
            &[&pack],
        )
        .await
        .unwrap();
    assert_eq!(row[0].get::<_, i64>(0), root);
    assert_eq!(row[0].get::<_, String>(1), "6");
    for _ in 0..2 {
        register(&env, &mut tx, &[pack]).await.unwrap();
    }
    let count: i64 = tx
        .query("SELECT count(*) FROM uom_reference_use", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count, 1);
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_denial_has_one_guard_statement_before_any_operation_savepoint() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Root", None, 1).await;
    let pack = unit(&mut tx, "Pack", Some(root), 6).await;
    let env = tx.env();
    register(&env, &mut tx, &[pack]).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    let before = tx.query_count();
    let error = tx
        .env()
        .records("uom.uom")
        .unwrap()
        .browse([pack])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("relative_factor", Cell::Decimal(Decimal::from(7))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "A referenced unit of measure would change. Create a new unit instead."
    );
    let absolute = tx.query_count() - before;
    let captured = tx.captured();
    let guards = captured
        .iter()
        .filter(|sql| sql.contains("UPDATE public.uom_reference_fence"))
        .count();
    eprintln!(
        "absolute_client_statements={absolute} guard_statements={guards} full_sql_capture={captured:?}"
    );
    assert_eq!(guards, 1);
    assert!(!captured.iter().any(|sql| {
        sql.trim_start()
            .to_ascii_uppercase()
            .starts_with("SAVEPOINT")
    }));
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn equivalent_unused_ancestor_reparent_preserves_used_identity() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let root = unit(&mut tx, "Root", None, 1).await;
    let a = unit(&mut tx, "A", Some(root), 2).await;
    let b = unit(&mut tx, "B", Some(root), 2).await;
    let x = unit(&mut tx, "X", Some(a), 3).await;
    let y = unit(&mut tx, "Y", Some(b), 3).await;
    let used = unit(&mut tx, "Used", Some(x), 2).await;
    let other_root = unit(&mut tx, "Other root", None, 1).await;
    let env = tx.env();
    register(&env, &mut tx, &[used]).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.env()
        .records("uom.uom")
        .unwrap()
        .browse([x])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("relative_uom_id", Cell::Id(b)),
        )
        .await
        .unwrap();
    tx.env()
        .records("uom.uom")
        .unwrap()
        .browse([used])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("active", Cell::Bool(false))
                .set_cell("name", Cell::Str("Archived used".into()))
                .set_cell("relative_factor", Cell::Decimal(Decimal::from(2))),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for (id, parent, ratio) in [(used, y, None), (used, b, Some(6)), (x, other_root, None)] {
        let mut tx = h.begin().await.unwrap();
        let mut values = RawVals::new().set_cell("relative_uom_id", Cell::Id(parent));
        if let Some(ratio) = ratio {
            values = values.set_cell("relative_factor", Cell::Decimal(Decimal::from(ratio)));
        }
        let error = tx
            .env()
            .records("uom.uom")
            .unwrap()
            .browse([id])
            .unwrap()
            .write(&mut tx, values)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "A referenced unit of measure would change. Create a new unit instead."
        );
        tx.rollback().await.unwrap();
    }
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let units = register(&env, &mut tx, &[used, y, used]).await.unwrap();
    assert_eq!(units.iter().map(|u| u.id()).collect::<Vec<_>>(), [y, used]);
    let used_unit = units.iter().find(|u| u.id() == used).unwrap();
    assert_eq!(used_unit.root(), root);
    assert_eq!(used_unit.factor(), Decimal::from(12));
    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn exact_reference_survives_multiplication_of_high_scale_ratios() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let ratio = Decimal::from_str_exact("1.0000000000000000000000000000").unwrap();
    let units = tx.env().records("uom.uom").unwrap();
    let root = units
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Scaled root".into()))
                    .set_cell("relative_factor", Cell::Decimal(ratio)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let leaf = units
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Scaled leaf".into()))
                    .set_cell("relative_uom_id", Cell::Id(root))
                    .set_cell("relative_factor", Cell::Decimal(ratio)),
            ],
        )
        .await
        .unwrap()
        .ids()[0];
    let stored_scale: i32 = tx
        .query(
            "SELECT scale(relative_factor) FROM uom_uom WHERE id=$1",
            &[&root],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(stored_scale, ratio.scale() as i32);
    let env = tx.env();
    for _ in 0..2 {
        let registered = register(&env, &mut tx, &[leaf])
            .await
            .expect("a mathematically exact factor of one must be representable");
        assert_eq!(registered[0].root(), root);
        assert_eq!(registered[0].factor(), Decimal::ONE);
    }
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
