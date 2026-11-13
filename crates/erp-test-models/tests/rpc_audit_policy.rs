// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod support;
use erp_fields::Error;
use erp_orm::Env;
use erp_rpc::{ReadAuditPolicy, RpcConfig};
use erp_sdk::prelude::*;
use erp_testkit::FieldPolicy;
use std::sync::atomic::{AtomicUsize, Ordering};
use support::rpc::*;

erp_sdk::addon!(name = "audit_policy", depends = ["test_rpc"]);
static PROBES: AtomicUsize = AtomicUsize::new(0);
#[model(
    name = "audit_policy.flagged",
    addon = "audit_policy",
    audit_reads = true
)]
pub struct Flagged {
    #[field(char)]
    pub name: Option<Arc<str>>,
}
#[methods(Flagged, addon = "audit_policy")]
impl Flagged {
    #[method]
    #[rpc(effect = read, model)]
    async fn probe(_this: &RecordSet<Flagged>, tx: &mut Transaction) -> Result<String> {
        PROBES.fetch_add(1, Ordering::SeqCst);
        let row = tx
            .query("SELECT current_setting('transaction_read_only')", &[])
            .await?;
        Ok(row[0].get(0))
    }
}
#[model(name = "audit_policy.elevated", addon = "audit_policy")]
pub struct Elevated {}
#[methods(Elevated, addon = "audit_policy")]
impl Elevated {
    #[method]
    #[rpc(effect = read, model)]
    async fn probe(_this: &RecordSet<Elevated>, tx: &mut Transaction) -> Result<bool> {
        let env = tx.env();
        let elevated = tx.audited_unscoped_sudo(&env, "audit_policy")?;
        Ok(elevated.principal().elevation().is_full())
    }

    /// A read call that opens one scoped block and reports what its body saw.
    #[method]
    #[rpc(effect = read, model)]
    async fn scoped(_this: &RecordSet<Elevated>, tx: &mut Transaction) -> Result<String> {
        let env = tx.env();
        scoped_block(tx, &env, SCOPED_REASON).await
    }

    /// A read call that opens two blocks one after the other.
    #[method]
    #[rpc(effect = read, model)]
    async fn scoped_twice(_this: &RecordSet<Elevated>, tx: &mut Transaction) -> Result<String> {
        let env = tx.env();
        let first = scoped_block(tx, &env, SCOPED_REASON).await?;
        let second = scoped_block(tx, &env, "audit_policy.test.scoped_again").await?;
        Ok(format!("{first}|{second}"))
    }
}
const SCOPED_REASON: &str = "audit_policy.test.scoped";

/// One scoped read block; the body reports `transaction_read_only` and whether its environment is
/// scoped (it runs no record operation: see the partial tag of the RPC tests below).
async fn scoped_block(tx: &mut Transaction, env: &Env, reason: &'static str) -> Result<String> {
    let model = tx
        .registry()
        .model_id("audit_policy.flagged")
        .ok_or_else(|| Error::programming("audit_policy.flagged is missing"))?;
    let scope = erp_orm::ElevationScope::new([erp_orm::ScopeItem::new(
        model,
        erp_orm::OperationSet::of([erp_orm::Operation::Read]),
    )?])?;
    let reason = erp_orm::audit::ElevationReason::new(reason)?;
    tx.elevated(env, reason, scope, async |tx, env| {
        let scoped = matches!(
            env.principal().elevation(),
            erp_orm::Elevation::Scoped { .. }
        );
        let state = tx
            .query("SELECT current_setting('transaction_read_only')", &[])
            .await?;
        Ok(format!("{}:{scoped}", state[0].get::<_, String>(0)))
    })
    .await
}
fn config(policy: ReadAuditPolicy) -> RpcConfig {
    RpcConfig::builder()
        .read_audit_policy(policy)
        .build()
        .unwrap()
}
async fn attempts(h: &erp_testkit::RpcHarness) -> i64 {
    sql_one(h, "SELECT count(*)::text FROM erp_event.ir_audit_entry WHERE entry_kind='attempt' AND outcome='read_ok'").await.parse().unwrap()
}

