//! Functional admission checks for the bounded native selection capability.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::{ErrorKind, Value};
use erp_orm::{OrmConfig, SelectionContext, SelectionLimits};
use erp_sdk::prelude::*;
use erp_testkit::{FieldPolicy, Harness, RpcHarness};
use std::sync::atomic::{AtomicBool, Ordering};
mod support;
static CANCEL_ENTERED: AtomicBool = AtomicBool::new(false);

erp_sdk::addon!(name = "selection_capability");

#[model(
    name = "selection_capability.item",
    description = "Selection capability"
)]
pub struct Item {
    #[field(selection, selection_method = "_choices")]
    pub choice: Option<Arc<str>>,
    #[field(selection, selection_method = "_choices")]
    pub sibling: Option<Arc<str>>,
    #[field(selection, selection_method = "_missing")]
    pub missing: Option<Arc<str>>,
    #[field(selection, selection_method = "_ordinary")]
    pub ordinary: Option<Arc<str>>,
    #[field(selection, selection_method = "_leaf")]
    pub leaf: Option<Arc<str>>,
    #[field(selection, selection_method = "_failure")]
    pub failure: Option<Arc<str>>,
    #[field(selection, selection = [("static", "Static")])]
    pub fixed: Option<Arc<str>>,
}

#[methods(Item)]
impl Item {
    #[selection]
    async fn _choices(_this: &RecordSet<Item>, ctx: &mut SelectionContext<'_>) -> Result<()> {
        match ctx
            .env()
            .context()
            .get("mode")
            .and_then(|v| match v {
                Value::Str(value) => Some(value.to_string()),
                _ => None,
            })
            .as_deref()
        {
            Some(
                mode @ ("raw_domain" | "raw_nested_domain" | "custom_domain" | "domain_value"
                | "large_domain"),
            ) => {
                use erp_sdk::domain::{Opaque, Value as DValue};
                let value = match mode {
                    "raw_nested_domain" => {
                        DValue::List(vec![DValue::Opaque(Opaque::new(Arc::new(RawSql)))])
                    }
                    "domain_value" => DValue::Domain(Domain::TRUE),
                    "large_domain" => DValue::List(vec![DValue::Int(1); 8192]),
                    _ => DValue::Opaque(Opaque::new(Arc::new(RawSql))),
                };
                let domain = if mode == "custom_domain" {
                    Domain::custom(Arc::new(RawSql))
                } else {
                    Domain::condition("company", Operator::In, value)?
                };
                return ctx
                    .catalog(
                        "selection_capability.catalog",
                        &domain,
                        "key",
                        "label",
                        "id",
                    )
                    .await;
            }
            Some("empty") => return Ok(()),
            Some("caught_failure_details") => {
                assert!(
                    ctx.resolve("selection_capability.item", "failure")
                        .await
                        .is_err()
                );
                return Ok(());
            }
            Some("dynamic_nested") => {
                ctx.charge(2)?;
                let model = ["selection_capability", ".item"].concat();
                let field = ["le", "af"].concat();
                return ctx.resolve(&model, &field).await;
            }
            Some("catalog_order_company") => {
                return ctx
                    .catalog(
                        "selection_capability.catalog",
                        &Domain::TRUE,
                        "key",
                        "label",
                        "company",
                    )
                    .await;
            }
            Some("catalog_domain_company") => {
                return ctx
                    .catalog(
                        "selection_capability.catalog",
                        &Domain::condition("company", Operator::Eq, 1)?,
                        "key",
                        "label",
                        "id",
                    )
                    .await;
            }
            Some("bad_projection") => {
                return ctx
                    .catalog(
                        "selection_capability.catalog",
                        &Domain::TRUE,
                        "key",
                        "alias",
                        "id",
                    )
                    .await;
            }
            Some("nested") => return ctx.resolve("selection_capability.item", "leaf").await,
            Some("caught_missing") => {
                assert!(
                    ctx.resolve("selection_capability.item", "missing")
                        .await
                        .is_err()
                );
                return Ok(());
            }
            Some("cancel") => {
                CANCEL_ENTERED.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            Some("two") => {
                ctx.emit("a", "b")?;
                ctx.emit("c", "d")?;
                return Ok(());
            }
            Some("duplicate") => {
                ctx.emit("é", "日本語")?;
                // Catching the refusal must not turn the metadata operation into success.
                assert!(ctx.emit("é", "other").is_err());
                return Ok(());
            }
            Some("recur") => {
                assert!(
                    ctx.resolve("selection_capability.item", "choice")
                        .await
                        .is_err()
                );
                return Ok(());
            }
            Some("work") => {
                assert!(ctx.charge(usize::MAX).is_err());
                return Ok(());
            }
            Some("catalog") => {
                return ctx
                    .catalog(
                        "selection_capability.catalog",
                        &Domain::TRUE,
                        "key",
                        "label",
                        "id",
                    )
                    .await;
            }
            _ => {}
        }
        ctx.emit("é", "日本語")?;
        Ok(())
    }

    #[selection]
    async fn _leaf(_this: &RecordSet<Item>, ctx: &mut SelectionContext<'_>) -> Result<()> {
        ctx.emit("leaf", "Leaf")
    }

    #[selection]
    async fn _failure(_this: &RecordSet<Item>, _ctx: &mut SelectionContext<'_>) -> Result<()> {
        Err(Error::new(ErrorKind::Database, "selection failure fixture")
            .with_sqlstate(Some("XX000".to_owned()))
            .with_constraint("selection_fixture")
            .with_condition(erp_fields::Condition::InvariantViolation)
            .with_context(Value::str("selection context"))
            .with_source(std::io::Error::other("fixture source")))
    }

    #[method]
    async fn _ordinary(_this: &RecordSet<Item>, _tx: &mut Transaction) -> Result<Value> {
        panic!("ordinary provider must never execute")
    }
}

#[model(
    name = "selection_capability.catalog",
    description = "Selection catalog"
)]
pub struct Catalog {
    #[field(char)]
    pub key: Option<Arc<str>>,
    #[field(text)]
    pub label: Option<Arc<str>>,
    #[field(boolean, default = true)]
    pub active: bool,
    #[field(integer)]
    pub company: i32,
    #[field(text, related = "label")]
    pub alias: Option<Arc<str>>,
}

