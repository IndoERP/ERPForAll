//! JSON-2 calls and the audit trail (stage B,/002/009): one summary row
//! per mutating call, independent attempt rows for denials, failures, replays and unknown
//! commits, nothing for an unselected read, and the retry accounting. Every test runs against the
//! real audit table.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::atomic::Ordering;

use erp_orm::CommitFault;
use erp_orm::ModelAllow;
use erp_rpc::{CorrelationId, RpcCall};
use erp_test_models::test_rpc::ATTEMPTS;
use serde_json::{Value, json};
use support::rpc::*;

const AUDIT: &str = "erp_event.ir_audit_entry";

/// One audit row as the columns the tests compare.
#[derive(Debug)]
struct Row {
    kind: String,
    origin: String,
    outcome: String,
    error_class: Option<String>,
    attempt: i16,
    operation: String,
    correlation: Option<String>,
    original_operation: Option<String>,
    principal_kind: Option<String>,
    subject: Option<i64>,
    actor_kind: Option<String>,
    actor_id: Option<i64>,
    model: Option<String>,
    method: Option<String>,
    effect: Option<String>,
    targets: Value,
    digest: Option<Vec<u8>>,
    redaction_version: Option<i16>,
    events: Value,
}

async fn rows(h: &erp_testkit::RpcHarness) -> Vec<Row> {
    let mut tx = h.harness().begin().await.unwrap();
    let found = tx
        .query(
            &format!(
                "SELECT entry_kind, origin, outcome, error_class, attempt, operation_id::text, \
                 correlation_id::text, original_operation_id::text, principal_kind, subject_id, \
                 actor_kind, actor_id, model, method, effect, target_ids::text, args_digest, \
                 redaction_version, events::text FROM {AUDIT} ORDER BY id"
            ),
            &[],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    found
        .iter()
        .map(|r| Row {
            kind: r.get(0),
            origin: r.get(1),
            outcome: r.get(2),
            error_class: r.get(3),
            attempt: r.get(4),
            operation: r.get(5),
            correlation: r.get(6),
            original_operation: r.get(7),
            principal_kind: r.get(8),
            subject: r.get(9),
            actor_kind: r.get(10),
            actor_id: r.get(11),
            model: r.get(12),
            method: r.get(13),
            effect: r.get(14),
            targets: serde_json::from_str(&r.get::<_, String>(15)).unwrap(),
            digest: r.get(16),
            redaction_version: r.get(17),
            events: serde_json::from_str(&r.get::<_, String>(18)).unwrap(),
        })
        .collect()
}

fn audit_inserts(response: &erp_rpc::RpcResponse) -> usize {
    response
        .captured()
        .expect("statements were captured")
        .iter()
        .filter(|s| s.starts_with("INSERT INTO \"erp_event\".\"ir_audit_entry\""))
        .count()
}

/// A bulk write by a key-authenticated principal makes exactly one audit INSERT whatever the
/// number of records, carrying who, what, which ids (as runs), the redacted digest and the
/// correlation of the call; a second call with the same arguments has the same digest, another
/// value another one; a rolled back call leaves no change row.
#[tokio::test]
async fn a_bulk_write_makes_one_summary_row() {
    let h = harness().await;
    let ids = [
        effect(&h, "a", 1).await,
        effect(&h, "b", 1).await,
        effect(&h, "c", 1).await,
    ];
    let scope = scope(41, "bulk key", ModelAllow::All, false);
    let correlation = CorrelationId::parse("018f3a1c-7b2d-7c4e-9a10-00000000aa01").unwrap();
    let body = json!({"ids": ids, "vals": {"name": "renamed"}}).to_string();
    let call = |body: &str| {
        RpcCall::json2(
            "/json/2/test_rpc.effects/write",
            body.as_bytes().to_vec(),
            alice_key(scope.clone()),
        )
        .correlation(correlation.clone())
        .capture_statements()
    };
    let first = h.service().call(call(&body)).await;
    ok(&first);
    assert_eq!(audit_inserts(&first), 1, "N records, one INSERT");
    let again = h.service().call(call(&body)).await;
    ok(&again);
    let other = h
        .service()
        .call(call(
            &json!({"ids": ids, "vals": {"name": "other"}}).to_string(),
        ))
        .await;
    ok(&other);
    let all = rows(&h).await;
    assert_eq!(all.len(), 3);
    let row = &all[0];
    assert_eq!(
        (row.kind.as_str(), row.origin.as_str(), row.outcome.as_str()),
        ("change", "rpc", "committed")
    );
    assert_eq!(row.error_class, None);
    assert_eq!(row.attempt, 1);
    assert_eq!(row.principal_kind.as_deref(), Some("user"));
    assert_eq!(row.subject, Some(ALICE));
    assert_eq!(row.actor_kind.as_deref(), Some("api_key_user"));
    assert_eq!(row.actor_id, Some(41));
    assert_eq!(row.model.as_deref(), Some("test_rpc.effects"));
    assert_eq!(row.method.as_deref(), Some("write"));
    assert_eq!(row.effect.as_deref(), Some("write"));
    assert_eq!(
        row.targets,
        json!([[ids[0], ids[2]]]),
        "consecutive ids are one run"
    );
    assert_eq!(row.digest.as_ref().map(Vec::len), Some(32));
    assert_eq!(row.redaction_version, Some(2));
    assert_eq!(row.correlation.as_deref(), Some(correlation.as_str()));
    assert_eq!(row.events, json!([]));
    assert_eq!(all[0].digest, all[1].digest, "same call, same digest");
    assert_ne!(
        all[0].digest, all[2].digest,
        "another value, another digest"
    );
    assert_ne!(
        all[0].operation, all[1].operation,
        "a new call has a new operation"
    );
    h.cleanup().await.unwrap();
}

/// A failed call leaves no change row and one `failed` attempt row with a bounded class; a denied
/// call, for a read and for a write, one `denied` attempt row; the failed business change is
/// absent.
#[tokio::test]
async fn failures_and_denials_leave_independent_attempt_rows() {
    let h = harness().await;
    let failed = call(
        &h,
        "/json/2/test_rpc.effects/bump_then_fail",
        r#"{"name": "doomed"}"#,
        alice(),
    )
    .await;
    assert_error(
        &failed,
        422,
        "erp.exceptions.ValidationError",
        "bump refused",
    );
    assert_eq!(count(&h, "test_rpc_effects").await, 0, "nothing stayed");
    // a write Bob may not do, and a read Bob may not do
    let denied_write = call(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "no"}"#,
        user(BOB),
    )
    .await;
    assert_eq!(denied_write.status(), 403);
    let denied_read = call(
        &h,
        "/json/2/test_rpc.model_a/search",
        r#"{"domain": []}"#,
        user(BOB),
    )
    .await;
    assert_eq!(denied_read.status(), 403);
    let all = rows(&h).await;
    assert_eq!(all.len(), 3, "{all:#?}");
    assert!(all.iter().all(|r| r.kind == "attempt" && r.origin == "rpc"));
    let outcomes: Vec<(&str, Option<&str>, &str)> = all
        .iter()
        .map(|r| {
            (
                r.outcome.as_str(),
                r.error_class.as_deref(),
                r.method.as_deref().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        [
            ("failed", Some("validation"), "bump_then_fail"),
            ("denied", Some("access"), "bump"),
            ("denied", Some("access"), "search"),
        ]
    );
    assert_eq!(all[1].subject, Some(BOB));
    assert!(all.iter().all(|r| r.digest.is_some() && r.attempt == 1));
    h.cleanup().await.unwrap();
}

/// A read of a user principal writes no audit row and sends no audit statement.
#[tokio::test]
async fn a_user_read_writes_nothing() {
    let h = harness().await;
    effect(&h, "readable", 1).await;
    let r = h
        .service()
        .call(
            RpcCall::json2(
                "/json/2/test_rpc.effects/search",
                br#"{"domain": []}"#.to_vec(),
                alice(),
            )
            .capture_statements(),
        )
        .await;
    ok(&r);
    assert_eq!(audit_inserts(&r), 0);
    assert!(rows(&h).await.is_empty());
    h.cleanup().await.unwrap();
}

/// A forced serialization failure counts every attempt's INSERT and leaves at most one committed
/// change row, with the number of the attempt that committed; an exhausted retry leaves one
/// `failed` attempt row of class `concurrency` with the last attempt number.
#[tokio::test]
async fn retries_leave_one_row_with_the_committing_attempt_number() {
    let h = harness().await;
    ATTEMPTS.store(0, Ordering::SeqCst);
    let r = h
        .service()
        .call(
            RpcCall::json2(
                "/json/2/test_rpc.effects/conflict_once",
                br#"{"name": "second"}"#.to_vec(),
                alice(),
            )
            .capture_statements(),
        )
        .await;
    ok(&r);
    assert_eq!(r.attempts(), 2);
    assert_eq!(count(&h, "test_rpc_effects").await, 1);
    let all = rows(&h).await;
    assert_eq!(all.len(), 1, "the rolled back attempt left no row");
    assert_eq!((all[0].kind.as_str(), all[0].attempt), ("change", 2));
    // the captured statements of the successful attempt hold one audit INSERT
    assert_eq!(audit_inserts(&r), 1);

    ATTEMPTS.store(0, Ordering::SeqCst);
    let r = call(
        &h,
        "/json/2/test_rpc.effects/always_conflict",
        "{}",
        alice(),
    )
    .await;
    assert_eq!(r.status(), 500);
    assert_eq!(r.attempts(), erp_rpc::MAX_ATTEMPTS);
    let all = rows(&h).await;
    assert_eq!(all.len(), 2);
    let last = &all[1];
    assert_eq!(
        (
            last.kind.as_str(),
            last.outcome.as_str(),
            last.error_class.as_deref()
        ),
        ("attempt", "failed", Some("concurrency"))
    );
    assert_eq!(i64::from(last.attempt), i64::from(erp_rpc::MAX_ATTEMPTS));
    h.cleanup().await.unwrap();
}

/// An idempotent replay writes one `replayed` attempt row naming the idempotency row, and no
/// second change row; a lost COMMIT answer writes a `commit_unknown` attempt row beside the change
/// row that did land, and one that never reached the database writes the row alone.
#[tokio::test]
async fn replays_and_unknown_commits_get_their_own_attempt_rows() {
    let h = harness().await;
    let k1 = key("018f3a1c-7b2d-7c4e-9a10-00000000aa11");
    let body = r#"{"name": "once"}"#;
    let first = call_keyed(&h, "/json/2/test_rpc.effects/bump", body, alice(), &k1).await;
    ok(&first);
    let replay = call_keyed(&h, "/json/2/test_rpc.effects/bump", body, alice(), &k1).await;
    assert!(replay.replayed());
    let all = rows(&h).await;
    assert_eq!(all.len(), 2, "{all:#?}");
    assert_eq!(
        (all[0].kind.as_str(), all[0].outcome.as_str()),
        ("change", "committed")
    );
    assert_eq!(
        (all[1].kind.as_str(), all[1].outcome.as_str()),
        ("attempt", "replayed")
    );
    let claim: i64 = sql_one(&h, "SELECT id::text FROM ir_idempotency_key LIMIT 1")
        .await
        .parse()
        .unwrap();
    assert_eq!(
        all[1].events,
        json!([{"type": "replay", "idempotency_row": claim}])
    );
    assert_eq!(count(&h, "test_rpc_effects").await, 1, "no second change");

    // the answer was lost after the commit landed
    let k2 = key("018f3a1c-7b2d-7c4e-9a10-00000000aa12");
    h.service()
        .inject_commit_fault(CommitFault::AnswerLostAfterCommit);
    let r = call_keyed(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "landed"}"#,
        alice(),
        &k2,
    )
    .await;
    assert_eq!(json(&r)["name"], "erp.commit_unknown");
    let all = rows(&h).await;
    assert_eq!(all.len(), 4, "{all:#?}");
    assert_eq!(
        (all[2].kind.as_str(), all[2].outcome.as_str()),
        ("change", "committed")
    );
    let unknown = &all[3];
    assert_eq!(
        (unknown.kind.as_str(), unknown.outcome.as_str()),
        ("attempt", "commit_unknown")
    );
    assert_eq!(
        unknown.original_operation.as_deref(),
        Some(unknown.operation.as_str())
    );
    assert_eq!(all[2].operation, unknown.operation, "same execution");

    // the connection closed before COMMIT: nothing landed, the attempt row says unknown
    let k3 = key("018f3a1c-7b2d-7c4e-9a10-00000000aa13");
    h.service()
        .inject_commit_fault(CommitFault::ConnectionClosedBeforeCommit);
    let r = call_keyed(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "lost"}"#,
        alice(),
        &k3,
    )
    .await;
    assert_eq!(json(&r)["name"], "erp.commit_unknown");
    let all = rows(&h).await;
    assert_eq!(all.len(), 5, "{all:#?}");
    assert_eq!(
        (all[4].kind.as_str(), all[4].outcome.as_str()),
        ("attempt", "commit_unknown")
    );
    h.cleanup().await.unwrap();
}

