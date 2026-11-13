//! guard evaluation through the compiled policy: `Transaction::evaluate_guards`
//! decides a plan's node guards on the subject's groups, the record guards on `group_ids` and the
//! field references on the field masks, in the pinned policy snapshot.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_fields::{ErrorKind, Value};
use erp_orm::{
    GroupId, GroupSpec, GuardIdx, GuardPlan, GuardPlanBuilder, GuardPlanLimits, Orm,
    PolicyProvider, RecordGuardIdx, UserId,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;
use support::{connect, pool};

static COUNTRY: AddonDescriptor = AddonDescriptor {
    name: "x_country",
    depends: &["base"],
    exact_only: false,
};

const FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::char("code").groups("base.group_system"),
];

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &COUNTRY,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("x_country", "x.country")
            .description("Country")
            .fields(FIELDS),
        ModelDecl::new("x_country", "x.hidden")
            .description("Hidden")
            .fields(FIELDS),
    ])
    .build()
    .await
    .expect("harness")
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

struct Seeded {
    /// internal user, also in `base.group_no_one`
    demo: i64,
    /// a user in no group
    nobody: i64,
    restricted: i64,
    no_one: i64,
    country: i64,
}

async fn seed(client: &Client) -> Seeded {
    let group = async |name: &str, xmlid: &str| -> i64 {
        let id = one(
            client,
            "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
            &[&name],
        )
        .await;
        let (module, name) = xmlid.split_once('.').unwrap();
        client
            .execute(
                "INSERT INTO ir_model_data (module, name, model, res_id) \
                 VALUES ($1, $2, 'res.groups', $3)",
                &[&module, &name, &id],
            )
            .await
            .unwrap();
        id
    };
    let internal = group("Internal User", "base.group_user").await;
    let restricted = group("Restricted", "x_country.group_restricted").await;
    group("Settings", "base.group_system").await;
    let no_one = group("Technical Features", "base.group_no_one").await;
    let user = async |name: &str| -> i64 {
        one(
            client,
            "INSERT INTO res_users (name, login, active, share) VALUES ($1, $1, true, false) \
             RETURNING id::int8",
            &[&name],
        )
        .await
    };
    let demo = user("demo").await;
    let nobody = user("nobody").await;
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2), ($1, $3)",
            &[&demo, &internal, &no_one],
        )
        .await
        .unwrap();
    let model = async |name: &str, model: &str| -> i64 {
        one(
            client,
            "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $2, 'base', false) \
             RETURNING id::int8",
            &[&name, &model],
        )
        .await
    };
    let country = model("Country", "x.country").await;
    model("Hidden", "x.hidden").await;
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) VALUES \
             ('country read', $1, $2, 'r', NULL, true)",
            &[&country, &internal],
        )
        .await
        .unwrap();
    Seeded {
        demo,
        nobody,
        restricted,
        no_one,
        country,
    }
}

async fn policy_orm(h: &Harness) -> Orm {
    let source = PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(h.registry()), pool(h, 2))
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap()
}

fn spec(text: &str) -> GroupSpec {
    GroupSpec::parse(text).unwrap()
}

/// The plan every test decides: node guards 0..=5, fields 0 (`name`) and 1 (`code`), and one
/// record guard on the restricted group when `record` names it.
fn plan(record: Option<i64>) -> Arc<GuardPlan> {
    let mut b = GuardPlanBuilder::new("x.country", GuardPlanLimits::default()).unwrap();
    for text in [
        "base.group_system",
        "!x_unknown.missing",
        "x_unknown.missing",
        "x_country.group_restricted,base.group_user",
        "base.group_no_one",
        "!base.group_user",
    ] {
        b.node_guard(spec(text)).unwrap();
    }
    b.field("x.country", "name").unwrap();
    b.field("x.country", "code").unwrap();
    if let Some(group) = record {
        b.record_guard(7, &[GroupId::new(group).unwrap()]).unwrap();
    }
    Arc::new(b.finish().unwrap())
}

