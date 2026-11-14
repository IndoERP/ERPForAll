//! Caller access remains enforced when unit values were already read as system.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_sdk::{Cell, Decimal, Domain, ErrorKind, Operation, RawVals};
use erp_testkit::{DomainPolicy, Harness, Otherwise};
#[path = "support/guard_fixture.rs"]
mod guard_fixture;

#[tokio::test]
async fn privileged_warm_unit_cannot_bypass_caller_model_read_denial() {
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule("uom.uom", Operation::Read, Domain::FALSE)
        .build()
        .unwrap();
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    guard_fixture::install(&h).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Reader', 'reader', true, false)",
    )
    .await
    .unwrap();
    let model = tx.env().records("uom.uom").unwrap();
    let unit = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Units".into()))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::ONE)),
            ],
        )
        .await
        .unwrap();
    let system = tx.env();
    let id = unit.ids()[0];
    erp_addon_uom::models::load_unit(&system, &mut tx, id)
        .await
        .unwrap();
    let user = system.with_user(42);
    let error = erp_addon_uom::models::load_unit(&user, &mut tx, id)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Your access rights do not let you read 'Unit of Measure' (uom.uom) records."
    );
    assert_eq!(
        erp_addon_uom::models::load_unit(&system, &mut tx, id)
            .await
            .unwrap()
            .factor(),
        Decimal::ONE
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn caller_must_read_target_and_all_ancestors_even_after_system_warmup() {
    use erp_sdk::{Operator, money::ExactRatio};
    let visible = Domain::condition("active", Operator::Eq, true).unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule("uom.uom", Operation::Read, visible)
        .build()
        .unwrap();
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    guard_fixture::install(&h).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Reader', 'reader', true, false)",
    )
    .await
    .unwrap();
    let model = tx.env().records("uom.uom").unwrap();
    let hidden = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Hidden root".into()))
                    .set_cell("active", Cell::Bool(false)),
            ],
        )
        .await
        .unwrap();
    let child = model
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Visible child".into()))
                    .set_cell("relative_uom_id", Cell::Id(hidden.ids()[0]))
                    .set_cell("relative_factor", Cell::Decimal(Decimal::from(2))),
            ],
        )
        .await
        .unwrap();
    let other = model
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Visible root".into()))],
        )
        .await
        .unwrap();
    let system = tx.env();
    for id in [hidden.ids()[0], child.ids()[0], other.ids()[0]] {
        erp_addon_uom::models::load_unit(&system, &mut tx, id)
            .await
            .unwrap();
    }
    let expected = format!(
        "Your access rights do not let you access 'Unit of Measure' (uom.uom) records.\n\nRecords: uom.uom({},), User: 42",
        hidden.ids()[0]
    );
    let user = system.with_user(42);
    let error = erp_addon_uom::references::register(&user, &mut tx, &[child.ids()[0]])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "You are not allowed to read all unit ancestry required by this operation."
    );
    assert_eq!(
        tx.query("SELECT count(*) FROM uom_reference_use", &[])
            .await
            .unwrap()[0]
            .get::<_, i64>(0),
        0
    );
    let allowed = erp_addon_uom::references::register(&user, &mut tx, &[other.ids()[0]])
        .await
        .unwrap();
    assert_eq!(allowed[0].id(), other.ids()[0]);
    // The source's ancestor is denied, then the destination itself is denied.
    for (source, target) in [(child, other.ids()[0]), (other.clone(), hidden.ids()[0])] {
        for quantity in [Decimal::ZERO, Decimal::ONE] {
            let error = erp_sdk::__private::call_method::<(Decimal, i64), ExactRatio>(
                &mut tx,
                &source.with_user(42),
                "_compute_quantity",
                (quantity, target),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Access);
            assert_eq!(error.message(), expected);
        }
    }
    // A denial does not revoke legitimate access or poison a read transaction.
    let user = system.with_user(42);
    assert_eq!(
        erp_addon_uom::models::load_unit(&user, &mut tx, other.ids()[0])
            .await
            .unwrap()
            .factor(),
        Decimal::ONE
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
