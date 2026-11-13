// SPDX-License-Identifier: MIT OR Apache-2.0
//! The RPC attempt hook (AI1.RPC-PRECONDITION, and D7): a per-call, server-side
//! hook with a `before_body` and an `after_body` phase inside each attempt's transaction, after a
//! new idempotency claim. Covers the pre-transaction refusals, the phase order, replays, typed
//! refusals and their rollback, retries, the `assistant` audit origin and an audited elevation
//! written by the hook. The confirm route and `ai.proposal` are later slices (AI1.TOOLS-WRITE).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Mutex;
use std::sync::atomic::Ordering;

use erp_orm::BoxFuture;
use erp_orm::audit::AuditOrigin;
use erp_rpc::{
    AttemptHook, CONCEALED_ACCESS_MESSAGE, HookCall, HookError, HookOrigin, HookRefusal,
    IdempotencyKey, RefusalKind, RpcCall, RpcConfig, RpcFailure, RpcResponse,
};
use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_test_models::test_rpc::{ATTEMPTS, Effects};
use erp_testkit::RpcHarness;
use support::rpc::*;

erp_sdk::addon!(name = "test_rpc_hook", depends = ["test_rpc"]);

/// The addon named by the hook's audited elevation.
const HOOK_ADDON: &str = "test_rpc_hook";
/// The record the elevated hook writes.
const HOOK_NOTE: &str = "written by the hook";
const EFFECTS: &str = "test_rpc_effects";
const NOTES: &str = "test_rpc_hook_job_note";
/// The message of the hook's own validation error.
const HOOK_INVALID: &str = "the hook found the proposal data invalid";

/// A record whose create method claims another audit origin for its attempt.
#[model(name = "test_rpc_hook.job_note", addon = "test_rpc_hook")]
pub struct JobNote {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(JobNote, addon = "test_rpc_hook")]
impl JobNote {
    /// `write`: sets the attempt's origin to `job`, then creates a note named `name`.
    #[method]
    #[rpc(effect = write, model)]
    async fn bump_as_job(
        this: &RecordSet<JobNote>,
        tx: &mut Transaction,
        name: String,
    ) -> Result<RecordSet<JobNote>> {
        tx.audit_origin(AuditOrigin::Job)?;
        this.create(tx, vec![this.vals().set(JobNote::NAME, name.as_str())])
            .await
    }

    /// `write`: creates a note named `name`, then fails with the access error whose text a
    /// concealed hook refusal uses: the response bodies match, only the hook refusal differs.
    #[method]
    #[rpc(effect = write, model)]
    async fn deny_as_concealed(
        this: &RecordSet<JobNote>,
        tx: &mut Transaction,
        name: String,
    ) -> Result<bool> {
        this.create(tx, vec![this.vals().set(JobNote::NAME, name.as_str())])
            .await?;
        Err(Error::access(CONCEALED_ACCESS_MESSAGE))
    }
}

/// What one hook phase does after it recorded what it saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Pass,
    Refuse(RefusalKind),
    /// A serialization failure of the hook's own work, on the first attempt only.
    ConflictOnFirst,
    /// A validation error of the hook's own work: not retryable, the call's error.
    InvalidData,
}

/// One `before_body` observation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Before {
    attempt: u32,
    /// `<model>.<method>:<effect>:<key>` of the [`HookCall`].
    call: String,
    /// Idempotency rows claimed and not stored yet (the attempt's own claim).
    open_claims: i64,
    /// Rows named after the body's record.
    body_rows: i64,
}

/// One `after_body` observation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct After {
    attempt: u32,
    body_rows: i64,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct Seen {
    before: Vec<Before>,
    after: Vec<After>,
}

/// A test hook: records what each phase saw (test state, not hook state), then does its step.
#[derive(Debug)]
struct Probe {
    before: Step,
    after: Step,
    /// Table and record name the phases count.
    table: &'static str,
    name: &'static str,
    /// `before_body` writes [`HOOK_NOTE`] through the audited unscoped elevation.
    elevate: bool,
    seen: Mutex<Seen>,
}

