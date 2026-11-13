// SPDX-License-Identifier: MIT OR Apache-2.0
//! Database fixture and catalog assertions shared by valuation schema tests.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_testkit::Harness;

pub async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_addon_web::ADDON,
        &erp_addon_uom::ADDON,
        &erp_addon_product::ADDON,
        &erp_addon_barcodes::ADDON,
        &erp_addon_barcodes_gs1_nomenclature::ADDON,
        &erp_addon_stock::ADDON,
        &erp_addon_analytic::ADDON,
        &erp_addon_account::ADDON,
        &erp_addon_stock_account::ADDON,
    ])
    .build()
    .await
    .unwrap()
}

/// Raw fixtures deliberately avoid future valuation writers and unrelated product create hooks.
pub async fn seed(tx: &mut Transaction) {
    let currency = erp_testkit::fixtures::seed_currency_in(tx).await.unwrap();
    tx.execute("INSERT INTO res_company (id,name,currency_id,parent_path) VALUES (101,'C1',$1,'101/'),(102,'C2',$1,'102/')", &[&currency]).await.unwrap();
    tx.batch("INSERT INTO uom_uom(id,name,relative_factor) VALUES (101,'{\"en_US\":\"Unit\"}',1);
        INSERT INTO product_category(id,name) VALUES (101,'{\"en_US\":\"Category\"}');
        INSERT INTO product_template(id,name,uom_id,type) VALUES (101,'{\"en_US\":\"P1\"}',101,'consu'),(102,'{\"en_US\":\"P2\"}',101,'consu');
        INSERT INTO product_product(id,product_tmpl_id) VALUES (101,101),(102,102);
        INSERT INTO stock_location(id,name,usage) VALUES (101,'Internal','internal'),(102,'Customer','customer');
        INSERT INTO stock_lot(id,name,product_id) VALUES (101,'L1',101),(102,'L2',102);
        INSERT INTO stock_move(id,date,state,procure_method,quantity,open_line_version,company_id,product_id,uom_id,location_id,location_dest_id,product_uom_qty,product_qty,quantity_product_uom,uom_factor,product_uom_factor,product_unit_digits)
          VALUES(101,'2026-01-01','draft','make_to_stock',1,0,101,101,101,101,102,1,1,1,1,1,2);
        INSERT INTO stock_move_line(id,membership,active,correction_seq,child_mode,picked,move_id,company_id,product_id,uom_id,location_id,location_dest_id,quantity,quantity_product_uom,uom_factor,product_uom_factor,product_unit_digits)
          VALUES(101,'open',true,0,false,false,101,101,101,101,101,102,1,1,1,1,2);").await.unwrap();
}

pub async fn source(tx: &mut Transaction, component: &str) -> i64 {
    tx.query("INSERT INTO stk_value_source(company_id,product_id,origin_model,origin_id,component,source_revision,kind,cost_partition,effective_time,affected_from_time,method,policy,schema_version,semantic_version,request_canonical,request_digest)
        VALUES(101,101,'stock.move.line',101,$1,0,'Movement','','2026-01-01','2026-01-01','average','cost_scale_v1',1,1,'{}','digest') RETURNING id", &[&component]).await.unwrap()[0].get(0)
}

pub async fn insert(tx: &mut Transaction, sql: &str) -> Result<i64> {
    tx.savepoint(async |tx| Ok(tx.query(sql, &[]).await?[0].get(0)))
        .await
}

pub async fn rejected(tx: &mut Transaction, sql: &str, table: &str, name: &str) {
    let error = tx
        .savepoint(async |tx| tx.execute(sql, &[]).await.map(|_| ()))
        .await
        .expect_err(sql);
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(
        error.constraint(),
        Some(format!("{table}{name}").as_str()),
        "{error:?}"
    );
    assert_eq!(error.sqlstate(), Some("23514"), "{error:?}");
    let model = tx.env().registry().model_by_table(table).unwrap().name;
    let full = format!("{table}{name}");
    let registry = tx.env().registry().clone();
    let object = registry.model(model).unwrap().table_object(&full).unwrap();
    let erp_sdk::__private::TableObjectKind::Constraint { message, .. } = &object.kind else {
        panic!("check")
    };
    assert_eq!(
        error.message(),
        format!("The operation cannot be completed: {message}")
    );
}

pub async fn columns(tx: &mut Transaction, table: &str, expected: &[(&str, &str, bool)]) {
    for (name, ty, nullable) in expected {
        let rows = tx.query("SELECT data_type::text,is_nullable = 'YES',numeric_precision::int4 FROM information_schema.columns WHERE table_schema=current_schema() AND table_name=$1 AND column_name=$2", &[&table,name]).await.unwrap();
        assert_eq!(rows.len(), 1, "{table}.{name}");
        assert_eq!(rows[0].get::<_, String>(0), *ty, "{table}.{name}");
        assert_eq!(rows[0].get::<_, bool>(1), *nullable, "{table}.{name}");
        if *ty == "numeric" {
            assert_eq!(rows[0].get::<_, Option<i32>>(2), None, "no numeric typmod");
        }
    }
}

pub async fn finite(tx: &mut Transaction, table: &str, id: i64, names: &[&str]) {
    for name in names {
        for value in ["NaN", "Infinity", "-Infinity"] {
            rejected(
                tx,
                &format!("UPDATE {table} SET {name}='{value}'::numeric WHERE id={id}"),
                table,
                &format!("_finite_{name}"),
            )
            .await;
        }
    }
}

pub async fn unique(tx: &mut Transaction, sql: &str, table: &str, name: &str) {
    let error = tx
        .savepoint(async |tx| tx.execute(sql, &[]).await.map(|_| ()))
        .await
        .expect_err(sql);
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(error.sqlstate(), Some("23505"), "{error:?}");
    assert_eq!(
        error.constraint(),
        Some(format!("{table}{name}").as_str()),
        "{error:?}"
    );
    let registry = tx.env().registry().clone();
    let object = registry
        .model_by_table(table)
        .unwrap()
        .table_object(&format!("{table}{name}"))
        .unwrap();
    let message = match &object.kind {
        erp_sdk::__private::TableObjectKind::Constraint { message, .. }
        | erp_sdk::__private::TableObjectKind::UniqueIndex { message, .. } => message,
        other => panic!("unique: {other:?}"),
    };
    assert_eq!(
        error.message(),
        format!("The operation cannot be completed: {message}")
    );
}

pub async fn index(tx: &mut Transaction, name: &str, pieces: &[&str]) {
    let rows=tx.query("SELECT indexdef::text FROM pg_indexes WHERE schemaname=current_schema() AND indexname=$1",&[&name]).await.unwrap();
    assert_eq!(rows.len(), 1, "{name}");
    let actual = rows[0].get::<_, String>(0);
    for piece in pieces {
        assert!(actual.contains(piece), "{name}: {actual} lacks {piece}");
    }
}

/// Complete FK mapper contract, including the scalar-field label or composite-key identifier.
pub fn foreign_error(tx: &Transaction, error: &Error, table: &str, sqlstate: &str) {
    assert_eq!(error.kind(), ErrorKind::Validation, "{error:?}");
    assert_eq!(error.sqlstate(), Some(sqlstate), "{error:?}");
    let constraint = error.constraint().expect("foreign key identity");
    let registry = tx.env().registry().clone();
    let model = registry.model_by_table(table).unwrap();
    let culprit = match model
        .fields
        .iter()
        .find(|f| format!("{table}_{}_fkey", f.name) == constraint)
    {
        Some(field) => format!("'{}' ({})", field.string, field.name),
        None => format!("'{constraint}'"),
    };
    assert_eq!(
        error.message(),
        format!(
            "The operation cannot be completed: Another model is using the record you are trying to delete.\n\nThe troublemaker is: '{}' ({})\nThanks to the following constraint: {culprit}\nHow about archiving the record instead?",
            model.description, model.name
        )
    );
}
