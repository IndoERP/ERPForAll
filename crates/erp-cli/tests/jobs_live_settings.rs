// SPDX-License-Identifier: MIT OR Apache-2.0
//! Committed policy through real base Settings storage and an independent bounded reader pool.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
use erp_addon_base::{
    ir_config_setting::write_values, jobs_settings_source::DatabaseJobSettingsSource,
};
use erp_orm::jobs::{JobSettings, Setting};
use erp_sdk::views::wire::SettingsScope;
use erp_sdk::{ErrorKind, Orm, SUPERUSER_ID, Value};
use std::sync::Arc;

async fn database() -> (erp_testkit::TestDb, Orm) {
    let db = erp_testkit::TestDb::create().await.unwrap();
    let mut index = erp_modules::AddonIndex::new(true);
    index.add_dev_path(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("addons"),
    );
    let identities = index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                std::path::Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect();
    let system = erp_modules::system::ModuleSystem::builder(db.pool().clone(), index)
        .native_artifact_identities(identities)
        .code([&erp_addon_base::ADDON, &erp_addon_uom::ADDON])
        .build()
        .unwrap();
    let initialized = system
        .initialize(erp_modules::system::InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let reader = Orm::builder(
        initialized.orm.registry().clone(),
        erp_orm::pool::create_pool(db.config(), 1).unwrap(),
    )
    .build()
    .unwrap();
    let source = Arc::new(DatabaseJobSettingsSource::new(reader).unwrap());
    let orm = initialized
        .orm
        .rebuild(initialized.orm.registry().clone())
        .job_settings_source(source)
        .build()
        .unwrap();
    (db, orm)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn database_policy_is_committed_isolated_and_fail_closed() {
    let (db, orm) = database().await;
    let (other_db, other) = database().await;
    let before = orm.job_settings_snapshot().await.unwrap();
    assert_eq!(
        before.settings().max_attempts(),
        JobSettings::default().max_attempts()
    );
    let digest = erp_orm::node::SectionDigest::of(orm.job_settings_identity()).unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        before.revision().to_string(),
        vec![("base.jobs.max_attempts".into(), Value::Int(3))],
    )
    .await
    .unwrap();
    assert_eq!(
        orm.job_settings_snapshot().await.unwrap().revision(),
        before.revision()
    );
    tx.rollback().await.unwrap();
    drop(tx);
    assert_eq!(
        orm.job_settings_snapshot().await.unwrap().revision(),
        before.revision()
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    write_values(
        &env,
        &mut tx,
        SettingsScope::Global {},
        before.revision().to_string(),
        vec![("base.jobs.max_attempts".into(), Value::Int(3))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let after = orm.job_settings_snapshot().await.unwrap();
    assert_eq!(after.settings().max_attempts(), 3);
    assert!(after.revision() > before.revision());
    assert_eq!(
        other
            .job_settings_snapshot()
            .await
            .unwrap()
            .settings()
            .max_attempts(),
        before.settings().max_attempts()
    );
    assert_eq!(
        digest.digest(),
        erp_orm::node::SectionDigest::of(orm.job_settings_identity())
            .unwrap()
            .digest()
    );
    let invalid = orm
        .rebuild(orm.registry().clone())
        .job_settings(
            JobSettings::builder()
                .set(Setting::MaxAttempts, 4)
                .build()
                .unwrap(),
        )
        .build()
        .err()
        .expect("Shared process override must be refused");
    assert_eq!(invalid.kind(), ErrorKind::Validation);
    assert_eq!(
        invalid.message(),
        "Remove process override jobs.max_attempts; migrate Shared policy through Settings before enabling database job policy."
    );
    let local = orm
        .rebuild(orm.registry().clone())
        .job_settings(
            JobSettings::builder()
                .set(Setting::Workers, 8)
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    assert_eq!(
        local
            .job_settings_snapshot()
            .await
            .unwrap()
            .settings()
            .workers(),
        8
    );
    assert_eq!(
        local
            .job_settings_snapshot()
            .await
            .unwrap()
            .settings()
            .max_attempts(),
        3
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("UPDATE ir_config_setting_value SET value_text='\"corrupt\"' WHERE setting_key='base.jobs.max_attempts'", &[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let error = orm.job_settings_snapshot().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Stored setting base.jobs.max_attempts is invalid."
    );
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("UPDATE ir_config_setting_value SET value_text='3' WHERE setting_key='base.jobs.max_attempts'", &[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(
        orm.job_settings_snapshot()
            .await
            .unwrap()
            .settings()
            .max_attempts(),
        3
    );
    drop(local);
    drop(other);
    drop(orm);
    other_db.cleanup().await.unwrap();
    db.cleanup().await.unwrap();
}

#[path = "support/native_http.rs"]
mod native_http;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_replicas_recover_policy_without_hints() {
    use native_http::*;
    use serde_json::{Value as Json, json};
    use std::time::{Duration, Instant};
    let fixture = NativeFixture::start_with_config("jobs_run_in_app=true").await;
    let peer = fixture.peer_with_config("jobs_run_in_app=true");
    let read = json_post(
        fixture.port,
        &fixture.cookies,
        "/erp/json/2/ir.config.setting/erp_read_values",
        &json!({"scope":{"kind":"global"},"keys":["base.jobs.history_days"]}).to_string(),
    );
    assert_eq!(read.status, 200, "{}", read.body);
    let current: Json = serde_json::from_str(&read.body).unwrap();
    let save = json_post(
        peer.port,
        &fixture.cookies,
        "/erp/json/2/ir.config.setting/erp_write_values",
        &json!({"scope":{"kind":"global"},"expected_revision":current["revision"],"changes":[
            {"key":"base.jobs.history_days","value":7},
            {"key":"base.jobs.poll_interval_secs","value":1}]})
        .to_string(),
    );
    assert_eq!(save.status, 200, "{}", save.body);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let count: i64 = fixture
            .client
            .query_one(
                "SELECT count(*) FROM erp_job WHERE kind='core.jobs_history_retention'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if count == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "core schedule was not installed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    fixture.client.execute("INSERT INTO erp_job_run(job_uuid,kind,kind_version,outcome,attempts,enqueued_at,finished_at,items_done,items_failed,run_as_kind,run_as_user,enqueued_by_user,last_error_code) SELECT job_uuid,kind,kind_version,'done',0,enqueued_at,clock_timestamp()-interval '2 days',0,0,run_as_kind,run_as_user,enqueued_by_user,'jobs-live-test' FROM erp_job WHERE kind='core.jobs_history_retention'", &[]).await.unwrap();
    // Deliberately omit both notification and revision wake hints. Every admission re-reads
    // committed storage; the runner's bounded poll is the recovery path.
    fixture.client.execute("UPDATE ir_config_setting_value SET value_text='\"broken\"' WHERE setting_key='base.jobs.history_days'", &[]).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let a = std::fs::read_to_string(&fixture.stderr_path).unwrap();
        let b = std::fs::read_to_string(peer.dir.path().join("server.stderr")).unwrap();
        if a.contains("job policy reload failed") && b.contains("job policy reload failed") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "both runners must visibly refuse bad policy: {a} {b}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    fixture.client.execute("UPDATE erp_job SET run_after=clock_timestamp() WHERE kind='core.jobs_history_retention'", &[]).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let count: i64 = fixture
        .client
        .query_one(
            "SELECT count(*) FROM erp_job_run WHERE last_error_code='jobs-live-test'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1, "invalid policy must stop retention admission");
    fixture.client.execute("UPDATE ir_config_setting_value SET value_text='1' WHERE setting_key='base.jobs.history_days'", &[]).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let count: i64 = fixture
            .client
            .query_one(
                "SELECT count(*) FROM erp_job_run WHERE last_error_code='jobs-live-test'",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if count == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "committed repaired policy was not adopted by polling"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(peer);
    fixture.cleanup().await;
}