impl Probe {
    fn new(before: Step, after: Step, table: &'static str, name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            before,
            after,
            table,
            name,
            elevate: false,
            seen: Mutex::new(Seen::default()),
        })
    }

    fn elevating(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            before: Step::Pass,
            after: Step::Pass,
            table: EFFECTS,
            name,
            elevate: true,
            seen: Mutex::new(Seen::default()),
        })
    }

    fn before_seen(&self) -> Vec<Before> {
        self.seen.lock().unwrap().before.clone()
    }

    fn after_seen(&self) -> Vec<After> {
        self.seen.lock().unwrap().after.clone()
    }

    async fn body_rows(&self, tx: &mut Transaction) -> i64 {
        let sql = format!("SELECT count(*) FROM {} WHERE name = $1", self.table);
        tx.query(&sql, &[&self.name]).await.unwrap()[0].get(0)
    }
}

fn refusal(kind: RefusalKind) -> HookRefusal {
    match kind {
        RefusalKind::Concealed => HookRefusal::new(
            "erp.ai.proposal_mismatch",
            "the proposal belongs to another session",
            kind,
        ),
        RefusalKind::Refused => {
            HookRefusal::new("erp.ai.proposal_expired", "The proposal expired.", kind)
        }
    }
    .unwrap()
}

fn step(step: Step, call: &HookCall<'_>) -> std::result::Result<(), HookError> {
    match step {
        Step::Pass => Ok(()),
        Step::Refuse(kind) => Err(HookError::Refused(refusal(kind))),
        Step::ConflictOnFirst if call.attempt == 1 => Err(Error::new(
            ErrorKind::Concurrency,
            "could not serialize access due to concurrent update",
        )
        .with_sqlstate(Some("40001".to_owned()))
        .into()),
        Step::ConflictOnFirst => Ok(()),
        Step::InvalidData => Err(Error::validation(HOOK_INVALID).into()),
    }
}

impl AttemptHook for Probe {
    fn origin(&self) -> HookOrigin {
        HookOrigin::Assistant
    }

    fn before_body<'a>(
        &'a self,
        tx: &'a mut Transaction,
        call: &'a HookCall<'a>,
    ) -> BoxFuture<'a, std::result::Result<(), HookError>> {
        Box::pin(async move {
            let open_claims: i64 = tx
                .query(
                    "SELECT count(*) FROM ir_idempotency_key WHERE status IS NULL",
                    &[],
                )
                .await
                .unwrap()[0]
                .get(0);
            let body_rows = self.body_rows(tx).await;
            let observed = Before {
                attempt: call.attempt,
                call: format!(
                    "{}.{}:{}:{}",
                    call.model,
                    call.method,
                    call.effect,
                    call.idempotency_key.as_str()
                ),
                open_claims,
                body_rows,
            };
            self.seen.lock().unwrap().before.push(observed);
            if self.elevate {
                let env = tx.env();
                let elevated = tx.audited_unscoped_sudo(&env, HOOK_ADDON)?;
                let effects = elevated.model::<Effects>()?;
                effects
                    .create(tx, vec![effects.vals().set(Effects::NAME, HOOK_NOTE)])
                    .await?;
            }
            step(self.before, call)
        })
    }

    fn after_body<'a>(
        &'a self,
        tx: &'a mut Transaction,
        call: &'a HookCall<'a>,
        body: &'a [u8],
    ) -> BoxFuture<'a, std::result::Result<(), HookError>> {
        Box::pin(async move {
            let body_rows = self.body_rows(tx).await;
            let observed = After {
                attempt: call.attempt,
                body_rows,
                bytes: body.to_vec(),
            };
            self.seen.lock().unwrap().after.push(observed);
            step(self.after, call)
        })
    }
}

async fn hook_harness() -> RpcHarness {
    harness_with(&[&ADDON], RpcConfig::default(), false).await
}

/// A call by Alice with `hook` attached, keyed when `key` is given.
async fn hooked(
    h: &RpcHarness,
    path: &str,
    body: &str,
    key: Option<&IdempotencyKey>,
    hook: &Arc<Probe>,
) -> RpcResponse {
    let hook: Arc<dyn AttemptHook> = Arc::clone(hook) as Arc<dyn AttemptHook>;
    let mut call = RpcCall::json2(path, body.as_bytes().to_vec(), alice()).attempt_hook(hook);
    if let Some(key) = key {
        call = call.idempotency_key(key.clone());
    }
    h.service().call(call).await
}

