//! Relational conditions in searches (DOM §4.5, §4.8, §4.10, §4.14; REL §4.14): FULL rewrites,
//! join / `EXISTS` shapes (sql-shape cases compare normalized structure), result
//! semantics of every relation type, hierarchy operators, ordering by many2one.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use std::collections::BTreeSet;

use erp_sdk::domain::Domain;
use erp_sdk::serde_json::{Value as Json, json};
use erp_sdk::{
    ErrorKind, Operation, OptimizationLevel, Records, SearchOptions, Transaction, Value,
};
use erp_testkit::{Harness, assert_query_count};
use support::{assert_sql, dom, map};

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

async fn search(tx: &mut Transaction, records: &Records, domain: Json) -> Vec<i64> {
    records.search(tx, &dom(domain)).await.unwrap().ids()
}

async fn search_set(tx: &mut Transaction, records: &Records, domain: Json) -> BTreeSet<i64> {
    search(tx, records, domain).await.into_iter().collect()
}

/// The SQL of a search (the last statement it runs).
async fn sql_of(tx: &mut Transaction, records: &Records, domain: Json) -> String {
    tx.capture_statements(true);
    records.search(tx, &dom(domain)).await.unwrap();
    let sql = tx.captured().last().cloned().unwrap_or_default();
    tx.capture_statements(false);
    sql
}

fn opt(records: &Records, domain: Json, level: OptimizationLevel) -> String {
    records
        .optimize_domain(&dom(domain), level, None)
        .unwrap()
        .to_string()
}

fn ids(v: &[i64]) -> BTreeSet<i64> {
    v.iter().copied().collect()
}

/// Record rules `id < 1000` on some models (DOM-040/041).
struct Rules(&'static [&'static str]);

impl erp_testkit::TestRules for Rules {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &erp_registry::ModelSpec,
        _op: Operation,
    ) -> erp_sdk::Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &erp_registry::ModelSpec,
        _op: Operation,
    ) -> erp_sdk::Result<Domain> {
        Ok(if self.0.contains(&model.name) {
            Domain::field("id")?.lt(1000)
        } else {
            Domain::TRUE
        })
    }
}

async fn rules_orm(models: &'static [&'static str]) -> Harness {
    Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_rel::ADDON,
    ])
    .rules(Rules(models))
    .build()
    .await
    .unwrap()
}

