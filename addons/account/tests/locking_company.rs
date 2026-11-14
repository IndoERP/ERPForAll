// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/locking.rs"]
mod support;
use erp_addon_account::locking::{LockField, LockPatch, change_lock_dates};
use erp_sdk::{ErrorKind, SUPERUSER_ID, prelude::*};
use support::*;

#[tokio::test]
async fn dates_periods_and_typed_audit_commit_together() {
    let h = harness().await;
    let c = company(&h, None).await;
    periods(&h, c).await;
    let stamp = h.db().client().await.unwrap();
    stamp
        .execute(
            "UPDATE res_company SET write_uid = NULL, write_date = '2000-01-01' WHERE id = $1",
            &[&c],
        )
        .await
        .unwrap();
    let mut tx = begin(&h).await;
    change_lock_dates(
        &mut tx,
        c,
        &LockPatch::new()
            .set(LockField::FiscalYear, Some(d(2024, 2, 29)))
            .set(LockField::Sale, Some(d(2024, 1, 15))),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // the change is stamped like an ORM write: the writer and the current UTC time
    let row = stamp
        .query_one(
            "SELECT write_uid::int8, \
             abs(extract(epoch FROM (now() AT TIME ZONE 'UTC') - write_date)) < 300 \
             FROM res_company WHERE id = $1",
            &[&c],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, Option<i64>>(0), Some(SUPERUSER_ID.get()));
    assert!(
        row.get::<_, bool>(1),
        "write_date is not the current UTC time"
    );
    drop(stamp);
    // a duplicated company starts without lock dates: none of them is copied (a copied date
    // would make the duplicate's create refuse)
    let mut tx = begin(&h).await;
    let env = tx.env();
    let data = env
        .records("res.company")
        .unwrap()
        .browse(vec![c])
        .unwrap()
        .copy_data(&mut tx, None)
        .await
        .unwrap();
    let data = data[0].as_ref().unwrap();
    for field in [
        "fiscalyear_lock_date",
        "tax_lock_date",
        "sale_lock_date",
        "purchase_lock_date",
        "hard_lock_date",
        "stock_lock_date",
        "fiscal_period_serial",
    ] {
        assert!(!data.contains_key(field), "{field} is copied");
    }
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        date(&h, c, "fiscalyear_lock_date").await,
        Some(d(2024, 2, 29))
    );
    let client = h.db().client().await.unwrap();
    let states: Vec<String> = client
        .query(
            "SELECT status FROM account_fiscal_period WHERE company_id=$1 ORDER BY number",
            &[&c],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(states, ["closed", "closed", "open"]);
    let row=client.query_one("SELECT subject_id,events::text FROM erp_event.ir_audit_entry WHERE events::text LIKE '%account.lock_date_changed%'",&[]).await.unwrap();
    assert_eq!(row.get::<_, Option<i64>>(0), Some(1));
    let events: erp_sdk::serde_json::Value =
        erp_sdk::serde_json::from_str(&row.get::<_, String>(1)).unwrap();
    assert_eq!(events.as_array().unwrap().len(), 2);
    assert_eq!(events[0]["company_id"], c);
    assert_eq!(events[0]["field"], "fiscalyear_lock_date");
    assert!(events[0]["old_days"].is_null());
    assert_eq!(events[0]["new_days"], d(2024, 2, 29).days());
    drop(client);
    let mut tx = begin(&h).await;
    change_lock_dates(
        &mut tx,
        c,
        &LockPatch::new().set(LockField::FiscalYear, Some(d(2024, 1, 31))),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        date(&h, c, "fiscalyear_lock_date").await,
        Some(d(2024, 1, 31))
    );
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn invalid_patch_and_generic_bypass_leave_dates_unchanged() {
    let h = harness().await;
    let c = company(&h, None).await;
    let mut tx = begin(&h).await;
    let error = change_lock_dates(
        &mut tx,
        c,
        &LockPatch::new()
            .set(LockField::Sale, Some(d(2024, 1, 1)))
            .set(LockField::Tax, Some(d(9999, 1, 1))),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "tax_lock_date cannot be set in the future."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(date(&h, c, "sale_lock_date").await, None);
    let mut tx = begin(&h).await;
    let env = tx.env();
    let error = env
        .records("res.company")
        .unwrap()
        .browse(vec![c])
        .unwrap()
        .write(
            &mut tx,
            RawVals::new().set_cell("stock_lock_date", Cell::Date(d(2024, 1, 1))),
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "stock_lock_date changes only through its stock lock operation."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn hard_lock_activation_is_explicitly_unavailable() {
    let h = harness().await;
    let c = company(&h, None).await;
    let mut tx = begin(&h).await;
    let error = change_lock_dates(
        &mut tx,
        c,
        &LockPatch::new().set(LockField::Hard, Some(d(2024, 1, 1))),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(
        error.message(),
        "Hard lock activation is unavailable: a later release must verify the maintenance drain, entry checks and activation audit first."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.db()
        .client()
        .await
        .unwrap()
        .execute(
            "UPDATE res_company SET hard_lock_date=$2 WHERE id=$1",
            &[&c, &d(2024, 1, 31)],
        )
        .await
        .unwrap();
    for proposed in [None, Some(d(2024, 1, 1))] {
        let mut tx = begin(&h).await;
        let error = change_lock_dates(&mut tx, c, &LockPatch::new().set(LockField::Hard, proposed))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "hard_lock_date cannot be removed or decreased."
        );
        tx.rollback().await.unwrap();
        drop(tx);
    }
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn failed_audit_insert_rolls_back_the_policy_change() {
    let h = harness().await;
    let c = company(&h, None).await;
    periods(&h, c).await;
    h.db().client().await.unwrap().batch_execute("CREATE FUNCTION reject_lock_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'audit failure probe'; END $$; CREATE TRIGGER reject_lock_audit BEFORE INSERT ON erp_event.ir_audit_entry FOR EACH ROW EXECUTE FUNCTION reject_lock_audit();").await.unwrap();
    let mut tx = begin(&h).await;
    change_lock_dates(
        &mut tx,
        c,
        &LockPatch::new().set(LockField::FiscalYear, Some(d(2024, 1, 31))),
    )
    .await
    .unwrap();
    let error = tx.commit().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(
        error.message(),
        "a later release (insertion): the audit INSERT failed (SQLSTATE P0001)"
    );
    drop(tx);
    assert_eq!(date(&h, c, "fiscalyear_lock_date").await, None);
    let client = h.db().client().await.unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM account_fiscal_period WHERE company_id=$1 AND status<>'open'",
                &[&c]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_nonmanager_cannot_change_lock_dates() {
    use erp_sdk::{BeginOptions, Context, UserId};
    let h = erp_testkit::Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
    .policy(erp_testkit::FieldPolicy::new().member(3, ["account.group_account_manager"]))
    .build()
    .await
    .unwrap();
    let c = company(&h, None).await;
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(UserId::new(2).unwrap()).context(Context::from_pairs([(
                "allowed_company_ids",
                Value::list([Value::Int(c)]),
            )])),
        )
        .await
        .unwrap();
    let error = change_lock_dates(
        &mut tx,
        c,
        &LockPatch::new().set(LockField::Sale, Some(d(2024, 1, 1))),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Only accounting administrators can change lock dates."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(date(&h, c, "sale_lock_date").await, None);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generic_soft_writes_are_audited_and_savepoint_rollback_discards_events() {
    let h = harness().await;
    let c = company(&h, None).await;
    let mut tx = begin(&h).await;
    let records = tx
        .env()
        .records("res.company")
        .unwrap()
        .browse(vec![c])
        .unwrap();
    records
        .write(
            &mut tx,
            RawVals::new().set_cell("sale_lock_date", Cell::Date(d(2024, 1, 1))),
        )
        .await
        .unwrap();
    let result: Result<()> = tx
        .savepoint(async |tx| {
            change_lock_dates(
                tx,
                c,
                &LockPatch::new().set(LockField::Sale, Some(d(2024, 2, 1))),
            )
            .await?;
            Err(Error::validation("rollback probe"))
        })
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "rollback probe");
    change_lock_dates(
        &mut tx,
        c,
        &LockPatch::new().set(LockField::Sale, Some(d(2024, 1, 1))),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(date(&h, c, "sale_lock_date").await, Some(d(2024, 1, 1)));
    let client = h.db().client().await.unwrap();
    let events:String=client.query_one("SELECT events::text FROM erp_event.ir_audit_entry WHERE events::text LIKE '%account.lock_date_changed%'",&[]).await.unwrap().get(0);
    assert_eq!(
        erp_sdk::serde_json::from_str::<erp_sdk::serde_json::Value>(&events)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn unavailable_audit_store_cannot_leave_a_company_date_changed() {
    let h = erp_testkit::Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
    .without_audit_store()
    .build()
    .await
    .unwrap();
    let c = company(&h, None).await;
    let mut tx = begin(&h).await;
    let error = change_lock_dates(
        &mut tx,
        c,
        &LockPatch::new().set(LockField::Sale, Some(d(2024, 1, 1))),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal);
    assert_eq!(
        error.message(),
        "a later release (unavailable): the audit store is not ready (the store was not checked yet); nothing was recorded, fix the store (run `erp audit maintain` or the runtime grant publication as the owner role)"
    );
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(date(&h, c, "sale_lock_date").await, None);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn normal_company_creation_stays_available_but_initial_locks_are_refused() {
    let h = harness().await;
    let existing = company(&h, None).await;
    let mut tx = begin(&h).await;
    let currency: i64 = tx
        .query(
            "SELECT currency_id::int8 FROM res_company WHERE id=$1",
            &[&existing],
        )
        .await
        .unwrap()[0]
        .get(0);
    let companies = tx.env().records("res.company").unwrap();
    let values = RawVals::new()
        .set_cell("name", Cell::Str("New unlocked company".into()))
        .set_cell("currency_id", Cell::Id(currency));
    let created = companies
        .create(&mut tx, vec![values.clone()])
        .await
        .unwrap();
    assert_eq!(created.len(), 1);
    assert_eq!(
        created.cell(&mut tx, "sale_lock_date").await.unwrap(),
        Cell::Null
    );
    let error = companies
        .create(
            &mut tx,
            vec![values.set_cell("sale_lock_date", Cell::Date(d(2024, 1, 1)))],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Create the company without lock dates, then change them through the lock policy."
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// of a new company)
#[tokio::test]
async fn company_creation_takes_no_lock_column_from_defaults() {
    let h = harness().await;
    let existing = company(&h, None).await;
    let mut tx = begin(&h).await;
    let currency: i64 = tx
        .query(
            "SELECT currency_id::int8 FROM res_company WHERE id=$1",
            &[&existing],
        )
        .await
        .unwrap()[0]
        .get(0);
    let companies = tx.env().records("res.company").unwrap();
    let values = RawVals::new()
        .set_cell("name", Cell::Str("Defaulted company".into()))
        .set_cell("currency_id", Cell::Id(currency));
    for (field, value) in [
        ("fiscalyear_lock_date", Value::str("2024-01-01")),
        ("tax_lock_date", Value::str("2024-01-01")),
        ("sale_lock_date", Value::str("2024-01-01")),
        ("purchase_lock_date", Value::str("2024-01-01")),
        ("hard_lock_date", Value::str("2024-01-01")),
        ("stock_lock_date", Value::str("2024-01-01")),
        ("fiscal_period_serial", Value::Int(41)),
    ] {
        let model = companies.with_context([(format!("default_{field}"), value)]);
        let row = values.clone();
        // Validation poisons the transaction by contract; isolate each expected refusal so
        // the following successful-create assertion exercises a usable transaction.
        let error = tx
            .savepoint(async move |tx| model.create(tx, vec![row]).await)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation, "{field}");
        assert_eq!(
            error.message(),
            format!(
                "A new company cannot take {field} from a default in the context; create it \
                 without lock dates, then change them through the lock policy."
            ),
            "{field}"
        );
    }
    // without any key the company is created unlocked
    let created = companies.create(&mut tx, vec![values]).await.unwrap();
    assert_eq!(
        created.cell(&mut tx, "sale_lock_date").await.unwrap(),
        Cell::Null
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
