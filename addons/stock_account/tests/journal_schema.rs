// SPDX-License-Identifier: MIT OR Apache-2.0
//! Journal and precision evidence storage, without posting or quantization.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod common;
use common::*;

async fn journal(tx: &mut erp_sdk::prelude::Transaction, sid: i64) -> i64 {
    insert(tx,&format!("INSERT INTO stk_value_journal(source_id,company_id,product_id,seq,role,quantity_delta,grid_delta,booked_delta,currency_id,currency_increment,rounding_mode,method,policy,category_id,effective_time,recognition_date,generation,recorded_at) VALUES({sid},101,101,0,'initial',1,10,10,(SELECT id FROM res_currency LIMIT 1),0.01,'half_away_from_zero','average','cost_scale_v1',101,'2026-01-01','2026-01-01',0,'2026-01-01') RETURNING id")).await.unwrap()
}

#[tokio::test]
async fn journal_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let sid = source(&mut tx, "j").await;
    let id = journal(&mut tx, sid).await;
    columns(
        &mut tx,
        "stk_value_journal",
        &[
            ("grid_delta", "numeric", false),
            ("quantity_delta", "numeric", false),
            ("booked_delta", "numeric", false),
            ("currency_increment", "numeric", false),
            ("recognition_date", "date", false),
            ("lot_id", "bigint", true),
            ("cause_journal_id", "bigint", true),
        ],
    )
    .await;
    finite(
        &mut tx,
        "stk_value_journal",
        id,
        &[
            "quantity_delta",
            "grid_delta",
            "booked_delta",
            "currency_increment",
        ],
    )
    .await;
    for (assignment, key) in [
        ("currency_increment=0", "_increment_positive"),
        (
            "seq=-1,role='mapping_transfer',quantity_delta=0",
            "_seq_nonneg",
        ),
        ("role='mapping_transfer',quantity_delta=0", "_role_seq"),
        ("role='mapping_transfer',seq=1", "_adjustment_no_quantity"),
        (
            "role='adjustment',seq=1,quantity_delta=0",
            "_adjustment_has_cause",
        ),
    ] {
        rejected(
            &mut tx,
            &format!("UPDATE stk_value_journal SET {assignment} WHERE id={id}"),
            "stk_value_journal",
            key,
        )
        .await;
    }
    unique(&mut tx,"INSERT INTO stk_value_journal(source_id,company_id,product_id,seq,role,quantity_delta,grid_delta,booked_delta,currency_id,currency_increment,rounding_mode,method,policy,category_id,effective_time,recognition_date,generation,recorded_at) SELECT source_id,company_id,product_id,seq,role,quantity_delta,grid_delta,booked_delta,currency_id,currency_increment,rounding_mode,method,policy,category_id,effective_time,recognition_date,generation,recorded_at FROM stk_value_journal","stk_value_journal","_identity").await;
    index(
        &mut tx,
        "stk_value_journal_cost_key_effective",
        &["company_id, product_id, lot_id, effective_time, source_id, id"],
    )
    .await;
    index(
        &mut tx,
        "stk_value_journal_recognition",
        &["company_id, recognition_date, id"],
    )
    .await;
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn remainder_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let sid = source(&mut tx, "rem").await;
    let jid = journal(&mut tx, sid).await;
    columns(
        &mut tx,
        "stk_value_remainder",
        &[
            ("journal_id", "bigint", false),
            ("operand_value", "numeric", true),
            ("operand_quantity", "numeric", true),
            ("operand_base_quantity", "numeric", true),
            ("remainder_num", "numeric", false),
            ("remainder_den", "numeric", false),
        ],
    )
    .await;
    let id=insert(&mut tx,&format!("INSERT INTO stk_value_remainder(journal_id,company_id,policy,boundary,remainder_num,remainder_den,recorded_at) VALUES({jid},101,'cost_scale_v1','source_value',0,1,'2026-01-01') RETURNING id")).await.unwrap();
    finite(
        &mut tx,
        "stk_value_remainder",
        id,
        &[
            "operand_value",
            "operand_quantity",
            "operand_base_quantity",
            "remainder_num",
            "remainder_den",
        ],
    )
    .await;
    unique(&mut tx,&format!("INSERT INTO stk_value_remainder(journal_id,company_id,policy,boundary,remainder_num,remainder_den,recorded_at) VALUES({jid},101,'cost_scale_v1','source_value',0,1,'2026-01-01')"),"stk_value_remainder","_journal").await;
    let mismatch = tx
        .savepoint(async |tx| {
            tx.execute(
                "UPDATE stk_value_remainder SET company_id=102 WHERE id=$1",
                &[&id],
            )
            .await
            .map(|_| ())
        })
        .await
        .unwrap_err();
    foreign_error(&tx, &mismatch, "stk_value_remainder", "23503");
    tx.rollback().await.unwrap();
}
