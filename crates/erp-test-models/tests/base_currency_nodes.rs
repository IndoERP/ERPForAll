//! Currency protection across independent ORM instances and pools on one database.
//! Recreating one instance exercises cold startup; this is not an OS-process kill test.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::sync::Arc;

use erp_fields::{Cell, ErrorKind, RecId, Value};
use erp_orm::{Orm, RawVals, SUPERUSER_ID, StaticPolicy};
use erp_test_models::base_users;
use erp_testkit::Harness;
use erp_testkit::deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use erp_testkit::tokio_postgres::NoTls;

fn independent_node(harness: &Harness) -> Orm {
    let manager = Manager::from_config(
        harness.db().config(),
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let pool = Pool::builder(manager)
        .max_size(2)
        .runtime(Runtime::Tokio1)
        .build()
        .unwrap();
    Orm::builder(Arc::clone(harness.registry()), pool)
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

async fn refuses_deactivation(node: &Orm, currency_id: RecId) {
    let mut tx = node.begin(SUPERUSER_ID).await.unwrap();
    let currency = tx
        .env()
        .records("res.currency")
        .unwrap()
        .browse([currency_id.get()])
        .unwrap();
    let error = currency
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
}

async fn observes_protected_currency(node: &Orm, company_id: RecId, currency_id: RecId) {
    let mut tx = node.begin(SUPERUSER_ID).await.unwrap();
    let company = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse([company_id.get()])
        .unwrap();
    assert_eq!(
        company.values(&mut tx, "currency_id").await.unwrap(),
        vec![Value::Int(currency_id.get())]
    );
    let currency = tx
        .env()
        .records("res.currency")
        .unwrap()
        .browse([currency_id.get()])
        .unwrap();
    assert_eq!(
        currency.values(&mut tx, "active").await.unwrap(),
        vec![Value::Bool(true)]
    );
    tx.rollback().await.unwrap();
}

async fn currency_world(node: &Orm, target_active: bool) -> (RecId, RecId) {
    let mut tx = node.begin(SUPERUSER_ID).await.unwrap();
    let currencies = tx.env().records("res.currency").unwrap();
    let created = currencies
        .create(
            &mut tx,
            [("USD", true), ("EUR", target_active)]
                .into_iter()
                .map(|(code, active)| {
                    RawVals::new()
                        .set_cell("name", Cell::Str(code.into()))
                        .set_cell("symbol", Cell::Str(code.into()))
                        .set_cell("rounding", Cell::Decimal("0.01".parse().unwrap()))
                        .set_cell("active", Cell::Bool(active))
                })
                .collect(),
        )
        .await
        .unwrap();
    let initial_id = created.rec_ids()[0];
    let protected_id = created.rec_ids()[1];
    let companies = tx.env().records("res.company").unwrap();
    let company = companies
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Shared company".into()))
                    .set_cell("currency_id", Cell::Id(initial_id.get())),
            ],
        )
        .await
        .unwrap();
    let company_id = company.rec_ids()[0];
    tx.commit().await.unwrap();
    (company_id, protected_id)
}

