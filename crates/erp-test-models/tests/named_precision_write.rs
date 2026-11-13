//! Named digits reject excess scale on every mutation boundary (approved L-9).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_fields::{Cell, Decimal, ErrorKind, Value};
use erp_orm::{RawVals, Records, Transaction};
use erp_test_models::{base_users, test_defaults};
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &test_defaults::ADDON,
    ])
    .build()
    .await
    .unwrap()
}
async fn precision(tx: &mut Transaction, scale: i32) -> Records {
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
fn raw(value: &str) -> RawVals {
    RawVals::new().set_cell("price", Cell::Decimal(value.parse::<Decimal>().unwrap()))
}
fn check(error: &erp_fields::Error, digits: u32) {
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!(
            "Excess decimal scale for test_defaults.item.price (precision 'Product Price'): maximum {digits} fractional digits."
        )
    );
}

#[tokio::test]
async fn named_create_and_write_refuse_excess_without_rounding() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    precision(&mut tx, 2).await;
    let items = tx
        .env()
        .with_company(1)
        .unwrap()
        .records("test_defaults.item")
        .unwrap();
    let error = items
        .create(&mut tx, vec![raw("1.23"), raw("1.234")])
        .await
        .unwrap_err();
    check(&error, 2);
    assert_eq!(
        support::one(&mut tx, "SELECT count(*)::text FROM test_defaults_item")
            .await
            .as_deref(),
        Some("0")
    );
    let records = items
        .create(&mut tx, vec![raw("-1.2300"), raw("0.000")])
        .await
        .unwrap();
    let error = records.write(&mut tx, raw("1.234")).await.unwrap_err();
    check(&error, 2);
    tx.flush().await.unwrap();
    assert_eq!(
        support::rows(
            &mut tx,
            "SELECT price::text FROM test_defaults_item ORDER BY id"
        )
        .await,
        vec![vec![Some("-1.2300".into())], vec![Some("0".into())]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn named_dynamic_write_ignores_metadata_override_and_tracks_local_precision() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let setting = precision(&mut tx, 2).await;
    let items = tx
        .env()
        .with_company(1)
        .unwrap()
        .records("test_defaults.item")
        .unwrap();
    tx.set_precision("Product Price", 28);
    let error = items
        .create_values(&mut tx, &[support::map(&[("price", Value::str("1.234"))])])
        .await
        .unwrap_err();
    check(&error, 2);
    setting
        .write(&mut tx, RawVals::new().set_cell("digits", Cell::Int(4)))
        .await
        .unwrap();
    let record = items.create(&mut tx, vec![raw("1.2345")]).await.unwrap();
    let error = record
        .write_values(&mut tx, &support::map(&[("price", Value::str("1.23456"))]))
        .await
        .unwrap_err();
    check(&error, 4);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn named_scale_zero_and_twenty_eight_are_exact() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let setting = precision(&mut tx, 0).await;
    let items = tx
        .env()
        .with_company(1)
        .unwrap()
        .records("test_defaults.item")
        .unwrap();
    items.create(&mut tx, vec![raw("-2.000")]).await.unwrap();
    check(
        &items.create(&mut tx, vec![raw("-2.1")]).await.unwrap_err(),
        0,
    );
    setting
        .write(&mut tx, RawVals::new().set_cell("digits", Cell::Int(28)))
        .await
        .unwrap();
    items
        .create(&mut tx, vec![raw("0.1234567890123456789012345678")])
        .await
        .unwrap();
    tx.flush().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn computed_harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_test_models::test_exact_numeric::ADDON,
    ])
    .build()
    .await
    .unwrap()
}
fn numeric(code: &str, field: &'static str, value: &str) -> RawVals {
    RawVals::new()
        .set_cell("code", Cell::Str(code.into()))
        .set_cell(field, Cell::Decimal(value.parse().unwrap()))
}
fn check_field(error: &erp_fields::Error, field: &str, digits: u32) {
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!(
            "Excess decimal scale for exact_numeric.item.{field} (precision 'Product Price'): maximum {digits} fractional digits."
        )
    );
}

