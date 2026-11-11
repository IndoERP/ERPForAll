//! Operator approval derives capability from durable membership; activation is explicit.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_orm::node::{
    AddonsDigest, BuildIdentity, FeatureKey, FeatureRollout, FeatureSet, FeatureVersion,
    LegacyEnrollment, NodeIdentity, NodeMembership, NodeOptions, NodeRole,
};
use erp_testkit::Harness;
use std::sync::Arc;
fn feature() -> FeatureRollout {
    FeatureRollout::new(
        FeatureKey::new("auth.login_principal").unwrap(),
        FeatureVersion::new(1).unwrap(),
    )
}
async fn fixture(
    capable: bool,
) -> (
    Harness,
    erp_testkit::deadpool_postgres::Pool,
    NodeMembership,
    String,
    String,
) {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let options = NodeOptions::builder()
        .heartbeat_interval_secs(60)
        .build()
        .unwrap();
    let pool = erp_orm::node::heartbeat_pool(h.db().config(), &options).unwrap();
    feature().register(&pool).await.unwrap();
    let mut builder = NodeIdentity::builder(
        NodeRole::App,
        "activation",
        "host",
        BuildIdentity::__from_build(
            "activation",
            "1",
            false,
            Some("0123456789abcdef0123456789abcdef01234567"),
            Some("0"),
        ),
        AddonsDigest::builder().build().unwrap(),
    )
    .options(options);
    if capable {
        builder = builder.features(
            FeatureSet::new([(
                FeatureKey::new("auth.login_principal").unwrap(),
                FeatureVersion::new(1).unwrap(),
            )])
            .unwrap(),
        );
    }
    let identity = builder.create(&pool).await.unwrap();
    let member = NodeMembership::register(Arc::new(identity), pool.clone())
        .await
        .unwrap();
    let maint = format!("am_{}", h.db().name());
    let req = format!("ar_{}", h.db().name());
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "CREATE ROLE {} NOLOGIN;CREATE ROLE {} NOLOGIN",
        erp_sql::quote(&maint),
        erp_sql::quote(&req)
    ))
    .await
    .unwrap();
    tx.grant_node_feature_roles(&maint, &req).await.unwrap();
    tx.commit().await.unwrap();
    (h, pool, member, maint, req)
}
async fn cleanup(h: Harness, member: NodeMembership, m: String, r: String) {
    member.shutdown().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "DROP OWNED BY {},{} CASCADE;DROP ROLE {},{}",
        erp_sql::quote(&m),
        erp_sql::quote(&r),
        erp_sql::quote(&m),
        erp_sql::quote(&r)
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
}
#[tokio::test]
async fn approve_enable_disable_requires_exact_live_capability() {
    let (h, pool, member, m, r) = fixture(true).await;
    let f = feature();
    f.approve(&pool, member.identity().digest()).await.unwrap();
    let enabled = f
        .enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
        .await
        .unwrap();
    assert!(enabled.enabled());
    assert_eq!(enabled.generation(), 1);
    assert_eq!(
        f.enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
            .await
            .unwrap(),
        enabled
    );
    let off = f.disable(&pool).await.unwrap();
    assert!(!off.enabled());
    assert_eq!(off.generation(), 2);
    let on = f
        .enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
        .await
        .unwrap();
    assert_eq!(on.generation(), 3);
    cleanup(h, member, m, r).await;
}
#[tokio::test]
async fn unsupported_starting_member_refuses_without_state_change() {
    let (h, pool, member, m, r) = fixture(false).await;
    let f = feature();
    let e = f
        .approve(&pool, member.identity().digest())
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        e.to_string(),
        format!(
            "database {}: feature auth.login_principal refused: digest has no fresh compatible membership declaration",
            h.db().name()
        )
    );
    let e = f
        .enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
        .await
        .unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        e.to_string(),
        format!(
            "database {}: feature auth.login_principal refused: incompatible fresh or recently expired membership exists: node {} digest {}",
            h.db().name(),
            member.identity().id(),
            member.identity().digest()
        )
    );
    assert!(!f.status(&pool).await.unwrap().enabled());
    cleanup(h, member, m, r).await;
}

