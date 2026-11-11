//! Guard evaluation through the testkit's `FieldPolicy` (4, GA-1):
//! the test policy keeps memberships in its own state, so it supplies `subject_groups`, resolves
//! group references itself and says its answers are not stable (plans are never cached).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_orm::{
    AccessPolicy, GroupRefResolution, GroupSpec, GuardIdx, GuardPlan, GuardPlanBuilder,
    GuardPlanLimits, QualifiedXmlId, RecordGuardIdx,
};
use erp_sdk::Value;
use erp_test_models::{base_users, test_field_access};
use erp_testkit::{FieldPolicy, Harness};

const MODEL: &str = "test_view_groups.model";

async fn harness_with_policy(policy: FieldPolicy) -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &erp_test_models::test_view_groups::ADDON,
    ])
    .with_decls([erp_data::ir_model_data_decl()])
    .policy(policy)
    .build()
    .await
    .expect("harness")
}

fn id_of(policy: &FieldPolicy, xmlid: &str) -> erp_orm::GroupId {
    match policy
        .resolve_group_ref(&QualifiedXmlId::parse(xmlid).unwrap())
        .unwrap()
    {
        GroupRefResolution::Known { id, .. } => id,
        other => panic!("{xmlid} is not a known group of the policy: {other:?}"),
    }
}

/// Node guards 0 `base.group_no_one`, 1 `base.group_user`, 2 `x.late`; record guard 0 on the
/// groups of `record`.
fn plan(record: &[erp_orm::GroupId]) -> Arc<GuardPlan> {
    let mut b = GuardPlanBuilder::new(MODEL, GuardPlanLimits::default()).unwrap();
    for text in ["base.group_no_one", "base.group_user", "x.late"] {
        b.node_guard(GroupSpec::parse(text).unwrap()).unwrap();
    }
    if !record.is_empty() {
        b.record_guard(7, record).unwrap();
    }
    Arc::new(b.finish().unwrap())
}

async fn verdicts(h: &Harness, uid: i64, debug: bool, plan: &Arc<GuardPlan>) -> Vec<bool> {
    let mut tx = h.begin().await.unwrap();
    let env = tx.env().with_user(uid).with_debug(debug);
    let evaluation = tx.evaluate_guards(&env, plan).await.unwrap();
    let mut out: Vec<bool> = (0..3)
        .map(|i| evaluation.node_allowed(GuardIdx::new(i).unwrap()).unwrap())
        .collect();
    if !evaluation.plan().record_guards().is_empty() {
        out.push(
            evaluation
                .record_allowed(RecordGuardIdx::new(0).unwrap())
                .unwrap(),
        );
    }
    out
}

/// A debug principal is not made a member of `base.group_no_one`: the group counts in debug mode
/// for a subject who is a member of it, and for nobody else.
#[tokio::test]
async fn the_debug_group_counts_only_for_its_members_in_debug_mode() {
    let policy = FieldPolicy::new()
        .member(10, ["base.group_user"])
        .member(11, ["base.group_user", "base.group_no_one"]);
    let h = harness_with_policy(policy).await;
    let plan = plan(&[]);
    // not a member: never, debug or not
    assert_eq!(verdicts(&h, 10, true, &plan).await, [false, true, false]);
    assert_eq!(verdicts(&h, 10, false, &plan).await, [false, true, false]);
    // a member: only in debug mode
    assert_eq!(verdicts(&h, 11, false, &plan).await, [false, true, false]);
    assert_eq!(verdicts(&h, 11, true, &plan).await, [true, true, false]);
}

/// The test policy says its group references are not stable, so nothing is cached, and a group
/// that becomes known inside the same snapshot is seen by the next evaluation (a cached plan
/// would still treat `x.late` as unknown).
#[tokio::test]
async fn unstable_group_references_bypass_the_resolved_plan_cache() {
    let policy = FieldPolicy::new().member(10, ["base.group_user"]);
    let handle = policy.clone();
    let h = harness_with_policy(policy).await;
    assert!(!handle.group_refs_are_stable());
    let plan = plan(&[]);
    assert_eq!(verdicts(&h, 10, false, &plan).await, [false, true, false]);
    handle.join(10, "x.late");
    assert_eq!(verdicts(&h, 10, false, &plan).await, [false, true, true]);
    let stats = h.orm().resolved_guard_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.skipped, stats.entries),
        (0, 0, 0, 0),
        "{stats:?}"
    );
}