/// Verdicts of the six node guards for `uid`.
async fn node_verdicts(orm: &Orm, uid: i64, debug: bool, sudo: bool) -> Vec<bool> {
    let mut tx = orm.begin(UserId::new(uid).unwrap()).await.unwrap();
    let env = tx.env().with_debug(debug).sudo(sudo);
    let plan = plan(None);
    let evaluation = tx.evaluate_guards(&env, &plan).await.unwrap();
    let verdicts = (0..6)
        .map(|i| evaluation.node_allowed(GuardIdx::new(i).unwrap()).unwrap())
        .collect();
    drop(evaluation);
    tx.commit().await.unwrap();
    verdicts
}

/// The internal demo user (also in the debug group) holds `base.group_user` and
/// `base.group_no_one`: `base.group_system` is refused, the unknown group never matches, its
/// negation never excludes, `restricted,user` passes on the user group, the debug group counts only
/// in debug mode, and `!base.group_user` refuses a member. Superuser mode does not change guard
/// answers.
#[tokio::test]
async fn node_guards_follow_the_subjects_groups() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;

    let normal = node_verdicts(&orm, seeded.demo, false, false).await;
    assert_eq!(normal, vec![false, true, false, true, false, false]);
    let debug = node_verdicts(&orm, seeded.demo, true, false).await;
    assert_eq!(debug, vec![false, true, false, true, true, false]);
    let sudo = node_verdicts(&orm, seeded.demo, false, true).await;
    assert_eq!(sudo, normal);
}

/// A user in no group is refused by every guard, the negative-only ones included. Superuser mode
/// reads the model so the question can be asked at all.
#[tokio::test]
async fn an_empty_group_set_refuses_every_guard() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;
    let verdicts = node_verdicts(&orm, seeded.nobody, false, true).await;
    assert_eq!(verdicts, vec![false; 6]);
}

/// `code` is restricted to `base.group_system`: unreadable for the internal user, all-readable in
/// superuser mode.
#[tokio::test]
async fn field_references_follow_the_field_masks() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;
    for (sudo, expected) in [(false, [true, false]), (true, [true, true])] {
        let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
        let env = tx.env().sudo(sudo);
        let plan = plan(None);
        let evaluation = tx.evaluate_guards(&env, &plan).await.unwrap();
        let readable: Vec<bool> = (0..2)
            .map(|i| {
                evaluation
                    .field_readable(erp_orm::FieldIdx::new(i).unwrap())
                    .unwrap()
            })
            .collect();
        assert_eq!(readable, expected, "sudo = {sudo}");
    }
}

#[tokio::test]
async fn an_index_outside_the_plan_is_a_programming_error() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let plan = plan(Some(seeded.restricted));
    let evaluation = tx.evaluate_guards(&env, &plan).await.unwrap();
    let err = evaluation
        .node_allowed(GuardIdx::new(6).unwrap())
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "guard index 6 outside plan of 6 guards");
    let err = evaluation
        .record_allowed(RecordGuardIdx::new(1).unwrap())
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "record guard index 1 outside plan of 1 record guards"
    );
    let err = evaluation
        .field_readable(erp_orm::FieldIdx::new(2).unwrap())
        .unwrap_err();
    assert_eq!(err.message(), "field index 2 outside plan of 2 fields");
}

/// A plan for a model the principal may not read never leaves; an evaluation is for the
/// environment it was decided for and for no other.
#[tokio::test]
async fn evaluation_checks_model_access_and_knows_its_principal() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();

    let mut b = GuardPlanBuilder::new("x.hidden", GuardPlanLimits::default()).unwrap();
    b.node_guard(spec("base.group_user")).unwrap();
    let hidden = Arc::new(b.finish().unwrap());
    let err = tx.evaluate_guards(&env, &hidden).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert!(
        err.message()
            .starts_with("You are not allowed to access 'Hidden' (x.hidden) records."),
        "{}",
        err.message()
    );

    let open = plan(None);
    let evaluation = tx.evaluate_guards(&env, &open).await.unwrap();
    assert!(evaluation.is_for(&env));
    assert!(
        !evaluation.is_for(&env.with_debug(true)),
        "debug is part of the principal"
    );
    assert_eq!(evaluation.subject().get(), seeded.demo);
    assert_eq!(evaluation.registry_generation(), h.registry().generation());
    assert!(evaluation.snapshot().is_shared());
}

