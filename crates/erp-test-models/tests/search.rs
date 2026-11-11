//! DOM search cases of slice S2: search results, SQL shapes (structure,/§5), active
//! test, order, pagination, counts, name search, read, errors and query budgets.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use erp_sdk::serde_json::json;
use erp_sdk::{
    DateTime, Domain, ErrorKind, NameSearch, Operator, OptimizationLevel, Records, SearchOptions,
    Transaction, Value,
};
use erp_testkit::assert_query_count;
use support::{dom, map};

type Json = erp_sdk::serde_json::Value;

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let empty = tx.env().records(model).unwrap();
    let values: Vec<_> = rows.iter().map(|r| map(r)).collect();
    let created = empty.create_values(tx, &values).await.unwrap();
    tx.flush().await.unwrap();
    created
}

fn s(v: &str) -> Value {
    Value::str(v)
}

async fn search_ids(tx: &mut Transaction, model: &str, domain: &Domain) -> Vec<i64> {
    let m = tx.env().records(model).unwrap();
    m.search_with(tx, domain, &SearchOptions::new().order("id"))
        .await
        .unwrap()
        .ids()
}

/// The single statement a block captured.
fn last_statement(tx: &Transaction) -> String {
    tx.captured().last().cloned().unwrap_or_default()
}