async fn blocked_by(h: &Harness, waiter: i32, blocker: i32) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let client = h.db().client().await.unwrap();
        loop {
            if client
                .query_one(
                    "SELECT $2 = ANY(pg_blocking_pids($1))",
                    &[&waiter, &blocker],
                )
                .await
                .unwrap()
                .get::<_, bool>(0)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned backend did not reach feature-lock barrier");
}
#[tokio::test]
async fn disable_waits_for_protected_finalization_transaction() {
    let (h, pool, member, m, r) = fixture(true).await;
    member.request_ready().await.unwrap();
    let f = feature();
    f.approve(&pool, member.identity().digest()).await.unwrap();
    f.enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
        .await
        .unwrap();
    let guarded = h.db().client().await.unwrap();
    guarded
        .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED")
        .await
        .unwrap();
    let id = member.identity().id().to_string();
    let digest = member.identity().digest();
    guarded.query("SELECT * FROM public.erp_node_feature_protect('auth.login_principal',1,$1::text::uuid,$2,1,true)",&[&id,&&digest.as_bytes()[..]]).await.unwrap();
    let blocker: i32 = guarded
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let waiter: i32 = pool
        .get()
        .await
        .unwrap()
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let p = pool.clone();
    let attempt = tokio::spawn(async move { feature().disable(&p).await });
    blocked_by(&h, waiter, blocker).await;
    assert!(!attempt.is_finished());
    guarded.batch_execute("COMMIT").await.unwrap();
    drop(guarded);
    let status = attempt.await.unwrap().unwrap();
    assert!(!status.enabled());
    assert_eq!(status.generation(), 2);
    cleanup(h, member, m, r).await;
}
#[tokio::test]
async fn stale_repeatable_read_protection_refuses_after_disable() {
    let (h, pool, member, m, r) = fixture(true).await;
    member.request_ready().await.unwrap();
    let f = feature();
    f.approve(&pool, member.identity().digest()).await.unwrap();
    f.enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
        .await
        .unwrap();
    let client = h.db().client().await.unwrap();
    client
        .batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    client
        .query_one(
            "SELECT generation FROM erp_node_feature WHERE key='auth.login_principal'",
            &[],
        )
        .await
        .unwrap();
    f.disable(&pool).await.unwrap();
    let id = member.identity().id().to_string();
    let digest = member.identity().digest();
    let result=client.query("SELECT * FROM public.erp_node_feature_protect('auth.login_principal',1,$1::text::uuid,$2,1,true)",&[&id,&&digest.as_bytes()[..]]).await;
    client.batch_execute("ROLLBACK").await.unwrap();
    drop(client);
    cleanup(h, member, m, r).await;
    let e = result.unwrap_err();
    let e = e.as_db_error().unwrap();
    assert_eq!(
        (e.code().code(), e.message()),
        (
            "40001",
            "could not serialize access due to concurrent update"
        )
    );
}
#[tokio::test]
async fn waiting_legacy_insert_cannot_observe_pre_activation_state() {
    let (h, pool, member, m, r) = fixture(true).await;
    let f = feature();
    f.approve(&pool, member.identity().digest()).await.unwrap();
    let gate = h.db().client().await.unwrap();
    gate.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED;UPDATE erp_node_feature SET enabled=true,generation=1,enrollment_confirmed=true,enrollment_actor=current_user,enrollment_at=clock_timestamp() WHERE key='auth.login_principal'").await.unwrap();
    let blocker: i32 = gate
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let old = h.db().client().await.unwrap();
    let waiter: i32 = old
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    // Owner connection executes the historical SQL shape here; role privilege coverage is separate.
    let insert = "INSERT INTO public.erp_node(id,name,host,pid,role,dev,digest,config_digest,build_digest,addons_digest,build_label,registered_at,state,heartbeat_at,expires_at,beat,interval_ms,lease_ms) VALUES(uuidv7(),'legacy','host',1,'app',false,decode(repeat('01',32),'hex'),decode(repeat('01',32),'hex'),decode(repeat('01',32),'hex'),decode(repeat('01',32),'hex'),'legacy',now(),'starting',now(),now()+interval '40 seconds',1,10000,40000)";
    let attempt = tokio::spawn(async move { old.execute(insert, &[]).await });
    blocked_by(&h, waiter, blocker).await;
    gate.batch_execute("COMMIT").await.unwrap();
    drop(gate);
    let result = attempt.await.unwrap();
    let rr = h.db().client().await.unwrap();
    rr.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .await
        .unwrap();
    let rr_result = rr.execute(insert, &[]).await;
    rr.batch_execute("ROLLBACK").await.unwrap();
    drop(rr);
    cleanup(h, member, m, r).await;
    let e = result.unwrap_err();
    let e = e.as_db_error().unwrap();
    assert_eq!(
        (e.code().code(), e.message()),
        (
            "ERF01",
            "node feature auth.login_principal refuses unapproved digest"
        )
    );
    let e = rr_result.unwrap_err();
    let e = e.as_db_error().unwrap();
    assert_eq!(
        (e.code().code(), e.message()),
        (
            "ERF01",
            "node feature membership writes require READ COMMITTED"
        )
    );
}

