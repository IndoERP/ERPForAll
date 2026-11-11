//! Field-level access on every ORM path: and
//! the `#[field_access]` override chain. The policy is the testkit's `FieldPolicy`, which decides
//! by the fields' `groups` text and the memberships a test states; the group tables and the
//! compiled masks come with.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_orm::FieldOp;
use erp_sdk::{
    Domain, Error, ErrorKind, Operator, ReadGroup, Records, SearchOptions, Transaction, Value,
};
use erp_test_models::base_users;
use erp_test_models::test_field_access::{self, test_field_access_ext};
use erp_testkit::{FieldPolicy, Harness, assert_query_count};
use support::{map, vmap};

const ITEM: &str = "test_fa.item";
const DESCRIPTION: &str = "Object For Test Access Right";

fn s(text: &str) -> Value {
    Value::str(text)
}

fn int(n: i64) -> Value {
    Value::Int(n)
}

fn denied(model: &str, description: &str, field: &str, op: &str, detail: Option<&str>) -> String {
    let mut text = format!(
        "You do not have enough rights to access the field \"{field}\" on {description} \
         ({model}). Please contact your system administrator.\n\nOperation: {op}"
    );
    if let Some(detail) = detail {
        text.push_str(detail);
    }
    text
}

#[track_caller]
fn assert_denied(error: &Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    assert_eq!(error.to_string(), message);
}

async fn harness(policy: FieldPolicy) -> Harness {
    Harness::builder([&base_users::ADDON, &test_field_access::ADDON])
        .policy(policy)
        .build()
        .await
        .expect("harness")
}

async fn user(tx: &mut Transaction, login: &str) -> i64 {
    let created = tx
        .env()
        .records("res.users")
        .unwrap()
        .create_values(tx, &[map(&[("name", s(login)), ("login", s(login))])])
        .await
        .unwrap();
    created.ids()[0]
}

/// One item and one bank line, created as the superuser.
struct Fixture {
    item: i64,
    parent: i64,
    bank: i64,
}

async fn fixture(tx: &mut Transaction) -> Fixture {
    let items = tx.env().records(ITEM).unwrap();
    let parent = items
        .create_values(tx, &[map(&[("name", s("parent")), ("forbidden2", int(9))])])
        .await
        .unwrap();
    let item = items
        .create_values(
            tx,
            &[map(&[
                ("name", s("item")),
                ("val", int(1)),
                ("forbidden", int(2)),
                ("forbidden2", int(5)),
                ("forbidden3", int(58)),
                ("email", s("a@example.com")),
                ("parent_id", int(parent.ids()[0])),
            ])],
        )
        .await
        .unwrap();
    let bank = tx
        .env()
        .records("test_fa.bank")
        .unwrap()
        .create_values(
            tx,
            &[map(&[
                ("item_id", int(item.ids()[0])),
                ("acc_number", s("BE01")),
                ("secret", s("pin")),
            ])],
        )
        .await
        .unwrap();
    Fixture {
        item: item.ids()[0],
        parent: parent.ids()[0],
        bank: bank.ids()[0],
    }
}

fn items_as(tx: &Transaction, uid: i64) -> Records {
    tx.env().records(ITEM).unwrap().with_user(uid)
}

fn cmd(code: i64, id: i64, values: Value) -> Value {
    Value::list([int(code), int(id), values])
}

