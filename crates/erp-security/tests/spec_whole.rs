//! Whole-case tests of the access engine's spec cases that were first delivered in halves
//! (COMPILE, SOURCE): SEC-015, SEC-045, SEC-046, SEC-047 and DOM-035. Every rule is an
//! `ir_access` row read by the real `PolicySource`, compiled into the snapshot and enforced by
//! the ORM entry points against PostgreSQL for principals the real `Resolver` builds from
//! `res_groups_users_rel`. Expected values are derived by the same ORM as superuser (a search of
//! the combined domain, a `filtered_domain`, the results of an unrestricted search); the texts
//! that are the spec's are asserted whole.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_domain::{Domain, Operator, OptimizationLevel, Value as DomainValue};
use erp_fields::{ErrorKind, OnDelete, Value};
use erp_orm::{Operation, Orm, PolicyProvider, SearchOptions, Transaction, UserId};
use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, StaticValue};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::{Harness, assert_query_count};
use support::{connect, pool};

static FIXTURE: AddonDescriptor = AddonDescriptor {
    name: "x_spec",
    depends: &["base"],
    exact_only: false,
};

const CATEG: &str = "x.categ";
const OBJ: &str = "x.obj";
const CONTAINER: &str = "x.container";
const CHILD: &str = "x.child";
const UNIT: &str = "x.unit";
const BOX: &str = "x.box";

/// Model, its `ir.model` name (the description the messages use).
const MODELS: [(&str, &str); 6] = [
    (CATEG, "Spec Category"),
    (OBJ, "Spec Object"),
    (CONTAINER, "Spec Container"),
    (CHILD, "Spec Child"),
    (UNIT, "Spec Unit"),
    (BOX, "Spec Box"),
];

const CATEG_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::boolean("active").default(StaticValue::Bool(true)),
];
const OBJ_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::integer("val"),
    FieldDecl::many2one("categ_id", CATEG),
];
const CONTAINER_FIELDS: &[FieldDecl] = &[FieldDecl::many2many("some_ids", OBJ)];
const CHILD_FIELDS: &[FieldDecl] = &[FieldDecl::char("name"), FieldDecl::many2one("obj_id", OBJ)];
const UNIT_FIELDS: &[FieldDecl] = &[FieldDecl::char("state"), FieldDecl::char("readonly_name")];
const BOX_FIELDS: &[FieldDecl] = &[
    FieldDecl::many2one("unit_id", UNIT)
        .required(true)
        .ondelete(OnDelete::Cascade),
    FieldDecl::integer("size"),
];

async fn harness() -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &FIXTURE,
    ])
    .with_decls([
        erp_data::ir_model_data_decl(),
        ModelDecl::new("x_spec", CATEG)
            .description("Spec Category")
            .fields(CATEG_FIELDS),
        ModelDecl::new("x_spec", OBJ)
            .description("Spec Object")
            .fields(OBJ_FIELDS),
        ModelDecl::new("x_spec", CONTAINER)
            .description("Spec Container")
            .fields(CONTAINER_FIELDS),
        ModelDecl::new("x_spec", CHILD)
            .description("Spec Child")
            .fields(CHILD_FIELDS),
        ModelDecl::new("x_spec", UNIT)
            .description("Spec Unit")
            .fields(UNIT_FIELDS),
        ModelDecl::new("x_spec", BOX)
            .description("Spec Box")
            .inherits(&[(UNIT, "unit_id")])
            .fields(BOX_FIELDS),
    ])
    .build()
    .await
    .expect("harness")
}