#[tokio::test]
async fn spec_dom_007_complement_covers_every_record() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let m = "test_search.mixed";
    let r1 = create(&mut tx, m, &[&[]]).await;
    let r2 = create(
        &mut tx,
        m,
        &[&[
            ("number", Value::Int(0)),
            ("x", Value::Float(0.0)),
            ("name", s("")),
        ]],
    )
    .await;
    let parent = r1.ids()[0];
    let r3 = create(
        &mut tx,
        m,
        &[&[
            ("number", Value::Int(5)),
            ("x", Value::Float(3.5)),
            ("name", s("b")),
            ("moment", s("2024-01-05 10:00:00")),
            ("parent_id", Value::Int(parent)),
        ]],
    )
    .await;
    let _r4 = create(
        &mut tx,
        m,
        &[&[
            ("number", Value::Int(-5)),
            ("x", Value::Float(-3.0)),
            ("name", s("a")),
            ("moment", s("2023-06-01 08:00:00")),
            ("parent_id", Value::Int(r3.ids()[0])),
        ]],
    )
    .await;
    let all = search_ids(&mut tx, m, &Domain::TRUE).await;
    assert_eq!(all.len(), 4);
    let cases = [
        json!([["number", ">", 2]]),
        json!([["number", ">", -2]]),
        json!([["number", "<", 1]]),
        json!([["number", "<=", 1]]),
        json!([["x", ">", 2]]),
        json!([["x", "<", 1]]),
        json!([["name", "<", ""]]),
        json!([["name", ">", "a"]]),
        json!([["moment", ">", "2024-01-01 00:00:00"]]),
        json!([["moment", "<=", "2024-01-01 00:00:00"]]),
        json!([["parent_id", "<", r2.ids()[0]]]),
    ];
    for case in cases {
        let d = dom(case.clone());
        let yes = search_ids(&mut tx, m, &d).await;
        let no = search_ids(&mut tx, m, &!&d).await;
        assert!(
            yes.iter().all(|id| !no.contains(id)),
            "{case}: {yes:?} {no:?}"
        );
        let mut union: Vec<i64> = yes.iter().chain(&no).copied().collect();
        union.sort_unstable();
        assert_eq!(union, all, "{case}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_036_boolean_logic_sql_shape() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    tx.capture_statements(true);
    partners
        .search(
            &mut tx,
            &dom(json!([
                "&",
                ["name", "like", "foo"],
                "|",
                ["country_id", "=", 1],
                "!",
                ["email", "=", "john@example.com"]
            ])),
        )
        .await
        .unwrap();
    let t = "\"test_search_partner\"";
    support::assert_sql(
        &last_statement(&tx),
        &format!(
            "SELECT {t}.\"id\" FROM {t} WHERE ({t}.\"active\" IS TRUE AND {t}.\"name\" LIKE \
             $1::text AND ({t}.\"country_id\" = ANY($2::int8[]) OR ({t}.\"email\" <> \
             ALL($3::text[]) OR {t}.\"email\" IS NULL))) ORDER BY {t}.\"id\""
        ),
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// the context or the search option; the implicit filter is the first predicate.
#[tokio::test]
async fn spec_dom_048_active_test() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = "test_search.partner";
    let a = create(&mut tx, p, &[&[("name", s("a"))]]).await.ids()[0];
    let b = create(
        &mut tx,
        p,
        &[&[("name", s("b")), ("active", Value::Bool(false))]],
    )
    .await
    .ids()[0];
    assert_eq!(search_ids(&mut tx, p, &Domain::TRUE).await, [a]);
    assert_eq!(
        search_ids(&mut tx, p, &dom(json!([["active", "=", false]]))).await,
        [b]
    );
    assert_eq!(
        search_ids(
            &mut tx,
            p,
            &dom(json!(["|", ["active", "=", true], ["active", "=", false]]))
        )
        .await,
        [a, b]
    );
    let no_test = tx
        .env()
        .with_context([("active_test", Value::Bool(false))])
        .records(p)
        .unwrap();
    assert_eq!(
        no_test.search(&mut tx, &Domain::TRUE).await.unwrap().ids(),
        [a, b]
    );
    let partners = tx.env().records(p).unwrap();
    let opts = SearchOptions::new().active_test(false);
    assert_eq!(
        partners
            .search_with(&mut tx, &Domain::TRUE, &opts)
            .await
            .unwrap()
            .ids(),
        [a, b]
    );
    tx.capture_statements(true);
    partners.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert!(
        last_statement(&tx).contains("WHERE \"test_search_partner\".\"active\" IS TRUE ORDER BY"),
        "{}",
        last_statement(&tx)
    );
    // x_active alone drives the filter; with both fields, `active` does
    let x = "test_search.x_active";
    let xa = create(&mut tx, x, &[&[("name", s("on"))]]).await.ids()[0];
    create(
        &mut tx,
        x,
        &[&[("name", s("off")), ("x_active", Value::Bool(false))]],
    )
    .await;
    assert_eq!(search_ids(&mut tx, x, &Domain::TRUE).await, [xa]);
    let both = "test_search.both_active";
    let ba = create(
        &mut tx,
        both,
        &[&[("name", s("1")), ("x_active", Value::Bool(false))]],
    )
    .await
    .ids()[0];
    create(
        &mut tx,
        both,
        &[&[("name", s("2")), ("active", Value::Bool(false))]],
    )
    .await;
    assert_eq!(search_ids(&mut tx, both, &Domain::TRUE).await, [ba]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_051_limit_and_offset() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = "test_search.partner";
    let rows: Vec<Vec<(&str, Value)>> = (0..12).map(|i| vec![("number", Value::Int(i))]).collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, p, &refs).await;
    let partners = tx.env().records(p).unwrap();
    let all = partners.search(&mut tx, &Domain::TRUE).await.unwrap().ids();
    let run = async |tx: &mut Transaction, opts: SearchOptions| {
        partners
            .search_with(tx, &Domain::TRUE, &opts)
            .await
            .unwrap()
            .ids()
    };
    assert_eq!(run(&mut tx, SearchOptions::new().offset(5)).await, all[5..]);
    assert_eq!(run(&mut tx, SearchOptions::new().limit(5)).await, all[..5]);
    assert_eq!(
        run(&mut tx, SearchOptions::new().offset(3).limit(7)).await,
        all[3..10]
    );
    assert_eq!(run(&mut tx, SearchOptions::new()).await, all);
    tx.capture_statements(true);
    run(&mut tx, SearchOptions::new().limit(5)).await;
    let sql = last_statement(&tx);
    assert!(
        sql.ends_with("LIMIT $1::int8") && !sql.contains("OFFSET"),
        "{sql}"
    );
    run(&mut tx, SearchOptions::new().offset(3).limit(7)).await;
    assert!(
        last_statement(&tx).ends_with("LIMIT $1::int8 OFFSET $2::int8"),
        "{}",
        last_statement(&tx)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_052_search_count() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = "test_search.partner";
    create(
        &mut tx,
        p,
        &[
            &[("name", s("foo1"))],
            &[("name", s("foo2"))],
            &[("name", s("bar"))],
            &[("name", s("x"))],
        ],
    )
    .await;
    let partners = tx.env().records(p).unwrap();
    let like = dom(json!([["name", "like", "foo"]]));
    tx.capture_statements(true);
    assert_eq!(
        partners.search_count(&mut tx, &like, None).await.unwrap(),
        2
    );
    let t = "\"test_search_partner\"";
    support::assert_sql(
        &last_statement(&tx),
        &format!(
            "SELECT COUNT(*) FROM {t} WHERE ({t}.\"active\" IS TRUE AND {t}.\"name\" LIKE $1::text)"
        ),
    );
    assert_eq!(
        partners
            .search_count(&mut tx, &like, Some(1))
            .await
            .unwrap(),
        1
    );
    support::assert_sql(
        &last_statement(&tx),
        &format!(
            "SELECT COUNT(*) FROM (SELECT FROM {t} WHERE ({t}.\"active\" IS TRUE AND {t}.\"name\" \
             LIKE $1::text) LIMIT $2::int8) AS \"__count\""
        ),
    );
    assert_eq!(
        partners
            .search_count(&mut tx, &Domain::TRUE, Some(3))
            .await
            .unwrap(),
        3
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// the `UserError` for invalid specifications.
#[tokio::test]
async fn spec_dom_053_order_plain_fields() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = "test_search.partner";
    let c = create(&mut tx, p, &[&[("name", s("C")), ("email", s("c@x"))]])
        .await
        .ids()[0];
    let d = create(
        &mut tx,
        p,
        &[&[("name", s("D")), ("active", Value::Bool(false))]],
    )
    .await
    .ids()[0];
    let a = create(&mut tx, p, &[&[("name", s("A")), ("email", s("a@x"))]])
        .await
        .ids()[0];
    let b = create(&mut tx, p, &[&[("name", s("B"))]]).await.ids()[0];
    let ab = create(&mut tx, p, &[&[("name", s("AB"))]]).await.ids()[0];
    let e = create(
        &mut tx,
        p,
        &[&[("name", s("E")), ("active", Value::Bool(false))]],
    )
    .await
    .ids()[0];
    let partners = tx.env().records(p).unwrap();
    let ordered = async |tx: &mut Transaction, domain: &Domain, order: &str| {
        partners
            .search_with(tx, domain, &SearchOptions::new().order(order))
            .await
            .unwrap()
            .ids()
    };
    assert_eq!(
        ordered(&mut tx, &Domain::TRUE, "name asc").await,
        [a, ab, b, c]
    );
    assert_eq!(
        ordered(&mut tx, &Domain::TRUE, "name desc").await,
        [c, b, ab, a]
    );
    assert_eq!(
        ordered(&mut tx, &Domain::TRUE, "id asc").await,
        [c, a, b, ab]
    );
    assert_eq!(
        ordered(&mut tx, &Domain::TRUE, "id desc").await,
        [ab, b, a, c]
    );
    let both = dom(json!([["active", "in", [true, false]]]));
    assert_eq!(
        ordered(&mut tx, &both, "active asc, id asc").await,
        [d, e, c, a, b, ab]
    );
    let abc = dom(json!([["id", "in", [a, b, c]]]));
    assert_eq!(ordered(&mut tx, &abc, "email").await, [a, c, b]);
    assert_eq!(ordered(&mut tx, &abc, "email desc").await, [b, c, a]);
    assert_eq!(
        ordered(&mut tx, &abc, "email asc nulls first").await,
        [b, a, c]
    );
    assert_eq!(
        ordered(&mut tx, &abc, "email desc nulls last").await,
        [c, a, b]
    );
    let err = partners
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("name; drop"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::UserError);
    assert!(
        err.message()
            .starts_with("Invalid \"order\" specified (name; drop).")
    );
    drop(tx);
    h.cleanup().await.unwrap();

    // `_order` patched to 'name ASC, id DESC' (a per-test model variant)
    static VARIANT: erp_registry::AddonDescriptor = erp_registry::AddonDescriptor {
        name: "test_search_order",
        depends: &["test_search"],
        exact_only: false,
    };
    let h = erp_testkit::Harness::builder([&erp_test_models::test_search::ADDON, &VARIANT])
        .with_decls([erp_registry::ModelDecl::extension(
            "test_search_order",
            "test_search.partner",
            &["test_search.partner"],
        )
        .order("name ASC, id DESC")])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records(p).unwrap();
    tx.capture_statements(true);
    partners.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert!(
        last_statement(&tx).ends_with(
            "ORDER BY \"test_search_partner\".\"name\" ASC, \"test_search_partner\".\"id\" DESC"
        ),
        "{}",
        last_statement(&tx)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_056_boolean_and_selection_ordering() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let m = "test_search.mixed";
    let t = create(
        &mut tx,
        m,
        &[&[("flag", Value::Bool(true)), ("state", s("a"))]],
    )
    .await
    .ids()[0];
    let f = create(
        &mut tx,
        m,
        &[&[("flag", Value::Bool(false)), ("state", s("b"))]],
    )
    .await
    .ids()[0];
    let n = create(&mut tx, m, &[&[("state", s("c"))]]).await.ids()[0];
    let records = tx.env().records(m).unwrap();
    let ordered = async |tx: &mut Transaction, order: &str| {
        records
            .search_with(tx, &Domain::TRUE, &SearchOptions::new().order(order))
            .await
            .unwrap()
            .ids()
    };
    assert_eq!(ordered(&mut tx, "flag, id").await, [f, n, t]);
    assert_eq!(ordered(&mut tx, "state").await, [n, f, t], "c, b, a");
    assert_eq!(ordered(&mut tx, "state desc").await, [t, f, n], "a, b, c");
    tx.capture_statements(true);
    ordered(&mut tx, "flag").await;
    assert!(
        last_statement(&tx).ends_with(
            "ORDER BY COALESCE(\"test_search_mixed\".\"flag\", FALSE), \"test_search_mixed\".\"id\""
        ),
        "{}",
        last_statement(&tx)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// still checked.
#[tokio::test]
async fn spec_dom_057_false_domains_run_no_query() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    let empty_in = dom(json!([["id", "in", []]]));
    let not_ilike = dom(json!([["name", "not ilike", ""]]));
    assert_query_count!(tx, <= 0, {
        assert!(partners.search(&mut tx, &empty_in).await.unwrap().is_empty());
        assert_eq!(partners.search_count(&mut tx, &empty_in, None).await.unwrap(), 0);
        assert!(partners.search(&mut tx, &not_ilike).await.unwrap().is_empty());
    });
    let err = partners
        .search_with(
            &mut tx,
            &empty_in,
            &SearchOptions::new().fetch_fields(["nope"]),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Invalid field 'nope' on 'test_search.partner'"
    );
    assert_query_count!(tx, >= 1, <= 1, {
        partners
            .search(&mut tx, &dom(json!([["name", "ilike", ""]])))
            .await
            .unwrap();
        partners.search(&mut tx, &not_ilike).await.unwrap();
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

/// columns are not flushed, those of WHERE/ORDER BY columns are.
#[tokio::test]
async fn spec_dom_058_search_fetch_in_one_query() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let m = "test_search.country";
    let created = create(
        &mut tx,
        m,
        &[
            &[("name", s("Brussels")), ("code", s("BE"))],
            &[("name", s("Paris")), ("code", s("FR"))],
        ],
    )
    .await;
    let (x, y) = (created.ids()[0], created.ids()[1]);
    tx.invalidate_all().await.unwrap();
    let countries = tx.env().records(m).unwrap();
    tx.capture_statements(true);
    let found = assert_query_count!(tx, >= 1, <= 1, {
        countries
            .search_with(
                &mut tx,
                &dom(json!([["id", "=", x]])),
                &SearchOptions::new().order("id").fetch_prefetchable(),
            )
            .await
            .unwrap()
    });
    assert_eq!(found.ids(), [x]);
    let t = "\"test_search_country\"";
    let sql = last_statement(&tx);
    for col in [
        "name",
        "code",
        "create_uid",
        "create_date",
        "write_uid",
        "write_date",
    ] {
        assert!(sql.contains(&format!("{t}.\"{col}\"")), "{col} in {sql}");
    }
    assert!(tx.is_cached(&found, "code").unwrap());

    // a pending write on a fetched column: no UPDATE
    let other = countries.browse([y]).unwrap();
    other.set_value(&mut tx, "name", s("Pariss")).await.unwrap();
    let before = tx.captured().len();
    assert_query_count!(tx, >= 1, <= 1, {
        countries
            .search_with(
                &mut tx,
                &dom(json!([["code", "=", "BE"]])),
                &SearchOptions::new().fetch_fields(["name"]),
            )
            .await
            .unwrap();
    });
    let statements = &tx.captured()[before..];
    assert!(
        statements[0].starts_with(&format!("SELECT {t}.\"id\", {t}.\"name\" FROM")),
        "{statements:?}"
    );
    assert!(tx.is_dirty(&other, "name").unwrap());

    // the same column in WHERE: UPDATE then SELECT
    let before = tx.captured().len();
    countries
        .search_with(
            &mut tx,
            &dom(json!([["name", "like", "Brussel"]])),
            &SearchOptions::new().fetch_fields(["name"]),
        )
        .await
        .unwrap();
    let statements = &tx.captured()[before..];
    assert_eq!(statements.len(), 2, "{statements:?}");
    assert!(statements[0].starts_with("UPDATE"), "{statements:?}");
    assert!(statements[1].starts_with("SELECT"), "{statements:?}");

    // in ORDER BY: UPDATE then SELECT
    other.set_value(&mut tx, "name", s("Paris")).await.unwrap();
    let before = tx.captured().len();
    countries
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("name").fetch_fields(["name"]),
        )
        .await
        .unwrap();
    let statements = &tx.captured()[before..];
    assert_eq!(statements.len(), 2, "{statements:?}");
    assert!(statements[0].starts_with("UPDATE"), "{statements:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// unknown fields. (Many2one `(id, name)` pairs arrive with relational fields, slice S3.)
#[tokio::test]
async fn spec_dom_061_read_results_and_deleted_records() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let m = "test_search.country";
    let created = create(&mut tx, m, &[&[("name", s("X"))], &[("name", s("Y"))]]).await;
    let (c1, c2) = (created.ids()[0], created.ids()[1]);
    tx.execute(
        &format!("DELETE FROM test_search_country WHERE id = {c1}"),
        &[],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let countries = tx.env().records(m).unwrap();
    let both = countries.browse([c1, c2]).unwrap();
    let rows = both.read(&mut tx, &["name"]).await.unwrap();
    assert_eq!(rows, [map(&[("id", Value::Int(c2)), ("name", s("Y"))])]);
    let gone = countries.browse([c1]).unwrap();
    assert!(gone.read(&mut tx, &["name"]).await.unwrap().is_empty());
    let all = countries
        .browse([c2])
        .unwrap()
        .read(&mut tx, &[])
        .await
        .unwrap();
    let keys: Vec<&str> = all[0].keys().map(String::as_str).collect();
    for key in [
        "id",
        "name",
        "code",
        "display_name",
        "create_uid",
        "write_date",
    ] {
        assert!(keys.contains(&key), "{key} in {keys:?}");
    }
    let err = both.read(&mut tx, &["nope"]).await.unwrap_err();
    assert_eq!(
        err.message(),
        "Invalid field 'nope' on 'test_search.country'"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// rec name (no restriction).
#[tokio::test]
async fn spec_dom_062_name_search() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let m = "test_search.named";
    let created = create(
        &mut tx,
        m,
        &[
            &[("name", s("foo bar")), ("code", s("z1"))],
            &[("name", s("baz")), ("code", s("foo2"))],
            &[("name", s("qux")), ("code", s("q"))],
        ],
    )
    .await;
    let ids = created.ids();
    let named = tx.env().records(m).unwrap();
    tx.capture_statements(true);
    let found = named.name_search(&mut tx, "foo").await.unwrap();
    assert_eq!(
        found,
        [
            (ids[1], Some("baz".into())),
            (ids[0], Some("foo bar".into()))
        ],
        "ordered by code"
    );
    let t = "\"test_search_named\"";
    support::assert_sql(
        &last_statement(&tx),
        &format!(
            "SELECT {t}.\"id\", {t}.\"name\" FROM {t} WHERE ({t}.\"code\" ILIKE $1::text OR \
             {t}.\"name\" ILIKE $2::text) ORDER BY {t}.\"code\", {t}.\"id\" LIMIT $3::int8"
        ),
    );
    let negative = named
        .name_search_with(
            &mut tx,
            "foo",
            &NameSearch::new().operator(Operator::NotILike).limit(None),
        )
        .await
        .unwrap();
    assert_eq!(negative, [(ids[2], Some("qux".into()))]);
    assert!(last_statement(&tx).contains(&format!(
        "(({t}.\"code\" NOT ILIKE $1::text OR {t}.\"code\" IS NULL) AND ({t}.\"name\" NOT \
         ILIKE $2::text OR {t}.\"name\" IS NULL))"
    )));
    // equality converts the value per field and skips what does not convert
    let exact = named
        .name_search_with(&mut tx, "q", &NameSearch::new().operator(Operator::Eq))
        .await
        .unwrap();
    assert_eq!(exact, [(ids[2], Some("qux".into()))]);

    let anonymous = "test_search.anonymous";
    let a = create(&mut tx, anonymous, &[&[("size", Value::Int(1))], &[]]).await;
    let any = tx.env().records(anonymous).unwrap();
    for name in ["", "x"] {
        let found = any.name_search(&mut tx, name).await.unwrap();
        let expected: Vec<(i64, Option<std::sync::Arc<str>>)> = a
            .ids()
            .into_iter()
            .map(|id| (id, Some(format!("test_search.anonymous,{id}").into())))
            .collect();
        assert_eq!(found, expected, "{name:?}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// needs `unaccent` (shape in erp-sql's test).
#[tokio::test]
async fn spec_dom_065_like_semantics() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = "test_search.partner";
    let created = create(
        &mut tx,
        p,
        &[
            &[("name", s("[default] _*%"))],
            &[("name", s("5%"))],
            &[("name", s("XY"))],
            &[("name", s("Hélène"))],
        ],
    )
    .await;
    let ids = created.ids();
    let find = async |tx: &mut Transaction, op: &str, value: &str| {
        search_ids(tx, p, &dom(json!([["name", op, value]]))).await
    };
    assert_eq!(find(&mut tx, "like", "[default]").await, [ids[0]]);
    assert_eq!(find(&mut tx, "like", "\\_*").await, [ids[0]]);
    assert_eq!(find(&mut tx, "=like", "%\\%").await, [ids[0], ids[1]]);
    assert_eq!(find(&mut tx, "ilike", "DEF").await, [ids[0]]);
    assert!(find(&mut tx, "=like", "Hel%").await.is_empty());
    assert!(find(&mut tx, "like", "ele").await.is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
#[allow(
    clippy::approx_constant,
    reason = "3.14 is the value of the reference ERP's fixture"
)]
async fn spec_dom_081_scalar_comparisons() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let m = "test_search.mixed";
    let lo = create(&mut tx, m, &[&[("x", Value::Float(0.01))]])
        .await
        .ids()[0];
    let hi = create(&mut tx, m, &[&[("x", Value::Float(3.14))]])
        .await
        .ids()[0];
    let f = |op: &str, v: f64| dom(json!([["x", op, v]]));
    assert_eq!(search_ids(&mut tx, m, &f("=", 0.01)).await, [lo]);
    assert_eq!(search_ids(&mut tx, m, &f("!=", 0.01)).await, [hi]);
    assert_eq!(search_ids(&mut tx, m, &f(">", 0.01)).await, [hi]);
    assert_eq!(search_ids(&mut tx, m, &f(">=", 0.01)).await, [lo, hi]);
    assert_eq!(search_ids(&mut tx, m, &f("<", 3.14)).await, [lo]);
    assert_eq!(search_ids(&mut tx, m, &f("<=", 3.14)).await, [lo, hi]);

    let p = "test_search.partner";
    let created = create(
        &mut tx,
        p,
        &[
            &[("name", s("p0")), ("number", Value::Int(1))],
            &[
                ("name", s("p1")),
                ("number", Value::Int(10)),
                ("website", s("Ljósálfaheimr")),
            ],
            &[("number", Value::Int(3)), ("website", s("York"))],
            &[("name", s("p3")), ("website", s("Springfield"))],
        ],
    )
    .await;
    let all = created.ids();
    let unknown = all.iter().max().unwrap() + 1000;
    for d in [
        json!([]),
        json!([["name", "not like", "nonexistent"]]),
        json!([["id", "not in", [unknown]]]),
        json!([["id", "not in", []]]),
        json!([["id", "!=", false]]),
    ] {
        assert_eq!(search_ids(&mut tx, p, &dom(d.clone())).await, all, "{d}");
    }
    let id = all[1];
    let others: Vec<i64> = all.iter().copied().filter(|i| *i != id).collect();
    for d in [
        json!([["id", "=", id]]),
        json!([["id", "not in", others]]),
        json!(["!", ["id", "!=", id]]),
        json!(["!", ["id", "in", others]]),
        json!([["id", "in", id]]),
        json!([["id", "in", [id]]]),
        json!([["name", "=", "p1"]]),
        json!([["name", "in", ["p1"]]]),
    ] {
        assert_eq!(search_ids(&mut tx, p, &dom(d.clone())).await, [id], "{d}");
    }
    assert_eq!(
        search_ids(&mut tx, p, &dom(json!([["number", "in", [1, 2, 10, 20]]]))).await,
        [all[0], all[1]]
    );
    assert_eq!(
        search_ids(&mut tx, p, &dom(json!([["website", "!=", "York"]]))).await,
        [all[0], all[1], all[3]],
        "NULL website included"
    );
    assert_eq!(
        search_ids(&mut tx, p, &dom(json!([["website", "not ilike", "field"]]))).await,
        [all[0], all[1], all[2]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_014_019_067_environment_levels() {
    let h = support::search_orm().await;
    let tx = h.begin().await.unwrap();
    let mixed = tx.env().records("test_search.mixed").unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    let opt =
        |r: &Records, d: Json, level| r.optimize_domain(&dom(d), level, None).unwrap().to_string();
    use OptimizationLevel::{Basic, DynamicValues, Full};
    // boolean tautology only at FULL
    let all_flags = json!([["flag", "not in", [true, false]]]);
    assert_eq!(
        opt(&mixed, all_flags.clone(), Basic),
        "[('flag', 'not in', [True, False])]"
    );
    assert_eq!(opt(&mixed, all_flags, Full), "[(0, '=', 1)]");
    let active = json!([["active", "in", [true, false]]]);
    assert_eq!(
        opt(&partners, active.clone(), DynamicValues),
        "[('active', 'in', [True, False])]"
    );
    assert_eq!(opt(&partners, active, Full), "[(1, '=', 1)]");
    // selection `not in` becomes `in` at DYNAMIC_VALUES
    assert_eq!(
        opt(&mixed, json!([["state", "in", ["c"]]]), DynamicValues),
        "[('state', 'in', ['c'])]"
    );
    assert_eq!(
        opt(&mixed, json!([["state", "not in", ["c"]]]), Basic),
        "[('state', 'not in', ['c'])]"
    );
    assert_eq!(
        opt(&mixed, json!([["state", "not in", ["c"]]]), DynamicValues),
        "[('state', 'in', ['b', 'a', False])]"
    );
    assert_eq!(
        opt(
            &mixed,
            json!([["state", "not in", ["c", false]]]),
            DynamicValues
        ),
        "[('state', 'in', ['b', 'a'])]"
    );
    assert_eq!(
        opt(&mixed, json!([["state", "not in", [false]]]), DynamicValues),
        "[('state', 'not in', [False])]"
    );
    // idempotent: optimizing an optimized domain gives an equal domain
    let d = dom(json!([
        "|",
        ["state", "not in", ["c"]],
        ["flag", "=", true]
    ]));
    let once = mixed.optimize_domain(&d, Full, None).unwrap();
    assert_eq!(mixed.optimize_domain(&once, Full, None).unwrap(), once);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// "now" in the user's timezone.
#[tokio::test]
async fn spec_dom_017_relative_dates() {
    let h = support::search_orm().await;
    let tx = h.begin().await.unwrap();
    let mixed = tx.env().records("test_search.mixed").unwrap();
    let now = DateTime::parse_odoo("2024-01-05 13:05:00");
    let opt = |d: Json, level| {
        mixed
            .optimize_domain(&dom(d), level, now)
            .unwrap()
            .to_string()
    };
    use OptimizationLevel::{Basic, Full};
    assert_eq!(
        opt(json!([["date", ">", "today"]]), Basic),
        "[('date', '>', 'today')]"
    );
    assert_eq!(
        opt(json!([["date", ">", "today"]]), Full),
        "[('date', '>', datetime.date(2024, 1, 5))]"
    );
    assert_eq!(
        opt(json!([["date", ">", "+12H"]]), Full),
        "[('date', '>', datetime.date(2024, 1, 6))]"
    );
    assert_eq!(
        opt(json!([["date", "=", "today"]]), Full),
        "[('date', 'in', [datetime.date(2024, 1, 5)])]"
    );
    assert_eq!(
        opt(json!([["moment", ">=", "today"]]), Full),
        "[('moment', '>=', datetime.datetime(2024, 1, 5, 0, 0))]"
    );
    assert_eq!(
        opt(json!([["moment", ">=", "+12H"]]), Full),
        "[('moment', '>=', datetime.datetime(2024, 1, 6, 1, 5))]"
    );
    // a search with a relative date is compiled per call, never memoized
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_031_search_time_errors() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let countries = tx.env().records("test_search.country").unwrap();
    let err = countries
        .search(&mut tx, &dom(json!([["abcdefg", "in", ["foo"]]])))
        .await
        .unwrap_err();
    assert!(err.message().starts_with("Invalid field"), "{err}");
    assert_eq!(err.kind(), ErrorKind::Validation, "external input");
    let err = countries
        .search(&mut tx, &dom(json!([["name.\"Et plouf\"", "in", ["foo"]]])))
        .await
        .unwrap_err();
    assert!(err.message().contains("Et plouf"), "{err}");
    let err = erp_sdk::Domain::from_json(&json!([["create_date", ">>", "foo"]])).unwrap_err();
    assert!(
        err.to_string().starts_with("Invalid operator")
            && err.to_string().ends_with("('create_date', '>>', 'foo')"),
        "{err}"
    );
    let noname = tx.env().records("test_orm.noname").unwrap();
    let err = noname
        .search(&mut tx, &dom(json!([["upper", "=", "X"]])))
        .await
        .unwrap_err();
    assert!(
        err.message()
            .starts_with("Field has no SQL representation because it is not stored in condition"),
        "{err}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// without a join (DOM-086); an unknown field in an order raises before any query (DOM-087).
#[tokio::test]
async fn spec_dom_086_087_many2one_and_unknown_order() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    let err = assert_query_count!(tx, <= 0, {
        partners
            .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order("zzz"))
            .await
            .unwrap_err()
    });
    assert_eq!(
        err.message(),
        "Invalid field 'zzz' on model 'test_search.partner'"
    );
    let countries = create(
        &mut tx,
        "test_search.country",
        &[&[("name", s("B"))], &[("name", s("A"))]],
    )
    .await;
    let (c1, c2) = (countries.ids()[0], countries.ids()[1]);
    let p = "test_search.partner";
    let x = create(&mut tx, p, &[&[("country_id", Value::Int(c2))]])
        .await
        .ids()[0];
    let y = create(&mut tx, p, &[&[("country_id", Value::Int(c1))]])
        .await
        .ids()[0];
    tx.capture_statements(true);
    let found = partners
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("country_id"),
        )
        .await
        .unwrap();
    assert_eq!(found.ids(), [y, x]);
    let sql = last_statement(&tx);
    assert!(
        sql.ends_with(
            "ORDER BY \"test_search_partner\".\"country_id\", \"test_search_partner\".\"id\""
        ) && !sql.contains("JOIN"),
        "{sql}"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_088_search_query_is_lazy() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        "test_search.country",
        &[&[("name", s("a"))], &[("name", s("b"))]],
    )
    .await;
    let countries = tx.env().records("test_search.country").unwrap();
    let mut q = assert_query_count!(tx, <= 0, {
        countries
            .search_query(&mut tx, &Domain::TRUE, &SearchOptions::new())
            .await
            .unwrap()
    });
    assert!(!q.is_empty() && q.known_ids().is_none());
    let ids = assert_query_count!(tx, >= 1, <= 1, { q.ids(&mut tx).await.unwrap() });
    assert!(ids.len() >= 2);
    assert_query_count!(tx, <= 0, {
        assert_eq!(q.ids(&mut tx).await.unwrap(), ids);
        assert_eq!(q.count(&mut tx).await.unwrap(), ids.len() as u64);
    });
    let fresh = countries
        .search_query(&mut tx, &Domain::TRUE, &SearchOptions::new().limit(1))
        .await
        .unwrap();
    tx.capture_statements(true);
    let n = assert_query_count!(tx, >= 1, <= 1, { fresh.count(&mut tx).await.unwrap() });
    assert_eq!(n, 1);
    assert!(last_statement(&tx).starts_with("SELECT COUNT(*) FROM (SELECT FROM"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// bounds).
#[tokio::test]
async fn spec_dom_089_search_fetch_and_name_search_budgets() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = create(
        &mut tx,
        "test_search.partner",
        &[&[("name", s("p1"))], &[("name", s("p2"))]],
    )
    .await;
    let pids = partners.ids();
    let rows: Vec<Vec<(&str, Value)>> = (0..5)
        .map(|i| {
            vec![
                ("name", s(if i == 4 { "blablu" } else { "rec" })),
                ("value", Value::Int(i)),
                ("partner_id", Value::Int(pids[(i % 2) as usize])),
            ]
        })
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, "test_search.perf", &refs).await;
    let base = tx.env().records("test_search.perf").unwrap();
    for _ in 0..2 {
        tx.invalidate_all().await.unwrap();
        let found = base.search(&mut tx, &Domain::TRUE).await.unwrap();
        found.cells(&mut tx, "partner_id").await.unwrap();
        base.search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().fetch_fields(["partner_id"]),
        )
        .await
        .unwrap();
        base.search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().fetch_fields(["value_pc"]),
        )
        .await
        .unwrap();
        base.name_search(&mut tx, "blablu").await.unwrap();
    }
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, >= 2, <= 2, {
        let found = base.search(&mut tx, &Domain::TRUE).await.unwrap();
        for r in found.iter() {
            r.value(&mut tx, "partner_id").await.unwrap();
        }
    });
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, >= 1, <= 1, {
        let found = base
            .search_with(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::new().fetch_fields(["partner_id"]),
            )
            .await
            .unwrap();
        for r in found.iter() {
            r.value(&mut tx, "partner_id").await.unwrap();
        }
    });
    tx.invalidate_all().await.unwrap();
    assert_query_count!(tx, >= 2, <= 2, {
        let found = base
            .search_with(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::new().fetch_fields(["value_pc"]),
            )
            .await
            .unwrap();
        for r in found.iter() {
            r.value(&mut tx, "partner_id").await.unwrap();
        }
    });
    tx.invalidate_all().await.unwrap();
    let found = assert_query_count!(tx, >= 1, <= 1, {
        base.name_search(&mut tx, "blablu")
            .await
            .unwrap()
    });
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1.as_deref(), Some("blablu"));
    drop(tx);
    h.cleanup().await.unwrap();
}

fn unaccent_on() -> erp_sdk::OrmConfig {
    erp_sdk::OrmConfig {
        unaccent: true,
        ..erp_sdk::OrmConfig::default()
    }
}

/// S2 review item 4: `unaccent` configured on without the extension fails schema sync clearly.
#[tokio::test]
async fn unaccent_without_the_extension_is_an_error() {
    let err = erp_testkit::Harness::builder([&erp_test_models::test_search::ADDON])
        .config(unaccent_on())
        .build()
        .await
        .err()
        .expect("schema sync must fail");
    assert!(
        err.to_string().contains("CREATE EXTENSION unaccent"),
        "{err}"
    );
}

/// stays accent- and case-sensitive.
#[tokio::test]
async fn spec_dom_065_accent_insensitive_with_unaccent() {
    let h = erp_testkit::Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_orm::ADDON,
        &erp_test_models::test_search::ADDON,
    ])
    .with_static_decls(crate::currency_fixture::decls())
    .before_schema(
        "CREATE EXTENSION IF NOT EXISTS unaccent; CREATE EXTENSION IF NOT EXISTS pg_trgm",
    )
    .config(unaccent_on())
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    // the trigram index and the condition use the same immutable wrapper
    let indexdef = support::one(
        &mut tx,
        "SELECT indexdef FROM pg_indexes WHERE indexname = 'test_orm_schema__index_trigram_index'",
    )
    .await
    .unwrap_or_default();
    assert!(
        indexdef.contains("gin (erp_unaccent(") && indexdef.contains("gin_trgm_ops"),
        "{indexdef}"
    );
    let p = "test_search.partner";
    let created = create(
        &mut tx,
        p,
        &[&[("name", s("Hélène"))], &[("name", s("Helga"))]],
    )
    .await;
    let ids = created.ids();
    tx.capture_statements(true);
    assert_eq!(
        search_ids(&mut tx, p, &dom(json!([["name", "ilike", "helene"]]))).await,
        [ids[0]]
    );
    assert!(
        last_statement(&tx).contains(
            "erp_unaccent(\"test_search_partner\".\"name\") ILIKE erp_unaccent($1::text)"
        ),
        "{}",
        last_statement(&tx)
    );
    assert!(
        search_ids(&mut tx, p, &dom(json!([["name", "=like", "Hel%ne"]])))
            .await
            .is_empty()
    );
    assert_eq!(
        search_ids(&mut tx, p, &dom(json!([["name", "not ilike", "HELENE"]]))).await,
        [ids[1]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_dom_019_selection_not_in_rewrite() {
    let h = support::orm().await;
    let tx = h.begin().await.unwrap();
    let selection = tx.env().records("test_orm.selection").unwrap();
    let opt = |d: Json, level| {
        selection
            .optimize_domain(&dom(d), level, None)
            .unwrap()
            .to_string()
    };
    use OptimizationLevel::DynamicValues;

    // no change for 'in'
    assert_eq!(
        opt(json!([["state", "in", ["foo"]]]), DynamicValues),
        "[('state', 'in', ['foo'])]"
    );
    assert_eq!(
        opt(json!([["state", "in", ["foo", false]]]), DynamicValues),
        "[('state', 'in', ['foo', False])]"
    );
    assert_eq!(
        opt(json!([["state", "in", [false]]]), DynamicValues),
        "[('state', 'in', [False])]"
    );

    // 'not in' becomes 'in'
    assert_eq!(
        opt(json!([["state", "not in", ["foo"]]]), DynamicValues),
        "[('state', 'in', ['bar', False])]"
    );
    assert_eq!(
        opt(json!([["state", "not in", ["foo", false]]]), DynamicValues),
        "[('state', 'in', ['bar'])]"
    );

    // this one remains as is, translated to SQL "state IS NOT NULL"
    assert_eq!(
        opt(json!([["state", "not in", [false]]]), DynamicValues),
        "[('state', 'not in', [False])]"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}
