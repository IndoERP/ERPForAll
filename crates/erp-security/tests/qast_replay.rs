//! a stored query AST replayed under the real access engine (022).
//!
//! The policy is `ir_access` rows read by the real `PolicySource`, principals are resolved by the
//! real `Resolver` from `res_groups_users_rel` and the company tables. Expected values come from
//! our code: the live `Records::search_read` / `Records::read_group` call under the same `Env`,
//! or a superuser search with literal values; the texts that are the spec's are asserted whole.
//!
//! Fixture: `qast.order` (name, state, amount, date, placed, user, company, partner, active,
//! `secret` restricted to `base.group_system`, `x_note`), `qast.partner` (name, country),
//! `qast.country`; two companies; users `root` (system), `alice` (sales: own orders, partners of
//! one country), `bob` (managers, companies A and B), `carol` (no order access), `dave`
//! (managers, company A) and `erin` (sales, like alice).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use erp_domain::query::{QueryAst, QueryLimits};
use erp_domain::{Domain, Operator, PrincipalParam, Value as DomainValue};
use erp_fields::{Error, ErrorKind, Value};
use erp_orm::query::{__statement, QueryResult, check, run};
use erp_orm::{
    BeginOptions, CompanyId, Context, Env, Orm, PolicyProvider, ReadGroup, SearchOptions,
    Transaction, UserId,
};
use erp_registry::{AddonDescriptor, CompanyAccessDecl, FieldDecl, ModelDecl, StaticValue};
use erp_security::resolver::Resolver;
use erp_security::source::PolicySource;
use erp_testkit::tokio_postgres::Client;
use erp_testkit::tokio_postgres::types::ToSql;
use erp_testkit::{Harness, assert_query_count, assert_snapshot};
use support::{connect, pool};

static FIXTURE: AddonDescriptor = AddonDescriptor {
    name: "x_qast",
    depends: &["base"],
    exact_only: false,
};

const COUNTRY: &str = "qast.country";
const PARTNER: &str = "qast.partner";
const ORDER: &str = "qast.order";
const GONE: &str = "qast.gone";

const COUNTRY_FIELDS: &[FieldDecl] = &[FieldDecl::char("name")];
const PARTNER_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::many2one("country_id", COUNTRY),
    FieldDecl::boolean("active").default(StaticValue::Bool(true)),
];
const ORDER_FIELDS: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::char("state"),
    FieldDecl::float("amount"),
    FieldDecl::date("date"),
    FieldDecl::datetime("placed"),
    FieldDecl::many2one("user_id", "res.users"),
    FieldDecl::many2one("company_id", "res.company"),
    FieldDecl::many2one("partner_id", PARTNER),
    FieldDecl::boolean("active").default(StaticValue::Bool(true)),
    FieldDecl::char("secret").groups("base.group_system"),
    FieldDecl::char("x_note"),
];
/// The order model without `x_note`, in the second registry.
const ORDER_FIELDS_TRIMMED: &[FieldDecl] = &[
    FieldDecl::char("name"),
    FieldDecl::char("state"),
    FieldDecl::float("amount"),
    FieldDecl::date("date"),
    FieldDecl::datetime("placed"),
    FieldDecl::many2one("user_id", "res.users"),
    FieldDecl::many2one("company_id", "res.company"),
    FieldDecl::many2one("partner_id", PARTNER),
    FieldDecl::boolean("active").default(StaticValue::Bool(true)),
    FieldDecl::char("secret").groups("base.group_system"),
];

fn decls(with_extras: bool) -> Vec<ModelDecl> {
    let mut decls = vec![
        erp_data::ir_model_data_decl(),
        ModelDecl::new("x_qast", COUNTRY)
            .description("Query Country")
            .fields(COUNTRY_FIELDS),
        ModelDecl::new("x_qast", PARTNER)
            .description("Query Partner")
            .fields(PARTNER_FIELDS),
        ModelDecl::new("x_qast", ORDER)
            .company_access(CompanyAccessDecl::unrestricted(
                "query replay fixture tests handwritten rules",
            ))
            .description("Query Order")
            // The data set pairs users with orders of another company on purpose (record rules
            // and company scope/018 read them);'s default consistency
            // check would refuse those rows at creation, so this model opts out.
            .check_company_auto(false)
            .fields(if with_extras {
                ORDER_FIELDS
            } else {
                ORDER_FIELDS_TRIMMED
            }),
    ];
    if with_extras {
        decls.push(
            ModelDecl::new("x_qast", GONE)
                .description("Query Gone")
                .fields(COUNTRY_FIELDS),
        );
    }
    decls
}

async fn harness_with(with_extras: bool) -> Harness {
    Harness::builder([
        &erp_addon_base::ADDON,
        &erp_data::model_data::ADDON,
        &FIXTURE,
    ])
    .with_decls(decls(with_extras))
    .build()
    .await
    .expect("harness")
}

async fn harness() -> Harness {
    harness_with(true).await
}

