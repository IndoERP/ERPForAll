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

mod company_rows_support;
mod users_support;

use std::sync::Arc;

use company_rows_support::UserKind::Internal;
use company_rows_support::fixture_user;
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
    // installation emits the generated company restriction of the two models; these databases
    // have no installation
    company_rows_support::insert_generated(&client, h.registry(), &["res.users"]).await;
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
    let company = company_rows_support::fixture_company(&client).await;
    let admin = fixture_user(&client, "admin", &[internal, system], Internal, company).await;
    let me = fixture_user(&client, "me", &[internal], Internal, company).await;
    let boss = fixture_user(&client, "boss", &[internal, system], Internal, company).await;
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

/// An internal user changes their own default company inside their allowed companies, through
/// the compiled policy: `company_id` is `user_writeable`, `company_ids` is not, and a company the
/// user is not allowed in is refused by the membership check, not by the field mask.
#[tokio::test]
async fn spec_company_031_users_change_their_default_company_within_their_companies() {
    let h = harness().await;
    let client = connect(&h).await;
    let internal = group(&client, "Internal User", "base.group_user").await;
    // the field mask of res.users looks the group up by its external id
    group(&client, "Administrator", "base.group_system").await;
    let users_model = model(&client, "res.users", "User").await;
    company_rows_support::insert_generated(&client, h.registry(), &["res.users"]).await;
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
    let currency = erp_testkit::fixtures::seed_currency(&client).await.unwrap();
    let mut companies = Vec::new();
    for name in ["Alpha", "Beta", "Gamma"] {
        companies.push(
            one(
                &client,
                "INSERT INTO res_company (name, currency_id, active) VALUES ($1, $2, true) \
                 RETURNING id::int8",
                &[&name, &currency],
            )
            .await,
        );
    }
    let [a, b, c] = [companies[0], companies[1], companies[2]];
    // default company A, allowed in A and B
    let me = fixture_user(&client, "me", &[internal], Internal, a).await;
    exec(
        &client,
        "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
        &[&me, &b],
    )
    .await;
    let orm = production_orm(&h).await;

    let mut tx = begin(&orm, me).await;
    let own = tx.env().records("res.users").unwrap().browse([me]).unwrap();
    // inside the allowed companies
    own.write_values(&mut tx, &map(&[("company_id", int(b))]))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let stored: i64 = one(
        &client,
        "SELECT company_id::int8 FROM res_users WHERE id = $1",
        &[&me],
    )
    .await;
    assert_eq!(stored, b);

    // the allowed companies stay with the administrators
    let mut tx = begin(&orm, me).await;
    let own = tx.env().records("res.users").unwrap().browse([me]).unwrap();
    let err: Error = own
        .write_values(&mut tx, &map(&[("company_ids", set_cmd(&[a, b, c]))]))
        .await
        .unwrap_err();
    assert_error(&err, ErrorKind::Access, &denied("company_ids"));
    drop(tx);

    // a company outside them
    let mut tx = begin(&orm, me).await;
    let own = tx.env().records("res.users").unwrap().browse([me]).unwrap();
    let err: Error = own
        .write_values(&mut tx, &map(&[("company_id", int(c))]))
        .await
        .unwrap_err();
    assert_error(
        &err,
        ErrorKind::Validation,
        "The default company of the user me is not one of the user's allowed companies \
         (Alpha, Beta).",
    );
    // the company the user may not read is not named
    assert!(!err.to_string().contains("Gamma"), "{err}");
    drop(tx);
    let stored: i64 = one(
        &client,
        "SELECT company_id::int8 FROM res_users WHERE id = $1",
        &[&me],
    )
    .await;
    assert_eq!(stored, b);
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn audit_group_selection_refreshes_database_membership() {
    let h = harness().await;
    let client = connect(&h).await;
    let internal = group(&client, "Internal", "base.group_user").await;
    let selected = group(&client, "Audit selected", "test.audit_selected").await;
    let parent = group(&client, "Audit parent", "test.audit_parent").await;
    exec(
        &client,
        "INSERT INTO res_groups_implied_rel (gid, hid) VALUES ($1, $2)",
        &[&parent, &selected],
    )
    .await;
    let company = company_rows_support::fixture_company(&client).await;
    let uid = fixture_user(
        &client,
        "audit-selected",
        &[internal, parent],
        Internal,
        company,
    )
    .await;
    let orm = production_orm(&h).await;
    let ids = [erp_orm::QualifiedXmlId::parse("test.audit_selected").unwrap()];
    let mut tx = begin(&orm, uid).await;
    let env = tx.env();
    assert!(
        tx.group_membership(&env)
            .await
            .unwrap()
            .has_any_effective_xmlid(&ids)
            .unwrap()
    );
    tx.rollback().await.unwrap();
    exec(
        &client,
        "DELETE FROM res_groups_users_rel WHERE uid=$1 AND gid=$2",
        &[&uid, &parent],
    )
    .await;
    let mut tx = begin(&orm, uid).await;
    let env = tx.env();
    assert!(
        !tx.group_membership(&env)
            .await
            .unwrap()
            .has_any_effective_xmlid(&ids)
            .unwrap()
    );
    tx.rollback().await.unwrap();
    drop(client);
    drop(orm);
    h.cleanup().await.unwrap();
}
