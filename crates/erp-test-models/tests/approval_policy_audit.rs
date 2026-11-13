// SPDX-License-Identifier: MIT OR Apache-2.0
//! Every change of an approval policy revision through RPC leaves its summary row in the audit
//! trail, and a refused change its attempt row (slice T2).
//! The masked approval refusal stays masked in the RPC debug field (S3, ADR
//! 0169 amendment 2026-11-13).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_addon_base::ir_approval_policy::{ApprovalCall, resolve};
use erp_orm::{Orm, UserId};
use erp_registry::ModelSpec;
use erp_rpc::{RpcCall, RpcConfig};
use erp_sdk::prelude::*;
use serde_json::{Value, json};
use support::rpc::*;

/// (kind, outcome, error class, model, method, targets) of one audit row.
type AuditRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Value,
);

/// Every audit row, in order.
async fn audit(h: &erp_testkit::RpcHarness) -> Vec<AuditRow> {
    let mut tx = h.harness().begin().await.unwrap();
    let found = tx
        .query(
            "SELECT entry_kind, outcome, error_class, model, method, target_ids::text, subject_id \
             FROM erp_event.ir_audit_entry ORDER BY id",
            &[],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    found
        .iter()
        .map(|r| {
            assert_eq!(r.get::<_, Option<i64>>(6), Some(ALICE));
            (
                r.get(0),
                r.get(1),
                r.get(2),
                r.get(3),
                r.get(4),
                serde_json::from_str(&r.get::<_, String>(5)).unwrap(),
            )
        })
        .collect()
}

/// approval facts come with)
#[tokio::test]
async fn policy_changes_leave_audit_rows() {
    let h = harness_with(
        &[&erp_addon_base::ADDON],
        erp_rpc::RpcConfig::default(),
        false,
    )
    .await;
    let created = call(
        &h,
        "/json/2/ir.approval.policy/create",
        &json!({"vals_list": [{"name": "deny deletes", "principal_kind": "user",
            "effect": "destructive", "outcome": "deny"}]})
        .to_string(),
        alice(),
    )
    .await;
    ok(&created);
    let id: i64 = sql_one(
        &h,
        "SELECT id::text FROM ir_approval_policy WHERE name = 'deny deletes'",
    )
    .await
    .parse()
    .unwrap();
    let archived = call(
        &h,
        "/json/2/ir.approval.policy/write",
        &json!({"ids": [id], "vals": {"active": false}}).to_string(),
        alice(),
    )
    .await;
    ok(&archived);
    let renamed = call(
        &h,
        "/json/2/ir.approval.policy/write",
        &json!({"ids": [id], "vals": {"name": "renamed"}}).to_string(),
        alice(),
    )
    .await;
    assert_error(
        &renamed,
        422,
        "erp.exceptions.ValidationError",
        "Approval policy revisions cannot be changed; archive this revision and create a new one.",
    );
    let deleted = call(
        &h,
        "/json/2/ir.approval.policy/unlink",
        &json!({"ids": [id]}).to_string(),
        alice(),
    )
    .await;
    assert_error(
        &deleted,
        422,
        "erp.exceptions.ValidationError",
        "Approval policy revisions cannot be deleted; archive them instead.",
    );
    let model = Some("ir.approval.policy".to_owned());
    let change = |method: &str| {
        (
            "change".to_owned(),
            "committed".to_owned(),
            None,
            model.clone(),
            Some(method.to_owned()),
            json!([id]),
        )
    };
    let refused = |method: &str| {
        (
            "attempt".to_owned(),
            "failed".to_owned(),
            Some("validation".to_owned()),
            model.clone(),
            Some(method.to_owned()),
            json!([id]),
        )
    };
    let rows = audit(&h).await;
    assert_eq!(rows.len(), 4, "{rows:#?}");
    // a create call names no input ids: its row has no targets
    let mut create = change("create");
    create.5 = json!([]);
    assert_eq!(rows[0], create);
    assert_eq!(rows[1], change("write"));
    assert_eq!(rows[2], refused("write"));
    assert_eq!(rows[3], refused("unlink"));
    assert_eq!(
        sql_one(
            &h,
            &format!("SELECT name || '/' || active::text FROM ir_approval_policy WHERE id = {id}")
        )
        .await,
        "deny deletes/false"
    );
    h.cleanup().await.unwrap();
}

/// closed after an addon override raises that effect; it is never passed over for a weaker one)
#[tokio::test]
async fn a_method_revision_of_an_earlier_effect_fails_closed() {
    let h = harness_with(
        &[&erp_addon_base::ADDON],
        erp_rpc::RpcConfig::default(),
        false,
    )
    .await;
    // written while `test_rpc.effects.widened` is a read method
    ok(&call(
        &h,
        "/json/2/ir.approval.policy/create",
        &json!({"vals_list": [{"name": "no widened", "principal_kind": "user", "effect": "read",
            "model": "test_rpc.effects", "method": "widened", "outcome": "deny"}]})
        .to_string(),
        alice(),
    )
    .await);
    let id: i64 = sql_one(
        &h,
        "SELECT id::text FROM ir_approval_policy WHERE name = 'no widened'",
    )
    .await
    .parse()
    .unwrap();
    // the same database once `test_rpc_ext` widens the method to external
    let registry = Arc::new(
        erp_sdk::RegistryBuilder::new([
            &erp_test_models::base_users::ADDON,
            &erp_test_models::test_orm::ADDON,
            &erp_test_models::test_rpc::ADDON,
            &erp_addon_base::ADDON,
            &erp_test_models::test_rpc::ext::ADDON,
        ])
        .with_decls(support::currency_fixture::decls())
        .build()
        .unwrap(),
    );
    let orm = Orm::builder(registry, h.harness().db().pool().clone())
        .policy(erp_testkit::RulesPolicy::new(FixturePolicy).unwrap())
        .build()
        .unwrap();
    let mut tx = orm.begin(UserId::new(ALICE).unwrap()).await.unwrap();
    let env = tx.env().with_context([(
        "allowed_company_ids",
        erp_sdk::Value::list([erp_sdk::Value::Int(1)]),
    )]);
    // a model method: resolved for every company Alice is allowed in (none recorded here) and
    // her active company 1
    let call = ApprovalCall::model_level("test_rpc.effects", "widened").unwrap();
    let error = resolve(&env, &mut tx, &call).await.unwrap_err();
    tx.rollback().await.unwrap();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation, "{error}");
    assert_eq!(
        error.message(),
        format!(
            "Approval policy revision 'no widened' (id {id}): it was written for effect read, \
             but 'test_rpc.effects.widened' now has effect external; archive it and create a \
             revision for the current effect."
        )
    );
    h.cleanup().await.unwrap();
}