/// One audit row as the columns these tests compare.
#[derive(Debug)]
struct AuditRow {
    kind: String,
    origin: String,
    outcome: String,
    error_class: Option<String>,
    method: Option<String>,
    events: serde_json::Value,
}

async fn audit_rows(h: &RpcHarness) -> Vec<AuditRow> {
    let mut tx = h.harness().begin().await.unwrap();
    let found = tx
        .query(
            "SELECT entry_kind, origin, outcome, error_class, method, events::text \
             FROM erp_event.ir_audit_entry ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    found
        .iter()
        .map(|r| AuditRow {
            kind: r.get(0),
            origin: r.get(1),
            outcome: r.get(2),
            error_class: r.get(3),
            method: r.get(4),
            events: serde_json::from_str(&r.get::<_, String>(5)).unwrap(),
        })
        .collect()
}

async fn names(h: &RpcHarness, table: &str) -> Vec<Option<String>> {
    sql(h, &format!("SELECT name FROM {table} ORDER BY name")).await
}

fn hook_call(entry: &str, effect: &str, key: &IdempotencyKey) -> String {
    format!("{entry}:{effect}:{}", key.as_str())
}

/// A hook on a read method (keyed or not) and on an unkeyed write call is refused before any
/// transaction: a programming error, no attempt, the hook never runs, nothing is written.
#[tokio::test]
async fn a_hook_needs_a_keyed_write_call() {
    let h = hook_harness().await;
    let hook = Probe::new(Step::Pass, Step::Pass, EFFECTS, "unkeyed");
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac01");
    let cases = [
        (
            "/json/2/test_rpc.effects/probe",
            "{}",
            Some(&k),
            "test_rpc.effects.probe",
            "read",
            "has a key",
        ),
        (
            "/json/2/test_rpc.effects/probe",
            "{}",
            None,
            "test_rpc.effects.probe",
            "read",
            "has no key",
        ),
        (
            "/json/2/test_rpc.effects/bump",
            r#"{"name": "unkeyed"}"#,
            None,
            "test_rpc.effects.bump",
            "write",
            "has no key",
        ),
    ];
    for (path, body, key, entry, effect, keyed) in cases {
        let r = hooked(&h, path, body, key, &hook).await;
        assert_error(
            &r,
            500,
            "erp.programming",
            &format!(
                "An attempt hook needs a call with a write effect and an Idempotency-Key; \
                 {entry} has effect '{effect}' and the call {keyed}."
            ),
        );
        assert_eq!(r.attempts(), 0, "{path} {keyed}: no transaction");
    }
    assert_eq!(hook.before_seen(), Vec::new(), "before_body never ran");
    assert_eq!(hook.after_seen(), Vec::new(), "after_body never ran");
    assert_eq!(count(&h, EFFECTS).await, 0);
    assert_eq!(count(&h, "ir_idempotency_key").await, 0);
    assert!(audit_rows(&h).await.is_empty(), "no attempt, no audit row");
    h.cleanup().await.unwrap();
}

/// The order inside one attempt is claim, `before_body`, method body, `after_body`, and
/// `after_body` receives exactly the bytes returned to the client.
#[tokio::test]
async fn the_hook_runs_after_the_claim_around_the_body() {
    let h = hook_harness().await;
    let hook = Probe::new(Step::Pass, Step::Pass, EFFECTS, "ordered");
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac02");
    let r = hooked(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "ordered"}"#,
        Some(&k),
        &hook,
    )
    .await;
    ok(&r);
    assert!(!r.replayed());
    assert_eq!(r.attempts(), 1);
    assert_eq!(r.hook_refusal(), None, "a success carries no refusal");
    assert_eq!(
        hook.before_seen(),
        vec![Before {
            attempt: 1,
            call: hook_call("test_rpc.effects.bump", "write", &k),
            open_claims: 1,
            body_rows: 0,
        }],
        "the claim is open and the body has not run"
    );
    assert_eq!(
        hook.after_seen(),
        vec![After {
            attempt: 1,
            body_rows: 1,
            bytes: r.body().to_vec(),
        }],
        "the body ran and after_body saw the response bytes"
    );
    assert_eq!(names(&h, EFFECTS).await, vec![Some("ordered".to_owned())]);
    h.cleanup().await.unwrap();
}

