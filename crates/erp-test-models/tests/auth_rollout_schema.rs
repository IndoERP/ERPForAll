//! Rollout validates canonical security schema instead of trusting object names.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod auth_support;
#[tokio::test]
async fn canonical_schema_passes_and_named_check_true_refuses() {
    let h = auth_support::builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    erp_security::auth::schema::check_login_rollout(&mut tx)
        .await
        .unwrap();
    tx.batch("ALTER TABLE erp_login_throttle DROP CONSTRAINT erp_login_throttle_kind_check;ALTER TABLE erp_login_throttle ADD CONSTRAINT erp_login_throttle_kind_check CHECK(true)").await.unwrap();
    let e = erp_security::auth::schema::check_login_rollout(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "login selection rollout requires canonical principal throttle schema"
    );
    tx.rollback().await.unwrap();
}
#[tokio::test]
async fn wrong_predicate_cannot_certify_rollout() {
    let h = auth_support::builder().build().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("DROP INDEX erp_login_throttle_counted_age;CREATE INDEX erp_login_throttle_counted_age ON erp_login_throttle(last_failure_at) WHERE false").await.unwrap();
    let e = erp_security::auth::schema::check_login_rollout(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        e.message(),
        "login selection rollout requires canonical principal throttle schema"
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn schema_convergence_preserves_counters_trust_membership_and_activation() {
    let h = auth_support::builder().build().await.unwrap();
    let client = h.db().client().await.unwrap();
    let fresh = client
        .query_one(
            "SELECT enabled,generation FROM erp_node_feature WHERE key='auth.login_principal'",
            &[],
        )
        .await
        .unwrap();
    assert!(!fresh.get::<_, bool>(0));
    assert_eq!(fresh.get::<_, i64>(1), 0);
    client.batch_execute("INSERT INTO erp_login_throttle(kind,key_hash,failures,last_failure_at) VALUES('principal',decode(repeat('11',32),'hex'),7,clock_timestamp()),('principal_trusted',decode(repeat('22',32),'hex'),1,clock_timestamp());DROP TABLE erp_node_feature_digest;DROP TABLE erp_node_feature;ALTER TABLE erp_node DROP COLUMN capabilities").await.unwrap();
    // Historical storage shape emulation, not an actual old executable.
    let node:String=client.query_one("INSERT INTO erp_node(id,name,host,pid,role,dev,digest,config_digest,build_digest,addons_digest,build_label,registered_at,state,heartbeat_at,expires_at,beat,interval_ms,lease_ms) VALUES(uuidv7(),'legacy','host',1,'app',false,decode(repeat('33',32),'hex'),decode(repeat('33',32),'hex'),decode(repeat('33',32),'hex'),decode(repeat('33',32),'hex'),'legacy',now(),'starting',now(),now()+interval '40 seconds',1,10000,40000) RETURNING id::text",&[]).await.unwrap().get(0);
    let read = "SELECT kind,encode(key_hash,'hex'),failures,extract(epoch FROM last_failure_at)::text FROM erp_login_throttle ORDER BY kind,key_hash";
    let before: Vec<(String, String, i32, String)> = client
        .query(read, &[])
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    assert!(h.orm().sync_schema().await.unwrap().is_empty());
    let converged = client
        .query_one(
            "SELECT enabled,generation FROM erp_node_feature WHERE key='auth.login_principal'",
            &[],
        )
        .await
        .unwrap();
    assert!(!converged.get::<_, bool>(0));
    assert_eq!(converged.get::<_, i64>(1), 0);
    client.batch_execute("INSERT INTO erp_node_feature_digest(feature,digest,version) VALUES('auth.login_principal',decode(repeat('33',32),'hex'),1);UPDATE erp_node_feature SET enabled=true,generation=42,enrollment_confirmed=true,enrollment_actor=current_user,enrollment_at=clock_timestamp() WHERE key='auth.login_principal'").await.unwrap();
    let state_before: (bool, i64, String, String) = {
        let r=client.query_one("SELECT enabled,generation,enrollment_actor::text,extract(epoch FROM enrollment_at)::text FROM erp_node_feature WHERE key='auth.login_principal'",&[]).await.unwrap();
        (r.get(0), r.get(1), r.get(2), r.get(3))
    };
    assert!(h.orm().sync_schema().await.unwrap().is_empty());
    assert!(h.orm().sync_schema().await.unwrap().is_empty());
    let after: Vec<(String, String, i32, String)> = client
        .query(read, &[])
        .await
        .unwrap()
        .into_iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3)))
        .collect();
    assert_eq!(before, after);
    let row=client.query_one("SELECT enabled,generation,enrollment_actor::text,extract(epoch FROM enrollment_at)::text FROM erp_node_feature WHERE key='auth.login_principal'",&[]).await.unwrap();
    assert_eq!(
        (
            row.get::<_, bool>(0),
            row.get::<_, i64>(1),
            row.get::<_, String>(2),
            row.get::<_, String>(3)
        ),
        state_before
    );
    let row = client
        .query_one(
            "SELECT id::text,capabilities::text,beat FROM erp_node WHERE id=$1::text::uuid",
            &[&node],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), node);
    assert_eq!(row.get::<_, String>(1), "{}");
    assert_eq!(row.get::<_, i64>(2), 1);
    assert_eq!(client.query_one("SELECT count(*) FROM erp_node_feature_digest WHERE feature='auth.login_principal' AND digest=decode(repeat('33',32),'hex') AND version=1",&[]).await.unwrap().get::<_,i64>(0),1);
}
