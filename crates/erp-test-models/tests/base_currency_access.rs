//! Global currency integrity must not depend on the caller's company visibility.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_orm::Operation;
use erp_sdk::{Cell, Domain, ErrorKind, Operator, RawVals};
use erp_test_models::base_users;
use erp_testkit::{DomainPolicy, Harness, Otherwise};

#[tokio::test]
async fn hidden_company_still_protects_its_currency_in_a_non_sudo_transaction() {
    let visible = Domain::condition("name", Operator::Eq, "Visible Company").unwrap();
    let policy = DomainPolicy::builder(Otherwise::Allow)
        .rule("res.company", Operation::Read, visible)
        .build()
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(policy)
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    let currency = erp_testkit::fixtures::seed_currency_in(&mut setup)
        .await
        .unwrap();
    setup
        .execute(
            "INSERT INTO res_company (name, currency_id, active) VALUES ('Hidden Company', $1, true)",
            &[&currency],
        )
        .await
        .unwrap();
    setup.execute(
        "INSERT INTO res_users (id, name, login, active, share) VALUES (42, 'Currency editor', 'currency-editor', true, false)",
        &[],
    ).await.unwrap();
    setup.commit().await.unwrap();

    let mut tx = h.begin_as(42).await.unwrap();
    assert!(!tx.env().su());
    let companies = tx.env().records("res.company").unwrap();
    assert_eq!(
        companies
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        0
    );
    let currencies = tx
        .env()
        .records("res.currency")
        .unwrap()
        .browse([currency])
        .unwrap();
    let error = currencies
        .write(
            &mut tx,
            RawVals::new().set_cell("active", Cell::Bool(false)),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "This currency is set on a company and therefore cannot be deactivated."
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