async fn one(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> i64 {
    client.query_one(sql, params).await.unwrap().get(0)
}

/// What `seed` committed.
struct World {
    sales: i64,
    managers: i64,
    system: i64,
    root: i64,
    alice: i64,
    bob: i64,
    carol: i64,
    dave: i64,
    erin: i64,
    company_a: i64,
    company_b: i64,
    models: BTreeMap<&'static str, i64>,
}

async fn seed(client: &Client) -> World {
    let group = async |name: &str, xml: Option<&str>| -> i64 {
        let id = one(
            client,
            "INSERT INTO res_groups (name, share) VALUES ($1, false) RETURNING id::int8",
            &[&name],
        )
        .await;
        if let Some(xml) = xml {
            client
                .execute(
                    "INSERT INTO ir_model_data (module, name, model, res_id) \
                     VALUES ('base', $1, 'res.groups', $2)",
                    &[&xml, &id],
                )
                .await
                .unwrap();
        }
        id
    };
    let internal = group("Internal User", Some("group_user")).await;
    let system = group("Settings", Some("group_system")).await;
    let sales = group("Sales", None).await;
    let managers = group("Managers", None).await;
    let currency = erp_testkit::fixtures::seed_currency(client).await.unwrap();
    let company = async |name: &str| -> i64 {
        one(
            client,
            "INSERT INTO res_company (name, currency_id) VALUES ($1, $2) RETURNING id::int8",
            &[&name, &currency],
        )
        .await
    };
    let company_a = company("A").await;
    let company_b = company("B").await;
    let user = async |name: &str, groups: &[i64], companies: &[i64]| -> i64 {
        let id = one(
            client,
            "INSERT INTO res_users (name, login, active, share, company_id) \
             VALUES ($1, $1, true, false, $2) RETURNING id::int8",
            &[&name, &companies[0]],
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
        for cid in companies {
            client
                .execute(
                    "INSERT INTO res_company_users_rel (user_id, cid) VALUES ($1, $2)",
                    &[&id, cid],
                )
                .await
                .unwrap();
        }
        id
    };
    let root = user("root", &[internal, system], &[company_a, company_b]).await;
    let alice = user("alice", &[internal, sales], &[company_a]).await;
    let bob = user("bob", &[internal, managers], &[company_a, company_b]).await;
    let carol = user("carol", &[internal], &[company_a]).await;
    let dave = user("dave", &[internal, managers], &[company_a]).await;
    let erin = user("erin", &[internal, sales], &[company_a]).await;
    let mut models = BTreeMap::new();
    for (model, name) in [
        (COUNTRY, "Query Country"),
        (PARTNER, "Query Partner"),
        (ORDER, "Query Order"),
        (GONE, "Query Gone"),
    ] {
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
        sales,
        managers,
        system,
        root,
        alice,
        bob,
        carol,
        dave,
        erin,
        company_a,
        company_b,
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
                &format!("{model} {operation} {group:?} {domain:?}"),
                &w.models[model],
                &group,
                &operation,
                &domain,
            ],
        )
        .await
        .unwrap();
}

/// The access rows of the fixture. `own_orders`: the sales group reads only its own orders;
/// `fr_partners`: the sales group reads only partners of `country`.
async fn acl(client: &Client, w: &World, own_orders: bool, fr_country: Option<i64>) {
    for model in [COUNTRY, PARTNER, ORDER] {
        rule(client, w, model, Some(w.system), "crud", None).await;
        rule(client, w, model, Some(w.managers), "r", None).await;
    }
    rule(client, w, COUNTRY, Some(w.sales), "r", None).await;
    let partner_domain = fr_country.map(|id| format!("[('country_id', '=', {id})]"));
    rule(
        client,
        w,
        PARTNER,
        Some(w.sales),
        "r",
        partner_domain.as_deref(),
    )
    .await;
    let order_domain = own_orders.then_some("[('user_id', '=', user.id)]");
    rule(client, w, ORDER, Some(w.sales), "r", order_domain).await;
    // everyone is held to the companies of the environment
    rule(
        client,
        w,
        ORDER,
        None,
        "r",
        Some("['|', ('company_id', '=', False), ('company_id', 'in', company_ids)]"),
    )
    .await;
}

async fn policy_orm(h: &Harness) -> Orm {
    // installation emits the generated company restrictions; this database has no installation
    let client = support::connect(h).await;
    support::insert_generated(&client, h.registry(), &["res.partner", "res.users"]).await;
    drop(client);
    let source = PolicySource::build(Arc::clone(h.registry()), pool(h, 12))
        .await
        .unwrap();
    let provider: Arc<dyn PolicyProvider> = Arc::new(source);
    Orm::builder(Arc::clone(h.registry()), pool(h, 12))
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

/// The data: two countries, three partners, nine orders (the last one archived).
struct Data {
    fr: i64,
    de: i64,
}

/// One order of the data set; `partner` indexes the partners made before it.
struct Seed {
    name: &'static str,
    state: &'static str,
    amount: f64,
    date: &'static str,
    user: i64,
    company: i64,
    partner: usize,
    active: bool,
}

async fn data(orm: &Orm, w: &World) -> Data {
    let countries = create(
        orm,
        COUNTRY,
        &[
            row(&[("name", Value::str("France"))]),
            row(&[("name", Value::str("Germany"))]),
        ],
    )
    .await;
    let (fr, de) = (countries[0], countries[1]);
    let partners = create(
        orm,
        PARTNER,
        &[
            row(&[("name", Value::str("p1")), ("country_id", Value::Int(fr))]),
            row(&[("name", Value::str("p2")), ("country_id", Value::Int(de))]),
            row(&[("name", Value::str("p3")), ("country_id", Value::Int(fr))]),
        ],
    )
    .await;
    let (a, b, r) = (w.alice, w.bob, w.root);
    let (ca, cb) = (w.company_a, w.company_b);
    let order =
        |name: &'static str, state, amount, day: &'static str, user, company, partner| Seed {
            name,
            state,
            amount,
            date: day,
            user,
            company,
            partner,
            active: true,
        };
    let mut orders = vec![
        order("O1", "posted", 100.0, "2026-01-05", a, ca, 0),
        order("O2", "posted", 250.5, "2026-01-20", a, ca, 1),
        order("O3", "draft", 75.0, "2026-02-03", b, ca, 0),
        order("O4", "posted", 300.0, "2026-02-14", b, cb, 2),
        order("O5", "posted", 50.0, "2026-03-01", r, ca, 1),
        order("O6", "draft", 125.0, "2026-03-09", a, ca, 2),
        order("O7", "posted", 80.0, "2026-03-10", b, ca, 0),
        order("O8", "posted", 20.0, "2026-03-28", r, cb, 1),
        order("O9", "draft", 10.0, "2026-04-01", a, ca, 0),
    ];
    // the last order is archived
    if let Some(last) = orders.last_mut() {
        last.active = false;
    }
    let rows: Vec<_> = orders
        .iter()
        .map(|o| {
            row(&[
                ("name", Value::str(o.name)),
                ("state", Value::str(o.state)),
                ("amount", Value::Float(o.amount)),
                ("date", Value::str(o.date)),
                ("placed", Value::str(format!("{} 10:00:00", o.date))),
                ("user_id", Value::Int(o.user)),
                ("company_id", Value::Int(o.company)),
                ("partner_id", Value::Int(partners[o.partner])),
                ("active", Value::Bool(o.active)),
                ("secret", Value::str(format!("s-{}", o.name))),
                ("x_note", Value::str("note")),
            ])
        })
        .collect();
    create(orm, ORDER, &rows).await;
    Data { fr, de }
}

async fn begin_as(orm: &Orm, uid: i64) -> Transaction {
    orm.begin(UserId::new(uid).unwrap()).await.unwrap()
}

/// A transaction of `uid` whose allowed companies are `companies` (current first), validated
/// against the user's own, as the RPC layer does for `allowed_company_ids`.
async fn begin_in(orm: &Orm, uid: i64, companies: &[i64]) -> Transaction {
    let resolved = begin_as(orm, uid).await;
    let principal = resolved.env().principal().clone();
    drop(resolved);
    let ids = companies
        .iter()
        .map(|c| CompanyId::new(*c).unwrap())
        .collect();
    let principal = principal.with_companies(ids).unwrap();
    // The environment derives its companies from the context, so the
    // `allowed_company_ids` the RPC layer would send go there as well.
    let context = Context::from_pairs([(
        "allowed_company_ids",
        Value::list(companies.iter().map(|c| Value::Int(*c))),
    )]);
    orm.begin_opts(BeginOptions::for_principal(principal).context(context))
        .await
        .unwrap()
}

fn parse(text: &str) -> QueryAst {
    match QueryAst::parse(text.as_bytes(), &QueryLimits::default()) {
        Ok(ast) => ast,
        Err(e) => panic!("{text} should parse: {e}"),
    }
}

fn search_doc(domain: &str, fields: &str, extra: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"search_read","model":"{ORDER}","domain":{domain},"fields":{fields},"limit":100{extra}}}"#
    )
}

