//! Real role publication for feature helpers; no helper is owned by a serving role.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_fields::ErrorKind;
use erp_testkit::Harness;

#[tokio::test]
async fn feature_snapshot_publication_has_nologin_owner_and_no_request_relation_grant() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let maintenance = format!("fm_{}", h.db().name());
    let request = format!("fr_{}", h.db().name());
    let public_only = format!("fp_{}", h.db().name());
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "CREATE ROLE {} NOLOGIN; CREATE ROLE {} NOLOGIN; CREATE ROLE {} NOLOGIN",
        erp_sql::quote(&maintenance),
        erp_sql::quote(&request),
        erp_sql::quote(&public_only)
    ))
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO erp_node_feature(key,version) VALUES ('auth.login_principal',1)",
        &[],
    )
    .await
    .unwrap();
    tx.grant_node_feature_roles(&maintenance, &request)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let rows = tx.query("SELECT r.rolcanlogin, p.prosecdef, r.rolname::text FROM pg_proc p JOIN pg_roles r ON r.oid=p.proowner WHERE p.oid='public.erp_node_feature_snapshot(text)'::regprocedure", &[]).await.unwrap();
    assert_eq!(
        (
            rows[0].get::<_, bool>(0),
            rows[0].get::<_, bool>(1),
            rows[0].get::<_, String>(2)
        ),
        (false, true, maintenance.clone())
    );
    tx.batch(&format!("SET LOCAL ROLE {}", erp_sql::quote(&request)))
        .await
        .unwrap();
    let rows = tx
        .query(
            "SELECT enabled, generation FROM public.erp_node_feature_snapshot($1)",
            &[&"auth.login_principal"],
        )
        .await
        .unwrap();
    assert_eq!(
        (rows[0].get::<_, bool>(0), rows[0].get::<_, i64>(1)),
        (false, 0)
    );
    let error = tx
        .query("SELECT * FROM public.erp_node_feature", &[])
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.sqlstate(), error.message()),
        (
            ErrorKind::Database,
            Some("42501"),
            "permission denied for table erp_node_feature"
        )
    );
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!("SET LOCAL ROLE {}", erp_sql::quote(&public_only)))
        .await
        .unwrap();
    let error = tx
        .query(
            "SELECT * FROM public.erp_node_feature_snapshot($1)",
            &[&"auth.login_principal"],
        )
        .await
        .unwrap_err();
    assert_eq!(
        (error.kind(), error.sqlstate(), error.message()),
        (
            ErrorKind::Database,
            Some("42501"),
            "permission denied for function erp_node_feature_snapshot"
        )
    );
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "DROP OWNED BY {}, {}, {} CASCADE; DROP ROLE {}, {}, {}",
        erp_sql::quote(&maintenance),
        erp_sql::quote(&request),
        erp_sql::quote(&public_only),
        erp_sql::quote(&maintenance),
        erp_sql::quote(&request),
        erp_sql::quote(&public_only)
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn legacy_registration_cannot_bypass_an_enabled_feature() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let maintenance = format!("lm_{}", h.db().name());
    let request = format!("lr_{}", h.db().name());
    let heartbeat = format!("lh_{}", h.db().name());
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "CREATE ROLE {} NOLOGIN; CREATE ROLE {} NOLOGIN; CREATE ROLE {} NOLOGIN",
        erp_sql::quote(&maintenance),
        erp_sql::quote(&request),
        erp_sql::quote(&heartbeat)
    ))
    .await
    .unwrap();
    tx.grant_node_role(&heartbeat).await.unwrap();
    tx.grant_node_feature_roles(&maintenance, &request)
        .await
        .unwrap();
    // Trusted fixture activation: not an operator enrollment or dev/testing activation claim.
    tx.execute("INSERT INTO erp_node_feature(key,version,enabled,generation,enrollment_confirmed,enrollment_actor,enrollment_at) VALUES ('auth.login_principal',1,true,1,true,current_user,clock_timestamp())", &[]).await.unwrap();
    tx.commit().await.unwrap();
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!(
            "BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL ROLE {}",
            erp_sql::quote(&heartbeat)
        ))
        .await
        .unwrap();
    // Historical registration shape omits capabilities, as the pre-rollout binary does.
    let result = client.execute("INSERT INTO public.erp_node(id,name,host,pid,role,dev,digest,config_digest,build_digest,addons_digest,build_label,registered_at,state,heartbeat_at,expires_at,beat,interval_ms,lease_ms) VALUES (uuidv7(),'legacy','host',1,'app',false,$1,$1,$1,$1,'legacy',now(),'starting',now(),now()+interval '40 seconds',1,10000,40000)", &[&&[1u8;32][..]]).await;
    client.batch_execute("ROLLBACK").await.unwrap();
    let mut cleanup = h.begin().await.unwrap();
    cleanup
        .batch(&format!(
            "DROP OWNED BY {}, {}, {} CASCADE; DROP ROLE {}, {}, {}",
            erp_sql::quote(&maintenance),
            erp_sql::quote(&request),
            erp_sql::quote(&heartbeat),
            erp_sql::quote(&maintenance),
            erp_sql::quote(&request),
            erp_sql::quote(&heartbeat)
        ))
        .await
        .unwrap();
    cleanup.commit().await.unwrap();
    let error = result.unwrap_err();
    let error = error.as_db_error().expect("original database policy error");
    assert_eq!(
        (error.code().code(), error.message()),
        (
            "ERF01",
            "node feature auth.login_principal refuses unapproved digest"
        )
    );
}