/// A store that cannot take a row refuses a call that changes data before it starts (503, nothing
/// changed) and does not touch reads of users.
#[tokio::test]
async fn a_missing_audit_store_refuses_writes_with_503_and_leaves_reads() {
    let h = harness().await;
    sql_batch(&h, "DROP SCHEMA erp_event CASCADE").await;
    h.orm().check_audit_store().await.unwrap();
    let refused = call(
        &h,
        "/json/2/test_rpc.effects/bump",
        r#"{"name": "x"}"#,
        alice(),
    )
    .await;
    assert_error(
        &refused,
        503,
        "erp.audit_unavailable",
        "The audit trail could not record this call: a call that changes data changed nothing, \
         and a result selected for audit is withheld. Retry later or tell an administrator.",
    );
    assert_eq!(count(&h, "test_rpc_effects").await, 0);
    let read = call(
        &h,
        "/json/2/test_rpc.effects/search",
        r#"{"domain": []}"#,
        alice(),
    )
    .await;
    ok(&read);
    h.cleanup().await.unwrap();
}

/// No RPC reaches the audit table: it is in the closed event namespace and has no registry model.
#[tokio::test]
async fn the_audit_table_is_not_reachable_through_rpc() {
    let h = harness().await;
    for model in ["ir.audit.entry", "erp_event.ir_audit_entry"] {
        let r = call(
            &h,
            &format!("/json/2/{model}/search"),
            r#"{"domain": []}"#,
            user(ADMIN),
        )
        .await;
        assert_eq!(r.status(), 404, "{model}");
        let r = call(
            &h,
            &format!("/json/2/{model}/unlink"),
            r#"{"ids": [1]}"#,
            user(ADMIN),
        )
        .await;
        assert_eq!(r.status(), 404, "{model}");
    }
    assert!(rows(&h).await.is_empty());
    h.cleanup().await.unwrap();
}

