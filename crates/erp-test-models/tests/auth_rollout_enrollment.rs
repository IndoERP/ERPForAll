//! Isolated rehearsal of the external legacy credential/session fencing precondition.
//! It does not attest that any deployment's old endpoints or credentials were retired.
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
#[tokio::test]
async fn legacy_database_sessions_and_inherited_credentials_are_explicitly_fenced() {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let admin = h.db().client().await.unwrap();
    let group = format!("eg_{}", h.db().name());
    let old = format!("eo_{}", h.db().name());
    let proxy = format!("ep_{}", h.db().name());
    let maintenance = format!("em_{}", h.db().name());
    let request = format!("er_{}", h.db().name());
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!("CREATE ROLE {} NOLOGIN;CREATE ROLE {} LOGIN PASSWORD 'fixture-enrollment-password';CREATE ROLE {} LOGIN PASSWORD 'fixture-enrollment-password' NOINHERIT;CREATE ROLE {} NOLOGIN;CREATE ROLE {} NOLOGIN;GRANT SELECT ON res_users TO {};GRANT {} TO {};GRANT {} TO {} WITH INHERIT FALSE,SET TRUE",erp_sql::quote(&group),erp_sql::quote(&old),erp_sql::quote(&proxy),erp_sql::quote(&maintenance),erp_sql::quote(&request),erp_sql::quote(&group),erp_sql::quote(&group),erp_sql::quote(&old),erp_sql::quote(&group),erp_sql::quote(&proxy))).await.unwrap();
    tx.grant_node_feature_roles(&maintenance, &request)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut connections = Vec::new();
    let mut pids = Vec::new();
    for role in [&old, &proxy] {
        let mut config = h.db().config();
        config.user(role).password("fixture-enrollment-password");
        let (client, connection) = config.connect(tokio_postgres::NoTls).await.unwrap();
        let driver = tokio::spawn(connection);
        client
            .batch_execute(&format!("SET ROLE {}", erp_sql::quote(&group)))
            .await
            .unwrap();
        client
            .query_one("SELECT count(*) FROM res_users", &[])
            .await
            .unwrap();
        pids.push(
            client
                .query_one("SELECT pg_backend_pid()", &[])
                .await
                .unwrap()
                .get::<_, i32>(0),
        );
        connections.push((client, driver));
    }
    // Both standalone connections exist, while membership cannot discover either of them.
    assert_eq!(
        admin
            .query_one("SELECT count(*) FROM erp_node", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    admin
        .batch_execute(&format!(
            "ALTER ROLE {} NOLOGIN;ALTER ROLE {} NOLOGIN;REVOKE {} FROM {},{}",
            erp_sql::quote(&old),
            erp_sql::quote(&proxy),
            erp_sql::quote(&group),
            erp_sql::quote(&old),
            erp_sql::quote(&proxy)
        ))
        .await
        .unwrap();
    for pid in &pids {
        assert!(
            admin
                .query_one("SELECT pg_terminate_backend($1)", &[pid])
                .await
                .unwrap()
                .get::<_, bool>(0)
        );
    }
    let mut terminated = Vec::new();
    for (client, driver) in connections {
        let result = driver.await.unwrap();
        drop(client);
        let error = result.unwrap_err();
        let error = error.as_db_error().unwrap();
        terminated.push((error.code().code().to_owned(), error.message().to_owned()));
    }
    let mut denials = Vec::new();
    for role in [&old, &proxy] {
        let mut config = h.db().config();
        config.user(role).password("fixture-enrollment-password");
        let error = config
            .connect(tokio_postgres::NoTls)
            .await
            .err()
            .expect("fenced role reconnected");
        let error = error.as_db_error().unwrap();
        denials.push((
            role.clone(),
            error.code().code().to_owned(),
            error.message().to_owned(),
        ));
    }
    let reachable = admin
        .query_one(
            "SELECT pg_has_role($1,$3,'MEMBER'),pg_has_role($2,$3,'MEMBER')",
            &[&old, &proxy, &group],
        )
        .await
        .unwrap();
    assert!(!reachable.get::<_, bool>(0));
    assert!(!reachable.get::<_, bool>(1));
    let pool = erp_orm::node::heartbeat_pool(h.db().config(), &NodeOptions::default()).unwrap();
    let key = FeatureKey::new("auth.login_principal").unwrap();
    let version = FeatureVersion::new(1).unwrap();
    let feature = FeatureRollout::new(key.clone(), version);
    feature.register(&pool).await.unwrap();
    let identity = NodeIdentity::builder(
        NodeRole::App,
        "enrolled",
        "host",
        BuildIdentity::__from_build(
            "enrollment-fixture",
            "1",
            false,
            Some("0123456789abcdef0123456789abcdef01234567"),
            Some("0"),
        ),
        AddonsDigest::builder().build().unwrap(),
    )
    .features(FeatureSet::new([(key, version)]).unwrap())
    .create(&pool)
    .await
    .unwrap();
    let member = NodeMembership::register(Arc::new(identity), pool.clone())
        .await
        .unwrap();
    feature
        .approve(&pool, member.identity().digest())
        .await
        .unwrap();
    let state = feature
        .enable(&pool, LegacyEnrollment::confirmed_drained_and_fenced())
        .await
        .unwrap();
    assert!(state.enabled());
    member.shutdown().await.unwrap();
    let mut cleanup = h.begin().await.unwrap();
    for role in [&group, &old, &proxy, &maintenance, &request] {
        cleanup
            .batch(&format!(
                "DROP OWNED BY {} CASCADE;DROP ROLE {}",
                erp_sql::quote(role),
                erp_sql::quote(role)
            ))
            .await
            .unwrap();
    }
    cleanup.commit().await.unwrap();
    for (code, message) in terminated {
        assert_eq!(code, "57P01");
        assert_eq!(
            message,
            "terminating connection due to administrator command"
        );
    }
    for (role, code, message) in denials {
        assert_eq!(code, "28000");
        assert_eq!(
            message,
            format!("role \"{role}\" is not permitted to log in")
        );
    }
}
