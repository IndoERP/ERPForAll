// SPDX-License-Identifier: MIT OR Apache-2.0
//! Cost state, admission control and ordering checkpoints; no coordination protocol.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod common;
use common::*;

#[tokio::test]
async fn state_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    columns(
        &mut tx,
        "stk_cost_state",
        &[
            ("grid_value", "numeric", false),
            ("quantity", "numeric", false),
            ("currency_carry", "numeric", false),
            ("row_version", "bigint", false),
            ("valued_through_time", "timestamp without time zone", true),
            ("valued_through_source_id", "bigint", true),
        ],
    )
    .await;
    let id=insert(&mut tx,"INSERT INTO stk_cost_state(company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation) VALUES(101,101,'average','cost_scale_v1',-1,-1,0,(SELECT id FROM res_currency LIMIT 1),0.01,0) RETURNING id").await.unwrap();
    finite(
        &mut tx,
        "stk_cost_state",
        id,
        &[
            "grid_value",
            "quantity",
            "currency_carry",
            "currency_increment",
        ],
    )
    .await;
    rejected(
        &mut tx,
        &format!("UPDATE stk_cost_state SET valued_through_time='2026-01-01' WHERE id={id}"),
        "stk_cost_state",
        "_valued_through_pair",
    )
    .await;
    rejected(
        &mut tx,
        &format!("UPDATE stk_cost_state SET generation=-1 WHERE id={id}"),
        "stk_cost_state",
        "_generation_nonneg",
    )
    .await;
    unique(&mut tx,"INSERT INTO stk_cost_state(company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation) SELECT company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation FROM stk_cost_state","stk_cost_state","_key").await;
    // Negative stock is valid, as are both signed half-increment endpoints (codec bound later).
    tx.execute(
        "UPDATE stk_cost_state SET currency_carry=-0.005 WHERE id=$1",
        &[&id],
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn control_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    columns(
        &mut tx,
        "stk_cost_control",
        &[
            ("worker_epoch", "integer", false),
            ("seal_revision", "integer", false),
            ("publication_generation", "integer", false),
            ("sealed", "boolean", false),
            ("seal_token", "uuid", true),
            ("sealed_until", "timestamp with time zone", true),
        ],
    )
    .await;
    let id=insert(&mut tx,"INSERT INTO stk_cost_control(company_id,product_id,worker_epoch,seal_revision,publication_generation,sealed,recorded_at) VALUES(101,101,0,0,0,false,'2026-01-01') RETURNING id").await.unwrap();
    let row=tx.query("SELECT worker_epoch,seal_revision,publication_generation,sealed FROM stk_cost_control WHERE id=$1",&[&id]).await.unwrap();
    assert_eq!(row[0].get::<_, i32>(0), 0);
    assert_eq!(row[0].get::<_, i32>(1), 0);
    assert_eq!(row[0].get::<_, i32>(2), 0);
    assert!(!row[0].get::<_, bool>(3));
    for field in ["worker_epoch", "seal_revision", "publication_generation"] {
        rejected(
            &mut tx,
            &format!("UPDATE stk_cost_control SET {field}=-1 WHERE id={id}"),
            "stk_cost_control",
            "_counters_nonneg",
        )
        .await;
    }
    for assignment in [
        "sealed=true",
        "seal_token='00000000-0000-4000-8000-000000000001'",
        "sealed_until=clock_timestamp()",
    ] {
        rejected(
            &mut tx,
            &format!("UPDATE stk_cost_control SET {assignment} WHERE id={id}"),
            "stk_cost_control",
            "_seal_complete",
        )
        .await;
    }
    tx.execute("UPDATE stk_cost_control SET sealed=true,seal_token='00000000-0000-4000-8000-000000000001',sealed_until=clock_timestamp() WHERE id=$1",&[&id]).await.unwrap();
    unique(&mut tx,"INSERT INTO stk_cost_control(company_id,product_id,worker_epoch,seal_revision,publication_generation,sealed,recorded_at) VALUES(101,101,0,0,0,false,'2026-01-01')","stk_cost_control","_key").await;
    assert!(
        h.registry()
            .model("stock.valuation.control")
            .unwrap()
            .field("seal_job_id")
            .is_none(),
        "L-2.7 defers the job reference"
    );
    index(
        &mut tx,
        "stk_cost_control_sealed",
        &["sealed_until", "WHERE sealed"],
    )
    .await;
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn checkpoint_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let sid = source(&mut tx, "checkpoint").await;
    columns(
        &mut tx,
        "stk_cost_checkpoint",
        &[
            ("source_id", "bigint", false),
            ("effective_time", "timestamp without time zone", false),
            ("grid_value", "numeric", false),
            ("quantity", "numeric", false),
            ("currency_carry", "numeric", false),
            ("generation", "integer", false),
        ],
    )
    .await;
    let sql = format!(
        "INSERT INTO stk_cost_checkpoint(company_id,product_id,source_id,effective_time,grid_value,quantity,currency_carry,currency_id,currency_increment,generation,recorded_at) VALUES(101,101,{sid},'2026-01-01',1,1,0,(SELECT id FROM res_currency LIMIT 1),0.01,0,'2026-01-01')"
    );
    let id = insert(&mut tx, &format!("{sql} RETURNING id"))
        .await
        .unwrap();
    finite(
        &mut tx,
        "stk_cost_checkpoint",
        id,
        &[
            "grid_value",
            "quantity",
            "currency_carry",
            "currency_increment",
        ],
    )
    .await;
    rejected(
        &mut tx,
        &format!("UPDATE stk_cost_checkpoint SET generation=-1 WHERE id={id}"),
        "stk_cost_checkpoint",
        "_generation_nonneg",
    )
    .await;
    unique(&mut tx, &sql, "stk_cost_checkpoint", "_position").await;
    tx.rollback().await.unwrap();
}