fn group_doc(groupby: &str, aggregates: &str, extra: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"read_group","model":"{ORDER}","groupby":{groupby},"aggregates":{aggregates},"limit":100{extra}}}"#
    )
}

fn names(result: &QueryResult) -> Vec<String> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("not rows: {result:?}")
    };
    let mut names: Vec<String> = rows
        .iter()
        .map(|r| match &r["name"] {
            Value::Str(s) => s.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    names.sort();
    names
}

/// Names of the orders matching `domain` as the superuser, literal values only.
async fn expected_names(orm: &Orm, domain: &Domain) -> Vec<String> {
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let env = tx.env();
    let records = env.records(ORDER).unwrap();
    let rows = records
        .search_read(&mut tx, domain, &["name"], &SearchOptions::new())
        .await
        .unwrap();
    let mut names: Vec<String> = rows
        .iter()
        .map(|r| match &r["name"] {
            Value::Str(s) => s.to_string(),
            other => panic!("{other:?}"),
        })
        .collect();
    names.sort();
    names
}

fn cond(field: &str, op: Operator, value: impl Into<DomainValue>) -> Domain {
    Domain::condition(field, op, value).unwrap()
}

fn same_error(left: &Error, right: &Error) {
    assert_eq!(left.kind(), right.kind(), "{left} / {right}");
    assert_eq!(left.message(), right.message());
}

/// One document with every parameter form, parsed from bytes, written canonically, and run by
/// alice and by bob: each gets their own rows and their own companies. The expected rows are a
/// superuser's search with the principal's literal values.
#[tokio::test]
async fn spec_qast_006_principal_params_replay() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;

    let doc = search_doc(
        r#"[["user_id","=",{"param":"uid"}],["company_id","in",{"param":"company_ids"}],
            ["date","<=",{"param":"today"}],["placed","<=",{"param":"now"}]]"#,
        r#"["name"]"#,
        "",
    );
    let ast = parse(&doc);
    // the canonical form keeps the parameters, and reads back as the same query
    let canonical = ast.canonical();
    let text = String::from_utf8(canonical.clone()).unwrap();
    for name in ["uid", "company_ids", "today", "now"] {
        assert!(text.contains(&format!(r#"{{"param":"{name}"}}"#)), "{text}");
    }
    assert_eq!(
        QueryAst::parse(&canonical, &QueryLimits::default()).unwrap(),
        ast
    );

    let companies = |ids: &[i64]| DomainValue::list(ids.iter().map(|i| DomainValue::Int(*i)));
    for (uid, own_companies) in [
        (w.alice, vec![w.company_a]),
        (w.bob, vec![w.company_a, w.company_b]),
    ] {
        let expected = expected_names(
            &orm,
            &(cond("user_id", Operator::Eq, uid)
                & cond("company_id", Operator::In, companies(&own_companies))),
        )
        .await;
        assert!(!expected.is_empty());
        let mut tx = begin_as(&orm, uid).await;
        let env = tx.env();
        let got = run(&mut tx, &env, &ast).await.unwrap();
        assert_eq!(names(&got), expected, "user {uid}");
        let QueryResult::Rows { truncated, .. } = got else {
            panic!()
        };
        assert!(!truncated);
    }
    // alice and bob see different rows from the same document
    let mut a = begin_as(&orm, w.alice).await;
    let mut b = begin_as(&orm, w.bob).await;
    let (ea, eb) = (a.env(), b.env());
    assert_ne!(
        names(&run(&mut a, &ea, &ast).await.unwrap()),
        names(&run(&mut b, &eb, &ast).await.unwrap())
    );

    // the current company: bob's current company is A, so his orders of company B are out
    let current = parse(&search_doc(
        r#"[["user_id","=",{"param":"uid"}],["company_id","=",{"param":"company_id"}]]"#,
        r#"["name"]"#,
        "",
    ));
    let expected = expected_names(
        &orm,
        &(cond("user_id", Operator::Eq, w.bob) & cond("company_id", Operator::Eq, w.company_a)),
    )
    .await;
    let mut tx = begin_as(&orm, w.bob).await;
    let env = tx.env();
    assert_eq!(
        names(&run(&mut tx, &env, &current).await.unwrap()),
        expected
    );
    // and with company B first, the same document selects B's rows
    let mut tx = begin_in(&orm, w.bob, &[w.company_b, w.company_a]).await;
    let env = tx.env();
    let expected = expected_names(
        &orm,
        &(cond("user_id", Operator::Eq, w.bob) & cond("company_id", Operator::Eq, w.company_b)),
    )
    .await;
    assert_eq!(
        names(&run(&mut tx, &env, &current).await.unwrap()),
        expected
    );
    drop(client);
    h.cleanup().await.unwrap();
}

/// The documents: what parses but does not fit the registry. Each is paired with
/// the live call that raises the same error.
fn registry_cases() -> Vec<(&'static str, QueryAst)> {
    let search =
        |domain: &str, fields: &str, extra: &str| parse(&search_doc(domain, fields, extra));
    let group = |groupby: &str, aggregates: &str| parse(&group_doc(groupby, aggregates, ""));
    vec![
        ("field", search("[]", r#"["zz"]"#, "")),
        ("domain", search(r#"[["zz","=",1]]"#, r#"["name"]"#, "")),
        (
            "order",
            search("[]", r#"["name"]"#, r#","order":[{"field":"zz"}]"#),
        ),
        (
            "groupby",
            group(r#"[{"path":"zz"}]"#, r#"[{"func":"count"}]"#),
        ),
        ("aggregate", group("[]", r#"[{"func":"sum","field":"zz"}]"#)),
        (
            "path through char",
            group(r#"[{"path":"name.name"}]"#, r#"[{"func":"count"}]"#),
        ),
        (
            "date without granularity",
            group(r#"[{"path":"date"}]"#, r#"[{"func":"count"}]"#),
        ),
        (
            "granularity on char",
            group(
                r#"[{"path":"name","granularity":"month"}]"#,
                r#"[{"func":"count"}]"#,
            ),
        ),
        (
            "operator for the type",
            search(r#"[["amount","=","abc"]]"#, r#"["name"]"#, ""),
        ),
    ]
}

/// The registry errors, as `Validation`: unknown and abstract models with the texts of the
/// spec, every other case with the ORM's own text, which is the text of the live call.
#[tokio::test]
async fn spec_qast_012_registry_validation() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;
    let mut tx = begin_as(&orm, w.root).await;
    let env = tx.env();

    let model = |name: &str| {
        parse(&format!(
            r#"{{"version":1,"kind":"search_read","model":"{name}","fields":["name"],"limit":5}}"#
        ))
    };
    for (name, text) in [
        ("qast.nope", "Unknown model 'qast.nope' in query AST"),
        ("base", "Model 'base' is abstract and has no records"),
    ] {
        let ast = model(name);
        for result in [
            run(&mut tx, &env, &ast).await.map(|_| ()),
            check(&mut tx, &env, &ast).await,
        ] {
            let err = result.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Validation, "{name}");
            assert_eq!(err.message(), text);
        }
    }

    for (label, ast) in registry_cases() {
        let err = match run(&mut tx, &env, &ast).await {
            Ok(_) => panic!("{label}: the run was accepted"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), ErrorKind::Validation, "{label}: {err}");
        if matches!(label, "field" | "order") {
            // names the ORM reports as `Programming` (its callers are Rust code) are `Validation`
            // for a stored query, with this text
            assert_eq!(err.message(), "Invalid field 'zz' on model 'qast.order'");
        } else {
            let live = live_error(&mut tx, &env, &ast).await;
            same_error(&err, &live);
        }
        // `check` raises the same error
        let checked = check(&mut tx, &env, &ast).await.unwrap_err();
        same_error(&checked, &err);
    }
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// The error of the live call equivalent to `ast` (built independently of the replay code).
async fn live_error(tx: &mut Transaction, env: &Env, ast: &QueryAst) -> Error {
    use erp_domain::query::QueryKind;
    let records = env.records(ast.model()).unwrap();
    let domain = ast.domain().clone();
    match ast.kind() {
        QueryKind::SearchRead { fields, order } => {
            let names: Vec<&str> = fields.iter().map(|f| f.as_str()).collect();
            let mut opts = SearchOptions::new().limit(u64::from(ast.limit()) + 1);
            if !order.is_empty() {
                let text: Vec<String> = order
                    .iter()
                    .map(|t| format!("{} {}", t.field(), t.dir()))
                    .collect();
                opts = opts.order(text.join(", "));
            }
            records
                .search_read(tx, &domain, &names, &opts)
                .await
                .unwrap_err()
        }
        QueryKind::ReadGroup {
            groupby,
            aggregates,
            ..
        } => {
            let args = ReadGroup::new(domain)
                .groupby(groupby.iter().map(|g| g.spec_name()))
                .aggregates(aggregates.iter().map(|a| a.spec_name()))
                .limit(u64::from(ast.limit()) + 1)
                .external(true);
            records.read_group(tx, args).await.unwrap_err()
        }
        _ => panic!("unknown kind"),
    }
}

/// The arguments of the live call equivalent to a corpus document, written by hand.
enum Live {
    Search {
        domain: Domain,
        fields: &'static [&'static str],
        order: Option<&'static str>,
        offset: u64,
    },
    Group {
        domain: Domain,
        groupby: &'static [&'static str],
        aggregates: &'static [&'static str],
        having: Vec<Value>,
        order: Option<&'static str>,
    },
}

fn param(p: PrincipalParam) -> DomainValue {
    DomainValue::Param(p)
}

fn corpus() -> Vec<(&'static str, QueryAst, Live)> {
    vec![
        (
            "search_posted_by_amount",
            parse(&search_doc(
                r#"[["state","=","posted"]]"#,
                r#"["name","amount"]"#,
                r#","order":[{"field":"amount","dir":"desc"}]"#,
            )),
            Live::Search {
                domain: cond("state", Operator::Eq, "posted"),
                fields: &["name", "amount"],
                order: Some("amount desc"),
                offset: 0,
            },
        ),
        (
            "search_my_orders",
            parse(&search_doc(
                r#"[["user_id","=",{"param":"uid"}],["company_id","in",{"param":"company_ids"}]]"#,
                r#"["name","state"]"#,
                r#","order":[{"field":"name"}]"#,
            )),
            Live::Search {
                domain: cond("user_id", Operator::Eq, param(PrincipalParam::Uid))
                    & cond(
                        "company_id",
                        Operator::In,
                        param(PrincipalParam::CompanyIds),
                    ),
                fields: &["name", "state"],
                order: Some("name asc"),
                offset: 0,
            },
        ),
        (
            "search_with_archived",
            parse(&search_doc(
                "[]",
                r#"["name","active"]"#,
                r#","context":{"active_test":false},"order":[{"field":"name"}]"#,
            )),
            Live::Search {
                domain: Domain::TRUE,
                fields: &["name", "active"],
                order: Some("name asc"),
                offset: 0,
            },
        ),
        (
            "search_page",
            parse(&search_doc(
                r#"[["date","<=",{"param":"today"}]]"#,
                r#"["name","date"]"#,
                r#","order":[{"field":"date","nulls":"last"}],"offset":1"#,
            )),
            Live::Search {
                domain: cond(
                    "date",
                    Operator::Le,
                    param(PrincipalParam::Now(erp_domain::NowKind::Date)),
                ),
                fields: &["name", "date"],
                order: Some("date asc nulls last"),
                offset: 1,
            },
        ),
        (
            "group_state",
            parse(&group_doc(
                r#"[{"path":"state"}]"#,
                r#"[{"func":"count"},{"func":"sum","field":"amount"}]"#,
                r#","order":[{"key":"amount:sum","dir":"desc"}]"#,
            )),
            Live::Group {
                domain: Domain::TRUE,
                groupby: &["state"],
                aggregates: &["__count", "amount:sum"],
                having: vec![],
                order: Some("amount:sum desc, state asc"),
            },
        ),
        (
            "group_month_and_country",
            parse(&group_doc(
                r#"[{"path":"date","granularity":"month"},{"path":"partner_id.country_id"}]"#,
                r#"[{"func":"count"}]"#,
                r#","having":[{"aggregate":"__count","op":">","value":0}]"#,
            )),
            Live::Group {
                domain: Domain::TRUE,
                groupby: &["date:month", "partner_id.country_id"],
                aggregates: &["__count"],
                having: vec![Value::list([
                    Value::str("__count"),
                    Value::str(">"),
                    Value::Int(0),
                ])],
                order: None,
            },
        ),
        (
            "group_totals_only",
            parse(&group_doc(
                "[]",
                r#"[{"func":"count"},{"func":"avg","field":"amount"},{"func":"max","field":"amount"}]"#,
                "",
            )),
            Live::Group {
                domain: Domain::TRUE,
                groupby: &[],
                aggregates: &["__count", "amount:avg", "amount:max"],
                having: vec![],
                order: None,
            },
        ),
        (
            "group_by_user_inactive_too",
            parse(&group_doc(
                r#"[{"path":"user_id"}]"#,
                r#"[{"func":"sum","field":"amount"}]"#,
                r#","context":{"active_test":false},"having":[{"aggregate":"amount:sum","op":">=","value":10}],"order":[{"key":"user_id","dir":"desc"}]"#,
            )),
            Live::Group {
                domain: Domain::TRUE,
                groupby: &["user_id"],
                aggregates: &["amount:sum"],
                having: vec![Value::list([
                    Value::str("amount:sum"),
                    Value::str(">="),
                    Value::Int(10),
                ])],
                order: Some("user_id desc"),
            },
        ),
    ]
}

/// The live call of a corpus entry: the rows (or groups), cut to the page and flagged.
async fn live(
    tx: &mut Transaction,
    env: &Env,
    ast: &QueryAst,
    how: &Live,
) -> erp_fields::Result<QueryResult> {
    let limit = usize::try_from(ast.limit()).unwrap();
    let probe = u64::from(ast.limit()) + 1;
    let records = env.records(ast.model())?;
    let env_flag = ast.context().active_test();
    let records = match env_flag {
        Some(flag) => env
            .with_context([("active_test", Value::Bool(flag))])
            .records(ast.model())?,
        None => records,
    };
    match how {
        Live::Search {
            domain,
            fields,
            order,
            offset,
        } => {
            let mut opts = SearchOptions::new().limit(probe).offset(*offset);
            if let Some(order) = order {
                opts = opts.order(*order);
            }
            let mut rows = records.search_read(tx, domain, fields, &opts).await?;
            let truncated = rows.len() > limit;
            rows.truncate(limit);
            Ok(QueryResult::Rows { rows, truncated })
        }
        Live::Group {
            domain,
            groupby,
            aggregates,
            having,
            order,
        } => {
            let mut args = ReadGroup::new(domain.clone())
                .groupby(groupby.iter().copied())
                .aggregates(aggregates.iter().copied())
                .having(having.clone())
                .limit(probe)
                .external(true);
            if let Some(order) = order {
                args = args.order(*order);
            }
            let mut rows = records.read_group(tx, args).await?;
            let truncated = rows.len() > limit;
            rows.truncate(limit);
            let columns = groupby
                .iter()
                .chain(aggregates.iter())
                .map(|s| (*s).to_owned())
                .collect();
            Ok(QueryResult::Groups {
                columns,
                rows,
                truncated,
            })
        }
    }
}

/// Each document of the corpus, run by root, alice and bob, equals the live call under the same
/// `Env`, and costs no more statements than it (each in a transaction of its own).
#[tokio::test]
async fn spec_qast_013_replay_equals_live() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    // the partner rule needs the country id: the rows are made before the rules
    let orm = policy_orm(&h).await;
    let d = data(&orm, &w).await;
    drop(orm);
    acl(&client, &w, true, Some(d.fr)).await;
    let orm = policy_orm(&h).await;

    for uid in [w.root, w.alice, w.bob] {
        for (name, ast, how) in corpus() {
            let mut live_tx = begin_as(&orm, uid).await;
            let live_env = live_tx.env();
            let before = live_tx.query_count();
            let expected = live(&mut live_tx, &live_env, &ast, &how).await.unwrap();
            let live_statements = live_tx.query_count() - before;
            drop(live_tx);

            let mut tx = begin_as(&orm, uid).await;
            let env = tx.env();
            let before = tx.query_count();
            let got = run(&mut tx, &env, &ast).await.unwrap();
            let replay_statements = tx.query_count() - before;
            assert_eq!(got, expected, "{name} as user {uid}");
            assert!(
                replay_statements <= live_statements,
                "{name} as user {uid}: replay {replay_statements} statements, live {live_statements}"
            );
        }
    }
    // something real came back for the principal who can see everything
    let mut tx = begin_as(&orm, w.root).await;
    let env = tx.env();
    let (_, ast, _) = corpus().remove(0);
    let QueryResult::Rows { rows, .. } = run(&mut tx, &env, &ast).await.unwrap() else {
        panic!()
    };
    assert!(rows.len() > 3, "{rows:?}");
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_qast_014_truncated() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;

    let mut tx = begin_as(&orm, w.root).await;
    let env = tx.env();
    // how many records and groups there are, from the live calls
    let records = env.records(ORDER).unwrap();
    let all = records
        .search(&mut tx, &Domain::TRUE)
        .await
        .unwrap()
        .ids()
        .len();
    let groups = records
        .read_group(
            &mut tx,
            ReadGroup::new(Domain::TRUE)
                .groupby(["state"])
                .aggregates(["__count"])
                .external(true),
        )
        .await
        .unwrap()
        .len();
    assert!(all >= 3 && groups >= 2, "{all} records, {groups} groups");

    for limit in [all - 1, all, all + 1] {
        let ast = parse(
            &search_doc("[]", r#"["name"]"#, "")
                .replace(r#""limit":100"#, &format!(r#""limit":{limit}"#)),
        );
        let before = tx.query_count();
        let QueryResult::Rows { rows, truncated } = run(&mut tx, &env, &ast).await.unwrap() else {
            panic!()
        };
        let used = tx.query_count() - before;
        assert_eq!(rows.len(), limit.min(all), "limit {limit}");
        assert_eq!(truncated, limit < all, "limit {limit}");
        assert!(used <= 2, "the probe adds no statement: {used}");
    }
    for limit in [groups - 1, groups, groups + 1] {
        let ast = parse(
            &group_doc(r#"[{"path":"state"}]"#, r#"[{"func":"count"}]"#, "")
                .replace(r#""limit":100"#, &format!(r#""limit":{limit}"#)),
        );
        let before = tx.query_count();
        let QueryResult::Groups {
            rows,
            truncated,
            columns,
        } = run(&mut tx, &env, &ast).await.unwrap()
        else {
            panic!()
        };
        let used = tx.query_count() - before;
        assert_eq!(columns, ["state", "__count"]);
        assert_eq!(rows.len(), limit.min(groups), "limit {limit}");
        assert_eq!(truncated, limit < groups, "limit {limit}");
        assert!(used <= 1, "the probe adds no statement: {used}");
    }
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_qast_015_acl_denied() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;
    let ast = parse(&search_doc("[]", r#"["name"]"#, ""));

    // saved by an administrator: it checks out
    let mut admin = begin_as(&orm, w.root).await;
    let env = admin.env();
    check(&mut admin, &env, &ast).await.unwrap();
    drop(admin);

    // carol has no read access to the model
    let mut tx = begin_as(&orm, w.carol).await;
    let env = tx.env();
    tx.capture_statements(true);
    let err = run(&mut tx, &env, &ast).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    let statements = tx.captured().to_vec();
    assert!(
        statements.iter().all(|s| !s.contains("qast_order")),
        "no data statement after the model check: {statements:#?}"
    );
    tx.capture_statements(false);
    // the text of the live call
    let live = env
        .records(ORDER)
        .unwrap()
        .search_read(&mut tx, &Domain::TRUE, &["name"], &SearchOptions::new())
        .await
        .unwrap_err();
    same_error(&err, &live);
    assert!(err.message().contains("Query Order"), "{}", err.message());
    let checked = check(&mut tx, &env, &ast).await.unwrap_err();
    same_error(&checked, &err);
    // the model check comes before anything that names a field: an unknown field does not hide it
    let unknown = parse(&search_doc("[]", r#"["name","zz"]"#, ""));
    let again = run(&mut tx, &env, &unknown).await.unwrap_err();
    let mut fresh = begin_as(&orm, w.carol).await;
    let fresh_env = fresh.env();
    let first = run(&mut fresh, &fresh_env, &unknown).await.unwrap_err();
    same_error(&first, &err);
    same_error(&again, &err);
    // a read_group is refused the same way
    let grouped = parse(&group_doc(
        r#"[{"path":"state"}]"#,
        r#"[{"func":"count"}]"#,
        "",
    ));
    let err = run(&mut tx, &env, &grouped).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_qast_016_record_rules_narrow() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, true, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;
    // saved by an administrator, matching every order
    let ast = parse(&search_doc("[]", r#"["name","user_id"]"#, ""));

    let all = expected_names(&orm, &Domain::TRUE).await;
    let mut admin = begin_as(&orm, w.root).await;
    let env = admin.env();
    let got = run(&mut admin, &env, &ast).await.unwrap();
    assert_eq!(names(&got), all);
    drop(admin);

    let mut tx = begin_as(&orm, w.alice).await;
    let env = tx.env();
    let got = run(&mut tx, &env, &ast).await.unwrap();
    let mine = expected_names(
        &orm,
        &(cond("user_id", Operator::Eq, w.alice) & cond("company_id", Operator::Eq, w.company_a)),
    )
    .await;
    assert_eq!(names(&got), mine);
    assert!(mine.len() < all.len());
    let QueryResult::Rows { rows, .. } = &got else {
        panic!()
    };
    // a many2one reads as `[id, display name]`
    assert!(rows.iter().all(|r| match &r["user_id"] {
        Value::List(pair) => pair.first() == Some(&Value::Int(w.alice)),
        _ => false,
    }));
    // the rule is in the statement: one statement, like the live call
    let mut live_tx = begin_as(&orm, w.alice).await;
    let live_env = live_tx.env();
    let before = live_tx.query_count();
    live_env
        .records(ORDER)
        .unwrap()
        .search_read(
            &mut live_tx,
            &Domain::TRUE,
            &["name", "user_id"],
            &SearchOptions::new().limit(101),
        )
        .await
        .unwrap();
    let live_statements = live_tx.query_count() - before;
    let before = tx.query_count();
    run(&mut tx, &env, &ast).await.unwrap();
    assert!(tx.query_count() - before <= live_statements);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_qast_017_unreadable_field_fails() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;
    let cases = [
        (
            "fields",
            parse(&search_doc("[]", r#"["name","secret"]"#, "")),
        ),
        (
            "domain",
            parse(&search_doc(r#"[["secret","=","s-O1"]]"#, r#"["name"]"#, "")),
        ),
        (
            "order",
            parse(&search_doc(
                "[]",
                r#"["name"]"#,
                r#","order":[{"field":"secret"}]"#,
            )),
        ),
        (
            "groupby",
            parse(&group_doc(
                r#"[{"path":"secret"}]"#,
                r#"[{"func":"count"}]"#,
                "",
            )),
        ),
        (
            "aggregate",
            parse(&group_doc(
                r#"[{"path":"state"}]"#,
                r#"[{"func":"count_distinct","field":"secret"}]"#,
                "",
            )),
        ),
    ];
    for (place, ast) in &cases {
        // admin (a member of the system group) can run each
        let mut admin = begin_as(&orm, w.root).await;
        let env = admin.env();
        let ok = run(&mut admin, &env, ast).await;
        assert!(ok.is_ok(), "{place}: {ok:?}");
        drop(admin);
        // bob cannot, and nothing partial comes back
        let mut tx = begin_as(&orm, w.bob).await;
        let env = tx.env();
        let err = run(&mut tx, &env, ast).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{place}: {err}");
        let live = live_error(&mut tx, &env, ast).await;
        same_error(&err, &live);
        assert!(
            err.message().contains("secret"),
            "{place}: {}",
            err.message()
        );
        let checked = check(&mut tx, &env, ast).await.unwrap_err();
        same_error(&checked, &err);
        drop(tx);
    }
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_qast_018_company_scope() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;
    // no company condition; saved while its author's current company was B
    let ast = parse(&search_doc("[]", r#"["name"]"#, ""));

    let in_companies = |ids: &[i64]| {
        cond(
            "company_id",
            Operator::In,
            DomainValue::list(ids.iter().map(|i| DomainValue::Int(*i))),
        )
    };
    let only_a = expected_names(&orm, &in_companies(&[w.company_a])).await;
    let both = expected_names(&orm, &in_companies(&[w.company_a, w.company_b])).await;
    assert!(only_a.len() < both.len());

    let mut tx = begin_in(&orm, w.bob, &[w.company_a]).await;
    let env = tx.env();
    assert_eq!(names(&run(&mut tx, &env, &ast).await.unwrap()), only_a);
    drop(tx);
    let mut tx = begin_in(&orm, w.bob, &[w.company_a, w.company_b]).await;
    let env = tx.env();
    assert_eq!(names(&run(&mut tx, &env, &ast).await.unwrap()), both);
    drop(tx);
    // a user of company A only gets A's orders
    let mut tx = begin_as(&orm, w.dave).await;
    let env = tx.env();
    assert_eq!(names(&run(&mut tx, &env, &ast).await.unwrap()), only_a);
    drop(tx);
    // and a company that is not the user's cannot be asked for
    let resolved = begin_as(&orm, w.dave).await;
    let principal = resolved.env().principal().clone();
    let refused = principal
        .with_companies(vec![CompanyId::new(w.company_b).unwrap()])
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "Access to unauthorized or invalid companies."
    );
    drop(resolved);
    drop(client);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_qast_019_read_group_replay() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let orm = policy_orm(&h).await;
    let d = data(&orm, &w).await;
    drop(orm);
    // alice: her own orders, and partners of France only
    acl(&client, &w, true, Some(d.fr)).await;
    let orm = policy_orm(&h).await;

    let ast = parse(&group_doc(
        r#"[{"path":"user_id"},{"path":"partner_id.country_id"}]"#,
        r#"[{"func":"count"},{"func":"sum","field":"amount"}]"#,
        "",
    ));
    let how = Live::Group {
        domain: Domain::TRUE,
        groupby: &["user_id", "partner_id.country_id"],
        aggregates: &["__count", "amount:sum"],
        having: vec![],
        order: None,
    };
    let mut results = Vec::new();
    for uid in [w.root, w.alice] {
        let mut live_tx = begin_as(&orm, uid).await;
        let live_env = live_tx.env();
        let expected = live(&mut live_tx, &live_env, &ast, &how).await.unwrap();
        drop(live_tx);
        let mut tx = begin_as(&orm, uid).await;
        let env = tx.env();
        let got = run(&mut tx, &env, &ast).await.unwrap();
        assert_eq!(got, expected, "user {uid}");
        results.push(got);
    }
    let QueryResult::Groups {
        rows: root_rows, ..
    } = &results[0]
    else {
        panic!()
    };
    let QueryResult::Groups {
        rows: alice_rows, ..
    } = &results[1]
    else {
        panic!()
    };
    // the country of the German partner shows for root and not for alice (her partner rule
    // applies to the path join, as)
    let has_country =
        |rows: &Vec<Vec<Value>>, country: i64| rows.iter().any(|r| r[1] == Value::Int(country));
    assert!(has_country(root_rows, d.de));
    assert!(!has_country(alice_rows, d.de));
    // alice counts only her own orders
    let total = |rows: &Vec<Vec<Value>>| -> i64 {
        rows.iter()
            .map(|r| match r[2] {
                Value::Int(n) => n,
                ref other => panic!("{other:?}"),
            })
            .sum()
    };
    assert!(total(alice_rows) < total(root_rows));
    drop(client);
    h.cleanup().await.unwrap();
}

/// Canonical texts stored in a table of the first database, read back and run on a registry that
/// no longer has the field or the model: they still parse, and the runs fail naming the element.
#[tokio::test]
async fn spec_qast_020_removed_field_or_model() {
    let h = harness().await;
    let client = connect(&h).await;
    client
        .batch_execute("CREATE TABLE qast_stored (id serial PRIMARY KEY, body text NOT NULL)")
        .await
        .unwrap();
    let on_gone = format!(
        r#"{{"version":1,"kind":"search_read","model":"{GONE}","fields":["name"],"limit":5}}"#
    );
    let stored = [
        parse(&search_doc("[]", r#"["name","x_note"]"#, "")),
        parse(&search_doc(r#"[["x_note","=","note"]]"#, r#"["name"]"#, "")),
        parse(&group_doc(
            r#"[{"path":"x_note"}]"#,
            r#"[{"func":"count"}]"#,
            "",
        )),
        parse(&on_gone),
    ];
    for ast in &stored {
        let text = String::from_utf8(ast.canonical()).unwrap();
        client
            .execute("INSERT INTO qast_stored (body) VALUES ($1)", &[&text])
            .await
            .unwrap();
    }
    // the same texts, read back from the table, still parse to the same queries
    let bodies = client
        .query("SELECT body FROM qast_stored ORDER BY id", &[])
        .await
        .unwrap();
    let reread: Vec<QueryAst> = bodies
        .iter()
        .map(|r| parse(&r.get::<_, String>(0)))
        .collect();
    assert_eq!(reread, stored);

    // in the first registry every one of them runs (the elements exist)
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    rule(&client, &w, GONE, Some(w.system), "crud", None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;
    let mut tx = begin_as(&orm, w.root).await;
    let env = tx.env();
    for ast in &reread[..3] {
        run(&mut tx, &env, ast).await.unwrap();
    }
    run(&mut tx, &env, &reread[3]).await.unwrap();
    drop(tx);

    // the second registry has neither `x_note` nor the model
    let h2 = harness_with(false).await;
    let client2 = connect(&h2).await;
    let w2 = seed(&client2).await;
    acl(&client2, &w2, false, None).await;
    let orm2 = policy_orm(&h2).await;
    let mut tx = begin_as(&orm2, w2.root).await;
    let env = tx.env();
    let texts = [
        "Invalid field 'x_note' on model 'qast.order'",
        "Invalid field qast.order.x_note in condition ('x_note', '=', 'note')",
        "",
    ];
    for ((label, text), ast) in ["fields", "domain", "groupby"]
        .iter()
        .zip(texts)
        .zip(&reread[..3])
    {
        let err = run(&mut tx, &env, ast).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{label}: {err}");
        if text.is_empty() {
            assert!(
                err.message().contains("x_note") && err.message().contains("qast.order"),
                "{label}: {}",
                err.message()
            );
        } else {
            assert_eq!(err.message(), text, "{label}");
        }
        let checked = check(&mut tx, &env, ast).await.unwrap_err();
        same_error(&checked, &err);
    }
    let err = run(&mut tx, &env, &reread[3]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), "Unknown model 'qast.gone' in query AST");
    drop(tx);
    drop(client);
    drop(client2);
    h.cleanup().await.unwrap();
    h2.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_qast_021_check_runs_nothing() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, true, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;

    let valid = [
        parse(&search_doc(
            r#"[["state","=","posted"]]"#,
            r#"["name"]"#,
            "",
        )),
        parse(&group_doc(
            r#"[{"path":"state"}]"#,
            r#"[{"func":"count"}]"#,
            "",
        )),
    ];
    let mut invalid: Vec<(&str, QueryAst)> = registry_cases();
    invalid.push((
        "secret field",
        parse(&search_doc("[]", r#"["name","secret"]"#, "")),
    ));
    invalid.push((
        "secret group",
        parse(&group_doc(
            r#"[{"path":"secret"}]"#,
            r#"[{"func":"count"}]"#,
            "",
        )),
    ));

    // a warm transaction of alice: her principal is resolved and her policy is pinned
    let mut tx = begin_as(&orm, w.alice).await;
    let env = tx.env();
    check(&mut tx, &env, &valid[0]).await.unwrap();
    for ast in &valid {
        assert_query_count!(tx, <= 0, {
            check(&mut tx, &env, ast).await.unwrap();
        });
    }
    for (label, ast) in &invalid {
        let checked = assert_query_count!(tx, <= 0, {
            match check(&mut tx, &env, ast).await {
                Ok(()) => panic!("{label}: check accepted the query"),
                Err(e) => e,
            }
        });
        let ran = match run(&mut tx, &env, ast).await {
            Ok(_) => panic!("{label}: run accepted the query"),
            Err(e) => e,
        };
        same_error(&checked, &ran);
        assert!(!checked.message().is_empty(), "{label}");
    }
    drop(tx);

    // the document: carol is refused by `check` as by `run`
    let mut tx = begin_as(&orm, w.carol).await;
    let env = tx.env();
    let refused = check(&mut tx, &env, &valid[0]).await.unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::Access);
    let ran = run(&mut tx, &env, &valid[0]).await.unwrap_err();
    same_error(&refused, &ran);
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// The statement each document would send for alice, compared with the committed file; erin has
/// alice's groups and company, so her statements have the same text with her own parameter
/// values: nothing of either user is inlined.
#[tokio::test]
async fn spec_qast_022_sql_snapshots() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    let orm = policy_orm(&h).await;
    let d = data(&orm, &w).await;
    drop(orm);
    acl(&client, &w, true, Some(d.fr)).await;
    let orm = policy_orm(&h).await;

    let mut alice = begin_as(&orm, w.alice).await;
    let alice_env = alice.env();
    let mut erin = begin_as(&orm, w.erin).await;
    let erin_env = erin.env();
    let neutral = |sql: String, uid: i64| sql.replace(&format!("__{uid}\""), "__UID\"");
    for (name, ast, _) in corpus() {
        let (sql, params) = __statement(&mut alice, &alice_env, &ast).await.unwrap();
        let sql = neutral(sql, w.alice);
        let text = format!("{sql}\n-- parameters: {}\n", params.len());
        assert_snapshot(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/snapshots/qast")
                .join(format!("{name}.sql")),
            &text,
        );
        let (erin_sql, erin_params) = __statement(&mut erin, &erin_env, &ast).await.unwrap();
        assert_eq!(
            neutral(erin_sql, w.erin),
            sql,
            "{name}: the text holds no value of either user"
        );
        assert_eq!(erin_params.len(), params.len(), "{name}");
        // the values travel as parameters: alice's own id is bound for her, erin's for erin
        let (alice_values, erin_values) = (params.join(" "), erin_params.join(" "));
        if name == "search_my_orders" {
            assert!(
                alice_values.contains(&format!("Int8({})", w.alice)),
                "{alice_values}"
            );
            assert!(
                erin_values.contains(&format!("Int8({})", w.erin)),
                "{erin_values}"
            );
            assert_ne!(alice_values, erin_values);
        }
    }
    drop(alice);
    drop(erin);
    drop(client);
    h.cleanup().await.unwrap();
}

// ------------------------------------------------------------------------------------------
// Review findings
// ------------------------------------------------------------------------------------------

const NO_SUDO: &str = "A query AST is not run in superuser mode: audited elevated replay is not \
                       supported until a later release and a later release";

/// F6: a stored query never runs in superuser mode, whoever asks.
#[tokio::test]
async fn run_and_check_refuse_superuser_mode() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;
    let ast = parse(&search_doc("[]", r#"["name"]"#, ""));
    let grouped = parse(&group_doc(
        r#"[{"path":"state"}]"#,
        r#"[{"func":"count"}]"#,
        "",
    ));

    let mut tx = begin_as(&orm, w.root).await;
    let elevated = tx.env().sudo(true);
    let mut system = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let system_env = system.env();
    for (tx, env) in [(&mut tx, &elevated), (&mut system, &system_env)] {
        for ast in [&ast, &grouped] {
            let ran = run(tx, env, ast).await.unwrap_err();
            assert_eq!(ran.kind(), ErrorKind::Access);
            assert_eq!(ran.message(), NO_SUDO);
            let checked = check(tx, env, ast).await.unwrap_err();
            same_error(&checked, &ran);
        }
    }
    // the same transaction's ordinary environment still runs it
    let env = tx.env();
    run(&mut tx, &env, &ast).await.unwrap();
    drop(tx);
    drop(system);
    drop(client);
    h.cleanup().await.unwrap();
}

/// F5: a grouped query whose domain matches nothing is still checked for its `having`.
#[tokio::test]
async fn read_group_checks_having_when_the_domain_is_empty() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;
    // a list of mixed kinds is not a valid `having` value
    let having = r#","having":[{"aggregate":"__count","op":"in","value":[1,"a"]}]"#;
    let live = parse(&group_doc(
        r#"[{"path":"state"}]"#,
        r#"[{"func":"count"}]"#,
        having,
    ));
    let empty = parse(&group_doc(
        r#"[{"path":"state"}]"#,
        r#"[{"func":"count"}]"#,
        &format!(r#"{having},"domain":[[0,"=",1]]"#),
    ));
    let mut tx = begin_as(&orm, w.root).await;
    let env = tx.env();
    let none = env
        .records(ORDER)
        .unwrap()
        .read_group(
            &mut tx,
            ReadGroup::new(Domain::FALSE)
                .groupby(["state"])
                .aggregates(["__count"])
                .having(vec![Value::list([
                    Value::str("__count"),
                    Value::str("in"),
                    Value::list([Value::Int(1), Value::str("a")]),
                ])])
                .external(true),
        )
        .await
        .unwrap();
    assert!(none.is_empty());
    let expected = run(&mut tx, &env, &live).await.unwrap_err();
    assert_eq!(expected.kind(), ErrorKind::Validation);
    for result in [
        run(&mut tx, &env, &empty).await.map(|_| ()),
        check(&mut tx, &env, &empty).await,
    ] {
        same_error(&result.unwrap_err(), &expected);
    }
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}

/// F8: groups tied on the aggregate come back in a fixed order, so offset paging is stable.
#[tokio::test]
async fn grouped_pages_do_not_overlap_when_aggregates_tie() {
    let h = harness().await;
    let client = connect(&h).await;
    let w = seed(&client).await;
    acl(&client, &w, false, None).await;
    let orm = policy_orm(&h).await;
    data(&orm, &w).await;
    // groups of (state, company) ordered by their count, which two of them share
    let page = |limit: u32, offset: u32| {
        parse(&group_doc(
            r#"[{"path":"state"},{"path":"company_id"}]"#,
            r#"[{"func":"count"}]"#,
            &format!(r#","order":[{{"key":"__count","dir":"desc"}}],"offset":{offset}"#),
        ))
        .with_page(limit, offset, &QueryLimits::default())
        .unwrap()
    };
    let mut tx = begin_as(&orm, w.root).await;
    let env = tx.env();
    let mut paged = Vec::new();
    for offset in 0..6 {
        match run(&mut tx, &env, &page(1, offset)).await.unwrap() {
            QueryResult::Groups { mut rows, .. } => paged.append(&mut rows),
            other => panic!("{other:?}"),
        }
    }
    let QueryResult::Groups { rows: all, .. } = run(&mut tx, &env, &page(100, 0)).await.unwrap()
    else {
        panic!()
    };
    assert!(all.len() >= 3, "{all:?}");
    // a tie really exists in the data, or the test proves nothing
    let counts: Vec<&Value> = all.iter().map(|r| &r[2]).collect();
    assert!(
        counts
            .iter()
            .enumerate()
            .any(|(i, c)| counts[..i].contains(c)),
        "no two groups share a count: {all:?}"
    );
    // the pages of one row, put together, are the one page of all of them
    assert_eq!(paged.len(), all.len(), "{paged:?}");
    assert_eq!(paged, all, "pages of one are the one page of all");
    drop(tx);
    drop(client);
    h.cleanup().await.unwrap();
}