/// A replay with the same key answers from the store and does not run the hook again.
#[tokio::test]
async fn a_replay_does_not_run_the_hook() {
    let h = hook_harness().await;
    let hook = Probe::new(Step::Pass, Step::Pass, EFFECTS, "once");
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac03");
    let body = r#"{"name": "once"}"#;
    let first = hooked(&h, "/json/2/test_rpc.effects/bump", body, Some(&k), &hook).await;
    ok(&first);
    let replay = hooked(&h, "/json/2/test_rpc.effects/bump", body, Some(&k), &hook).await;
    assert!(replay.replayed());
    assert_eq!(replay.body(), first.body());
    assert_eq!(replay.hook_refusal(), None);
    assert_eq!(hook.before_seen().len(), 1, "before_body ran once");
    assert_eq!(hook.after_seen().len(), 1, "after_body ran once");
    assert_eq!(count(&h, EFFECTS).await, 1);
    // the replay row carries the request channel's origin, although the hook did not run
    let rows = audit_rows(&h).await;
    let seen: Vec<(&str, &str, &str)> = rows
        .iter()
        .map(|r| (r.kind.as_str(), r.outcome.as_str(), r.origin.as_str()))
        .collect();
    assert_eq!(
        seen,
        vec![
            ("change", "committed", AuditOrigin::Assistant.as_str()),
            ("attempt", "replayed", AuditOrigin::Assistant.as_str()),
        ]
    );
    h.cleanup().await.unwrap();
}

/// A concealed `before_body` refusal renders exactly as the ordinary access error, rolls back
/// the claim, runs one attempt and leaves one `denied` attempt row with origin `assistant`; the
/// same key then claims anew and runs.
#[tokio::test]
async fn a_concealed_before_body_refusal_rolls_back_the_attempt() {
    let h = hook_harness().await;
    let hook = Probe::new(
        Step::Refuse(RefusalKind::Concealed),
        Step::Pass,
        EFFECTS,
        "concealed",
    );
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac04");
    let body = r#"{"name": "concealed"}"#;
    let r = hooked(&h, "/json/2/test_rpc.effects/bump", body, Some(&k), &hook).await;
    let ordinary = RpcFailure::from(Error::access(CONCEALED_ACCESS_MESSAGE));
    assert_error(&r, ordinary.status(), ordinary.name(), &ordinary.message());
    let hidden = refusal(RefusalKind::Concealed);
    assert_eq!(
        r.hook_refusal().map(|got| (got.code(), got.kind())),
        Some((hidden.code(), RefusalKind::Concealed)),
        "the server-side caller sees the typed refusal"
    );
    let text = String::from_utf8_lossy(r.body()).into_owned();
    assert!(
        !text.contains(hidden.code()) && !text.contains(hidden.message()),
        "nothing of the refusal leaks: {text}"
    );
    assert_eq!(r.attempts(), 1, "a refusal is never retried");
    assert_eq!(hook.before_seen().len(), 1);
    assert_eq!(hook.after_seen(), Vec::new(), "the body never ran");
    assert_eq!(count(&h, EFFECTS).await, 0);
    assert_eq!(
        count(&h, "ir_idempotency_key").await,
        0,
        "claim rolled back"
    );
    let rows = audit_rows(&h).await;
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(
        (
            rows[0].kind.as_str(),
            rows[0].origin.as_str(),
            rows[0].outcome.as_str(),
            rows[0].error_class.as_deref(),
        ),
        (
            "attempt",
            AuditOrigin::Assistant.as_str(),
            "denied",
            Some("access")
        )
    );
    // the same key claims anew and runs
    let pass = Probe::new(Step::Pass, Step::Pass, EFFECTS, "concealed");
    let again = hooked(&h, "/json/2/test_rpc.effects/bump", body, Some(&k), &pass).await;
    ok(&again);
    assert!(!again.replayed());
    assert_eq!(pass.before_seen().len(), 1);
    assert_eq!(count(&h, EFFECTS).await, 1);
    h.cleanup().await.unwrap();
}

