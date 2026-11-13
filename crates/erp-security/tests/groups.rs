//! through the whole engine: the policy loaded from the database tables
//! (`res_groups`, `res_groups_implied_rel`, `res_groups_privilege`, `ir_access`), compiled and
//! decided on for resolved users. and 011 (the group lists of the model-level error
//! and `allowing_groups`), 023 (the field error with the groups' full names), 025 (`has_groups`)
//! and 026 (`res.users.has_group` on other users).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_fields::{ErrorKind, Value};
use erp_orm::{AccessPolicy, Operation, Orm, PolicyProvider, Transaction, UserId};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::{Harness, assert_query_count};
use support::{connect, pool};

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "x_groups",
    depends: &["base"],
    exact_only: false,
};

const OBJ: &str = "test_access_right.some_obj";
const CATEG: &str = "test_access_right.obj_categ";
const DESCRIPTION: &str = "Object For Test Access Right";

const OBJ_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::integer("val"),
    FieldDecl::many2one("categ_id", CATEG),
    FieldDecl::char("forbidden").groups("base.group_portal,x_groups.group_test"),
    FieldDecl::char("forbidden2").groups("x_groups.group_test"),
    FieldDecl::char("forbidden3").groups("."),
    FieldDecl::char("not_portal").groups("!base.group_portal"),
];

async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON, &ADDON])
        .with_decls([
            erp_data::ir_model_data_decl(),
            ModelDecl::new("x_groups", CATEG)
                .description("Object Category")
                .fields(const { &[FieldDecl::char("name")] }),
            ModelDecl::new("x_groups", OBJ)
                .description(DESCRIPTION)
                .fields(OBJ_FIELDS),
        ])
        .build()
        .await
        .expect("harness")
}