/// SQL compared with whitespace runs collapsed.
fn norm(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The part of a normalized statement between `FROM` and `WHERE`.
fn from_of(sql: &str) -> String {
    let sql = norm(sql);
    let before = sql.split_once(" WHERE ").unwrap().0;
    before.split_once(" FROM ").unwrap().1.to_owned()
}

/// The WHERE clause of a normalized statement.
fn where_of(sql: &str) -> String {
    let sql = norm(sql);
    let after = sql.split_once(" WHERE ").unwrap().1;
    after.split_once(" ORDER BY ").unwrap().0.to_owned()
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

/// The committed policy base: three groups and three users (`bob` internal, `eve` internal and
/// elevated, `pat` portal), the fixture models' `ir.model` rows.
struct World {
    internal: i64,
    elevated: i64,
    bob: i64,
    eve: i64,
    pat: i64,
    models: BTreeMap<&'static str, i64>,
}

async fn seed(client: &Client) -> World {
    let group = async |name: &str, share: bool| -> i64 {
        one(
            client,
            "INSERT INTO res_groups (name, share) VALUES ($1, $2) RETURNING id::int8",
            &[&name, &share],
        )
        .await
    };
    let internal = group("Internal User", false).await;
    let elevated = group("Elevated", false).await;
    let portal = group("Portal", true).await;
    client
        .execute(
            "INSERT INTO ir_model_data (module, name, model, res_id) \
             VALUES ('base', 'group_user', 'res.groups', $1), \
                    ('base', 'group_portal', 'res.groups', $2)",
            &[&internal, &portal],
        )
        .await
        .unwrap();
    let user = async |name: &str, share: bool, groups: &[i64]| -> i64 {
        let id = one(
            client,
            "INSERT INTO res_users (name, login, active, share) \
             VALUES ($1, $1, true, $2) RETURNING id::int8",
            &[&name, &share],
        )
        .await;
        for gid in groups {
            client
                .execute(
                    "INSERT INTO res_groups_users_rel (uid, gid) VALUES ($1, $2)",
                    &[&id, gid],
                )
                .await
                .unwrap();
        }
        id
    };
    let bob = user("Bob Bobman", false, &[internal]).await;
    let eve = user("Eve Elevated", false, &[internal, elevated]).await;
    let pat = user("Pat Portal", true, &[portal]).await;
    let mut models = BTreeMap::new();
    for (model, name) in MODELS {
        let id = one(
            client,
            "INSERT INTO ir_model (name, model, state, transient) \
             VALUES ($1, $2, 'base', false) RETURNING id::int8",
            &[&name, &model],
        )
        .await;
        models.insert(model, id);
    }
    World {
        internal,
        elevated,
        bob,
        eve,
        pat,
        models,
    }
}

/// One `ir_access` row: a permission of `group`, or a restriction for everyone when `None`.
async fn rule(
    client: &Client,
    w: &World,
    model: &str,
    group: Option<i64>,
    operation: &str,
    domain: Option<&str>,
) {
    client
        .execute(
            "INSERT INTO ir_access (name, model_id, group_id, operation, domain, active) \
             VALUES ($1, $2, $3, $4, $5, true)",
            &[
                &format!("{model} {operation} {domain:?}"),
                &w.models[model],
                &group,
                &operation,
                &domain,
            ],
        )
        .await
        .unwrap();
}

/// The ORM deciding with the policy read from the rows now in the database.
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

fn row(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.clone()))
        .collect()
}

/// Rows created as superuser, committed.
async fn create(orm: &Orm, model: &str, rows: &[BTreeMap<String, Value>]) -> Vec<i64> {
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let ids = tx
        .env()
        .records(model)
        .unwrap()
        .create_values(&mut tx, rows)
        .await
        .unwrap()
        .ids();
    tx.commit().await.unwrap();
    ids
}

async fn begin_as(orm: &Orm, uid: i64) -> Transaction {
    orm.begin(UserId::new(uid).unwrap()).await.unwrap()
}

