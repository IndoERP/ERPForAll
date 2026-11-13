// SPDX-License-Identifier: MIT OR Apache-2.0
//! Runner selection is an execution boundary, including direct claims and reaping.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
mod support;
use erp_fields::ErrorKind;
use erp_orm::{BoxFuture, jobs::*};
use erp_test_models::base_users;
use erp_testkit::Harness;
use std::sync::Arc;
use support::jobs::{PassResolver, cell, enqueue, exec, state};
use tokio::sync::Semaphore;
struct Nop;
impl JobHandler for Nop {
    fn run<'a>(&'a self, _: &'a mut JobContext<'_>) -> BoxFuture<'a, Result<RunOutcome, JobError>> {
        Box::pin(async { Ok(RunOutcome::Done) })
    }
}
static NOP: Nop = Nop;
erp_orm::register_job_kind!(
    MAINT,
    JobKindDecl::new("base", "base.test_maintenance", HandlerRef::Native(&NOP))
        .principal(PrincipalPolicy::System)
        .timeouts(TimeoutClass::Maintenance)
        .isolation(KindIsolation::ReadCommitted)
);
erp_orm::register_job_kind!(
    CALLER,
    JobKindDecl::new("base", "base.test_caller", HandlerRef::Native(&NOP))
);
erp_orm::register_job_kind!(
    DEFAULT_TIMEOUT,
    JobKindDecl::new("base", "base.test_default", HandlerRef::Native(&NOP))
        .principal(PrincipalPolicy::System)
);
erp_orm::register_job_kind!(
    STEP,
    JobKindDecl::new(
        "base",
        "base.test_step",
        HandlerRef::Step {
            model: "res.users",
            method: "test"
        }
    )
);
erp_orm::register_job_kind!(
    REPEATABLE,
    JobKindDecl::new("base", "base.test_repeatable", HandlerRef::Native(&NOP))
        .principal(PrincipalPolicy::System)
        .timeouts(TimeoutClass::Maintenance)
);
static SPOOF: JobKindDecl =
    JobKindDecl::new("base", "base.test_maintenance", HandlerRef::Native(&NOP))
        .principal(PrincipalPolicy::System)
        .timeouts(TimeoutClass::Maintenance)
        .isolation(KindIsolation::ReadCommitted);
static UNKNOWN: JobKindDecl =
    JobKindDecl::new("base", "base.unregistered", HandlerRef::Native(&NOP));
async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON])
        .build()
        .await
        .unwrap()
}
fn builder(h: &Harness) -> JobRunnerBuilder {
    JobRunner::builder(h.orm().clone(), PassResolver)
}

