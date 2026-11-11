//! Helpers of the `res.groups` / `res.users` rule tests: a database with `base`
//! and `ir.model.data`, groups with external ids, users with groups.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test helpers"
)]

use std::collections::BTreeMap;

use erp_sdk::{Error, ErrorKind, Records, Transaction, Value};
use erp_testkit::Harness;

pub fn s(text: &str) -> Value {
    Value::str(text)
}

pub fn int(n: i64) -> Value {
    Value::Int(n)
}

pub fn map(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// `base` and `ir.model.data` on a fresh database.
pub async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .expect("harness")
}

/// The value of an x2many field that sets `ids`: `[[6, 0, ids]]`.
pub fn set_cmd(ids: &[i64]) -> Value {
    Value::list([Value::list([
        int(6),
        int(0),
        Value::list(ids.iter().map(|i| int(*i))),
    ])])
}

/// `[4, id, 0]`.
pub fn link_cmd(id: i64) -> Value {
    Value::list([int(4), int(id), int(0)])
}

/// `[3, id, 0]`.
pub fn unlink_cmd(id: i64) -> Value {
    Value::list([int(3), int(id), int(0)])
}

/// Create one record. Groups are created in the base installation state unless the test
/// brought `base.group_user_regular` itself: these databases hold none of base's data, and
/// creating a group applies that marker group, which is an error outside the installation.
pub async fn create(tx: &mut Transaction, model: &str, pairs: &[(&str, Value)]) -> i64 {
    let was = erp_addon_base::group_graph::base_installing(tx);
    if model == "res.groups" && tx.env().uid() == erp_orm::SUPERUSER_ID.get() {
        erp_testkit::base_install::set_base_installing(tx, true).unwrap();
    }
    let id = tx
        .env()
        .records(model)
        .unwrap()
        .create_values(tx, &[map(pairs)])
        .await
        .unwrap()
        .ids()[0];
    erp_testkit::base_install::set_base_installing(tx, was).unwrap();
    id
}

/// Switch the base installation state of the (System) transaction: tests of databases that have
/// none of base's data switch it on for the writes that need the regular-user marker group.
pub fn installing(tx: &mut Transaction, on: bool) {
    erp_testkit::base_install::set_base_installing(tx, on).unwrap();
}

/// The `base.group_user_regular` marker group, created if the database has none, as an installed
/// base has it (every group created or rearranged afterwards applies it). Returns its id.
pub async fn regular_marker(tx: &mut Transaction) -> i64 {
    let rows = tx
        .query(
            "SELECT res_id FROM ir_model_data \
              WHERE module = 'base' AND name = 'group_user_regular'",
            &[],
        )
        .await
        .unwrap();
    if let Some(row) = rows.first() {
        return row.get(0);
    }
    // the marker cannot be applied before it exists: the install state covers its own create
    let was = erp_addon_base::group_graph::base_installing(tx);
    installing(tx, true);
    let id = create(tx, "res.groups", &[("name", s("Is regular user"))]).await;
    bind(tx, "base.group_user_regular", id).await;
    installing(tx, was);
    id
}

/// A group without external id; `implied` are its implied groups.
pub async fn group(tx: &mut Transaction, name: &str, implied: &[i64]) -> i64 {
    let mut pairs = vec![("name", s(name))];
    if !implied.is_empty() {
        pairs.push(("implied_ids", set_cmd(implied)));
    }
    create(tx, "res.groups", &pairs).await
}

/// A group with external id `module.name`.
pub async fn named_group(tx: &mut Transaction, xmlid: &str, name: &str) -> i64 {
    let id = group(tx, name, &[]).await;
    bind(tx, xmlid, id).await;
    id
}

/// Bind the external id `module.name` to group `id`.
pub async fn bind(tx: &mut Transaction, xmlid: &str, id: i64) {
    let (module, name) = xmlid.split_once('.').unwrap();
    tx.execute(
        "INSERT INTO ir_model_data (module, name, model, res_id) \
         VALUES ($1, $2, 'res.groups', $3)",
        &[&module, &name, &id],
    )
    .await
    .unwrap();
}

/// The three user type groups, as `(internal, portal, public)`, with the administrator group and
/// its administrator (see [`user_types_and_admin`]).
pub async fn user_types(tx: &mut Transaction) -> (i64, i64, i64) {
    let t = user_types_and_admin(tx).await;
    (t.internal, t.portal, t.public)
}

/// What a database that has `base` installed has: the user type groups, `base.group_system` and
/// one administrator `admin`, a direct member of `base.group_system` only.
pub struct UserTypes {
    pub internal: i64,
    pub portal: i64,
    pub public: i64,
    pub system: i64,
    pub admin: i64,
}

pub async fn user_types_and_admin(tx: &mut Transaction) -> UserTypes {
    let internal = named_group(tx, "base.group_user", "Internal User").await;
    let portal = named_group(tx, "base.group_portal", "Portal").await;
    let public = named_group(tx, "base.group_public", "Public").await;
    let system = named_group(tx, "base.group_system", "Administrator").await;
    let admin = user(tx, "admin", &[system]).await;
    UserTypes {
        internal,
        portal,
        public,
        system,
        admin,
    }
}

/// A user with the given direct groups (none: `group_ids` is not given).
pub async fn user(tx: &mut Transaction, login: &str, groups: &[i64]) -> i64 {
    let mut pairs = vec![("name", s(login)), ("login", s(login))];
    if !groups.is_empty() {
        pairs.push(("group_ids", set_cmd(groups)));
    }
    create(tx, "res.users", &pairs).await
}

pub async fn try_user(tx: &mut Transaction, login: &str, groups: &[i64]) -> Result<Records, Error> {
    tx.env()
        .records("res.users")
        .unwrap()
        .create_values(
            tx,
            &[map(&[
                ("name", s(login)),
                ("login", s(login)),
                ("group_ids", set_cmd(groups)),
            ])],
        )
        .await
}

pub fn records(tx: &Transaction, model: &str, ids: &[i64]) -> Records {
    tx.env()
        .records(model)
        .unwrap()
        .browse(ids.iter().copied())
        .unwrap()
}

/// Sorted values of a mapped path.
pub async fn mapped(tx: &mut Transaction, rs: &Records, path: &str) -> Vec<String> {
    let mut values: Vec<String> = rs
        .mapped_path(tx, path)
        .await
        .unwrap()
        .into_values()
        .expect("a scalar path")
        .into_iter()
        .map(|v| match v {
            Value::Str(s) => s.to_string(),
            other => panic!("not a text: {other:?}"),
        })
        .collect();
    values.sort();
    values
}

/// Sorted ids of a relational field.
pub async fn ids_of(tx: &mut Transaction, rs: &Records, field: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = match rs.value(tx, field).await.unwrap() {
        Value::List(items) => items
            .iter()
            .map(|v| match v {
                Value::Int(i) => *i,
                other => panic!("not an id: {other:?}"),
            })
            .collect(),
        other => panic!("not a list: {other:?}"),
    };
    ids.sort_unstable();
    ids
}

#[track_caller]
pub fn assert_error(error: &Error, kind: ErrorKind, message: &str) {
    assert_eq!(error.kind(), kind, "{error}");
    assert_eq!(error.to_string(), message);
}