#[tokio::test]
async fn spec_sec_022_field_groups_block_read_write_commands() {
    let policy = FieldPolicy::new().enforce_groups();
    let h = harness(policy.clone()).await;
    let mut tx = h.begin().await.unwrap();
    let fx = fixture(&mut tx).await;
    let uid = user(&mut tx, "demo").await;
    let items = items_as(&tx, uid);
    let item = items.browse([fx.item]).unwrap();
    let read_email = denied(ITEM, DESCRIPTION, "email", "read", None);
    let write_email = denied(ITEM, DESCRIPTION, "email", "write", None);

    // search_fetch, fetch, read, attribute access: Access, whatever the entry point; no statement
    // is spent on the refusal
    let found = assert_query_count!(tx, <= 0, {
        items
            .search_with(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::new().fetch_fields(["email"]),
            )
            .await
            .unwrap_err()
    });
    assert_denied(&found, &read_email);
    assert_denied(
        &item.fetch(&mut tx, Some(&["email"])).await.unwrap_err(),
        &read_email,
    );
    assert_denied(
        &item.read(&mut tx, &["email"]).await.unwrap_err(),
        &read_email,
    );
    assert_denied(
        &item.value(&mut tx, "email").await.unwrap_err(),
        &read_email,
    );
    let none = items.browse([]).unwrap();
    assert_denied(
        &none.read(&mut tx, &["email"]).await.unwrap_err(),
        &read_email,
    );

    // write, and create and delete commands on a restricted one2many
    assert_denied(
        &item
            .write_values(&mut tx, &map(&[("email", s("z@example.com"))]))
            .await
            .unwrap_err(),
        &write_email,
    );
    let bank_write = denied(ITEM, DESCRIPTION, "bank_ids", "write", None);
    let new_line = Value::Map(std::sync::Arc::new(vmap(&[("acc_number", s("new"))])));
    for command in [
        cmd(0, 0, new_line),
        cmd(2, fx.bank, int(0)),
        cmd(3, fx.bank, int(0)),
    ] {
        assert_denied(
            &item
                .write_values(&mut tx, &map(&[("bank_ids", Value::list([command]))]))
                .await
                .unwrap_err(),
            &bank_write,
        );
    }
    // the bank record is untouched
    let bank = tx
        .env()
        .records("test_fa.bank")
        .unwrap()
        .browse([fx.bank])
        .unwrap();
    assert_eq!(bank.exists(&mut tx).await.unwrap().len(), 1);
    assert_eq!(bank.value(&mut tx, "item_id").await.unwrap(), int(fx.item));

    // create with the field, and a `default_<field>` context key: refused before any INSERT
    let su_items = tx.env().records(ITEM).unwrap();
    let before = su_items
        .search_count(&mut tx, &Domain::TRUE, None)
        .await
        .unwrap();
    let given = assert_query_count!(tx, <= 0, {
        items
            .create_values(&mut tx, &[map(&[("name", s("n")), ("email", s("e"))])])
            .await
            .unwrap_err()
    });
    assert_denied(&given, &write_email);
    let ctx_default = assert_query_count!(tx, <= 0, {
        items
            .with_context([("default_email", s("e"))])
            .create_values(&mut tx, &[map(&[("name", s("n"))])])
            .await
            .unwrap_err()
    });
    assert_denied(&ctx_default, &write_email);
    assert_eq!(
        su_items
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        before
    );

    // unrestricted fields still read; `read()` without names leaves the restricted ones out
    let rows = item.read(&mut tx, &["name", "val"]).await.unwrap();
    assert_eq!(rows[0]["name"], s("item"));
    assert_eq!(rows[0]["val"], int(1));
    let all = item.read(&mut tx, &[]).await.unwrap();
    for hidden in ["email", "bank_ids", "forbidden", "forbidden2", "forbidden3"] {
        assert!(
            !all[0].contains_key(hidden),
            "{hidden} leaked: {:?}",
            all[0]
        );
    }
    assert_eq!(all[0]["name"], s("item"));

    // a member of the group reads and writes the field, and runs the commands
    policy.join(uid, "base.group_system");
    assert_eq!(
        item.value(&mut tx, "email").await.unwrap(),
        s("a@example.com")
    );
    item.write_values(&mut tx, &map(&[("email", s("z@example.com"))]))
        .await
        .unwrap();
    item.write_values(
        &mut tx,
        &map(&[("bank_ids", Value::list([cmd(2, fx.bank, int(0))]))]),
    )
    .await
    .unwrap();
    assert_eq!(bank.exists(&mut tx).await.unwrap().len(), 0);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// default_get path is covered)
#[tokio::test]
async fn spec_sec_022_commands_check_the_fields_they_update_on_the_comodel() {
    let policy =
        FieldPolicy::new()
            .enforce_groups()
            .deny("test_fa.bank", "acc_number", FieldOp::Write);
    let h = harness(policy.clone()).await;
    let mut tx = h.begin().await.unwrap();
    let fx = fixture(&mut tx).await;
    let uid = user(&mut tx, "demo").await;
    policy.join(uid, "base.group_system");
    let item = items_as(&tx, uid).browse([fx.item]).unwrap();
    let denied_acc = denied("test_fa.bank", "Test Bank", "acc_number", "write", None);
    let values = Value::Map(std::sync::Arc::new(vmap(&[("acc_number", s("x"))])));
    for command in [cmd(1, fx.bank, values.clone()), cmd(0, 0, values)] {
        let error = item
            .write_values(&mut tx, &map(&[("bank_ids", Value::list([command]))]))
            .await
            .unwrap_err();
        assert_denied(&error, &denied_acc);
    }
    let bank = tx
        .env()
        .records("test_fa.bank")
        .unwrap()
        .browse([fx.bank])
        .unwrap();
    assert_eq!(bank.value(&mut tx, "acc_number").await.unwrap(), s("BE01"));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_051_field_groups_on_write_block_even_with_unchanged_values() {
    let policy = FieldPolicy::new().enforce_groups();
    let h = harness(policy.clone()).await;
    let mut tx = h.begin().await.unwrap();
    let fx = fixture(&mut tx).await;
    let bob = user(&mut tx, "bob").await;
    let item = items_as(&tx, bob).browse([fx.item]).unwrap();
    let stored = item.sudo(true).value(&mut tx, "forbidden2").await.unwrap();
    assert_eq!(stored, int(5));
    // the stored value is written back: refused all the same, before any statement, and the
    // first denied key of the dictionary is the one reported
    let message = denied(ITEM, DESCRIPTION, "forbidden2", "write", None);
    let error = assert_query_count!(tx, <= 0, {
        item.write_values(&mut tx, &map(&[("forbidden2", stored.clone())]))
            .await
            .unwrap_err()
    });
    assert_denied(&error, &message);
    let error = item
        .write_values(
            &mut tx,
            &map(&[("val", int(1)), ("forbidden2", stored.clone())]),
        )
        .await
        .unwrap_err();
    assert_denied(&error, &message);
    // a member of the group may
    policy.join(bob, "test_fa.group_manager");
    item.write_values(&mut tx, &map(&[("forbidden2", stored)]))
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_021_field_groups_hide_fields() {
    let policy = FieldPolicy::new()
        .enforce_groups()
        .deny(ITEM, "val", FieldOp::Write);
    let h = harness(policy.clone()).await;
    let mut tx = h.begin().await.unwrap();
    let uid = user(&mut tx, "demo").await;
    let items = items_as(&tx, uid);
    let hidden = [
        "forbidden",
        "forbidden2",
        "forbidden3",
        "bank_ids",
        "email",
        "mirror",
        "mirror_open",
        "mirror_chain",
    ];
    let fields = assert_query_count!(tx, <= 0, {
        items.fields_get(&mut tx, None, Some(&["readonly"])).await.unwrap()
    });
    for name in hidden {
        assert!(!fields.contains_key(name), "{name} is visible");
    }
    for name in ["id", "name", "val", "parent_id"] {
        assert!(fields.contains_key(name), "{name} is hidden");
    }
    // readonly is forced for the field the user may read but not write
    assert_eq!(fields["val"]["readonly"], Value::Bool(true));
    assert_eq!(fields["name"]["readonly"], Value::Bool(false));
    // the superuser sees everything
    let su = tx.env().records(ITEM).unwrap();
    let all = su
        .fields_get(&mut tx, None, Some(&["readonly"]))
        .await
        .unwrap();
    for name in hidden {
        assert!(
            all.contains_key(name),
            "{name} is hidden from the superuser"
        );
    }
    // visible again after joining the group
    policy.join(uid, "test_fa.group_manager");
    let after = items
        .fields_get(&mut tx, None, Some(&["readonly"]))
        .await
        .unwrap();
    assert!(after.contains_key("forbidden2"));
    assert!(after.contains_key("mirror"), "related to the same group");
    assert!(!after.contains_key("forbidden"), "another group");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// names; the whole case, with the group names from the groups tables, is
/// `erp-security/tests/groups.rs`)
#[tokio::test]
async fn spec_sec_023_field_access_error_message_in_debug_mode() {
    let policy = FieldPolicy::new()
        .enforce_groups()
        .group_name("base.group_portal", "Role / Portal")
        .group_name("test_fa.group_test", "Test Group");
    let h = harness(policy.clone()).await;
    let mut tx = h.begin().await.unwrap();
    let fx = fixture(&mut tx).await;
    let uid = user(&mut tx, "demo").await;
    let debug = tx.env().with_user(uid).with_debug(true);
    let item = debug.records(ITEM).unwrap().browse([fx.item]).unwrap();
    let groups = format!("\nUser: {uid}\nGroups: allowed for groups 'Role / Portal', 'Test Group'");
    assert_denied(
        &item.value(&mut tx, "forbidden").await.unwrap_err(),
        &denied(ITEM, DESCRIPTION, "forbidden", "read", Some(&groups)),
    );
    let forbidden = format!("\nUser: {uid}\nGroups: always forbidden");
    assert_denied(
        &item.value(&mut tx, "forbidden3").await.unwrap_err(),
        &denied(ITEM, DESCRIPTION, "forbidden3", "read", Some(&forbidden)),
    );
    // write: the first denied key, dictionary order
    let error = item
        .write_values(
            &mut tx,
            &map(&[("forbidden", int(1)), ("forbidden2", int(2))]),
        )
        .await
        .unwrap_err();
    assert_denied(
        &error,
        &denied(ITEM, DESCRIPTION, "forbidden", "write", Some(&groups)),
    );
    // outside debug mode the lines are absent
    let plain = tx
        .env()
        .with_user(uid)
        .records(ITEM)
        .unwrap()
        .browse([fx.item])
        .unwrap();
    assert_denied(
        &plain.value(&mut tx, "forbidden").await.unwrap_err(),
        &denied(ITEM, DESCRIPTION, "forbidden", "read", None),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

fn eq58(field: &str) -> Domain {
    Domain::condition(field, Operator::Eq, 58i64).unwrap()
}

#[tokio::test]
async fn spec_sec_024_restricted_fields_in_domains_order_and_grouping() {
    let policy = FieldPolicy::new().enforce_groups();
    let h = harness(policy.clone()).await;
    let mut tx = h.begin().await.unwrap();
    let fx = fixture(&mut tx).await;
    let uid = user(&mut tx, "demo").await;
    let items = items_as(&tx, uid);
    let read =
        |model: &str, description: &str| denied(model, description, "forbidden3", "read", None);
    let on_item = read(ITEM, DESCRIPTION);

    // domains: directly, through a path, through `any`, on an `_inherits` child
    let any = Domain::field("parent_id").unwrap().any(eq58("forbidden3"));
    for domain in [eq58("forbidden3"), eq58("parent_id.forbidden3"), any] {
        let error = items.search(&mut tx, &domain).await.unwrap_err();
        assert_denied(&error, &on_item);
        let error = items
            .search_count(&mut tx, &domain, None)
            .await
            .unwrap_err();
        assert_denied(&error, &on_item);
    }
    let children = tx.env().records("test_fa.inherits").unwrap().with_user(uid);
    let error = children
        .search(&mut tx, &eq58("forbidden3"))
        .await
        .unwrap_err();
    assert_denied(&error, &read("test_fa.inherits", "Inherits"));
    // the bypassing `any!` evaluates its sub-domain as superuser
    let bypass = Domain::condition("parent_id", Operator::AnyBypass, eq58("forbidden3")).unwrap();
    items.search(&mut tx, &bypass).await.unwrap();

    // order: any position; ordering by `val` alone works
    items
        .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order("val"))
        .await
        .unwrap();
    for order in [
        "forbidden3 DESC",
        "forbidden3",
        "val DESC,    forbidden3       DESC",
    ] {
        let error = items
            .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order(order))
            .await
            .unwrap_err();
        assert_denied(&error, &on_item);
    }

    // group by, aggregates and the domain of a grouping
    items
        ._read_group(&mut tx, ReadGroup::new(Domain::TRUE).groupby(["val"]))
        .await
        .unwrap();
    let attempts = [
        ReadGroup::new(eq58("forbidden3")).groupby(["val"]),
        ReadGroup::new(eq58("parent_id.forbidden3")).groupby(["val"]),
        ReadGroup::new(Domain::TRUE).groupby(["forbidden3"]),
        ReadGroup::new(Domain::TRUE).aggregates(["forbidden3:array_agg"]),
    ];
    for args in attempts {
        let error = items._read_group(&mut tx, args).await.unwrap_err();
        assert_denied(&error, &on_item);
    }
    // the superuser may do all of it
    let su = tx.env().records(ITEM).unwrap();
    assert_eq!(
        su.search(&mut tx, &eq58("forbidden3")).await.unwrap().ids(),
        [fx.item]
    );
    let _ = fx.parent;
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_050_related_fields_take_the_target_groups_unless_they_declare_their_own() {
    let policy = FieldPolicy::new().enforce_groups();
    let h = harness(policy.clone()).await;
    let item = h.registry().model(ITEM).unwrap();
    let target = item.field("forbidden2").unwrap().groups;
    assert!(target.is_some());
    // without `groups`: the target's, along a chain of related fields too
    assert_eq!(item.field("mirror").unwrap().groups, target);
    assert_eq!(item.field("mirror_chain").unwrap().groups, target);
    // with `groups`: its own
    let own = item.field("mirror_open").unwrap().groups;
    assert_eq!(own, Some("base.group_user"));
    assert_ne!(own, target);
    // an `_inherits` child's field carries the parent field's groups
    let child = h.registry().model("test_fa.inherits").unwrap();
    assert_eq!(child.field("forbidden2").unwrap().groups, target);
    assert_eq!(child.field("label").unwrap().groups, None);

    // enforcement through the related path
    let mut tx = h.begin().await.unwrap();
    let fx = fixture(&mut tx).await;
    let uid = user(&mut tx, "demo").await;
    let record = items_as(&tx, uid).browse([fx.item]).unwrap();
    for field in ["mirror", "mirror_chain"] {
        let error = record.value(&mut tx, field).await.unwrap_err();
        assert_denied(&error, &denied(ITEM, DESCRIPTION, field, "read", None));
    }
    let error = record
        .write_values(&mut tx, &map(&[("mirror", int(1))]))
        .await
        .unwrap_err();
    assert_denied(&error, &denied(ITEM, DESCRIPTION, "mirror", "write", None));
    policy.join(uid, "test_fa.group_manager");
    assert_eq!(record.value(&mut tx, "mirror").await.unwrap(), int(9));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The superuser bypasses field groups in one place: the policy is not even asked.
#[tokio::test]
async fn superuser_mode_bypasses_field_groups_without_asking_the_policy() {
    let policy = FieldPolicy::new().enforce_groups().forbid(ITEM, "val");
    let h = harness(policy.clone()).await;
    let mut tx = h.begin().await.unwrap();
    let fx = fixture(&mut tx).await;
    let uid = user(&mut tx, "demo").await;
    let su = tx.env().records(ITEM).unwrap().browse([fx.item]).unwrap();
    su.read(&mut tx, &["email", "forbidden3", "val"])
        .await
        .unwrap();
    su.write_values(&mut tx, &map(&[("val", int(2)), ("forbidden3", int(1))]))
        .await
        .unwrap();
    su.search_with(
        &mut tx,
        &Domain::TRUE,
        &SearchOptions::new().order("forbidden3"),
    )
    .await
    .unwrap();
    // elevation of a user: `sudo()` behaves the same
    let elevated = items_as(&tx, uid).sudo(true).browse([fx.item]).unwrap();
    elevated
        .read(&mut tx, &["email", "forbidden3", "val"])
        .await
        .unwrap();
    assert_eq!(policy.mask_calls(), 0, "no call site tests su() itself");
    // without elevation the policy is asked, and `val` is forbidden
    let plain = items_as(&tx, uid).browse([fx.item]).unwrap();
    let error = plain.value(&mut tx, "val").await.unwrap_err();
    assert_denied(&error, &denied(ITEM, DESCRIPTION, "val", "read", None));
    assert!(policy.mask_calls() > 0);
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- `#[field_access]` overrides ----------------------------------------------------------------

const GUARDED: &str = "test_fa.guarded";

async fn guarded_harness(policy: FieldPolicy, ext: bool) -> Harness {
    let mut addons = vec![&base_users::ADDON, &test_field_access::ADDON];
    if ext {
        addons.push(&test_field_access_ext::ADDON);
    }
    Harness::builder(addons)
        .policy(policy)
        .build()
        .await
        .expect("harness")
}

async fn guarded(tx: &mut Transaction) -> i64 {
    let created = tx
        .env()
        .records(GUARDED)
        .unwrap()
        .create_values(
            tx,
            &[map(&[("name", s("g")), ("val", int(1)), ("note", s("n"))])],
        )
        .await
        .unwrap();
    created.ids()[0]
}

/// An override narrows what the policy leaves: a group-dependent read, a context-dependent write.
#[tokio::test]
async fn field_access_override_narrows_the_policys_mask() {
    let policy = FieldPolicy::new()
        .with_overrides()
        .deny(GUARDED, "name", FieldOp::Write);
    let h = guarded_harness(policy.clone(), false).await;
    policy.attach(h.registry()).unwrap();
    let mut tx = h.begin().await.unwrap();
    let id = guarded(&mut tx).await;
    let uid = user(&mut tx, "demo").await;
    let rec = tx
        .env()
        .records(GUARDED)
        .unwrap()
        .with_user(uid)
        .browse([id])
        .unwrap();
    let read_note = denied(GUARDED, "Guarded", "note", "read", None);
    assert_denied(&rec.value(&mut tx, "note").await.unwrap_err(), &read_note);
    assert_eq!(rec.value(&mut tx, "val").await.unwrap(), int(1));
    policy.join(uid, "test_fa.group_test");
    assert_eq!(rec.value(&mut tx, "note").await.unwrap(), s("n"));

    // the declared context key narrows the write; absent or false it does not
    rec.write_values(&mut tx, &map(&[("val", int(2))]))
        .await
        .unwrap();
    let frozen = rec.with_context([("fa_freeze", Value::Bool(true))]);
    let error = frozen
        .write_values(&mut tx, &map(&[("val", int(3))]))
        .await
        .unwrap_err();
    assert_denied(&error, &denied(GUARDED, "Guarded", "val", "write", None));
    let thawed = rec.with_context([("fa_freeze", Value::Bool(false))]);
    thawed
        .write_values(&mut tx, &map(&[("val", int(3))]))
        .await
        .unwrap();
    // and shows in fields_get
    let described = frozen
        .fields_get(&mut tx, None, Some(&["readonly"]))
        .await
        .unwrap();
    assert_eq!(described["val"]["readonly"], Value::Bool(true));
    // what the policy denied stays denied below the override (`super`)
    let error = rec
        .write_values(&mut tx, &map(&[("name", s("x"))]))
        .await
        .unwrap_err();
    assert_denied(&error, &denied(GUARDED, "Guarded", "name", "write", None));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Overrides of two addons form a chain, newest first; the declared keys are the union.
#[tokio::test]
async fn field_access_overrides_of_two_addons_chain_through_super() {
    let policy = FieldPolicy::new().with_overrides();
    let h = guarded_harness(policy.clone(), true).await;
    policy.attach(h.registry()).unwrap();
    let mut tx = h.begin().await.unwrap();
    let id = guarded(&mut tx).await;
    let uid = user(&mut tx, "demo").await;
    policy.join(uid, "test_fa.group_test");
    let guarded_id = h.registry().model_id(GUARDED).unwrap();
    assert_eq!(
        policy.context_keys(guarded_id),
        ["fa_freeze_name", "fa_freeze"],
        "newest addon first"
    );
    let rec = tx
        .env()
        .records(GUARDED)
        .unwrap()
        .with_user(uid)
        .browse([id])
        .unwrap();
    let both = rec.with_context([
        ("fa_freeze", Value::Bool(true)),
        ("fa_freeze_name", Value::Bool(true)),
    ]);
    for (field, value) in [("val", int(9)), ("name", s("x"))] {
        let error = both
            .write_values(&mut tx, &map(&[(field, value)]))
            .await
            .unwrap_err();
        assert_denied(&error, &denied(GUARDED, "Guarded", field, "write", None));
    }
    let only_name = rec.with_context([("fa_freeze_name", Value::Bool(true))]);
    only_name
        .write_values(&mut tx, &map(&[("val", int(7))]))
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// An override may only narrow: widening is a `Programming` error naming the override and field.
#[tokio::test]
async fn field_access_override_that_widens_is_a_programming_error() {
    let policy =
        FieldPolicy::new()
            .with_overrides()
            .deny("test_fa.widening", "name", FieldOp::Write);
    let h = guarded_harness(policy.clone(), false).await;
    policy.attach(h.registry()).unwrap();
    let mut tx = h.begin().await.unwrap();
    let created = tx
        .env()
        .records("test_fa.widening")
        .unwrap()
        .create_values(&mut tx, &[map(&[("name", s("w"))])])
        .await
        .unwrap();
    let uid = user(&mut tx, "demo").await;
    let rec = created.with_user(uid);
    let error = rec
        .write_values(&mut tx, &map(&[("name", s("x"))]))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        "field access override test_fa.widening.widen widened field 'name'"
    );
    // the mask of the model is wrong as a whole: reads fail the same way (fail closed)
    let error = rec.value(&mut tx, "name").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Reading a context key the override did not declare is a `Programming` error.
#[tokio::test]
async fn field_access_override_cannot_read_undeclared_context_keys() {
    let policy = FieldPolicy::new().with_overrides();
    let h = guarded_harness(policy.clone(), false).await;
    policy.attach(h.registry()).unwrap();
    let mut tx = h.begin().await.unwrap();
    let created = tx
        .env()
        .records("test_fa.sloppy")
        .unwrap()
        .create_values(&mut tx, &[map(&[("name", s("w"))])])
        .await
        .unwrap();
    let uid = user(&mut tx, "demo").await;
    let error = created
        .with_user(uid)
        .value(&mut tx, "name")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        "a field access override of test_fa.sloppy read the context key 'undeclared_key', which \
         no #[field_access(context = [..])] of the model declares"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Field checks cost no statement (engine note §7): the same operations spend the same number of
/// statements for a restricted user as for the superuser.
#[tokio::test]
async fn field_checks_add_no_statements() {
    let policy = FieldPolicy::new().enforce_groups();
    let h = harness(policy.clone()).await;
    let mut tx = h.begin().await.unwrap();
    let fx = fixture(&mut tx).await;
    let uid = user(&mut tx, "demo").await;
    let order = SearchOptions::new()
        .order("val DESC")
        .fetch_fields(["name", "val"]);
    let by_groups = Domain::condition("parent_id.name", Operator::Eq, "parent").unwrap();
    let mut used: Vec<u64> = Vec::new();
    for (label, records) in [
        ("superuser", tx.env().records(ITEM).unwrap()),
        ("restricted user", items_as(&tx, uid)),
    ] {
        tx.invalidate_all().await.unwrap();
        let before = tx.query_count();
        let found = records
            .search_with(&mut tx, &by_groups, &order)
            .await
            .unwrap();
        let rows = found
            .read(&mut tx, &["name", "val", "parent_id"])
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "{label}");
        records
            .browse([fx.item])
            .unwrap()
            .write_values(&mut tx, &map(&[("name", s(label))]))
            .await
            .unwrap();
        tx.flush().await.unwrap();
        used.push(tx.query_count() - before);
    }
    assert!(used[0] > 0, "the work spends statements");
    assert_eq!(used[0], used[1], "field checks must not add statements");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_comp_036_inverse_respects_field_access() {
    let policy = FieldPolicy::new().enforce_groups();
    let h = harness(policy).await;
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().model::<test_field_access::Item>().unwrap();
    let item = model
        .create(
            &mut tx,
            vec![model.vals().set(test_field_access::Item::NAME, "Foo")],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();

    let user_id = user(&mut tx, "plain_user").await;
    let item_user = item.with_user(user_id);

    // Assigning non-stored inverse field without base.group_system fails with AccessError
    let err = item_user
        .set(&mut tx, test_field_access::Item::INV_NAME, "Forbidden")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.to_string(),
        denied(ITEM, DESCRIPTION, "inv_name", "write", None)
    );

    // Superuser can write and inverse sets name
    item.set(&mut tx, test_field_access::Item::INV_NAME, "Allowed")
        .await
        .unwrap();
    assert_eq!(
        item.get(&mut tx, test_field_access::Item::NAME)
            .await
            .unwrap()
            .as_deref(),
        Some("Allowed")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