#[tokio::test]
async fn maintenance_selection_rejects_unregistered_and_nonmaintenance_kinds() {
    let h = harness().await;
    for decl in [
        &UNKNOWN,
        &SPOOF,
        &CALLER,
        &DEFAULT_TIMEOUT,
        &STEP,
        &REPEATABLE,
    ] {
        let err = builder(&h).maintenance_kinds(&[decl]).build().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!(
                "Maintenance runner kind '{}' must be an exact registered native System declaration with Maintenance timeouts and ReadCommitted isolation.",
                decl.name
            )
        );
    }
    let err = builder(&h).maintenance_kinds(&[]).build().unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "A maintenance runner requires at least one admitted kind."
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn ordinary_and_maintenance_claim_disjoint_kinds_and_recheck_execution() {
    let h = harness().await;
    let ids = enqueue(
        &h,
        vec![
            EnqueueRequest::new(MAINT.name),
            EnqueueRequest::new(CALLER.name),
        ],
    )
    .await;
    let owner = builder(&h).maintenance_kinds(&[&MAINT]).build().unwrap();
    let ordinary = builder(&h).exclude_kinds([MAINT.name]).build().unwrap();
    let mut claim = ordinary.claim(10).await.unwrap();
    assert_eq!(claim.len(), 1);
    assert_eq!(claim[0].job(), ids[1]);
    let err = owner.run_claimed(claim.pop().unwrap()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "The claimed job kind is not admitted by this runner."
    );
    assert_eq!(
        state(&h, ids[1]).await,
        "running",
        "wrong runner must not settle a claim"
    );
    let mut claim = owner.claim(10).await.unwrap();
    assert_eq!(claim.len(), 1);
    assert_eq!(claim[0].job(), ids[0]);
    owner.run_claimed(claim.pop().unwrap()).await.unwrap();
    assert_eq!(
        state(&h, ids[0]).await,
        "",
        "completed jobs leave the active queue"
    );
    assert_eq!(
        cell(
            &h,
            "SELECT outcome FROM erp_job_run WHERE kind='base.test_maintenance'"
        )
        .await,
        "done"
    );
    enqueue(&h, vec![EnqueueRequest::new(CALLER.name)]).await;
    assert!(owner.next_due().await.unwrap().is_none());
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn owner_reaper_never_touches_an_unrelated_kind() {
    let h = harness().await;
    let ids = enqueue(
        &h,
        vec![
            EnqueueRequest::new(MAINT.name),
            EnqueueRequest::new(CALLER.name),
        ],
    )
    .await;
    let all = builder(&h).build().unwrap();
    assert_eq!(all.claim(10).await.unwrap().len(), 2);
    exec(&h, "UPDATE erp_job SET lease_until=clock_timestamp()-interval '1 second' WHERE state='running'").await;
    let owner = builder(&h).maintenance_kinds(&[&MAINT]).build().unwrap();
    assert_eq!(owner.reap(10).await.unwrap().requeued, 1);
    assert_eq!(state(&h, ids[0]).await, "ready");
    assert_eq!(state(&h, ids[1]).await, "running");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn runners_share_database_and_process_capacity() {
    let h = harness().await;
    let db = Arc::new(Semaphore::new(1));
    let process = Arc::new(Semaphore::new(1));
    let owner = builder(&h)
        .maintenance_kinds(&[&MAINT])
        .database_slots(db.clone())
        .process_slots(process.clone())
        .build()
        .unwrap();
    let ordinary = builder(&h)
        .exclude_kinds([MAINT.name])
        .database_slots(db.clone())
        .process_slots(process.clone())
        .build()
        .unwrap();
    enqueue(
        &h,
        vec![
            EnqueueRequest::new(MAINT.name),
            EnqueueRequest::new(CALLER.name),
        ],
    )
    .await;
    let held = db.acquire().await.unwrap();
    assert_eq!(owner.tick().await.unwrap().claimed, 0);
    assert_eq!(ordinary.tick().await.unwrap().claimed, 0);
    drop(held);
    let held = process.acquire().await.unwrap();
    assert_eq!(owner.tick().await.unwrap().claimed, 0);
    assert_eq!(ordinary.tick().await.unwrap().claimed, 0);
    drop(held);
    assert_eq!(owner.tick().await.unwrap().claimed, 1);
    assert_eq!(ordinary.tick().await.unwrap().claimed, 1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn exclusion_remains_additive_and_conflicting_maintenance_selection_is_refused() {
    let h = harness().await;
    let err = builder(&h)
        .maintenance_kinds(&[&MAINT])
        .exclude_kinds([MAINT.name])
        .build()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Maintenance runner inclusion conflicts with excluded kinds."
    );
    let runner = builder(&h)
        .exclude_kinds([MAINT.name])
        .exclude_kinds([CALLER.name])
        .build()
        .unwrap();
    enqueue(
        &h,
        vec![
            EnqueueRequest::new(MAINT.name),
            EnqueueRequest::new(CALLER.name),
        ],
    )
    .await;
    assert!(runner.claim(10).await.unwrap().is_empty());
    h.cleanup().await.unwrap();
}
