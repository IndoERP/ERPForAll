//! The end-of-run cleanup and the module system's schema records: a test of
//! `load_record_specs.rs` moved here because it needs the `base` addon, which that file's harness
//! (`base_users`, not `base`) does not load.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_data::load::{LoadSession, NoFiles, SessionOptions};
use erp_sdk::{Domain, Records, Transaction};
use erp_test_models::{base_users, test_convert};
use erp_testkit::Harness;

const M: &str = "test_convert";

fn session() -> LoadSession {
    LoadSession::new(SessionOptions::new("20.0", Arc::new(NoFiles)))
}

async fn all(tx: &mut Transaction, model: &str) -> Records {
    tx.env()
        .records(model)
        .unwrap()
        .search(tx, &Domain::TRUE)
        .await
        .unwrap()
}

async fn schema_ids_without_write_date(tx: &mut Transaction) -> i64 {
    let rows = tx
        .query(
            "SELECT count(*) FROM ir_model_data WHERE module = 'test_convert' \
             AND model IN ('ir.model', 'ir.model.fields') AND write_date IS NULL",
            &[],
        )
        .await
        .unwrap();
    rows[0].get::<_, i64>(0)
}

/// The schema records the module system's reflection writes are never obsolete: their external
/// ids have no `write_date` and no data file names them, but the module system, not the cleanup,
/// removes a module's schema (`process_end`).
#[tokio::test]
async fn schema_records_are_never_obsolete() {
    let h = Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &test_convert::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .unlink_hook(erp_data::model_data::UNLINK_HOOK)
    .build()
    .await
    .expect("harness");
    let mut tx = h.begin().await.unwrap();
    erp_orm::reflection::reflect_models(&mut tx).await.unwrap();
    let before = schema_ids_without_write_date(&mut tx).await;
    assert!(
        before > 0,
        "reflection wrote schema external ids without write_date"
    );
    let models_before = all(&mut tx, "ir.model").await.ids();

    let mut session = session();
    session.process_end(&mut tx, &[M]).await.unwrap();

    assert_eq!(schema_ids_without_write_date(&mut tx).await, before);
    assert_eq!(all(&mut tx, "ir.model").await.ids(), models_before);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
