// SPDX-License-Identifier: MIT OR Apache-2.0
//! Scheduled attachment cleanup on a fresh database, through the job kinds and the runner only:
//! no test calls the ownership coordinator. The heartbeat job establishes ownership and opens the
//! observation window; the cleaner then releases consumed staging. A store this database does not
//! own, or a contested one, is skipped and reported while other rows are still cleaned.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod attachment_support;
mod support;

use attachment_support::*;
use erp_addon_base::attachments::{self, AttachmentContent, NewAttachment, gc, ownership};
use erp_blob::{AttachmentStore, Namespace, OwnerIdentity, OwnerMarker};
use erp_testkit::Harness;
use std::sync::Arc;

/// Stage `bytes` and link them to a new attachment, so the upload row is consumed (state 2).
async fn consumed(f: &Fixture, name: &str, bytes: &[u8]) {
    let mut tx = f.h.begin().await.unwrap();
    let env = tx.env();
    tx.rollback().await.unwrap();
    drop(tx);
    let up = stage(f, &env, name, bytes).await;
    let mut tx = f.h.begin().await.unwrap();
    attachments::create(
        &mut tx,
        vec![NewAttachment::new(name, AttachmentContent::Upload(up.id))],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn exec(f: &Fixture, sql: &str) {
    exec_h(&f.h, sql).await;
}

async fn exec_h(h: &Harness, sql: &str) {
    let mut tx = h.begin().await.unwrap();
    tx.batch(sql).await.unwrap();
    tx.commit().await.unwrap();
}

/// Make the jobs matching `kinds` (a LIKE pattern) due and let one runner tick, `times` times.
async fn run(f: &Fixture, kinds: &str, times: usize) {
    run_h(&f.h, kinds, times).await;
}

async fn run_h(h: &Harness, kinds: &str, times: usize) {
    let runner = support::jobs::runner(h);
    for _ in 0..times {
        exec_h(
            h,
            &format!(
                "UPDATE erp_job SET run_after = now() WHERE kind LIKE '{kinds}' AND state = 'ready'"
            ),
        )
        .await;
        runner.tick().await.unwrap();
    }
}

/// `(subject reserved bytes, database reserved bytes)` of the staging budget, as `quota::admit`
/// sums them.
async fn reserved(f: &Fixture, subject: i64) -> (i64, i64) {
    let mut tx = f.h.begin().await.unwrap();
    let row = &tx
        .query(
            "SELECT COALESCE(sum(reserved_bytes) FILTER (WHERE subject_user=$1),0)::int8, \
             COALESCE(sum(reserved_bytes),0)::int8 FROM ir_blob_upload",
            &[&subject],
        )
        .await
        .unwrap()[0];
    let pair = (row.get(0), row.get(1));
    tx.rollback().await.unwrap();
    pair
}

async fn no_dead_jobs(f: &Fixture) {
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM erp_job WHERE kind LIKE 'base.attachment_%' AND state = 'dead'"
        )
        .await,
        0
    );
}

/// The clean job's last chunk report of `skipped` rows.
async fn skipped(f: &Fixture) -> i64 {
    scalar(
        &f.h,
        "SELECT COALESCE((progress->>'skipped')::int8, -1) FROM erp_job \
         WHERE kind = 'base.attachment_clean_staging'",
    )
    .await
}

/// The time the window takes is the only thing a test may shorten: the window itself was opened
/// by the heartbeat job (production delay 660 s), so its timestamps are moved into the past.
async fn window_elapsed(f: &Fixture) {
    exec(f, "UPDATE ir_blob_owner_state SET observation_started_at=clock_timestamp()-interval '700 seconds',observation_not_before=clock_timestamp()-interval '1 second'").await;
}

/// A fresh database with one consumed external upload and one consumed inline upload, its
/// schedules created and run until the heartbeat opened the observation window.
async fn scheduled() -> (Fixture, i64) {
    let f = fixture(external_limits()).await;
    consumed(&f, "external.txt", b"scheduled cleanup bytes").await;
    consumed(&f, "in.txt", b"abc").await;
    let subject = scalar(
        &f.h,
        "SELECT subject_user FROM ir_blob_upload WHERE inline_data IS NULL",
    )
    .await;
    assert_eq!(f.store.staging_keys().len(), 1);
    let (mine, all) = reserved(&f, subject).await;
    assert!(
        mine > 0 && all == mine,
        "consumed external staging is reserved"
    );
    gc::ensure_schedules(f.h.orm()).await.unwrap();
    // One kind per tick, so the order is fixed whatever the runner's slot count.
    run(&f, "base.attachment_owner_heartbeat", 1).await;
    assert_eq!(
        scalar_text(
            &f,
            "SELECT state || coalesce(' ' || last_error_code, '') || coalesce(' ' || last_error, '') \
             FROM erp_job WHERE kind = 'base.attachment_owner_heartbeat'"
        )
        .await,
        "ready",
        "the heartbeat ran and rescheduled itself"
    );
    run(&f, "base.attachment_clean_staging", 1).await;
    (f, subject)
}

