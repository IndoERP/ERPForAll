//! Parity tests for named decimal precision (`digits='Name'`).
//! Spec.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_fields::{Cell, Error, ErrorKind, Value};
use erp_orm::{RawVals, Records, Transaction};
use erp_rpc::RpcCall;
use erp_test_models::{base_users, test_defaults};
use erp_testkit::{FieldPolicy, Harness, RpcHarness};

async fn precision_harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_defaults::ADDON,
    ])
    .build()
    .await
    .expect("harness")
}

async fn digits(tx: &mut Transaction, items: &Records) -> Value {
    items
        .fields_get(tx, Some(&["price"]), Some(&["digits"]))
        .await
        .unwrap()["price"]["digits"]
        .clone()
}

fn expected_digits(scale: i64) -> Value {
    Value::list([Value::Int(16), Value::Int(scale)])
}

async fn configured(tx: &mut Transaction, scale: i32) -> Records {
    tx.env()
        .records("decimal.precision")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Product Price".into()))
                    .set_cell("digits", Cell::Int(scale)),
            ],
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn spec_field_022_named_decimal_precision() {
    let h = precision_harness().await;
    let mut tx = h.begin().await.unwrap();

    let items = tx.env().records("test_defaults.item").unwrap();

    // 1. With no decimal.precision record, named precision resolves to fallback (16, 2)
    let desc1 = items.fields_get(&mut tx, None, None).await.unwrap();
    let price_desc1 = desc1
        .get("price")
        .expect("price field description expected");
    assert_eq!(
        price_desc1.get("digits"),
        Some(&Value::list([Value::Int(16), Value::Int(2)]))
    );

    // 2. Insert decimal.precision record for "Product Price" with digits = 4
    let precisions = tx.env().records("decimal.precision").unwrap();
    precisions
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Product Price".into()))
                    .set_cell("digits", Cell::Int(4)),
            ],
        )
        .await
        .unwrap();

    // 3. fields_get now returns (16, 4)
    let desc2 = items.fields_get(&mut tx, None, None).await.unwrap();
    let price_desc2 = desc2
        .get("price")
        .expect("price field description expected");
    assert_eq!(
        price_desc2.get("digits"),
        Some(&Value::list([Value::Int(16), Value::Int(4)]))
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn fields_get_precision_cache_tracks_create_update_rename_and_delete() {
    let h = precision_harness().await;
    let mut tx = h.begin().await.unwrap();
    let items = tx.env().records("test_defaults.item").unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(2));
    let row = configured(&mut tx, 4).await;
    assert_eq!(digits(&mut tx, &items).await, expected_digits(4));
    // The second request reuses the transaction's resolved digits.
    let before = tx.query_count();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(4));
    assert_eq!(tx.query_count(), before);
    row.write(&mut tx, RawVals::new().set_cell("digits", Cell::Int(0)))
        .await
        .unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(0));
    row.write(&mut tx, RawVals::new().set_cell("digits", Cell::Int(28)))
        .await
        .unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(28));
    row.write(
        &mut tx,
        RawVals::new().set_cell("name", Cell::Str("Other".into())),
    )
    .await
    .unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(2));
    row.write(
        &mut tx,
        RawVals::new().set_cell("name", Cell::Str("Product Price".into())),
    )
    .await
    .unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(28));
    row.unlink(&mut tx).await.unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(2));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn fields_get_precision_cache_does_not_escape_rollback_scopes() {
    let h = precision_harness().await;
    let mut tx = h.begin().await.unwrap();
    let items = tx.env().records("test_defaults.item").unwrap();
    let row = configured(&mut tx, 4).await;
    assert_eq!(digits(&mut tx, &items).await, expected_digits(4));
    let error = tx
        .savepoint(async |tx| {
            row.write(tx, RawVals::new().set_cell("digits", Cell::Int(7)))
                .await?;
            assert_eq!(digits(tx, &items).await, expected_digits(7));
            Err::<(), _>(Error::validation("precision rollback probe"))
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "precision rollback probe");
    assert_eq!(digits(&mut tx, &items).await, expected_digits(4));
    tx.isolated_for_tests(async |tx| {
        row.write(tx, RawVals::new().set_cell("digits", Cell::Int(9)))
            .await?;
        assert_eq!(digits(tx, &items).await, expected_digits(9));
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(4));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// not the multi-process node-loss contract)
#[tokio::test]
async fn fields_get_precision_cache_is_transaction_local_across_orm_instances() {
    let h = precision_harness().await;
    let a = h.orm().rebuild(h.registry().clone()).build().unwrap();
    let b = h.orm().rebuild(h.registry().clone()).build().unwrap();
    let mut before = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let items = before.env().records("test_defaults.item").unwrap();
    assert_eq!(digits(&mut before, &items).await, expected_digits(2));
    before.rollback().await.unwrap();
    let mut writer = a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    configured(&mut writer, 6).await;
    writer.commit().await.unwrap();
    drop(a);
    let mut after = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let items = after.env().records("test_defaults.item").unwrap();
    assert_eq!(digits(&mut after, &items).await, expected_digits(6));
    after.rollback().await.unwrap();
    drop(b);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn fields_get_exposes_only_digits_to_users_denied_the_precision_table() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_defaults::ADDON,
    ])
    .policy(FieldPolicy::new().deny_model("decimal.precision"))
    .build()
    .await
    .unwrap();
    let mut setup = h.begin().await.unwrap();
    configured(&mut setup, 5).await;
    setup
        .batch(
            "INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Clerk', 'clerk', true, false)",
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let mut tx = h.begin_as(42).await.unwrap();
    let env = tx.env();
    let error = env
        .records("decimal.precision")
        .unwrap()
        .check_access(&mut tx, erp_orm::Operation::Read)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you read 'Decimal Precision' (decimal.precision) records."
    );
    let items = env.records("test_defaults.item").unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(5));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn fields_get_precision_rpc_uses_the_stored_metadata_service() {
    let h = RpcHarness::builder(
        Harness::builder([
            &base_users::ADDON,
            &erp_addon_base::ADDON,
            &test_defaults::ADDON,
        ]),
        FieldPolicy::new().deny_model("decimal.precision"),
    )
    .host(|_| support::rpc::FixtureHost)
    .build()
    .await
    .unwrap();
    let mut tx = h.harness().begin().await.unwrap();
    configured(&mut tx, 8).await;
    tx.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'Alice', 'alice', true, false)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let response = h
        .service()
        .call(RpcCall::json2(
            "/json/2/test_defaults.item/fields_get",
            br#"{"allfields":["price"],"attributes":["digits"]}"#.to_vec(),
            support::rpc::alice(),
        ))
        .await;
    assert_eq!(
        response.status(),
        200,
        "{}",
        String::from_utf8_lossy(response.body())
    );
    let value: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(value["price"]["digits"], serde_json::json!([16, 8]));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn fields_get_checks_requested_fields_and_attributes_before_resolving_digits() {
    let h = precision_harness().await;
    let mut tx = h.begin().await.unwrap();
    let items = tx.env().records("test_defaults.item").unwrap();
    // A deliberately invalid legacy cache override: describing other attributes does not use it.
    tx.set_precision("Product Price", 29);
    let before = tx.query_count();
    let names = items
        .fields_get(&mut tx, Some(&["price"]), Some(&["string"]))
        .await
        .unwrap();
    assert!(names["price"].contains_key("string"));
    assert!(!names["price"].contains_key("digits"));
    assert!(
        items
            .fields_get(&mut tx, Some(&[]), None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(tx.query_count(), before);
    let error = items
        .fields_get(&mut tx, Some(&["price"]), Some(&["digits"]))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Decimal precision 'Product Price' must be between 0 and 28, got 29."
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn explicit_invalidation_discards_precision_metadata_after_raw_updates() {
    let h = precision_harness().await;
    let mut tx = h.begin().await.unwrap();
    let items = tx.env().records("test_defaults.item").unwrap();
    let row = configured(&mut tx, 4).await;
    assert_eq!(digits(&mut tx, &items).await, expected_digits(4));
    tx.batch("UPDATE decimal_precision SET digits = 6 WHERE name = 'Product Price'")
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(6));
    tx.batch("UPDATE decimal_precision SET digits = 7 WHERE name = 'Product Price'")
        .await
        .unwrap();
    tx.invalidate_model("decimal.precision", Some(&["digits"]))
        .await
        .unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(7));
    tx.batch("UPDATE decimal_precision SET digits = 8 WHERE name = 'Product Price'")
        .await
        .unwrap();
    tx.invalidate_records(&row, Some(&["digits"]))
        .await
        .unwrap();
    assert_eq!(digits(&mut tx, &items).await, expected_digits(8));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn hidden_named_fields_do_not_resolve_their_precision() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_defaults::ADDON,
    ])
    .policy(FieldPolicy::new().forbid("test_defaults.item", "price"))
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Clerk', 'clerk', true, false)")
        .await
        .unwrap();
    tx.set_precision("Product Price", 29);
    let items = tx
        .env()
        .records("test_defaults.item")
        .unwrap()
        .with_user(42);
    let described = items.fields_get(&mut tx, None, None).await.unwrap();
    assert!(!described.contains_key("price"));
    drop(tx);
    h.cleanup().await.unwrap();
}
