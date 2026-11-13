// SPDX-License-Identifier: MIT OR Apache-2.0
//! Company-dependent many2one deletion and dangling references.
//!
//! A reference held in a company map is not a foreign key. Deleting its target runs a referential
//! action (refuse or set null) in the ORM, whatever the caller may see; a reference whose target is
//! gone reads as empty and never as the fallback.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use erp_fields::{Cell, Error, ErrorKind, Value};
use erp_orm::{
    BeginOptions, Env, Operation, OperationIdentity, Records, SUPERUSER_ID, Spec, Transaction,
    WebReadGroup, ir_default,
};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_test_models::base_users;
use erp_testkit::{DomainPolicy, Harness, Otherwise};
use serde_json::json;

mod fixture {
    use erp_sdk::prelude::*;
    erp_sdk::addon!(name = "property_relations", depends = ["base"]);
    #[model(name = "prel.tag", addon = "property_relations")]
    pub struct Tag {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(many2one, comodel = "prel.tag", ondelete = "cascade")]
        pub parent_id: Option<RecId>,
    }
    #[model(name = "prel.holder", addon = "property_relations")]
    pub struct Holder {
        #[field(char)]
        pub name: Option<Arc<str>>,
        #[field(many2one, comodel = "prel.tag", company_dependent = true)]
        pub null_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "prel.tag",
            company_dependent = true,
            ondelete = "restrict"
        )]
        pub restrict_id: Option<RecId>,
        #[field(
            many2one,
            comodel = "prel.tag",
            company_dependent = true,
            ondelete = "cascade"
        )]
        pub cascade_id: Option<RecId>,
        #[field(many2one, comodel = "prel.tag")]
        pub plain_id: Option<RecId>,
        #[field(char, compute = "_compute_null_state")]
        pub null_state: Option<Arc<str>>,
    }
    #[methods(Holder, addon = "property_relations")]
    impl Holder {
        #[compute(depends = ["null_id"], depends_context = ["company"])]
        fn _compute_null_state(batch: &mut Batch<'_, Holder>) -> Result<()> {
            for i in batch.indices() {
                let state = if batch.get(i, Holder::NULL_ID)?.is_some() {
                    "set"
                } else {
                    "empty"
                };
                batch.set(i, Holder::NULL_STATE, Some(state.into()))?;
            }
            Ok(())
        }
    }
}

const REFUSAL: &str = "You cannot delete a record used by a company-dependent field";

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .build()
        .await
        .unwrap()
}
/// A harness whose record rules hide the holder row named `hidden` from reads, and refuse
/// every unlink of a tag when `lock_tags` is set.
async fn ruled_harness(lock_tags: bool) -> Harness {
    let visible = erp_sdk::Domain::condition("name", erp_sdk::Operator::Ne, "hidden").unwrap();
    let mut policy =
        DomainPolicy::builder(Otherwise::Allow).rule("prel.holder", Operation::Read, visible);
    if lock_tags {
        policy = policy.deny("prel.tag", Operation::Unlink);
    }
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &fixture::ADDON])
        .policy(policy.build().unwrap())
        .build()
        .await
        .unwrap()
}
fn company(env: &Env, id: i64) -> Env {
    env.with_context([("allowed_company_ids", Value::list([Value::Int(id)]))])
}
fn vals(items: &[(&str, Value)]) -> std::collections::BTreeMap<String, Value> {
    items
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}
async fn seed(tx: &mut Transaction) {
    let currency = erp_testkit::fixtures::seed_currency_in(tx).await.unwrap();
    tx.execute("INSERT INTO res_company (id, name, currency_id) VALUES (1, 'One', $1), (2, 'Two', $1), (3, 'Three', $1) ON CONFLICT DO NOTHING", &[&currency]).await.unwrap();
    // a user without elevation (the system principal is always elevated)
    tx.execute(
        "INSERT INTO res_users (id, name, login, active) VALUES (7, 'plain', 'plain', true) ON CONFLICT DO NOTHING",
        &[],
    )
    .await
    .unwrap();
}
async fn tag(tx: &mut Transaction, env: &Env, name: &str, parent: Option<i64>) -> Records {
    let mut values = vec![("name", Value::str(name))];
    if let Some(parent) = parent {
        values.push(("parent_id", Value::Int(parent)));
    }
    env.records("prel.tag")
        .unwrap()
        .create_values(tx, &[vals(&values)])
        .await
        .unwrap()
}
/// A holder row written straight to its table, with `map` in column `column`.
async fn holder_raw(tx: &mut Transaction, name: &str, column: &str, map: serde_json::Value) -> i64 {
    let sql =
        format!("INSERT INTO prel_holder (name, {column}) VALUES ($1, $2) RETURNING id::int8");
    tx.query(&sql, &[&name, &map]).await.unwrap()[0].get(0)
}
async fn raw(tx: &mut Transaction, column: &str, id: i64) -> serde_json::Value {
    let sql = format!("SELECT {column} FROM prel_holder WHERE id = $1");
    tx.query(&sql, &[&id]).await.unwrap()[0].get(0)
}
async fn exists(tx: &mut Transaction, id: i64) -> bool {
    !tx.query("SELECT 1 FROM prel_tag WHERE id = $1", &[&id])
        .await
        .unwrap()
        .is_empty()
}
fn error(error: Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind);
    assert_eq!(error.message(), message);
}
fn force(env: &Env) -> Env {
    env.with_context([("force_delete", Value::Bool(true))])
}

