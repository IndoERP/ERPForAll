//! Durable administrative state for; ordinary rollout windows cannot enable a feature.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_fields::ErrorKind;
use erp_orm::node::{FeatureKey, FeatureRollout, FeatureVersion, NodeOptions};
use erp_testkit::{Harness, deadpool_postgres::Pool};

async fn database() -> (Harness, Pool) {
    let h = Harness::builder([&erp_test_models::base_users::ADDON])
        .build()
        .await
        .unwrap();
    let pool = erp_orm::node::heartbeat_pool(h.db().config(), &NodeOptions::default()).unwrap();
    (h, pool)
}

fn rollout(key: &str) -> FeatureRollout {
    FeatureRollout::new(
        FeatureKey::new(key).unwrap(),
        FeatureVersion::new(1).unwrap(),
    )
}

#[tokio::test]
async fn feature_registration_is_disabled_and_idempotent() {
    let (_h, pool) = database().await;
    let feature = rollout("auth.login_principal");
    let first = feature.register(&pool).await.unwrap();
    assert!(!first.enabled());
    assert_eq!(first.generation(), 0);
    assert_eq!(feature.register(&pool).await.unwrap(), first);
    assert_eq!(feature.status(&pool).await.unwrap(), first);
    assert_eq!(feature.disable(&pool).await.unwrap(), first);
}

#[tokio::test]
async fn missing_feature_state_is_a_loud_refusal() {
    let (h, pool) = database().await;
    let error = rollout("auth.login_principal")
        .status(&pool)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        format!(
            "database {}: feature auth.login_principal refused: feature state is missing; register it before use",
            h.db().name()
        )
    );
}

#[tokio::test]
async fn feature_registration_bounds_the_database_not_only_one_identity() {
    let (h, pool) = database().await;
    for i in 0..8 {
        rollout(&format!("feature{i}"))
            .register(&pool)
            .await
            .unwrap();
    }
    let error = rollout("feature8").register(&pool).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        format!(
            "database {}: feature feature8 refused: a database registers at most 8 feature keys",
            h.db().name()
        )
    );
    let client = pool.get().await.unwrap();
    let row = client
        .query_one("SELECT count(*) FROM erp_node_feature", &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 8);
}

#[tokio::test]
async fn existing_feature_version_cannot_be_reinterpreted() {
    let (h, pool) = database().await;
    let feature = rollout("auth.login_principal");
    let original = feature.register(&pool).await.unwrap();
    let newer = FeatureRollout::new(
        FeatureKey::new("auth.login_principal").unwrap(),
        FeatureVersion::new(2).unwrap(),
    );
    for result in [
        newer.register(&pool).await,
        newer.status(&pool).await,
        newer.disable(&pool).await,
    ] {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.to_string(),
            format!(
                "database {}: feature auth.login_principal refused: stored version 1 does not match required version 2",
                h.db().name()
            )
        );
    }
    assert_eq!(feature.status(&pool).await.unwrap(), original);
}

#[tokio::test]
async fn disable_preserves_registration_and_checks_generation_exhaustion() {
    let (h, pool) = database().await;
    let feature = rollout("auth.login_principal");
    feature.register(&pool).await.unwrap();
    // Trusted operator fixture, not real cluster activation or serving authorization.
    let client = pool.get().await.unwrap();
    client.batch_execute("UPDATE public.erp_node_feature SET enabled = true, generation = 4,
        enrollment_confirmed = true, enrollment_actor = current_user, enrollment_at = clock_timestamp()
        WHERE key = 'auth.login_principal'").await.unwrap();
    drop(client);
    let enabled = feature.status(&pool).await.unwrap();
    assert!(enabled.enabled());
    assert_eq!(enabled.generation(), 4);
    assert_eq!(feature.register(&pool).await.unwrap(), enabled);
    let disabled = feature.disable(&pool).await.unwrap();
    assert!(!disabled.enabled());
    assert_eq!(disabled.generation(), 5);
    assert_eq!(feature.disable(&pool).await.unwrap(), disabled);
    let client = pool.get().await.unwrap();
    client
        .batch_execute(
            "UPDATE public.erp_node_feature SET enabled = true, generation = 9223372036854775807
        WHERE key = 'auth.login_principal'",
        )
        .await
        .unwrap();
    drop(client);
    let error = feature.disable(&pool).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        format!(
            "database {}: feature auth.login_principal refused: feature generation is exhausted",
            h.db().name()
        )
    );
    let unchanged = feature.status(&pool).await.unwrap();
    assert!(unchanged.enabled());
    assert_eq!(unchanged.generation(), i64::MAX);
}