#[tokio::test]
async fn changed_helper_search_path_cannot_be_published() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let m = format!("im_{}", h.db().name());
    let r = format!("ir_{}", h.db().name());
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!("CREATE ROLE {} NOLOGIN;CREATE ROLE {} NOLOGIN;ALTER FUNCTION public.erp_node_feature_snapshot(text) SET search_path=public,pg_catalog",erp_sql::quote(&m),erp_sql::quote(&r))).await.unwrap();
    let result = tx.grant_node_feature_roles(&m, &r).await;
    tx.rollback().await.unwrap();
    let e = result.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "node feature helper public.erp_node_feature_snapshot(text) differs from this binary"
    );
}
#[tokio::test]
async fn maintenance_business_privileges_refuse_publication() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let m = format!("pm_{}", h.db().name());
    let r = format!("pr_{}", h.db().name());
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "CREATE ROLE {} NOLOGIN;CREATE ROLE {} NOLOGIN;GRANT SELECT ON res_users TO {}",
        erp_sql::quote(&m),
        erp_sql::quote(&r),
        erp_sql::quote(&m)
    ))
    .await
    .unwrap();
    let result = tx.grant_node_feature_roles(&m, &r).await;
    tx.rollback().await.unwrap();
    let e = result.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "node feature maintenance role must not access business tables"
    );
}

#[tokio::test]
async fn heartbeat_membership_cannot_reach_helper_ownership() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let m = format!("hm_{}", h.db().name());
    let r = format!("hr_{}", h.db().name());
    let b = format!("hb_{}", h.db().name());
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "CREATE ROLE {} NOLOGIN;CREATE ROLE {} NOLOGIN;CREATE ROLE {} NOLOGIN",
        erp_sql::quote(&m),
        erp_sql::quote(&r),
        erp_sql::quote(&b)
    ))
    .await
    .unwrap();
    tx.grant_node_role(&b).await.unwrap();
    tx.batch(&format!(
        "GRANT {} TO {}",
        erp_sql::quote(&m),
        erp_sql::quote(&b)
    ))
    .await
    .unwrap();
    let result = tx.grant_node_feature_roles(&m, &r).await;
    tx.rollback().await.unwrap();
    let e = result.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "heartbeat roles must not reach node feature helper ownership"
    );
}