/// An `after_body` refusal rolls back the body's change and the claim, answers 409 with the
/// refusal's code and message, runs one attempt and leaves one `failed` attempt row with origin
/// `assistant`, also when the method body set another origin; the same key then claims anew.
#[tokio::test]
async fn an_after_body_refusal_rolls_back_the_attempt() {
    let h = hook_harness().await;
    let hook = Probe::new(
        Step::Pass,
        Step::Refuse(RefusalKind::Refused),
        EFFECTS,
        "refused",
    );
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac05");
    let body = r#"{"name": "refused"}"#;
    let r = hooked(&h, "/json/2/test_rpc.effects/bump", body, Some(&k), &hook).await;
    let expected = refusal(RefusalKind::Refused);
    assert_error(&r, 409, expected.code(), expected.message());
    assert_eq!(
        r.hook_refusal().map(|got| (got.code(), got.kind())),
        Some((expected.code(), RefusalKind::Refused))
    );
    assert_eq!(r.attempts(), 1, "a refusal is never retried");
    assert_eq!(hook.before_seen().len(), 1);
    assert_eq!(hook.after_seen().len(), 1);
    assert_eq!(hook.after_seen()[0].body_rows, 1, "the body had run");
    assert_eq!(count(&h, EFFECTS).await, 0, "the body's change rolled back");
    assert_eq!(
        count(&h, "ir_idempotency_key").await,
        0,
        "claim rolled back"
    );

    // the body claimed the `job` origin: the failed attempt still says `assistant`
    let job = Probe::new(
        Step::Pass,
        Step::Refuse(RefusalKind::Refused),
        NOTES,
        "job refused",
    );
    let kj = key("018f3a1c-7b2d-7c4e-9a10-00000000ac15");
    let r = hooked(
        &h,
        "/json/2/test_rpc_hook.job_note/bump_as_job",
        r#"{"name": "job refused"}"#,
        Some(&kj),
        &job,
    )
    .await;
    assert_error(&r, 409, expected.code(), expected.message());
    assert_eq!(count(&h, NOTES).await, 0);

    let rows = audit_rows(&h).await;
    assert_eq!(rows.len(), 2, "{rows:#?}");
    for (row, method) in rows.iter().zip(["bump", "bump_as_job"]) {
        assert_eq!(
            (
                row.kind.as_str(),
                row.origin.as_str(),
                row.outcome.as_str(),
                row.error_class.as_deref(),
                row.method.as_deref(),
            ),
            (
                "attempt",
                AuditOrigin::Assistant.as_str(),
                "failed",
                Some("validation"),
                Some(method),
            )
        );
    }
    // the same key claims anew and runs
    let pass = Probe::new(Step::Pass, Step::Pass, EFFECTS, "refused");
    let again = hooked(&h, "/json/2/test_rpc.effects/bump", body, Some(&k), &pass).await;
    ok(&again);
    assert!(!again.replayed());
    assert_eq!(pass.before_seen().len(), 1);
    assert_eq!(count(&h, EFFECTS).await, 1);
    h.cleanup().await.unwrap();
}

/// A serialization failure of the method body retries the whole attempt: `before_body` runs
/// on both attempts, the change is committed once.
#[tokio::test]
async fn a_retried_attempt_runs_the_hook_again() {
    let h = hook_harness().await;
    ATTEMPTS.store(0, Ordering::SeqCst);
    let hook = Probe::new(Step::Pass, Step::Pass, EFFECTS, "retried");
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac06");
    let r = hooked(
        &h,
        "/json/2/test_rpc.effects/conflict_once",
        r#"{"name": "retried"}"#,
        Some(&k),
        &hook,
    )
    .await;
    ok(&r);
    assert_eq!(r.attempts(), 2);
    let attempts: Vec<u32> = hook.before_seen().iter().map(|b| b.attempt).collect();
    assert_eq!(attempts, vec![1, 2], "before_body on every attempt");
    assert!(
        hook.before_seen()
            .iter()
            .all(|b| b.open_claims == 1 && b.body_rows == 0),
        "each attempt claims anew; the first attempt's row rolled back"
    );
    let after: Vec<u32> = hook.after_seen().iter().map(|a| a.attempt).collect();
    assert_eq!(
        after,
        vec![2],
        "after_body only on the attempt that got there"
    );
    assert_eq!(names(&h, EFFECTS).await, vec![Some("retried".to_owned())]);
    h.cleanup().await.unwrap();
}