fn vals(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

async fn exec(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) {
    client.execute(sql, params).await.unwrap();
}

/// The ids of the fixture's groups and models.
struct Ids {
    user: i64,
    portal: i64,
    no_one: i64,
    everyone: i64,
    test_group: i64,
    group0: i64,
    group1: i64,
    group2: i64,
    group3: i64,
    obj: i64,
    categ: i64,
    record: i64,
}

async fn group(
    client: &Client,
    name: &str,
    xmlid: Option<(&str, &str)>,
    privilege: Option<i64>,
    share: bool,
) -> i64 {
    let id = one(
        client,
        "INSERT INTO res_groups (name, share, privilege_id) VALUES ($1, $2, $3) RETURNING id::int8",
        &[&name, &share, &privilege],
    )
    .await;
    if let Some((module, xml_name)) = xmlid {
        exec(
            client,
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ($1, $2, 'res.groups', $3)",
            &[&module, &xml_name, &id],
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

async fn seed(client: &Client) -> Ids {
    let role = one(
        client,
        "INSERT INTO res_groups_privilege (name) VALUES ('Role') RETURNING id::int8",
        &[],
    )
    .await;
    let user = group(
        client,
        "Internal User",
        Some(("base", "group_user")),
        None,
        false,
    )
    .await;
    let portal = group(
        client,
        "Portal",
        Some(("base", "group_portal")),
        Some(role),
        true,
    )
    .await;
    let no_one = group(
        client,
        "Technical Features",
        Some(("base", "group_no_one")),
        None,
        false,
    )
    .await;
    let everyone = group(
        client,
        "Everyone",
        Some(("base", "group_everyone")),
        None,
        false,
    )
    .await;
    let test_group = group(
        client,
        "Test Group",
        Some(("x_groups", "group_test")),
        None,
        false,
    )
    .await;
    let group0 = group(client, "Group 0", None, None, false).await;
    let group1 = group(client, "Group 1", None, None, false).await;
    let group2 = group(client, "Group 2", None, None, false).await;
    let group3 = group(client, "Group 3", None, None, false).await;
    for (gid, hid) in [(user, everyone), (portal, everyone)] {
        exec(
            client,
            "INSERT INTO res_groups_implied_rel (gid, hid) VALUES ($1, $2)",
            &[&gid, &hid],
        )
        .await;
    }
    let obj = model(client, OBJ, DESCRIPTION).await;
    let categ = model(client, CATEG, "Object Category").await;
    let record = one(
        client,
        "INSERT INTO test_access_right_some_obj (name, val) VALUES ('record', 5) RETURNING id::int8",
        &[],
    )
    .await;
    Ids {
        user,
        portal,
        no_one,
        everyone,
        test_group,
        group0,
        group1,
        group2,
        group3,
        obj,
        categ,
        record,
    }
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

async fn bob(client: &Client, groups: &[i64]) -> i64 {
    new_user(client, "bob", "Bob Bobman", groups).await
}

async fn new_user(client: &Client, login: &str, name: &str, groups: &[i64]) -> i64 {
    let uid = one(
        client,
        "INSERT INTO res_users (name, login, active, share) VALUES ($1, $2, true, false) \
         RETURNING id::int8",
        &[&name, &login],
    )
    .await;
    for group in groups {
        exec(
            client,
            "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
            &[&uid, group],
        )
        .await;
    }
    uid
}

async fn make_orm(h: &Harness) -> Orm {
    // installation emits the generated company restrictions; this database has no installation
    let client = connect(h).await;
    support::insert_generated(&client, h.registry(), &["res.partner", "res.users"]).await;
    drop(client);
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

fn not_allowed(verb: &str, groups: &[&str]) -> String {
    let info = if groups.is_empty() {
        "No group currently allows this operation.".to_owned()
    } else {
        let list: Vec<String> = groups.iter().map(|g| format!("\t- {g}")).collect();
        format!(
            "This operation is allowed for the following groups:\n{}",
            list.join("\n")
        )
    };
    format!(
        "You are not allowed to {verb} '{DESCRIPTION}' ({OBJ}) records.\n\n{info}\n\nContact \
         your administrator to request access if necessary."
    )
}

#[track_caller]
fn assert_access(error: &erp_fields::Error, message: &str) {
    assert_eq!(error.kind(), ErrorKind::Access, "{error}");
    assert_eq!(error.message(), message);
}

/// `test_no_groups`, `test_one_group`, `test_two_groups` of `TestACLFeedback` (Group 1 reads,
/// Group 0 creates and reads; Bob is in neither) and `test_error_message_no_access` (Group 3
/// creates and deletes), through the ORM from the database tables.
#[tokio::test]
async fn spec_sec_008_model_level_error_message() {
    let h = harness().await;
    let client = connect(&h).await;
    let ids = seed(&client).await;
    access(&client, "read", ids.obj, ids.group1, "r", None).await;
    access(&client, "create-and-read", ids.obj, ids.group0, "cr", None).await;
    let bob = bob(&client, &[ids.group2, ids.user]).await;
    let orm = make_orm(&h).await;
    let mut tx = begin(&orm, bob).await;
    let records = tx.env().records(OBJ).unwrap();
    let record = records.browse([ids.record]).unwrap();

    // test_no_groups: nobody may write
    let error = record
        .write_values(&mut tx, &vals(&[("val", Value::Int(10))]))
        .await
        .unwrap_err();
    assert_access(&error, &not_allowed("modify", &[]));
    // test_one_group: Group 0 may create
    let error = records
        .create_values(&mut tx, &[vals(&[("val", Value::Int(1))])])
        .await
        .unwrap_err();
    assert_access(&error, &not_allowed("create", &["Group 0"]));
    // test_two_groups: Group 0 and Group 1 may read, whichever way it is read
    let expected = not_allowed("access", &["Group 0", "Group 1"]);
    assert_access(&record.value(&mut tx, "val").await.unwrap_err(), &expected);
    assert_access(
        &record.read(&mut tx, &["val"]).await.unwrap_err(),
        &expected,
    );
    // nobody may delete
    assert_access(
        &record.unlink(&mut tx).await.unwrap_err(),
        &not_allowed("delete", &[]),
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();

    // test_error_message_no_access: Group 3 may create and delete
    let h = harness().await;
    let client = connect(&h).await;
    let ids = seed(&client).await;
    access(&client, "cd", ids.obj, ids.group3, "cd", None).await;
    let bob = self::bob(&client, &[ids.group2, ids.user]).await;
    let orm = make_orm(&h).await;
    let mut tx = begin(&orm, bob).await;
    let record = tx.env().records(OBJ).unwrap().browse([ids.record]).unwrap();
    assert_access(
        &record.value(&mut tx, "val").await.unwrap_err(),
        &not_allowed("access", &[]),
    );
    assert_access(
        &record
            .write_values(&mut tx, &vals(&[("val", Value::Int(2))]))
            .await
            .unwrap_err(),
        &not_allowed("modify", &[]),
    );
    assert_access(
        &record.unlink(&mut tx).await.unwrap_err(),
        &not_allowed("delete", &["Group 3"]),
    );
    assert_access(
        &tx.env()
            .records(OBJ)
            .unwrap()
            .create_values(&mut tx, &[vals(&[("val", Value::Int(2))])])
            .await
            .unwrap_err(),
        &not_allowed("create", &["Group 3"]),
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// The groups allowing `op` on `model`, as ids, from a policy freshly built over the database.
async fn allowing(h: &Harness, model: &str, op: Operation) -> Vec<i64> {
    let source = PolicySource::build(Arc::clone(h.registry()), pool(h, 2))
        .await
        .unwrap();
    let snapshot = source.current();
    let spec = h.registry().model(model).unwrap();
    let mut ids: Vec<i64> = snapshot
        .allowing_groups(spec, op)
        .unwrap()
        .iter()
        .map(|g| g.get())
        .collect();
    ids.sort_unstable();
    ids
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// `test_get_groups_with_access` step by step on database rows: the `access` operator
/// intersecting, uniting and subtracting the comodel's groups, with `res.groups` records whose
/// implications are in `res_groups_implied_rel`.
#[tokio::test]
async fn spec_sec_011_groups_that_would_allow_an_operation() {
    let h = harness().await;
    let client = connect(&h).await;
    let ids = seed(&client).await;
    // Group 2 implies the internal user type, which implies everyone
    exec(
        &client,
        "INSERT INTO res_groups_implied_rel (gid, hid) VALUES ($1, $2)",
        &[&ids.group2, &ids.user],
    )
    .await;
    // the comodel: readable by internal users, writable by Group 2
    access(&client, "categ read", ids.categ, ids.user, "r", None).await;
    access(&client, "categ write", ids.categ, ids.group2, "u", None).await;
    let get = |op| allowing(&h, OBJ, op);
    assert!(get(Operation::Read).await.is_empty());

    // simple group
    access(
        &client,
        "Good guys",
        ids.obj,
        ids.group1,
        "r",
        Some("[('id', 'in', [1, 2, 3, 4, 5])]"),
    )
    .await;
    assert_eq!(get(Operation::Read).await, [ids.group1]);

    // using the access operator (group everyone); everyone itself is not a comodel reader
    let comodel = allowing(&h, CATEG, Operation::Read).await;
    assert_eq!(comodel, sorted(vec![ids.user, ids.group2]));
    assert!(!comodel.contains(&ids.everyone));
    let delegated = one(
        &client,
        "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
         VALUES ('Delegated permission', $1, $2, 'r', \
                 '[(''categ_id'', ''access'', ''read'')]', true) RETURNING id::int8",
        &[&ids.obj, &ids.everyone],
    )
    .await;
    let set_rule = async |group: i64, domain: &str| {
        exec(
            &client,
            "UPDATE ir_access SET group_id = $2, domain = $3 WHERE id = $1",
            &[&delegated, &group, &domain],
        )
        .await;
    };
    let union = |a: &[i64], b: &[i64]| sorted([a, b].concat());
    let groups = get(Operation::Read).await;
    assert_eq!(groups, union(&[ids.group1], &comodel));
    assert!(
        !groups.contains(&ids.everyone),
        "everyone is not part of the group"
    );

    // a restricted group: its implying groups that also read the comodel
    set_rule(ids.user, "[('categ_id', 'access', 'read')]").await;
    assert_eq!(
        get(Operation::Read).await,
        sorted(vec![ids.group1, ids.user, ids.group2])
    );

    // the access condition is necessary
    set_rule(
        ids.everyone,
        "['&', ('categ_id', 'access', 'read'), ('id', '>', 5)]",
    )
    .await;
    assert_eq!(get(Operation::Read).await, union(&[ids.group1], &comodel));

    // not necessary: every group implying everyone
    set_rule(
        ids.everyone,
        "['|', ('categ_id', 'access', 'read'), ('id', '>', 5)]",
    )
    .await;
    assert_eq!(
        get(Operation::Read).await,
        sorted(vec![
            ids.group1,
            ids.everyone,
            ids.user,
            ids.portal,
            ids.group2
        ])
    );

    // two necessary conditions intersect
    let write = allowing(&h, CATEG, Operation::Write).await;
    set_rule(
        ids.everyone,
        "['&', ('categ_id', 'access', 'read'), ('categ_id', 'access', 'write')]",
    )
    .await;
    let both: Vec<i64> = comodel
        .iter()
        .copied()
        .filter(|g| write.contains(g))
        .collect();
    assert_eq!(get(Operation::Read).await, union(&[ids.group1], &both));

    // a negated condition subtracts
    set_rule(
        ids.everyone,
        "['&', ('categ_id', 'access', 'read'), '!', ('categ_id', 'access', 'write')]",
    )
    .await;
    let minus: Vec<i64> = comodel
        .iter()
        .copied()
        .filter(|g| !write.contains(g))
        .collect();
    assert_eq!(get(Operation::Read).await, union(&[ids.group1], &minus));
    drop(client);
    h.cleanup().await.unwrap();
}

/// `TestFieldGroupFeedback::test_read` / `test_write`: an internal user in debug mode with the
/// technical group is told which groups would allow the field, by their full names.
#[tokio::test]
async fn spec_sec_023_field_access_error_message() {
    let h = harness().await;
    let client = connect(&h).await;
    let ids = seed(&client).await;
    access(&client, "internal", ids.obj, ids.user, "crud", None).await;
    let bob = bob(&client, &[ids.user, ids.no_one]).await;
    let plain = new_user(&client, "plain", "Plain", &[ids.user]).await;
    let orm = make_orm(&h).await;
    let message = |uid: i64, field: &str, op: &str, groups: Option<&str>| {
        let mut text = format!(
            "You do not have enough rights to access the field \"{field}\" on {DESCRIPTION} \
             ({OBJ}). Please contact your system administrator.\n\nOperation: {op}"
        );
        if let Some(groups) = groups {
            text.push_str(&format!("\nUser: {uid}\nGroups: {groups}"));
        }
        text
    };
    let mut tx = begin(&orm, bob).await;
    let debug = tx.env().with_debug(true);
    let record = debug.records(OBJ).unwrap().browse([ids.record]).unwrap();
    let allowed = "allowed for groups 'Role / Portal', 'Test Group'";
    assert_access(
        &record.value(&mut tx, "forbidden").await.unwrap_err(),
        &message(bob, "forbidden", "read", Some(allowed)),
    );
    assert_access(
        &record.value(&mut tx, "forbidden3").await.unwrap_err(),
        &message(bob, "forbidden3", "read", Some("always forbidden")),
    );
    // a field closed to portal users only is open to Bob
    record.value(&mut tx, "not_portal").await.unwrap();
    // `fields_get` leaves out what Bob may not read, without a statement
    let shown = assert_query_count!(tx, <= 0, {
        record.fields_get(&mut tx, None, Some(&["string"])).await.unwrap()
    });
    let names: Vec<&str> = shown.keys().map(String::as_str).collect();
    assert!(
        names.contains(&"not_portal") && names.contains(&"name"),
        "{names:?}"
    );
    for hidden in ["forbidden", "forbidden2", "forbidden3"] {
        assert!(!names.contains(&hidden), "{hidden} in {names:?}");
    }
    // write: the first denied key
    let error = record
        .write_values(
            &mut tx,
            &vals(&[
                ("forbidden", Value::str("a")),
                ("forbidden2", Value::str("b")),
            ]),
        )
        .await
        .unwrap_err();
    assert_access(&error, &message(bob, "forbidden", "write", Some(allowed)));
    // outside debug mode the lines are absent
    let quiet = tx.env().records(OBJ).unwrap().browse([ids.record]).unwrap();
    assert_access(
        &quiet.value(&mut tx, "forbidden").await.unwrap_err(),
        &message(bob, "forbidden", "read", None),
    );
    drop(tx);
    // a user without the technical group is told nothing, debug mode or not
    let mut tx = begin(&orm, plain).await;
    let debug = tx.env().with_debug(true);
    let record = debug.records(OBJ).unwrap().browse([ids.record]).unwrap();
    assert_access(
        &record.value(&mut tx, "forbidden").await.unwrap_err(),
        &message(plain, "forbidden", "read", None),
    );
    // the same user is let in by the group of the field
    exec(
        &client,
        "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
        &[&plain, &ids.test_group],
    )
    .await;
    drop(tx);
    let mut tx = begin(&orm, plain).await;
    let record = tx.env().records(OBJ).unwrap().browse([ids.record]).unwrap();
    record.value(&mut tx, "forbidden").await.unwrap();
    record.value(&mut tx, "forbidden2").await.unwrap();
    assert_access(
        &record.value(&mut tx, "forbidden3").await.unwrap_err(),
        &message(plain, "forbidden3", "read", None),
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// A user in `g1` only: `has_groups` of `test_env_uid` / `test_record` and the specs around
/// them, and the technical group (`base.group_no_one`) counting only in debug mode. No
/// statements.
#[tokio::test]
async fn spec_sec_025_group_spec_semantics() {
    let h = harness().await;
    let client = connect(&h).await;
    let ids = seed(&client).await;
    let g1 = group(&client, "g1", Some(("x_groups", "g1")), None, false).await;
    group(&client, "g2", Some(("x_groups", "g2")), None, false).await;
    let user = new_user(&client, "g1_only", "G1 Only", &[g1]).await;
    let technical = new_user(&client, "tech", "Tech", &[g1, ids.no_one]).await;
    let orm = make_orm(&h).await;
    let mut tx = begin(&orm, user).await;
    let env = tx.env();
    for (spec, expected) in [
        ("x_groups.g1", true),
        ("x_groups.g2", false),
        ("x_groups.g1,x_groups.g2", true),
        ("!x_groups.g2", true),
        ("x_groups.g1,!x_groups.g1", false),
        (".", false),
        ("base.group_no_one", false),
    ] {
        let got = assert_query_count!(tx, <= 0, { tx.has_groups(&env, spec).await.unwrap() });
        assert_eq!(got, expected, "{spec}");
    }
    assert!(tx.has_group(&env, "x_groups.g1").await.unwrap());
    assert!(!tx.has_group(&env, "x_groups.g2").await.unwrap());
    // a spec that is not one group is refused by has_group
    let error = tx
        .has_group(&env, "x_groups.g1,x_groups.g2")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    // unknown groups are errors, never false
    let error = tx.has_groups(&env, "x_groups.nobody").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the group spec names the unknown group 'x_groups.nobody'"
    );
    drop(tx);
    // the technical group counts in debug mode only
    let mut tx = begin(&orm, technical).await;
    let env = tx.env();
    assert!(!tx.has_groups(&env, "base.group_no_one").await.unwrap());
    let debug = env.with_debug(true);
    assert!(tx.has_groups(&debug, "base.group_no_one").await.unwrap());
    assert!(
        !tx.has_groups(&debug, "x_groups.g1,!base.group_no_one")
            .await
            .unwrap()
    );
    assert!(
        tx.has_groups(&env, "x_groups.g1,!base.group_no_one")
            .await
            .unwrap()
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// `test_other_user`: a non-internal user may not ask about another user, an internal user and
/// superuser mode may.
#[tokio::test]
async fn spec_sec_026_has_group_on_other_users() {
    let h = harness().await;
    let client = connect(&h).await;
    let ids = seed(&client).await;
    let group0 = group(&client, "group0", Some(("x_groups", "group0")), None, false).await;
    let test_user = new_user(&client, "testuser", "Strawman Test User", &[group0]).await;
    let internal = new_user(&client, "internal", "Internal", &[ids.user]).await;
    let tech_user = new_user(&client, "tech", "Tech", &[ids.no_one]).await;
    let orm = make_orm(&h).await;
    let has_group = async |tx: &mut Transaction, env: &erp_orm::Env, target: i64| {
        let users = env.records("res.users").unwrap().browse([target]).unwrap();
        users
            .call::<(String,), bool>(tx, "has_group", ("x_groups.group0".to_owned(),))
            .await
    };

    let mut tx = begin(&orm, test_user).await;
    let env = tx.env();
    // oneself: no statements
    assert!(assert_query_count!(tx, <= 0, { has_group(&mut tx, &env, test_user).await.unwrap() }));
    let error = has_group(&mut tx, &env, internal).await.unwrap_err();
    assert_access(
        &error,
        "You can ony call user.has_group() with your current user.",
    );
    // superuser mode answers about the other user's groups
    let sudo = env.sudo(true);
    assert!(!has_group(&mut tx, &sudo, internal).await.unwrap());
    assert!(has_group(&mut tx, &sudo, test_user).await.unwrap());
    drop(tx);

    // an internal user may ask about anyone (one statement for another user)
    let mut tx = begin(&orm, internal).await;
    let env = tx.env();
    assert!(!has_group(&mut tx, &env, internal).await.unwrap());
    assert!(assert_query_count!(tx, <= 1, { has_group(&mut tx, &env, test_user).await.unwrap() }));
    // an unknown group is an error
    let users = env
        .records("res.users")
        .unwrap()
        .browse([internal])
        .unwrap();
    let error = users
        .call::<(String,), bool>(&mut tx, "has_group", ("x_groups.nobody".to_owned(),))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    // the technical group follows the asker's debug mode, whoever is asked about
    let technical = async |tx: &mut Transaction, env: &erp_orm::Env| {
        let users = env
            .records("res.users")
            .unwrap()
            .browse([tech_user])
            .unwrap();
        users
            .call::<(String,), bool>(tx, "has_group", ("base.group_no_one".to_owned(),))
            .await
            .unwrap()
    };
    assert!(!technical(&mut tx, &env).await);
    assert!(technical(&mut tx, &env.with_debug(true)).await);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// debug flag; pinned before moved the answer off `with_user`)
#[tokio::test]
async fn has_group_on_a_missing_or_archived_user() {
    let h = harness().await;
    let client = connect(&h).await;
    let ids = seed(&client).await;
    let group0 = group(&client, "group0", Some(("x_groups", "group0")), None, false).await;
    let internal = new_user(&client, "internal", "Internal", &[ids.user]).await;
    let archived = new_user(&client, "archived", "Archived", &[group0, ids.no_one]).await;
    exec(
        &client,
        "UPDATE res_users SET active = false WHERE id = $1",
        &[&archived],
    )
    .await;
    let orm = make_orm(&h).await;
    let mut tx = begin(&orm, internal).await;
    let env = tx.env();
    let ask = async |tx: &mut Transaction, env: &erp_orm::Env, target: i64, group: &str| {
        env.records("res.users")
            .unwrap()
            .browse([target])
            .unwrap()
            .call::<(String,), bool>(tx, "has_group", (group.to_owned(),))
            .await
    };
    let outcomes = [
        ask(&mut tx, &env, archived, "x_groups.group0").await,
        ask(&mut tx, &env, archived, "base.group_no_one").await,
        ask(
            &mut tx,
            &env.with_debug(true),
            archived,
            "base.group_no_one",
        )
        .await,
        ask(&mut tx, &env, 999_999, "x_groups.group0").await,
    ];
    // the principal resolver refuses the other user, whatever the group and the asker's debug
    // flag; the run on the code before the slice is the raw log under
    let archived_refusal = format!("The user principal cannot act: user {archived} is archived.");
    for outcome in &outcomes[..3] {
        assert_access(outcome.as_ref().unwrap_err(), &archived_refusal);
    }
    assert_access(
        outcomes[3].as_ref().unwrap_err(),
        "The user principal cannot act: user 999999 does not exist.",
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

erp_orm::capability!(APPROVE = "x_groups.approve");
erp_orm::capability!(UNMAPPED = "x_groups.unmapped");

/// `Transaction::has_capability` over the `ir_capability` rows: the mapped group spec against the
/// user's groups, an unmapped capability an error, and no statement.
#[tokio::test]
async fn capabilities_follow_the_ir_capability_rows() {
    let h = harness().await;
    let client = connect(&h).await;
    let ids = seed(&client).await;
    exec(
        &client,
        "INSERT INTO ir_capability (name, groups) VALUES ('x_groups.approve', \
         'x_groups.group_test,!base.group_portal')",
        &[],
    )
    .await;
    let member = new_user(&client, "member", "Member", &[ids.test_group]).await;
    let outsider = new_user(&client, "outsider", "Outsider", &[ids.user]).await;
    let orm = make_orm(&h).await;
    let mut tx = begin(&orm, member).await;
    let env = tx.env();
    let got = assert_query_count!(tx, <= 0, { tx.has_capability(&env, &APPROVE).await.unwrap() });
    assert!(got);
    // superuser mode does not grant it
    drop(tx);
    let mut tx = begin(&orm, outsider).await;
    let env = tx.env();
    assert!(!tx.has_capability(&env, &APPROVE).await.unwrap());
    assert!(!tx.has_capability(&env.sudo(true), &APPROVE).await.unwrap());
    let error = tx.has_capability(&env, &UNMAPPED).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.message(),
        "the capability 'x_groups.unmapped' has no ir_capability row in this policy: map it to \
         groups (the build report lists it)"
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}