#[tokio::test]
async fn scheduled_heartbeat_admits_scheduled_cleanup_of_consumed_staging() {
    let (f, subject) = scheduled().await;
    // Before the window completes the cleaner neither fails nor deletes: it reports the row.
    assert_eq!(
        ownership::health(f.h.orm(), "main").await.unwrap(),
        ownership::Health::Healthy
    );
    assert!(matches!(
        ownership::observation(f.h.orm(), "main").await.unwrap(),
        ownership::Observation::Waiting { .. }
    ));
    no_dead_jobs(&f).await;
    assert_eq!(skipped(&f).await, 1);
    assert_eq!(f.store.staging_keys().len(), 1);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_upload WHERE inline_data IS NOT NULL"
        )
        .await,
        0,
        "inline rows are cleaned while the store waits"
    );
    let before = reserved(&f, subject).await;
    assert!(before.0 > 0);

    window_elapsed(&f).await;
    run(&f, "base.attachment_owner_heartbeat", 1).await;
    run(&f, "base.attachment_clean_staging", 1).await;
    no_dead_jobs(&f).await;
    assert_eq!(skipped(&f).await, 0);
    assert!(f.store.staging_keys().is_empty());
    assert_eq!(
        scalar(&f.h, "SELECT count(*) FROM ir_blob_upload").await,
        0,
        "consumed external and inline rows are cleaned"
    );
    assert_eq!(reserved(&f, subject).await, (0, 0));
    f.h.cleanup().await.unwrap();
}

async fn namespace(f: &Fixture) -> Namespace {
    let ns: String = scalar_text(
        f,
        "SELECT namespace::text FROM ir_blob_store WHERE store_key='main'",
    )
    .await;
    Namespace::parse(&ns).unwrap()
}

async fn scalar_text(f: &Fixture, sql: &str) -> String {
    text_h(&f.h, sql).await
}

async fn text_h(h: &Harness, sql: &str) -> String {
    let mut tx = h.begin().await.unwrap();
    let v: String = tx.query(sql, &[]).await.unwrap()[0].get(0);
    tx.rollback().await.unwrap();
    v
}

/// A marker of another database (different physical identity) for this namespace.
async fn foreign_marker(f: &Fixture, ns: &Namespace) -> (OwnerMarker, erp_blob::OwnerState) {
    foreign_marker_in(&*f.store, ns).await
}

/// [`foreign_marker`] for any store.
async fn foreign_marker_in(
    store: &dyn AttachmentStore,
    ns: &Namespace,
) -> (OwnerMarker, erp_blob::OwnerState) {
    let current = store.owner(ns).await.unwrap().unwrap();
    let mine = current.marker().identity();
    let other = OwnerIdentity::new(
        mine.system_identifier().wrapping_add(1).max(1),
        mine.database_oid(),
        Namespace::random().unwrap(),
    )
    .unwrap();
    let marker = OwnerMarker::new(
        other,
        current.marker().generation() + 1,
        Namespace::random().unwrap(),
        current.marker().observed_at(),
    )
    .unwrap();
    (marker, current)
}

/// Run only the cleaner and check that it skipped the external row and kept its bytes and
/// reservation, while the inline row was still cleaned.
async fn refused_store_is_skipped(f: &Fixture, subject: i64) {
    consumed(f, "late.txt", b"xyz").await;
    let before = reserved(f, subject).await;
    run(f, "base.attachment_clean_staging", 1).await;
    no_dead_jobs(f).await;
    assert_eq!(skipped(f).await, 1);
    assert_eq!(f.store.staging_keys().len(), 1);
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_upload WHERE state=2 AND inline_data IS NULL"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_upload WHERE inline_data IS NOT NULL"
        )
        .await,
        0,
        "the inline row is cleaned although the store is refused"
    );
    assert_eq!(reserved(f, subject).await, before);
}