erp_sdk::addon!(name = "test_appr_gate", depends = ["base"]);

/// The refusal of `resolve` for a caller who may not read the revisions involved.
const UNAPPROVABLE: &str =
    "This action needs an approval that is not set up correctly; ask an administrator.";
/// The name of the revision the caller may not read.
const HIDDEN: &str = "hidden-rev-5c1e";

/// A model whose RPC method asks `resolve` for its own call.
#[model(name = "test_appr_gate.gate", addon = "test_appr_gate")]
pub struct Gate {
    #[field(char)]
    pub name: Option<Arc<str>>,
}

#[methods(Gate, addon = "test_appr_gate")]
impl Gate {
    /// `write`, model level: resolves the approval policy of this very call and returns its
    /// refusal unchanged.
    #[method]
    #[rpc(effect = write, model)]
    async fn check(this: &RecordSet<Gate>, tx: &mut Transaction) -> Result<bool> {
        let call = ApprovalCall::model_level("test_appr_gate.gate", "check")?;
        resolve(this.env(), tx, &call).await?;
        Ok(true)
    }
}

/// The fixture policy, except that nobody reads approval policy revisions.
#[derive(Debug, Clone, Copy)]
struct HiddenRevisions;

impl erp_testkit::TestRules for HiddenRevisions {
    fn check_model(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: erp_orm::Operation,
    ) -> Result<()> {
        if model.name == "ir.approval.policy" && op == erp_orm::Operation::Read {
            return Err(Error::access(
                "You are not allowed to read approval policy revisions.",
            ));
        }
        erp_testkit::TestRules::check_model(&FixturePolicy, b, model, op)
    }

    fn access_domain(
        &self,
        b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: erp_orm::Operation,
    ) -> Result<erp_sdk::Domain> {
        erp_testkit::TestRules::access_domain(&FixturePolicy, b, model, op)
    }
}

/// error source, so the RPC debug field of a debug-capable caller shows neither the revision's
/// name nor its id)
#[tokio::test]
async fn the_masked_refusal_leaves_nothing_for_the_debug_field() {
    let h = harness_with_policy(
        &[&erp_addon_base::ADDON, &ADDON],
        erp_testkit::RulesPolicy::new(HiddenRevisions).unwrap(),
        RpcConfig::builder().error_debug(true).build().unwrap(),
        false,
    )
    .await;
    // an Approve revision without approvers: its refusal names it ("... '<name>' (id N): ...")
    let id = sql_one(
        &h,
        &format!(
            "INSERT INTO ir_approval_policy (name, active, principal_kind, effect, model, method, \
             outcome) VALUES ('{HIDDEN}', true, 'user', 'write', 'test_appr_gate.gate', 'check', \
             'approve') RETURNING id::text"
        ),
    )
    .await;
    let r = h
        .service()
        .call(RpcCall::json2(
            "/json/2/test_appr_gate.gate/check",
            b"{}".to_vec(),
            user(ADMIN),
        ))
        .await;
    assert_error(&r, 422, "erp.exceptions.ValidationError", UNAPPROVABLE);
    let text = String::from_utf8_lossy(r.body()).into_owned();
    let debug = json(&r)["debug"].as_str().unwrap().to_owned();
    // ADMIN holds the debug capability: the field is rendered
    assert!(
        debug.contains(&format!("error: {UNAPPROVABLE}")),
        "debug rendered: {text}"
    );
    assert!(!debug.contains("caused by"), "no error source: {debug}");
    for hidden in [
        HIDDEN.to_owned(),
        format!("(id {id})"),
        format!("revision {id}"),
    ] {
        assert!(!text.contains(&hidden), "{hidden} leaked: {text}");
    }
    h.cleanup().await.unwrap();
}
