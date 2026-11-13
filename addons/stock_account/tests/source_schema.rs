// SPDX-License-Identifier: MIT OR Apache-2.0
//! Sources, attribution and receipts: schema only, no valuation engine.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod common;
use common::*;
use erp_sdk::prelude::*;

#[tokio::test]
async fn source_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    columns(
        &mut tx,
        "stk_value_source",
        &[
            ("id", "bigint", false),
            ("company_id", "bigint", false),
            ("product_id", "bigint", false),
            ("origin_id", "bigint", false),
            ("component", "character varying", false),
            ("source_revision", "integer", false),
            ("request_canonical", "jsonb", false),
            ("affected_from_source_id", "bigint", true),
            ("effective_time", "timestamp without time zone", false),
        ],
    )
    .await;
    let id = source(&mut tx, "").await;
    let err = tx
        .savepoint(async |tx| Ok(source_duplicate(tx).await?))
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), Some("23505"));
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The operation cannot be completed: A valuation source revision component must be unique."
    );
    for (assignment, key) in [
        ("source_revision=-1", "_revision_nonneg"),
        ("schema_version=0", "_versions_positive"),
        ("semantic_version=0", "_versions_positive"),
        ("affected_from_time='2026-01-02'", "_affected_not_after"),
    ] {
        rejected(
            &mut tx,
            &format!("UPDATE stk_value_source SET {assignment} WHERE id={id}"),
            "stk_value_source",
            key,
        )
        .await;
    }
    index(
        &mut tx,
        "stk_value_source_effective",
        &["company_id, product_id, effective_time, id"],
    )
    .await;
    let row=tx.query("SELECT seqtypid='int8'::regtype,seqincrement,seqcache,seqcycle FROM pg_sequence WHERE seqrelid='stk_value_source_seq'::regclass",&[]).await.unwrap();
    assert!(row[0].get::<_, bool>(0));
    assert_eq!(row[0].get::<_, i64>(1), 1);
    assert_eq!(row[0].get::<_, i64>(2), 1);
    assert!(!row[0].get::<_, bool>(3));
    let mut consumed = 0i64;
    let result: Result<()> = tx
        .savepoint(async |tx| {
            consumed = tx
                .query("SELECT nextval('stk_value_source_seq')", &[])
                .await?[0]
                .get(0);
            Err(Error::validation("rollback fixture"))
        })
        .await;
    let rollback_error = result.unwrap_err();
    assert_eq!(rollback_error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(rollback_error.message(), "rollback fixture");
    assert!(source(&mut tx, "later").await > consumed);
    let model = tx.env().records("stock.valuation.source").unwrap();
    let time = DateTime::parse_odoo("2026-01-01 00:00:00").unwrap();
    let vals = RawVals::new()
        .set_cell("company_id", Cell::Id(101))
        .set_cell("product_id", Cell::Id(101))
        .set_cell("origin_model", Cell::Str("stock.move.line".into()))
        .set_cell("origin_id", Cell::Id(101))
        .set_cell("source_revision", Cell::Int(1))
        .set_cell("kind", Cell::Str("Movement".into()))
        .set_cell("method", Cell::Str("average".into()))
        .set_cell("policy", Cell::Str("cost_scale_v1".into()))
        .set_cell("effective_time", Cell::DateTime(time))
        .set_cell("affected_from_time", Cell::DateTime(time))
        .set_cell("schema_version", Cell::Int(1))
        .set_cell("semantic_version", Cell::Int(1))
        .set_cell(
            "request_canonical",
            Cell::Json(Arc::new(erp_sdk::serde_json::json!({"fixture":true}))),
        )
        .set_cell("request_digest", Cell::Str("fixture-digest".into()));
    let created = model.create(&mut tx, vec![vals]).await.unwrap();
    assert_eq!(
        created.cell(&mut tx, "component").await.unwrap(),
        Cell::Str("".into())
    );
    assert_eq!(
        created.cell(&mut tx, "cost_partition").await.unwrap(),
        Cell::Str("".into())
    );
    tx.rollback().await.unwrap();
}

async fn source_duplicate(tx: &mut Transaction) -> Result<()> {
    tx.execute("INSERT INTO stk_value_source(company_id,product_id,origin_model,origin_id,component,source_revision,kind,cost_partition,effective_time,affected_from_time,method,policy,schema_version,semantic_version,request_canonical,request_digest) SELECT company_id,product_id,origin_model,origin_id,component,source_revision,kind,cost_partition,effective_time,affected_from_time,method,policy,schema_version,semantic_version,request_canonical,request_digest FROM stk_value_source",&[]).await?;
    Ok(())
}