/// Every audit row as one text, every column included, to search for cleartext.
async fn everything(h: &erp_testkit::RpcHarness) -> String {
    let text = sql(h, &format!("SELECT t::text FROM {AUDIT} t ORDER BY id")).await;
    text.into_iter().flatten().collect::<Vec<_>>().join("\n")
}

/// A field marked `secret` never reaches the audit row, in clear or in a guessable hash: the digest
/// of `write`, `create` and `web_save` with two different secrets is the same (the value is
/// replaced by `{"$redacted":"secret"}` before hashing), and no column holds the cleartext, for a
/// committed call as for one that fails.
#[tokio::test]
async fn secret_values_never_reach_an_audit_row() {
    let h = harness().await;
    let id = effect(&h, "holder", 1).await;
    let mut digests = Vec::new();
    for secret in ["CANARY-hunter2-AAAA", "CANARY-correct-horse-BBBB"] {
        let write = call(
            &h,
            "/json/2/test_rpc.effects/write",
            &json!({"ids": [id], "vals": {"token": secret}}).to_string(),
            alice(),
        )
        .await;
        ok(&write);
        let create = call(
            &h,
            "/json/2/test_rpc.effects/create",
            &json!({"vals_list": [{"name": "n", "token": secret}]}).to_string(),
            alice(),
        )
        .await;
        ok(&create);
        let saved = call(
            &h,
            "/json/2/test_rpc.effects/web_save",
            &json!({"ids": [id], "vals": {"token": secret}, "specification": {"name": {}}})
                .to_string(),
            alice(),
        )
        .await;
        ok(&saved);
        // a failing call carries the same digest rule
        let refused = call(
            &h,
            "/json/2/test_rpc.effects/write",
            &json!({"ids": [id], "vals": {"token": secret, "no_such_field": 1}}).to_string(),
            alice(),
        )
        .await;
        assert!(refused.status() >= 400);
        digests.push(rows(&h).await);
    }
    let all = digests.pop().unwrap();
    let first_pass = &digests[0];
    assert_eq!(
        all.len(),
        8,
        "four calls per secret, one row each: {all:#?}"
    );
    for (index, method) in ["write", "create", "web_save"].iter().enumerate() {
        let (a, b) = (&all[index], &all[4 + index]);
        assert_eq!(
            (a.kind.as_str(), a.method.as_deref(), a.redaction_version),
            ("change", Some(*method), Some(2))
        );
        assert!(a.digest.is_some());
        assert_eq!(
            a.digest, b.digest,
            "{method}: the secret is not in the digest"
        );
    }
    assert_eq!(all[3].kind, "attempt");
    assert_eq!(all[3].digest, all[7].digest, "a failed call redacts too");
    assert_eq!(first_pass.len(), 4);
    let shown = everything(&h).await;
    assert!(!shown.contains("CANARY"), "{shown}");
    h.cleanup().await.unwrap();
}

