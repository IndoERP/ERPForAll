// SPDX-License-Identifier: MIT OR Apache-2.0
//! FIFO layer and consumption schema; costing and residual validation remain future slices.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod common;
use common::*;
use erp_sdk::prelude::*;

async fn layer(tx: &mut Transaction, sid: i64) -> i64 {
    insert(tx,&format!("INSERT INTO stk_cost_layer(company_id,product_id,source_id,kind,effective_time,original_quantity,original_value,consumed_quantity,consumed_value,residual_num,residual_den,generation) VALUES(101,101,{sid},'receipt','2026-01-01',3,10,0,0,0,1,0) RETURNING id")).await.unwrap()
}

#[tokio::test]
async fn layer_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let sid = source(&mut tx, "layer").await;
    let id = layer(&mut tx, sid).await;
    columns(
        &mut tx,
        "stk_cost_layer",
        &[
            ("original_quantity", "numeric", false),
            ("original_value", "numeric", false),
            ("consumed_quantity", "numeric", false),
            ("consumed_value", "numeric", false),
            ("residual_num", "numeric", false),
            ("residual_den", "numeric", false),
            ("row_version", "bigint", false),
            ("shortage_source_id", "bigint", true),
            ("shortage_policy", "character varying", true),
        ],
    )
    .await;
    let row=tx.query("SELECT consumed_quantity::text,consumed_value::text,residual_num::text,residual_den::text FROM stk_cost_layer WHERE id=$1",&[&id]).await.unwrap();
    for col in 0..3 {
        assert_eq!(row[0].get::<_, String>(col), "0");
    }
    assert_eq!(row[0].get::<_, String>(3), "1");
    finite(
        &mut tx,
        "stk_cost_layer",
        id,
        &[
            "original_quantity",
            "original_value",
            "consumed_quantity",
            "consumed_value",
            "residual_num",
            "residual_den",
        ],
    )
    .await;
    for (assignment, key) in [
        ("original_quantity=0", "_original_nonzero"),
        ("kind='shortage'", "_shortage_complete"),
        ("consumed_quantity=4", "_consumed_within"),
        ("consumed_quantity=-1", "_consumed_within"),
    ] {
        rejected(
            &mut tx,
            &format!("UPDATE stk_cost_layer SET {assignment} WHERE id={id}"),
            "stk_cost_layer",
            key,
        )
        .await;
    }
    let record = tx
        .env()
        .records("stock.valuation.layer")
        .unwrap()
        .browse([id])
        .unwrap();
    let error = tx
        .savepoint(async |tx| {
            record
                .write(
                    tx,
                    RawVals::new().set_cell("shortage_policy", Cell::Str("invented".into())),
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Wrong value for stock.valuation.layer.shortage_policy: 'invented'"
    );
    unique(&mut tx,&format!("INSERT INTO stk_cost_layer(company_id,product_id,source_id,kind,effective_time,original_quantity,original_value,consumed_quantity,consumed_value,residual_num,residual_den,generation) VALUES(101,101,{sid},'receipt','2026-01-01',3,10,0,0,0,1,0)"),"stk_cost_layer","_identity").await;
    index(
        &mut tx,
        "stk_cost_layer_open_stack",
        &[
            "company_id, product_id, lot_id, effective_time, id",
            "consumed_quantity <> original_quantity",
        ],
    )
    .await;
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn consumption_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let sid = source(&mut tx, "consumption").await;
    let lid = layer(&mut tx, sid).await;
    columns(
        &mut tx,
        "stk_layer_consumption",
        &[
            ("layer_id", "bigint", false),
            ("out_source_id", "bigint", false),
            ("seq", "integer", false),
            ("quantity", "numeric", false),
            ("grid_value", "numeric", false),
            ("generation", "integer", false),
        ],
    )
    .await;
    let sql = format!(
        "INSERT INTO stk_layer_consumption(layer_id,out_source_id,seq,company_id,product_id,quantity,grid_value,generation,recorded_at) VALUES({lid},{sid},0,101,101,-1,-3,0,'2026-01-01')"
    );
    let id = insert(&mut tx, &format!("{sql} RETURNING id"))
        .await
        .unwrap();
    finite(
        &mut tx,
        "stk_layer_consumption",
        id,
        &["quantity", "grid_value"],
    )
    .await;
    unique(&mut tx, &sql, "stk_layer_consumption", "_identity").await;
    let mismatch = tx
        .savepoint(async |tx| {
            tx.execute(
                "UPDATE stk_layer_consumption SET company_id=102 WHERE id=$1",
                &[&id],
            )
            .await
            .map(|_| ())
        })
        .await
        .unwrap_err();
    foreign_error(&tx, &mismatch, "stk_layer_consumption", "23503");
    tx.rollback().await.unwrap();
}