#[tokio::test]
async fn scheduled_cleanup_skips_a_store_this_database_does_not_own() {
    let (f, subject) = scheduled().await;
    window_elapsed(&f).await;
    let ns = namespace(&f).await;
    let (marker, current) = foreign_marker(&f, &ns).await;
    f.store
        .put_owner(&ns, &marker, Some(current.version()))
        .await
        .unwrap();
    refused_store_is_skipped(&f, subject).await;
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn scheduled_cleanup_skips_a_contested_store() {
    let (f, subject) = scheduled().await;
    window_elapsed(&f).await;
    let ns = namespace(&f).await;
    let (marker, _) = foreign_marker(&f, &ns).await;
    f.store.create_contested(&ns, &marker).await.unwrap();
    refused_store_is_skipped(&f, subject).await;
    f.h.cleanup().await.unwrap();
}

const HEARTBEAT: &str = "base.attachment_owner_heartbeat";
const CLEAN: &str = "base.attachment_clean_staging";

/// `state`, error code and error of the job of `kind`, as one text.
async fn job_outcome(h: &Harness, kind: &str) -> String {
    text_h(
        h,
        &format!(
            "SELECT state || coalesce(' ' || last_error_code, '') || coalesce(' ' || last_error, '') \
             FROM erp_job WHERE kind = '{kind}'"
        ),
    )
    .await
}

/// The owner generation the store of `key` holds for its namespace.
async fn generation(h: &Harness, rt: &erp_blob::AttachmentRuntime, key: &str) -> u64 {
    let ns = store_namespace(h, key).await;
    rt.store(key)
        .unwrap()
        .owner(&ns)
        .await
        .unwrap()
        .unwrap()
        .marker()
        .generation()
}

async fn store_namespace(h: &Harness, key: &str) -> Namespace {
    let ns = text_h(
        h,
        &format!("SELECT namespace::text FROM ir_blob_store WHERE store_key='{key}'"),
    )
    .await;
    Namespace::parse(&ns).unwrap()
}

/// The heartbeat job's stored progress, as JSON text with sorted keys.
async fn heartbeat_progress(h: &Harness) -> erp_sdk::serde_json::Value {
    let text = text_h(
        h,
        &format!("SELECT coalesce(progress::text, 'null') FROM erp_job WHERE kind = '{HEARTBEAT}'"),
    )
    .await;
    erp_sdk::serde_json::from_str(&text).unwrap()
}

/// A database with three local stores `a`, `b` and `c` (tempdirs kept alive by the first value).
async fn three_stores() -> (
    Vec<tempfile::TempDir>,
    Arc<erp_blob::AttachmentRuntime>,
    Harness,
) {
    use erp_blob::{AttachmentConfig, AttachmentRuntime};
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut config = String::from("[attachments]\nwrite_store='a'\n");
    for (key, dir) in ["a", "b", "c"].iter().zip(&dirs) {
        config.push_str(&format!(
            "[attachments.stores.{key}]\nkind='local'\npath='{}'\nshared_filesystem=true\n",
            dir.path().display()
        ));
    }
    let rt = Arc::new(
        AttachmentRuntime::open(AttachmentConfig::from_toml(&config).unwrap())
            .await
            .unwrap(),
    );
    let h = Harness::builder([&erp_test_models::base_users::ADDON, &erp_addon_base::ADDON])
        .attach(Arc::clone(&rt))
        .build()
        .await
        .unwrap();
    (dirs, rt, h)
}

/// write, then by the next preparation; separate-process nodes not covered)
#[tokio::test]
async fn heartbeat_beats_the_stores_after_a_refused_one() {
    use erp_sdk::serde_json::json;
    let (_dirs, rt, h) = three_stores().await;
    gc::ensure_schedules(h.orm()).await.unwrap();
    run_h(&h, HEARTBEAT, 1).await;
    assert_eq!(job_outcome(&h, HEARTBEAT).await, "ready");
    for key in ["a", "b", "c"] {
        assert_eq!(generation(&h, &rt, key).await, 1, "store {key} was beaten");
    }

    // Another database's contest marker appears in the middle store.
    let ns = store_namespace(&h, "b").await;
    let store_b = rt.store("b").unwrap();
    let (marker, _) = foreign_marker_in(&**store_b, &ns).await;
    store_b.create_contested(&ns, &marker).await.unwrap();

    // The store write finds it (settle refuses), then the next preparation finds the durable
    // contested mode (prepare refuses). Either way the stores after it are still beaten and the
    // run reschedules instead of failing.
    for beat in [2, 3] {
        run_h(&h, HEARTBEAT, 1).await;
        assert_eq!(job_outcome(&h, HEARTBEAT).await, "ready", "beat {beat}");
        assert_eq!(generation(&h, &rt, "a").await, beat);
        assert_eq!(
            generation(&h, &rt, "c").await,
            beat,
            "the store after the refused one"
        );
        assert_eq!(generation(&h, &rt, "b").await, 1);
        assert_eq!(
            heartbeat_progress(&h).await,
            json!({"after": null, "refused": {"b": ownership::Refusal::Contested.code()}}),
            "beat {beat}"
        );
        assert_eq!(
            scalar(
                &h,
                "SELECT count(*) FROM ir_blob_owner_state o JOIN ir_blob_store s ON s.id=o.store_id \
                 WHERE s.store_key IN ('a','c') AND o.mode='healthy' AND o.observation_id IS NOT NULL \
                 AND o.last_success_at > clock_timestamp() - interval '60 seconds'"
            )
            .await,
            2
        );
        assert_eq!(
            text_h(
                &h,
                "SELECT o.mode || coalesce(' ' || o.observation_id::text, '') FROM ir_blob_owner_state o \
                 JOIN ir_blob_store s ON s.id=o.store_id WHERE s.store_key='b'"
            )
            .await,
            "contested"
        );
    }
    assert_eq!(
        scalar(&h, "SELECT count(*) FROM erp_job WHERE state = 'dead'").await,
        0
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn heartbeat_reads_the_progress_of_earlier_versions() {
    let f = fixture(external_limits()).await;
    gc::ensure_schedules(f.h.orm()).await.unwrap();
    run(&f, HEARTBEAT, 1).await;
    assert_eq!(generation(&f.h, &f.rt, "main").await, 1);
    // An old run that stopped after its last confirmed key stored that key as a string: the next
    // run resumes after it, so no store is beaten, and ends the cycle in the current shape.
    exec(
        &f,
        &format!("UPDATE erp_job SET progress = to_jsonb('main'::text) WHERE kind = '{HEARTBEAT}'"),
    )
    .await;
    run(&f, HEARTBEAT, 1).await;
    assert_eq!(job_outcome(&f.h, HEARTBEAT).await, "ready");
    assert_eq!(generation(&f.h, &f.rt, "main").await, 1);
    assert_eq!(
        heartbeat_progress(&f.h).await,
        erp_sdk::serde_json::Value::Null,
        "a cycle without refusals ends with the progress an earlier binary reads"
    );
    // A completed old cycle stored JSON null: the next run is a full cycle.
    exec(
        &f,
        &format!("UPDATE erp_job SET progress = 'null'::jsonb WHERE kind = '{HEARTBEAT}'"),
    )
    .await;
    run(&f, HEARTBEAT, 1).await;
    assert_eq!(job_outcome(&f.h, HEARTBEAT).await, "ready");
    assert_eq!(generation(&f.h, &f.rt, "main").await, 2);
    assert_eq!(
        heartbeat_progress(&f.h).await,
        erp_sdk::serde_json::Value::Null,
        "a cycle without refusals ends with the progress an earlier binary reads"
    );
    // A later version may add keys: they are ignored, and the next run is a full cycle.
    exec(
        &f,
        &format!(
            "UPDATE erp_job SET progress = '{{\"after\": null, \"refused\": {{}}, \"later\": 1}}'::jsonb \
             WHERE kind = '{HEARTBEAT}'"
        ),
    )
    .await;
    run(&f, HEARTBEAT, 1).await;
    assert_eq!(job_outcome(&f.h, HEARTBEAT).await, "ready");
    assert_eq!(generation(&f.h, &f.rt, "main").await, 3);
    assert_eq!(
        heartbeat_progress(&f.h).await,
        erp_sdk::serde_json::Value::Null
    );
    no_dead_jobs(&f).await;
    f.h.cleanup().await.unwrap();
}

/// store stays refused is set back in the stored progress, never slept)
#[tokio::test]
async fn cleanup_fails_its_run_once_a_store_stays_refused_past_the_observation() {
    let (f, subject) = scheduled().await;
    let store_id = scalar(
        &f.h,
        "SELECT id::int8 FROM ir_blob_store WHERE store_key='main'",
    )
    .await;
    // The first cleaner run was refused while the window is open: the store is listed with the
    // reason and the database time of its first refusal.
    let reason = scalar_text(
        &f,
        &format!("SELECT progress->'refused'->'{store_id}'->>'reason' FROM erp_job WHERE kind = '{CLEAN}'"),
    )
    .await;
    let since_query = format!(
        "SELECT (progress->'refused'->'{store_id}'->>'since')::int8 FROM erp_job WHERE kind = '{CLEAN}'"
    );
    let since = scalar(&f.h, &since_query).await;
    assert_eq!(
        scalar(
            &f.h,
            &format!(
                "SELECT ({since} BETWEEN floor(extract(epoch FROM clock_timestamp()))::int8 - 60 \
                 AND floor(extract(epoch FROM clock_timestamp()))::int8)::int::int8"
            )
        )
        .await,
        1,
        "since is the database time of the first refusal"
    );
    // A later run in the same unbroken refusal keeps the first time.
    consumed(&f, "second.txt", b"de").await;
    run(&f, CLEAN, 1).await;
    assert_eq!(job_outcome(&f.h, CLEAN).await, "ready");
    assert_eq!(scalar(&f.h, &since_query).await, since);

    // The store has been refused for longer than the observation period.
    let age = i64::try_from(ownership::OBSERVATION_SECS).unwrap() + 1;
    let earlier = since - age;
    exec(
        &f,
        &format!(
            "UPDATE erp_job SET progress = jsonb_set(progress, '{{refused,{store_id},since}}', to_jsonb({earlier}::int8)) \
             WHERE kind = '{CLEAN}'"
        ),
    )
    .await;
    consumed(&f, "third.txt", b"fg").await;
    let before = reserved(&f, subject).await;
    run(&f, CLEAN, 1).await;
    // The admitted work committed first: the inline row is gone, the store's row and
    // reservation stay. Then the run failed for retry, naming the store and its reason.
    assert_eq!(
        scalar(
            &f.h,
            "SELECT count(*) FROM ir_blob_upload WHERE inline_data IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(f.store.staging_keys().len(), 1);
    assert_eq!(reserved(&f, subject).await, before);
    assert_eq!(
        job_outcome(&f.h, CLEAN).await,
        format!(
            "ready cleanup_refused Attachment cleanup has refused these stores for longer than {} \
             seconds; their rows and reservations are kept: store {store_id} ({reason})",
            ownership::OBSERVATION_SECS
        )
    );
    assert_eq!(scalar(&f.h, &since_query).await, earlier);
    no_dead_jobs(&f).await;

    // Admitted again: the store leaves the list and the run succeeds.
    window_elapsed(&f).await;
    run(&f, HEARTBEAT, 1).await;
    run(&f, CLEAN, 1).await;
    assert_eq!(job_outcome(&f.h, CLEAN).await, "ready");
    assert_eq!(
        scalar(
            &f.h,
            &format!("SELECT count(*) FROM erp_job, jsonb_object_keys(progress->'refused') WHERE kind = '{CLEAN}'")
        )
        .await,
        0
    );
    assert!(f.store.staging_keys().is_empty());
    no_dead_jobs(&f).await;
    f.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn heartbeat_beats_the_stores_after_one_under_recovery() {
    use erp_sdk::serde_json::json;
    let (_dirs, rt, h) = three_stores().await;
    gc::ensure_schedules(h.orm()).await.unwrap();
    run_h(&h, HEARTBEAT, 1).await;
    assert_eq!(job_outcome(&h, HEARTBEAT).await, "ready");
    // An operator recovery of `b` is decided and not completed.
    exec_h(
        &h,
        "INSERT INTO ir_blob_owner_claim (operation, store_id, namespace, fingerprint, chosen, \
         previous_coordinator, reason) SELECT gen_random_uuid(), o.store_id, o.namespace, \
         o.fingerprint, o.confirmed_marker, '{}'::jsonb, 'restore_primary' \
         FROM ir_blob_owner_state o JOIN ir_blob_store s ON s.id = o.store_id WHERE s.store_key = 'b'; \
         INSERT INTO ir_blob_owner_recovery (claim_id, store_id) SELECT id, store_id FROM ir_blob_owner_claim",
    )
    .await;
    run_h(&h, HEARTBEAT, 1).await;
    assert_eq!(job_outcome(&h, HEARTBEAT).await, "ready");
    assert_eq!(generation(&h, &rt, "a").await, 2);
    assert_eq!(generation(&h, &rt, "b").await, 1);
    assert_eq!(
        generation(&h, &rt, "c").await,
        2,
        "the store after the refused one"
    );
    assert_eq!(
        heartbeat_progress(&h).await,
        json!({"after": null, "refused": {"b": ownership::Refusal::RecoveryActive.code()}})
    );
    assert_eq!(
        scalar(&h, "SELECT count(*) FROM erp_job WHERE state = 'dead'").await,
        0
    );
    h.cleanup().await.unwrap();
}
