//! integration review D1: the field checks of `read`, `search_with` (with fetched fields),
//! `search_after` and `default_get` bring the policy and the subject's groups up to date first, so a
//! membership or group write of the same transaction, or a fresh `with_user`, does not fail them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use std::sync::Arc;

use erp_orm::{Fetch, Orm, PolicyProvider, SearchOptions, UserId};
use erp_sdk::{Domain, Transaction, Value};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};
use users_support::{harness, map, s, set_cmd};

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn exec(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    client.execute(sql, params).await.unwrap();
}

async fn group(client: &Client, name: &str, xmlid: Option<&str>) -> i64 {
    let id = one(
        client,
        "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
        &[&name],
    )
    .await;
    if let Some((module, local)) = xmlid.and_then(|x| x.split_once('.')) {
        exec(
            client,
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ($1, $2, 'res.groups', $3)",
            &[&module, &local, &id],
        )
        .await;
    }
    id
}

async fn model(client: &Client, name: &str, description: &str) -> i64 {
    one(
        client,
        "INSERT INTO ir_model (name, model, state, transient) VALUES ($1, $2, 'base', false) \
         RETURNING id::int8",
        &[&description, &name],
    )
    .await
}

async fn access(client: &Client, name: &str, model: i64, group: i64, ops: &str) {
    exec(
        client,
        "INSERT INTO ir_access (name, model_id, group_id, operation, active) \
         VALUES ($1, $2, $3, $4, true)",
        &[&name, &model, &group, &ops],
    )
    .await;
}

async fn new_user(client: &Client, login: &str, groups: &[i64]) -> i64 {
    let uid = one(
        client,
        "INSERT INTO res_users (name, login, active, share) VALUES ($1, $1, true, false) \
         RETURNING id::int8",
        &[&login],
    )
    .await;
    for g in groups {
        exec(
            client,
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&uid, g],
        )
        .await;
    }
    uid
}

async fn production_orm(h: &Harness) -> Orm {
    let pool = || erp_orm::pool::create_pool(h.db().config(), 2).unwrap();
    let source = PolicySource::build(Arc::clone(h.registry()), pool())
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(h.registry()), pool())
        .policy(provider)
        .resolver(Resolver::id_order())
        .build()
        .unwrap()
}

async fn begin(orm: &Orm, uid: i64) -> Transaction {
    orm.begin(UserId::new(uid).unwrap()).await.unwrap()
}

struct World {
    h: Harness,
    orm: Orm,
    extra: i64,
    internal: i64,
    boss: i64,
    me: i64,
    boss2: i64,
}

/// `boss` (internal + access-rights manager + settings) reads and writes users and groups; `me` is internal.
async fn world() -> World {
    let h = harness().await;
    let client = connect(&h).await;
    let internal = group(&client, "Internal User", Some("base.group_user")).await;
    let manager = group(&client, "Access Rights", Some("base.group_erp_manager")).await;
    let extra = group(&client, "Extra", None).await;
    let system = group(&client, "Administrator", Some("base.group_system")).await;
    new_user(&client, "admin", &[internal, system]).await;
    let users_model = model(&client, "res.users", "User").await;
    let groups_model = model(&client, "res.groups", "Access Groups").await;
    access(&client, "internal read users", users_model, internal, "r").await;
    access(&client, "managers write users", users_model, manager, "u").await;
    access(&client, "internal read groups", groups_model, internal, "r").await;
    access(&client, "managers write groups", groups_model, manager, "u").await;
    // user management is a Settings (base.group_system) right here: it exempts the field mask of
    // res.users, which these cases rely on for `group_ids`
    let boss = new_user(&client, "boss", &[internal, manager, system]).await;
    let me = new_user(&client, "me", &[internal]).await;
    let boss2 = new_user(&client, "boss2", &[internal, manager]).await;
    let orm = production_orm(&h).await;
    World {
        boss2,
        h,
        orm,
        extra,
        internal,
        boss,
        me,
    }
}

/// A non-superuser writes a user's `group_ids`, then reads the user back in the same transaction.
#[tokio::test]
async fn read_after_a_membership_write_in_the_same_transaction() {
    let w = world().await;
    let mut tx = begin(&w.orm, w.boss).await;
    let user = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([w.me])
        .unwrap();
    user.write_values(
        &mut tx,
        &map(&[("group_ids", set_cmd(&[w.internal, w.extra]))]),
    )
    .await
    .unwrap();
    let rows = user.read(&mut tx, &["name", "login"]).await.unwrap();
    assert_eq!(rows[0]["login"], s("me"));
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// A fresh transaction reads as another user without any earlier access decision.
#[tokio::test]
async fn read_as_another_user_in_a_fresh_transaction() {
    let w = world().await;
    let mut tx = begin(&w.orm, w.boss).await;
    let user = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([w.boss])
        .unwrap()
        .with_user(w.me);
    let rows = user.read(&mut tx, &["name", "login"]).await.unwrap();
    assert_eq!(rows[0]["login"], s("boss"));
    drop(tx);
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn search_with_fetched_fields_after_a_group_write() {
    let w = world().await;
    let mut tx = begin(&w.orm, w.boss).await;
    let groups = tx
        .env()
        .records("res.groups")
        .unwrap()
        .browse([w.extra])
        .unwrap();
    groups
        .write_values(&mut tx, &map(&[("name", s("Extra renamed"))]))
        .await
        .unwrap();
    let opts = SearchOptions {
        fetch: Fetch::Fields(vec!["name".to_owned()]),
        ..SearchOptions::default()
    };
    let found = tx
        .env()
        .records("res.groups")
        .unwrap()
        .search_with(&mut tx, &Domain::constant(true), &opts)
        .await
        .unwrap();
    assert!(found.ids().contains(&w.extra));
    drop(tx);
    w.h.cleanup().await.unwrap();
}

#[tokio::test]
async fn default_get_as_another_user_in_a_fresh_transaction() {
    let w = world().await;
    let mut tx = begin(&w.orm, w.boss).await;
    // an UPDATE command of a `default_group_ids` context key is access-checked on the comodel
    let update = Value::list([Value::list([
        Value::Int(1),
        Value::Int(w.extra),
        Value::Map(std::sync::Arc::new(
            [("name".to_owned(), s("x"))].into_iter().collect(),
        )),
    ])]);
    let users = tx
        .env()
        .records("res.users")
        .unwrap()
        .with_user(w.boss2)
        .with_context([("default_group_ids", update)]);
    users.default_get(&mut tx, &["group_ids"]).await.unwrap();
    drop(tx);
    w.h.cleanup().await.unwrap();
}

/// `search_after` with fetched fields right after a group write of the same transaction: the field
/// masks are built only after the policy generation and the subject's groups are brought up to date.
#[tokio::test]
async fn search_after_with_fields_after_a_group_write() {
    let w = world().await;
    let mut tx = begin(&w.orm, w.boss).await;
    let groups = tx
        .env()
        .records("res.groups")
        .unwrap()
        .browse([w.extra])
        .unwrap();
    groups
        .write_values(&mut tx, &map(&[("name", s("Extra renamed"))]))
        .await
        .unwrap();
    let opts = SearchOptions {
        fetch: Fetch::Fields(vec!["name".to_owned()]),
        limit: Some(10),
        order: Some("id".to_owned()),
        ..SearchOptions::default()
    };
    let page = tx
        .env()
        .records("res.groups")
        .unwrap()
        .search_after(&mut tx, &Domain::constant(true), &opts, None)
        .await
        .unwrap();
    assert!(page.records.ids().contains(&w.extra));
    drop(tx);
    w.h.cleanup().await.unwrap();
}
