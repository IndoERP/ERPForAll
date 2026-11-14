// SPDX-License-Identifier: MIT OR Apache-2.0
//! Admission snapshots remain immutable across changes to the committed settings source.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod support;
use erp_fields::{Error, ErrorKind, Result};
use erp_orm::BoxFuture;
use erp_orm::jobs::{
    ChunkResult, EnqueueRequest, HandlerRef, JobContext, JobError, JobHandler, JobKindDecl,
    JobRunner, JobSettings, JobSettingsCompatibility, JobSettingsSnapshot, JobSettingsSource,
    RunOutcome, Setting,
};
use erp_test_models::base_users;
use erp_testkit::Harness;
use std::sync::{Arc, RwLock};
use support::jobs::PassResolver;

#[derive(Clone)]
struct Source(Arc<RwLock<Option<JobSettingsSnapshot>>>);
impl JobSettingsSource for Source {
    fn compatibility(&self) -> JobSettingsCompatibility {
        JobSettingsCompatibility::new("test.live", 1).unwrap()
    }
    fn read(&self) -> BoxFuture<'_, Result<JobSettingsSnapshot>> {
        Box::pin(async {
            self.0
                .read()
                .unwrap()
                .clone()
                .ok_or_else(|| Error::validation("Job policy unavailable."))
        })
    }
}
fn snapshot(revision: u64, limit: u64) -> JobSettingsSnapshot {
    JobSettingsSnapshot::new(
        revision,
        JobSettings::builder()
            .set(Setting::ProgressMaxBytes, limit)
            .set(Setting::MaxAttempts, revision + 2)
            .set(Setting::LeaseSecs, if revision == 1 { 120 } else { 15 })
            .set(
                Setting::JobDeadlineSecs,
                if revision == 1 { 60 } else { 10 },
            )
            .set(Setting::RunBudgetSecs, if revision == 1 { 2 } else { 1 })
            .set(
                Setting::ChunkDefaultLockMs,
                if revision == 1 { 200 } else { 100 },
            )
            .set(
                Setting::ChunkDefaultStatementMs,
                if revision == 1 { 2000 } else { 1000 },
            )
            .set(
                Setting::ChunkDefaultTransactionMs,
                if revision == 1 { 4000 } else { 2000 },
            )
            .build()
            .unwrap(),
    )
    .unwrap()
}
struct Checkpoint;
impl JobHandler for Checkpoint {
    fn run<'a>(
        &'a self,
        ctx: &'a mut JobContext<'_>,
    ) -> BoxFuture<'a, std::result::Result<RunOutcome, JobError>> {
        Box::pin(async move {
            // The first run was claimed before the limit dropped. The second may still read
            // this checkpoint, but must refuse replacing it with another oversized value.
            let revision = ctx.settings_revision();
            let expected = snapshot(revision, if revision == 1 { 8192 } else { 4096 });
            assert_eq!(ctx.settings(), expected.settings());
            let expected_statement =
                i32::try_from(ctx.settings().get(Setting::ChunkDefaultStatementMs)).unwrap();
            let old: Option<serde_json::Value> = ctx.progress()?;
            if revision == 2 {
                assert_eq!(old.unwrap()["text"].as_str().unwrap().len(), 6000);
            }
            let result = ctx
                .chunk(move |tx| {
                    Box::pin(async move {
                        let rows = tx.query("SELECT setting::int4 FROM pg_settings WHERE name='statement_timeout'", &[]).await?;
                        assert_eq!(rows[0].get::<_, i32>(0), expected_statement);
                        Ok(ChunkResult::more(
                            serde_json::json!({"text": "x".repeat(6000)}),
                            1,
                            None,
                        ))
                    })
                })
                .await;
            if revision == 1 {
                result?;
            } else {
                let JobError::Permanent { code, message, .. } = result.unwrap_err() else {
                    panic!("expected a permanent progress-size refusal")
                };
                assert_eq!(code, "progress_too_large");
                let encoded = format!("{{\"text\": \"{}\"}}", "x".repeat(6000));
                assert_eq!(
                    message.as_ref(),
                    format!(
                        "the progress of a chunk is {} bytes; the limit is {} bytes (jobs.progress_max_bytes)",
                        encoded.len(),
                        ctx.settings().progress_max_bytes()
                    )
                );
            }
            Ok(RunOutcome::Hold)
        })
    }
}
static CHECKPOINT: Checkpoint = Checkpoint;
erp_orm::register_job_kind!(
    LIVE,
    JobKindDecl::new(
        "test_jobs",
        "test_jobs.live",
        HandlerRef::Native(&CHECKPOINT)
    )
);