/// A retryable database error raised by the hook's own work retries the attempt.
#[tokio::test]
async fn a_retryable_error_from_the_hook_retries_the_attempt() {
    let h = hook_harness().await;
    let hook = Probe::new(Step::ConflictOnFirst, Step::Pass, EFFECTS, "hook retry");
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac07");
    let r = hooked(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "hook retry"}"#,
        Some(&k),
        &hook,
    )
    .await;
    ok(&r);
    assert_eq!(r.attempts(), 2);
    let attempts: Vec<u32> = hook.before_seen().iter().map(|b| b.attempt).collect();
    assert_eq!(attempts, vec![1, 2]);
    let after: Vec<u32> = hook.after_seen().iter().map(|a| a.attempt).collect();
    assert_eq!(after, vec![2]);
    assert_eq!(
        names(&h, EFFECTS).await,
        vec![Some("hook retry".to_owned())]
    );
    h.cleanup().await.unwrap();
}

/// The committed change row of a hooked call has origin `assistant` even when the method body set
/// another origin; the same body without a hook keeps its own, a plain write says `rpc`.
#[tokio::test]
async fn the_committed_row_says_assistant() {
    let h = hook_harness().await;
    let hook = Probe::new(Step::Pass, Step::Pass, NOTES, "hooked job");
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac08");
    let r = hooked(
        &h,
        "/json/2/test_rpc_hook.job_note/bump_as_job",
        r#"{"name": "hooked job"}"#,
        Some(&k),
        &hook,
    )
    .await;
    ok(&r);
    let plain_job = call(
        &h,
        "/json/2/test_rpc_hook.job_note/bump_as_job",
        r#"{"name": "plain job"}"#,
        alice(),
    )
    .await;
    ok(&plain_job);
    let plain = call(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "plain"}"#,
        alice(),
    )
    .await;
    ok(&plain);
    let rows = audit_rows(&h).await;
    let seen: Vec<(&str, &str, &str, Option<&str>)> = rows
        .iter()
        .map(|r| {
            (
                r.kind.as_str(),
                r.outcome.as_str(),
                r.origin.as_str(),
                r.method.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            (
                "change",
                "committed",
                AuditOrigin::Assistant.as_str(),
                Some("bump_as_job")
            ),
            (
                "change",
                "committed",
                AuditOrigin::Job.as_str(),
                Some("bump_as_job")
            ),
            (
                "change",
                "committed",
                AuditOrigin::Rpc.as_str(),
                Some("bump")
            ),
        ]
    );
    h.cleanup().await.unwrap();
}

/// A hook write through the audited unscoped elevation commits together with the body, and the
/// elevation is in the committed row; when the hook then refuses, both writes roll back.
#[tokio::test]
async fn an_elevated_hook_write_commits_with_the_body() {
    let h = hook_harness().await;
    let hook = Probe::elevating("body");
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac09");
    let r = hooked(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "body"}"#,
        Some(&k),
        &hook,
    )
    .await;
    ok(&r);
    assert_eq!(
        names(&h, EFFECTS).await,
        vec![Some("body".to_owned()), Some(HOOK_NOTE.to_owned())]
    );
    let rows = audit_rows(&h).await;
    assert_eq!(rows.len(), 1, "{rows:#?}");
    let row = &rows[0];
    assert_eq!(
        (row.kind.as_str(), row.origin.as_str(), row.outcome.as_str()),
        ("change", AuditOrigin::Assistant.as_str(), "committed")
    );
    let events = row.events.as_array().unwrap();
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(events[0]["reason"], "core.sudo.unscoped");
    assert_eq!(events[0]["grant"]["cause"]["kind"], "unscoped_sudo");
    assert_eq!(events[0]["grant"]["cause"]["addon"], HOOK_ADDON);
    assert_eq!(events[0]["original"]["subject"], ALICE);

    // the same elevated write followed by a refusal leaves nothing
    let refusing = Arc::new(Probe {
        before: Step::Pass,
        after: Step::Refuse(RefusalKind::Refused),
        table: EFFECTS,
        name: "refused body",
        elevate: true,
        seen: Mutex::new(Seen::default()),
    });
    let k2 = key("018f3a1c-7b2d-7c4e-9a10-00000000ac19");
    let r = hooked(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "refused body"}"#,
        Some(&k2),
        &refusing,
    )
    .await;
    let expected = refusal(RefusalKind::Refused);
    assert_error(&r, 409, expected.code(), expected.message());
    assert_eq!(
        names(&h, EFFECTS).await,
        vec![Some("body".to_owned()), Some(HOOK_NOTE.to_owned())],
        "neither the hook's nor the body's row stayed"
    );
    h.cleanup().await.unwrap();
}