#[tokio::test]
async fn spec_dom_021_022_any_rights_and_id_lookup() {
    let h = support::rel_orm().await;
    let tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_rel.partner").unwrap();
    let user = partners.with_user(7);
    use OptimizationLevel::Full;
    let any = json!([["country_id", "any", [["name", "like", "one"]]]]);
    assert_eq!(
        opt(&user, any.clone(), Full),
        "[('country_id', 'any', [('name', 'like', 'one')])]",
        "unchanged for a plain user"
    );
    assert_eq!(
        opt(&partners, any, Full),
        "[('country_id', 'any!', [('name', 'like', 'one')])]",
        "superuser bypasses access"
    );
    // a bypass field (the delegate many2one of `_inherits`) is `any!` for any user
    let boxes = tx.env().records("test_rel.box").unwrap().with_user(7);
    assert_eq!(
        opt(
            &boxes,
            json!([["unit_id", "any", [["size", "=", 3]]]]),
            Full
        ),
        "[('unit_id', 'any!', [('size', 'in', [3])])]"
    );
    // DOM-022: `m2o.id` comparisons collapse to the column once access is bypassed
    assert_eq!(
        opt(&user, json!([["country_id.id", "=", 2]]), Full),
        "[('country_id', 'any', [('id', 'in', [2])])]"
    );
    assert_eq!(
        opt(&partners, json!([["country_id.id", "=", 2]]), Full),
        "[('country_id', 'in', [2])]"
    );
    assert_eq!(
        opt(
            &partners,
            json!([["country_id.id", "in", [2, false]]]),
            Full
        ),
        "[('country_id', 'in', [2])]"
    );
    assert_eq!(
        opt(&partners, json!([["country_id.id", "not in", [2]]]), Full),
        "[('country_id', 'not in', [2, False])]"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_023_073_074_x2many_conditions() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let cats = create(
        &mut tx,
        "test_rel.category",
        &[&[("name", Value::str("A"))], &[("name", Value::str("B"))]],
    )
    .await;
    let (ca, cb) = (cats.ids()[0], cats.ids()[1]);
    let p = |name: &str, cats: Vec<i64>| {
        vec![
            ("name", Value::str(name)),
            ("category_id", Value::list(cats.into_iter().map(Value::Int))),
        ]
    };
    let rows = [
        p("a", vec![ca]),
        p("b", vec![cb]),
        p("ab", vec![ca, cb]),
        p("c", vec![]),
    ];
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let ps = create(&mut tx, "test_rel.partner", &refs).await;
    let [a, b, ab, c] = [0, 1, 2, 3].map(|i| ps.ids()[i]);
    let partners = tx.env().records("test_rel.partner").unwrap();
    let all = ids(&ps.ids());
    for (domain, expected) in [
        (json!([["category_id", "in", [ca]]]), vec![a, ab]),
        (json!([["category_id", "in", [cb]]]), vec![b, ab]),
        (json!([["category_id", "in", [ca, cb]]]), vec![a, b, ab]),
        (
            json!([
                "|",
                ["category_id", "in", [ca]],
                ["category_id", "in", [cb]]
            ]),
            vec![a, b, ab],
        ),
        (
            json!([["category_id", "in", [ca]], ["category_id", "in", [cb]]]),
            vec![ab],
        ),
        (json!([["category_id", "not in", [ca, cb]]]), vec![c]),
        (
            json!([
                ["category_id", "not in", [ca]],
                ["category_id", "not in", [cb]]
            ]),
            vec![c],
        ),
        (json!([["category_id", "not in", [ca]]]), vec![b, c]),
        (json!([["category_id", "in", [false]]]), vec![c]),
        (json!([["category_id", "in", [ca, false]]]), vec![a, ab, c]),
        (json!([["category_id", "in", []]]), vec![]),
        (json!([["category_id", "=", false]]), vec![c]),
        (json!([["category_id", "!=", false]]), vec![a, b, ab]),
        (json!([["category_id", "=", "A"]]), vec![a, ab]),
        (json!([["category_id", "!=", "B"]]), vec![a, c]),
        (json!([["category_id", "like", "A"]]), vec![a, ab]),
        (json!([["category_id", "not ilike", "B"]]), vec![a, c]),
    ] {
        let found = search_set(&mut tx, &partners, domain.clone()).await;
        assert_eq!(found, ids(&expected), "{domain}");
        // a domain and its negation partition the records
        let negated = partners
            .search(&mut tx, &!&dom(domain.clone()))
            .await
            .unwrap()
            .ids();
        let union: BTreeSet<i64> = found.iter().chain(negated.iter()).copied().collect();
        assert_eq!(union, all, "{domain}");
        assert!(negated.iter().all(|i| !found.contains(i)), "{domain}");
    }
    // one2many: lines with a NOT NULL inverse (no IS NOT NULL guard), sql-shape of DOM-023
    let orders = create(&mut tx, "test_rel.base", &[&[], &[], &[]]).await;
    let [x, y, z] = [0, 1, 2].map(|i| orders.ids()[i]);
    let lines = create(
        &mut tx,
        "test_rel.line",
        &[
            &[("base_id", Value::Int(x)), ("value", Value::Int(1))],
            &[("base_id", Value::Int(x)), ("value", Value::Int(2))],
            &[("base_id", Value::Int(y)), ("value", Value::Int(3))],
        ],
    )
    .await;
    let [l1, l2, l3] = [0, 1, 2].map(|i| lines.ids()[i]);
    let bases = tx.env().records("test_rel.base").unwrap();
    for (domain, expected) in [
        (json!([["line_ids", "in", [l1, l2, l3]]]), vec![x, y]),
        (json!([["line_ids", "in", [l1, l3]]]), vec![x, y]),
        (json!([["line_ids", "in", [l1]]]), vec![x]),
        (json!([["line_ids", "in", []]]), vec![]),
        (json!([["line_ids", "not in", [l1]]]), vec![y, z]),
        (json!([["line_ids", "=", false]]), vec![z]),
        (json!([["line_ids", "!=", false]]), vec![x, y]),
        (json!([["line_ids.value", "=", 1]]), vec![x]),
        (json!([["line_ids.value", "!=", 1]]), vec![x, y]),
    ] {
        assert_eq!(
            search_set(&mut tx, &bases, domain.clone()).await,
            ids(&expected),
            "{domain}"
        );
    }
    let sql = sql_of(&mut tx, &bases, json!([["line_ids", "!=", false]])).await;
    assert_sql(
        &sql,
        "SELECT \"test_rel_base\".\"id\" FROM \"test_rel_base\" WHERE EXISTS(SELECT FROM (SELECT \
         \"test_rel_line\".\"base_id\" AS \"__inverse\" FROM \"test_rel_line\") AS \"__sub\" \
         WHERE \"__inverse\" = \"test_rel_base\".\"id\") ORDER BY \"test_rel_base\".\"id\"",
    );
    let sql = sql_of(&mut tx, &bases, json!([["line_ids", "=", false]])).await;
    assert!(
        sql.contains("WHERE NOT EXISTS(SELECT FROM (SELECT"),
        "{sql}"
    );
    let sql = sql_of(&mut tx, &bases, json!([["line_ids", "in", [l1]]])).await;
    assert_sql(
        &sql,
        "SELECT \"test_rel_base\".\"id\" FROM \"test_rel_base\" WHERE EXISTS(SELECT FROM (SELECT \
         \"test_rel_line\".\"base_id\" AS \"__inverse\" FROM \"test_rel_line\" WHERE \
         \"test_rel_line\".\"id\" = ANY($1::int8[])) AS \"__sub\" WHERE \"__inverse\" = \
         \"test_rel_base\".\"id\") ORDER BY \"test_rel_base\".\"id\"",
    );
    let sql = sql_of(&mut tx, &partners, json!([["category_id", "in", [ca, cb]]])).await;
    assert_sql(
        &sql,
        "SELECT \"test_rel_partner\".\"id\" FROM \"test_rel_partner\" WHERE \
         (\"test_rel_partner\".\"active\" IS TRUE AND EXISTS (SELECT 1 FROM \
         \"test_rel_category_partner_rel\" AS \"test_rel_partner__category_id\" WHERE \
         \"test_rel_partner__category_id\".\"partner_id\" = \"test_rel_partner\".\"id\" AND \
         \"test_rel_partner__category_id\".\"category_id\" = ANY($1::int8[]))) ORDER BY \
         \"test_rel_partner\".\"id\"",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_024_comodel_context_inside_any() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let biz = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("owner"))]],
    )
    .await;
    let business = create(
        &mut tx,
        "test_rel.business",
        &[&[
            ("name", Value::str("archived biz")),
            ("active", Value::Bool(false)),
            ("partner_id", Value::Int(biz.ids()[0])),
        ]],
    )
    .await;
    let parent = create(
        &mut tx,
        "test_rel.partner",
        &[&[
            ("name", Value::str("parent")),
            ("business_id", Value::Int(business.ids()[0])),
        ]],
    )
    .await;
    let partners = tx.env().records("test_rel.partner").unwrap();
    // many2one: the archived business is found
    assert_eq!(
        search(
            &mut tx,
            &partners,
            json!([["business_id", "any", [["name", "like", "archived"]]]])
        )
        .await,
        parent.ids()
    );
    // x2many: an archived child does not count
    create(
        &mut tx,
        "test_rel.partner",
        &[&[
            ("name", Value::str("hidden child")),
            ("active", Value::Bool(false)),
            ("parent_id", Value::Int(parent.ids()[0])),
        ]],
    )
    .await;
    assert!(
        search(
            &mut tx,
            &partners,
            json!([["child_ids.name", "like", "hidden"]])
        )
        .await
        .is_empty()
    );
    let sql = sql_of(
        &mut tx,
        &partners,
        json!([["child_ids.name", "like", "hidden"]]),
    )
    .await;
    assert!(
        sql.contains("\"test_rel_partner\".\"active\" IS TRUE AND"),
        "{sql}"
    );
    // an archived record is found when the domain mentions the active field
    let cats = create(
        &mut tx,
        "test_rel.category",
        &[&[("name", Value::str("sellers"))]],
    )
    .await;
    let archived = create(
        &mut tx,
        "test_rel.partner",
        &[&[
            ("name", Value::str("seller")),
            ("active", Value::Bool(false)),
            ("category_id", Value::list([Value::Int(cats.ids()[0])])),
        ]],
    )
    .await;
    assert_eq!(
        search(
            &mut tx,
            &partners,
            json!([["category_id", "ilike", "sellers"], ["active", "=", false]])
        )
        .await,
        archived.ids()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_032_inherits_field_search() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let boxes = create(
        &mut tx,
        "test_rel.box",
        &[
            &[("name", Value::str("foo box"))],
            &[("name", Value::str("bar box"))],
        ],
    )
    .await;
    let model = tx.env().records("test_rel.box").unwrap();
    let expected = "SELECT \"test_rel_box\".\"id\" FROM \"test_rel_box\" JOIN \"test_rel_unit\" AS \
                    \"test_rel_box__unit_id\" ON (\"test_rel_box\".\"unit_id\" = \
                    \"test_rel_box__unit_id\".\"id\") WHERE \"test_rel_box__unit_id\".\"name\" LIKE \
                    $1::text ORDER BY \"test_rel_box\".\"id\"";
    for domain in [
        json!([["name", "like", "foo"]]),
        json!([["unit_id.name", "like", "foo"]]),
    ] {
        assert_eq!(
            search(&mut tx, &model, domain.clone()).await,
            [boxes.ids()[0]]
        );
        assert_sql(&sql_of(&mut tx, &model, domain).await, expected);
    }
    // as a plain user too (the delegate link bypasses access)
    assert_eq!(
        search(
            &mut tx,
            &model.with_user(7),
            json!([["name", "like", "bar"]])
        )
        .await,
        [boxes.ids()[1]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_034_hierarchy_without_parent_store() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        "test_rel.category",
        &[&[("name", Value::str("Root name"))]],
    )
    .await;
    let mid = create(
        &mut tx,
        "test_rel.category",
        &[&[
            ("name", Value::str("mid")),
            ("parent_id", Value::Int(root.ids()[0])),
        ]],
    )
    .await;
    let leaf = create(
        &mut tx,
        "test_rel.category",
        &[&[
            ("name", Value::str("leaf")),
            ("parent_id", Value::Int(mid.ids()[0])),
        ]],
    )
    .await;
    let other = create(
        &mut tx,
        "test_rel.category",
        &[&[("name", Value::str("other"))]],
    )
    .await;
    let cats = tx.env().records("test_rel.category").unwrap();
    let family = ids(&[root.ids()[0], mid.ids()[0], leaf.ids()[0]]);
    assert_eq!(
        search_set(&mut tx, &cats, json!([["id", "child_of", root.ids()[0]]])).await,
        family
    );
    assert_eq!(
        search_set(
            &mut tx,
            &cats,
            json!([["parent_id", "child_of", "Root name"]])
        )
        .await,
        family
    );
    assert_eq!(
        search_set(
            &mut tx,
            &cats,
            json!([["id", "parent_of", [leaf.ids()[0]]]])
        )
        .await,
        family
    );
    // many2many: records linked to the category or one of its descendants
    let ps = create(
        &mut tx,
        "test_rel.partner",
        &[
            &[
                ("name", Value::str("p1")),
                ("category_id", Value::list([Value::Int(leaf.ids()[0])])),
            ],
            &[
                ("name", Value::str("p2")),
                ("category_id", Value::list([Value::Int(other.ids()[0])])),
            ],
        ],
    )
    .await;
    let partners = tx.env().records("test_rel.partner").unwrap();
    assert_eq!(
        search(
            &mut tx,
            &partners,
            json!([["category_id", "child_of", [root.ids()[0]]]])
        )
        .await,
        [ps.ids()[0]]
    );
    // False: FALSE without any query
    let none = assert_query_count!(tx, <= 0, {
        cats.search(&mut tx, &dom(json!([["id", "child_of", false]])))
            .await
            .unwrap()
    });
    assert!(none.is_empty());
    // a non-relational field
    let err = cats
        .search(&mut tx, &dom(json!([["name", "child_of", 1]])))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message().contains("works only for relational fields"),
        "{err}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_038_many2one_join_shapes() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let bot = create(&mut tx, "test_rel.bot", &[&[("name", Value::str("xbot"))]]).await;
    let mid = create(
        &mut tx,
        "test_rel.mid",
        &[&[
            ("name", Value::str("xmid")),
            ("bot_rel_req_id", Value::Int(bot.ids()[0])),
        ]],
    )
    .await;
    let top = create(
        &mut tx,
        "test_rel.top",
        &[&[
            ("name", Value::str("t")),
            ("mid_rel_req_id", Value::Int(mid.ids()[0])),
        ]],
    )
    .await;
    let tops = tx.env().records("test_rel.top").unwrap();
    let head = "SELECT \"test_rel_top\".\"id\" FROM \"test_rel_top\"";
    let tail = "ORDER BY \"test_rel_top\".\"id\"";
    // required: JOIN, no guard
    let sql = sql_of(
        &mut tx,
        &tops,
        json!([["mid_rel_req_id.name", "like", "x"]]),
    )
    .await;
    assert_sql(
        &sql,
        &format!(
            "{head} JOIN \"test_rel_mid\" AS \"test_rel_top__mid_rel_req_id\" ON \
             (\"test_rel_top\".\"mid_rel_req_id\" = \"test_rel_top__mid_rel_req_id\".\"id\") WHERE \
             \"test_rel_top__mid_rel_req_id\".\"name\" LIKE $1::text {tail}"
        ),
    );
    // optional: LEFT JOIN with the guards
    let sql = sql_of(&mut tx, &tops, json!([["mid_rel_id.name", "like", "x"]])).await;
    assert_sql(
        &sql,
        &format!(
            "{head} LEFT JOIN \"test_rel_mid\" AS \"test_rel_top__mid_rel_id\" ON \
             (\"test_rel_top\".\"mid_rel_id\" = \"test_rel_top__mid_rel_id\".\"id\") WHERE \
             (\"test_rel_top\".\"mid_rel_id\" IS NOT NULL AND \"test_rel_top__mid_rel_id\".\"id\" IS \
             NOT NULL AND \"test_rel_top__mid_rel_id\".\"name\" LIKE $1::text) {tail}"
        ),
    );
    // in an OR, the guards stay inside the branch
    let sql = sql_of(
        &mut tx,
        &tops,
        json!(["|", ["mid_rel_id.name", "like", "x"], ["id", "=", 0]]),
    )
    .await;
    assert!(
        norm(&sql).contains(
            "WHERE (\"test_rel_top\".\"id\" = ANY($1::int8[]) OR (\"test_rel_top\".\"mid_rel_id\" IS NOT \
             NULL AND \"test_rel_top__mid_rel_id\".\"id\" IS NOT NULL AND \
             \"test_rel_top__mid_rel_id\".\"name\" LIKE $2::text))"
        ),
        "{sql}"
    );
    // required then optional: JOIN then LEFT JOIN
    let sql = sql_of(
        &mut tx,
        &tops,
        json!([["mid_rel_req_id.bot_rel_id.name", "ilike", "x"]]),
    )
    .await;
    assert!(
        norm(&sql).contains(
            "FROM \"test_rel_top\" JOIN \"test_rel_mid\" AS \"test_rel_top__mid_rel_req_id\" ON \
             (\"test_rel_top\".\"mid_rel_req_id\" = \"test_rel_top__mid_rel_req_id\".\"id\") LEFT JOIN \
             \"test_rel_bot\" AS \"test_rel_top__mid_rel_req_id__bot_rel_id\""
        ),
        "{sql}"
    );
    // optional then required: LEFT JOIN twice (not promoted), no guard on the required link
    let sql = sql_of(
        &mut tx,
        &tops,
        json!([["mid_rel_id.bot_rel_req_id.name", "ilike", "x"]]),
    )
    .await;
    let n = norm(&sql);
    assert!(
        n.contains("LEFT JOIN \"test_rel_mid\" AS \"test_rel_top__mid_rel_id\"")
            && n.contains(
                "LEFT JOIN \"test_rel_bot\" AS \"test_rel_top__mid_rel_id__bot_rel_req_id\""
            ),
        "{sql}"
    );
    // `not like` stays a positive `any` with the guards and a NULL-tolerant negation inside
    let sql = sql_of(
        &mut tx,
        &tops,
        json!([["mid_rel_id.name", "not like", "x"]]),
    )
    .await;
    assert!(
        norm(&sql).contains(
            "(\"test_rel_top\".\"mid_rel_id\" IS NOT NULL AND \"test_rel_top__mid_rel_id\".\"id\" IS \
             NOT NULL AND (\"test_rel_top__mid_rel_id\".\"name\" NOT LIKE $1::text OR \
             \"test_rel_top__mid_rel_id\".\"name\" IS NULL))"
        ),
        "{sql}"
    );
    // results
    assert_eq!(
        search(
            &mut tx,
            &tops,
            json!([["mid_rel_req_id.name", "like", "x"]])
        )
        .await,
        top.ids()
    );
    assert!(
        search(&mut tx, &tops, json!([["mid_rel_id.name", "like", "x"]]))
            .await
            .is_empty()
    );
    assert_eq!(
        search(
            &mut tx,
            &tops,
            json!([["mid_rel_req_id.bot_rel_req_id.name", "=", "xbot"]])
        )
        .await,
        top.ids()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

fn norm(sql: &str) -> String {
    support::norm_sql(sql)
}

/// instead of `IS NULL OR … NOT IN (…)`; no join, same rows)
#[tokio::test]
async fn spec_dom_039_070_many2one_sub_field_shapes() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ps = create(
        &mut tx,
        "test_rel.partner",
        &[
            &[
                ("name", Value::str("jack")),
                ("email", Value::str("jack@example.com")),
            ],
            &[
                ("name", Value::str("jill")),
                ("email", Value::str("jill@sgc.us")),
            ],
        ],
    )
    .await;
    let ms = create(
        &mut tx,
        "test_rel.multi",
        &[
            &[("partner", Value::Int(ps.ids()[0]))],
            &[("partner", Value::Int(ps.ids()[1]))],
            &[],
        ],
    )
    .await;
    let [m_jack, m_jill, m_none] = [0, 1, 2].map(|i| ms.ids()[i]);
    let multi = tx.env().records("test_rel.multi").unwrap();
    let join = "LEFT JOIN \"test_rel_partner\" AS \"test_rel_multi__partner\" ON \
                (\"test_rel_multi\".\"partner\" = \"test_rel_multi__partner\".\"id\")";
    let guard = "\"test_rel_multi\".\"partner\" IS NOT NULL AND \"test_rel_multi__partner\".\"id\" IS NOT NULL";
    // AND / OR: one join, sub-conditions in field order
    let sql = sql_of(
        &mut tx,
        &multi,
        json!([
            ["partner.name", "like", "jack"],
            ["partner.email", "like", "@example.com"]
        ]),
    )
    .await;
    assert_sql(
        &sql,
        &format!(
            "SELECT \"test_rel_multi\".\"id\" FROM \"test_rel_multi\" {join} WHERE ({guard} AND \
             (\"test_rel_multi__partner\".\"email\" LIKE $1::text AND \
             \"test_rel_multi__partner\".\"name\" LIKE $2::text)) ORDER BY \"test_rel_multi\".\"id\""
        ),
    );
    let sql = sql_of(
        &mut tx,
        &multi,
        json!([
            "|",
            ["partner.name", "like", "jack"],
            ["partner.email", "like", "@example.com"]
        ]),
    )
    .await;
    assert!(
        norm(&sql).contains(&norm(&format!(
            "{join} WHERE ({guard} AND (\"test_rel_multi__partner\".\"email\" LIKE $1::text OR \
         \"test_rel_multi__partner\".\"name\" LIKE $2::text))"
        ))),
        "{sql}"
    );
    // negations: NOT EXISTS, no join
    for negated in [
        json!([
            "!",
            "&",
            ["partner.name", "like", "jack"],
            ["partner.email", "like", "@example.com"]
        ]),
        json!([["partner", "not like", "jack"]]),
    ] {
        let sql = sql_of(&mut tx, &multi, negated.clone()).await;
        assert!(!sql.contains("JOIN"), "{sql}");
        assert!(
            norm(&sql).contains(
                "NOT EXISTS (SELECT 1 FROM \"test_rel_partner\" AS \"test_rel_multi__partner\" WHERE \
                 \"test_rel_multi__partner\".\"id\" = \"test_rel_multi\".\"partner\" AND"
            ),
            "{sql}"
        );
    }
    assert_eq!(
        search_set(&mut tx, &multi, json!([["partner", "not like", "jack"]])).await,
        ids(&[m_jill, m_none])
    );
    assert_eq!(
        search_set(
            &mut tx,
            &multi,
            json!([
                "!",
                "|",
                ["partner.name", "like", "jack"],
                ["partner.email", "like", "sgc"]
            ])
        )
        .await,
        ids(&[m_none])
    );
    assert_eq!(
        search_set(
            &mut tx,
            &multi,
            json!([
                ["partner.email", "like", "@"],
                "|",
                ["partner.name", "like", "jack"],
                ["partner.email", "like", "sgc"]
            ])
        )
        .await,
        ids(&[m_jack, m_jill])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_040_one2many_any_shape() {
    let h = rules_orm(&["test_rel.multi", "test_rel.multi_line"]).await;
    let mut tx = h.begin().await.unwrap();
    let ms = create(&mut tx, "test_rel.multi", &[&[], &[]]).await;
    create(
        &mut tx,
        "test_rel.multi_line",
        &[&[
            ("name", Value::str("a")),
            ("multi", Value::Int(ms.ids()[0])),
        ]],
    )
    .await;
    let multi = tx.env().records("test_rel.multi").unwrap().with_user(7);
    let sql = sql_of(
        &mut tx,
        &multi,
        json!([["lines", "any", [["name", "=", "a"]]]]),
    )
    .await;
    assert_sql(
        &sql,
        "SELECT \"test_rel_multi\".\"id\" FROM \"test_rel_multi\" WHERE EXISTS(SELECT FROM (SELECT \
         \"test_rel_multi_line\".\"multi\" AS \"__inverse\" FROM \"test_rel_multi_line\" WHERE \
         (\"test_rel_multi_line\".\"id\" < $1::int8 AND \"test_rel_multi_line\".\"multi\" IS NOT \
         NULL AND \"test_rel_multi_line\".\"name\" = ANY($2::text[]))) AS \"__sub\" WHERE \
         \"__inverse\" = \"test_rel_multi\".\"id\") AND \"test_rel_multi\".\"id\" < $3::int8 ORDER BY \
         \"test_rel_multi\".\"id\"",
    );
    tx.capture_statements(true);
    let bypass = Domain::condition(
        "lines",
        erp_sdk::Operator::AnyBypass,
        Domain::condition("name", erp_sdk::Operator::Eq, "a").unwrap(),
    )
    .unwrap();
    multi.search(&mut tx, &bypass).await.unwrap();
    let sql = tx.captured().last().cloned().unwrap();
    tx.capture_statements(false);
    assert!(
        !norm(&sql).contains("\"test_rel_multi_line\".\"id\" <"),
        "any! drops the comodel's rule: {sql}"
    );
    assert_eq!(
        search(
            &mut tx,
            &multi,
            json!([["lines", "any", [["name", "=", "a"]]]])
        )
        .await,
        [ms.ids()[0]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_041_many2many_any_shape() {
    let h = rules_orm(&["test_rel.multi_line", "test_rel.multi_tag"]).await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("test_rel.multi_line").unwrap();
    let sql = sql_of(
        &mut tx,
        &lines.with_user(7),
        json!([["tags", "any", [["name", "=", "a"]]]]),
    )
    .await;
    assert_sql(
        &sql,
        "SELECT \"test_rel_multi_line\".\"id\" FROM \"test_rel_multi_line\" WHERE EXISTS (SELECT 1 \
         FROM \"test_rel_multi_line_test_rel_multi_tag_rel\" AS \"test_rel_multi_line__tags\" WHERE \
         \"test_rel_multi_line__tags\".\"test_rel_multi_line_id\" = \"test_rel_multi_line\".\"id\" AND \
         \"test_rel_multi_line__tags\".\"test_rel_multi_tag_id\" IN (SELECT \"test_rel_multi_tag\".\"id\" \
         FROM \"test_rel_multi_tag\" WHERE (\"test_rel_multi_tag\".\"id\" < $1::int8 AND \
         \"test_rel_multi_tag\".\"name\" = ANY($2::text[])))) AND \"test_rel_multi_line\".\"id\" < \
         $3::int8 ORDER BY \"test_rel_multi_line\".\"id\"",
    );
    let sql = sql_of(&mut tx, &lines, json!([["tags", "in", [1, 2, 3]]])).await;
    assert_sql(
        &sql,
        "SELECT \"test_rel_multi_line\".\"id\" FROM \"test_rel_multi_line\" WHERE EXISTS (SELECT 1 FROM \
         \"test_rel_multi_line_test_rel_multi_tag_rel\" AS \"test_rel_multi_line__tags\" WHERE \
         \"test_rel_multi_line__tags\".\"test_rel_multi_line_id\" = \"test_rel_multi_line\".\"id\" AND \
         \"test_rel_multi_line__tags\".\"test_rel_multi_tag_id\" = ANY($1::int8[])) ORDER BY \
         \"test_rel_multi_line\".\"id\"",
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_071_x2many_sub_field_shapes() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let multi = tx.env().records("test_rel.multi").unwrap();
    let exists_lines = |inner: &str| {
        format!(
            "EXISTS(SELECT FROM (SELECT \"test_rel_multi_line\".\"multi\" AS \"__inverse\" FROM \
             \"test_rel_multi_line\" WHERE {inner}) AS \"__sub\" WHERE \"__inverse\" = \
             \"test_rel_multi\".\"id\")"
        )
    };
    let sql = sql_of(
        &mut tx,
        &multi,
        json!([["lines.name", "like", "x"], ["lines.name", "like", "y"]]),
    )
    .await;
    let first = exists_lines(
        "(\"test_rel_multi_line\".\"multi\" IS NOT NULL AND \"test_rel_multi_line\".\"name\" LIKE $1::text)",
    );
    let second = exists_lines(
        "(\"test_rel_multi_line\".\"multi\" IS NOT NULL AND \"test_rel_multi_line\".\"name\" LIKE $2::text)",
    );
    assert!(
        norm(&sql).contains(&norm(&format!("WHERE ({first} AND {second})"))),
        "{sql}"
    );
    let sql = sql_of(
        &mut tx,
        &multi,
        json!([
            "|",
            ["lines.name", "like", "x"],
            ["lines.name", "like", "y"]
        ]),
    )
    .await;
    let one = exists_lines(
        "(\"test_rel_multi_line\".\"multi\" IS NOT NULL AND (\"test_rel_multi_line\".\"name\" LIKE \
         $1::text OR \"test_rel_multi_line\".\"name\" LIKE $2::text))",
    );
    assert!(norm(&sql).contains(&norm(&format!("WHERE {one}"))), "{sql}");
    // many2many: the field domain is repeated in every sub-select
    let sql = sql_of(
        &mut tx,
        &multi,
        json!([["tags.name", "like", "x"], ["tags.name", "like", "y"]]),
    )
    .await;
    assert_eq!(norm(&sql).matches("ILIKE").count(), 2, "{sql}");
    assert_eq!(
        norm(&sql)
            .matches("EXISTS (SELECT 1 FROM \"test_rel_multi_test_rel_multi_tag_rel\"")
            .count(),
        2,
        "{sql}"
    );
    let sql = sql_of(
        &mut tx,
        &multi,
        json!(["|", ["tags.name", "like", "x"], ["tags.name", "like", "y"]]),
    )
    .await;
    assert_eq!(norm(&sql).matches("EXISTS").count(), 1, "{sql}");
    assert!(
        norm(&sql).contains("\"test_rel_multi_tag\".\"name\" ILIKE $1::text AND (\"test_rel_multi_tag\".\"name\" LIKE $2::text OR \"test_rel_multi_tag\".\"name\" LIKE $3::text)"),
        "{sql}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_072_empty_query_short_circuits() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ms = create(&mut tx, "test_rel.multi", &[&[], &[]]).await;
    let multi = tx.env().records("test_rel.multi").unwrap();
    let empty = tx.env().records("test_rel.multi_tag").unwrap().as_query();
    let any = Domain::condition("tags", erp_sdk::Operator::Any, empty.clone()).unwrap();
    let none = assert_query_count!(tx, <= 0, { multi.search(&mut tx, &any).await.unwrap() });
    assert!(none.is_empty());
    let not_any = Domain::condition("tags", erp_sdk::Operator::NotAny, empty).unwrap();
    tx.capture_statements(true);
    let all = multi.search(&mut tx, &not_any).await.unwrap();
    assert_eq!(ids(&all.ids()), ids(&ms.ids()));
    assert!(
        tx.captured().iter().all(|s| !s.contains("tag_rel")),
        "{:?}",
        tx.captured()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_075_085_many2one_nulls_and_names() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let parents = create(
        &mut tx,
        "test_rel.partner",
        &[
            &[("name", Value::str("Pepper Street"))],
            &[("name", Value::str("Inner Works"))],
        ],
    )
    .await;
    let (pepper, inner) = (parents.ids()[0], parents.ids()[1]);
    let kids = create(
        &mut tx,
        "test_rel.partner",
        &[
            &[
                ("name", Value::str("k1")),
                ("parent_id", Value::Int(pepper)),
            ],
            &[("name", Value::str("k2")), ("parent_id", Value::Int(inner))],
        ],
    )
    .await;
    let (k1, k2) = (kids.ids()[0], kids.ids()[1]);
    let partners = tx.env().records("test_rel.partner").unwrap();
    let all = ids(&[pepper, inner, k1, k2]);
    let with = ids(&[k1, k2]);
    let without = ids(&[pepper, inner]);
    for (domain, expected) in [
        (json!([["parent_id", "=", false]]), without.clone()),
        (json!([["parent_id", "=", "Pepper Street"]]), ids(&[k1])),
        (json!([["parent_id", "in", "Pepper Street"]]), ids(&[k1])),
        (
            json!([["parent_id", "in", ["Pepper Street", "Inner Works"]]]),
            with.clone(),
        ),
        (json!([["parent_id", "in", [false]]]), without.clone()),
        (json!([["parent_id", "in", []]]), ids(&[])),
        (json!([["parent_id", "not in", [999999]]]), all.clone()),
        (
            json!([["parent_id", "not like", "nonexistent"]]),
            all.clone(),
        ),
        (json!([["parent_id", "like", "nonexistent"]]), ids(&[])),
        (json!([["parent_id", "!=", false]]), with.clone()),
        (json!([["parent_id", "not in", [false]]]), with.clone()),
        (json!([["parent_id", "ilike", ""]]), with.clone()),
        (json!([["parent_id", "not ilike", ""]]), without.clone()),
        (json!([["parent_id", "not in", []]]), all.clone()),
        (
            json!(["!", ["parent_id", "=", "Pepper Street"]]),
            ids(&[pepper, inner, k2]),
        ),
        (json!([["parent_id.parent_id", "in", []]]), ids(&[])),
        (json!([["parent_id.parent_id", "not in", []]]), with.clone()),
    ] {
        assert_eq!(
            search_set(&mut tx, &partners, domain.clone()).await,
            expected,
            "{domain}"
        );
    }
    // DOM-085
    let tops = tx.env().records("test_rel.top").unwrap();
    let sql = sql_of(
        &mut tx,
        &tops,
        json!([
            "|",
            ["mid_rel_id", "=", false],
            ["mid_rel_id.name", "=", false]
        ]),
    )
    .await;
    assert!(
        norm(&sql).contains(
            "LEFT JOIN \"test_rel_mid\" AS \"test_rel_top__mid_rel_id\" ON (\"test_rel_top\".\"mid_rel_id\" = \
             \"test_rel_top__mid_rel_id\".\"id\") WHERE (\"test_rel_top\".\"mid_rel_id\" IS NULL OR \
             (\"test_rel_top\".\"mid_rel_id\" IS NOT NULL AND \"test_rel_top__mid_rel_id\".\"id\" IS NOT \
             NULL AND (\"test_rel_top__mid_rel_id\".\"name\" = ANY($1::text[]) OR \
             \"test_rel_top__mid_rel_id\".\"name\" IS NULL)))"
        ),
        "{sql}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_076_any_not_any_complements() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let countries = create(
        &mut tx,
        "test_rel.country",
        &[&[("name", Value::str("BE"))]],
    )
    .await;
    create(
        &mut tx,
        "test_rel.partner",
        &[
            &[
                ("name", Value::str("a")),
                ("country_id", Value::Int(countries.ids()[0])),
            ],
            &[("name", Value::str("b"))],
        ],
    )
    .await;
    let boxes = create(
        &mut tx,
        "test_rel.box",
        &[
            &[("name", Value::str("Jean"))],
            &[("name", Value::str("Clude"))],
        ],
    )
    .await;
    let orders = create(&mut tx, "test_rel.base", &[&[], &[], &[]]).await;
    create(
        &mut tx,
        "test_rel.line",
        &[
            &[
                ("base_id", Value::Int(orders.ids()[0])),
                ("value", Value::Int(1)),
            ],
            &[
                ("base_id", Value::Int(orders.ids()[1])),
                ("value", Value::Int(20)),
            ],
        ],
    )
    .await;
    let tags = create(
        &mut tx,
        "test_rel.tag",
        &[
            &[("name", Value::str("Urgent"))],
            &[("name", Value::str("Other"))],
        ],
    )
    .await;
    orders
        .at(0)
        .unwrap()
        .set_value(&mut tx, "tag_ids", Value::list([Value::Int(tags.ids()[0])]))
        .await
        .unwrap();
    orders
        .at(1)
        .unwrap()
        .set_value(&mut tx, "tag_ids", Value::list([Value::Int(tags.ids()[1])]))
        .await
        .unwrap();
    let cases: Vec<(&str, &str, Json)> = vec![
        (
            "test_rel.partner",
            "country_id",
            json!([["name", "=", "BE"]]),
        ),
        ("test_rel.box", "unit_id", json!([["name", "=", "Jean"]])),
        ("test_rel.base", "line_ids", json!([["value", "=", 1]])),
        ("test_rel.base", "tag_ids", json!([["name", "=", "Urgent"]])),
    ];
    for user in [None, Some(7)] {
        for (model, field, sub) in &cases {
            let mut records = tx.env().records(model).unwrap();
            if let Some(uid) = user {
                records = records.with_user(uid);
            }
            let all = search_set(&mut tx, &records, json!([])).await;
            let any = search_set(&mut tx, &records, json!([[field, "any", sub]])).await;
            let not_any = search_set(&mut tx, &records, json!([[field, "not any", sub]])).await;
            assert!(!any.is_empty(), "{model}.{field}");
            assert!(any.is_disjoint(&not_any), "{model}.{field}");
            assert_eq!(&any | &not_any, all, "{model}.{field}");
        }
    }
    let box_model = tx.env().records("test_rel.box").unwrap();
    assert_eq!(
        search(
            &mut tx,
            &box_model,
            json!([["unit_id", "any", [["name", "=", "Jean"]]]])
        )
        .await,
        [boxes.ids()[0]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_077_active_test_inside_x2many() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let parent = create(&mut tx, "test_rel.active", &[&[("name", Value::str("P"))]]).await;
    let pid = parent.ids()[0];
    let kids = create(
        &mut tx,
        "test_rel.active",
        &[
            &[("name", Value::str("A")), ("parent_id", Value::Int(pid))],
            &[
                ("name", Value::str("B")),
                ("parent_id", Value::Int(pid)),
                ("active", Value::Bool(false)),
            ],
        ],
    )
    .await;
    parent
        .set_value(
            &mut tx,
            "relatives_ids",
            Value::list(kids.ids().into_iter().map(Value::Int)),
        )
        .await
        .unwrap();
    let model = tx.env().records("test_rel.active").unwrap();
    for (field, name, found) in [
        ("children_ids", "A", true),
        ("children_ids", "B", false),
        ("all_children_ids", "A", true),
        ("all_children_ids", "B", true),
    ] {
        for domain in [
            json!([[format!("{field}.name"), "=", name]]),
            json!([[field, "=", name]]),
            json!([[field, "child_of", name]]),
        ] {
            let ids = search(&mut tx, &model, domain.clone()).await;
            assert_eq!(ids.contains(&pid), found, "{domain}");
        }
    }
    for (field, kid, found) in [
        ("relatives_ids", kids.ids()[0], true),
        ("relatives_ids", kids.ids()[1], false),
        ("all_relatives_ids", kids.ids()[1], true),
    ] {
        let ids = search(&mut tx, &model, json!([[field, "child_of", kid]])).await;
        assert_eq!(ids.contains(&pid), found, "{field} child_of {kid}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_091_hierarchy_domain_seed() {
    use erp_sdk::domain::{Operator, Value as DomainValue};
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let root = create(
        &mut tx,
        "test_rel.category",
        &[&[("name", Value::str("root"))]],
    )
    .await;
    let mid = create(
        &mut tx,
        "test_rel.category",
        &[&[
            ("name", Value::str("mid")),
            ("parent_id", Value::Int(root.ids()[0])),
        ]],
    )
    .await;
    let leaf = create(
        &mut tx,
        "test_rel.category",
        &[&[
            ("name", Value::str("leaf")),
            ("parent_id", Value::Int(mid.ids()[0])),
        ]],
    )
    .await;
    let other = create(
        &mut tx,
        "test_rel.category",
        &[&[("name", Value::str("other"))]],
    )
    .await;
    let cats = tx.env().records("test_rel.category").unwrap();
    let seed = |name: &str| {
        DomainValue::Domain(Domain::condition("name", Operator::Eq, name.to_owned()).unwrap())
    };
    let (r, m, l) = (root.ids()[0], mid.ids()[0], leaf.ids()[0]);
    let down = Domain::condition("id", Operator::ChildOf, seed("mid")).unwrap();
    assert_eq!(
        cats.search(&mut tx, &down)
            .await
            .unwrap()
            .ids()
            .into_iter()
            .collect::<BTreeSet<_>>(),
        ids(&[m, l])
    );
    let up = Domain::condition("id", Operator::ParentOf, seed("mid")).unwrap();
    assert_eq!(
        cats.search(&mut tx, &up)
            .await
            .unwrap()
            .ids()
            .into_iter()
            .collect::<BTreeSet<_>>(),
        ids(&[r, m])
    );
    let none = Domain::condition("id", Operator::ChildOf, seed("absent")).unwrap();
    assert!(cats.search(&mut tx, &none).await.unwrap().is_empty());
    // the external JSON form carries no typed seed: a list of lists is not a start record
    let err = cats
        .search(
            &mut tx,
            &dom(json!([["id", "child_of", [["name", "=", "mid"]]]])),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message().contains("is not supported for child_of"),
        "{err}"
    );
    assert!(
        !cats
            .search(&mut tx, &down)
            .await
            .unwrap()
            .ids()
            .contains(&other.ids()[0])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_078_hierarchy_edge_cases() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let parents = create(
        &mut tx,
        "test_rel.active",
        &[
            &[("name", Value::str("Parent"))],
            &[
                ("name", Value::str("Parent")),
                ("active", Value::Bool(false)),
            ],
        ],
    )
    .await;
    let (pa, pi) = (parents.ids()[0], parents.ids()[1]);
    let kids = create(
        &mut tx,
        "test_rel.active",
        &[
            &[("name", Value::str("ca")), ("parent_id", Value::Int(pa))],
            &[("name", Value::str("ci")), ("parent_id", Value::Int(pi))],
        ],
    )
    .await;
    let (ca, ci) = (kids.ids()[0], kids.ids()[1]);
    let model = tx.env().records("test_rel.active").unwrap();
    // a many2one sub-search ignores `active`
    assert_eq!(
        search_set(&mut tx, &model, json!([["parent_id.name", "=", "Parent"]])).await,
        ids(&[ca, ci])
    );
    assert_eq!(
        search_set(&mut tx, &model, json!([["parent_id", "=", "Parent"]])).await,
        ids(&[ca, ci])
    );
    assert_eq!(
        search_set(&mut tx, &model, json!([["id", "child_of", pa]])).await,
        ids(&[pa, ca])
    );
    // a self-referencing field means the record and its descendants, like `id`
    assert_eq!(
        search_set(&mut tx, &model, json!([["parent_id", "child_of", pa]])).await,
        ids(&[pa, ca])
    );
    // value forms of child_of / parent_of
    let cats = tx.env().records("test_rel.category").unwrap();
    let b3 = create(
        &mut tx,
        "test_rel.category",
        &[&[("name", Value::str("B3"))]],
    )
    .await;
    let b4 = create(
        &mut tx,
        "test_rel.category",
        &[&[
            ("name", Value::str("B4")),
            ("parent_id", Value::Int(b3.ids()[0])),
        ]],
    )
    .await;
    let both = ids(&[b3.ids()[0], b4.ids()[0]]);
    for value in [
        json!(b3.ids()[0]),
        json!([b3.ids()[0]]),
        json!("B3"),
        json!(["B3"]),
    ] {
        assert_eq!(
            search_set(&mut tx, &cats, json!([["id", "child_of", value.clone()]])).await,
            both,
            "{value}"
        );
    }
    for value in [
        json!(b4.ids()[0]),
        json!([b4.ids()[0]]),
        json!("B4"),
        json!(["B4"]),
    ] {
        assert_eq!(
            search_set(&mut tx, &cats, json!([["id", "parent_of", value.clone()]])).await,
            both,
            "{value}"
        );
    }
    // a self loop terminates
    b3.set_value(&mut tx, "parent_id", Value::Int(b4.ids()[0]))
        .await
        .unwrap();
    assert_eq!(
        search_set(&mut tx, &cats, json!([["id", "parent_of", b3.ids()[0]]])).await,
        both
    );
    assert_eq!(
        search_set(&mut tx, &cats, json!([["id", "child_of", b3.ids()[0]]])).await,
        both
    );
    // missing ids: empty results, no error
    assert!(
        search(
            &mut tx,
            &cats,
            json!([["parent_id", "parent_of", 999999999]])
        )
        .await
        .is_empty()
    );
    assert!(
        search(
            &mut tx,
            &cats,
            json!([["parent_id", "child_of", 999999999]])
        )
        .await
        .is_empty()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_054_055_ordering_by_many2one() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let cs = create(
        &mut tx,
        "test_rel.ocountry",
        &[
            &[
                ("name", Value::str("c01")),
                ("phone_code", Value::str("01")),
            ],
            &[
                ("name", Value::str("c02")),
                ("phone_code", Value::str("02")),
            ],
        ],
    )
    .await;
    let ps = create(
        &mut tx,
        "test_rel.opartner",
        &[
            &[
                ("name", Value::str("a")),
                ("country_id", Value::Int(cs.ids()[0])),
            ],
            &[
                ("name", Value::str("c")),
                ("country_id", Value::Int(cs.ids()[1])),
            ],
            &[("name", Value::str("b"))],
        ],
    )
    .await;
    let [a, c, b] = [0, 1, 2].map(|i| ps.ids()[i]);
    let partners = tx.env().records("test_rel.opartner").unwrap();
    let ordered = async |tx: &mut Transaction, order: &str| {
        partners
            .search_with(tx, &Domain::TRUE, &SearchOptions::new().order(order))
            .await
            .unwrap()
            .ids()
    };
    assert_eq!(ordered(&mut tx, "country_id").await, [a, c, b]);
    assert_eq!(ordered(&mut tx, "country_id desc").await, [b, c, a]);
    assert_eq!(ordered(&mut tx, "country_id nulls first").await, [b, a, c]);
    assert_eq!(
        ordered(&mut tx, "country_id desc nulls last").await,
        [c, a, b]
    );
    assert_eq!(ordered(&mut tx, "country_id.id").await, [a, c, b]);
    tx.capture_statements(true);
    ordered(&mut tx, "country_id").await;
    let sql = tx.captured().last().cloned().unwrap();
    tx.capture_statements(false);
    assert_sql(
        &sql,
        "SELECT \"test_rel_opartner\".\"id\" FROM \"test_rel_opartner\" LEFT JOIN \"test_rel_ocountry\" \
         AS \"test_rel_opartner__country_id\" ON (\"test_rel_opartner\".\"country_id\" = \
         \"test_rel_opartner__country_id\".\"id\") ORDER BY \"test_rel_opartner__country_id\".\"phone_code\", \
         \"test_rel_opartner\".\"id\"",
    );
    let err = partners
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("country_id.name"),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Cannot order by comodel fields in 'country_id.name'"
    );
    // b gets a country whose phone code is NULL: the NULLS clause does not reach the inner key
    let nullc = create(
        &mut tx,
        "test_rel.ocountry",
        &[&[("name", Value::str("cnull"))]],
    )
    .await;
    ps.at(2)
        .unwrap()
        .set_value(&mut tx, "country_id", Value::Int(nullc.ids()[0]))
        .await
        .unwrap();
    assert_eq!(ordered(&mut tx, "country_id").await, [a, c, b]);
    assert_eq!(ordered(&mut tx, "country_id desc").await, [b, c, a]);
    // DOM-055: a self-referencing order terminates
    let alphas = create(
        &mut tx,
        "test_rel.alpha",
        &[&[("name", Value::str("A"))], &[("name", Value::str("B"))]],
    )
    .await;
    alphas
        .at(0)
        .unwrap()
        .set_value(&mut tx, "alpha_loop_id", Value::Int(alphas.ids()[1]))
        .await
        .unwrap();
    let model = tx.env().records("test_rel.alpha").unwrap();
    let found = model.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert_eq!(found.len(), 2);
    drop(tx);
    h.cleanup().await.unwrap();
}
