// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company-tree accounting configuration; companies are created through the native ORM chain.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_sdk::{Command, ErrorKind, prelude::*};
use erp_testkit::Harness;

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
    ])
    .build()
    .await
    .unwrap()
}
async fn company(tx: &mut Transaction, name: &str, parent: Option<i64>) -> i64 {
    let mut vals = RawVals::new().set_cell("name", Cell::Str(name.into()));
    if let Some(id) = parent {
        vals = vals.set_cell("parent_id", Cell::Id(id));
    } else {
        let rows = tx
            .query("SELECT id::int8 FROM res_currency ORDER BY id LIMIT 1", &[])
            .await
            .unwrap();
        let currency = match rows.first() {
            Some(row) => row.get(0),
            None => erp_testkit::fixtures::seed_currency_in(tx).await.unwrap(),
        };
        vals = vals.set_cell("currency_id", Cell::Id(currency));
    }
    tx.env()
        .records("res.company")
        .unwrap()
        .create(tx, vec![vals])
        .await
        .unwrap()
        .ids()[0]
}
fn account_model(tx: &Transaction, company: i64) -> Records {
    tx.env()
        .with_company(company)
        .unwrap()
        .records("account.account")
        .unwrap()
}
async fn account(
    tx: &mut Transaction,
    company: i64,
    name: &str,
    kind: &str,
    code: &str,
) -> Records {
    account_model(tx, company)
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_cell("account_type", Cell::Str(kind.into()))
                    .set_cell("code", Cell::Str(code.into())),
            ],
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn shared_accounts_keep_company_membership_and_cash_stays_single_company() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Shared root", None).await;
    let branch = company(&mut tx, "Shared branch", Some(root)).await;
    let cash = account(&mut tx, root, "Cash", "asset_cash", "1010").await;
    let error = tx
        .savepoint(async |tx| {
            cash.write(
                tx,
                RawVals::new().set_commands(
                    "company_ids",
                    vec![Command::link(RecId::from_real(branch).unwrap())],
                ),
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "A bank or cash account belongs to one company only."
    );
    let shared = account(&mut tx, root, "Shared asset", "asset_current", "1020").await;
    shared
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "company_ids",
                vec![Command::link(RecId::from_real(branch).unwrap())],
            ),
        )
        .await
        .unwrap();
    let error = tx
        .savepoint(async |tx| {
            shared
                .write(
                    tx,
                    RawVals::new().set_commands("company_ids", vec![Command::clear()]),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Each account needs at least one company; these have none:\n- Shared asset"
    );
    assert_eq!(
        shared.related(&mut tx, "company_ids").await.unwrap().ids(),
        [root, branch]
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn nested_branches_read_the_root_code_and_other_roots_keep_their_code() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let root = company(&mut tx, "Code root", None).await;
    let branch = company(&mut tx, "Code branch", Some(root)).await;
    let leaf = company(&mut tx, "Code leaf", Some(branch)).await;
    let other = company(&mut tx, "Code other", None).await;
    let shared = account(&mut tx, root, "Shared code", "asset_current", "1200").await;
    shared
        .write(
            &mut tx,
            RawVals::new().set_commands(
                "company_ids",
                vec![Command::link(RecId::from_real(other).unwrap())],
            ),
        )
        .await
        .unwrap();
    let other_view = account_model(&tx, other).browse(shared.ids()).unwrap();
    other_view
        .write(
            &mut tx,
            RawVals::new().set_cell("code", Cell::Str("2200".into())),
        )
        .await
        .unwrap();
    for id in [root, branch, leaf] {
        assert_eq!(
            account_model(&tx, id)
                .browse(shared.ids())
                .unwrap()
                .cell(&mut tx, "code")
                .await
                .unwrap(),
            Cell::Str("1200".into())
        );
    }
    assert_eq!(
        other_view.cell(&mut tx, "code").await.unwrap(),
        Cell::Str("2200".into())
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}