/// Each actor has its own physical connection and pool; one-slot pools do not serialize actors.
async fn actor(h: &Harness, label: &str) -> (Pool, i32) {
    let mut config = h.db().config();
    config.application_name(label);
    let pool = erp_orm::node::heartbeat_pool(config, &NodeOptions::default()).unwrap();
    let client = pool.get().await.unwrap();
    let pid = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    drop(client);
    (pool, pid)
}

/// Observe actual lock admission before releasing a barrier; only these test-owned backends count.
async fn wait_for_locked_actors(h: &Harness, pids: &[i32]) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let client = h.db().client().await.unwrap();
        loop {
            let row = client.query_one(
                "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE datname = current_database()
                 AND pid = ANY($1::int4[]) AND wait_event_type = 'Lock'",
                &[&pids],
            ).await.unwrap();
            if row.get::<_, i64>(0) == i64::try_from(pids.len()).unwrap() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actors did not reach the database lock barrier");
}

async fn readiness_barrier(h: &Harness) -> erp_testkit::deadpool_postgres::Object {
    let client = h.db().client().await.unwrap();
    client.batch_execute("BEGIN").await.unwrap();
    let (class, key) = erp_orm::node::READINESS_LOCK;
    client
        .query_one("SELECT pg_advisory_xact_lock($1, $2)", &[&class, &key])
        .await
        .unwrap();
    client
}

#[tokio::test]
async fn concurrent_eighth_and_ninth_registration_use_one_database_bound() {
    let (h, pool) = database().await;
    for i in 0..7 {
        rollout(&format!("feature{i}"))
            .register(&pool)
            .await
            .unwrap();
    }
    let barrier = readiness_barrier(&h).await;
    let (left, left_pid) = actor(&h, "feature-bound-left").await;
    let (right, right_pid) = actor(&h, "feature-bound-right").await;
    assert_ne!(left_pid, right_pid);
    let first = tokio::spawn(async move { rollout("feature7").register(&left).await });
    let second = tokio::spawn(async move { rollout("feature8").register(&right).await });
    wait_for_locked_actors(&h, &[left_pid, right_pid]).await;
    barrier.batch_execute("COMMIT").await.unwrap();
    let results = [
        ("feature7", first.await.unwrap()),
        ("feature8", second.await.unwrap()),
    ];
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    for (key, result) in results {
        match result {
            Ok(state) => {
                assert!(!state.enabled());
                assert_eq!(state.generation(), 0);
            }
            Err(error) => {
                assert_eq!(error.kind(), ErrorKind::Programming);
                assert_eq!(
                    error.to_string(),
                    format!(
                        "database {}: feature {key} refused: a database registers at most 8 feature keys",
                        h.db().name()
                    )
                );
            }
        }
    }
    let client = h.db().client().await.unwrap();
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM public.erp_node_feature", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        8
    );
}

#[tokio::test]
async fn concurrent_same_key_registration_is_idempotent() {
    let (h, pool) = database().await;
    let barrier = readiness_barrier(&h).await;
    let (left, left_pid) = actor(&h, "feature-same-left").await;
    let (right, right_pid) = actor(&h, "feature-same-right").await;
    assert_ne!(left_pid, right_pid);
    let first = tokio::spawn(async move { rollout("auth.login_principal").register(&left).await });
    let second =
        tokio::spawn(async move { rollout("auth.login_principal").register(&right).await });
    wait_for_locked_actors(&h, &[left_pid, right_pid]).await;
    barrier.batch_execute("COMMIT").await.unwrap();
    let first = first.await.unwrap().unwrap();
    assert_eq!(second.await.unwrap().unwrap(), first);
    assert_eq!(
        rollout("auth.login_principal").status(&pool).await.unwrap(),
        first
    );
    let client = h.db().client().await.unwrap();
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM public.erp_node_feature", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
}