async fn harness(limits: SelectionLimits) -> Harness {
    Harness::builder([&ADDON])
        .config(OrmConfig {
            selection_limits: limits,
            ..OrmConfig::default()
        })
        .build()
        .await
        .unwrap()
}

async fn choices(tx: &mut Transaction, mode: &str, fields: &[&str]) -> Result<Value> {
    let items = tx
        .env()
        .records("selection_capability.item")?
        .with_context([("mode", Value::str(mode))]);
    let metadata = items
        .fields_get(tx, Some(fields), Some(&["selection"]))
        .await?;
    Ok(metadata[fields[0]]["selection"].clone())
}

#[tokio::test]
async fn specialized_provider_reaches_metadata() {
    let h = Harness::builder([&ADDON]).build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let items = tx.env().records("selection_capability.item").unwrap();
    let metadata = items
        .fields_get(&mut tx, Some(&["choice"]), Some(&["selection"]))
        .await
        .unwrap();
    assert_eq!(
        metadata["choice"]["selection"],
        Value::list([Value::list([Value::str("é"), Value::str("日本語")])])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn limits_are_positive_and_tightening_only() {
    for args in [
        (0, 1, 1, 1),
        (1, 0, 1, 1),
        (1, 1, 0, 1),
        (1, 1, 1, 0),
        (4097, 1, 1, 1),
        (1, 1048577, 1, 1),
        (1, 1, 17, 1),
        (1, 1, 1, 8193),
    ] {
        let err = SelectionLimits::new(args.0, args.1, args.2, args.3).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.to_string(),
            "selection limits must be positive and at most 4096 pairs, 1048576 bytes, depth 16 and 8192 work units"
        );
    }
    SelectionLimits::new(4096, 1048576, 16, 8192).unwrap();
}

#[tokio::test]
async fn unicode_exact_bytes_siblings_and_reset() {
    // UTF-8 charges: 2-byte key and 9-byte label, before copying.
    let h = harness(SelectionLimits::new(2, 11, 16, 8192).unwrap()).await;
    let mut tx = h.begin().await.unwrap();
    choices(&mut tx, "", &["choice"]).await.unwrap();
    let err = choices(&mut tx, "", &["choice", "sibling"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.to_string(),
        "selection 'selection_capability.item.sibling' provider '_choices': byte limit exceeded"
    );
    // Ordinary failure restores operation state, so a separate operation has a new budget.
    choices(&mut tx, "", &["choice"]).await.unwrap();
    assert_eq!(
        choices(&mut tx, "empty", &["choice"]).await.unwrap(),
        Value::list([])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caught_failures_refuse_outer_result_and_old_provider_never_executes() {
    let h = harness(SelectionLimits::default()).await;
    let mut tx = h.begin().await.unwrap();
    for (mode, field, kind, detail) in [
        (
            "duplicate",
            "choice",
            ErrorKind::Programming,
            "duplicate key 'é'",
        ),
        (
            "work",
            "choice",
            ErrorKind::ResourceExhausted,
            "work limit exceeded",
        ),
        (
            "recur",
            "choice",
            ErrorKind::Programming,
            "recursive provider resolution",
        ),
        (
            "",
            "missing",
            ErrorKind::Programming,
            "missing specialized provider",
        ),
        (
            "",
            "ordinary",
            ErrorKind::Programming,
            "provider requires the specialized SelectionContext signature",
        ),
    ] {
        let err = choices(&mut tx, mode, &[field]).await.unwrap_err();
        assert_eq!(err.kind(), kind);
        let method = match field {
            "missing" => "_missing",
            "ordinary" => "_ordinary",
            _ => "_choices",
        };
        assert_eq!(
            err.to_string(),
            format!("selection 'selection_capability.item.{field}' provider '{method}': {detail}")
        );
    }
    let items = tx.env().records("selection_capability.item").unwrap();
    // Attribute filtering must never dispatch even missing/wrong-signature providers.
    let before = tx.query_count();
    items
        .fields_get(&mut tx, None, Some(&["string"]))
        .await
        .unwrap();
    assert_eq!(tx.query_count(), before);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn catalog_reads_pending_values_and_masks_oversized_payload_in_sql() {
    let h = harness(SelectionLimits::new(4, 11, 16, 8192).unwrap()).await;
    let mut tx = h.begin().await.unwrap();
    let catalog = tx.env().records("selection_capability.catalog").unwrap();
    let row = catalog
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("key", Cell::Str("old".into()))
                    .set_cell("label", Cell::Str("old".into())),
            ],
        )
        .await
        .unwrap();
    row.write(
        &mut tx,
        RawVals::new()
            .set_cell("key", Cell::Str("é".into()))
            .set_cell("label", Cell::Str("日本語".into())),
    )
    .await
    .unwrap();
    assert_eq!(
        choices(&mut tx, "catalog", &["choice"]).await.unwrap(),
        Value::list([Value::list([Value::str("é"), Value::str("日本語")])])
    );
    row.write(
        &mut tx,
        RawVals::new().set_cell("label", Cell::Str("x".repeat(1048577).into())),
    )
    .await
    .unwrap();
    let err = choices(&mut tx, "catalog", &["choice"]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ResourceExhausted);
    assert_eq!(
        err.to_string(),
        "selection 'selection_capability.item.choice' provider '_choices': catalog output limit exceeded"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn nested_depth_pairs_and_work_boundaries() {
    for (limits, mode, fields, expected) in [
        (
            SelectionLimits::new(1, 100, 1, 20).unwrap(),
            "nested",
            vec!["choice"],
            Some("depth limit exceeded"),
        ),
        (
            SelectionLimits::new(1, 100, 2, 20).unwrap(),
            "nested",
            vec!["choice"],
            None,
        ),
        (
            SelectionLimits::new(2, 4, 1, 3).unwrap(),
            "two",
            vec!["choice"],
            None,
        ),
        (
            SelectionLimits::new(1, 100, 1, 20).unwrap(),
            "two",
            vec!["choice"],
            Some("pair limit exceeded"),
        ),
        (
            SelectionLimits::new(2, 100, 1, 2).unwrap(),
            "two",
            vec!["choice"],
            Some("work limit exceeded"),
        ),
        (
            SelectionLimits::new(2, 22, 1, 4).unwrap(),
            "",
            vec!["choice", "sibling"],
            None,
        ),
    ] {
        let h = harness(limits).await;
        let mut tx = h.begin().await.unwrap();
        let result = choices(&mut tx, mode, &fields).await;
        match expected {
            Some(detail) => {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::ResourceExhausted);
                let (field, method) = if mode == "nested" {
                    ("leaf", "_leaf")
                } else {
                    ("choice", "_choices")
                };
                assert_eq!(
                    error.message(),
                    format!(
                        "selection 'selection_capability.item.{field}' provider '{method}': {detail}"
                    )
                );
            }
            None => {
                result.unwrap();
            }
        }
        drop(tx);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn catalog_empty_null_duplicate_and_active_context() {
    let h = harness(SelectionLimits::default()).await;
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        choices(&mut tx, "catalog", &["choice"]).await.unwrap(),
        Value::list([])
    );
    let catalog = tx.env().records("selection_capability.catalog").unwrap();
    let rows = catalog
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("key", Cell::Str("a".into()))
                    .set_cell("label", Cell::Str("First".into())),
                RawVals::new()
                    .set_cell("key", Cell::Str("a".into()))
                    .set_cell("label", Cell::Str("Archived".into()))
                    .set_cell("active", Cell::Bool(false)),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        choices(&mut tx, "catalog", &["choice"]).await.unwrap(),
        Value::list([Value::list([Value::str("a"), Value::str("First")])])
    );
    let items = tx
        .env()
        .records("selection_capability.item")
        .unwrap()
        .with_context([
            ("mode", Value::str("catalog")),
            ("active_test", Value::Bool(false)),
        ]);
    let error = items
        .fields_get(&mut tx, Some(&["choice"]), Some(&["selection"]))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "selection 'selection_capability.item.choice' provider '_choices': duplicate key 'a'"
    );
    rows.browse_ids([rows.rec_ids()[0]])
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("key", Cell::Null)
                .set_cell("label", Cell::Str("x".repeat(1048577).into())),
        )
        .await
        .unwrap();
    let error = choices(&mut tx, "catalog", &["choice"]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "selection 'selection_capability.item.choice' provider '_choices': catalog key and label must be non-null text"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn metadata_filters_visibility_and_catalog_authority() {
    use erp_test_models::base_users;
    for denied in ["missing", "ordinary"] {
        let h = Harness::builder([&ADDON, &base_users::ADDON])
            .policy(FieldPolicy::new().deny(
                "selection_capability.item",
                denied,
                erp_orm::FieldOp::Read,
            ))
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        let items = tx
            .env()
            .records("selection_capability.item")
            .unwrap()
            .with_user(42);
        let result = items
            .fields_get(&mut tx, Some(&[denied]), Some(&["selection"]))
            .await
            .unwrap();
        assert!(result.is_empty());
        items.fields_get(&mut tx, None, Some(&[])).await.unwrap();
        drop(tx);
        h.cleanup().await.unwrap();
    }
    for (denied, policy) in [
        FieldPolicy::new().deny_model("selection_capability.catalog"),
        FieldPolicy::new().deny(
            "selection_capability.catalog",
            "key",
            erp_orm::FieldOp::Read,
        ),
        FieldPolicy::new().deny(
            "selection_capability.catalog",
            "label",
            erp_orm::FieldOp::Read,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let h = Harness::builder([&ADDON, &base_users::ADDON])
            .policy(policy)
            .build()
            .await
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        let items = tx
            .env()
            .records("selection_capability.item")
            .unwrap()
            .with_user(42)
            .with_context([("mode", Value::str("catalog"))]);
        let error = items
            .fields_get(&mut tx, Some(&["choice"]), Some(&["selection"]))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        let expected = if denied == 0 {
            "You are not allowed to read 'Selection catalog' (selection_capability.catalog) records.".to_owned()
        } else {
            let field = if denied == 1 { "key" } else { "label" };
            format!(
                "You do not have enough rights to access the field \"{field}\" on Selection catalog (selection_capability.catalog). Please contact your system administrator.\n\nOperation: read"
            )
        };
        assert_eq!(error.message(), expected);
        drop(tx);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn cancellation_cannot_reuse_a_half_completed_operation() {
    let h = harness(SelectionLimits::default()).await;
    let mut tx = h.begin().await.unwrap();
    let items = tx
        .env()
        .records("selection_capability.item")
        .unwrap()
        .with_context([("mode", Value::str("cancel"))]);
    {
        let future = items.fields_get(&mut tx, Some(&["choice"]), Some(&["selection"]));
        tokio::pin!(future);
        tokio::select! {
            result = &mut future => panic!("provider unexpectedly completed: {result:?}"),
            _ = async { while !CANCEL_ENTERED.load(Ordering::SeqCst) { tokio::task::yield_now().await; } } => {}
        }
    }
    let error = choices(&mut tx, "", &["choice"]).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the transaction can no longer be used: an ORM operation was cancelled (its future was dropped) while it was changing the transaction's state; roll back"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

mod overriding {
    use super::*;
    erp_sdk::addon!(
        name = "selection_override",
        depends = ["selection_capability"]
    );
    #[extend(Item, addon = "selection_override")]
    pub struct Override {}
    #[methods(Item, addon = "selection_override")]
    impl Override {
        #[selection(override)]
        async fn _choices(
            this: &RecordSet<Item>,
            ctx: &mut SelectionContext<'_>,
            sup: erp_orm::SelectionSuper,
        ) -> Result<()> {
            ctx.emit("first", "First")?;
            sup.call(ctx, this.as_records()).await
        }
    }
}

#[tokio::test]
async fn specialized_provider_uses_native_mro_and_super() {
    let h = Harness::builder([&ADDON, &overriding::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        choices(&mut tx, "", &["choice"]).await.unwrap(),
        Value::list([
            Value::list([Value::str("first"), Value::str("First")]),
            Value::list([Value::str("é"), Value::str("日本語")]),
        ])
    );
    let records = tx.env().records("selection_capability.item").unwrap();
    let error = records
        .call::<(), Value>(&mut tx, "_choices", ())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    // The full dispatcher message includes generated source file/line locations, which move
    // as this fixture grows. Pin the kind and specialized signature instead of those locations.
    assert!(
        error.message().contains("SelectionContext"),
        "{}",
        error.message()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rpc_uses_specialized_metadata_and_static_conversion_is_unchanged() {
    use erp_rpc::RpcCall;
    use erp_test_models::base_users;
    let h = RpcHarness::builder(
        Harness::builder([&ADDON, &base_users::ADDON]),
        FieldPolicy::new(),
    )
    .host(|_| support::rpc::FixtureHost)
    .build()
    .await
    .unwrap();
    let response = h
        .service()
        .call(RpcCall::json2(
            "/json/2/selection_capability.item/fields_get",
            br#"{"allfields":["choice"],"attributes":["selection"]}"#.to_vec(),
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
    assert_eq!(
        value["choice"]["selection"],
        serde_json::json!([["é", "日本語"]])
    );
    let mut tx = h.harness().begin().await.unwrap();
    let items = tx.env().records("selection_capability.item").unwrap();
    let row = items
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("choice", Cell::Str("unlisted".into()))],
        )
        .await
        .unwrap();
    assert_eq!(
        row.read(&mut tx, &["choice"]).await.unwrap()[0]["choice"],
        Value::str("unlisted")
    );
    let metadata = items
        .fields_get(&mut tx, Some(&["fixed"]), Some(&["selection"]))
        .await
        .unwrap();
    assert_eq!(
        metadata["fixed"]["selection"],
        Value::list([Value::list([Value::str("static"), Value::str("Static")])])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generated_sql_masks_payload_before_the_driver_and_limits_candidates() {
    let h = harness(SelectionLimits::default()).await;
    let mut tx = h.begin().await.unwrap();
    let catalog = tx.env().records("selection_capability.catalog").unwrap();
    catalog
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("key", Cell::Str("a".into()))
                    .set_cell("label", Cell::Str("é".into())),
                RawVals::new()
                    .set_cell("key", Cell::Str("b".into()))
                    .set_cell("label", Cell::Str("日本語".into())),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let before = tx.query_count();
    choices(&mut tx, "catalog", &["choice"]).await.unwrap();
    assert_eq!(
        tx.query_count() - before,
        1,
        "one catalog statement after preparation"
    );
    let spec = catalog.spec();
    let fields = [
        spec.field("key").unwrap().id,
        spec.field("label").unwrap().id,
    ];
    let sql = erp_sql::select::selection_catalog(
        spec,
        fields,
        &erp_sql::select::SearchSql {
            limit: Some(1),
            ..Default::default()
        },
        2,
        3,
    );
    // Exactly 3 UTF-8 bytes in the first pair, no truncation or character-count accounting.
    let rows = tx.query(&sql, &[&3_i64, &3_i64, &2_i64]).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].try_get::<_, Option<&str>>(0).unwrap(), Some("a"));
    assert_eq!(rows[0].try_get::<_, Option<&str>>(1).unwrap(), Some("é"));
    assert!(!rows[0].try_get::<_, bool>(2).unwrap());
    assert_eq!(rows[1].try_get::<_, Option<&str>>(0).unwrap(), None);
    assert_eq!(rows[1].try_get::<_, Option<&str>>(1).unwrap(), None);
    assert!(rows[1].try_get::<_, bool>(2).unwrap());
    // The extra candidate detects the pair boundary, also without transferring its strings.
    let rows = tx.query(&sql, &[&2_i64, &100_i64, &1_i64]).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].try_get::<_, Option<&str>>(1).unwrap(), None);
    assert!(rows[1].try_get::<_, bool>(2).unwrap());
    // A NULL key cannot smuggle an unbounded non-null label through SUM's NULL behavior.
    catalog
        .browse_ids([catalog
            .search(&mut tx, &Domain::TRUE)
            .await
            .unwrap()
            .rec_ids()[0]])
        .write(
            &mut tx,
            RawVals::new()
                .set_cell("key", Cell::Null)
                .set_cell("label", Cell::Str("x".repeat(1048577).into())),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let rows = tx.query(&sql, &[&1_i64, &3_i64, &1_i64]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].try_get::<_, Option<&str>>(0).unwrap(), None);
    assert_eq!(rows[0].try_get::<_, Option<&str>>(1).unwrap(), None);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn catalog_snapshot_local_activation_and_new_node_transaction() {
    let h = harness(SelectionLimits::default()).await;
    let a = h.orm().rebuild(h.registry().clone()).build().unwrap();
    let b = h.orm().rebuild(h.registry().clone()).build().unwrap();
    let mut setup = a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let catalog = setup.env().records("selection_capability.catalog").unwrap();
    let row = catalog
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("key", Cell::Str("lang".into()))
                    .set_cell("label", Cell::Str("Language".into()))
                    .set_cell("active", Cell::Bool(false)),
            ],
        )
        .await
        .unwrap();
    let id = row.rec_ids()[0];
    setup.commit().await.unwrap();
    let mut old_snapshot = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(
        choices(&mut old_snapshot, "catalog", &["choice"])
            .await
            .unwrap(),
        Value::list([])
    );
    let mut writer = a.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    writer
        .env()
        .records("selection_capability.catalog")
        .unwrap()
        .browse_ids([id])
        .write(
            &mut writer,
            RawVals::new().set_cell("active", Cell::Bool(true)),
        )
        .await
        .unwrap();
    let expected = Value::list([Value::list([Value::str("lang"), Value::str("Language")])]);
    assert_eq!(
        choices(&mut writer, "catalog", &["choice"]).await.unwrap(),
        expected
    );
    writer.commit().await.unwrap();
    // Existing REPEATABLE READ snapshot is not silently replaced by provider execution.
    assert_eq!(
        choices(&mut old_snapshot, "catalog", &["choice"])
            .await
            .unwrap(),
        Value::list([])
    );
    let mut fresh = b.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(
        choices(&mut fresh, "catalog", &["choice"]).await.unwrap(),
        expected
    );
    old_snapshot.rollback().await.unwrap();
    fresh.rollback().await.unwrap();
    drop((a, b, writer, old_snapshot, fresh, setup));
    h.cleanup().await.unwrap();
}

#[derive(Debug)]
struct CompanyCatalogPolicy;
impl erp_testkit::TestRules for CompanyCatalogPolicy {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &erp_registry::ModelSpec,
        _op: erp_orm::Operation,
    ) -> Result<()> {
        Ok(())
    }
    fn access_domain(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        _op: erp_orm::Operation,
    ) -> Result<Domain> {
        if model.name != "selection_capability.catalog" {
            return Ok(Domain::TRUE);
        }
        Ok(Domain::condition(
            "company",
            Operator::In,
            erp_sdk::domain::Value::list(
                b.principal()
                    .companies()
                    .allowed()
                    .iter()
                    .map(|company| company.get()),
            ),
        )?)
    }
}

#[tokio::test]
async fn catalog_retains_company_rule_and_refuses_invalid_company_context() {
    use erp_orm::{CompanyId, GroupSet, PrincipalBuilder, Resolution, UserId};
    use erp_test_models::base_users;
    let h = Harness::builder([&ADDON, &base_users::ADDON])
        .policy(erp_testkit::RulesPolicy::new(CompanyCatalogPolicy).unwrap())
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    setup
        .env()
        .records("selection_capability.catalog")
        .unwrap()
        .create(
            &mut setup,
            vec![
                RawVals::new()
                    .set_cell("key", Cell::Str("one".into()))
                    .set_cell("label", Cell::Str("One".into()))
                    .set_cell("company", Cell::Int(1)),
                RawVals::new()
                    .set_cell("key", Cell::Str("two".into()))
                    .set_cell("label", Cell::Str("Two".into()))
                    .set_cell("company", Cell::Int(2)),
            ],
        )
        .await
        .unwrap();
    setup.commit().await.unwrap();
    let one = CompanyId::new(1).unwrap();
    let two = CompanyId::new(2).unwrap();
    let principal = PrincipalBuilder::user(UserId::new(42).unwrap())
        .resolution(
            Resolution::in_id_order(GroupSet::default(), [one, two], std::num::NonZeroU64::MIN)
                .unwrap(),
        )
        .companies(vec![one])
        .build()
        .unwrap();
    let mut tx = h
        .orm()
        .begin_opts(erp_orm::BeginOptions::for_principal(principal))
        .await
        .unwrap();
    assert_eq!(
        choices(&mut tx, "catalog", &["choice"]).await.unwrap(),
        Value::list([Value::list([Value::str("one"), Value::str("One")])])
    );
    let items = tx
        .env()
        .records("selection_capability.item")
        .unwrap()
        .with_context([
            ("mode", Value::str("catalog")),
            ("allowed_company_ids", Value::list([Value::Int(99)])),
        ]);
    let error = items
        .fields_get(&mut tx, Some(&["choice"]), Some(&["selection"]))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Access to unauthorized or invalid companies."
    );
    drop((tx, setup));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn catalog_rpc_applies_record_rules_and_company_context() {
    use erp_test_models::base_users;
    let h = RpcHarness::builder(
        Harness::builder([&ADDON, &base_users::ADDON]),
        erp_testkit::RulesPolicy::new(CompanyCatalogPolicy).unwrap(),
    )
    .host(|_| support::rpc::FixtureHost)
    .build()
    .await
    .unwrap();
    let mut tx = h.harness().begin().await.unwrap();
    tx.env()
        .records("selection_capability.catalog")
        .unwrap()
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("key", Cell::Str("one".into()))
                    .set_cell("label", Cell::Str("One".into()))
                    .set_cell("company", Cell::Int(1)),
                RawVals::new()
                    .set_cell("key", Cell::Str("two".into()))
                    .set_cell("label", Cell::Str("Two".into()))
                    .set_cell("company", Cell::Int(2)),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for (company, expected) in [
        (1, serde_json::json!([["one", "One"]])),
        (2, serde_json::json!([["two", "Two"]])),
    ] {
        let body = serde_json::to_vec(
            &serde_json::json!({"allfields":["choice"], "attributes":["selection"],
            "context":{"mode":"catalog", "allowed_company_ids":[company]}}),
        )
        .unwrap();
        let response = h
            .service()
            .call(erp_rpc::RpcCall::json2(
                "/json/2/selection_capability.item/fields_get",
                body,
                support::rpc::alice(),
            ))
            .await;
        assert_eq!(
            response.status(),
            200,
            "{}",
            String::from_utf8_lossy(response.body())
        );
        let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
        assert_eq!(body["choice"]["selection"], expected);
    }
    let response = h.service().call(erp_rpc::RpcCall::json2("/json/2/selection_capability.item/fields_get",
        br#"{"allfields":["choice"],"attributes":["selection"],"context":{"mode":"catalog","allowed_company_ids":[99]}}"#.to_vec(), support::rpc::alice())).await;
    assert_eq!(response.status(), 403);
    let body: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(body["name"], "erp.exceptions.AccessError");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caught_provider_failure_retains_sqlstate_condition_context_and_source() {
    use std::error::Error as _;
    let h = harness(SelectionLimits::default()).await;
    let mut tx = h.begin().await.unwrap();
    let error = choices(&mut tx, "caught_failure_details", &["choice"])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.message(), "selection failure fixture");
    assert_eq!(error.sqlstate(), Some("XX000"));
    assert_eq!(error.constraint(), Some("selection_fixture"));
    assert_eq!(
        error.condition(),
        Some(erp_fields::Condition::InvariantViolation)
    );
    assert_eq!(error.context(), Some(&Value::str("selection context")));
    let mut source = error.source();
    let mut found = false;
    while let Some(error) = source {
        if error.downcast_ref::<std::io::Error>().is_some() {
            found = true;
            break;
        }
        source = error.source();
    }
    assert!(found, "original provider source retained");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn dynamic_names_remain_borrowed_without_static_coercion() {
    let h = harness(SelectionLimits::default()).await;
    let mut tx = h.begin().await.unwrap();
    for _ in 0..8 {
        assert_eq!(
            choices(&mut tx, "dynamic_nested", &["choice"])
                .await
                .unwrap(),
            Value::list([Value::list([Value::str("leaf"), Value::str("Leaf")])])
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn catalog_projection_domain_and_order_boundaries() {
    use erp_test_models::base_users;
    let h = Harness::builder([&ADDON, &base_users::ADDON])
        .policy(FieldPolicy::new().deny(
            "selection_capability.catalog",
            "company",
            erp_orm::FieldOp::Read,
        ))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin_as(42).await.unwrap();
    for mode in ["catalog_order_company", "catalog_domain_company"] {
        let error = choices(&mut tx, mode, &["choice"]).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Access);
        assert_eq!(
            error.message(),
            "You do not have enough rights to access the field \"company\" on Selection catalog (selection_capability.catalog). Please contact your system administrator.\n\nOperation: read"
        );
    }
    let error = choices(&mut tx, "bad_projection", &["choice"])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "selection catalog 'selection_capability.catalog.alias' requires stored text"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn selection_hooks_are_not_portable_method_descriptors() {
    let described: serde_json::Value = serde_json::from_str(&erp_sdk::describe()).unwrap();
    let contributions = described["contributions"].as_array().unwrap();
    assert!(
        contributions
            .iter()
            .any(|item| item["addon"] == "selection_capability"
                && item["kind"] == "method"
                && item["name"] == "_ordinary")
    );
    assert!(
        !contributions
            .iter()
            .any(|item| item["addon"] == "selection_capability"
                && item["kind"] == "method"
                && item["name"] == "_choices")
    );
    assert!(
        !contributions
            .iter()
            .any(|item| item["addon"] == "selection_override"
                && item["kind"] == "override"
                && item["name"] == "_choices")
    );
}

#[derive(Debug)]
struct RawSql;
impl erp_sdk::domain::ExternalValue for RawSql {
    fn kind(&self) -> erp_sdk::domain::OpaqueKind {
        erp_sdk::domain::OpaqueKind::Sql
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl erp_sdk::domain::CustomDomain for RawSql {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[tokio::test]
async fn catalog_refuses_raw_sql_operands_without_executing_a_statement() {
    let h = harness(SelectionLimits::default()).await;
    let mut tx = h.begin().await.unwrap();
    for (mode, kind, message) in [
        (
            "raw_domain",
            ErrorKind::Programming,
            "selection catalogs require literal or principal-parameter domain values",
        ),
        (
            "raw_nested_domain",
            ErrorKind::Programming,
            "selection catalogs require literal or principal-parameter domain values",
        ),
        (
            "domain_value",
            ErrorKind::Programming,
            "selection catalogs require literal or principal-parameter domain values",
        ),
        (
            "custom_domain",
            ErrorKind::Programming,
            "selection catalogs require scalar domain conditions, not custom nodes",
        ),
        (
            "large_domain",
            ErrorKind::ResourceExhausted,
            "selection 'selection_capability.item.choice' provider '_choices': work limit exceeded",
        ),
    ] {
        let before = tx.query_count();
        let error = choices(&mut tx, mode, &["choice"]).await.unwrap_err();
        assert_eq!(error.kind(), kind, "{mode}");
        assert_eq!(error.message(), message, "{mode}");
        assert_eq!(tx.query_count(), before, "{mode}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}