fn cond(field: &str, op: Operator, value: impl Into<DomainValue>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

fn refused(user: &str, uid: i64, label: &str) -> String {
    format!(
        "Access rules protect some of these records.\n\n{user} (id={uid}) has no 'read' access to:\n- {label}\n\nAsk an administrator if you need access to these records."
    )
}

/// The SQL of the last statement of `search` (a single statement is asserted by the caller).
async fn sql_of(
    tx: &mut Transaction,
    model: &str,
    domain: &Domain,
    opts: &SearchOptions,
) -> (Vec<i64>, Vec<String>) {
    let records = tx.env().records(model).unwrap();
    tx.capture_statements(true);
    let found = records.search_with(tx, domain, opts).await.unwrap().ids();
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    (found, statements)
}

/// The rule is compiled into a sub-query of literal conditions, so every context
/// sees the same rows and the same statement, and an archived category stays out of the rule's
/// reach even when the caller asks for archived records.
#[tokio::test]
async fn spec_sec_015_context_does_not_affect_access_domains() {
    let h = harness().await;
    let client = connect(&h).await;
    // Non-default context languages must exist and be active before query admission.
    client.batch_execute(
        "INSERT INTO res_lang (name,code,url_code,active,direction,date_format,time_format,week_start,grouping,decimal_point) \
         VALUES ('French','fr_FR','fr',true,'ltr','%m/%d/%Y','%H:%M:%S','7','[3,0]','.')",
    ).await.unwrap();
    let w = seed(&client).await;
    rule(&client, &w, CATEG, Some(w.internal), "r", None).await;
    rule(&client, &w, OBJ, Some(w.internal), "crud", None).await;
    rule(
        &client,
        &w,
        OBJ,
        None,
        "crud",
        Some("[('categ_id', 'in', user.env['x.categ'].search([]).ids)]"),
    )
    .await;
    let orm = policy_orm(&h).await;
    let categs = create(
        &orm,
        CATEG,
        &[
            row(&[("name", Value::str("Food"))]),
            row(&[("name", Value::str("Old")), ("active", Value::Bool(false))]),
        ],
    )
    .await;
    let objs = create(
        &orm,
        OBJ,
        &[
            row(&[
                ("name", Value::str("food")),
                ("categ_id", Value::Int(categs[0])),
            ]),
            row(&[
                ("name", Value::str("archived")),
                ("categ_id", Value::Int(categs[1])),
            ]),
            row(&[("name", Value::str("none"))]),
        ],
    )
    .await;

    let contexts: Vec<Vec<(&str, Value)>> = vec![
        vec![],
        vec![("only_media", Value::Bool(true))],
        vec![("active_test", Value::Bool(false))],
        vec![("tz", Value::str("Pacific/Kiritimati"))],
        vec![
            ("lang", Value::str("fr_FR")),
            ("active_test", Value::Bool(false)),
            ("only_media", Value::Bool(true)),
        ],
    ];
    let mut seen: Vec<(Vec<i64>, Vec<String>)> = Vec::new();
    for context in contexts {
        let mut tx = begin_as(&orm, w.bob).await;
        let env = tx.env().with_context(context.clone());
        let records = env.records(OBJ).unwrap();
        // Admit language metadata outside the access-predicate statement comparison. The
        // subsequent search must still be exactly one statement with identical rule SQL.
        let admitted = records.search(&mut tx, &Domain::TRUE).await.unwrap().ids();
        tx.capture_statements(true);
        let found = records.search(&mut tx, &Domain::TRUE).await.unwrap().ids();
        let statements = tx.captured().to_vec();
        tx.capture_statements(false);
        assert_eq!(admitted, found);
        assert_eq!(statements.len(), 1, "{context:?}: {statements:#?}");
        seen.push((found, statements));
        drop(tx);
    }
    // the expectation is the superuser's search of the rule's own meaning: records whose
    // category is among the active ones
    let mut su = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let active = su
        .env()
        .records(CATEG)
        .unwrap()
        .search(&mut su, &Domain::TRUE)
        .await
        .unwrap()
        .ids();
    assert_eq!(active, [categs[0]], "only the active category is searched");
    let expected = su
        .env()
        .records(OBJ)
        .unwrap()
        .search(
            &mut su,
            &cond("categ_id", Operator::In, DomainValue::list(active)),
        )
        .await
        .unwrap()
        .ids();
    drop(su);
    assert_eq!(expected, [objs[0]]);
    for (found, statements) in &seen {
        assert_eq!(*found, expected);
        assert_eq!(statements, &seen[0].1, "the context never reaches the SQL");
    }
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_045_parent_rule_in_a_child_search_ordered_by_an_inherited_field() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    rule(&client, &w, BOX, Some(w.internal), "crud", None).await;
    rule(
        &client,
        &w,
        UNIT,
        Some(w.internal),
        "r",
        Some("[('state', '=', 'a')]"),
    )
    .await;
    let orm = policy_orm(&h).await;
    // b sorts before a: an unfiltered ordered search would list b first
    let boxes = create(
        &orm,
        BOX,
        &[
            row(&[
                ("state", Value::str("a")),
                ("readonly_name", Value::str("zeta")),
                ("size", Value::Int(1)),
            ]),
            row(&[
                ("state", Value::str("b")),
                ("readonly_name", Value::str("alpha")),
                ("size", Value::Int(2)),
            ]),
        ],
    )
    .await;
    let by_id = cond("id", Operator::In, DomainValue::list(boxes.clone()));
    let opts = SearchOptions::new().order("readonly_name");

    // superuser: both boxes, ordered by the inherited name; and the SQL of the same search
    // with the rule's condition written by hand on the inherited field
    let mut su = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let (all, _) = sql_of(&mut su, BOX, &by_id, &opts).await;
    assert_eq!(all, [boxes[1], boxes[0]], "the order is by readonly_name");
    // the search alone and the inherited condition alone, as the pieces of the expected statement
    let (_, id_sql) = sql_of(&mut su, BOX, &by_id, &opts).await;
    let (_, state_sql) = sql_of(&mut su, BOX, &cond("state", Operator::Eq, "a"), &opts).await;
    drop(su);

    let mut tx = begin_as(&orm, w.bob).await;
    let (found, statements) = sql_of(&mut tx, BOX, &by_id, &opts).await;
    assert_eq!(found, [boxes[0]], "only box a");
    assert_eq!(statements.len(), 1, "one query: {statements:#?}");
    let sql = norm(&statements[0]);
    assert_eq!(sql.matches("JOIN").count(), 1, "one join: {sql}");
    assert_eq!(sql.matches("SELECT").count(), 1, "no sub-select: {sql}");
    let (before_order, order) = sql.split_once(" ORDER BY ").unwrap();
    assert!(order.contains("readonly_name"), "{sql}");
    assert!(
        before_order.contains("\"state\""),
        "the rule is a predicate of the WHERE clause: {sql}"
    );
    // the statement is the search's join and ordering with the rule as a second conjunct
    assert_eq!(from_of(&statements[0]), from_of(&id_sql[0]));
    assert_eq!(
        where_of(&statements[0]),
        format!(
            "{} AND {}",
            where_of(&id_sql[0]),
            where_of(&state_sql[0]).replace("$1", "$2")
        )
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_sec_046_read_access_domain_is_part_of_the_search_statement() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    rule(&client, &w, CATEG, Some(w.internal), "r", None).await;
    rule(
        &client,
        &w,
        OBJ,
        Some(w.internal),
        "r",
        Some("[('categ_id.name', '=', 'food')]"),
    )
    .await;
    let orm = policy_orm(&h).await;
    let categs = create(
        &orm,
        CATEG,
        &[
            row(&[("name", Value::str("food"))]),
            row(&[("name", Value::str("drink"))]),
        ],
    )
    .await;
    let obj = |name: &str, val: i64, categ: Option<i64>| {
        let mut r = row(&[("name", Value::str(name)), ("val", Value::Int(val))]);
        if let Some(c) = categ {
            r.insert("categ_id".to_owned(), Value::Int(c));
        }
        r
    };
    let objs = create(
        &orm,
        OBJ,
        &[
            obj("a", 1, Some(categs[0])),
            obj("zero", 0, Some(categs[0])),
            obj("drink", 2, Some(categs[1])),
            obj("none", 3, None),
            obj("e", 5, Some(categs[0])),
        ],
    )
    .await;
    let search = cond("val", Operator::Ne, false);
    let path = cond("categ_id.name", Operator::Eq, "food");

    let mut su = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let (unrestricted, search_sql) = sql_of(&mut su, OBJ, &search, &SearchOptions::new()).await;
    let (expected, by_hand) = sql_of(
        &mut su,
        OBJ,
        &Domain::and([search.clone(), path.clone()]),
        &SearchOptions::new(),
    )
    .await;
    let (_, path_sql) = sql_of(&mut su, OBJ, &path, &SearchOptions::new()).await;
    drop(su);
    assert!(
        expected.len() < unrestricted.len() && !expected.is_empty(),
        "the rule hides some of what the search finds"
    );

    let mut tx = begin_as(&orm, w.bob).await;
    let (found, statements) = sql_of(&mut tx, OBJ, &search, &SearchOptions::new()).await;
    assert_eq!(found, expected);
    assert_eq!(statements.len(), 1, "{statements:#?}");
    let sql = norm(&statements[0]);
    // the path needs a join of the category table, the same one a hand-written condition on
    // the path needs; the WHERE clause is the search's conjunct and then the rule's, whose
    // placeholder follows the search's
    assert_eq!(from_of(&statements[0]), from_of(&by_hand[0]), "{sql}");
    assert!(sql.contains("LEFT JOIN \"x_categ\""), "{sql}");
    assert_eq!(
        where_of(&statements[0]),
        format!(
            "{} AND {}",
            where_of(&search_sql[0]),
            where_of(&path_sql[0]).replace("$1", "$2")
        )
    );

    // what the rule hides is a refusal, not an empty value
    let hidden = tx.env().records(OBJ).unwrap().browse([objs[2]]).unwrap();
    let err = hidden.value(&mut tx, "val").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        refused("Bob Bobman", w.bob, "Spec Object (x.obj)")
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// The snapshot's memo of the model-level decision
/// is asserted by `spec_sec_047_model_level_decisions_are_memoized` (`erp-security`, unit).
#[tokio::test]
async fn spec_sec_047_model_level_versus_record_level_evaluation() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    rule(
        &client,
        &w,
        OBJ,
        Some(w.internal),
        "r",
        Some("[('val', '>', 0)]"),
    )
    .await;
    rule(
        &client,
        &w,
        OBJ,
        Some(w.internal),
        "u",
        Some("[('val', '>', 1)]"),
    )
    .await;
    let orm = policy_orm(&h).await;
    let ids = create(
        &orm,
        OBJ,
        &[
            row(&[("name", Value::str("one")), ("val", Value::Int(1))]),
            row(&[("name", Value::str("two")), ("val", Value::Int(2))]),
            row(&[("name", Value::str("hidden")), ("val", Value::Int(-1))]),
        ],
    )
    .await;

    // the user with permissions: the model-level check costs no statement, repeatedly
    let mut tx = begin_as(&orm, w.bob).await;
    let model = tx.env().records(OBJ).unwrap();
    for _ in 0..2 {
        assert_query_count!(tx, <= 0, {
            assert!(model.has_access(&mut tx, Operation::Read).await.unwrap());
            assert!(model.has_access(&mut tx, Operation::Write).await.unwrap());
        });
    }
    // no permission to create: the model-level answer is False, still without a statement
    assert_query_count!(tx, <= 0, {
        assert!(!model.has_access(&mut tx, Operation::Create).await.unwrap());
    });

    // populated: the first check reads the records, the second is served from the memo
    let visible = model.browse(ids[..2].to_vec()).unwrap();
    assert_query_count!(tx, >= 1, <= 1, {
        assert!(visible.has_access(&mut tx, Operation::Read).await.unwrap());
    });
    assert_query_count!(tx, <= 0, {
        assert!(visible.has_access(&mut tx, Operation::Read).await.unwrap());
        assert_eq!(
            visible
                .filtered_access(&mut tx, Operation::Read)
                .await
                .unwrap()
                .ids(),
            ids[..2]
        );
    });
    // a recordset with a hidden record is refused as a whole, every time
    let all = model.browse(ids.clone()).unwrap();
    for _ in 0..2 {
        assert!(!all.has_access(&mut tx, Operation::Read).await.unwrap());
    }
    assert_eq!(
        all.filtered_access(&mut tx, Operation::Read)
            .await
            .unwrap()
            .ids(),
        ids[..2]
    );

    // a non-read operation keeps the records that satisfy its predicate, which is the
    // superuser's filtering by the same domain
    let wanted = cond("val", Operator::Gt, 1);
    let by_domain = all
        .sudo(true)
        .filtered_domain(&mut tx, &wanted)
        .await
        .unwrap()
        .ids();
    assert_eq!(by_domain, [ids[1]]);
    let writable = all
        .filtered_access(&mut tx, Operation::Write)
        .await
        .unwrap()
        .ids();
    assert_eq!(writable, by_domain);
    assert!(!all.has_access(&mut tx, Operation::Write).await.unwrap());
    assert!(
        model
            .browse([ids[1]])
            .unwrap()
            .has_access(&mut tx, Operation::Write)
            .await
            .unwrap()
    );
    drop(tx);

    // a user whose groups carry no permission: False at the model level, nothing readable
    let mut tx = begin_as(&orm, w.pat).await;
    let model = tx.env().records(OBJ).unwrap();
    assert_query_count!(tx, <= 0, {
        assert!(!model.has_access(&mut tx, Operation::Read).await.unwrap());
    });
    let all = model.browse(ids.clone()).unwrap();
    assert!(
        all.filtered_access(&mut tx, Operation::Read)
            .await
            .unwrap()
            .is_empty()
    );
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// For each user, with and without sudo, the `access` leaf is left alone by the
/// basic level, equals the user's read predicate at the full level, becomes `many2one any!
#[tokio::test]
async fn spec_dom_035_access_operator() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    rule(
        &client,
        &w,
        OBJ,
        Some(w.internal),
        "crud",
        Some("[('val', '>', 20)]"),
    )
    .await;
    rule(&client, &w, OBJ, Some(w.elevated), "crud", None).await;
    rule(&client, &w, CATEG, Some(w.internal), "r", None).await;
    rule(
        &client,
        &w,
        CATEG,
        Some(w.internal),
        "u",
        Some("[('name', '=', 'foo')]"),
    )
    .await;
    rule(
        &client,
        &w,
        CATEG,
        Some(w.elevated),
        "u",
        Some("[('name', 'in', ['foo', 'bar'])]"),
    )
    .await;
    rule(&client, &w, CONTAINER, Some(w.internal), "r", None).await;
    rule(
        &client,
        &w,
        CHILD,
        Some(w.internal),
        "r",
        Some("[('obj_id', 'access', 'read')]"),
    )
    .await;
    let orm = policy_orm(&h).await;
    let categ = create(&orm, CATEG, &[row(&[("name", Value::str("bar"))])]).await[0];
    let objs = create(
        &orm,
        OBJ,
        &[
            row(&[("val", Value::Int(11))]),
            row(&[("val", Value::Int(22))]),
            row(&[("val", Value::Int(23)), ("categ_id", Value::Int(categ))]),
        ],
    )
    .await;
    let children = create(
        &orm,
        CHILD,
        &[
            row(&[("name", Value::str("c11")), ("obj_id", Value::Int(objs[0]))]),
            row(&[("name", Value::str("c22")), ("obj_id", Value::Int(objs[1]))]),
            row(&[("name", Value::str("c23")), ("obj_id", Value::Int(objs[2]))]),
            row(&[("name", Value::str("orphan"))]),
        ],
    )
    .await;

    let id_read = cond("id", Operator::Access, "read");
    let categ_write = cond("categ_id", Operator::Access, "write");
    let foo = cond("name", Operator::Eq, "foo");
    let foo_or_bar = Domain::or([
        foo.clone(),
        cond("name", Operator::In, DomainValue::list(["foo", "bar"])),
    ]);
    for (user, uid) in [("internal", w.bob), ("elevated", w.eve), ("portal", w.pat)] {
        for sudo in [false, true] {
            let tx = begin_as(&orm, uid).await;
            let model = tx.env().records(OBJ).unwrap().sudo(sudo);
            let basic = model
                .optimize_domain(&id_read, OptimizationLevel::Basic, None)
                .unwrap();
            assert_eq!(
                basic, id_read,
                "BASIC leaves it unchanged: {user} sudo {sudo}"
            );
            let full = model
                .optimize_domain_in(&tx, &id_read, OptimizationLevel::Full, None)
                .unwrap();
            let read_predicate = match user {
                "internal" => cond("val", Operator::Gt, 20),
                "elevated" => Domain::TRUE,
                _ => Domain::FALSE,
            };
            let expected = model
                .optimize_domain(&read_predicate, OptimizationLevel::Full, None)
                .unwrap();
            assert_eq!(full, expected, "{user} sudo {sudo}");
            // the many2one form: `any!` of the comodel's write predicate, FALSE without access
            let m2o = model
                .optimize_domain_in(&tx, &categ_write, OptimizationLevel::Full, None)
                .unwrap();
            let expected = match user {
                "internal" => cond("categ_id", Operator::AnyBypass, foo.clone()),
                "elevated" => cond("categ_id", Operator::AnyBypass, foo_or_bar.clone()),
                _ => Domain::FALSE,
            };
            let expected = model
                .optimize_domain(&expected, OptimizationLevel::Full, None)
                .unwrap();
            assert_eq!(m2o, expected, "{user} sudo {sudo}");
            drop(tx);
        }
    }
    // with full access to the comodel the many2one only has to be set
    let tx = begin_as(&orm, w.eve).await;
    let model = tx.env().records(OBJ).unwrap();
    assert_eq!(
        model
            .optimize_domain_in(
                &tx,
                &cond("categ_id", Operator::Access, "read"),
                OptimizationLevel::Full,
                None
            )
            .unwrap(),
        model
            .optimize_domain(
                &cond("categ_id", Operator::Ne, false),
                OptimizationLevel::Full,
                None
            )
            .unwrap()
    );
    drop(tx);

    // in-memory evaluation of the dynamic form equals the filtered access, the many2one form
    // never keeps a record without a target, and a search finds what the leaf selects
    let mut tx = begin_as(&orm, w.bob).await;
    let records = tx.env().records(OBJ).unwrap().browse(objs.clone()).unwrap();
    let in_memory = records.filtered_domain(&mut tx, &id_read).await.unwrap();
    let filtered = records
        .filtered_access(&mut tx, Operation::Read)
        .await
        .unwrap();
    assert_eq!(in_memory.ids(), filtered.ids());
    assert_eq!(in_memory.ids(), objs[1..]);
    let with_target = records
        .sudo(true)
        .filtered_domain(&mut tx, &categ_write)
        .await
        .unwrap();
    assert!(with_target.is_empty(), "the category is not named foo");
    let found = records.search(&mut tx, &id_read).await.unwrap().ids();
    assert_eq!(found, filtered.ids());

    // a rule naming the parent's access compiles into the parent's predicate: one statement
    let child = tx.env().records(CHILD).unwrap();
    let visible = assert_query_count!(tx, >= 1, <= 1, {
        child.search(&mut tx, &Domain::TRUE).await.unwrap().ids()
    });
    let su_children = {
        let mut su = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
        let objs_seen = found.clone();
        su.env()
            .records(CHILD)
            .unwrap()
            .search(
                &mut su,
                &cond("obj_id", Operator::In, DomainValue::list(objs_seen)),
            )
            .await
            .unwrap()
            .ids()
    };
    assert_eq!(visible, su_children);
    assert_eq!(visible, [children[1], children[2]]);

    // the invalid forms
    let container = tx.env().records(CONTAINER).unwrap();
    for (records, domain, message) in [
        (
            &container,
            cond("some_ids", Operator::Access, "read"),
            "The 'access' operator works only for many2one and 'id' fields in condition \
             ('some_ids', 'access', 'read')",
        ),
        (
            &records,
            cond("val", Operator::Access, "read"),
            "The 'access' operator works only for many2one and 'id' fields in condition \
             ('val', 'access', 'read')",
        ),
        (
            &records,
            cond("categ_id", Operator::Access, "blabla"),
            "Invalid value for 'access' operator in condition ('categ_id', 'access', 'blabla')",
        ),
    ] {
        let err = records.search(&mut tx, &domain).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
        assert_eq!(err.message(), message);
    }
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}
