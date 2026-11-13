//! Native base contact-tag defaults and archived many2many relations.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_orm::{Operation, PrincipalBinding};
use erp_registry::ModelSpec;
use erp_rpc::{Caller, RpcCall, RpcHost};
use erp_sdk::{
    Cell, Command, Domain, Error, ErrorKind, RawVals, RecId, Records, Result, Transaction, Value,
};
use erp_test_models::base_users;
use erp_testkit::{Harness, RpcHarness, RulesPolicy, TestRules};
use serde_json::json;

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap()
}

async fn tag(tx: &mut Transaction, name: &str) -> Records {
    tx.env()
        .records("res.partner.category")
        .unwrap()
        .create(
            tx,
            vec![RawVals::new().set_cell("name", Cell::Str(name.into()))],
        )
        .await
        .unwrap()
}

async fn contact(tx: &mut Transaction, name: &str, tags: &[i64]) -> Records {
    tx.env()
        .records("res.partner")
        .unwrap()
        .create(
            tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str(name.into()))
                    .set_commands(
                        "category_id",
                        vec![Command::Set(
                            tags.iter()
                                .map(|id| RecId::from_real(*id).unwrap())
                                .collect(),
                        )],
                    ),
            ],
        )
        .await
        .unwrap()
}

async fn tag_ids(tx: &mut Transaction, contact: &Records) -> Vec<i64> {
    let mut ids = contact.related(tx, "category_id").await.unwrap().ids();
    ids.sort_unstable();
    ids
}

fn sorted(ids: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let mut ids: Vec<_> = ids.into_iter().collect();
    ids.sort_unstable();
    ids
}

fn default_set(ids: &[i64]) -> Value {
    Value::list([Value::list([
        Value::Int(6),
        Value::Int(0),
        Value::list(ids.iter().copied().map(Value::Int)),
    ])])
}