#[tokio::test]
async fn named_precompute_and_recompute_refuse_before_publishing_outputs() {
    let h = computed_harness().await;
    let mut tx = h.begin().await.unwrap();
    precision(&mut tx, 2).await;
    let all = tx.env().records("exact_numeric.item").unwrap();
    check_field(
        &tx.savepoint(async |tx| all.create(tx, vec![numeric("bad", "input", "1.234")]).await)
            .await
            .unwrap_err(),
        "early",
        2,
    );
    assert_eq!(
        support::one(&mut tx, "SELECT count(*)::text FROM exact_numeric_item")
            .await
            .as_deref(),
        Some("0")
    );
    let record = all
        .create(
            &mut tx,
            vec![
                numeric("good", "input", "1.23")
                    .set_cell("late_input", Cell::Decimal(Decimal::ZERO)),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let error = tx
        .savepoint(async |tx| {
            record
                .write(
                    tx,
                    RawVals::new().set_cell("late_input", Cell::Decimal("1.234".parse().unwrap())),
                )
                .await?;
            tx.flush().await
        })
        .await
        .unwrap_err();
    check_field(&error, "late", 2);
    assert_eq!(
        support::one(&mut tx, "SELECT late_input::text FROM exact_numeric_item")
            .await
            .as_deref(),
        Some("0")
    );
    tx.flush().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn named_upsert_validates_insert_update_and_actual_accumulation() {
    use erp_orm::UpsertCall;
    let h = computed_harness().await;
    let mut tx = h.begin().await.unwrap();
    let setting = precision(&mut tx, 2).await;
    let all = tx.env().records("exact_numeric.item").unwrap();
    let call = |value: &str| {
        UpsertCall::new("_code_key")
            .with_rows([numeric("a", "amount", value)])
            .with_accumulate("amount")
    };
    check_field(
        &tx.savepoint(async |tx| all.upsert(tx, call("1.234")).await)
            .await
            .unwrap_err(),
        "amount",
        2,
    );
    all.upsert(&mut tx, call("1.23")).await.unwrap();
    all.upsert(&mut tx, call("0.01")).await.unwrap();
    assert_eq!(
        support::one(&mut tx, "SELECT amount::text FROM exact_numeric_item")
            .await
            .as_deref(),
        Some("1.24")
    );
    let update = UpsertCall::new("_code_key")
        .with_rows([numeric("a", "amount", "1.234")])
        .with_update(["amount"]);
    check_field(
        &tx.savepoint(async |tx| all.upsert(tx, update).await)
            .await
            .unwrap_err(),
        "amount",
        2,
    );
    // An older row can have a scale higher than the newly configured write limit.
    setting
        .write(&mut tx, RawVals::new().set_cell("digits", Cell::Int(1)))
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx| all.upsert(tx, call("0.1")).await)
        .await
        .unwrap_err();
    check_field(&error, "amount", 1);
    assert_eq!(
        support::one(&mut tx, "SELECT amount::text FROM exact_numeric_item")
            .await
            .as_deref(),
        Some("1.24")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn named_related_target_and_rollback_follow_authoritative_scale() {
    let h = computed_harness().await;
    let mut tx = h.begin().await.unwrap();
    let setting = precision(&mut tx, 2).await;
    let all = tx.env().records("exact_numeric.item").unwrap();
    let parent = all
        .create(&mut tx, vec![numeric("p", "amount", "1.23")])
        .await
        .unwrap();
    let parent_id = parent.ids()[0];
    let child = all
        .create(
            &mut tx,
            vec![numeric("c", "amount", "0").set_cell("parent_id", Cell::Id(parent_id))],
        )
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx| {
            child
                .write(
                    tx,
                    RawVals::new()
                        .set_cell("related_amount", Cell::Decimal("1.234".parse().unwrap())),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Excess decimal scale for exact_numeric.item.related_amount (precision 'Product Price'): maximum 2 fractional digits."
    );
    let error = tx
        .savepoint(async |tx| {
            setting
                .write(tx, RawVals::new().set_cell("digits", Cell::Int(4)))
                .await?;
            parent
                .write(
                    tx,
                    RawVals::new().set_cell("amount", Cell::Decimal("1.2345".parse().unwrap())),
                )
                .await?;
            Err::<(), _>(erp_fields::Error::validation("rollback precision"))
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "rollback precision");
    check_field(
        &parent
            .write(
                &mut tx,
                RawVals::new().set_cell("amount", Cell::Decimal("1.234".parse().unwrap())),
            )
            .await
            .unwrap_err(),
        "amount",
        2,
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn pending_named_values_are_revalidated_after_precision_changes() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let setting = precision(&mut tx, 4).await;
    let items = tx
        .env()
        .with_company(1)
        .unwrap()
        .records("test_defaults.item")
        .unwrap();
    let record = items.create(&mut tx, vec![raw("1.23")]).await.unwrap();
    tx.flush().await.unwrap();
    let error = tx
        .savepoint(async |tx| {
            record.write(tx, raw("1.2345")).await?;
            setting
                .write(tx, RawVals::new().set_cell("digits", Cell::Int(2)))
                .await?;
            tx.flush().await
        })
        .await
        .unwrap_err();
    check(&error, 2);
    assert_eq!(
        support::one(&mut tx, "SELECT price::text FROM test_defaults_item")
            .await
            .as_deref(),
        Some("1.23")
    );
    record.write(&mut tx, raw("1.2345")).await.unwrap();
    tx.flush().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn addon_macro_emits_matching_native_and_describe_numeric_policy() {
    assert!(erp_test_models::test_exact_numeric::ADDON.exact_only);
    let document: serde_json::Value = serde_json::from_str(&erp_sdk::describe()).unwrap();
    let contribution = document["contributions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["addon"] == "test_exact_numeric" && entry["kind"] == "addon")
        .expect("numeric addon contribution");
    assert_eq!(contribution["detail"]["exact_only"], true);
}

#[tokio::test]
async fn caught_post_sql_scale_failure_refuses_commit() {
    use erp_orm::UpsertCall;
    let h = computed_harness().await;
    let mut setup = h.begin().await.unwrap();
    let setting = precision(&mut setup, 2).await;
    let all = setup.env().records("exact_numeric.item").unwrap();
    all.create(&mut setup, vec![numeric("a", "amount", "1.24")])
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let mut configure = h.begin().await.unwrap();
    setting
        .write(
            &mut configure,
            RawVals::new().set_cell("digits", Cell::Int(1)),
        )
        .await
        .unwrap();
    configure.commit().await.unwrap();
    let mut writer = h.begin().await.unwrap();
    let all = writer.env().records("exact_numeric.item").unwrap();
    let call = UpsertCall::new("_code_key")
        .with_rows([numeric("a", "amount", "0.1")])
        .with_accumulate("amount");
    let error = all.upsert(&mut writer, call).await.unwrap_err();
    check_field(&error, "amount", 1);
    let commit = writer.commit().await.unwrap_err();
    check_field(&commit, "amount", 1);
    drop(writer);
    let mut reader = h.begin().await.unwrap();
    assert_eq!(
        support::one(&mut reader, "SELECT amount::text FROM exact_numeric_item")
            .await
            .as_deref(),
        Some("1.24")
    );
    drop(reader);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn named_defaults_and_child_commands_obey_the_same_admission() {
    let h = computed_harness().await;
    let mut tx = h.begin().await.unwrap();
    precision(&mut tx, 2).await;
    let all = tx.env().records("exact_numeric.item").unwrap();
    let defaulted = all.with_context([("default_amount", Value::str("1.234"))]);
    let error = tx
        .savepoint(async |tx| {
            defaulted
                .create(
                    tx,
                    vec![RawVals::new().set_cell("code", Cell::Str("default".into()))],
                )
                .await
        })
        .await
        .unwrap_err();
    check_field(&error, "amount", 2);
    let parent = numeric("parent", "amount", "1.23").set_commands(
        "children",
        [erp_orm::Command::create(numeric(
            "child", "amount", "1.234",
        ))],
    );
    let error = tx
        .savepoint(async |tx| all.create(tx, vec![parent]).await)
        .await
        .unwrap_err();
    check_field(&error, "amount", 2);
    assert_eq!(
        support::one(&mut tx, "SELECT count(*)::text FROM exact_numeric_item")
            .await
            .as_deref(),
        Some("0")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