#[tokio::test]
async fn empty_approval_registry_cannot_activate() {
    let (h, pool, member, m, r) = fixture(true).await;
    member.shutdown().await.unwrap();
    let result = feature()
        .enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
        .await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "DROP OWNED BY {},{} CASCADE;DROP ROLE {},{}",
        erp_sql::quote(&m),
        erp_sql::quote(&r),
        erp_sql::quote(&m),
        erp_sql::quote(&r)
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let e = result.unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        e.to_string(),
        format!(
            "database {}: feature auth.login_principal refused: activation requires at least one approved digest",
            h.db().name()
        )
    );
}
#[tokio::test]
async fn approval_is_disabled_only() {
    let (h, pool, member, m, r) = fixture(true).await;
    let f = feature();
    f.approve(&pool, member.identity().digest()).await.unwrap();
    f.enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
        .await
        .unwrap();
    let expected = format!(
        "database {}: feature auth.login_principal refused: disable the feature before approving a digest",
        h.db().name()
    );
    let result = f.approve(&pool, member.identity().digest()).await;
    cleanup(h, member, m, r).await;
    let e = result.unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(e.to_string(), expected);
}

#[tokio::test]
async fn activation_checks_every_state_and_the_recent_lease_horizon() {
    let (h, pool, member, m, r) = fixture(false).await;
    let db = h.db().name().to_owned();
    let id = member.identity().id().to_string();
    let client = h.db().client().await.unwrap();
    for state in ["starting", "ready", "unready", "stopping"] {
        for recent in [false, true] {
            for dev in [false, true] {
                client.execute("UPDATE erp_node SET state=$2,unready_reason=CASE WHEN $2='unready' THEN 'test' ELSE NULL END,dev=$3,heartbeat_at=clock_timestamp()-interval '10 seconds',expires_at=clock_timestamp()+CASE WHEN $4 THEN interval '-1 second' ELSE interval '20 seconds' END WHERE id=$1::text::uuid",&[&id,&state,&dev,&recent]).await.unwrap();
                let e = feature()
                    .enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
                    .await
                    .unwrap_err();
                assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
                assert_eq!(
                    e.to_string(),
                    format!(
                        "database {db}: feature auth.login_principal refused: incompatible fresh or recently expired membership exists: node {id} digest {}",
                        member.identity().digest()
                    )
                );
            }
        }
    }
    drop(client);
    cleanup(h, member, m, r).await;
}