#[tokio::test]
async fn selected_read_is_read_only_and_withheld_if_attempt_insert_fails() {
    let h = harness_with(&[&ADDON], RpcConfig::default(), false).await;
    let plain = call(&h, "/json/2/test_rpc.effects/probe", "{}", alice()).await;
    assert_eq!(ok(&plain)["read_only"], true);
    assert_eq!(attempts(&h).await, 0);
    let flagged = call(
        &h,
        "/json/2/audit_policy.flagged/probe",
        r#"{"context":{"audit_reads":false}}"#,
        alice(),
    )
    .await;
    assert_eq!(ok(&flagged), "on");
    assert_eq!(attempts(&h).await, 1);
    sql_batch(&h, "ALTER TABLE erp_event.ir_audit_entry ADD CONSTRAINT audit_policy_refuse CHECK (outcome <> 'read_ok') NOT VALID").await;
    let before = PROBES.load(Ordering::SeqCst);
    let withheld = call(&h, "/json/2/audit_policy.flagged/probe", "{}", alice()).await;
    assert_error(
        &withheld,
        503,
        "erp.audit_unavailable",
        "The audit trail could not record this call: a call that changes data changed nothing, and a result selected for audit is withheld. Retry later or tell an administrator.",
    );
    assert!(
        !withheld.body().windows(4).any(|part| part == b"\"on\""),
        "the successful read result must be withheld"
    );
    assert_eq!(PROBES.load(Ordering::SeqCst), before + 1);
    assert_eq!(attempts(&h).await, 1);
    sql_batch(
        &h,
        "ALTER TABLE erp_event.ir_audit_entry DROP CONSTRAINT audit_policy_refuse",
    )
    .await;
    ok(&call(&h, "/json/2/audit_policy.flagged/probe", "{}", alice()).await);
    assert_eq!(attempts(&h).await, 2);
    // The audited unscoped sudo is admitted in a read call (A2): the service owns the
    // transaction, so the elevation leaves one more `read_ok` row.
    let admitted = call(&h, "/json/2/audit_policy.elevated/probe", "{}", alice()).await;
    assert_eq!(ok(&admitted), true);
    assert_eq!(attempts(&h).await, 3);
    // the new row is a `read_ok` attempt of the elevated probe and carries the reason
    let (outcome, events) = {
        let outcome = sql_one(&h, "SELECT outcome FROM erp_event.ir_audit_entry WHERE model='audit_policy.elevated' AND method='probe' ORDER BY id DESC LIMIT 1").await;
        let events = sql_one(&h, "SELECT events::text FROM erp_event.ir_audit_entry WHERE model='audit_policy.elevated' AND method='probe' ORDER BY id DESC LIMIT 1").await;
        (outcome, events)
    };
    assert_eq!(outcome, "read_ok");
    assert!(events.contains("core.sudo.unscoped"), "{events}");
    assert_eq!(sql_one(&h, "SELECT count(*)::text FROM erp_event.ir_audit_entry WHERE model='audit_policy.elevated' AND outcome='failed'").await, "0");
    h.cleanup().await.unwrap();
    let h = harness_with(&[&ADDON], config(ReadAuditPolicy::all_users()), false).await;
    ok(&call(&h, "/json/2/test_rpc.effects/probe", "{}", alice()).await);
    ok(&call(
        &h,
        "/json/2/audit_policy.flagged/search",
        r#"{"domain":[],"context":{"audit_reads":false}}"#,
        alice(),
    )
    .await);
    assert_eq!(
        attempts(&h).await,
        2,
        "overlapping selection reasons still yield one attempt"
    );
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn read_policy_uses_current_groups_and_never_short_circuits_unknown_ids() {
    let policy = FieldPolicy::new()
        .member(ALICE, ["base.group_user"])
        .group_name("base.group_user", "Internal");
    let handle = policy.clone();
    let h = harness_with_policy(
        &[],
        policy,
        config(ReadAuditPolicy::user_groups(&["base.group_user"]).unwrap()),
        false,
    )
    .await;
    ok(&call(&h, "/json/2/test_rpc.effects/probe", "{}", alice()).await);
    assert_eq!(attempts(&h).await, 1);
    let _handle = handle.member(ALICE, []);
    ok(&call(&h, "/json/2/test_rpc.effects/probe", "{}", alice()).await);
    assert_eq!(attempts(&h).await, 1);
    h.cleanup().await.unwrap();
    let policy = FieldPolicy::new().member(ALICE, ["base.group_user"]);
    let h = harness_with_policy(
        &[&ADDON],
        policy,
        config(ReadAuditPolicy::user_groups(&["base.group_user", "x.unknown"]).unwrap()),
        false,
    )
    .await;
    let denied = call(
        &h,
        "/json/2/audit_policy.flagged/search",
        r#"{"domain":[]}"#,
        alice(),
    )
    .await;
    assert_error(
        &denied,
        500,
        "erp.programming",
        "audit read policy names unresolved group 'x.unknown'",
    );
    assert_eq!(attempts(&h).await, 0);
    h.cleanup().await.unwrap();
}

/// The `read_ok` rows of the calls of `audit_policy.elevated.<method>`: how many, and the events
/// text of the first.
async fn elevated_rows(h: &erp_testkit::RpcHarness, method: &str) -> (usize, String) {
    let count: usize = sql_one(h, &format!("SELECT count(*)::text FROM erp_event.ir_audit_entry WHERE outcome='read_ok' AND model='audit_policy.elevated' AND method='{method}'")).await.parse().unwrap();
    let events = if count == 0 {
        String::new()
    } else {
        sql_one(h, &format!("SELECT events::text FROM erp_event.ir_audit_entry WHERE outcome='read_ok' AND model='audit_policy.elevated' AND method='{method}' ORDER BY id LIMIT 1")).await
    };
    (count, events)
}

/// statement, not an ORM read, because the compiled access policy refuses scoped decisions until
/// ) a read-effect call opens a scoped block in its READ ONLY transaction: it
/// answers 200 with one `read_ok` row that carries the reason, also when read policy selects the
/// call and when two blocks open in one call, and an audit store that cannot write the row answers
/// the audit-unavailable response with the result withheld.
#[tokio::test]
async fn read_call_with_a_scoped_block_writes_one_read_ok_row_or_is_withheld() {
    let h = harness_with(&[&ADDON], RpcConfig::default(), false).await;
    let answered = call(&h, "/json/2/audit_policy.elevated/scoped", "{}", alice()).await;
    assert_eq!(ok(&answered), "on:true");
    let (count, events) = elevated_rows(&h, "scoped").await;
    assert_eq!(count, 1, "{events}");
    assert!(events.contains(SCOPED_REASON), "{events}");
    assert_eq!(attempts(&h).await, 1);

    let twice = call(
        &h,
        "/json/2/audit_policy.elevated/scoped_twice",
        "{}",
        alice(),
    )
    .await;
    assert_eq!(ok(&twice), "on:true|on:true");
    let (count, events) = elevated_rows(&h, "scoped_twice").await;
    assert_eq!(count, 1, "one row for a call with two blocks: {events}");
    assert!(events.contains(SCOPED_REASON), "{events}");
    assert!(
        events.contains("audit_policy.test.scoped_again"),
        "{events}"
    );
    assert_eq!(attempts(&h).await, 2);

    sql_batch(&h, "ALTER TABLE erp_event.ir_audit_entry ADD CONSTRAINT audit_policy_refuse CHECK (outcome <> 'read_ok') NOT VALID").await;
    let withheld = call(&h, "/json/2/audit_policy.elevated/scoped", "{}", alice()).await;
    assert_error(
        &withheld,
        503,
        "erp.audit_unavailable",
        "The audit trail could not record this call: a call that changes data changed nothing, and a result selected for audit is withheld. Retry later or tell an administrator.",
    );
    assert!(
        !withheld.body().windows(7).any(|part| part == b"on:true"),
        "the result must be withheld"
    );
    assert_eq!(attempts(&h).await, 2);
    h.cleanup().await.unwrap();

    // read policy selecting the same call still yields one row
    let h = harness_with(&[&ADDON], config(ReadAuditPolicy::all_users()), false).await;
    let answered = call(&h, "/json/2/audit_policy.elevated/scoped", "{}", alice()).await;
    assert_eq!(ok(&answered), "on:true");
    assert_eq!(attempts(&h).await, 1, "selection and elevation: one row");
    let (count, events) = elevated_rows(&h, "scoped").await;
    assert_eq!(count, 1);
    assert!(events.contains(SCOPED_REASON), "{events}");
    h.cleanup().await.unwrap();
}