#[tokio::test]
async fn user_without_elevation_deletes_targets_and_keeps_other_company_keys() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let t1 = tag(&mut tx, &env, "t1", None).await.ids()[0];
    let t2 = tag(&mut tx, &env, "t2", None).await.ids()[0];
    let t3 = tag(&mut tx, &env, "t3", None).await.ids()[0];
    let h1 = holder_raw(&mut tx, "h1", "null_id", json!({"1": t1, "2": t2})).await;
    let h2 = holder_raw(&mut tx, "h2", "null_id", json!({"3": t1})).await;
    let h3 = holder_raw(&mut tx, "h3", "restrict_id", json!({"2": t3})).await;
    // a column holding a JSON scalar cannot name a record: the scans skip it, never fail on it
    let s1 = holder_raw(&mut tx, "s1", "null_id", json!(5)).await;
    let s2 = holder_raw(&mut tx, "s2", "restrict_id", json!("text")).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    let user = company(&tx.env(), 1);
    assert!(!user.su());
    let tags = user.records("prel.tag").unwrap();
    // the refusal is decided over every row and names neither the holder nor the target
    error(
        tags.browse([t3])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap_err(),
        ErrorKind::UserError,
        REFUSAL,
    );
    assert!(exists(&mut tx, t3).await);
    assert_eq!(raw(&mut tx, "restrict_id", h3).await, json!({"2": t3}));

    tags.browse([t1]).unwrap().unlink(&mut tx).await.unwrap();
    assert!(!exists(&mut tx, t1).await);
    assert_eq!(
        raw(&mut tx, "null_id", h1).await,
        json!({"1": null, "2": t2})
    );
    assert_eq!(raw(&mut tx, "null_id", h2).await, json!({"3": null}));
    assert_eq!(raw(&mut tx, "null_id", s1).await, json!(5));
    assert_eq!(raw(&mut tx, "restrict_id", s2).await, json!("text"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn restrict_reference_of_another_company_refuses_after_unrelated_rows() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let other = tag(&mut tx, &env, "other", None).await.ids()[0];
    let target = tag(&mut tx, &env, "target", None).await;
    let t = target.ids()[0];
    for n in 0..40 {
        holder_raw(
            &mut tx,
            &format!("noise{n}"),
            "restrict_id",
            json!({"1": other, "3": other}),
        )
        .await;
    }
    let late = holder_raw(&mut tx, "late", "restrict_id", json!({"2": t})).await;
    error(
        target.unlink(&mut tx).await.unwrap_err(),
        ErrorKind::UserError,
        REFUSAL,
    );
    assert!(exists(&mut tx, t).await);
    // `force_delete` skips the refusal; the reference is cleaned like a set-null one
    let forced = target.with_env(force(&env));
    forced.unlink(&mut tx).await.unwrap();
    assert!(!exists(&mut tx, t).await);
    assert_eq!(raw(&mut tx, "restrict_id", late).await, json!({"2": null}));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn refusal_is_decided_before_any_set_null_cleanup() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let target = tag(&mut tx, &env, "target", None).await;
    let t = target.ids()[0];
    // the set-null field comes first in registry order, the restrict field after it
    let nulled = holder_raw(&mut tx, "nulled", "null_id", json!({"1": t})).await;
    let kept = holder_raw(&mut tx, "kept", "restrict_id", json!({"1": t})).await;
    error(
        target.unlink(&mut tx).await.unwrap_err(),
        ErrorKind::UserError,
        REFUSAL,
    );
    assert!(exists(&mut tx, t).await);
    assert_eq!(raw(&mut tx, "null_id", nulled).await, json!({"1": t}));
    assert_eq!(raw(&mut tx, "restrict_id", kept).await, json!({"1": t}));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn only_a_company_dependent_fallback_blocks_deletion() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let fallback = tag(&mut tx, &env, "fallback", None).await;
    let later = tag(&mut tx, &env, "later", None).await;
    let plain = tag(&mut tx, &env, "plain", None).await;
    let set = async |tx: &mut Transaction, field: &str, id: i64| {
        ir_default::set(
            tx,
            &env,
            "prel.holder",
            field,
            &Value::Int(id),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    };
    set(&mut tx, "null_id", fallback.ids()[0]).await;
    set(&mut tx, "cascade_id", later.ids()[0]).await;
    set(&mut tx, "plain_id", plain.ids()[0]).await;
    let plain_defaults = async |tx: &mut Transaction| -> i64 {
        tx.query(
            "SELECT count(*)::int8 FROM ir_default d JOIN ir_model_fields f ON f.id = d.field_id WHERE f.model = 'prel.holder' AND f.name = 'plain_id'",
            &[],
        )
        .await
        .unwrap()[0]
            .get(0)
    };
    assert_eq!(plain_defaults(&mut tx).await, 1);

    // two fallbacks in one call: the most recent default is the one named
    let both = fallback.browse_ids(fallback.rec_ids().iter().chain(later.rec_ids()).copied());
    let id = later.ids()[0];
    error(
        both.unlink(&mut tx).await.unwrap_err(),
        ErrorKind::UserError,
        &format!(
            "Unable to delete prel.tag,{id} because it is used as the default value of cascade_id"
        ),
    );
    assert!(exists(&mut tx, fallback.ids()[0]).await && exists(&mut tx, id).await);

    // an ordinary many2one default is discarded with its target and never blocks
    plain.unlink(&mut tx).await.unwrap();
    assert!(!exists(&mut tx, plain.ids()[0]).await);
    assert_eq!(plain_defaults(&mut tx).await, 0);

    both.with_env(force(&env)).unlink(&mut tx).await.unwrap();
    assert!(!exists(&mut tx, fallback.ids()[0]).await && !exists(&mut tx, id).await);
    let left: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM ir_default d JOIN ir_model_fields f ON f.id = d.field_id WHERE f.model = 'prel.holder'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(left, 0, "the forced deletion discards the fallback rows");
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn dangling_case(with_fallback: bool) {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let live = tag(&mut tx, &env, "live", None).await.ids()[0];
    let other = tag(&mut tx, &env, "other", None).await.ids()[0];
    let parent = tag(&mut tx, &env, "parent", None).await.ids()[0];
    let child = tag(&mut tx, &env, "child", Some(parent)).await.ids()[0];
    if with_fallback {
        ir_default::set(
            &mut tx,
            &env,
            "prel.holder",
            "null_id",
            &Value::Int(live),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    let model = env.records("prel.holder").unwrap();
    let never = holder_raw(&mut tx, "never", "null_id", json!({"1": 1000})).await;
    let cascaded = holder_raw(&mut tx, "cascaded", "null_id", json!({"1": child})).await;
    let absent = holder_raw(&mut tx, "absent", "restrict_id", json!({"2": live})).await;
    // the child goes with its parent through the database cascade, as a rule outside the ORM would
    tx.flush().await.unwrap();
    tx.execute("DELETE FROM prel_tag WHERE id = $1", &[&parent])
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    for id in [never, cascaded] {
        let one = model.browse([id]).unwrap();
        assert_eq!(one.cell(&mut tx, "null_id").await.unwrap(), Cell::Null);
        assert_eq!(one.value(&mut tx, "null_id").await.unwrap(), Value::False);
        let typed = env
            .model::<fixture::Holder>()
            .unwrap()
            .browse([id])
            .unwrap();
        assert_eq!(
            typed.get(&mut tx, fixture::Holder::NULL_ID).await.unwrap(),
            None
        );
    }
    // a row with no entry of its own still reads the fallback
    let expected = if with_fallback {
        Cell::Id(live)
    } else {
        Cell::Null
    };
    assert_eq!(
        model
            .browse([absent])
            .unwrap()
            .cell(&mut tx, "null_id")
            .await
            .unwrap(),
        expected
    );
    let found = model
        .search(
            &mut tx,
            &erp_sdk::Domain::condition("null_id", erp_sdk::Operator::Eq, false).unwrap(),
        )
        .await
        .unwrap();
    for id in [never, cascaded] {
        assert!(found.ids().contains(&id), "{id} is found by a false search");
    }
    assert_eq!(found.ids().contains(&absent), !with_fallback);
    // the filtered entry is never written back: another company's write keeps the stored one
    model
        .browse([never])
        .unwrap()
        .with_env(company(&env, 2))
        .write_values(&mut tx, &vals(&[("null_id", Value::Int(other))]))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    assert_eq!(
        raw(&mut tx, "null_id", never).await,
        json!({"1": 1000, "2": other})
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn dangling_override_reads_empty() {
    dangling_case(false).await;
}

#[tokio::test]
async fn dangling_override_never_falls_back() {
    dangling_case(true).await;
}

#[tokio::test]
async fn dangling_fallback_reads_empty_and_is_found_by_a_false_search() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let fallback = tag(&mut tx, &env, "fallback", None).await.ids()[0];
    ir_default::set(
        &mut tx,
        &env,
        "prel.holder",
        "null_id",
        &Value::Int(fallback),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let row = holder_raw(&mut tx, "row", "restrict_id", json!({"1": fallback})).await;
    let model = env.records("prel.holder").unwrap();
    let one = model.browse([row]).unwrap();
    assert_eq!(
        one.cell(&mut tx, "null_id").await.unwrap(),
        Cell::Id(fallback)
    );
    // the fallback target disappears outside the ORM, the default row stays
    tx.flush().await.unwrap();
    tx.execute("DELETE FROM prel_tag WHERE id = $1", &[&fallback])
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(one.cell(&mut tx, "null_id").await.unwrap(), Cell::Null);
    assert_eq!(one.value(&mut tx, "null_id").await.unwrap(), Value::False);
    let found = model
        .search(
            &mut tx,
            &erp_sdk::Domain::condition("null_id", erp_sdk::Operator::Eq, false).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(found.ids(), [row]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn cascade_deletion_leaves_no_stale_reference_in_a_warm_cache() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let parent = tag(&mut tx, &env, "parent", None).await;
    let child = tag(&mut tx, &env, "child", Some(parent.ids()[0])).await;
    let holder = env
        .records("prel.holder")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("null_id", Value::Int(child.ids()[0]))])])
        .await
        .unwrap();
    assert_eq!(
        holder.cell(&mut tx, "null_id").await.unwrap(),
        Cell::Id(child.ids()[0])
    );
    assert_eq!(
        holder.value(&mut tx, "null_state").await.unwrap(),
        Value::str("set")
    );
    parent.unlink(&mut tx).await.unwrap();
    assert!(!exists(&mut tx, child.ids()[0]).await);
    assert_eq!(holder.cell(&mut tx, "null_id").await.unwrap(), Cell::Null);
    // what depends on the forgotten map follows it
    assert_eq!(
        holder.value(&mut tx, "null_state").await.unwrap(),
        Value::str("empty")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn rolled_back_deletion_keeps_the_reference() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let target = tag(&mut tx, &env, "target", None).await;
    let t = target.ids()[0];
    let holder = env
        .records("prel.holder")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("null_id", Value::Int(t))])])
        .await
        .unwrap();
    assert_eq!(holder.cell(&mut tx, "null_id").await.unwrap(), Cell::Id(t));
    let doomed = target.clone();
    let outcome = tx
        .savepoint(async |tx: &mut Transaction| -> erp_fields::Result<()> {
            doomed.unlink(tx).await?;
            Err(Error::user("roll back"))
        })
        .await;
    error(outcome.unwrap_err(), ErrorKind::UserError, "roll back");
    assert!(exists(&mut tx, t).await);
    assert_eq!(holder.cell(&mut tx, "null_id").await.unwrap(), Cell::Id(t));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn dependents_follow_the_cleared_reference_and_cascade_means_set_null() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let target = tag(&mut tx, &env, "target", None).await;
    let t = target.ids()[0];
    let holder = env
        .records("prel.holder")
        .unwrap()
        .create_values(
            &mut tx,
            &[vals(&[
                ("null_id", Value::Int(t)),
                ("cascade_id", Value::Int(t)),
            ])],
        )
        .await
        .unwrap();
    assert_eq!(
        holder.value(&mut tx, "null_state").await.unwrap(),
        Value::str("set")
    );
    tx.flush().await.unwrap();
    target.unlink(&mut tx).await.unwrap();
    assert_eq!(
        holder.value(&mut tx, "null_state").await.unwrap(),
        Value::str("empty")
    );
    assert_eq!(
        holder.cell(&mut tx, "cascade_id").await.unwrap(),
        Cell::Null
    );
    // the holder survives: `cascade` on a company map clears the entry, it deletes nothing
    assert_eq!(holder.exists(&mut tx).await.unwrap().ids(), holder.ids());
    assert_eq!(
        raw(&mut tx, "cascade_id", holder.ids()[0]).await,
        json!({"1": null})
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

static EXOTIC: AddonDescriptor = AddonDescriptor {
    name: "property_relations_exotic",
    depends: &["base"],
    exact_only: false,
};

fn exotic_decls(eventful: bool) -> [ModelDecl; 2] {
    let holder = ModelDecl::new("property_relations_exotic", "prel.xholder")
        .table("prel_xholder")
        .eventful(eventful);
    let fields: &'static [FieldDecl] = if eventful {
        const { &[FieldDecl::many2one("tag_id", "prel.xtag").company_dependent(true)] }
    } else {
        const {
            &[
                FieldDecl::char("name").tracking(10),
                FieldDecl::many2one("tag_id", "prel.xtag").company_dependent(true),
            ]
        }
    };
    [
        ModelDecl::new("property_relations_exotic", "prel.xtag")
            .table("prel_xtag")
            .fields(const { &[FieldDecl::char("name")] }),
        holder.fields(fields),
    ]
}

async fn exotic_harness(eventful: bool) -> Harness {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON, &EXOTIC])
        .with_decls(exotic_decls(eventful))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    if eventful {
        tx.install_event_schema().await.unwrap();
    }
    seed(&mut tx).await;
    tx.commit().await.unwrap();
    h
}

/// records no change; the company-dependent field itself is not tracked, tracked properties stay
/// refused until)
#[tokio::test]
async fn holder_model_with_a_tracked_field_is_no_longer_refused() {
    let h = exotic_harness(false).await;
    let mut tx = h.begin().await.unwrap();
    let env = company(&tx.env(), 1);
    let tags = env.records("prel.xtag").unwrap();
    let target = tags
        .create_values(&mut tx, &[vals(&[("name", Value::str("t"))])])
        .await
        .unwrap();
    let t = target.ids()[0];
    let holder = env
        .records("prel.xholder")
        .unwrap()
        .create_values(
            &mut tx,
            &[vals(&[
                ("name", Value::str("tracked")),
                ("tag_id", Value::Int(t)),
            ])],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    // what creating the holder recorded is not what this test is about
    tx.take_tracked_changes().await.unwrap();
    target.unlink(&mut tx).await.unwrap();
    assert!(tx.take_tracked_changes().await.unwrap().is_empty());
    assert_eq!(holder.cell(&mut tx, "tag_id").await.unwrap(), Cell::Null);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn eventful_holder_keeps_the_ancillary_refusal() {
    let h = exotic_harness(true).await;
    let mut tx = h
        .orm()
        .begin_opts(
            BeginOptions::new(SUPERUSER_ID)
                .operation_identity(OperationIdentity::generate().unwrap()),
        )
        .await
        .unwrap();
    let env = company(&tx.env(), 1);
    let target = env
        .records("prel.xtag")
        .unwrap()
        .create_values(&mut tx, &[vals(&[("name", Value::str("t"))])])
        .await
        .unwrap();
    error(
        target.unlink(&mut tx).await.unwrap_err(),
        ErrorKind::Programming,
        "a later release S8: event-relevant ancillary delete effects on model 'prel.xholder' are not implemented",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn force_delete_from_a_user_without_elevation_changes_nothing() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let held = tag(&mut tx, &env, "held", None).await.ids()[0];
    let default = tag(&mut tx, &env, "default", None).await.ids()[0];
    let row = holder_raw(&mut tx, "row", "restrict_id", json!({"1": held})).await;
    ir_default::set(
        &mut tx,
        &env,
        "prel.holder",
        "null_id",
        &Value::Int(default),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    let user = force(&company(&tx.env(), 1));
    assert!(!user.su());
    let tags = user.records("prel.tag").unwrap();
    error(
        tags.browse([held])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap_err(),
        ErrorKind::UserError,
        REFUSAL,
    );
    error(
        tags.browse([default])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap_err(),
        ErrorKind::UserError,
        &format!(
            "Unable to delete prel.tag,{default} because it is used as the default value of null_id"
        ),
    );
    assert!(exists(&mut tx, held).await && exists(&mut tx, default).await);
    assert_eq!(raw(&mut tx, "restrict_id", row).await, json!({"1": held}));
    let defaults: i64 = tx
        .query(
            "SELECT count(*)::int8 FROM ir_default d JOIN ir_model_fields f ON f.id = d.field_id WHERE f.model = 'prel.holder' AND f.name = 'null_id'",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(defaults, 1);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn record_rules_do_not_hide_references_from_deletion_and_reads_stay_filtered() {
    let h = ruled_harness(false).await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let restricted = tag(&mut tx, &env, "restricted", None).await.ids()[0];
    let cleared = tag(&mut tx, &env, "cleared", None).await.ids()[0];
    let hidden_restrict =
        holder_raw(&mut tx, "hidden", "restrict_id", json!({"1": restricted})).await;
    let hidden_null = holder_raw(&mut tx, "hidden", "null_id", json!({"1": cleared})).await;
    let dangling = holder_raw(&mut tx, "dangling", "null_id", json!({"1": 1000})).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    let user = company(&tx.env(), 1);
    let tags = user.records("prel.tag").unwrap();
    error(
        tags.browse([restricted])
            .unwrap()
            .unlink(&mut tx)
            .await
            .unwrap_err(),
        ErrorKind::UserError,
        REFUSAL,
    );
    assert!(exists(&mut tx, restricted).await);
    assert_eq!(
        raw(&mut tx, "restrict_id", hidden_restrict).await,
        json!({"1": restricted})
    );
    tags.browse([cleared])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap();
    assert_eq!(
        raw(&mut tx, "null_id", hidden_null).await,
        json!({"1": null})
    );

    // a restricted read goes through the filtered statement of the record rule
    let holders = user.records("prel.holder").unwrap();
    assert_eq!(
        holders
            .browse([dangling])
            .unwrap()
            .cell(&mut tx, "null_id")
            .await
            .unwrap(),
        Cell::Null
    );
    // an opened group reading the field runs without error; the cell below may reload through
    // the read path, so this does not prove the group statement filtered (review round 2)
    tx.invalidate_all().await.unwrap();
    let mut args = WebReadGroup::new(
        erp_sdk::Domain::condition("id", erp_sdk::Operator::Eq, dangling).unwrap(),
        ["name"],
    );
    args.auto_unfold = true;
    args.unfold_read_specification = Some(Spec::new().field("null_id"));
    holders.web_read_group(&mut tx, args).await.unwrap();
    assert_eq!(
        holders
            .browse([dangling])
            .unwrap()
            .cell(&mut tx, "null_id")
            .await
            .unwrap(),
        Cell::Null
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_user_without_unlink_access_to_the_target_changes_no_map() {
    let h = ruled_harness(true).await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let target = tag(&mut tx, &env, "target", None).await.ids()[0];
    let row = holder_raw(&mut tx, "row", "null_id", json!({"1": target})).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    let user = company(&tx.env(), 1);
    let refused = user
        .records("prel.tag")
        .unwrap()
        .browse([target])
        .unwrap()
        .unlink(&mut tx)
        .await
        .unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Access);
    assert_eq!(
        refused.message(),
        "You are not allowed to unlink 'prel.tag' (prel.tag) records."
    );
    assert!(exists(&mut tx, target).await);
    assert_eq!(raw(&mut tx, "null_id", row).await, json!({"1": target}));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn drafts_read_references_only_through_readable_origins() {
    let h = ruled_harness(false).await;
    let mut tx = h.begin().await.unwrap();
    seed(&mut tx).await;
    let env = company(&tx.env(), 1);
    let target = tag(&mut tx, &env, "target", None).await.ids()[0];
    let shown = holder_raw(&mut tx, "shown", "null_id", json!({"1": target})).await;
    let hidden = holder_raw(&mut tx, "hidden", "null_id", json!({"1": target})).await;
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(7).await.unwrap();
    let user = company(&tx.env(), 1);
    let holders = user.records("prel.holder").unwrap();
    let origin = |id: i64| Some(erp_fields::RecId::from_real(id).unwrap());
    // a readable origin: the draft reads its reference, and the compute over it follows the draft
    let draft = tx
        .new_record(&holders, erp_orm::RawVals::new(), origin(shown), None)
        .await
        .unwrap();
    assert_eq!(
        draft.cell(&mut tx, "null_id").await.unwrap(),
        Cell::Id(target)
    );
    assert_eq!(
        draft.value(&mut tx, "null_state").await.unwrap(),
        Value::str("set")
    );
    draft
        .write_values(&mut tx, &vals(&[("null_id", Value::False)]))
        .await
        .unwrap();
    assert_eq!(draft.cell(&mut tx, "null_id").await.unwrap(), Cell::Null);
    assert_eq!(
        draft.value(&mut tx, "null_state").await.unwrap(),
        Value::str("empty")
    );
    // a draft never points at an unsaved record through a company map
    let unsaved = tx
        .new_record(
            &user.records("prel.tag").unwrap(),
            erp_orm::RawVals::new(),
            None,
            None,
        )
        .await
        .unwrap();
    let link = erp_orm::RawVals::new().set_cell("null_id", Cell::Id(unsaved.rec_ids()[0].get()));
    let result = tx
        .savepoint(async |tx| draft.write(tx, link.clone()).await)
        .await;
    error(
        result.unwrap_err(),
        ErrorKind::Validation,
        "prel.holder.null_id: a company-dependent many2one to an unsaved record is not supported until a later release",
    );
    // a hidden origin: the draft's read is refused like the stored record's, twice
    let secret = tx
        .new_record(&holders, erp_orm::RawVals::new(), origin(hidden), None)
        .await
        .unwrap();
    let stored = holders
        .browse([hidden])
        .unwrap()
        .cell(&mut tx, "null_id")
        .await
        .unwrap_err();
    for _ in 0..2 {
        let refused = secret.cell(&mut tx, "null_id").await.unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Access);
        assert_eq!(refused.message(), stored.message());
    }
    // nothing of the draft reached the table
    tx.flush().await.unwrap();
    assert_eq!(raw(&mut tx, "null_id", shown).await, json!({"1": target}));
    drop(tx);
    h.cleanup().await.unwrap();
}