/// The record guard on the restricted group follows the user's membership transaction by
/// transaction, while the resolved plan stays cached for the snapshot (hits grow, nothing is
/// resolved again); the symbolic plan is unchanged.
#[tokio::test]
async fn membership_changes_are_seen_by_the_next_transaction() {
    let h = harness().await;
    let client = connect(&h).await;
    let seeded = seed(&client).await;
    let orm = policy_orm(&h).await;
    let plan = plan(Some(seeded.restricted));
    let before = plan.clone();

    let verdict = async |orm: &Orm| -> (bool, bool) {
        let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
        let env = tx.env();
        let evaluation = tx.evaluate_guards(&env, &plan).await.unwrap();
        let out = (
            evaluation
                .record_allowed(RecordGuardIdx::new(0).unwrap())
                .unwrap(),
            evaluation.node_allowed(GuardIdx::new(3).unwrap()).unwrap(),
        );
        drop(evaluation);
        tx.commit().await.unwrap();
        out
    };
    // node guard 3 (`restricted,user`) passes on the user group either way; the record guard needs
    // the restricted group itself
    assert_eq!(verdict(&orm).await, (false, true));
    client
        .execute(
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&seeded.demo, &seeded.restricted],
        )
        .await
        .unwrap();
    assert_eq!(verdict(&orm).await, (true, true));
    client
        .execute(
            "DELETE FROM res_groups_users_rel WHERE uid = $1 AND gid = $2",
            &[&seeded.demo, &seeded.restricted],
        )
        .await
        .unwrap();
    assert_eq!(verdict(&orm).await, (false, true));

    let stats = orm.resolved_guard_cache_stats();
    assert_eq!(stats.misses, 1, "resolved once for the snapshot: {stats:?}");
    assert!(stats.hits >= 2, "{stats:?}");
    assert_eq!(plan, before);
}

fn plan_with(root: &str, fields: &[(&str, &str)]) -> Arc<GuardPlan> {
    let mut b = GuardPlanBuilder::new(root, GuardPlanLimits::default()).unwrap();
    for (model, path) in fields {
        b.field(model, path).unwrap();
    }
    Arc::new(b.finish().unwrap())
}

/// A model the registry does not have, a field the model does not have and a hop through a
/// field that is not relational each fail the evaluation with `Programming` naming the plan.
#[tokio::test]
async fn a_plan_the_registry_cannot_satisfy_fails_loudly() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    for (fields, message) in [
        (
            ("x.nope", "name"),
            "the guard plan of 'x.country' names the model 'x.nope', which the registry does not \
             have",
        ),
        (
            ("x.country", "nope"),
            "the guard plan of 'x.country' reads 'x.country.nope', but 'x.country' has no field \
             'nope'",
        ),
        (
            ("x.country", "name.code"),
            "the guard plan of 'x.country' reads 'x.country.name.code', but 'name' of \
             'x.country' is not a relational field",
        ),
    ] {
        let plan = plan_with("x.country", &[fields]);
        let err = tx.evaluate_guards(&env, &plan).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{message}");
        assert_eq!(err.message(), message);
    }
}

/// Not an error: every field reference through that model is unreadable, the references on the
/// readable root stay decided by their mask, and superuser mode reads them all.
#[tokio::test]
async fn an_unreadable_non_root_model_makes_its_fields_unreadable_without_an_error() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;
    let mut b = GuardPlanBuilder::new("x.country", GuardPlanLimits::default()).unwrap();
    let own = b.field("x.country", "name").unwrap();
    let foreign = b.field("x.hidden", "name").unwrap();
    let plan = Arc::new(b.finish().unwrap());
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let evaluation = tx.evaluate_guards(&env, &plan).await.unwrap();
    assert!(evaluation.field_readable(own).unwrap());
    assert!(!evaluation.field_readable(foreign).unwrap());
    drop(evaluation);
    let env = tx.env().sudo(true);
    let evaluation = tx.evaluate_guards(&env, &plan).await.unwrap();
    assert!(evaluation.field_readable(own).unwrap());
    assert!(evaluation.field_readable(foreign).unwrap());
}

