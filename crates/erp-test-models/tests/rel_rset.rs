//! Recordset operations through relational fields (RSET §4.8–4.13 relational parts, DOM §4.17):
//! `filtered`/`mapped` paths, `filtered_domain` with relational conditions and record rules,
//! `sorted` and `grouped` on relational fields, and their prefetch budgets.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_registry::ModelSpec;
use erp_sdk::serde_json::json;
use erp_sdk::{
    Domain, ErrorKind, Mapped, Operation, Operator, Records, Result, SearchOptions, Transaction,
    Value,
};
use erp_test_models::{base_users, test_rel};
use erp_testkit::{Harness, assert_query_count};
use support::{dom, map};

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

fn id(records: &Records, i: usize) -> i64 {
    records.ids()[i]
}

struct F1 {
    countries: Records,
    categories: Records,
    parents: Records,
    children: Records,
    /// Parents then children, in id order.
    all: Records,
}

async fn f1(tx: &mut Transaction) -> F1 {
    let countries = create(
        tx,
        "test_rel.country",
        &[
            &[("name", Value::str("Belgium")), ("code", Value::str("BE"))],
            &[("name", Value::str("France")), ("code", Value::str("FR"))],
        ],
    )
    .await;
    let categories = create(
        tx,
        "test_rel.category",
        &[
            &[("name", Value::str("vip"))],
            &[("name", Value::str("other"))],
        ],
    )
    .await;
    let (be, fr) = (id(&countries, 0), id(&countries, 1));
    let (vip, other) = (id(&categories, 0), id(&categories, 1));
    let parents = create(
        tx,
        "test_rel.partner",
        &[
            &[
                ("name", Value::str("P1")),
                ("country_id", Value::Int(be)),
                ("category_id", Value::list([Value::Int(vip)])),
            ],
            &[("name", Value::str("P2"))],
            &[
                ("name", Value::str("P3")),
                ("country_id", Value::Int(fr)),
                (
                    "category_id",
                    Value::list([Value::Int(vip), Value::Int(other)]),
                ),
            ],
        ],
    )
    .await;
    let (p1, p2, p3) = (id(&parents, 0), id(&parents, 1), id(&parents, 2));
    let children = create(
        tx,
        "test_rel.partner",
        &[
            &[("name", Value::str("c1")), ("parent_id", Value::Int(p1))],
            &[
                ("name", Value::str("kid")),
                ("parent_id", Value::Int(p1)),
                ("category_id", Value::list([Value::Int(other)])),
            ],
            &[("name", Value::str("c3")), ("parent_id", Value::Int(p2))],
            &[
                ("name", Value::str("c4")),
                ("parent_id", Value::Int(p3)),
                ("country_id", Value::Int(be)),
            ],
            &[("name", Value::str("c5"))],
        ],
    )
    .await;
    tx.flush().await.unwrap();
    let all = parents.union(&children).unwrap();
    F1 {
        countries,
        categories,
        parents,
        children,
        all,
    }
}

async fn names(tx: &mut Transaction, records: &Records) -> Vec<Value> {
    records.values(tx, "name").await.unwrap()
}

