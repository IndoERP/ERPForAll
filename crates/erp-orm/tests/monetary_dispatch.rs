//! Monetary provider absence in a binary which does not link the base addon.
//! Fixtures use in-memory records; no application schema is needed.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_fields::{Cell, Decimal, ErrorKind, Value};
use erp_orm::{Orm, RawVals, SUPERUSER_ID, StaticPolicy};
use erp_registry::{AddonDescriptor, FieldDecl, KindDecl, ModelDecl, RegistryBuilder};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "monetary_boundary",
    depends: &[],
    exact_only: false,
};
static PROVIDER: ModelDecl = ModelDecl::new("monetary_boundary", "res.currency");
static ITEM: ModelDecl = ModelDecl::new("monetary_boundary", "monetary.item").fields(&[
    FieldDecl::many2one("currency_id", "res.currency"),
    FieldDecl::new(
        "amount",
        KindDecl::Monetary {
            currency_field: Some("currency_id"),
        },
    ),
]);

fn orm() -> Orm {
    let decls = vec![&ITEM, &PROVIDER];
    let registry = RegistryBuilder::new([&ADDON])
        .linked(false)
        .with_decls(decls)
        .build()
        .unwrap();
    let config = std::env::var("DATABASE_URL").unwrap().parse().unwrap();
    let pool = erp_orm::pool::create_pool(config, 1).unwrap();
    Orm::builder(Arc::new(registry), pool)
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

#[tokio::test]
async fn no_currency_needs_no_provider_and_keeps_precision() {
    {
        let orm = orm();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let model = tx.env().records("monetary.item").unwrap();
        let amount = Decimal::new(1470126, 5);
        let row = tx
            .new_record(
                &model,
                RawVals::new().set_cell("amount", Cell::Decimal(amount)),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            row.values(&mut tx, "amount").await.unwrap(),
            vec![Value::Decimal(amount)]
        );
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn present_currency_requires_native_provider() {
    {
        let orm = orm();
        let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
        let model = tx.env().records("monetary.item").unwrap();
        for _ in 0..2 {
            let error = tx
                .new_record(
                    &model,
                    RawVals::new()
                        .set_cell("currency_id", Cell::Id(1))
                        .set_cell("amount", Cell::Decimal(Decimal::new(2675, 3))),
                    None,
                    None,
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Programming);
            assert_eq!(
                error.message(),
                "monetary.item.amount requires res.currency._round_monetary"
            );
        }
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn zero_and_null_shared_write_need_no_provider() {
    let orm = orm();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let model = tx.env().records("monetary.item").unwrap();
    let row = tx
        .new_record(
            &model,
            RawVals::new().set_cell("currency_id", Cell::Id(1)),
            None,
            None,
        )
        .await
        .unwrap();
    for cell in [Cell::Decimal(Decimal::ZERO), Cell::Null] {
        let expected = if matches!(cell, Cell::Null) {
            Value::Float(0.0)
        } else {
            Value::Decimal(Decimal::ZERO)
        };
        row.write(&mut tx, RawVals::new().set_cell("amount", cell))
            .await
            .unwrap();
        assert_eq!(row.values(&mut tx, "amount").await.unwrap(), vec![expected]);
    }
    tx.rollback().await.unwrap();
}