#[tokio::test]
async fn spec_partner_072_context_tag_default_and_explicit_values() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let first = tag(&mut tx, "Context tag").await.ids()[0];
    let second = tag(&mut tx, "Explicit tag").await.ids()[0];
    let partners = tx
        .env()
        .records("res.partner")
        .unwrap()
        .with_context([("category_id", Value::Int(first))]);
    let defaults = partners
        .default_get(&mut tx, &["category_id"])
        .await
        .unwrap();
    assert_eq!(defaults.get("category_id"), Some(&default_set(&[first])));
    assert_eq!(defaults.len(), 1);
    assert!(
        !partners
            .default_get(&mut tx, &["name"])
            .await
            .unwrap()
            .contains_key("category_id")
    );
    let created = partners
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", Cell::Str("Defaulted".into()))],
        )
        .await
        .unwrap();
    assert_eq!(tag_ids(&mut tx, &created).await, [first]);
    let explicit = partners
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Explicit".into()))
                    .set_commands(
                        "category_id",
                        vec![Command::Set(vec![RecId::from_real(second).unwrap()])],
                    ),
            ],
        )
        .await
        .unwrap();
    assert_eq!(tag_ids(&mut tx, &explicit).await, [second]);
    let empty = partners
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", Cell::Str("Explicit empty".into()))
                    .set_commands("category_id", vec![Command::Set(vec![])]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(tag_ids(&mut tx, &empty).await, Vec::<i64>::new());
    let higher = partners.with_context([("default_category_id", default_set(&[second]))]);
    assert_eq!(
        higher
            .default_get(&mut tx, &["category_id"])
            .await
            .unwrap()
            .get("category_id"),
        Some(&default_set(&[second]))
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn context_tag_wrong_type_is_not_silently_ignored() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for value in [
        Value::str("not-an-id"),
        Value::Int(0),
        Value::Int(-1),
        Value::list([Value::Int(1)]),
    ] {
        let partners = tx
            .env()
            .records("res.partner")
            .unwrap()
            .with_context([("category_id", value)]);
        let error = partners
            .default_get(&mut tx, &["category_id"])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "res.partner.category_id: context category_id must be a positive integer or false"
        );
    }
    let partners = tx
        .env()
        .records("res.partner")
        .unwrap()
        .with_context([("category_id", Value::False)]);
    assert_eq!(
        partners
            .default_get(&mut tx, &["category_id"])
            .await
            .unwrap()
            .get("category_id"),
        Some(&default_set(&[]))
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

async fn typed_archive_sequence() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t1 = tag(&mut tx, "T1").await;
    let t2 = tag(&mut tx, "T2").await;
    let t3 = tag(&mut tx, "T3").await;
    let ids = [t1.ids()[0], t2.ids()[0], t3.ids()[0]];
    let partner = contact(&mut tx, "Tagged contact", &ids[..2]).await;
    assert_eq!(
        tag_ids(&mut tx, &partner).await,
        sorted(ids[..2].iter().copied())
    );
    // Warm both sides before archiving to exercise reciprocal cache invalidation.
    assert_eq!(
        t2.related(&mut tx, "partner_ids").await.unwrap().ids(),
        partner.ids()
    );
    t2.write(
        &mut tx,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    assert_eq!(tag_ids(&mut tx, &partner).await, [ids[0]]);
    let replace = RawVals::new().set_commands(
        "category_id",
        vec![Command::Set(vec![t1.rec_ids()[0], t3.rec_ids()[0]])],
    );
    partner.write(&mut tx, replace.clone()).await.unwrap();
    partner.write(&mut tx, replace).await.unwrap();
    assert_eq!(tag_ids(&mut tx, &partner).await, sorted([ids[0], ids[2]]));
    assert_eq!(
        tag_ids(
            &mut tx,
            &partner.with_context([("active_test", Value::Bool(false))])
        )
        .await,
        sorted(ids)
    );
    t2.write(&mut tx, RawVals::new().set_cell("active", Cell::Bool(true)))
        .await
        .unwrap();
    assert_eq!(tag_ids(&mut tx, &partner).await, sorted(ids));
    tx.invalidate_all().await.unwrap();
    assert_eq!(tag_ids(&mut tx, &partner).await, sorted(ids));
    assert_eq!(
        t2.related(&mut tx, "partner_ids").await.unwrap().ids(),
        partner.ids()
    );
    let partner_id = partner.ids()[0];
    tx.commit().await.unwrap();
    let mut fresh = h.begin().await.unwrap();
    let partner = fresh
        .env()
        .records("res.partner")
        .unwrap()
        .browse([partner_id])
        .unwrap();
    assert_eq!(tag_ids(&mut fresh, &partner).await, sorted(ids));
    // An explicit non-active-test replacement may remove the archived relation.
    let t2 = fresh
        .env()
        .records("res.partner.category")
        .unwrap()
        .browse([ids[1]])
        .unwrap();
    t2.write(
        &mut fresh,
        RawVals::new().set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    partner
        .with_context([("active_test", Value::Bool(false))])
        .write(
            &mut fresh,
            RawVals::new().set_commands(
                "category_id",
                vec![Command::Set(vec![RecId::from_real(ids[0]).unwrap()])],
            ),
        )
        .await
        .unwrap();
    t2.write(
        &mut fresh,
        RawVals::new().set_cell("active", Cell::Bool(true)),
    )
    .await
    .unwrap();
    assert_eq!(tag_ids(&mut fresh, &partner).await, [ids[0]]);
    fresh.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[derive(Debug)]
struct TagRules;

impl TestRules for TagRules {
    fn check_model(
        &self,
        binding: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        let uid = binding.principal().subject().get();
        if uid == support::rpc::ALICE || (uid == support::rpc::BOB && op == Operation::Read) {
            Ok(())
        } else {
            Err(Error::access(format!(
                "You are not allowed to {} '{}' ({}) records.",
                op.as_str(),
                model.description,
                model.name
            )))
        }
    }

    fn access_domain(
        &self,
        _binding: &PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<Domain> {
        // This fixture restricts operations in check_model; it has no record-level exclusions.
        Ok(Domain::TRUE)
    }
}

struct TagHost;
impl RpcHost for TagHost {
    fn language_active(&self, _code: &str) -> Result<bool> {
        Ok(false)
    }
    fn has_capability(
        &self,
        _principal: &erp_orm::Principal,
        _capability: &erp_orm::Capability,
    ) -> Result<bool> {
        Ok(false)
    }
}

async fn rpc(
    h: &RpcHarness,
    model: &str,
    method: &str,
    body: serde_json::Value,
    caller: Caller,
) -> erp_rpc::RpcResponse {
    h.service()
        .call(RpcCall::json2(
            &format!("/json/2/{model}/{method}"),
            serde_json::to_vec(&body).unwrap(),
            caller,
        ))
        .await
}

async fn rpc_archive_sequence() {
    let h = RpcHarness::builder(
        Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON]),
        RulesPolicy::new(TagRules).unwrap(),
    )
    .host(|_| TagHost)
    .build()
    .await
    .unwrap();
    let mut setup = h.harness().begin().await.unwrap();
    setup.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'alice', 'alice', true, false), (3, 'bob', 'bob', true, false); SELECT setval(pg_get_serial_sequence('res_users', 'id'), 3)").await.unwrap();
    let mut ids = Vec::new();
    for name in ["T1", "T2", "T3"] {
        ids.push(tag(&mut setup, name).await.ids()[0]);
    }
    let partner = contact(&mut setup, "RPC contact", &ids[..2]).await.ids()[0];
    setup.commit().await.unwrap();
    let read = |caller| {
        rpc(
            &h,
            "res.partner",
            "read",
            json!({"ids":[partner],"fields":["category_id"]}),
            caller,
        )
    };
    support::rpc::ok(
        &rpc(
            &h,
            "res.partner.category",
            "write",
            json!({"ids":[ids[1]],"vals":{"active":false}}),
            support::rpc::alice(),
        )
        .await,
    );
    assert_eq!(
        support::rpc::ok(&read(support::rpc::alice()).await),
        json!([{"id":partner,"category_id":[ids[0]]}])
    );
    let denied = rpc(
        &h,
        "res.partner",
        "write",
        json!({"ids":[partner],"vals":{"category_id":[[6,0,[ids[0],ids[2]]]]}}),
        support::rpc::user(support::rpc::BOB),
    )
    .await;
    support::rpc::assert_error(
        &denied,
        403,
        "erp.exceptions.AccessError",
        "You are not allowed to write 'Contact' (res.partner) records.",
    );
    assert_eq!(
        support::rpc::ok(&read(support::rpc::alice()).await),
        json!([{"id":partner,"category_id":[ids[0]]}])
    );
    support::rpc::ok(
        &rpc(
            &h,
            "res.partner",
            "write",
            json!({"ids":[partner],"vals":{"category_id":[[6,0,[ids[0],ids[2]]]]}}),
            support::rpc::alice(),
        )
        .await,
    );
    support::rpc::ok(
        &rpc(
            &h,
            "res.partner.category",
            "write",
            json!({"ids":[ids[1]],"vals":{"active":true}}),
            support::rpc::alice(),
        )
        .await,
    );
    let response = support::rpc::ok(&read(support::rpc::alice()).await);
    let actual: Vec<i64> = response[0]["category_id"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert_eq!(sorted(actual), sorted(ids));
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_partner_080_archived_tags_hidden_and_preserved_typed_and_rpc() {
    typed_archive_sequence().await;
    rpc_archive_sequence().await;
}

#[tokio::test]
async fn spec_partner_070_category_loop_rpc_preserves_paths() {
    let h = RpcHarness::builder(
        Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON]),
        RulesPolicy::new(TagRules).unwrap(),
    )
    .host(|_| TagHost)
    .build()
    .await
    .unwrap();
    let mut setup = h.harness().begin().await.unwrap();
    setup.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'alice', 'alice', true, false); SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)").await.unwrap();
    let customer = tag(&mut setup, "Customer").await;
    let vip = tag(&mut setup, "VIP").await;
    let gold = tag(&mut setup, "Gold").await;
    vip.write(
        &mut setup,
        RawVals::new().set_cell("parent_id", Cell::Id(customer.ids()[0])),
    )
    .await
    .unwrap();
    gold.write(
        &mut setup,
        RawVals::new().set_cell("parent_id", Cell::Id(vip.ids()[0])),
    )
    .await
    .unwrap();
    let all_ids = [customer.ids()[0], vip.ids()[0], gold.ids()[0]];
    let all = setup
        .env()
        .records("res.partner.category")
        .unwrap()
        .browse(all_ids)
        .unwrap();
    let before_paths = all.values(&mut setup, "parent_path").await.unwrap();
    let before_parents = all.values(&mut setup, "parent_id").await.unwrap();
    for (child, parent) in [(&customer, gold.ids()[0]), (&vip, vip.ids()[0])] {
        let error = setup
            .savepoint(async |tx| {
                child
                    .write(tx, RawVals::new().set_cell("parent_id", Cell::Id(parent)))
                    .await
            })
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UserError);
        assert_eq!(
            error.message(),
            "You are creating a loop in your 'Partner Tag' records. A record cannot be a child of itself or one of its own sub-items. Please select a different parent."
        );
        assert_eq!(
            all.values(&mut setup, "parent_path").await.unwrap(),
            before_paths
        );
        assert_eq!(
            all.values(&mut setup, "parent_id").await.unwrap(),
            before_parents
        );
    }
    setup.commit().await.unwrap();
    let before = support::rpc::ok(
        &rpc(
            &h,
            "res.partner.category",
            "read",
            json!({"ids":all_ids,"fields":["parent_id","parent_path"]}),
            support::rpc::alice(),
        )
        .await,
    );
    for (child, parent) in [
        (customer.ids()[0], gold.ids()[0]),
        (vip.ids()[0], vip.ids()[0]),
    ] {
        let response = rpc(
            &h,
            "res.partner.category",
            "write",
            json!({"ids":[child],"vals":{"parent_id":parent}}),
            support::rpc::alice(),
        )
        .await;
        support::rpc::assert_error(
            &response,
            422,
            "erp.exceptions.UserError",
            "You are creating a loop in your 'Partner Tag' records. A record cannot be a child of itself or one of its own sub-items. Please select a different parent.",
        );
        let after = support::rpc::ok(
            &rpc(
                &h,
                "res.partner.category",
                "read",
                json!({"ids":all_ids,"fields":["parent_id","parent_path"]}),
                support::rpc::alice(),
            )
            .await,
        );
        assert_eq!(after, before);
    }
    h.cleanup().await.unwrap();
}