#[tokio::test]
async fn spec_rset_029_filtered_through_relational_fields() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = f1(&mut tx).await;
    let all = &f.all;
    let with_country = all.filtered_path(&mut tx, "country_id").await.unwrap();
    assert_eq!(
        with_country.ids(),
        [id(&f.parents, 0), id(&f.parents, 2), id(&f.children, 3)]
    );
    assert!(with_country.len() < all.len());
    assert_eq!(with_country.prefetch_ids(), all.prefetch_ids());
    let by_values = all
        .filtered_values(&mut tx, "country_id", Value::is_truthy)
        .await
        .unwrap();
    assert_eq!(by_values, with_country);
    // `parent_id.country_id`: children of P1 and P3
    let through = all
        .filtered_path(&mut tx, "parent_id.country_id")
        .await
        .unwrap();
    assert_eq!(
        through.ids(),
        [id(&f.children, 0), id(&f.children, 1), id(&f.children, 3)]
    );
    assert_eq!(through.prefetch_ids(), all.prefetch_ids());
    // the same records as checking each record's parent
    let mut expected = Vec::new();
    for rec in all.iter() {
        let parent = rec.related(&mut tx, "parent_id").await.unwrap();
        if !parent.is_empty() && parent.value(&mut tx, "country_id").await.unwrap() != Value::False
        {
            expected.push(rec.ids()[0]);
        }
    }
    assert_eq!(through.ids(), expected);
    // a longer path, through an x2many
    let deep = all
        .filtered_path(&mut tx, "child_ids.country_id.code")
        .await
        .unwrap();
    assert_eq!(deep.ids(), [id(&f.parents, 2)]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// so their `TypeError` cannot occur)
#[tokio::test]
async fn spec_rset_030_filtered_argument_forms() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = f1(&mut tx).await;
    let partners = tx.env().records("test_rel.partner").unwrap();
    let pq = create(
        &mut tx,
        "test_rel.partner",
        &[
            &[("name", Value::str("p")), ("email", Value::str("p@x"))],
            &[("name", Value::str("q"))],
        ],
    )
    .await;
    let (p, q) = (
        pq.browse_ids([pq.rec_ids()[0]]),
        pq.browse_ids([pq.rec_ids()[1]]),
    );
    let pqp = p.union(&q).unwrap().concat(&p).unwrap();
    let with_email = pqp.filtered_path(&mut tx, "email").await.unwrap();
    assert_eq!(
        with_email.ids(),
        [p.ids()[0], p.ids()[0]],
        "duplicates kept"
    );
    let same = pqp.filtered_path(&mut tx, "").await.unwrap();
    assert_eq!(same.rec_ids(), pqp.rec_ids());
    let by_domain = pqp
        .filtered_domain(&mut tx, &dom(json!([["email", "!=", false]])))
        .await
        .unwrap();
    assert_eq!(by_domain, with_email);
    let err = pqp.filtered_path(&mut tx, "nope").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    // p's children, one of them named
    create(
        &mut tx,
        "test_rel.partner",
        &[
            &[("parent_id", Value::Int(p.ids()[0]))],
            &[
                ("name", Value::str("named child")),
                ("parent_id", Value::Int(p.ids()[0])),
            ],
        ],
    )
    .await;
    let named = p
        .union(&q)
        .unwrap()
        .filtered_path(&mut tx, "child_ids.name")
        .await
        .unwrap();
    assert_eq!(named, p);
    let _ = (partners, f);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_033_mapped_on_relational_fields_and_paths() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = f1(&mut tx).await;
    let partners = &f.children;
    // the union of each partner's parent, built one by one
    let mut parents = partners
        .browse_ids([])
        .related(&mut tx, "parent_id")
        .await
        .unwrap();
    for rec in partners.iter() {
        let parent = rec.related(&mut tx, "parent_id").await.unwrap();
        parents = parents.union(&parent).unwrap();
    }
    assert!(parents.len() > 1);
    let related = partners.related(&mut tx, "parent_id").await.unwrap();
    assert_eq!(related, parents);
    assert_eq!(
        related.ids(),
        [id(&f.parents, 0), id(&f.parents, 1), id(&f.parents, 2)],
        "no duplicates, first-occurrence order"
    );
    assert_eq!(
        partners.mapped_path(&mut tx, "parent_id").await.unwrap(),
        Mapped::Records(parents.clone())
    );
    let parent_names = names(&mut tx, &parents).await;
    assert_eq!(
        partners
            .mapped_path(&mut tx, "parent_id.name")
            .await
            .unwrap(),
        Mapped::Values(parent_names.clone()),
        "one entry per distinct parent"
    );
    assert_eq!(parents.values(&mut tx, "name").await.unwrap(), parent_names);
    let empty = partners.browse_ids([]);
    match empty.mapped_path(&mut tx, "parent_id").await.unwrap() {
        Mapped::Records(r) => {
            assert!(r.is_empty());
            assert_eq!(r.spec().name, "test_rel.partner");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        empty.mapped_path(&mut tx, "parent_id.name").await.unwrap(),
        Mapped::Values(Vec::new())
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// records and plain values cannot be written in the typed API)
#[tokio::test]
async fn spec_rset_035_mapped_edge_cases() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = f1(&mut tx).await;
    let rs = &f.parents;
    assert_eq!(
        rs.mapped_path(&mut tx, "").await.unwrap(),
        Mapped::Records(rs.clone())
    );
    for (path, unknown) in [("nope", "'nope'"), ("parent_id.nope", "'nope'")] {
        let err = rs.mapped_path(&mut tx, path).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{path}");
        assert!(err.message().contains(unknown), "{err:?}");
    }
    let err = rs.mapped_path(&mut tx, "name.nope").await.unwrap_err();
    assert!(err.message().contains("is not a path"), "{err:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_036_relational_mapped_in_a_loop_is_batched() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let line = |v: i64| -> Value {
        Value::list([
            Value::Int(0),
            Value::Int(0),
            Value::Map(std::sync::Arc::new(support::vmap(&[(
                "value",
                Value::Int(v),
            )]))),
        ])
    };
    let recs = create(
        &mut tx,
        "test_rel.base",
        &[
            &[
                ("name", Value::str("a")),
                ("line_ids", Value::list([line(1), line(2)])),
            ],
            &[
                ("name", Value::str("b")),
                ("line_ids", Value::list([line(3)])),
            ],
            &[
                ("name", Value::str("c")),
                ("line_ids", Value::list([line(4), line(5)])),
            ],
        ],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    let mut values = Vec::new();
    assert_query_count!(tx, >= 1, <= 2, {
        for rec in recs.iter() {
            let lines = rec.related(&mut tx, "line_ids").await.unwrap();
            values.extend(lines.values(&mut tx, "value").await.unwrap());
        }
    });
    assert_eq!(values, (1..=5).map(Value::Int).collect::<Vec<_>>());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_045_sorted_keeps_the_prefetch_set() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("A"))], &[("name", Value::str("B"))]],
    )
    .await;
    for p in partners.ids() {
        create(
            &mut tx,
            "test_rel.partner",
            &[
                &[("name", Value::str("z")), ("parent_id", Value::Int(p))],
                &[("name", Value::str("a")), ("parent_id", Value::Int(p))],
            ],
        )
        .await;
    }
    // the children are known; their names are not
    partners.related(&mut tx, "child_ids").await.unwrap();
    tx.invalidate_model("test_rel.partner", Some(&["name"]))
        .await
        .unwrap();
    let mut seen = Vec::new();
    assert_query_count!(tx, >= 1, <= 1, {
        for partner in partners.iter() {
            let children = partner.related(&mut tx, "child_ids").await.unwrap();
            let sorted = children.sorted(&mut tx, Some("id"), false).await.unwrap();
            seen.extend(sorted.values(&mut tx, "name").await.unwrap());
        }
    });
    assert_eq!(
        seen,
        ["z", "a", "z", "a"].map(Value::str).to_vec(),
        "children by id"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_046_sorted_order_spec_errors() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(
        &mut tx,
        "test_rel.partner",
        &[&[("name", Value::str("b"))], &[("name", Value::str("a"))]],
    )
    .await;
    for (order, message) in [
        ("name DESCENDING", "Invalid order 'name DESCENDING' to sort"),
        ("name,", "Invalid order 'name,' to sort"),
        (
            "child_ids",
            "Invalid order on relational field 'child_ids' to sort",
        ),
        (
            "parent_id.name",
            "Invalid order on relational field 'parent_id.name' to sort",
        ),
    ] {
        let err = rs.sorted(&mut tx, Some(order), false).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{order}");
        assert_eq!(err.message(), message);
    }
    let err = rs.sorted(&mut tx, Some("nope"), false).await.unwrap_err();
    assert!(err.message().contains("'nope'"), "{err:?}");
    let by_month = rs
        .sorted(&mut tx, Some("create_date:month, name"), false)
        .await
        .unwrap();
    assert_eq!(
        names(&mut tx, &by_month).await,
        [Value::str("a"), Value::str("b")]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Rust, where every grouping key type implements `Hash`)
#[tokio::test]
async fn spec_rset_049_grouped_key_forms() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = f1(&mut tx).await;
    let all = &f.all;
    let groups = all.grouped_records(&mut tx, "country_id").await.unwrap();
    let keys: Vec<Vec<i64>> = groups.iter().map(|(k, _)| k.ids()).collect();
    let (be, fr) = (id(&f.countries, 0), id(&f.countries, 1));
    assert_eq!(keys, [vec![be], vec![], vec![fr]]);
    for (key, _) in &groups {
        assert_eq!(key.spec().name, "test_rel.country");
        assert!(key.len() <= 1);
    }
    // lookup by an equal recordset
    let belgium = f.countries.browse_ids([f.countries.rec_ids()[0]]);
    let (_, members) = groups.iter().find(|(k, _)| *k == belgium).unwrap();
    assert_eq!(members.ids(), [id(&f.parents, 0), id(&f.children, 3)]);
    assert_eq!(members.prefetch_ids(), all.prefetch_ids());
    // p + q + p in the same country, grouped by a key computed from loaded data
    let p = f.parents.browse_ids([f.parents.rec_ids()[0]]);
    let q = f.children.browse_ids([f.children.rec_ids()[3]]);
    let pqp = p.union(&q).unwrap().concat(&p).unwrap();
    let country_names = pqp.mapped_path(&mut tx, "country_id.name").await.unwrap();
    assert_eq!(country_names, Mapped::Values(vec![Value::str("Belgium")]));
    let groups = pqp.grouped_records(&mut tx, "country_id").await.unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].1.ids(), pqp.ids(), "duplicates kept");
    // a dotted name is not a field
    let err = all.grouped(&mut tx, "country_id.name").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    // x2many keys compare as sets
    let by_categories = f
        .parents
        .grouped_records(&mut tx, "category_id")
        .await
        .unwrap();
    assert_eq!(by_categories.len(), 3);
    assert_eq!(by_categories[1].0.ids(), Vec::<i64>::new());
    let _ = f.categories;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Every relational domain kind: `search(D)` equals `search([]).filtered_domain(D)`, and `D` / `!D`