#[tokio::test]
async fn approval_cardinality_is_bounded_at_32() {
    let (h, pool, member, m, r) = fixture(true).await;
    let mut rows = Vec::new();
    for i in 0_u8..32 {
        let mut digest = *member.identity().digest().as_bytes();
        digest[0] ^= 128;
        digest[31] = i;
        rows.push(digest.to_vec());
    }
    let client = h.db().client().await.unwrap();
    client.execute("INSERT INTO erp_node_feature_digest(feature,digest,version) SELECT 'auth.login_principal',v,1 FROM unnest($1::bytea[]) AS v",&[&rows]).await.unwrap();
    let result = feature().approve(&pool, member.identity().digest()).await;
    let stored: i64 = client
        .query_one("SELECT count(*) FROM erp_node_feature_digest", &[])
        .await
        .unwrap()
        .get(0);
    drop(client);
    let expected = format!(
        "database {}: feature auth.login_principal refused: a feature approves at most 32 digests",
        h.db().name()
    );
    cleanup(h, member, m, r).await;
    let e = result.unwrap_err();
    assert_eq!(e.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(e.to_string(), expected);
    assert_eq!(stored, 32);
}

#[tokio::test]
async fn operator_status_reports_approval_and_membership_readiness() {
    let (h, pool, member, m, r) = fixture(true).await;
    let before = feature().status(&pool).await.unwrap();
    assert_eq!(before.required_version().get(), 1);
    assert_eq!(before.incompatible_memberships(), 1);
    assert!(before.approved_digests().is_empty());
    let approved = feature()
        .approve(&pool, member.identity().digest())
        .await
        .unwrap();
    assert_eq!(approved.incompatible_memberships(), 0);
    assert_eq!(approved.approved_digests(), &[member.identity().digest()]);
    cleanup(h, member, m, r).await;
}

#[tokio::test]
async fn protected_waiters_revalidate_after_the_exclusive_transition_commits() {
    for isolation in ["READ COMMITTED", "REPEATABLE READ"] {
        let (h, pool, member, m, r) = fixture(true).await;
        member.request_ready().await.unwrap();
        feature()
            .approve(&pool, member.identity().digest())
            .await
            .unwrap();
        feature()
            .enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
            .await
            .unwrap();
        let transition = h.db().client().await.unwrap();
        transition
            .batch_execute("CREATE TABLE feature_protected_write_probe(value int8)")
            .await
            .unwrap();
        let protected = h.db().client().await.unwrap();
        protected
            .batch_execute(&format!("BEGIN ISOLATION LEVEL {isolation}"))
            .await
            .unwrap();
        // Establish the old snapshot before the competing transition starts, including in RR.
        assert_eq!(protected.query_one("SELECT generation FROM public.erp_node_feature WHERE key='auth.login_principal'", &[]).await.unwrap().get::<_, i64>(0), 1);
        let waiter: i32 = protected
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        transition.batch_execute("BEGIN; UPDATE public.erp_node_feature SET enabled=false,generation=generation+1 WHERE key='auth.login_principal'").await.unwrap();
        let blocker: i32 = transition
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0);
        let id = member.identity().id().to_string();
        let digest = member.identity().digest();
        let attempt = tokio::spawn(async move {
            // This stand-in caller write shares the helper's transaction, as session/identity
            // finalization does. A stale helper must never allow the dependent write to execute.
            let result = protected.query("WITH guarded AS MATERIALIZED (SELECT * FROM public.erp_node_feature_protect('auth.login_principal',1,$1::text::uuid,$2,1,true)) INSERT INTO feature_protected_write_probe SELECT generation FROM guarded RETURNING value", &[&id, &&digest.as_bytes()[..]]).await;
            protected.batch_execute("ROLLBACK").await.unwrap();
            assert_eq!(
                protected
                    .query_one("SELECT 1::int4", &[])
                    .await
                    .unwrap()
                    .get::<_, i32>(0),
                1
            );
            result
        });
        blocked_by(&h, waiter, blocker).await;
        assert!(!attempt.is_finished());
        transition.batch_execute("COMMIT").await.unwrap();
        let error = attempt.await.unwrap().unwrap_err();
        let error = error.as_db_error().unwrap();
        let expected = if isolation == "READ COMMITTED" {
            ("ERF01", "login selection rollout generation changed")
        } else {
            (
                "40001",
                "could not serialize access due to concurrent update",
            )
        };
        assert_eq!((error.code().code(), error.message()), expected);
        assert_eq!(
            transition
                .query_one("SELECT count(*) FROM feature_protected_write_probe", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        drop(transition);
        let state = feature().status(&pool).await.unwrap();
        assert!(!state.enabled());
        assert_eq!(state.generation(), 2);
        cleanup(h, member, m, r).await;
    }
}