#[tokio::test]
async fn admitted_run_retains_policy_and_new_run_reads_old_checkpoint() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let source = Source(Arc::new(RwLock::new(Some(snapshot(1, 8192)))));
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings_source(Arc::new(source.clone()))
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_orm::UserId::new(7).unwrap()).await.unwrap();
    let env = tx.env();
    let jobs = tx
        .enqueue_many(
            &env,
            vec![
                EnqueueRequest::new("test_jobs.live"),
                EnqueueRequest::new("test_jobs.live")
                    .singleton(erp_orm::jobs::JobKey::new("live-snapshot").unwrap()),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let runner = JobRunner::builder(orm.clone(), PassResolver)
        .build()
        .unwrap();
    let claims = runner.claim(2).await.unwrap();
    assert_eq!(claims.len(), 2);
    *source.0.write().unwrap() = Some(snapshot(2, 4096));
    for claim in claims {
        runner.run_claimed(claim).await.unwrap();
    }
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let id = jobs[0].job().id();
    assert_eq!(
        tx.query("SELECT max_attempts FROM erp_job WHERE id=$1", &[&id])
            .await
            .unwrap()[0]
            .get::<_, i32>(0),
        3
    );
    tx.execute("UPDATE erp_job SET state='ready' WHERE id=$1", &[&id])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let claim = runner.claim(1).await.unwrap().remove(0);
    runner.run_claimed(claim).await.unwrap();
    let mut tx = orm.begin(erp_orm::UserId::new(7).unwrap()).await.unwrap();
    let env = tx.env();
    let added = tx
        .enqueue_many(
            &env,
            vec![
                EnqueueRequest::new("test_jobs.live"),
                EnqueueRequest::new("test_jobs.live")
                    .max_attempts(5)
                    .unwrap(),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    for (job, expected) in added.iter().zip([4, 5]) {
        assert_eq!(
            tx.query(
                "SELECT max_attempts FROM erp_job WHERE id=$1",
                &[&job.job().id()]
            )
            .await
            .unwrap()[0]
                .get::<_, i32>(0),
            expected
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    // Rebuild retains authority; failed reload admits nothing and never returns static defaults.
    *source.0.write().unwrap() = None;
    let rebuilt = orm.rebuild(h.registry().clone()).build().unwrap();
    let error = rebuilt.job_settings_snapshot().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Job policy unavailable.");
    let error = runner.claim(1).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Job policy unavailable.");
    let mut tx = orm.begin(erp_orm::UserId::new(7).unwrap()).await.unwrap();
    let env = tx.env();
    let error = tx
        .enqueue_many(&env, vec![EnqueueRequest::new("test_jobs.live")])
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.message(), "Job policy unavailable.");
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[test]
fn source_refuses_node_settings() {
    let error = JobSettingsSnapshot::new(
        1,
        JobSettings::builder()
            .set(Setting::Workers, 8)
            .build()
            .unwrap(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Database job policy cannot override jobs.workers."
    );
}

#[test]
fn provider_contract_is_hashed_and_invalid_identities_fail() {
    use erp_orm::node::SectionDigest;
    let a = JobSettingsCompatibility::new("base.policy", 1).unwrap();
    let b = JobSettingsCompatibility::new("base.policy", 2).unwrap();
    assert_ne!(
        SectionDigest::of(&a).unwrap().digest(),
        SectionDigest::of(&b).unwrap().digest()
    );
    for (provider, version) in [("", 1), ("bad provider", 1), ("base.policy", 0)] {
        let error = JobSettingsCompatibility::new(provider, version).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Invalid job settings provider compatibility identity."
        );
    }
}

struct Stalled;
impl JobSettingsSource for Stalled {
    fn compatibility(&self) -> JobSettingsCompatibility {
        JobSettingsCompatibility::new("test.stalled", 1).unwrap()
    }
    fn read(&self) -> BoxFuture<'_, Result<JobSettingsSnapshot>> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test]
async fn stalled_source_hits_node_transaction_timeout() {
    use std::time::Duration;
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let config = erp_orm::OrmConfig {
        timeouts: erp_orm::Timeouts::new(
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(3),
        )
        .unwrap(),
        ..Default::default()
    };
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .config(config)
        .job_settings_source(Arc::new(Stalled))
        .build()
        .unwrap();
    let error = orm.job_settings_snapshot().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Database);
    assert_eq!(
        error.message(),
        "Job policy read exceeded the node transaction timeout; new admission stopped."
    );
    h.cleanup().await.unwrap();
}

struct FirstReadFails {
    calls: std::sync::atomic::AtomicUsize,
}
impl JobSettingsSource for FirstReadFails {
    fn compatibility(&self) -> JobSettingsCompatibility {
        JobSettingsCompatibility::new("test.first-read-fails", 1).unwrap()
    }
    fn read(&self) -> BoxFuture<'_, Result<JobSettingsSnapshot>> {
        Box::pin(async {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Err(Error::validation("Job policy temporarily unavailable."));
            }
            JobSettingsSnapshot::new(
                1,
                JobSettings::builder()
                    .set(Setting::PollIntervalSecs, 1)
                    .build()
                    .unwrap(),
            )
        })
    }
}
#[tokio::test]
async fn core_schedules_are_retried_after_startup_policy_failure() {
    use std::time::{Duration, Instant};
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let orm = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings_source(Arc::new(FirstReadFails {
            calls: std::sync::atomic::AtomicUsize::new(0),
        }))
        .build()
        .unwrap();
    let handle = JobRunner::builder(orm, PassResolver)
        .build()
        .unwrap()
        .spawn();
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let count: i64 = h.db().client().await.unwrap().query_one("SELECT count(*) FROM erp_job WHERE kind IN ('core.jobs_history_retention','core.jobs_dead_expiry')", &[]).await.unwrap().get(0);
        if count == 2 {
            break;
        }
        assert!(
            Instant::now() < until,
            "core schedules were not retried after the committed policy source recovered"
        );
        assert!(
            !handle.is_finished(),
            "source failure must not terminate the runner"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    handle.shutdown().await;
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn explicit_shared_default_is_refused_in_database_mode() {
    let h = Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap();
    let source = Source(Arc::new(RwLock::new(Some(snapshot(1, 8192)))));
    let settings = JobSettings::builder()
        .set(Setting::MaxAttempts, Setting::MaxAttempts.default_value())
        .build()
        .unwrap();
    assert_eq!(
        settings,
        JobSettings::default(),
        "builder intent must not change effective-value equality"
    );
    let error = h
        .orm()
        .rebuild(h.registry().clone())
        .job_settings(settings)
        .job_settings_source(Arc::new(source))
        .build()
        .err()
        .expect("an explicit Shared default is still a process override");
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(
        error.message(),
        "Remove process override jobs.max_attempts; migrate Shared policy through Settings before enabling database job policy."
    );
    h.cleanup().await.unwrap();
}