#[tokio::test]
async fn company_currency_protection_survives_node_recreation() {
    let harness = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let node_a = independent_node(&harness);
    let node_b = independent_node(&harness);

    let (company_id, protected_id) = currency_world(&node_a, true).await;

    // The assignment changes on B after A has created the original company.
    {
        let mut tx = node_b.begin(SUPERUSER_ID).await.unwrap();
        let company = tx
            .env()
            .records("res.company")
            .unwrap()
            .browse([company_id.get()])
            .unwrap();
        company
            .write(
                &mut tx,
                RawVals::new().set_cell("currency_id", Cell::Id(protected_id.get())),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    refuses_deactivation(&node_a, protected_id).await;
    observes_protected_currency(&node_b, company_id, protected_id).await;

    drop(node_a);
    let restarted_a = independent_node(&harness);
    refuses_deactivation(&restarted_a, protected_id).await;
    observes_protected_currency(&node_b, company_id, protected_id).await;

    drop(restarted_a);
    drop(node_b);
    harness.cleanup().await.unwrap();
}

/// A stale transaction cannot archive a currency after another node assigns it to a company.
#[tokio::test]
async fn stale_currency_archive_conflicts_with_committed_company_assignment() {
    let harness = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let node_a = independent_node(&harness);
    let node_b = independent_node(&harness);
    let (company_id, currency_id) = currency_world(&node_a, true).await;
    let mut a = node_a.begin(SUPERUSER_ID).await.unwrap();
    let mut b = node_b.begin(SUPERUSER_ID).await.unwrap();

    // Both nodes take their repeatable-read snapshots before B changes the assignment.
    let currency = a
        .env()
        .records("res.currency")
        .unwrap()
        .browse([currency_id.get()])
        .unwrap();
    assert_eq!(
        currency.values(&mut a, "active").await.unwrap(),
        vec![Value::Bool(true)]
    );
    let company = b
        .env()
        .records("res.company")
        .unwrap()
        .browse([company_id.get()])
        .unwrap();
    let original = company.values(&mut b, "currency_id").await.unwrap();
    assert_ne!(original, vec![Value::Int(currency_id.get())]);
    // Warm active=true in the assigning node: its activation write must still create a
    // database row version even if the ORM skips the unchanged active field itself.
    let assigned_currency = b
        .env()
        .records("res.currency")
        .unwrap()
        .browse([currency_id.get()])
        .unwrap();
    assert_eq!(
        assigned_currency.values(&mut b, "active").await.unwrap(),
        vec![Value::Bool(true)]
    );
    company
        .write(
            &mut b,
            RawVals::new().set_cell("currency_id", Cell::Id(currency_id.get())),
        )
        .await
        .unwrap();
    b.commit().await.unwrap();
    drop(b);

    let error = currency
        .write(&mut a, RawVals::new().set_cell("active", Cell::Bool(false)))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency);
    assert_eq!(error.sqlstate(), Some("40001"));
    assert_eq!(
        error.message(),
        "could not serialize access due to concurrent update"
    );
    a.rollback().await.unwrap();
    drop(a);

    // The fresh retry observes the committed assignment and returns the business refusal.
    refuses_deactivation(&node_a, currency_id).await;
    observes_protected_currency(&node_b, company_id, currency_id).await;
    drop(node_a);
    drop(node_b);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_creation_activates_selected_currency() {
    let harness = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let node_a = independent_node(&harness);
    let node_b = independent_node(&harness);
    let (company_id, currency_id) = {
        let mut tx = node_a.begin(SUPERUSER_ID).await.unwrap();
        let currencies = tx.env().records("res.currency").unwrap();
        let currency = currencies
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("USD".into()))
                        .set_cell("symbol", Cell::Str("$".into()))
                        .set_cell("rounding", Cell::Decimal("0.01".parse().unwrap()))
                        .set_cell("active", Cell::Bool(false)),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            currency.values(&mut tx, "active").await.unwrap(),
            vec![Value::Bool(false)]
        );
        let currency_id = currency.rec_ids()[0];
        let companies = tx.env().records("res.company").unwrap();
        let company = companies
            .create(
                &mut tx,
                vec![
                    RawVals::new()
                        .set_cell("name", Cell::Str("Assigned on create".into()))
                        .set_cell("currency_id", Cell::Id(currency_id.get())),
                ],
            )
            .await
            .unwrap();
        let company_id = company.rec_ids()[0];
        tx.commit().await.unwrap();
        (company_id, currency_id)
    };
    observes_protected_currency(&node_b, company_id, currency_id).await;
    drop(node_a);
    drop(node_b);
    harness.cleanup().await.unwrap();
}

#[tokio::test]
async fn company_currency_change_activates_selected_currency() {
    let harness = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap();
    let node_a = independent_node(&harness);
    let node_b = independent_node(&harness);
    let (company_id, currency_id) = currency_world(&node_a, false).await;
    {
        let mut tx = node_b.begin(SUPERUSER_ID).await.unwrap();
        let currency = tx
            .env()
            .records("res.currency")
            .unwrap()
            .browse([currency_id.get()])
            .unwrap();
        assert_eq!(
            currency.values(&mut tx, "active").await.unwrap(),
            vec![Value::Bool(false)]
        );
        let company = tx
            .env()
            .records("res.company")
            .unwrap()
            .browse([company_id.get()])
            .unwrap();
        company
            .write(
                &mut tx,
                RawVals::new().set_cell("currency_id", Cell::Id(currency_id.get())),
            )
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    observes_protected_currency(&node_a, company_id, currency_id).await;
    drop(node_a);
    drop(node_b);
    harness.cleanup().await.unwrap();
}