#[derive(Debug)]
struct TagReadDeniedRules;

impl TestRules for TagReadDeniedRules {
    fn check_model(
        &self,
        binding: &PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> Result<()> {
        if model.name == "res.partner.category" && op == Operation::Read {
            return Err(Error::access(
                "You are not allowed to read 'Partner Tag' (res.partner.category) records.",
            ));
        }
        TagRules.check_model(binding, model, op)
    }

    fn access_domain(
        &self,
        _binding: &PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<Domain> {
        Ok(Domain::TRUE)
    }
}

#[tokio::test]
async fn context_tag_default_does_not_bypass_target_read_access() {
    let h = Harness::builder([&base_users::ADDON, &erp_addon_base::ADDON])
        .policy(RulesPolicy::new(TagReadDeniedRules).unwrap())
        .build()
        .await
        .unwrap();
    let mut setup = h.begin().await.unwrap();
    setup.batch("INSERT INTO res_users (id, name, login, active, share) VALUES (2, 'alice', 'alice', true, false); SELECT setval(pg_get_serial_sequence('res_users', 'id'), 2)").await.unwrap();
    let tag_id = tag(&mut setup, "Unreadable tag").await.ids()[0];
    setup.commit().await.unwrap();
    let mut tx = h.begin_as(support::rpc::ALICE).await.unwrap();
    let partners = tx
        .env()
        .records("res.partner")
        .unwrap()
        .with_context([("category_id", Value::Int(tag_id))]);
    let error = tx
        .savepoint(async |tx| {
            partners
                .create(
                    tx,
                    vec![RawVals::new().set_cell("name", Cell::Str("Refused contact".into()))],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.message(),
        "Failed to write field res.partner.category_id\nYou are not allowed to read 'Partner Tag' (res.partner.category) records."
    );
    let contacts = tx
        .env()
        .records("res.partner")
        .unwrap()
        .search(
            &mut tx,
            &Domain::condition("name", erp_sdk::Operator::Eq, "Refused contact").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(contacts.ids(), Vec::<i64>::new());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn context_tag_default_missing_positive_target_refuses_create() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let missing_id = 987_654_321;
    let partners = tx
        .env()
        .records("res.partner")
        .unwrap()
        .with_context([("category_id", Value::Int(missing_id))]);
    // Defaults are proposed values. Existence is enforced by the ordinary relation
    // write, not by a privileged or extra lookup in the category adapter.
    assert_eq!(
        partners
            .default_get(&mut tx, &["category_id"])
            .await
            .unwrap()
            .get("category_id"),
        Some(&default_set(&[missing_id]))
    );
    let error = tx
        .savepoint(async |tx| {
            partners
                .create(
                    tx,
                    vec![RawVals::new().set_cell("name", Cell::Str("Missing tag contact".into()))],
                )
                .await
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.sqlstate(), Some("23503"));
    assert_eq!(
        error.message(),
        "The operation cannot be completed: insert or update on table \"res_partner_res_partner_category_rel\" violates foreign key constraint \"res_partner_res_partner_category_rel_category_id_fkey\""
    );
    let all_partners = tx.env().records("res.partner").unwrap();
    let absent = all_partners
        .search(
            &mut tx,
            &Domain::condition("name", erp_sdk::Operator::Eq, "Missing tag contact").unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(absent.ids(), Vec::<i64>::new());
    let surviving = contact(&mut tx, "Usable after missing tag refusal", &[]).await;
    assert_eq!(tag_ids(&mut tx, &surviving).await, Vec::<i64>::new());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

fn independent_tag_node(h: &Harness) -> erp_orm::Orm {
    use erp_testkit::deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
    let manager = Manager::from_config(
        h.db().config(),
        erp_testkit::tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let pool = Pool::builder(manager)
        .max_size(2)
        .runtime(Runtime::Tokio1)
        .build()
        .unwrap();
    erp_orm::Orm::builder(std::sync::Arc::clone(h.registry()), pool)
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .policy(erp_orm::StaticPolicy::AllowAll)
        .build()
        .unwrap()
}

#[tokio::test]
async fn category_paths_and_hidden_links_survive_cross_node_commit_and_abort() {
    let h = harness().await;
    let node = independent_tag_node(&h);
    let mut a = h.begin().await.unwrap();
    let root = tag(&mut a, "Cross-node root").await;
    let child = tag(&mut a, "Cross-node child").await;
    child
        .write(
            &mut a,
            RawVals::new().set_cell("parent_id", Cell::Id(root.ids()[0])),
        )
        .await
        .unwrap();
    let partner = contact(&mut a, "Cross-node contact", &child.ids()).await;
    let partner_id = partner.ids()[0];
    let root_id = root.ids()[0];
    let child_id = child.ids()[0];
    a.commit().await.unwrap();
    let mut b = node.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let p = b
        .env()
        .records("res.partner")
        .unwrap()
        .browse([partner_id])
        .unwrap();
    let c = b
        .env()
        .records("res.partner.category")
        .unwrap()
        .browse([child_id])
        .unwrap();
    assert_eq!(tag_ids(&mut b, &p).await, [child_id]);
    assert_eq!(
        c.value(&mut b, "parent_path").await.unwrap(),
        Value::str(format!("{root_id}/{child_id}/"))
    );
    b.rollback().await.unwrap();
    let mut a = h.begin().await.unwrap();
    let c = a
        .env()
        .records("res.partner.category")
        .unwrap()
        .browse([child_id])
        .unwrap();
    c.write(
        &mut a,
        RawVals::new()
            .set_cell("parent_id", Cell::Null)
            .set_cell("active", Cell::Bool(false)),
    )
    .await
    .unwrap();
    let p = a
        .env()
        .records("res.partner")
        .unwrap()
        .browse([partner_id])
        .unwrap();
    p.write(
        &mut a,
        RawVals::new().set_commands("category_id", vec![Command::Set(vec![])]),
    )
    .await
    .unwrap();
    a.commit().await.unwrap();
    let mut b = node.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let p = b
        .env()
        .records("res.partner")
        .unwrap()
        .browse([partner_id])
        .unwrap();
    let c = b
        .env()
        .records("res.partner.category")
        .unwrap()
        .browse([child_id])
        .unwrap();
    assert_eq!(tag_ids(&mut b, &p).await, Vec::<i64>::new());
    assert_eq!(
        c.value(&mut b, "parent_path").await.unwrap(),
        Value::str(format!("{child_id}/"))
    );
    b.rollback().await.unwrap();
    let mut a = h.begin().await.unwrap();
    let c = a
        .env()
        .records("res.partner.category")
        .unwrap()
        .browse([child_id])
        .unwrap();
    c.write(&mut a, RawVals::new().set_cell("active", Cell::Bool(true)))
        .await
        .unwrap();
    a.commit().await.unwrap();
    let mut aborted = h.begin().await.unwrap();
    let c = aborted
        .env()
        .records("res.partner.category")
        .unwrap()
        .browse([child_id])
        .unwrap();
    c.write(
        &mut aborted,
        RawVals::new().set_cell("parent_id", Cell::Id(root_id)),
    )
    .await
    .unwrap();
    aborted.rollback().await.unwrap();
    drop(node);
    let restarted = independent_tag_node(&h);
    let mut b = restarted.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let p = b
        .env()
        .records("res.partner")
        .unwrap()
        .browse([partner_id])
        .unwrap();
    let c = b
        .env()
        .records("res.partner.category")
        .unwrap()
        .browse([child_id])
        .unwrap();
    assert_eq!(tag_ids(&mut b, &p).await, [child_id]);
    assert_eq!(
        c.value(&mut b, "parent_path").await.unwrap(),
        Value::str(format!("{child_id}/"))
    );
    b.rollback().await.unwrap();
    drop(restarted);
    h.cleanup().await.unwrap();
}