/// A record guard and a node guard naming the same groups give the same answer, for a member, a
/// non-member, and the debug group in and out of debug mode.
#[tokio::test]
async fn record_guards_and_node_guards_agree() {
    let policy = FieldPolicy::new()
        .member(10, ["base.group_user"])
        .member(11, ["base.group_user", "base.group_no_one"])
        .member(12, ["x.other"]);
    let handle = policy.clone();
    let h = harness_with_policy(policy).await;
    let user = id_of(&handle, "base.group_user");
    let no_one = id_of(&handle, "base.group_no_one");
    for (uid, debug) in [(10, false), (11, false), (11, true), (12, true), (10, true)] {
        let by_user = verdicts(&h, uid, debug, &plan(&[user])).await;
        assert_eq!(
            by_user[3], by_user[1],
            "base.group_user, user {uid}, debug {debug}"
        );
        let by_no_one = verdicts(&h, uid, debug, &plan(&[no_one])).await;
        assert_eq!(
            by_no_one[3], by_no_one[0],
            "base.group_no_one, user {uid}, debug {debug}"
        );
    }
}

const VEILED: &str = "test_fa.veiled";

async fn veiled_harness(policy: FieldPolicy) -> Harness {
    let h = Harness::builder([&base_users::ADDON, &test_field_access::ADDON])
        .policy(policy.clone())
        .build()
        .await
        .expect("harness");
    policy.attach(h.registry()).unwrap();
    h
}

/// A plan with one field reference: `secret` of the veiled model.
fn veiled_plan() -> (Arc<GuardPlan>, erp_orm::FieldIdx) {
    let mut b = GuardPlanBuilder::new(VEILED, GuardPlanLimits::default()).unwrap();
    let field = b.field(VEILED, "secret").unwrap();
    (Arc::new(b.finish().unwrap()), field)
}

/// An evaluation minted under the open context is not for the same principal with a declared
/// context key that changes the field verdict, and the reverse; an undeclared key is not part of
/// the identity; another user or debug mode is a different principal.
#[tokio::test]
async fn declared_field_access_context_is_part_of_the_evaluation_identity() {
    let policy = FieldPolicy::new()
        .with_overrides()
        .member(10, ["base.group_user"])
        .member(11, ["base.group_user"]);
    let h = veiled_harness(policy).await;
    let (plan, secret) = veiled_plan();
    let mut tx = h.begin().await.unwrap();
    let open = tx.env().with_user(10);
    let veiled = open.with_context([("fa_veil", Value::Bool(true))]);

    // minted under the open context: readable, and for that context only
    let evaluation = tx.evaluate_guards(&open, &plan).await.unwrap();
    assert!(evaluation.field_readable(secret).unwrap());
    assert!(evaluation.is_for(&open));
    assert!(
        !evaluation.is_for(&veiled),
        "fa_veil=true changes the verdict"
    );
    // controls: an undeclared key is not identity; another user and debug mode are
    let other_key = open.with_context([("lang", Value::str("fr_FR"))]);
    assert!(evaluation.is_for(&other_key));
    assert!(!evaluation.is_for(&open.with_user(11)));
    assert!(!evaluation.is_for(&open.with_debug(true)));
    drop(evaluation);

    // minted under the denying context: unreadable, and for that context only
    let evaluation = tx.evaluate_guards(&veiled, &plan).await.unwrap();
    assert!(!evaluation.field_readable(secret).unwrap());
    assert!(evaluation.is_for(&veiled));
    assert!(!evaluation.is_for(&open));
    drop(evaluation);
    drop(tx);
    h.cleanup().await.unwrap();
}
