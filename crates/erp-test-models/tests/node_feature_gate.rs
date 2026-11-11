//! Actual SQL boundary of the feature gate; raw calls do not manufacture an Auth binding.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_fields::ErrorKind;
use erp_testkit::Harness;

async fn database() -> Harness {
    Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap()
}

#[tokio::test]
async fn state_only_snapshot_reads_disabled_generation_without_a_node_identity() {
    let h = database().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO erp_node_feature(key,version) VALUES ('auth.login_principal',1)",
        &[],
    )
    .await
    .unwrap();
    let rows = tx.query(
        "SELECT required_version, enabled, generation FROM public.erp_node_feature_snapshot($1)",
        &[&"auth.login_principal"],
    ).await.expect("the bounded state-only helper must exist before unbound authentication can check activation");
    assert_eq!(
        (
            rows[0].get::<_, i32>(0),
            rows[0].get::<_, bool>(1),
            rows[0].get::<_, i64>(2)
        ),
        (1, false, 0)
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn state_only_snapshot_refuses_missing_state() {
    let h = database().await;
    let mut tx = h.begin().await.unwrap();
    let error = tx
        .query(
            "SELECT * FROM public.erp_node_feature_snapshot($1)",
            &[&"auth.login_principal"],
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(error.sqlstate(), Some("ERF01"));
    assert_eq!(
        error.message(),
        "feature auth.login_principal state is missing"
    );
    // The original exception aborted this transaction. Do not mask it with a later query/25P02.
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn active_protection_rejects_an_unbound_identity() {
    let h = database().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute("INSERT INTO erp_node_feature(key,version,enabled,generation,enrollment_confirmed,enrollment_actor,enrollment_at) VALUES ('auth.login_principal',1,true,1,true,current_user,clock_timestamp())", &[]).await.unwrap();
    let error = tx
        .query(
            "SELECT * FROM public.erp_node_feature_protect($1,$2,NULL::uuid,NULL::bytea,$3,$4)",
            &[&"auth.login_principal", &1i32, &1i64, &true],
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.sqlstate(), error.message()),
        (
            ErrorKind::Database,
            Some("ERF01"),
            "custom login selection requires an active membership-bound rollout"
        )
    );
    tx.rollback().await.unwrap();
}