/// partition the records in memory as in SQL.
async fn assert_filtered_matches_search(
    tx: &mut Transaction,
    all: &Records,
    domains: &[erp_sdk::serde_json::Value],
) {
    let m = all.browse_ids([]);
    let opts = SearchOptions::new().order("id").active_test(false);
    let mut ids = all.ids();
    ids.sort_unstable();
    // in id order, like `search([], order='id')`
    let all = &m.browse(ids.iter().copied()).unwrap();
    for json in domains {
        let d = dom(json.clone());
        let searched = m.search_with(tx, &d, &opts).await.unwrap();
        let filtered = all.filtered_domain(tx, &d).await.unwrap();
        assert_eq!(filtered.ids(), searched.ids(), "{json}");
        let rest = all.filtered_domain(tx, &!&d).await.unwrap();
        let negated = m.search_with(tx, &!&d, &opts).await.unwrap();
        assert_eq!(rest.ids(), negated.ids(), "not {json}");
        let mut union = [filtered.ids(), rest.ids()].concat();
        union.sort_unstable();
        assert_eq!(union, ids, "partition of {json}");
    }
}

/// hierarchy operators; the scalar domains are checked in `rset_ops.rs`)
#[tokio::test]
async fn spec_dom_063_filtered_domain_matches_search_on_relational_domains() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let f = f1(&mut tx).await;
    let (be, vip) = (id(&f.countries, 0), id(&f.categories, 0));
    let (p1, c4) = (id(&f.parents, 0), id(&f.children, 3));
    let domains = [
        json!([["country_id", "=", be]]),
        json!([["country_id", "=", false]]),
        json!([["country_id", "in", [be, false]]]),
        json!([["country_id.code", "=", "BE"]]),
        json!([["country_id.code", "!=", "BE"]]),
        json!([["country_id.name", "ilike", "an"]]),
        json!([["parent_id.name", "=", "P1"]]),
        json!([["parent_id.country_id.code", "=", "FR"]]),
        json!(["!", ["parent_id.name", "=", "P1"]]),
        json!([["child_ids.name", "=", "kid"]]),
        json!([["child_ids", "=", false]]),
        json!([["child_ids", "!=", false]]),
        json!([["child_ids", "in", [c4]]]),
        json!([["child_ids", "not in", [c4]]]),
        json!([["child_ids", "any", [["country_id", "!=", false]]]]),
        json!([["child_ids", "not any", [["name", "=", "kid"]]]]),
        json!([["category_id", "in", [vip]]]),
        json!([["category_id", "not in", [vip]]]),
        json!([["category_id.name", "ilike", "vi"]]),
        json!([["category_id", "=", false]]),
        json!([
            "|",
            ["country_id.code", "=", "FR"],
            ["parent_id.country_id", "!=", false]
        ]),
        json!([["parent_id", "child_of", [p1]]]),
        json!([["id", "child_of", [p1]]]),
        json!([["id", "parent_of", [c4]]]),
        json!([["parent_id", "any", [["category_id.name", "=", "other"]]]]),
    ];
    assert_filtered_matches_search(&mut tx, &f.all, &domains).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Partners named "hidden" are invisible to users.