/// Without superuser mode the user with no group cannot read the model, so the question is
/// refused (`Access`) before any guard is decided.
#[tokio::test]
async fn a_user_in_no_group_is_refused_before_guards_are_decided() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm
        .begin(UserId::new(seeded.nobody).unwrap())
        .await
        .unwrap();
    let env = tx.env();
    let err = tx.evaluate_guards(&env, &plan(None)).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert!(
        err.message()
            .starts_with("You are not allowed to access 'Country' (x.country) records."),
        "{}",
        err.message()
    );
}

#[tokio::test]
async fn an_evaluation_is_not_for_another_user() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;
    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let evaluation = tx.evaluate_guards(&env, &plan(None)).await.unwrap();
    assert!(evaluation.is_for(&env));
    assert!(!evaluation.is_for(&env.with_user(seeded.nobody)));
    assert!(
        evaluation.is_for(&env.with_context([("undeclared_key", Value::Bool(true))])),
        "a context key no override declared is not part of the identity"
    );
}

#[tokio::test]
async fn record_guards_agree_with_node_guards_on_the_debug_group() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;
    for debug in [false, true] {
        let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
        let env = tx.env().with_debug(debug);
        let plan = plan(Some(seeded.no_one));
        let evaluation = tx.evaluate_guards(&env, &plan).await.unwrap();
        let node = evaluation.node_allowed(GuardIdx::new(4).unwrap()).unwrap();
        let record = evaluation
            .record_allowed(RecordGuardIdx::new(0).unwrap())
            .unwrap();
        assert_eq!((node, record), (debug, debug), "debug = {debug}");
    }
}

/// After a restriction is created in the transaction the evaluation runs on the transaction's
/// private snapshot (the policy refresh happens first, so a stale policy is never decided on);
/// nothing of it enters the shared cache.
#[tokio::test]
async fn a_private_snapshot_is_never_cached() {
    let h = harness().await;
    let seeded = seed(&connect(&h).await).await;
    let orm = policy_orm(&h).await;

    let mut tx = orm.begin(UserId::new(seeded.demo).unwrap()).await.unwrap();
    let env = tx.env();
    let shared = tx.evaluate_guards(&env, &plan(None)).await.unwrap();
    assert!(shared.snapshot().is_shared());
    let shared_id = shared.snapshot().id();
    drop(shared);
    let before = orm.resolved_guard_cache_stats();
    assert_eq!(before.misses, 1, "{before:?}");

    let sudo = tx.env().sudo(true);
    let vals: std::collections::BTreeMap<String, Value> = [
        ("name", Value::str("harmless")),
        ("model_id", Value::Int(seeded.country)),
        ("operation", Value::str("r")),
        ("domain", Value::str("[('name', '!=', 'zzz')]")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    sudo.records("ir.access")
        .unwrap()
        .create_values(&mut tx, &[vals])
        .await
        .unwrap();

    let evaluation = tx.evaluate_guards(&env, &plan(None)).await.unwrap();
    assert!(
        !evaluation.snapshot().is_shared(),
        "rebuilt for this transaction"
    );
    assert_ne!(evaluation.snapshot().id(), shared_id);
    let verdicts: Vec<bool> = (0..6)
        .map(|i| evaluation.node_allowed(GuardIdx::new(i).unwrap()).unwrap())
        .collect();
    assert_eq!(verdicts, vec![false, true, false, true, false, false]);
    drop(evaluation);
    let after = orm.resolved_guard_cache_stats();
    assert_eq!(
        (after.hits, after.misses, after.entries),
        (before.hits, before.misses, before.entries),
        "the private snapshot did not touch the shared cache"
    );
    tx.rollback().await.unwrap();
}