#[tokio::test]
async fn allocation_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let sid = source(&mut tx, "a").await;
    columns(
        &mut tx,
        "stk_value_source_alloc",
        &[
            ("source_id", "bigint", false),
            ("quantity", "numeric", false),
            ("serial_child_id", "bigint", true),
            ("correction_id", "bigint", true),
            ("recorded_at", "timestamp without time zone", false),
        ],
    )
    .await;
    let id=insert(&mut tx,&format!("INSERT INTO stk_value_source_alloc(source_id,ordinal,company_id,product_id,move_line_id,side,quantity,recorded_at) VALUES({sid},0,101,101,101,'original',1,'2026-01-01') RETURNING id")).await.unwrap();
    finite(&mut tx, "stk_value_source_alloc", id, &["quantity"]).await;
    rejected(
        &mut tx,
        &format!("UPDATE stk_value_source_alloc SET ordinal=-1 WHERE id={id}"),
        "stk_value_source_alloc",
        "_ordinal_nonneg",
    )
    .await;
    rejected(
        &mut tx,
        &format!("UPDATE stk_value_source_alloc SET side='old' WHERE id={id}"),
        "stk_value_source_alloc",
        "_side_correction",
    )
    .await;
    unique(&mut tx,&format!("INSERT INTO stk_value_source_alloc(source_id,ordinal,company_id,product_id,move_line_id,side,quantity,recorded_at) VALUES({sid},0,101,101,101,'original',1,'2026-01-01')"),"stk_value_source_alloc","_identity").await;
    index(
        &mut tx,
        "stk_value_source_alloc_cost_key_source",
        &["company_id, product_id, lot_id, source_id"],
    )
    .await;
    for assignment in ["company_id=102", "product_id=102", "lot_id=102"] {
        let err = tx
            .savepoint(async |tx| {
                tx.execute(
                    &format!("UPDATE stk_value_source_alloc SET {assignment} WHERE id={id}"),
                    &[],
                )
                .await
                .map(|_| ())
            })
            .await
            .unwrap_err();
        foreign_error(&tx, &err, "stk_value_source_alloc", "23503");
    }
    tx.batch("INSERT INTO stock_move_line(id,move_id,company_id,product_id,uom_id,location_id,location_dest_id,quantity,quantity_product_uom,uom_factor,product_uom_factor,product_unit_digits,membership,active,correction_seq,child_mode,picked)
        SELECT 102,move_id,company_id,product_id,uom_id,location_id,101,quantity,quantity_product_uom,uom_factor,product_uom_factor,product_unit_digits,membership,active,correction_seq,child_mode,picked FROM stock_move_line WHERE id=101;
        INSERT INTO stock_move_line_serial(id,line_id,move_id,lot_id,active) VALUES(101,101,101,101,true);").await.unwrap();
    tx.execute(
        "UPDATE stk_value_source_alloc SET serial_child_id=101 WHERE id=$1",
        &[&id],
    )
    .await
    .unwrap();
    let child_error = tx
        .savepoint(async |tx| {
            tx.execute(
                "UPDATE stk_value_source_alloc SET move_line_id=102 WHERE id=$1",
                &[&id],
            )
            .await
            .map(|_| ())
        })
        .await
        .unwrap_err();
    assert_eq!(
        child_error.constraint(),
        Some("stk_value_source_alloc_child_line")
    );
    foreign_error(&tx, &child_error, "stk_value_source_alloc", "23503");
    let delete_error = tx
        .savepoint(async |tx| {
            tx.execute("DELETE FROM stk_value_source WHERE id=$1", &[&sid])
                .await
                .map(|_| ())
        })
        .await
        .unwrap_err();
    foreign_error(&tx, &delete_error, "stk_value_source_alloc", "23001");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn receipt_schema() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let sid = source(&mut tx, "r").await;
    columns(
        &mut tx,
        "stk_value_receipt",
        &[
            ("source_id", "bigint", false),
            ("state", "character varying", false),
            ("applied_generation", "integer", true),
            ("applied_mode", "character varying", true),
            ("applied_at", "timestamp without time zone", true),
        ],
    )
    .await;
    let id=insert(&mut tx,&format!("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,recorded_at) VALUES({sid},101,101,'pending','2026-01-01') RETURNING id")).await.unwrap();
    assert_eq!(
        tx.query(
            "SELECT state::text FROM stk_value_receipt WHERE id=$1",
            &[&id]
        )
        .await
        .unwrap()[0]
            .get::<_, String>(0),
        "pending"
    );
    rejected(
        &mut tx,
        &format!("UPDATE stk_value_receipt SET state='applied' WHERE id={id}"),
        "stk_value_receipt",
        "_applied_complete",
    )
    .await;
    tx.execute("UPDATE stk_value_receipt SET state='applied',applied_mode='inline',applied_generation=0,applied_at='2026-01-01' WHERE id=$1",&[&id]).await.unwrap();
    rejected(
        &mut tx,
        &format!("UPDATE stk_value_receipt SET applied_generation=-1 WHERE id={id}"),
        "stk_value_receipt",
        "_generation_nonneg",
    )
    .await;
    for col in ["applied_mode", "applied_generation", "applied_at"] {
        rejected(
            &mut tx,
            &format!("UPDATE stk_value_receipt SET {col}=NULL WHERE id={id}"),
            "stk_value_receipt",
            "_applied_complete",
        )
        .await;
    }
    unique(&mut tx,&format!("INSERT INTO stk_value_receipt(source_id,company_id,product_id,state,recorded_at) VALUES({sid},101,101,'pending','2026-01-01')"),"stk_value_receipt","_source").await;
    index(
        &mut tx,
        "stk_value_receipt_pending",
        &["company_id, product_id, source_id", "state"],
    )
    .await;
    tx.rollback().await.unwrap();
}