/// An access error raised by the method body of a hooked call renders like a concealed hook
/// refusal, but carries no hook refusal for the server-side caller: a confirm route can tell the
/// two apart. The body's change rolls back and the attempt row is `denied`.
#[tokio::test]
async fn a_body_access_error_is_not_a_hook_refusal() {
    let h = hook_harness().await;
    let hook = Probe::new(Step::Pass, Step::Pass, NOTES, "denied by body");
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac0a");
    let r = hooked(
        &h,
        "/json/2/test_rpc_hook.job_note/deny_as_concealed",
        r#"{"name": "denied by body"}"#,
        Some(&k),
        &hook,
    )
    .await;
    let ordinary = RpcFailure::from(Error::access(CONCEALED_ACCESS_MESSAGE));
    assert_error(&r, ordinary.status(), ordinary.name(), &ordinary.message());
    assert_eq!(
        r.hook_refusal(),
        None,
        "a body error is never a hook refusal"
    );
    assert_eq!(r.attempts(), 1);
    assert_eq!(hook.before_seen().len(), 1);
    assert_eq!(hook.after_seen(), Vec::new(), "after_body never ran");
    assert_eq!(count(&h, NOTES).await, 0);
    let rows = audit_rows(&h).await;
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(
        (
            rows[0].kind.as_str(),
            rows[0].origin.as_str(),
            rows[0].outcome.as_str(),
            rows[0].error_class.as_deref(),
        ),
        (
            "attempt",
            AuditOrigin::Assistant.as_str(),
            "denied",
            Some("access")
        )
    );
    h.cleanup().await.unwrap();
}

/// A non-retryable database error of the hook's own work is the call's domain error: rendered as
/// that error, one attempt, no hook refusal, a `failed` attempt row, nothing committed.
#[tokio::test]
async fn a_non_retryable_hook_error_is_the_calls_domain_error() {
    let h = hook_harness().await;
    let hook = Probe::new(Step::InvalidData, Step::Pass, EFFECTS, "invalid");
    let k = key("018f3a1c-7b2d-7c4e-9a10-00000000ac0b");
    let r = hooked(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "invalid"}"#,
        Some(&k),
        &hook,
    )
    .await;
    let expected = RpcFailure::from(Error::validation(HOOK_INVALID));
    assert_error(&r, expected.status(), expected.name(), &expected.message());
    assert_eq!(r.hook_refusal(), None);
    assert_eq!(r.attempts(), 1, "not retried");
    assert_eq!(hook.before_seen().len(), 1);
    assert_eq!(hook.after_seen(), Vec::new());
    assert_eq!(count(&h, EFFECTS).await, 0);
    assert_eq!(count(&h, "ir_idempotency_key").await, 0);
    let rows = audit_rows(&h).await;
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(
        (
            rows[0].kind.as_str(),
            rows[0].origin.as_str(),
            rows[0].outcome.as_str(),
            rows[0].error_class.as_deref(),
        ),
        (
            "attempt",
            AuditOrigin::Assistant.as_str(),
            "failed",
            Some("validation")
        )
    );
    h.cleanup().await.unwrap();
}
