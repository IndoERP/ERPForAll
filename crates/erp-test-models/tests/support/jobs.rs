//! Helpers of the job queue tests: a resolver that accepts the stored subject, a runner
//! on a harness, and small raw-SQL readers.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers"
)]

use erp_orm::jobs::{
    EnqueueOutcome, EnqueueRequest, JobPrincipalResolver, JobRef, JobRunner, PrincipalRefusal,
    StoredPrincipal,
};
use erp_orm::{BoxFuture, Principal, Transaction};
use erp_testkit::Harness;

/// Accepts the principal the chunk's transaction was opened with (the harness policy resolves no
/// groups), after the same company check the real resolver makes.
pub struct PassResolver;

impl JobPrincipalResolver for PassResolver {
    fn admit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        stored: &'a StoredPrincipal,
    ) -> BoxFuture<'a, Result<Principal, PrincipalRefusal>> {
        Box::pin(async move {
            let principal = tx.env().principal().clone();
            if principal.subject() != stored.subject {
                return Err(PrincipalRefusal::new("subject_unavailable"));
            }
            Ok(principal)
        })
    }
}

/// A runner on `h` with the pass-through resolver and the default settings.
pub fn runner(h: &Harness) -> JobRunner {
    JobRunner::builder(h.orm().clone(), PassResolver)
        .build()
        .unwrap()
}

/// Enqueue `requests` as user 7 in one committed transaction.
pub async fn enqueue(h: &Harness, requests: Vec<EnqueueRequest>) -> Vec<JobRef> {
    let mut tx = h.begin_as(7).await.unwrap();
    let env = tx.env();
    let outcomes: Vec<EnqueueOutcome> = tx.enqueue_many(&env, requests).await.unwrap();
    tx.commit().await.unwrap();
    outcomes.iter().map(EnqueueOutcome::job).collect()
}

/// One text cell of a query on a fresh transaction.
pub async fn cell(h: &Harness, sql: &str) -> String {
    let mut tx = h.begin().await.unwrap();
    super::one(&mut tx, sql).await.unwrap_or_default()
}

/// Run `sql` (committing).
pub async fn exec(h: &Harness, sql: &str) {
    let mut tx = h.begin().await.unwrap();
    tx.batch(sql).await.unwrap();
    tx.commit().await.unwrap();
}

/// `state` of the job.
pub async fn state(h: &Harness, job: JobRef) -> String {
    cell(
        h,
        &format!("SELECT state FROM erp_job WHERE id = {}", job.id()),
    )
    .await
}

/// A handler whose behaviour the payload names (`{"do": "<action>"}`), so one kind serves the
/// retry, fence and outcome tests.
pub struct Script;

/// Times the `serialization_once` action ran its closure.
pub static SERIALIZATION_RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub static SCRIPT: Script = Script;

impl erp_orm::jobs::JobHandler for Script {
    fn run<'a>(
        &'a self,
        ctx: &'a mut erp_orm::jobs::JobContext<'_>,
    ) -> BoxFuture<'a, Result<erp_orm::jobs::RunOutcome, erp_orm::jobs::JobError>> {
        use erp_orm::jobs::{ChunkResult, JobError, RunOutcome};
        Box::pin(async move {
            let payload: serde_json::Value = ctx.payload()?;
            let action = payload
                .get("do")
                .and_then(|a| a.as_str())
                .unwrap_or("ok")
                .to_owned();
            match action.as_str() {
                "ok" => {
                    ctx.chunk(|_tx| {
                        Box::pin(async {
                            Ok(ChunkResult::done(serde_json::json!({"ok": true}), 1))
                        })
                    })
                    .await?;
                    Ok(RunOutcome::Done)
                }
                "native_done" => Ok(RunOutcome::Done),
                "continue" => Ok(RunOutcome::Continue),
                "transient" => Err(JobError::transient("flaky", "temporary failure")),
                "transient_huge" => Err(JobError::transient("flaky", "x".repeat(4_096))),
                "permanent" => Err(JobError::permanent("broken", "cannot be fixed")),
                "yield" => Err(JobError::Yield {
                    after: std::time::Duration::from_secs(60),
                }),
                "progress_then_transient" => {
                    ctx.chunk(|_tx| {
                        Box::pin(async {
                            Ok(ChunkResult::more(
                                serde_json::json!({"step": 1}),
                                2,
                                Some(4),
                            ))
                        })
                    })
                    .await?;
                    Err(JobError::transient(
                        "flaky",
                        "temporary failure after progress",
                    ))
                }
                "serialization_once" => {
                    let report = ctx
                        .chunk(|_tx| {
                            Box::pin(async {
                                let run = SERIALIZATION_RUNS
                                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                if run == 0 {
                                    return Err(JobError::from(
                                        erp_fields::Error::new(
                                            erp_fields::ErrorKind::Concurrency,
                                            "could not serialize access",
                                        )
                                        .with_sqlstate(Some("40001".to_owned())),
                                    ));
                                }
                                Ok(ChunkResult::done(serde_json::json!({"ok": true}), 1))
                            })
                        })
                        .await?;
                    assert_eq!(report.retries, 1);
                    Ok(RunOutcome::Done)
                }
                "panic" => panic!("the script handler was told to panic"),
                "sleep" => {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    Ok(RunOutcome::Done)
                }
                other => Err(JobError::permanent(
                    "unknown_action",
                    format!("the script has no action '{other}'"),
                )),
            }
        })
    }
}

/// A script job.
pub fn script(action: &str) -> erp_orm::jobs::EnqueueRequest {
    erp_orm::jobs::EnqueueRequest::new("test_jobs.script")
        .payload(&serde_json::json!({ "do": action }))
        .unwrap()
}
