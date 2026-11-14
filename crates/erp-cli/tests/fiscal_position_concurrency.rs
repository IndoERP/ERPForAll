// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
#[path = "support/fpos.rs"]
mod support;
use erp_sdk::{ErrorKind, prelude::*};
use support::*;

#[tokio::test]
async fn stale_snapshot_cannot_admit_a_second_foreign_vat_identity() {
    let h = harness().await;
    let mut seed = h.begin().await.unwrap();
    let be = country(&mut seed, "BE").await;
    let fr = country(&mut seed, "FR").await;
    let co = company(&mut seed, be, None).await;
    seed.commit().await.unwrap();
    let mut stale = h.begin().await.unwrap();
    stale
        .query("SELECT count(*) FROM res_company", &[])
        .await
        .unwrap();
    let mut first = h.begin().await.unwrap();
    let fp = position(
        &mut first,
        co,
        "First",
        RawVals::new()
            .set_cell("country_id", Cell::Id(fr))
            .set_cell("foreign_vat", text("FR1234")),
    )
    .await;
    let first_id = fp.ids()[0];
    first.commit().await.unwrap();
    let error = model(&stale, co, "account.fiscal.position")
        .create(
            &mut stale,
            vec![
                RawVals::new()
                    .set_cell("name", text("Stale second"))
                    .set_cell("country_id", Cell::Id(fr))
                    .set_cell("foreign_vat", text("FR5678")),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Concurrency);
    assert_eq!(error.sqlstate(), Some("40001"));
    assert_eq!(
        error.message(),
        "could not serialize access due to concurrent update"
    );
    stale.rollback().await.unwrap();
    let mut fresh = h.begin().await.unwrap();
    assert_eq!(
        model(&fresh, co, "account.fiscal.position")
            .browse([first_id])
            .unwrap()
            .cell(&mut fresh, "foreign_vat")
            .await
            .unwrap(),
        text("FR1234")
    );
    let conflict = tx_create(&mut fresh, co, fr, "FR5678").await.unwrap_err();
    refused(
        conflict,
        "Fiscal position \"First\" already uses a different foreign_vat for this country.",
    );
    tx_create(&mut fresh, co, fr, "fr.12-34").await.unwrap();
    fresh.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
async fn tx_create(tx: &mut Transaction, co: i64, country: i64, vat: &str) -> Result<Records> {
    let rec = model(tx, co, "account.fiscal.position");
    tx.savepoint(async |tx| {
        rec.create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", text("Next"))
                    .set_cell("country_id", Cell::Id(country))
                    .set_cell("foreign_vat", text(vat)),
            ],
        )
        .await
    })
    .await
}

#[tokio::test]
async fn rolled_back_fence_does_not_make_a_later_write_skip_validation() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let fr = country(&mut tx, "FR").await;
    let co = company(&mut tx, be, None).await;
    let fp = position(
        &mut tx,
        co,
        "France",
        RawVals::new()
            .set_cell("country_id", Cell::Id(fr))
            .set_cell("foreign_vat", text("FR1234")),
    )
    .await;
    let result: Result<()> = tx
        .savepoint(async |tx| {
            fp.write(tx, RawVals::new().set_cell("foreign_vat", text("FR5678")))
                .await?;
            Err(Error::user("Rollback fixture"))
        })
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::UserError);
    assert_eq!(error.message(), "Rollback fixture");
    assert_eq!(
        fp.cell(&mut tx, "foreign_vat").await.unwrap(),
        text("FR1234")
    );
    refused(
        tx_create(&mut tx, co, fr, "FR5678").await.unwrap_err(),
        "Fiscal position \"France\" already uses a different foreign_vat for this country.",
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn exhausted_company_revision_refuses_a_write_without_wrapping() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let be = country(&mut tx, "BE").await;
    let co = company(&mut tx, be, None).await;
    tx.flush().await.unwrap();
    tx.execute(
        "UPDATE res_company SET fpos_revision=$1 WHERE id=$2",
        &[&i32::MAX, &co],
    )
    .await
    .unwrap();
    let rec = model(&tx, co, "account.fiscal.position");
    let error = tx
        .savepoint(async |tx| {
            rec.create(
                tx,
                vec![RawVals::new().set_cell("name", text("Must not be created"))],
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("22003"));
    assert_eq!(error.message(), "integer out of range");
    let revision: i32 = tx
        .query("SELECT fpos_revision FROM res_company WHERE id=$1", &[&co])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(revision, i32::MAX);
    assert!(rec.search(&mut tx, &Domain::TRUE).await.unwrap().is_empty());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