struct HideNamed;

impl erp_testkit::TestRules for HideNamed {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> Result<()> {
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        _op: Operation,
    ) -> Result<Domain> {
        Ok(match model.name {
            "test_rel.partner" => Domain::condition("name", Operator::Ne, "hidden").unwrap(),
            _ => Domain::TRUE,
        })
    }
}

#[tokio::test]
async fn spec_dom_064_filtered_domain_any_and_record_rules() {
    let h = Harness::builder([&base_users::ADDON, &test_rel::ADDON])
        .rules(HideNamed)
        .build()
        .await
        .unwrap();
    let mut su = h.begin().await.unwrap();
    let g = create(&mut su, "test_rel.partner", &[&[("name", Value::str("G"))]]).await;
    let parents = create(
        &mut su,
        "test_rel.partner",
        &[
            &[
                ("name", Value::str("P")),
                ("parent_id", Value::Int(g.ids()[0])),
            ],
            &[("name", Value::str("Q"))],
            &[("name", Value::str("R"))],
        ],
    )
    .await;
    let p = parents.ids()[0];
    create(
        &mut su,
        "test_rel.partner",
        &[
            &[("name", Value::str("hidden")), ("parent_id", Value::Int(p))],
            &[
                ("name", Value::str("visible")),
                ("parent_id", Value::Int(p)),
            ],
            &[
                ("name", Value::str("other")),
                ("parent_id", Value::Int(parents.ids()[1])),
            ],
        ],
    )
    .await;
    su.commit().await.unwrap();

    let mut tx = h.begin_as(2).await.unwrap();
    let partners = tx.env().records("test_rel.partner").unwrap();
    let p_set = partners.browse([p]).unwrap();
    let hidden = json!([["name", "=", "hidden"]]);
    let any_bang =
        Domain::condition("child_ids", Operator::AnyBypass, dom(hidden.clone())).unwrap();
    let any = Domain::condition("child_ids", Operator::Any, dom(hidden.clone())).unwrap();
    assert_eq!(
        p_set
            .filtered_domain(&mut tx, &any_bang)
            .await
            .unwrap()
            .ids(),
        [p],
        "any! sees the hidden child"
    );
    assert!(
        p_set
            .filtered_domain(&mut tx, &any)
            .await
            .unwrap()
            .is_empty(),
        "any applies the rule"
    );
    // nested: a plain `any` under `any!` applies the rule at its level again
    let g_set = partners.browse(g.ids()).unwrap();
    let nested = |outer: Operator, inner: Operator| {
        let inner = Domain::condition("child_ids", inner, dom(hidden.clone())).unwrap();
        Domain::condition("child_ids", outer, inner).unwrap()
    };
    for (outer, inner, found) in [
        (Operator::AnyBypass, Operator::Any, false),
        (Operator::AnyBypass, Operator::AnyBypass, true),
        (Operator::Any, Operator::AnyBypass, true),
        (Operator::Any, Operator::Any, false),
    ] {
        let kept = g_set
            .filtered_domain(&mut tx, &nested(outer, inner))
            .await
            .unwrap();
        assert_eq!(!kept.is_empty(), found, "{outer:?} / {inner:?}");
    }
    // the sub-domain is evaluated once for the targets of all records: the cost does not grow
    // with the number of parents
    let three = partners.browse(parents.ids()).unwrap();
    tx.invalidate_all().await.unwrap();
    let visible = json!([["name", "=", "visible"]]);
    let d = Domain::condition("child_ids", Operator::Any, dom(visible)).unwrap();
    let kept = assert_query_count!(tx, <= 3, {
        three.filtered_domain(&mut tx, &d).await.unwrap()
    });
    assert_eq!(kept.ids(), [p]);
    drop(tx);
    h.cleanup().await.unwrap();
}
