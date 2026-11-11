//! Login rollout admission (proposed).
//! These tests exercise served admission, never the fixture-only selection constructor.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod auth_support;

#[tokio::test]
async fn rollout_schema_exposes_feature_state_and_membership_capabilities() {
    let h = auth_support::builder().build().await.expect("harness");
    let mut tx = h.begin().await.expect("begin catalog inspection");
    let rows = tx
        .query(
            "SELECT to_regclass('public.erp_node_feature') IS NOT NULL, \
                    to_regclass('public.erp_node_feature_digest') IS NOT NULL, \
                    EXISTS (SELECT FROM pg_attribute \
                             WHERE attrelid = 'public.erp_node'::regclass \
                               AND attname = 'capabilities' \
                               AND atttypid = 'jsonb'::regtype \
                               AND attnotnull AND NOT attisdropped)",
            &[],
        )
        .await
        .expect("read rollout schema identity");
    let state_table: bool = rows[0].get(0);
    let digest_table: bool = rows[0].get(1);
    let capabilities_column: bool = rows[0].get(2);
    tx.rollback().await.expect("end catalog inspection");
    assert_eq!(
        (state_table, digest_table, capabilities_column),
        (true, true, true),
        "schema sync must create durable rollout admission before a served path can enable it"
    );
}
