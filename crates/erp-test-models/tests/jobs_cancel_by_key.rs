//! a caller-scoped cancel by (kind, dedup key) (`Transaction::job_cancel_by_key`).
//! Added, whose scheduled messages keep one job per row and drop it when the
//! row is cancelled or rescheduled.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_orm::StaticPolicy;
use erp_orm::jobs::{HandlerRef, JobKey, JobKindDecl, MAX_CANCEL_KEYS, OnDuplicate};
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::jobs::{SCRIPT, cell, enqueue, exec, script};

erp_orm::register_job_kind!(
    SCRIPT_KIND,
    JobKindDecl::new("test_jobs", "test_jobs.script", HandlerRef::Native(&SCRIPT)).max_attempts(2)
);

const KIND: &str = "test_jobs.script";

async fn harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON])
        .policy(StaticPolicy::AllowAll)
        .build()
        .await
        .expect("harness");
    exec(
        &h,
        "INSERT INTO res_users (id, name, login, active) OVERRIDING SYSTEM VALUE \
         VALUES (7, 'u7', 'u7', true), (8, 'u8', 'u8', true)",
    )
    .await;
    h
}

fn keyed(key: &str) -> erp_orm::jobs::EnqueueRequest {
    script("native_done").dedup(JobKey::new(key).unwrap(), OnDuplicate::Keep)
}

/// Cancel by key as `uid` (`None`: superuser mode), committed; the count.
async fn cancel(h: &Harness, uid: Option<i64>, key: &str) -> u64 {
    let mut tx = match uid {
        Some(uid) => h.begin_as(uid).await.unwrap(),
        None => h.begin().await.unwrap(),
    };
    let env = tx.env();
    let count = tx
        .job_cancel_by_key(&env, KIND, &JobKey::new(key).unwrap())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    count
}

async fn live(h: &Harness) -> String {
    cell(h, "SELECT count(*)::text FROM erp_job").await
}

#[tokio::test]
async fn a_user_cancels_only_the_jobs_they_enqueued_for_the_key() {
    let h = harness().await;
    enqueue(&h, vec![keyed("row:1"), keyed("row:2")]).await;
    assert_eq!(live(&h).await, "2");
    // another user's cancel touches nothing, and says so with a zero
    assert_eq!(cancel(&h, Some(8), "row:1").await, 0);
    assert_eq!(live(&h).await, "2");
    // a key nobody enqueued is a zero too, never an error
    assert_eq!(cancel(&h, Some(7), "row:99").await, 0);
    // the owner cancels exactly the key's job; the other key stays
    assert_eq!(cancel(&h, Some(7), "row:1").await, 1);
    assert_eq!(live(&h).await, "1");
    assert_eq!(
        cell(
            &h,
            "SELECT string_agg(outcome, ',') FROM erp_job_run WHERE kind = 'test_jobs.script'"
        )
        .await,
        "cancelled"
    );
    // the second time there is nothing left
    assert_eq!(cancel(&h, Some(7), "row:1").await, 0);
    // superuser mode (server code) cancels the job of any user
    assert_eq!(cancel(&h, None, "row:2").await, 1);
    assert_eq!(live(&h).await, "0");
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_running_job_is_asked_to_stop_and_a_dead_letter_is_not_live() {
    let h = harness().await;
    enqueue(&h, vec![keyed("row:running"), keyed("row:dead")]).await;
    exec(
        &h,
        "UPDATE erp_job SET state = 'running', lease_owner = gen_random_uuid(), \
         lease_until = clock_timestamp() + interval '1 minute' WHERE dedup_key = 'row:running'",
    )
    .await;
    exec(
        &h,
        "UPDATE erp_job SET state = 'dead', dead_at = clock_timestamp(), last_error_code = 'broken' \
         WHERE dedup_key = 'row:dead'",
    )
    .await;
    assert_eq!(cancel(&h, Some(7), "row:running").await, 1);
    assert_eq!(
        cell(
            &h,
            "SELECT cancel_requested::text FROM erp_job WHERE dedup_key = 'row:running'"
        )
        .await,
        "true"
    );
    assert_eq!(cancel(&h, Some(7), "row:dead").await, 0);
    assert_eq!(
        cell(&h, "SELECT state FROM erp_job WHERE dedup_key = 'row:dead'").await,
        "dead"
    );
    h.cleanup().await.unwrap();
}

/// Cancel by many keys as `uid` (`None`: superuser mode), committed; the count.
async fn cancel_many(h: &Harness, uid: Option<i64>, keys: &[&str]) -> u64 {
    let mut tx = match uid {
        Some(uid) => h.begin_as(uid).await.unwrap(),
        None => h.begin().await.unwrap(),
    };
    let env = tx.env();
    let keys: Vec<JobKey> = keys.iter().map(|k| JobKey::new(k).unwrap()).collect();
    let count = tx.job_cancel_by_keys(&env, KIND, &keys).await.unwrap();
    tx.commit().await.unwrap();
    count
}

#[tokio::test]
async fn many_keys_are_cancelled_in_one_call_for_their_owner_only() {
    let h = harness().await;
    enqueue(&h, vec![keyed("k:1"), keyed("k:2"), keyed("k:3")]).await;
    // a job of another user under a key of the same list
    let mut tx = h.begin_as(8).await.unwrap();
    let env = tx.env();
    tx.enqueue(&env, keyed("k:4")).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(live(&h).await, "4");
    // an empty list is a zero and sends no statement that matters
    assert_eq!(cancel_many(&h, Some(7), &[]).await, 0);
    // the owner of three keys cancels three, and not the fourth key's job of user 8
    assert_eq!(
        cancel_many(&h, Some(7), &["k:1", "k:2", "k:3", "k:4", "k:5"]).await,
        3
    );
    assert_eq!(live(&h).await, "1");
    assert_eq!(
        cell(
            &h,
            "SELECT count(*)::text FROM erp_job_run WHERE outcome = 'cancelled'"
        )
        .await,
        "3"
    );
    // user 7 cancelled nothing of user 8's; user 8 and superuser mode each find their own
    assert_eq!(cancel_many(&h, Some(7), &["k:4"]).await, 0);
    assert_eq!(cancel_many(&h, None, &["k:4", "k:4"]).await, 1);
    assert_eq!(live(&h).await, "0");
    // the bound on one call is typed
    let mut tx = h.begin().await.unwrap();
    let env = tx.env();
    let keys: Vec<JobKey> = (0..=MAX_CANCEL_KEYS)
        .map(|n| JobKey::new(&format!("k:{n}")).unwrap())
        .collect();
    let error = tx.job_cancel_by_keys(&env, KIND, &keys).await.unwrap_err();
    assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(
        error.message(),
        format!(
            "job_cancel_by_keys takes at most {MAX_CANCEL_KEYS} keys per call, not {}",
            MAX_CANCEL_KEYS + 1
        )
    );
    h.cleanup().await.unwrap();
}
