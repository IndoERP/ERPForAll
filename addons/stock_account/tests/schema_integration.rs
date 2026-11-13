// SPDX-License-Identifier: MIT OR Apache-2.0
//! Registry ordering, generic numeric failure and dependency boundaries.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod common;
use common::*;

const MODELS: [&str; 10] = [
    "stock.valuation.source",
    "stock.valuation.source.alloc",
    "stock.valuation.receipt",
    "stock.valuation.journal",
    "stock.valuation.remainder",
    "stock.valuation.state",
    "stock.valuation.control",
    "stock.valuation.checkpoint",
    "stock.valuation.layer",
    "stock.valuation.layer.consumption",
];

#[tokio::test]
async fn valuation_models_are_registered_target_first_and_start_empty() {
    let h = harness().await;
    let actual: Vec<&str> = h
        .registry()
        .models()
        .iter()
        .filter(|m| m.name.starts_with("stock.valuation."))
        .map(|m| m.name)
        .collect();
    assert_eq!(actual, MODELS);
    let mut tx = h.begin().await.unwrap();
    for name in MODELS {
        let model = h.registry().model(name).unwrap();
        assert_eq!(
            tx.query(&format!("SELECT count(*)::int8 FROM {}", model.table), &[])
                .await
                .unwrap()[0]
                .get::<_, i64>(0),
            0
        );
        assert!(model.field("company_id").unwrap().required);
        let expected = column_contract(model.table.as_str());
        let actual:Vec<String>=tx.query("SELECT column_name::text FROM information_schema.columns WHERE table_schema=current_schema() AND table_name=$1 ORDER BY column_name",&[&model.table]).await.unwrap().iter().map(|r|r.get(0)).collect();
        let mut expected: Vec<&str> = expected.split_whitespace().collect();
        expected.sort_unstable();
        assert_eq!(actual, expected, "exact columns of {}", model.table);
        let bad=tx.query("SELECT conname::text FROM pg_constraint WHERE conrelid=$1::text::regclass AND contype='f' AND confdeltype <> 'r' AND conname <> $1 || '_create_uid_fkey' AND conname <> $1 || '_write_uid_fkey'",&[&model.table]).await.unwrap();
        assert!(
            bad.is_empty(),
            "every business valuation FK restricts deletion: {}",
            model.table
        );
    }
    assert!(erp_addon_stock_account::ADDON.depends.contains(&"account"));
    assert!(!erp_addon_stock::ADDON.depends.contains(&"account"));
    tx.rollback().await.unwrap();
    assert!(
        h.orm()
            .sync_schema()
            .await
            .unwrap()
            .kept_comments()
            .is_empty()
    );
}

#[tokio::test]
async fn generic_decimal_read_refuses_large_grid_value() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let id=insert(&mut tx,"INSERT INTO stk_cost_state(company_id,product_id,method,policy,grid_value,quantity,currency_carry,currency_id,currency_increment,generation) VALUES(101,101,'average','cost_scale_v1',1e40,1,0,(SELECT id FROM res_currency LIMIT 1),0.01,0) RETURNING id").await.unwrap();
    let rec = tx
        .env()
        .records("stock.valuation.state")
        .unwrap()
        .browse([id])
        .unwrap();
    let error = rec.cell(&mut tx, "grid_value").await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Database);
    assert_eq!(error.message(), "could not decode a column value");
    assert!(std::error::Error::source(&error).is_some());
    tx.rollback().await.unwrap();
}

// Independent column inventory from approved table note 2.1–2.10, with L-2.7 applied.
fn column_contract(table: &str) -> &'static str {
    match table {
        "stk_value_source" => {
            "id company_id product_id origin_model origin_id component source_revision kind cost_partition effective_time affected_from_time affected_from_source_id method policy schema_version semantic_version request_canonical request_digest create_uid create_date write_uid write_date"
        }
        "stk_value_source_alloc" => {
            "id source_id ordinal company_id product_id lot_id move_line_id serial_child_id correction_id side quantity recorded_at"
        }
        "stk_value_receipt" => {
            "id source_id company_id product_id state applied_mode applied_generation applied_at recorded_at"
        }
        "stk_value_journal" => {
            "id source_id company_id product_id lot_id seq role quantity_delta grid_delta booked_delta currency_id currency_increment rounding_mode method policy category_id effective_time recognition_date cause_journal_id cause_source_id generation recorded_at"
        }
        "stk_value_remainder" => {
            "id journal_id company_id policy boundary operand_value operand_quantity operand_base_quantity remainder_num remainder_den recorded_at"
        }
        "stk_cost_state" => {
            "id company_id product_id lot_id method policy grid_value quantity currency_carry currency_id currency_increment valued_through_time valued_through_source_id covered_through_source_id generation row_version"
        }
        "stk_cost_control" => {
            "id company_id product_id worker_epoch seal_revision sealed seal_token sealed_until publication_generation drained_source_id recorded_at"
        }
        "stk_cost_checkpoint" => {
            "id company_id product_id lot_id effective_time source_id grid_value quantity currency_carry currency_id currency_increment generation recorded_at"
        }
        "stk_cost_layer" => {
            "id company_id product_id lot_id source_id kind effective_time original_quantity original_value consumed_quantity consumed_value residual_num residual_den shortage_source_id shortage_policy generation row_version"
        }
        "stk_layer_consumption" => {
            "id layer_id out_source_id seq company_id product_id quantity grid_value generation recorded_at"
        }
        other => panic!("unknown valuation table {other}"),
    }
}
