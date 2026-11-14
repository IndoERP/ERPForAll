// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use crate::partition_upkeep::{maintain, policy, supervise};
use erp_fields::ErrorKind;
use erp_orm::Orm;
use erp_test_models::{base_users, test_partition};
use erp_testkit::Harness;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

#[tokio::test]
async fn generic_upkeep_recovers_inserts_and_independent_registries() {
    let h = Harness::builder([&base_users::ADDON, &test_partition::ADDON])
        .with_decls([
            erp_registry::ModelDecl::new("test_partition", "scale.weekly").partition(
                erp_registry::PartitionDecl::new(
                    erp_registry::PartitionInterval::Week,
                    2,
                    erp_registry::PartitionRetain::Intervals(4),
                ),
            ),
        ])
        .build()
        .await
        .unwrap();
    let client = h.db().client().await.unwrap();
    // Only fixture tables are interpolated: the database chooses its own partition names.
    let rows = client.query("SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid=i.inhrelid WHERE i.inhparent IN ('scale_event'::regclass,'scale_weekly'::regclass)", &[]).await.unwrap();
    for row in rows {
        let name: String = row.get(0);
        client
            .batch_execute(&format!("DROP TABLE \"{}\"", name.replace('"', "\"\"")))
            .await
            .unwrap();
    }
    let error = client
        .execute(
            "INSERT INTO scale_event (id,create_date) VALUES (100,now() AT TIME ZONE 'UTC')",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(error.code().map(|code| code.code()), Some("23514"));
    // PostgreSQL includes the current UTC timestamp in DETAIL; its primary message is stable.
    assert_eq!(
        error.as_db_error().unwrap().message(),
        "no partition of relation \"scale_event\" found for row"
    );
    drop(client);
    let report = maintain(h.orm(), h.db().pool()).await.unwrap();
    assert!(report.is_complete());
    assert!(!report.created().is_empty());
    assert!(
        report
            .entries()
            .iter()
            .any(|entry| entry.model() == "scale.weekly"
                && matches!(entry.action(), erp_orm::PartitionAction::Created))
    );
    let client = h.db().client().await.unwrap();
    assert_eq!(
        client
            .execute(
                "INSERT INTO scale_event (id,create_date) VALUES (100,now() AT TIME ZONE 'UTC')",
                &[]
            )
            .await
            .unwrap(),
        1
    );
    drop(client);

    // Independent process view: both engine instances use the same database locks, no leader.
    let mut other_config = h.db().config();
    other_config.options("-c timezone=Pacific/Kiritimati");
    let other_pool = erp_orm::pool::create_pool(other_config, 1).unwrap();
    let other = Orm::builder(Arc::clone(h.registry()), other_pool.clone())
        .build()
        .unwrap();
    let (a, b) = tokio::join!(
        maintain(h.orm(), h.db().pool()),
        maintain(&other, &other_pool)
    );
    assert!(a.unwrap().is_complete());
    let b = b.unwrap();
    assert!(b.is_complete());
    assert!(
        b.created().is_empty(),
        "the non-UTC session must not shift the declared window: {b:?}"
    );
    assert_eq!(
        policy(h.orm(), h.db().pool()).await.unwrap(),
        Duration::from_secs(3600)
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generic_upkeep_reports_contention_and_recovers_without_hiding_other_models() {
    let h = Harness::builder([&base_users::ADDON, &test_partition::ADDON])
        .build()
        .await
        .unwrap();
    let holder = h.db().client().await.unwrap();
    let rows = holder.query("SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid=i.inhrelid WHERE i.inhparent='scale_audit'::regclass", &[]).await.unwrap();
    for row in rows {
        let name: String = row.get(0);
        holder
            .batch_execute(&format!("DROP TABLE \"{}\"", name.replace('"', "\"\"")))
            .await
            .unwrap();
    }
    // Same lock namespace as erp_sql::partition::SESSION_LOCK; this is a fixture obstruction.
    holder
        .query(
            "SELECT pg_advisory_lock(1701, hashtext($1::text))",
            &[&"scale_event"],
        )
        .await
        .unwrap();
    let fast = Orm::builder(Arc::clone(h.registry()), h.db().pool().clone())
        .config(erp_orm::OrmConfig {
            partition_lock_timeout: Duration::from_millis(200),
            ..erp_orm::OrmConfig::default()
        })
        .build()
        .unwrap();
    let error = maintain(&fast, h.db().pool()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(
        error.message(),
        "partition upkeep failed: scale.event scale_event: prepare: partition upkeep of 'scale.event' could not take the upkeep lock of table 'scale_event' (another upkeep or a schema synchronization holds it): canceling statement due to lock timeout (SQLSTATE 55P03)"
    );
    let repaired: i64 = holder
        .query_one(
            "SELECT count(*) FROM pg_inherits WHERE inhparent='scale_audit'::regclass",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        repaired, 3,
        "the healthy monthly model is repaired despite daily-model contention"
    );
    holder
        .query(
            "SELECT pg_advisory_unlock(1701, hashtext($1::text))",
            &[&"scale_event"],
        )
        .await
        .unwrap();
    drop(holder);
    let report = maintain(&fast, h.db().pool()).await.unwrap();
    assert!(report.is_complete());
    assert!(
        report
            .entries()
            .iter()
            .any(|entry| entry.model() == "scale.audit")
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generic_upkeep_reads_bounded_policy_from_the_database() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .await
        .unwrap();
    let key = "base.partition_upkeep_interval_secs";
    assert_eq!(
        policy(h.orm(), h.db().pool()).await.unwrap(),
        Duration::from_secs(3600)
    );
    let client = h.db().client().await.unwrap();
    client
        .execute(
            "INSERT INTO ir_config_parameter (key,value) VALUES ($1,'60')",
            &[&key],
        )
        .await
        .unwrap();
    drop(client);
    assert_eq!(
        policy(h.orm(), h.db().pool()).await.unwrap(),
        Duration::from_secs(60)
    );
    for value in ["0".to_owned(), "bad".to_owned(), "9".repeat(1000)] {
        let client = h.db().client().await.unwrap();
        client
            .execute(
                "UPDATE ir_config_parameter SET value=$2 WHERE key=$1",
                &[&key, &value],
            )
            .await
            .unwrap();
        drop(client);
        let error = policy(h.orm(), h.db().pool()).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "base.partition_upkeep_interval_secs must be an integer from 60 to 3600."
        );
    }
    h.db()
        .client()
        .await
        .unwrap()
        .execute("DELETE FROM ir_config_parameter WHERE key=$1", &[&key])
        .await
        .unwrap();
    assert_eq!(
        policy(h.orm(), h.db().pool()).await.unwrap(),
        Duration::from_secs(3600)
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generic_upkeep_keeps_ddl_off_runtime_credentials() {
    let h = Harness::builder([&base_users::ADDON, &test_partition::ADDON])
        .build()
        .await
        .unwrap();
    let role = format!("upkeep_runtime_{}", std::process::id());
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!(
            "CREATE ROLE {role} LOGIN PASSWORD 'upkeep-role-fixture-password'"
        ))
        .await
        .unwrap();
    let rows = client.query("SELECT c.relname FROM pg_inherits i JOIN pg_class c ON c.oid=i.inhrelid WHERE i.inhparent='scale_event'::regclass", &[]).await.unwrap();
    for row in rows {
        let name: String = row.get(0);
        client
            .batch_execute(&format!("DROP TABLE \"{}\"", name.replace('"', "\"\"")))
            .await
            .unwrap();
    }
    drop(client);
    let mut config = h.db().config();
    config.user(&role).password("upkeep-role-fixture-password");
    let runtime = erp_orm::pool::create_pool(config, 1).unwrap();
    {
        let client = runtime.get().await.unwrap();
        let identity = client
            .query_one("SELECT current_user::text,session_user::text", &[])
            .await
            .unwrap();
        assert_eq!(identity.get::<_, String>(0), role);
        assert_eq!(identity.get::<_, String>(1), role);
    }
    let error = maintain(h.orm(), &runtime).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    // The aggregate includes server/version-dependent DDL context. Verify its stable contract
    // plus the PostgreSQL permission classification, not timestamp/detail text.
    assert!(
        error
            .message()
            .starts_with("partition upkeep failed: scale.event "),
        "{error}"
    );
    assert!(error.message().contains("SQLSTATE 42501"), "{error}");
    runtime.close();
    assert!(
        maintain(h.orm(), h.db().pool())
            .await
            .unwrap()
            .is_complete()
    );
    h.db()
        .client()
        .await
        .unwrap()
        .batch_execute(&format!("DROP ROLE {role}"))
        .await
        .unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generic_upkeep_uses_the_registry_supplied_by_the_current_generation() {
    let h = Harness::builder([&base_users::ADDON, &test_partition::ADDON])
        .build()
        .await
        .unwrap();
    let registry = Harness::builder([&base_users::ADDON])
        .registry_only()
        .unwrap();
    let without_models = Orm::builder(registry, h.db().pool().clone())
        .build()
        .unwrap();
    assert!(
        maintain(&without_models, h.db().pool())
            .await
            .unwrap()
            .entries()
            .is_empty()
    );
    assert!(
        !maintain(h.orm(), h.db().pool())
            .await
            .unwrap()
            .entries()
            .is_empty()
    );
    assert!(
        maintain(&without_models, h.db().pool())
            .await
            .unwrap()
            .entries()
            .is_empty()
    );
    h.cleanup().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn generic_upkeep_supervisor_reloads_policy_and_retries_failures() {
    let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let logger = erp_server::logging::LineLogger::new(
        erp_server::logging::LogFilter::parse("debug").unwrap(),
        CapturedLog(Arc::clone(&bytes)),
    );
    let _guard = tracing::subscriber::set_default(logger);
    let reads = AtomicUsize::new(0);
    let runs = AtomicUsize::new(0);
    let start = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(241),
        supervise(
            Duration::from_secs(60),
            async || match reads.fetch_add(1, Ordering::SeqCst) {
                0 => Err(erp_fields::Error::validation("corrupt upkeep policy")),
                1 => Ok(Duration::from_secs(120)),
                _ => Ok(Duration::from_secs(60)),
            },
            async || {
                assert!(
                    tokio::time::Instant::now().duration_since(start) >= Duration::from_secs(120)
                );
                match runs.fetch_add(1, Ordering::SeqCst) {
                    0 => Err(erp_fields::Error::db("upkeep statement failed")),
                    _ => Ok(()),
                }
            },
        ),
    )
    .await;
    assert!(
        result.is_err(),
        "supervision must continue until its serving future is dropped"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 3);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    let log = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(
        log.contains("generic partition upkeep policy is invalid; maintenance skipped"),
        "{log}"
    );
    assert!(
        log.contains("scheduled generic partition upkeep failed; it runs again at the next tick"),
        "{log}"
    );
    tokio::time::advance(Duration::from_secs(120)).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "dropping supervision cancels future ticks"
    );
}

#[derive(Clone)]
struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for CapturedLog {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn generic_upkeep_delays_slow_ticks_without_catchup() {
    let starts = std::sync::Mutex::new(Vec::new());
    let start = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(601),
        supervise(
            Duration::from_secs(60),
            async || Ok(Duration::from_secs(60)),
            async || {
                starts
                    .lock()
                    .unwrap()
                    .push(tokio::time::Instant::now().duration_since(start).as_secs());
                tokio::time::sleep(Duration::from_secs(180)).await;
                Ok(())
            },
        ),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(*starts.lock().unwrap(), vec![60, 300, 540]);
}

#[tokio::test]
async fn generic_upkeep_refuses_missing_schema_and_owner_pool_failures() {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .require_core_tables(erp_security::auth::core_tables())
        .build()
        .await
        .unwrap();
    let client = h.db().client().await.unwrap();
    client
        .batch_execute("ALTER TABLE ir_config_parameter RENAME TO fixture_saved_parameters")
        .await
        .unwrap();
    let error = policy(h.orm(), h.db().pool()).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(
        error.message(),
        "could not read generic partition upkeep policy"
    );
    client
        .batch_execute("ALTER TABLE fixture_saved_parameters RENAME TO ir_config_parameter")
        .await
        .unwrap();
    drop(client);
    let owner = erp_orm::pool::create_pool(h.db().config(), 1).unwrap();
    owner.close();
    let error = policy(h.orm(), &owner).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(
        error.message(),
        "could not read generic partition upkeep policy"
    );
    let error = maintain(h.orm(), &owner).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(
        error.message(),
        "could not read the generic partition upkeep clock"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn generic_upkeep_cancellation_releases_its_owner_session() {
    let h = Harness::builder([&base_users::ADDON, &test_partition::ADDON])
        .build()
        .await
        .unwrap();
    let blocker = h.db().client().await.unwrap();
    blocker
        .query(
            "SELECT pg_advisory_lock(1701, hashtext('scale_event'))",
            &[],
        )
        .await
        .unwrap();
    let mut config = h.db().config();
    config.application_name("fixture-generic-upkeep-cancellation");
    let owner = erp_orm::pool::create_pool(config, 1).unwrap();
    let mut work = Box::pin(maintain(h.orm(), &owner));
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                result = &mut work => panic!("upkeep completed before cancellation: {result:?}"),
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            let waiting: bool = blocker.query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='fixture-generic-upkeep-cancellation' AND wait_event='advisory')", &[]
            ).await.unwrap().get(0);
            if waiting { break; }
        }
    }).await.unwrap();
    drop(work);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let active: bool = blocker.query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='fixture-generic-upkeep-cancellation')", &[]
            ).await.unwrap().get(0);
            if !active { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    blocker
        .query(
            "SELECT pg_advisory_unlock(1701, hashtext('scale_event'))",
            &[],
        )
        .await
        .unwrap();
    drop(blocker);
    assert!(maintain(h.orm(), &owner).await.unwrap().is_complete());
    h.cleanup().await.unwrap();
}