/// The password fields of `base` (`res.users.password` and the three password wizards) are marked
/// `secret` in the registry, so the redaction of `write`, `create` and `web_save` on them is the
/// rule tested above.
#[tokio::test]
async fn the_password_fields_of_base_are_secret() {
    let registry = erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON])
        .build()
        .unwrap();
    for (model, field) in [
        ("res.users", "password"),
        ("change.password.own", "new_password"),
        ("change.password.own", "confirm_password"),
        ("res.users.identitycheck", "password"),
        ("change.password.user", "new_passwd"),
        ("ir.config_parameter", "value"),
    ] {
        let spec = registry.model(model).unwrap_or_else(|| panic!("{model}"));
        assert!(
            spec.field(field)
                .unwrap_or_else(|| panic!("{model}.{field}"))
                .secret,
            "{model}.{field} must be secret"
        );
    }
}

/// A write that runs into the deadline during its work leaves no change row and no business
/// change, and one `failed` attempt row of class `unavailable`, like every other failure.
#[tokio::test]
async fn a_write_that_hits_the_deadline_leaves_an_attempt_row() {
    let h = harness().await;
    let r = call_with_deadline(
        &h,
        "/json/2/test_rpc.effects/slow_bump",
        r#"{"name": "late", "millis": 3000}"#,
        alice(),
        std::time::Duration::from_secs(1),
    )
    .await;
    assert_eq!(r.status(), 504);
    assert_eq!(count(&h, "test_rpc_effects").await, 0, "rolled back");
    let all = rows(&h).await;
    assert_eq!(all.len(), 1, "{all:#?}");
    let row = &all[0];
    assert_eq!(
        (
            row.kind.as_str(),
            row.outcome.as_str(),
            row.error_class.as_deref(),
            row.method.as_deref(),
            row.attempt
        ),
        (
            "attempt",
            "failed",
            Some("unavailable"),
            Some("slow_bump"),
            1
        )
    );
    assert_eq!(row.subject, Some(ALICE));
    assert!(row.digest.is_some());
    h.cleanup().await.unwrap();
}