async fn enabled_fixture(pool: &Pool) {
    rollout("auth.login_principal")
        .register(pool)
        .await
        .unwrap();
    let client = pool.get().await.unwrap();
    // Trusted fixture state only: this does not certify enrollment, activation or serving.
    client.batch_execute("UPDATE public.erp_node_feature SET enabled = true, generation = 4,
        enrollment_confirmed = true, enrollment_actor = current_user, enrollment_at = clock_timestamp()
        WHERE key = 'auth.login_principal'").await.unwrap();
}

#[tokio::test]
async fn concurrent_disable_increments_once() {
    let (h, pool) = database().await;
    enabled_fixture(&pool).await;
    let barrier = readiness_barrier(&h).await;
    let (left, left_pid) = actor(&h, "feature-disable-left").await;
    let (right, right_pid) = actor(&h, "feature-disable-right").await;
    assert_ne!(left_pid, right_pid);
    let first = tokio::spawn(async move { rollout("auth.login_principal").disable(&left).await });
    let second = tokio::spawn(async move { rollout("auth.login_principal").disable(&right).await });
    wait_for_locked_actors(&h, &[left_pid, right_pid]).await;
    barrier.batch_execute("COMMIT").await.unwrap();
    let state = first.await.unwrap().unwrap();
    assert!(!state.enabled());
    assert_eq!(state.generation(), 5);
    assert_eq!(second.await.unwrap().unwrap(), state);
    assert_eq!(
        rollout("auth.login_principal").status(&pool).await.unwrap(),
        state
    );
}

#[tokio::test]
async fn cancelling_a_lock_wait_discards_connection_and_preserves_state() {
    let (h, pool) = database().await;
    enabled_fixture(&pool).await;
    let original = rollout("auth.login_principal").status(&pool).await.unwrap();
    let blocker = h.db().client().await.unwrap();
    blocker
        .batch_execute(
            "BEGIN; SELECT * FROM public.erp_node_feature
        WHERE key = 'auth.login_principal' FOR UPDATE",
        )
        .await
        .unwrap();
    let (waiting_pool, waiting_pid) = actor(&h, "feature-abort-waiter").await;
    let task_pool = waiting_pool.clone();
    let operation =
        tokio::spawn(async move { rollout("auth.login_principal").disable(&task_pool).await });
    wait_for_locked_actors(&h, &[waiting_pid]).await;
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    // Once the row barrier is released, the abandoned connection must end its transaction;
    // the cancelled future cannot send COMMIT even if PostgreSQL finishes its blocked SELECT.
    blocker.batch_execute("ROLLBACK").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let client = h.db().client().await.unwrap();
        loop {
            let row = client
                .query_one(
                    "SELECT count(*) FROM pg_catalog.pg_stat_activity
                WHERE datname = current_database() AND pid = $1",
                    &[&waiting_pid],
                )
                .await
                .unwrap();
            if row.get::<_, i64>(0) == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled operator backend remained connected");
    let replacement = waiting_pool.get().await.unwrap();
    assert_ne!(
        replacement
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .unwrap()
            .get::<_, i32>(0),
        waiting_pid
    );
    let (class, key) = erp_orm::node::READINESS_LOCK;
    assert!(
        replacement
            .query_one("SELECT pg_try_advisory_xact_lock($1, $2)", &[&class, &key])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(replacement);
    assert_eq!(
        rollout("auth.login_principal").status(&pool).await.unwrap(),
        original
    );
}

#[tokio::test]
async fn unprivileged_login_cannot_register_read_or_disable_feature_state() {
    use std::error::Error;

    let (h, pool) = database().await;
    enabled_fixture(&pool).await;
    let original = rollout("auth.login_principal").status(&pool).await.unwrap();
    let role = format!("nf_{}", h.db().name());
    assert!(role.len() <= 63);
    assert!(
        role.bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    );
    // Unique generated fixture credential, confined to this test-owned role and database.
    let password = format!("pw_{role}");
    let admin = h.db().client().await.unwrap();
    admin
        .batch_execute(&format!(
            "CREATE ROLE \"{role}\" LOGIN NOINHERIT NOSUPERUSER
        NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '{password}';
        GRANT CONNECT ON DATABASE \"{}\" TO \"{role}\";
        GRANT USAGE ON SCHEMA public TO \"{role}\"",
            h.db().name()
        ))
        .await
        .unwrap();
    let mut config = h.db().config();
    config
        .user(&role)
        .password(&password)
        .application_name("feature-unprivileged");
    let unprivileged = erp_orm::node::heartbeat_pool(config, &NodeOptions::default()).unwrap();
    let client = unprivileged.get().await.unwrap();
    let identity = client
        .query_one("SELECT current_user::text, session_user::text", &[])
        .await
        .unwrap();
    let current: String = identity.get(0);
    let session: String = identity.get(1);
    drop(client);
    let feature = rollout("auth.login_principal");
    let results = [
        (
            "registering a feature",
            feature.register(&unprivileged).await,
        ),
        (
            "reading feature status",
            feature.status(&unprivileged).await,
        ),
        ("disabling a feature", feature.disable(&unprivileged).await),
    ];
    unprivileged.close();
    // Clean up only this test's role even if the assertions below expose an unexpected success.
    admin
        .batch_execute(&format!("DROP OWNED BY \"{role}\"; DROP ROLE \"{role}\""))
        .await
        .unwrap();
    assert_eq!(current, role);
    assert_eq!(session, role);
    for (what, result) in results {
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Database);
        assert_eq!(error.sqlstate(), Some("42501"));
        assert_eq!(
            error.to_string(),
            format!(
                "database {}: {what} failed: permission denied for table erp_node_feature",
                h.db().name()
            )
        );
        let source = error
            .source()
            .unwrap()
            .downcast_ref::<tokio_postgres::Error>()
            .unwrap();
        let database_error = source.as_db_error().unwrap();
        assert_eq!(database_error.code().code(), "42501");
        assert_eq!(
            database_error.message(),
            "permission denied for table erp_node_feature"
        );
    }
    assert_eq!(feature.status(&pool).await.unwrap(), original);
    assert_eq!(
        admin
            .query_one("SELECT count(*) FROM public.erp_node_feature", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
}

#[tokio::test]
async fn lost_commit_acknowledgment_is_never_reported_as_success() {
    let (h, pool) = database().await;
    let before: i32 = pool
        .get()
        .await
        .unwrap()
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let result = rollout("auth.login_principal")
        .with_lost_commit_ack_for_tests()
        .register(&pool)
        .await;
    let e = result.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Database);
    assert_eq!(e.sqlstate(), None);
    assert_eq!(
        e.to_string(),
        format!(
            "database {}: registering a feature failed: commit acknowledgment timed out; outcome unknown; reconcile status",
            h.db().name()
        )
    );
    let after: i32 = pool
        .get()
        .await
        .unwrap()
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    assert_ne!(before, after, "ambiguous connection must be discarded");
    let state = rollout("auth.login_principal").status(&pool).await.unwrap();
    assert!(!state.enabled());
    assert_eq!(state.generation(), 0);
}

#[tokio::test]
async fn operator_deadline_discards_a_waiting_transaction() {
    let (h, pool) = database().await;
    enabled_fixture(&pool).await;
    let original = rollout("auth.login_principal").status(&pool).await.unwrap();
    let blocker = h.db().client().await.unwrap();
    blocker
        .batch_execute(
            "BEGIN;SELECT FROM erp_node_feature WHERE key='auth.login_principal' FOR UPDATE",
        )
        .await
        .unwrap();
    let (waiting, pid) = actor(&h, "feature-real-timeout").await;
    let p = waiting.clone();
    let job = tokio::spawn(async move { rollout("auth.login_principal").disable(&p).await });
    wait_for_locked_actors(&h, &[pid]).await;
    let result = tokio::time::timeout(std::time::Duration::from_secs(40), job)
        .await
        .expect("operator exceeded its own deadline")
        .unwrap();
    blocker.batch_execute("ROLLBACK").await.unwrap();
    let e = result.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Database);
    match &e {
        erp_orm::node::NodeError::Timeout { .. } => assert_eq!(
            e.to_string(),
            format!(
                "database {}: disabling a feature did not finish within 30000 ms",
                h.db().name()
            )
        ),
        erp_orm::node::NodeError::Database { .. } => {
            let expected = match e.sqlstate() {
                Some("55P03") => "canceling statement due to lock timeout",
                Some("57014") => "canceling statement due to statement timeout",
                Some("25P04") => "terminating connection due to transaction timeout",
                other => panic!("unexpected bounded wait fault SQLSTATE {other:?}"),
            };
            assert_eq!(
                e.to_string(),
                format!(
                    "database {}: disabling a feature failed: {expected}",
                    h.db().name()
                )
            );
        }
        other => panic!("unexpected deadline result {other:?}"),
    }
    let new_pid: i32 = waiting
        .get()
        .await
        .unwrap()
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    assert_ne!(pid, new_pid);
    assert_eq!(
        rollout("auth.login_principal").status(&pool).await.unwrap(),
        original
    );
}
