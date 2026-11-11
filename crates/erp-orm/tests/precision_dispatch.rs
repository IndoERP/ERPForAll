//! The core precision boundary in a binary that deliberately does not link the base addon.
//! Only a pool checkout is needed; none of these metadata fixtures sends SQL or changes data.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_fields::{Digits, Error, ErrorKind, Result, Value};
use erp_orm::{
    BoxFuture, ImplKind, MethodFn, MethodImpl, Orm, Records, StaticPolicy, Super, Transaction,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "precision_boundary",
    depends: &[],
    exact_only: false,
};
static ITEM: ModelDecl =
    ModelDecl::new("precision_boundary", "precision.item").fields(&[FieldDecl::float_digits(
        "price",
        Digits::Named("Product Price"),
    )]);
static PRECISION: ModelDecl = ModelDecl::new("precision_boundary", "decimal.precision");

type Provider = MethodFn<(String,), u32>;

fn failed<'a>(
    _tx: &'a mut Transaction,
    _records: &'a Records,
    _args: (String,),
    _sup: Super<(String,), u32>,
) -> BoxFuture<'a, Result<u32>> {
    Box::pin(async {
        Err(Error::new(
            ErrorKind::Database,
            "precision metadata store unavailable",
        ))
    })
}

fn failed_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static F: Provider = failed;
    &F
}

static FAILED: MethodImpl = MethodImpl {
    addon: "precision_boundary",
    model: "decimal.precision",
    method: "_precision_get",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<Provider>,
    entry: failed_entry,
    file: file!(),
    line: line!(),
};

fn orm(with_model: bool, with_provider: bool) -> Orm {
    let mut decls = vec![&ITEM];
    if with_model {
        decls.push(&PRECISION);
    }
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls(decls)
        .build()
        .unwrap();
    // CI supplies the database endpoint; this binary neither creates tables nor reads them.
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must name the test cluster");
    let config = url.parse().expect("valid test cluster configuration");
    let pool = erp_orm::pool::create_pool(config, 1).unwrap();
    let mut builder = Orm::builder(Arc::new(registry), pool).policy(StaticPolicy::AllowAll);
    if with_provider {
        builder = builder.impls([&FAILED]);
    }
    builder.build().unwrap()
}

async fn assert_provider_failure(orm: &Orm, kind: ErrorKind, message: &str) {
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let records = tx.env().records("precision.item").unwrap();
    let before = tx.query_count();
    let described = records
        .fields_get(&mut tx, None, Some(&["string"]))
        .await
        .unwrap();
    assert!(described["price"].contains_key("string"));
    assert!(!described["price"].contains_key("digits"));
    // Repeat after the error to prove that a fallback was not cached.
    for _ in 0..2 {
        let error = records
            .fields_get(&mut tx, None, Some(&["digits"]))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), kind);
        assert_eq!(error.message(), message);
        assert_eq!(tx.get_precision("Product Price"), None);
    }
    assert_eq!(tx.query_count(), before);
}

#[tokio::test]
async fn installed_precision_model_without_adapter_fails_loudly() {
    assert_provider_failure(
        &orm(true, false),
        ErrorKind::Programming,
        "'decimal.precision' object has no attribute '_precision_get'",
    )
    .await;
}

#[tokio::test]
async fn precision_adapter_error_preserves_kind_and_message_without_caching_a_default() {
    assert_provider_failure(
        &orm(true, true),
        ErrorKind::Database,
        "precision metadata store unavailable",
    )
    .await;
}

#[tokio::test]
async fn absent_precision_model_has_no_configured_usage_and_uses_documented_default() {
    let orm = orm(false, false);
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let records = tx.env().records("precision.item").unwrap();
    let before = tx.query_count();
    let described = records
        .fields_get(&mut tx, None, Some(&["digits"]))
        .await
        .unwrap();
    assert_eq!(
        described["price"]["digits"],
        Value::list([Value::Int(16), Value::Int(2)])
    );
    assert_eq!(tx.query_count(), before);
}

#[tokio::test]
async fn named_write_preserves_adapter_failure_instead_of_accepting_a_default() {
    for (provider, kind, message) in [
        (
            false,
            ErrorKind::Programming,
            "'decimal.precision' object has no attribute '_precision_get'",
        ),
        (
            true,
            ErrorKind::Database,
            "precision metadata store unavailable",
        ),
    ] {
        let orm = orm(true, provider);
        let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
        let records = tx.env().records("precision.item").unwrap();
        for _ in 0..2 {
            let error = records
                .create(
                    &mut tx,
                    vec![
                        erp_orm::RawVals::new()
                            .set_cell("price", erp_fields::Cell::Decimal("1.23".parse().unwrap())),
                    ],
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), kind);
            assert_eq!(error.message(), message);
        }
    }
}

#[tokio::test]
async fn absent_precision_model_enforces_documented_two_place_write_default() {
    let orm = orm(false, false);
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let records = tx.env().records("precision.item").unwrap();
    let error = records
        .create(
            &mut tx,
            vec![
                erp_orm::RawVals::new()
                    .set_cell("price", erp_fields::Cell::Decimal("1.234".parse().unwrap())),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Excess decimal scale for precision.item.price (precision 'Product Price'): maximum 2 fractional digits."
    );
}
