//! Small-budget SQL regressions for the fetched tree (SS-1).
#![allow(clippy::unwrap_used, reason = "test assertions")]
use super::LOCK_AND_FETCH_SQL;
use erp_testkit::TestDb;

#[tokio::test]
async fn oversized_metadata_is_refused_before_transfer() {
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    client
        .batch_execute(
            "CREATE TEMP TABLE ir_ui_view (
        id bigint PRIMARY KEY, name text, model text, type text,
        inherit_id bigint, mode text, priority integer, active boolean, arch_db text);
        INSERT INTO ir_ui_view VALUES (1, repeat('n', 64), 'm', 'form', NULL,
            'primary', 16, true, '<form/>');",
        )
        .await
        .unwrap();
    let empty_roots: &[i64] = &[];
    let rows = client
        .query(
            LOCK_AND_FETCH_SQL,
            &[&vec![1_i64], &8_i32, &10_i64, &32_i64, &empty_roots],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    // The crossing row is a size sentinel, never the oversized text payload.
    assert_eq!(rows[0].get::<_, Option<String>>(1), None);
    assert_eq!(rows[0].get::<_, Option<String>>(8), None);
    assert!(rows[0].get::<_, i64>(9) > 32);
    drop(client);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn alias_fetch_bounds_both_count_and_text_before_collection() {
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    client
        .batch_execute(
            "CREATE TEMP TABLE ir_model_data (
        id bigint PRIMARY KEY, res_id bigint, model text, module text, name text);
        INSERT INTO ir_model_data SELECT n, 1, 'ir.ui.view', 'm', n::text
            FROM generate_series(1, 4) n;",
        )
        .await
        .unwrap();
    // A count bound of two fetches only its one crossing row.
    let rows = client
        .query(super::EXTERNAL_IDS_SQL, &[&vec![1_i64], &32_i64, &3_i64])
        .await
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].get::<_, String>(2), "3");
    client
        .batch_execute("UPDATE ir_model_data SET module = repeat('m', 64) WHERE id = 1")
        .await
        .unwrap();
    let rows = client
        .query(super::EXTERNAL_IDS_SQL, &[&vec![1_i64], &32_i64, &3_i64])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, Option<String>>(1), None);
    assert_eq!(rows[0].get::<_, Option<String>>(2), None);
    assert_eq!(rows[0].get::<_, i64>(3), 65);
    drop(client);
    db.cleanup().await.unwrap();
}

#[test]
fn alias_payload_errors_keep_the_limit_kind_and_message() {
    use erp_sdk::views::record::{RecordLimit, ViewRecordError};
    let error = super::check_payload_bytes(65, 32, RecordLimit::AliasBytes).unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.to_string(),
        "the external-id text of the checked views is 65 bytes or more, more than the 32 allowed"
    );
    let source = std::error::Error::source(&error).unwrap();
    assert!(matches!(
        source.downcast_ref::<ViewRecordError>(),
        Some(ViewRecordError::Limit {
            limit: RecordLimit::AliasBytes,
            actual: 65,
            max: 32,
        })
    ));
}