/// The same canary against the real declarations of `base`: `write` and `web_save` on
/// `res.users.password` and `create` on `change.password.own`, whether the call succeeds, is
/// refused or fails, leave a row whose digest does not depend on the secret and no cleartext in
/// any column.
#[tokio::test]
async fn base_password_values_never_reach_an_audit_row() {
    let h = harness_with(
        &[&erp_addon_base::ADDON],
        erp_rpc::RpcConfig::default(),
        false,
    )
    .await;
    let sc = scope(41, "password key", ModelAllow::All, false);
    let mut by_secret = Vec::new();
    for secret in ["CANARY-hunter2-AAAA", "CANARY-correct-horse-BBBB"] {
        let calls = [
            (
                "res.users/write",
                json!({"ids": [ALICE], "vals": {"password": secret}}),
            ),
            (
                "res.users/web_save",
                json!({"ids": [ALICE], "vals": {"password": secret}, "specification": {"name": {}}}),
            ),
            (
                "change.password.own/create",
                json!({"vals_list": [{"new_password": secret, "confirm_password": secret}]}),
            ),
            (
                "res.users/create",
                json!({"vals_list": [{"name": "n", "login": "n-login", "password": secret}]}),
            ),
            // users created through the partner's one2many, as a plain list of maps and as triples
            (
                "res.partner/create",
                json!({"vals_list": [{"name": "p", "user_ids":
                    [{"name": "u", "login": "u-plain", "password": secret}]}]}),
            ),
            (
                "res.partner/create",
                json!({"vals_list": [{"name": "p", "user_ids":
                    [[0, 0, {"name": "u", "login": "u-triple", "password": secret}]]}]}),
            ),
            (
                "res.partner/write",
                json!({"ids": [999_999], "vals": {"user_ids":
                    [{"name": "u", "login": "u-w1", "password": secret}]}}),
            ),
            (
                "res.partner/write",
                json!({"ids": [999_999], "vals": {"user_ids":
                    [[0, 0, {"name": "u", "login": "u-w2", "password": secret}]]}}),
            ),
            // a credential kept as a config parameter value (D3)
            (
                "ir.config_parameter/create",
                json!({"vals_list": [{"key": "mail.smtp.password", "value": secret}]}),
            ),
            // the wizard refuses the password: the attempt row still carries a redacted digest
            (
                "change.password.wizard/create",
                json!({"vals_list": [{"user_ids":
                    [{"user_id": ALICE, "new_passwd": secret}]}]}),
            ),
            (
                "change.password.wizard/create",
                json!({"vals_list": [{"user_ids":
                    [[0, 0, {"user_id": ALICE, "new_passwd": secret}]]}]}),
            ),
        ];
        for (route, body) in calls {
            h.service()
                .call(RpcCall::json2(
                    &format!("/json/2/{route}"),
                    body.to_string().into_bytes(),
                    alice_key(sc.clone()),
                ))
                .await;
        }
        by_secret.push(rows(&h).await);
    }
    let second = by_secret.pop().unwrap();
    let first = by_secret.pop().unwrap();
    assert_eq!(
        first.len() * 2,
        second.len(),
        "eleven audited calls per secret: {first:#?}"
    );
    assert_eq!(first.len(), 11, "{first:#?}");
    for i in 0..11 {
        let (a, b) = (&first[i], &second[first.len() + i]);
        assert_eq!(a.method, b.method);
        assert!(
            a.digest.is_some() && a.redaction_version == Some(2),
            "{a:#?}"
        );
        assert_eq!(
            a.digest, b.digest,
            "{:?}: the secret is not in the digest",
            a.method
        );
    }
    let shown = everything(&h).await;
    assert!(!shown.contains("CANARY"), "{shown}");
    h.cleanup().await.unwrap();
}
