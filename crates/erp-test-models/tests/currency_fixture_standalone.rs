//! Shared monetary fixtures in a binary that genuinely does not link the base addon.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
#[path = "support/currency_fixture.rs"]
mod currency_fixture;

use erp_registry::{MODEL_DECLS, RegistryBuilder};
use erp_sdk::{Cell, Decimal, ErrorKind};
use erp_test_models::{base_users, test_impex, test_orm};
use erp_testkit::Harness;

#[test]
fn shared_currency_stub_is_metadata_only_without_linked_base() {
    assert!(!crate::currency_fixture::has_currency_root(
        MODEL_DECLS.iter()
    ));
    let registry = RegistryBuilder::new([&base_users::ADDON, &test_orm::ADDON, &test_impex::ADDON])
        .with_decls(crate::currency_fixture::decls())
        .build()
        .unwrap();
    assert!(registry.model("ir.default").is_none());
    assert!(registry.model("test_orm.currency").is_none());
    assert!(
        registry
            .model("res.currency")
            .unwrap()
            .field("rounding")
            .is_none()
    );
    for (model, amount) in [("test_orm.mixed", "monetary"), ("export.typed", "amount")] {
        assert_eq!(
            registry
                .model(model)
                .unwrap()
                .field(amount)
                .unwrap()
                .ty
                .type_name(),
            "monetary"
        );
        assert_eq!(
            registry
                .model(model)
                .unwrap()
                .field("currency_id")
                .unwrap()
                .ty
                .comodel(),
            Some("res.currency")
        );
    }
}

#[tokio::test]
async fn standalone_shared_fixture_does_not_fabricate_a_rounding_provider() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON, &test_impex::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    for (model, amount) in [("test_orm.mixed", "monetary"), ("export.typed", "amount")] {
        let records = tx.env().records(model).unwrap();
        let error = tx
            .new_record(
                &records,
                erp_sdk::RawVals::new()
                    .set_cell("currency_id", Cell::Id(1))
                    .set_cell(amount, Cell::Decimal(Decimal::new(2675, 3))),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            format!("{model}.{amount} requires res.currency._round_monetary")
        );
    }
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
