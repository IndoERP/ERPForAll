//! on the production policy. The field mask of `res.users` is a
//! `#[field_access]` override of `addons/base`; `FieldPolicy` of the testkit applies overrides in
//! its own way, so the rule is also proved here through the compiled policy of `erp-security`
//! (`PolicySource`, `SnapshotPolicy`), the principal resolver and the database tables.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod users_support;

use std::sync::Arc;

use erp_orm::{Orm, PolicyProvider, UserId};
use erp_sdk::{Error, ErrorKind, Transaction, Value};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::tokio_postgres::{Client, NoTls};
use users_support::{assert_error, create, harness, int, map, s, set_cmd};

async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("base_users_policy: connection closed with an error: {error}");
        }
    });
    client
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn exec(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    client.execute(sql, params).await.unwrap();
}

async fn group(client: &Client, name: &str, xmlid: &str) -> i64 {
    let id = one(
        client,
        "INSERT INTO res_groups (name) VALUES ($1) RETURNING id::int8",
        &[&name],
    )
    .await;
    let (module, local) = xmlid.split_once('.').unwrap();
    exec(
        client,
        "INSERT INTO ir_model_data (module, name, model, res_id) \
         VALUES ($1, $2, 'res.groups', $3)",
        &[&module, &local, &id],
    )
    .await;
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

async fn access(
    client: &Client,
    name: &str,
    model: i64,
    group: i64,
    ops: &str,
    domain: Option<&str>,
) {
    exec(
        client,
        "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
         VALUES ($1, $2, $3, $4, $5, true)",
        &[&name, &model, &group, &ops, &domain],
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

fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

async fn production_orm(h: &Harness) -> Orm {
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

async fn begin(orm: &Orm, uid: i64) -> Transaction {
    orm.begin(UserId::new(uid).unwrap()).await.unwrap()
}

fn denied(field: &str) -> String {
    format!(
        "You do not have enough rights to access the field \"{field}\" on User (res.users). \
         Please contact your system administrator.\n\nOperation: write"
    )
}

async fn memberships(client: &Client, uid: i64) -> Vec<i64> {
    client
        .query(
            "SELECT gid FROM res_groups_users_rel WHERE uid = $1 ORDER BY gid",
            &[&uid],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test]
async fn spec_sec_033_users_write_only_user_writeable_fields_on_the_compiled_policy() {
    let h = harness().await;
    let client = connect(&h).await;
    let internal = group(&client, "Internal User", "base.group_user").await;
    let system = group(&client, "Administrator", "base.group_system").await;
    let users_model = model(&client, "res.users", "User").await;
    let groups_model = model(&client, "res.groups", "Access Groups").await;
    access(
        &client,
        "internal users read users",
        users_model,
        internal,
        "r",
        None,
    )
    .await;
    access(
        &client,
        "internal users write themselves",
        users_model,
        internal,
        "u",
        Some("[('id', '=', user.id)]"),
    )
    .await;
    access(
        &client,
        "administrators write users",
        users_model,
        system,
        "u",
        None,
    )
    .await;
    access(
        &client,
        "internal users read groups",
        groups_model,
        internal,
        "r",
        None,
    )
    .await;
    let admin = new_user(&client, "admin", &[internal, system]).await;
    let me = new_user(&client, "me", &[internal]).await;
    let boss = new_user(&client, "boss", &[internal, system]).await;
    let orm = production_orm(&h).await;

    // `lang` must name an installed language: this database installed none
    let mut tx = begin(&orm, erp_orm::SUPERUSER_ID.get()).await;
    create(
        &mut tx,
        "res.lang",
        &[
            ("name", s("Indonesian")),
            ("code", s("id_ID")),
            ("url_code", s("id")),
            ("active", Value::Bool(true)),
        ],
    )
    .await;
    tx.commit().await.unwrap();
    drop(tx);

    // an internal user on their own record: the user_writeable fields succeed
    let mut tx = begin(&orm, me).await;
    let own = tx.env().records("res.users").unwrap().browse([me]).unwrap();
    own.write_values(
        &mut tx,
        &map(&[
            ("name", s("Me Myself")),
            ("email", s("me@example.com")),
            ("tz", s("Asia/Jakarta")),
            ("lang", s("id_ID")),
        ]),
    )
    .await
    .unwrap();
    assert_eq!(
        own.value(&mut tx, "email").await.unwrap(),
        s("me@example.com")
    );
    // and nothing else does, whatever else is in the same write; adding base.group_system is the
    // privilege escalation the mask exists for
    let escalation = Value::list([Value::list([int(4), int(system), int(0)])]);
    for (field, value) in [
        ("group_ids", escalation),
        ("group_ids", set_cmd(&[internal, system])),
        ("login", s("changed")),
        ("active", Value::Bool(true)),
        ("share", Value::Bool(true)),
        ("company_id", int(1)),
        ("company_ids", set_cmd(&[])),
    ] {
        let err: Error = own
            .write_values(&mut tx, &map(&[("name", s("Other Name")), (field, value)]))
            .await
            .unwrap_err();
        assert_error(&err, ErrorKind::Access, &denied(field));
    }
    let err: Error = own
        .write_values(
            &mut tx,
            &map(&[("name", s("Other Name")), ("active", Value::Bool(false))]),
        )
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::UserError,
        "You cannot deactivate the user you're currently logged in as.",
    );
    assert_eq!(own.value(&mut tx, "name").await.unwrap(), s("Me Myself"));
    assert_eq!(own.value(&mut tx, "login").await.unwrap(), s("me"));
    tx.commit().await.unwrap();
    // a committed transaction holds its connection until it is dropped
    drop(tx);
    assert_eq!(memberships(&client, me).await, [internal]);

    // another user's record is out of reach of the record rule, whatever the field
    let mut tx = begin(&orm, me).await;
    let other = tx
        .env()
        .records("res.users")
        .unwrap()
        .browse([boss])
        .unwrap();
    let err = other
        .write_values(&mut tx, &map(&[("email", s("x@example.com"))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err}");
    drop(tx);

    // a Settings administrator writes the rest, the superuser mode too
    let mut tx = begin(&orm, boss).await;
    let target = tx.env().records("res.users").unwrap().browse([me]).unwrap();
    target
        .write_values(
            &mut tx,
            &map(&[
                ("login", s("me2")),
                ("group_ids", set_cmd(&[internal, system])),
            ]),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert_eq!(memberships(&client, me).await, [internal, system]);
    let mut tx = begin(&orm, me).await;
    let sudo = tx
        .env()
        .sudo(true)
        .records("res.users")
        .unwrap()
        .browse([admin])
        .unwrap();
    sudo.write_values(&mut tx, &map(&[("login", s("admin2"))]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let login: String = client
        .query_one("SELECT login FROM res_users WHERE id = $1", &[&admin])
        .await
        .unwrap()
        .get(0);
    assert_eq!(login, "admin2");
    drop(client);
    h.cleanup().await.unwrap();
}
