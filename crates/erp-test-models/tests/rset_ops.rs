//! RSET cases of slice S6: `exists`, row locks, `filtered*`, `filtered_domain`, `mapped`,
//! `sorted`, `grouped`, batch contracts, per-model overrides, and the in-memory query budgets.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use std::collections::HashMap;

use erp_registry::ModelSpec;
use erp_sdk::serde_json::json;
use erp_sdk::{
    Date, DateTime, Decimal, Domain, Error, ErrorKind, Mapped, Operation, Operator, RawVals,
    RecordSet, Records, Result, SearchOptions, Transaction, Vals, Value,
};
use erp_test_models::test_rset::{
    self, ACTION_IDS_CACHE, Action, Category, City, Country, Line, Partner, Region, Tree,
};
use erp_test_models::{base_users, test_orm, test_search};
use erp_testkit::{Harness, assert_query_count};
use support::{assert_sql, dom};

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_orm::ADDON, &test_rset::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .expect("harness")
}

fn named(name: &str) -> Vals<Partner> {
    Vals::new().set(Partner::NAME, name)
}

async fn create(tx: &mut Transaction, vals: Vec<Vals<Partner>>) -> RecordSet<Partner> {
    let created = tx
        .env()
        .model::<Partner>()
        .unwrap()
        .create(tx, vals)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    created
}

const F1: [&str; 9] = [
    "Inner Works",
    "Pepper Street",
    "AnalytIQ",
    "Urban Trends",
    "Ctrl-Alt-Fix",
    "Ignitive Labs",
    "Amber & Forge",
    "Rebecca Day",
    "Gabriella Jennings",
];

async fn f1(tx: &mut Transaction) -> RecordSet<Partner> {
    create(tx, F1.iter().map(|n| named(n)).collect()).await
}

fn id_of<M: erp_sdk::Model>(r: &RecordSet<M>) -> i64 {
    r.ids()[0]
}

async fn delete_raw(tx: &mut Transaction, table: &str, id: i64) {
    let n = tx
        .execute(&format!("DELETE FROM \"{table}\" WHERE id = $1"), &[&id])
        .await
        .unwrap();
    assert_eq!(n, 1);
}

async fn names(tx: &mut Transaction, rs: &Records) -> Vec<String> {
    rs.values(tx, "name")
        .await
        .unwrap()
        .into_iter()
        .map(|v| match v {
            Value::Str(s) => s.to_string(),
            Value::False => "<False>".to_owned(),
            other => format!("{other}"),
        })
        .collect()
}

// ----- exists ------------------------------------------------------------------------------------

/// <= 0; the record `d` is deleted with raw SQL until `unlink` arrives in S3, which
/// `exists()` observes the same way)
#[tokio::test]
async fn spec_rset_023_exists() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(&mut tx, vec![named("p"), named("q"), named("d")]).await;
    let (p, d) = (rs.at(0).unwrap(), rs.at(2).unwrap());
    delete_raw(&mut tx, "test_rset_partner", id_of(&d)).await;
    let m = tx.env().model::<Partner>().unwrap();
    let all = m.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert_eq!(all.len(), 2);
    let existing = all.exists(&mut tx).await.unwrap();
    assert_eq!(existing.ids(), all.ids());

    let n = m.new_record(&mut tx, named("n")).await.unwrap();
    assert_query_count!(tx, <= 0, {
        let e = n.exists(&mut tx).await.unwrap();
        assert_eq!(e.rec_ids(), n.rec_ids(), "new records exist by convention");
        assert!(!e.is_empty());
    });
    assert!(
        m.browse([999_999])
            .unwrap()
            .exists(&mut tx)
            .await
            .unwrap()
            .is_empty()
    );
    let mixed = &(&(&p + &d) + &p) + &n;
    let e = assert_query_count!(tx, >= 1, <= 1, { mixed.exists(&mut tx).await.unwrap() });
    assert_eq!(
        e.rec_ids(),
        [p.rec_ids()[0], p.rec_ids()[0], n.rec_ids()[0]],
        "order and duplicates of the input, deleted record dropped"
    );
    assert_query_count!(tx, <= 0, {
        assert!(m.exists(&mut tx).await.unwrap().is_empty());
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Record rules for: the user cannot read partners named `hidden`.
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
            "test_rset.partner" => Domain::condition("name", Operator::Ne, "hidden").unwrap(),
            _ => Domain::TRUE,
        })
    }
}

#[tokio::test]
async fn spec_rset_024_exists_query_shape_and_cache_neutrality() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON, &test_rset::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .rules(HideNamed)
        .build()
        .await
        .unwrap();
    let mut su = h.begin().await.unwrap();
    let rs = create(
        &mut su,
        vec![
            named("p"),
            named("x").set(Partner::ACTIVE, false),
            named("hidden"),
            named("d"),
        ],
    )
    .await;
    let ids = rs.ids();
    su.commit().await.unwrap();

    let mut tx = h.begin_as(2).await.unwrap();
    let m = tx.env().model::<Partner>().unwrap();
    let (p, x, hid, d) = (
        m.browse([ids[0]]).unwrap(),
        m.browse([ids[1]]).unwrap(),
        m.browse([ids[2]]).unwrap(),
        m.browse([ids[3]]).unwrap(),
    );
    assert_eq!(
        p.get(&mut tx, Partner::NAME).await.unwrap().as_deref(),
        Some("p")
    );
    assert_eq!(
        d.get(&mut tx, Partner::NAME).await.unwrap().as_deref(),
        Some("d")
    );
    delete_raw(&mut tx, "test_rset_partner", ids[3]).await;

    let all = &(&(&p + &x) + &hid) + &d;
    tx.capture_statements(true);
    let existing = assert_query_count!(tx, >= 1, <= 1, { all.exists(&mut tx).await.unwrap() });
    assert_eq!(
        existing.ids(),
        &ids[..3],
        "archived and hidden records exist"
    );
    assert_sql(
        tx.captured().last().unwrap(),
        "SELECT \"test_rset_partner\".\"id\" FROM \"test_rset_partner\" \
         WHERE \"test_rset_partner\".\"id\" = ANY($1::int8[])",
    );
    // exists() neither reads nor invalidates the cache
    let name = assert_query_count!(tx, <= 0, { d.get(&mut tx, Partner::NAME).await.unwrap() });
    assert_eq!(name.as_deref(), Some("d"));
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- row locks ---------------------------------------------------------------------------------

async fn committed_partners(h: &Harness, n: usize) -> Vec<i64> {
    let mut su = h.begin().await.unwrap();
    let rs = create(
        &mut su,
        (0..n).map(|i| named(&format!("p{}", i + 1))).collect(),
    )
    .await;
    su.commit().await.unwrap();
    rs.ids()
}

fn is_lock_error(e: &Error) -> bool {
    e.kind() == ErrorKind::Lock && e.message() == "Cannot grab a lock on records"
}

#[tokio::test]
async fn spec_rset_025_lock_for_update() {
    let h = harness().await;
    let ids = committed_partners(&h, 2).await;
    let mut tx1 = h.begin().await.unwrap();
    let mut tx2 = h.begin().await.unwrap();
    let m1 = tx1.env().model::<Partner>().unwrap();
    let m2 = tx2.env().model::<Partner>().unwrap();
    let (p1, p2) = (m1.browse([ids[0]]).unwrap(), m1.browse([ids[1]]).unwrap());

    tx1.capture_statements(true);
    p1.lock_for_update(&mut tx1, true).await.unwrap();
    assert_sql(
        tx1.captured().last().unwrap(),
        "SELECT \"test_rset_partner\".\"id\" FROM \"test_rset_partner\" WHERE \
         \"test_rset_partner\".\"id\" = ANY($1::int8[]) FOR NO KEY UPDATE SKIP LOCKED",
    );
    p1.lock_for_update(&mut tx1, false).await.unwrap();
    assert_sql(
        tx1.captured().last().unwrap(),
        "SELECT \"test_rset_partner\".\"id\" FROM \"test_rset_partner\" WHERE \
         \"test_rset_partner\".\"id\" = ANY($1::int8[]) FOR UPDATE SKIP LOCKED",
    );

    let both = m2.browse([ids[0], ids[1]]).unwrap();
    let err = both.lock_for_update(&mut tx2, false).await.unwrap_err();
    assert!(is_lock_error(&err), "{err:?}");
    assert_eq!(err.kind().http_status(), 409);
    m2.browse([ids[1]])
        .unwrap()
        .lock_for_update(&mut tx2, false)
        .await
        .unwrap();

    // the first transaction can still read p2, but not lock it
    tx1.invalidate_all().await.unwrap();
    assert_eq!(
        p2.get(&mut tx1, Partner::NAME).await.unwrap().as_deref(),
        Some("p2")
    );
    let err = p2.lock_for_update(&mut tx1, false).await.unwrap_err();
    assert!(is_lock_error(&err), "{err:?}");

    tx2.rollback().await.unwrap();
    drop(tx2);
    p2.lock_for_update(&mut tx1, false).await.unwrap();
    drop(tx1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_026_lock_for_update_on_missing_new_and_empty_records() {
    let h = harness().await;
    let ids = committed_partners(&h, 2).await;
    let mut tx = h.begin().await.unwrap();
    let m = tx.env().model::<Partner>().unwrap();
    delete_raw(&mut tx, "test_rset_partner", ids[1]).await;
    let inexisting = m.browse([ids[1]]).unwrap();
    let err = inexisting
        .lock_for_update(&mut tx, false)
        .await
        .unwrap_err();
    assert!(is_lock_error(&err), "{err:?}");
    assert_eq!(err.kind().erp_name(), Some("erp.exceptions.LockError"));

    let n = m.new_record(&mut tx, named("n")).await.unwrap();
    assert_query_count!(tx, <= 0, {
        n.lock_for_update(&mut tx, false).await.unwrap();
        m.lock_for_update(&mut tx, false).await.unwrap();
    });
    let p = m.browse([ids[0]]).unwrap();
    (&p + &p).lock_for_update(&mut tx, false).await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_027_try_lock_for_update() {
    let h = harness().await;
    committed_partners(&h, 4).await;
    let mut tx1 = h.begin().await.unwrap();
    let m1 = tx1.env().model::<Partner>().unwrap();
    let recs = m1
        .search_with(&mut tx1, &Domain::TRUE, &SearchOptions::new().limit(4))
        .await
        .unwrap();
    let ids = recs.ids();
    let p1 = recs.at(0).unwrap();
    assert_eq!(
        p1.try_lock_for_update(&mut tx1, true, None)
            .await
            .unwrap()
            .ids(),
        [ids[0]]
    );
    assert_eq!(
        p1.try_lock_for_update(&mut tx1, false, None)
            .await
            .unwrap()
            .ids(),
        [ids[0]]
    );

    let mut tx2 = h.begin().await.unwrap();
    let m2 = tx2.env().model::<Partner>().unwrap();
    let pair = m2.browse([ids[0], ids[1]]).unwrap();
    assert_eq!(
        pair.try_lock_for_update(&mut tx2, false, None)
            .await
            .unwrap()
            .ids(),
        [ids[1]],
        "p1 is skipped, no error"
    );
    tx2.rollback().await.unwrap();
    drop(tx2);

    assert_eq!(
        recs.try_lock_for_update(&mut tx1, false, Some(1))
            .await
            .unwrap()
            .ids(),
        [ids[0]]
    );
    assert_eq!(
        recs.try_lock_for_update(&mut tx1, false, None)
            .await
            .unwrap()
            .ids(),
        ids
    );
    let reversed = recs.slice(None, None, -1).unwrap();
    assert_eq!(
        reversed
            .try_lock_for_update(&mut tx1, false, Some(1))
            .await
            .unwrap()
            .ids(),
        [ids[3]],
        "the limit follows recordset order"
    );
    drop(tx1);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_028_try_lock_for_update_with_new_records_and_limits() {
    let h = harness().await;
    let ids = committed_partners(&h, 2).await;
    let mut tx = h.begin().await.unwrap();
    let m = tx.env().model::<Partner>().unwrap();
    let (p1, p2) = (m.browse([ids[0]]).unwrap(), m.browse([ids[1]]).unwrap());
    let n1 = m.new_record(&mut tx, named("n1")).await.unwrap();
    let n2 = m.new_record(&mut tx, named("n2")).await.unwrap();
    let (n1_id, n2_id) = (n1.rec_ids()[0], n2.rec_ids()[0]);

    let locked = assert_query_count!(tx, <= 0, {
        (&(&p1 + &n1) + &n2)
            .try_lock_for_update(&mut tx, false, Some(2))
            .await
            .unwrap()
    });
    assert_eq!(locked.rec_ids(), [n1_id, n2_id]);

    tx.capture_statements(true);
    let locked = assert_query_count!(tx, >= 1, <= 1, {
        (&(&p1 + &n1) + &p2)
            .try_lock_for_update(&mut tx, false, Some(2))
            .await
            .unwrap()
    });
    assert_eq!(locked.rec_ids(), [p1.rec_ids()[0], n1_id]);
    assert_sql(
        tx.captured().last().unwrap(),
        "SELECT \"test_rset_partner\".\"id\" FROM \"test_rset_partner\" JOIN unnest($1::int8[]) \
         WITH ORDINALITY AS \"__ids\"(\"id\", \"ord\") ON \"test_rset_partner\".\"id\" = \
         \"__ids\".\"id\" ORDER BY \"__ids\".\"ord\" LIMIT $2::int8 FOR UPDATE OF \
         \"test_rset_partner\" SKIP LOCKED",
    );

    assert_query_count!(tx, <= 0, {
        let both = &n1 + &n2;
        assert_eq!(
            both.try_lock_for_update(&mut tx, false, None)
                .await
                .unwrap()
                .rec_ids(),
            [n1_id, n2_id]
        );
        assert!(
            m.try_lock_for_update(&mut tx, false, Some(3))
                .await
                .unwrap()
                .is_empty()
        );
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- filtered_domain, filtered, mapped ---------------------------------------------------------

#[tokio::test]
async fn spec_rset_031_filtered_domain_contract() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = f1(&mut tx).await;
    let m = tx.env().model::<Partner>().unwrap();
    for pattern in ["j", "in", "e"] {
        let d = dom(json!([["name", "ilike", pattern]]));
        let found = partners.filtered_domain(&mut tx, &d).await.unwrap();
        let searched = m
            .search_with(&mut tx, &d, &SearchOptions::new().order("id"))
            .await
            .unwrap();
        assert_eq!(found.ids(), searched.ids(), "{pattern}");
        assert_eq!(found.prefetch_ids(), partners.prefetch_ids());
        // the input's order, not the search order
        let reversed = partners.slice(None, None, -1).unwrap();
        let found = reversed.filtered_domain(&mut tx, &d).await.unwrap();
        let mut expected = searched.ids();
        expected.reverse();
        assert_eq!(found.ids(), expected);
    }
    assert_query_count!(tx, <= 0, {
        let d = dom(json!([["name", "ilike", "j"]]));
        assert!(m.filtered_domain(&mut tx, &d).await.unwrap().is_empty());
        let same = partners.filtered_domain(&mut tx, &Domain::TRUE).await.unwrap();
        assert_eq!(same.rec_ids(), partners.rec_ids());
        assert_eq!(same.prefetch_ids(), partners.prefetch_ids());
    });
    let (p, q) = (partners.at(0).unwrap(), partners.at(1).unwrap());
    let qpq = &(&q + &p) + &q;
    let d = dom(json!(["|", ["id", "=", id_of(&p)], ["id", "=", id_of(&q)]]));
    let found = qpq.filtered_domain(&mut tx, &d).await.unwrap();
    assert_eq!(found.rec_ids(), qpq.rec_ids(), "order and duplicates kept");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Rows with every kind of value, NULLs and falsy values included, for the search/filter
/// equivalence.
async fn mixed_rows(tx: &mut Transaction) -> (RecordSet<Partner>, i64) {
    let countries = tx.env().model::<Country>().unwrap();
    let c = countries
        .create(tx, vec![Vals::new().set(Country::NAME, "BE")])
        .await
        .unwrap();
    let cid = c.rec_ids()[0];
    let d = |s: &str| Date::parse_odoo(s).unwrap();
    let dt = |s: &str| DateTime::parse_odoo(s).unwrap();
    let rows = vec![
        Vals::new(),
        named("")
            .set(Partner::NUMBER, 0)
            .set(Partner::AMOUNT, 0.0)
            .set(Partner::PRICE, Decimal::ZERO),
        named("x")
            .set(Partner::NUMBER, 5)
            .set(Partner::AMOUNT, 2.5)
            .set(Partner::PRICE, Decimal::new(125, 2))
            .set(Partner::STATE, "draft")
            .set(Partner::DATE, d("2024-01-05"))
            .set(Partner::MOMENT, dt("2024-01-05 11:06:02"))
            .set(Partner::EMAIL, "x@example.com")
            .set(Partner::COUNTRY_ID, cid)
            .set(Partner::DATA, Some(json!({"a": 1}))),
        named("Bob")
            .set(Partner::NUMBER, -3)
            .set(Partner::AMOUNT, 7.25)
            .set(Partner::STATE, "open")
            .set(Partner::DATE, d("2023-12-31"))
            .set(Partner::ACTIVE, false)
            .set(Partner::EMAIL, "BOB@EXAMPLE.COM"),
        named("y_z%")
            .set(Partner::NUMBER, 12)
            .set(Partner::STATE, "cancel")
            .set(Partner::MOMENT, dt("2024-01-04 23:59:59"))
            .set(Partner::PRICE, Decimal::new(300, 2)),
        named("Hélène").set(Partner::NUMBER, 5),
        named("bob").set(Partner::NUMBER, 7),
        // text forms PostgreSQL's `::text` casts produce: exponent floats, numeric scale,
        // fractional seconds
        named("big")
            .set(Partner::NUMBER, 1)
            .set(Partner::AMOUNT, 1e16)
            .set(Partner::PRICE, Decimal::new(100, 2))
            .set(
                Partner::MOMENT,
                DateTime::from_parts(d("2024-01-06"), 8, 0, 0, 500_000).unwrap(),
            ),
        named("tiny")
            .set(Partner::NUMBER, 2)
            .set(Partner::AMOUNT, 0.00001)
            .set(Partner::PRICE, Decimal::new(5, 3))
            .set(
                Partner::MOMENT,
                DateTime::from_parts(d("2024-01-06"), 9, 30, 15, 123_456).unwrap(),
            ),
        named("neg")
            .set(Partner::NUMBER, 3)
            .set(Partner::AMOUNT, -0.0),
    ];
    (create(tx, rows).await, cid.get())
}

type Json = erp_sdk::serde_json::Value;

/// For each domain `D`: `search(D)` equals `all.filtered_domain(D)` (both by id), the same for
/// `!D`, and the two results partition `all`. `all` must be in id order; its environment is used
/// for both sides (timezone, context).
async fn assert_filtered_matches_search(
    tx: &mut Transaction,
    all: &RecordSet<Partner>,
    domains: &[Json],
) {
    let m = all.browse([]).unwrap();
    let opts = SearchOptions::new().order("id").active_test(false);
    let mut ids = all.ids();
    ids.sort_unstable();
    assert_eq!(all.ids(), ids, "`all` must be in id order");
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

/// The scalar part: for every scalar domain of the S2 search cases' kinds,
/// `search(D)` equals `search([]).filtered_domain(D)`, and `D` / `!D` partition the records in
/// memory as they do in SQL. Relational domains are added with slice S3.
#[tokio::test]
async fn filtered_domain_matches_search_on_scalar_domains() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (rows, cid) = mixed_rows(&mut tx).await;
    let mut ids = rows.ids();
    ids.sort_unstable();
    let m = tx.env().model::<Partner>().unwrap();
    let opts = SearchOptions::new().order("id").active_test(false);
    let all = m.search_with(&mut tx, &Domain::TRUE, &opts).await.unwrap();
    assert_eq!(all.ids(), ids);
    let domains = vec![
        // code-point order of the C collation ('B' < 'a' < 'b')
        json!([["name", "<", "a"]]),
        json!([["name", ">", "B"]]),
        json!([["name", "<", "b"]]),
        // Unicode case folding of ILIKE under the C collation with UTF-8 character type
        json!([["name", "ilike", "HÉL"]]),
        json!([["name", "ilike", "hél"]]),
        json!([["name", "=ilike", "HÉLÈNE"]]),
        json!([["name", "ilike", "bo"]]),
        // `like` on the `::text` forms of non-text columns
        json!([["price", "like", "1.0"]]),
        json!([["price", "like", ".00"]]),
        json!([["price", "=like", "0.005"]]),
        json!([["amount", "like", "e+"]]),
        json!([["amount", "like", "e-0"]]),
        json!([["amount", "=like", "-0"]]),
        json!([["amount", "like", "2.5"]]),
        json!([["moment", "like", "11:06"]]),
        json!([["moment", "like", ".5"]]),
        json!([["moment", "=like", "2024-01-06 09:30:15.123456"]]),
        json!([["date", "like", "-12-"]]),
        json!([["id", "like", "1"]]),
        // hashed date and datetime sets
        json!([["date", "in", ["2024-01-05", "2023-12-31"]]]),
        json!([["date", "not in", ["2024-01-05"]]]),
        json!([[
            "moment",
            "in",
            ["2024-01-05 11:06:02", "2024-01-06 08:00:00.5"]
        ]]),
        json!([["name", "=", false]]),
        json!([["name", "!=", false]]),
        json!([["name", "=", ""]]),
        json!([["name", "in", ["x", "y", false]]]),
        json!([["name", "not in", ["x"]]]),
        json!([["name", "like", "o"]]),
        json!([["name", "not like", "o"]]),
        json!([["name", "ilike", "B"]]),
        json!([["name", "not ilike", "B"]]),
        json!([["name", "=like", "y\\_z\\%"]]),
        json!([["name", "=ilike", "X"]]),
        json!([["name", "like", "_"]]),
        json!([["name", "ilike", "é"]]),
        json!([["name", "<", "m"]]),
        json!([["name", ">=", "x"]]),
        json!([["number", "=", 0]]),
        json!([["number", "!=", 0]]),
        json!([["number", "in", [0, 5]]]),
        json!([["number", "not in", [5]]]),
        json!([["number", "<", 1]]),
        json!([["number", ">", -5]]),
        json!([["number", "<=", false]]),
        json!([["number", "=", false]]),
        json!([["number", "=", "12"]]),
        json!([["number", "like", "2"]]),
        json!([["amount", "=", 0]]),
        json!([["amount", ">", 1]]),
        json!([["amount", "<", 3]]),
        json!([["amount", "!=", 2.5]]),
        json!([["price", "=", false]]),
        json!([["price", ">=", 1.25]]),
        json!([["price", "<", 2]]),
        json!([["price", "in", [3, 1.25]]]),
        json!([["state", "=", "draft"]]),
        json!([["state", "!=", "draft"]]),
        json!([["state", "in", ["open", "cancel"]]]),
        json!([["state", "=", false]]),
        json!([["state", "not in", ["open"]]]),
        json!([["state", ">", "d"]]),
        json!([["date", "=", false]]),
        json!([["date", ">", "2024-01-01"]]),
        json!([["date", "<=", "2024-01-05"]]),
        json!([["date", "!=", "2024-01-05"]]),
        json!([["moment", ">=", "2024-01-05 10:00:00"]]),
        json!([["moment", "<", "2024-01-05 00:00:00"]]),
        json!([["moment", "=", false]]),
        json!([["active", "=", true]]),
        json!([["active", "=", false]]),
        json!([["active", "!=", true]]),
        json!([["active", "in", [true, false]]]),
        json!([["id", "in", [ids[0], ids[3]]]]),
        json!([["id", "not in", [ids[1]]]]),
        json!([["id", ">", ids[2]]]),
        json!([["email", "ilike", "EXAMPLE"]]),
        json!([["email", "=ilike", "bob@example.com"]]),
        json!([["country_id", "=", false]]),
        json!([["country_id", "in", [cid]]]),
        json!([["country_id", "!=", cid]]),
        json!([["data", "=", false]]),
        json!([["data", "!=", false]]),
        json!(["|", ["name", "=", "x"], ["number", ">", 3]]),
        json!(["&", ["name", "!=", false], "!", ["number", "=", 5]]),
        json!(["!", ["state", "in", ["draft", "open"]]]),
        json!([
            "|",
            "&",
            ["number", ">", 0],
            ["amount", "=", false],
            ["state", "=", "open"]
        ]),
    ];
    assert_filtered_matches_search(&mut tx, &all, &domains).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Date literals on datetime fields and datetime sets resolve in the environment's timezone the
/// same way for `filtered_domain` and `search` (part).
#[tokio::test]
async fn filtered_domain_matches_search_under_a_timezone() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    mixed_rows(&mut tx).await;
    let opts = SearchOptions::new().order("id").active_test(false);
    for tz in ["Europe/Brussels", "Asia/Jakarta", "America/New_York"] {
        let m = tx
            .env()
            .model::<Partner>()
            .unwrap()
            .with_context([("tz", Value::str(tz))]);
        let all = m.search_with(&mut tx, &Domain::TRUE, &opts).await.unwrap();
        let domains = vec![
            json!([["moment", ">=", "2024-01-05"]]),
            json!([["moment", "<", "2024-01-06"]]),
            json!([["moment", "=", "2024-01-05"]]),
            json!([["moment", "in", ["2024-01-05 11:06:02"]]]),
            json!([["moment", ">", "2024-01-05 00:30:00"]]),
            json!([["date", "=", "2024-01-05"]]),
            json!([["date", ">=", "2024-01-05 23:00:00"]]),
        ];
        assert_filtered_matches_search(&mut tx, &all, &domains).await;
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// With `OrmConfig::unaccent` on, in-memory `ilike` folds accents like PostgreSQL's `unaccent()`
/// for Latin letters, then ASCII case (part and).
#[tokio::test]
async fn filtered_domain_matches_search_with_unaccent() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON, &test_rset::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .config(erp_sdk::OrmConfig {
            unaccent: true,
            ..erp_sdk::OrmConfig::default()
        })
        .before_schema("CREATE EXTENSION IF NOT EXISTS unaccent")
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        [
            "Hélène",
            "HELENE",
            "Ærøskøbing",
            "Straße",
            "Œuvre",
            "Ångström",
            "Çà et là",
            "Łódź",
            "naïve",
            "plain",
        ]
        .iter()
        .map(|n| named(n))
        .collect(),
    )
    .await;
    let m = tx.env().model::<Partner>().unwrap();
    let opts = SearchOptions::new().order("id").active_test(false);
    let all = m.search_with(&mut tx, &Domain::TRUE, &opts).await.unwrap();
    let domains = vec![
        json!([["name", "ilike", "helene"]]),
        json!([["name", "ilike", "HÉLÈNE"]]),
        json!([["name", "ilike", "aeroskobing"]]),
        json!([["name", "ilike", "strasse"]]),
        json!([["name", "ilike", "oeuvre"]]),
        json!([["name", "=ilike", "angstrom"]]),
        json!([["name", "ilike", "ca et la"]]),
        json!([["name", "ilike", "lodz"]]),
        json!([["name", "not ilike", "naive"]]),
        json!([["name", "like", "Hel"]]),
        json!([["name", "like", "Hél"]]),
    ];
    assert_filtered_matches_search(&mut tx, &all, &domains).await;
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Errors are reported even where the evaluation could stop early: an unknown field next to a
/// condition that optimizes to FALSE, and a `LIKE` pattern ending with the escape character
/// (PostgreSQL rejects it).
#[tokio::test]
async fn filtered_domain_input_errors() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(&mut tx, vec![named("a"), named("b")]).await;
    for json in [
        json!([["nope", "=", 1], ["id", "in", []]]),
        json!(["|", ["id", "in", []], ["nope", "=", 1]]),
        json!([["name", "=like", "a\\"]]),
        json!([["name", "=ilike", "\\"]]),
    ] {
        let err = rs
            .filtered_domain(&mut tx, &dom(json.clone()))
            .await
            .expect_err(&json.to_string());
        assert_eq!(err.kind(), ErrorKind::Validation, "{json}: {err:?}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Part; the relational cases are in `rel_rset.rs`.
#[tokio::test]
async fn filtered_domain_on_computed_display_and_relational_fields() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(&mut tx, vec![named("ann"), named("bob"), named("Anna")]).await;
    let ids = rs.ids();
    let upper = dom(json!([["upper", "like", "ANN"]]));
    assert_eq!(
        rs.filtered_domain(&mut tx, &upper).await.unwrap().ids(),
        [ids[0], ids[2]],
        "non-stored computed field"
    );
    let label = dom(json!([["display_name", "ilike", "ann"]]));
    assert_eq!(
        rs.filtered_domain(&mut tx, &label).await.unwrap().ids(),
        [ids[0], ids[2]]
    );
    let be = countries_bac(&mut tx).await.rec_ids()[0];
    rs.at(1)
        .unwrap()
        .set(&mut tx, Partner::COUNTRY_ID, be)
        .await
        .unwrap();
    tx.env()
        .model::<Country>()
        .unwrap()
        .browse([be.get()])
        .unwrap()
        .set(&mut tx, Country::NAME, "BE")
        .await
        .unwrap();
    for json in [
        json!([["country_id.name", "=", "BE"]]),
        json!([["country_id", "any", [["name", "=", "BE"]]]]),
    ] {
        let found = rs
            .filtered_domain(&mut tx, &dom(json.clone()))
            .await
            .unwrap();
        assert_eq!(found.ids(), [ids[1]], "{json}");
    }
    let built = Domain::condition("country_id.name", Operator::Eq, "BE").unwrap();
    let found = rs.filtered_domain(&mut tx, &built).await.unwrap();
    assert_eq!(found.ids(), [ids[1]]);
    let none = Domain::condition("country_id", Operator::Eq, false).unwrap();
    let found = rs.filtered_domain(&mut tx, &none).await.unwrap();
    assert_eq!(found.ids(), [ids[0], ids[2]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The scalar part and: `filtered` with a callable over a field,
/// with a field name and with a callable needing no data (the relational paths are in
/// `rel_rset.rs`; the list-domain `TypeError` belongs to the dynamic layer).
#[tokio::test]
async fn filtered_by_callable_field_name_and_path() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(
        &mut tx,
        vec![
            named("p").set(Partner::EMAIL, "p@x"),
            named("q"),
            named("r").set(Partner::EMAIL, ""),
            named("s").set(Partner::EMAIL, "s@x"),
        ],
    )
    .await;
    let ids = rs.ids();
    let with_email = rs
        .filtered(&mut tx, Partner::EMAIL, |e| {
            e.as_deref().is_some_and(|e| !e.is_empty())
        })
        .await
        .unwrap();
    assert_eq!(with_email.ids(), [ids[0], ids[3]]);
    assert_eq!(with_email.prefetch_ids(), rs.prefetch_ids());
    let by_name = rs
        .as_records()
        .filtered_path(&mut tx, "email")
        .await
        .unwrap();
    assert_eq!(by_name.ids(), with_email.ids(), "'' is falsy like False");
    let (p, q) = (rs.at(0).unwrap(), rs.at(1).unwrap());
    let pqp = &(&p + &q) + &p;
    assert_eq!(
        pqp.as_records()
            .filtered_path(&mut tx, "email")
            .await
            .unwrap()
            .ids(),
        [ids[0], ids[0]],
        "duplicates kept"
    );
    assert_query_count!(tx, <= 0, {
        let same = rs.as_records().filtered_path(&mut tx, "").await.unwrap();
        assert_eq!(same.rec_ids(), rs.rec_ids());
    });
    let odd = rs.filtered_by(|r| r.ids()[0] % 2 == 1);
    assert!(odd.ids().iter().all(|id| id % 2 == 1));
    assert_eq!(odd.prefetch_ids(), rs.prefetch_ids());

    let err = rs
        .as_records()
        .filtered_path(&mut tx, "nope")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    let err = rs
        .as_records()
        .filtered_path(&mut tx, "country_id.nope")
        .await
        .unwrap_err();
    assert!(err.message().contains("Invalid field 'nope'"), "{err:?}");
    // nobody has a country here
    let through = rs
        .as_records()
        .filtered_path(&mut tx, "country_id.name")
        .await
        .unwrap();
    assert!(through.is_empty());
    let err = rs
        .as_records()
        .filtered_path(&mut tx, "name.upper")
        .await
        .unwrap_err();
    assert!(err.message().contains("is not a path"), "{err:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_032_mapped_on_a_scalar_field() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(
        &mut tx,
        vec![
            named("p")
                .set(Partner::EMAIL, "a@x")
                .set(Partner::NUMBER, 7),
            named("q"),
        ],
    )
    .await;
    let (p, q) = (rs.at(0).unwrap(), rs.at(1).unwrap());
    let pqp = &(&p + &q) + &p;
    let emails = pqp.mapped(&mut tx, Partner::EMAIL).await.unwrap();
    assert_eq!(
        emails.iter().map(|e| e.as_deref()).collect::<Vec<_>>(),
        [Some("a@x"), None, Some("a@x")]
    );
    assert_eq!(
        pqp.as_records()
            .mapped_path(&mut tx, "email")
            .await
            .unwrap(),
        Mapped::Values(vec![Value::str("a@x"), Value::False, Value::str("a@x")])
    );
    match pqp.as_records().mapped_path(&mut tx, "").await.unwrap() {
        Mapped::Records(same) => assert_eq!(same.rec_ids(), pqp.rec_ids()),
        other => panic!("{other:?}"),
    }
    let empty = rs.browse([]).unwrap();
    assert!(
        empty
            .mapped(&mut tx, Partner::EMAIL)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        (&p + &q)
            .as_records()
            .mapped_path(&mut tx, "id")
            .await
            .unwrap(),
        Mapped::Values(vec![Value::Int(id_of(&p)), Value::Int(id_of(&q))])
    );
    // the integer of q is NULL in the database and reads as 0
    assert_eq!(
        (&p + &q).mapped(&mut tx, Partner::NUMBER).await.unwrap(),
        [7, 0]
    );
    // nobody has a country: no value at all (one per distinct target)
    assert_eq!(
        rs.as_records()
            .mapped_path(&mut tx, "country_id.name")
            .await
            .unwrap(),
        Mapped::Values(Vec::new())
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- sorted ------------------------------------------------------------------------------------

async fn countries_bac(tx: &mut Transaction) -> RecordSet<Country> {
    tx.env()
        .model::<Country>()
        .unwrap()
        .create(
            tx,
            ["B", "A", "C"]
                .iter()
                .map(|n| Vals::new().set(Country::NAME, *n))
                .collect(),
        )
        .await
        .unwrap()
}

/// F2: countries B, A, C and cities c2→C, b1→B, b2→B, c1→C, a1→A, a2→A.
async fn f2(tx: &mut Transaction) -> (RecordSet<Country>, RecordSet<City>) {
    let countries = countries_bac(tx).await;
    let id = |i: isize| countries.at(i).unwrap().rec_ids()[0];
    let (b, a, c) = (id(0), id(1), id(2));
    let cities = tx
        .env()
        .model::<City>()
        .unwrap()
        .create(
            tx,
            [
                ("c2", c),
                ("b1", b),
                ("b2", b),
                ("c1", c),
                ("a1", a),
                ("a2", a),
            ]
            .iter()
            .map(|(n, k)| Vals::new().set(City::NAME, *n).set(City::COUNTRY_ID, *k))
            .collect(),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    (countries, cities)
}

#[tokio::test]
async fn spec_rset_037_sorted_by_the_model_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = f1(&mut tx).await;
    let n = partners.len() as isize;
    let shuffled = &partners.slice(Some(n / 2), None, 1).unwrap()
        + &partners.slice(None, Some(n / 2), 1).unwrap();
    assert_ne!(shuffled.ids(), partners.ids());
    assert_eq!(
        shuffled.sorted(&mut tx, None, false).await.unwrap().ids(),
        partners.ids()
    );

    let (countries, _) = f2(&mut tx).await;
    let m = tx.env().model::<Country>().unwrap();
    let searched = m.search(&mut tx, &Domain::TRUE).await.unwrap();
    tx.invalidate_model("test_rset.country", None)
        .await
        .unwrap();
    let sorted = assert_query_count!(tx, >= 1, <= 1, {
        countries.sorted(&mut tx, None, false).await.unwrap()
    });
    assert_eq!(sorted.ids(), searched.ids());
    assert_eq!(names(&mut tx, &sorted).await, ["A", "B", "C"]);
    let reversed = assert_query_count!(tx, <= 0, {
        countries.sorted(&mut tx, None, true).await.unwrap()
    });
    let mut expected = searched.ids();
    expected.reverse();
    assert_eq!(reversed.ids(), expected);
    assert_eq!(sorted.prefetch_ids(), countries.prefetch_ids());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_038_sorted_with_a_callable_or_a_field_name() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut vals: Vec<Vals<Partner>> = F1.iter().map(|n| named(n)).collect();
    vals.push(named("Pepper Street"));
    let partners = create(&mut tx, vals).await;
    let name_of: HashMap<i64, String> = partners
        .ids()
        .into_iter()
        .zip(names(&mut tx, partners.as_records()).await)
        .collect();
    let mut by_name = partners.ids();
    by_name.sort_by(|a, b| name_of[a].cmp(&name_of[b]));

    let by_callable = partners.sorted_by_key(|p| name_of[&p.ids()[0]].clone(), false);
    assert_eq!(by_callable.ids(), by_name);
    assert_eq!(
        partners
            .sorted(&mut tx, Some("name"), false)
            .await
            .unwrap()
            .ids(),
        by_name
    );
    let mut reverse = partners.ids();
    reverse.sort_by(|a, b| name_of[b].cmp(&name_of[a]));
    assert_eq!(
        partners
            .sorted(&mut tx, Some("name"), true)
            .await
            .unwrap()
            .ids(),
        reverse,
        "stable reverse: equal names keep their input order"
    );
    let mut desc_id = partners.ids();
    desc_id.sort_by(|a, b| name_of[b].cmp(&name_of[a]).then(a.cmp(b)));
    assert_eq!(
        partners
            .sorted(&mut tx, Some("name DESC, id"), false)
            .await
            .unwrap()
            .ids(),
        desc_id
    );
    let p = partners.at(0).unwrap();
    assert_query_count!(tx, <= 0, {
        assert_eq!(
            p.sorted(&mut tx, Some("nope"), false).await.unwrap().ids(),
            p.ids()
        );
        let empty = partners.browse([]).unwrap();
        assert!(empty.sorted(&mut tx, Some("nope"), false).await.unwrap().is_empty());
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_039_sorted_is_stable() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, cities) = f2(&mut tx).await;
    let twice = cities
        .sorted(&mut tx, Some("name"), true)
        .await
        .unwrap()
        .sorted(&mut tx, Some("country_id.id"), false)
        .await
        .unwrap();
    let country: Vec<i64> = cities
        .mapped(&mut tx, City::COUNTRY_ID)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.map_or(0, |c| c.get()))
        .collect();
    let name = names(&mut tx, cities.as_records()).await;
    let key: HashMap<i64, (i64, String)> = cities
        .ids()
        .into_iter()
        .zip(country.into_iter().zip(name))
        .map(|(id, (c, n))| (id, (-c, n)))
        .collect();
    let by_key = cities.sorted_by_key(|c| key[&c.ids()[0]].clone(), true);
    assert_eq!(twice.ids(), by_key.ids());
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn reference(tx: &mut Transaction, sql: &str, ids: &[i64]) -> Vec<i64> {
    let ids = ids.to_vec();
    tx.query(sql, &[&ids])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, i64>(0))
        .collect()
}

/// comodel order on a LEFT JOIN — is the reference)
#[tokio::test]
async fn spec_rset_040_sorted_on_many2one_uses_the_comodel_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (_, cities) = f2(&mut tx).await;
    let m = tx.env().model::<City>().unwrap();
    let within = dom(json!([["id", "in", cities.ids()]]));
    for (order, expected) in [
        (None, ["a1", "a2", "b1", "b2", "c1", "c2"]),
        (
            Some("country_id DESC, id ASC"),
            ["c2", "c1", "b1", "b2", "a1", "a2"],
        ),
    ] {
        tx.invalidate_all().await.unwrap();
        let sorted = assert_query_count!(tx, >= 2, <= 2, {
            cities.sorted(&mut tx, order, false).await.unwrap()
        });
        assert_eq!(names(&mut tx, sorted.as_records()).await, expected);
        let opts = order.map_or_else(SearchOptions::new, |o| {
            SearchOptions::new().order(o.to_owned())
        });
        let searched = m.search_with(&mut tx, &within, &opts).await.unwrap();
        assert_eq!(sorted.ids(), searched.ids(), "{order:?}");
        let reversed = assert_query_count!(tx, <= 0, {
            cities.sorted(&mut tx, order, true).await.unwrap()
        });
        let mut back = sorted.ids();
        back.reverse();
        assert_eq!(reversed.ids(), back);
    }
    let countries = tx.env().model::<Country>().unwrap();
    let a = countries
        .search(&mut tx, &dom(json!([["name", "=", "A"]])))
        .await
        .unwrap();
    a.set(&mut tx, Country::NAME, "Z").await.unwrap();
    let sorted = cities.sorted(&mut tx, None, false).await.unwrap();
    assert_eq!(
        names(&mut tx, sorted.as_records()).await,
        ["b1", "b2", "c1", "c2", "a1", "a2"]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_041_sorted_null_placement_matches_sql() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let countries = countries_bac(&mut tx).await;
    let id = |i: isize| countries.at(i).unwrap().rec_ids()[0];
    let (b, a, c) = (id(0), id(1), id(2));
    // a country without a name: its cities sort by the comodel's NULL name, not as a NULL
    // many2one
    let nameless = tx
        .env()
        .model::<Country>()
        .unwrap()
        .create(&mut tx, vec![Vals::new()])
        .await
        .unwrap()
        .rec_ids()[0];
    let rows: Vec<Vals<City>> = vec![
        Vals::new()
            .set(City::NAME, "nameless land")
            .set(City::COUNTRY_ID, nameless),
        Vals::new()
            .set(City::NAME, "not null 2")
            .set(City::COUNTRY_ID, c),
        Vals::new()
            .set(City::NAME, "not null 0")
            .set(City::COUNTRY_ID, b),
        Vals::new().set(City::COUNTRY_ID, a),
        Vals::new().set(City::NAME, ""),
        Vals::new(),
        Vals::new()
            .set(City::NAME, "not null 1")
            .set(City::COUNTRY_ID, a),
    ];
    let cities = tx
        .env()
        .model::<City>()
        .unwrap()
        .create(&mut tx, rows)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let ids = cities.ids();
    let m = tx.env().model::<City>().unwrap();
    let within = dom(json!([["id", "in", ids.clone()]]));
    for field in ["country_id", "name"] {
        for dir in ["ASC", "DESC"] {
            for nulls in ["", " NULLS FIRST", " NULLS LAST"] {
                let order = format!("{field} {dir}{nulls}, id");
                let sorted = cities.sorted(&mut tx, Some(&order), false).await.unwrap();
                let expected = m
                    .search_with(&mut tx, &within, &SearchOptions::new().order(order.clone()))
                    .await
                    .unwrap()
                    .ids();
                assert_eq!(sorted.ids(), expected, "{order}");
            }
        }
    }
    // '' is a value, not null: ascending puts it first, NULLs last
    let sorted = cities
        .sorted(&mut tx, Some("name, id"), false)
        .await
        .unwrap();
    let names = names(&mut tx, sorted.as_records()).await;
    assert_eq!(names.first().map(String::as_str), Some(""));
    assert_eq!(&names[5..], ["<False>", "<False>"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The NULL rules of a many2one term beyond's fixture: a record without target sorts
/// as if every comodel column were NULL (where the comodel order places NULLs: first here, since
/// it starts with `name desc`), unless the term has an explicit NULLS clause; a target whose
/// order fields are NULL sorts by them. Reference: `search` with the same order.
#[tokio::test]
async fn sorted_many2one_null_targets_follow_the_comodel_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let regions = tx
        .env()
        .model::<Region>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new().set(Region::NAME, "North"),
                Vals::new().set(Region::NAME, "South"),
                Vals::new(),
            ],
        )
        .await
        .unwrap();
    let r = |i: usize| regions.rec_ids()[i];
    let rows: Vec<Vals<City>> = vec![
        Vals::new().set(City::NAME, "x").set(City::REGION_ID, r(0)),
        Vals::new().set(City::NAME, "y").set(City::REGION_ID, r(1)),
        Vals::new().set(City::NAME, "z"),
        Vals::new().set(City::NAME, "w").set(City::REGION_ID, r(2)),
        Vals::new().set(City::NAME, "v"),
        Vals::new().set(City::NAME, "u").set(City::REGION_ID, r(0)),
    ];
    let cities = tx
        .env()
        .model::<City>()
        .unwrap()
        .create(&mut tx, rows)
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let m = tx.env().model::<City>().unwrap();
    let within = dom(json!([["id", "in", cities.ids()]]));
    for dir in ["ASC", "DESC"] {
        for nulls in ["", " NULLS FIRST", " NULLS LAST"] {
            let order = format!("region_id {dir}{nulls}, id");
            let sorted = cities.sorted(&mut tx, Some(&order), false).await.unwrap();
            let searched = m
                .search_with(&mut tx, &within, &SearchOptions::new().order(order.clone()))
                .await
                .unwrap();
            assert_eq!(sorted.ids(), searched.ids(), "{order}");
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_042_sorted_on_booleans() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(
        &mut tx,
        vec![
            named("a"),
            named("b").set(Partner::ACTIVE, false),
            named("c"),
        ],
    )
    .await;
    let ids = rs.ids();
    rs.fetch(&mut tx, None).await.unwrap();
    assert_query_count!(tx, <= 0, {
        assert_eq!(
            rs.sorted(&mut tx, Some("active, name"), false)
                .await
                .unwrap()
                .ids(),
            [ids[1], ids[0], ids[2]]
        );
        assert_eq!(
            rs.sorted(&mut tx, Some("active DESC, name"), false)
                .await
                .unwrap()
                .ids(),
            [ids[0], ids[2], ids[1]]
        );
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_043_sorted_text_collation_matches_the_database() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let names = [
        "é", "e", "É", "1.0", "1,0", "01", "10", "9", "Ab", "👍", "AB", "Aa", "AA",
    ];
    let countries = tx
        .env()
        .model::<Country>()
        .unwrap()
        .create(
            &mut tx,
            names
                .iter()
                .map(|n| Vals::new().set(Country::NAME, *n))
                .collect(),
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();
    let ids = countries.ids();
    for dir in ["ASC", "DESC"] {
        let sorted = countries
            .sorted(&mut tx, Some(&format!("name {dir}")), false)
            .await
            .unwrap();
        let sql = format!(
            "SELECT id FROM test_rset_country WHERE id = ANY($1) ORDER BY name COLLATE \"C\" {dir}"
        );
        assert_eq!(sorted.ids(), reference(&mut tx, &sql, &ids).await, "{dir}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_044_sorted_on_a_self_referencing_many2one() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let m = tx.env().model::<Category>().unwrap();
    let roots = m
        .create(
            &mut tx,
            ["Mammals", "Birds"]
                .iter()
                .map(|n| Vals::new().set(Category::NAME, *n))
                .collect(),
        )
        .await
        .unwrap();
    let (mammals, birds) = (roots.rec_ids()[0], roots.rec_ids()[1]);
    m.create(
        &mut tx,
        [
            ("Whales", mammals),
            ("Eagles", birds),
            ("Cats", mammals),
            ("Owls", birds),
        ]
        .iter()
        .map(|(n, p)| {
            Vals::new()
                .set(Category::NAME, *n)
                .set(Category::PARENT, *p)
        })
        .collect(),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();
    let all = m.search(&mut tx, &Domain::TRUE).await.unwrap();
    let within = dom(json!([["id", "in", all.ids()]]));
    for (dir, id_dir) in [
        ("ASC", "ASC"),
        ("ASC", "DESC"),
        ("DESC", "ASC"),
        ("DESC", "DESC"),
    ] {
        let order = format!("parent {dir}, id {id_dir}");
        tx.invalidate_all().await.unwrap();
        let sorted = all.sorted(&mut tx, Some(&order), false).await.unwrap();
        let searched = m
            .search_with(&mut tx, &within, &SearchOptions::new().order(order.clone()))
            .await
            .unwrap();
        assert_eq!(sorted.ids(), searched.ids(), "{order}");
    }

    // a model whose order starts with its own parent: the inner many2one compares as equal
    let t = tx.env().model::<Tree>().unwrap();
    let r = t
        .create(&mut tx, vec![Vals::new().set(Tree::NAME, "r")])
        .await
        .unwrap();
    let rid = r.rec_ids()[0];
    let xy = t
        .create(
            &mut tx,
            ["y", "x"]
                .iter()
                .map(|n| Vals::new().set(Tree::NAME, *n).set(Tree::PARENT_ID, rid))
                .collect(),
        )
        .await
        .unwrap();
    let x = xy.rec_ids()[1];
    let z = t
        .create(
            &mut tx,
            vec![Vals::new().set(Tree::NAME, "z").set(Tree::PARENT_ID, x)],
        )
        .await
        .unwrap();
    let trees = &(&z + &xy) + &r;
    let sorted = trees.sorted(&mut tx, None, false).await.unwrap();
    assert_eq!(
        names(&mut tx, sorted.as_records()).await,
        ["x", "y", "z", "r"],
        "parents compared by name; no parent last"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The parts that need no x2many field: order-spec errors and the ignored
/// `:func` (the one2many case completes with slice S3).
#[tokio::test]
async fn sorted_order_spec_errors() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(&mut tx, vec![named("b"), named("a")]).await;
    for (order, message) in [
        ("name DESCENDING", "Invalid order 'name DESCENDING' to sort"),
        ("name,", "Invalid order 'name,' to sort"),
        ("", "Invalid order '' to sort"),
        (
            "country_id.name",
            "Invalid order on relational field 'country_id.name' to sort",
        ),
    ] {
        let err = rs.sorted(&mut tx, Some(order), false).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming, "{order}");
        assert_eq!(err.message(), message);
    }
    let err = rs.sorted(&mut tx, Some("nope"), false).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("'nope'"), "{err:?}");
    let err = rs.sorted(&mut tx, Some("data"), false).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    let by_func = rs
        .sorted(&mut tx, Some("create_date:month, name"), false)
        .await
        .unwrap();
    assert_eq!(names(&mut tx, by_func.as_records()).await, ["a", "b"]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn sorted_keeps_references_differences_from_search_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(
        &mut tx,
        vec![
            named("o")
                .set(Partner::STATE, "open")
                .set(Partner::NUMBER, 1),
            named("c").set(Partner::STATE, "cancel"),
            named("d")
                .set(Partner::STATE, "draft")
                .set(Partner::NUMBER, 0),
        ],
    )
    .await;
    let ids = rs.ids();
    let m = tx.env().model::<Partner>().unwrap();
    let opts = |o: &str| SearchOptions::new().order(o.to_owned());
    assert_eq!(
        rs.sorted(&mut tx, Some("state"), false)
            .await
            .unwrap()
            .ids(),
        [ids[1], ids[2], ids[0]],
        "cancel, draft, open"
    );
    assert_eq!(
        m.search_with(&mut tx, &Domain::TRUE, &opts("state"))
            .await
            .unwrap()
            .ids(),
        [ids[2], ids[0], ids[1]],
        "draft, open, cancel"
    );
    // c (NULL) and d (0) tie on 0 and fall back to id
    let (c, d) = (ids[1], ids[2]);
    assert_eq!(
        rs.sorted(&mut tx, Some("number, id"), false)
            .await
            .unwrap()
            .ids(),
        [c.min(d), c.max(d), ids[0]],
        "NULL reads 0"
    );
    assert_eq!(
        m.search_with(&mut tx, &Domain::TRUE, &opts("number, id"))
            .await
            .unwrap()
            .ids(),
        [ids[2], ids[0], ids[1]],
        "NULL last"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Negative and positive float zero tie, as in SQL.
#[tokio::test]
async fn sorted_matches_search_order_for_every_scalar_type() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let countries = countries_bac(&mut tx).await;
    let k = |i: isize| countries.at(i).unwrap().rec_ids()[0];
    let d = |s: &str| Date::parse_odoo(s).unwrap();
    let dt = |s: &str| DateTime::parse_odoo(s).unwrap();
    let full = |name: &str, n: i32, amount: f64, price: i64, day: &str, k: erp_sdk::RecId| {
        named(name)
            .set(Partner::NUMBER, n)
            .set(Partner::AMOUNT, amount)
            .set(Partner::PRICE, Decimal::new(price, 2))
            .set(Partner::DATE, d(day))
            .set(Partner::MOMENT, dt(&format!("{day} 10:00:00")))
            .set(Partner::COUNTRY_ID, k)
    };
    create(
        &mut tx,
        vec![
            full("b", 3, -0.0, 150, "2024-02-01", k(0)).set(Partner::EMAIL, "z@x"),
            full("B", -1, 0.0, -20, "2023-12-31", k(1)).set(Partner::ACTIVE, false),
            full("a", 3, 2.5, 150, "2024-02-01", k(2)).set(Partner::STATE, "open"),
            full("Ä", 10, -7.25, 0, "2024-01-15", k(0)).set(Partner::STATE, "draft"),
            full("aa", 0, 1e16, 99, "2024-01-15", k(1)).set(Partner::EMAIL, "A@x"),
        ],
    )
    .await;
    let m = tx.env().model::<Partner>().unwrap();
    let all = m
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().active_test(false).order("id"),
        )
        .await
        .unwrap();
    for field in [
        "name",
        "email",
        "number",
        "amount",
        "price",
        "date",
        "moment",
        "active",
        "country_id.id",
        "id",
    ] {
        for dir in ["ASC", "DESC"] {
            let order = format!("{field} {dir}, id");
            let sorted = all.sorted(&mut tx, Some(&order), false).await.unwrap();
            let searched = m
                .search_with(
                    &mut tx,
                    &Domain::TRUE,
                    &SearchOptions::new().active_test(false).order(order.clone()),
                )
                .await
                .unwrap();
            assert_eq!(sorted.ids(), searched.ids(), "{order}");
        }
    }
    // selections by key text: draft, open, then NULLs last
    let by_state = all.sorted(&mut tx, Some("state, id"), false).await.unwrap();
    let states: Vec<Option<String>> = by_state
        .mapped(&mut tx, Partner::STATE)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.map(|s| s.to_string()))
        .collect();
    assert_eq!(
        states,
        [Some("draft".into()), Some("open".into()), None, None, None]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// `lock_for_update` stops at the first chunk that could not lock every row: the later chunks
/// are not sent.
#[tokio::test]
async fn lock_for_update_stops_at_the_first_short_chunk() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON, &test_rset::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .config(erp_sdk::OrmConfig {
            prefetch_chunk: 1,
            ..erp_sdk::OrmConfig::default()
        })
        .build()
        .await
        .unwrap();
    let ids = committed_partners(&h, 3).await;
    let mut other = h.begin().await.unwrap();
    other
        .env()
        .model::<Partner>()
        .unwrap()
        .browse([ids[0]])
        .unwrap()
        .lock_for_update(&mut other, false)
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let all = tx
        .env()
        .model::<Partner>()
        .unwrap()
        .browse(ids.clone())
        .unwrap();
    let err = assert_query_count!(tx, >= 1, <= 1, {
        all.lock_for_update(&mut tx, false).await.unwrap_err()
    });
    assert!(is_lock_error(&err), "{err:?}");
    drop(tx);
    drop(other);
    h.cleanup().await.unwrap();
}

// ----- grouped -----------------------------------------------------------------------------------

#[tokio::test]
async fn spec_rset_048_grouped() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mut vals: Vec<Vals<Partner>> = F1.iter().map(|n| named(n)).collect();
    vals[0] = named(F1[0]).set(Partner::EMAIL, "@guest.com");
    vals[1] = named(F1[1]).set(Partner::EMAIL, "@host.com");
    vals[2] = named(F1[2]).set(Partner::EMAIL, "@guest.com");
    let partners = create(&mut tx, vals).await;
    let groups = partners.grouped(&mut tx, Partner::EMAIL).await.unwrap();
    let keys: Vec<Option<&str>> = groups.iter().map(|(k, _)| k.as_deref()).collect();
    assert_eq!(keys, [Some("@guest.com"), Some("@host.com"), None]);
    tx.invalidate_model("test_rset.partner", Some(&["name"]))
        .await
        .unwrap();
    let guest = &groups[0].1;
    let guest_names = assert_query_count!(tx, >= 1, <= 1, {
        guest.mapped(&mut tx, Partner::NAME).await.unwrap()
    });
    assert_eq!(
        guest_names.iter().map(|n| n.as_deref()).collect::<Vec<_>>(),
        [Some("Inner Works"), Some("AnalytIQ")]
    );
    let host = &groups[1].1;
    let host_name = assert_query_count!(tx, <= 0, {
        host.get(&mut tx, Partner::NAME).await.unwrap()
    });
    assert_eq!(host_name.as_deref(), Some("Pepper Street"));
    assert_eq!(groups[2].1.len(), 6);
    assert!(
        groups
            .iter()
            .all(|(_, g)| g.prefetch_ids() == partners.prefetch_ids())
    );

    let dynamic = partners
        .as_records()
        .grouped(&mut tx, "email")
        .await
        .unwrap();
    let keys: Vec<Value> = dynamic.iter().map(|(k, _)| k.clone()).collect();
    assert_eq!(
        keys,
        [
            Value::str("@guest.com"),
            Value::str("@host.com"),
            Value::False
        ]
    );
    assert_query_count!(tx, <= 0, {
        let empty = partners.browse([]).unwrap();
        assert!(empty.grouped(&mut tx, Partner::EMAIL).await.unwrap().is_empty());
    });
    let err = partners
        .as_records()
        .grouped(&mut tx, "country_id.name")
        .await
        .unwrap_err();
    assert_eq!(
        err.kind(),
        ErrorKind::Programming,
        "a dotted name is not a path"
    );
    let (p, q) = (partners.at(0).unwrap(), partners.at(2).unwrap());
    let pqp = &(&p + &q) + &p;
    let by_parity = pqp.grouped_by(|_| "same");
    assert_eq!(by_parity.len(), 1);
    assert_eq!(by_parity[0].1.rec_ids(), pqp.rec_ids(), "duplicates kept");
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- batch contracts ---------------------------------------------------------------------------

async fn every_model_harness() -> Harness {
    Harness::builder([
        &base_users::ADDON,
        &test_orm::ADDON,
        &test_search::ADDON,
        &test_rset::ADDON,
        &erp_test_models::test_compute::ADDON,
        &erp_test_models::test_inherit_a::ADDON,
        &erp_test_models::test_inherit_b::ADDON,
        &erp_test_models::test_inherit_c::ADDON,
    ])
    .with_static_decls(crate::currency_fixture::decls())
    .build()
    .await
    .expect("harness")
}

#[tokio::test]
async fn spec_rset_053_create_empty_on_every_model() {
    let h = every_model_harness().await;
    let mut tx = h.begin().await.unwrap();
    let models: Vec<&'static str> = h
        .registry()
        .models()
        .iter()
        .filter(|m| m.has_table())
        .map(|m| m.name)
        .collect();
    assert!(models.len() > 20, "{}", models.len());
    assert_query_count!(tx, <= 0, {
        for name in &models {
            let empty = tx.env().records(name).unwrap();
            let created = empty.create(&mut tx, Vec::new()).await.unwrap();
            assert!(created.is_empty(), "{name}");
            assert_eq!(created.model_name(), *name);
        }
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The `write` half (the `unlink` half arrives with slice S3).
#[tokio::test]
async fn empty_write_on_every_model() {
    let h = every_model_harness().await;
    let mut tx = h.begin().await.unwrap();
    let models: Vec<&'static str> = h
        .registry()
        .models()
        .iter()
        .filter(|m| m.has_table())
        .map(|m| m.name)
        .collect();
    assert_query_count!(tx, <= 0, {
        for name in &models {
            let empty = tx.env().records(name).unwrap();
            empty.write(&mut tx, RawVals::new()).await.unwrap();
        }
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- per-model overrides -----------------------------------------------------------------------

/// text id, has no counterpart: ids are integers, divergent)
#[tokio::test]
async fn spec_rset_062_per_model_overrides_of_exists_and_filtered_domain() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let m = tx.env().model::<Action>().unwrap();
    let actions = m
        .create(
            &mut tx,
            ["a1", "a2", "a3"]
                .iter()
                .map(|n| Vals::new().set(Action::NAME, *n))
                .collect(),
        )
        .await
        .unwrap();
    let ids = actions.ids();
    assert_eq!(actions.exists(&mut tx).await.unwrap().ids(), ids);
    delete_raw(&mut tx, "test_rset_action", ids[2]).await;
    let cached = assert_query_count!(tx, <= 0, { actions.exists(&mut tx).await.unwrap() });
    assert_eq!(cached.ids(), ids, "answered from the cached id set");
    tx.invalidate_ormcache(ACTION_IDS_CACHE);
    assert_eq!(actions.exists(&mut tx).await.unwrap().ids(), &ids[..2]);

    let lines = tx
        .env()
        .model::<Line>()
        .unwrap()
        .create(
            &mut tx,
            vec![
                Vals::new().set(Line::TAG, "A").set(Line::TAG_CODE, "B"),
                Vals::new().set(Line::TAG, "B").set(Line::TAG_CODE, "A"),
            ],
        )
        .await
        .unwrap();
    let found = lines
        .filtered_domain(&mut tx, &dom(json!([["tag", "=", "A"]])))
        .await
        .unwrap();
    assert_eq!(found.ids(), [lines.ids()[1]], "evaluated on tag_code");
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- budgets -----------------------------------------------------------------------------------

/// (`mapped('value')` over 1,280 records: ≤ 1) extended to the other in-memory
/// operations: at most one query when the values are not cached, none when they are.
#[tokio::test]
async fn in_memory_operations_over_1280_records_fetch_once() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rs = create(
        &mut tx,
        (0..1280)
            .map(|i| {
                named(&format!("p{:04}", (i * 7919) % 1280))
                    .set(Partner::NUMBER, i % 10)
                    .set(Partner::ACTIVE, i % 3 != 0)
            })
            .collect(),
    )
    .await;
    let domain = dom(json!(["|", ["number", ">", 5], ["name", "ilike", "p00"]]));
    for round in 0..2 {
        let budget: u64 = if round == 0 { 1 } else { 0 };
        let min: u64 = budget;
        if round == 0 {
            tx.invalidate_all().await.unwrap();
        }
        let values = assert_query_count!(tx, >= min, <= budget, {
            rs.mapped(&mut tx, Partner::NAME).await.unwrap()
        });
        assert_eq!(values.len(), 1280);
        if round == 0 {
            tx.invalidate_all().await.unwrap();
        }
        let sorted = assert_query_count!(tx, >= min, <= budget, {
            rs.sorted(&mut tx, Some("number desc, name"), false)
                .await
                .unwrap()
        });
        assert_eq!(sorted.len(), 1280);
        if round == 0 {
            tx.invalidate_all().await.unwrap();
        }
        let kept = assert_query_count!(tx, >= min, <= budget, {
            rs.filtered(&mut tx, Partner::ACTIVE, |a| *a).await.unwrap()
        });
        assert_eq!(kept.len(), 853);
        if round == 0 {
            tx.invalidate_all().await.unwrap();
        }
        let matched = assert_query_count!(tx, >= min, <= budget, {
            rs.filtered_domain(&mut tx, &domain).await.unwrap()
        });
        assert!(!matched.is_empty());
        if round == 0 {
            tx.invalidate_all().await.unwrap();
        }
        let groups = assert_query_count!(tx, >= min, <= budget, {
            rs.grouped(&mut tx, Partner::NUMBER).await.unwrap()
        });
        assert_eq!(groups.len(), 10);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_field_048_in_memory_filtering() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let mixed_model = tx.env().model::<test_orm::Mixed>().unwrap();
    let rec = mixed_model
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(test_orm::Mixed::CHAR, "Foo")
                    .set(test_orm::Mixed::HTML, "Foo\nBar"),
            ],
        )
        .await
        .unwrap();

    let d_like = dom(json!([["char", "like", "F"]]));
    let d_ilike = dom(json!([["char", "ilike", "f"]]));

    // Initially "Foo": matches both like 'F' and ilike 'f'
    assert_eq!(
        rec.filtered_domain(&mut tx, &d_like).await.unwrap().len(),
        1
    );
    assert_eq!(
        rec.filtered_domain(&mut tx, &d_ilike).await.unwrap().len(),
        1
    );

    // Set char to "Bar": no match for both
    rec.set_value(&mut tx, "char", Value::str("Bar"))
        .await
        .unwrap();
    assert_eq!(
        rec.filtered_domain(&mut tx, &d_like).await.unwrap().len(),
        0
    );
    assert_eq!(
        rec.filtered_domain(&mut tx, &d_ilike).await.unwrap().len(),
        0
    );

    // Set char to False: no match for both
    rec.set_value(&mut tx, "char", Value::False).await.unwrap();
    assert_eq!(
        rec.filtered_domain(&mut tx, &d_like).await.unwrap().len(),
        0
    );
    assert_eq!(
        rec.filtered_domain(&mut tx, &d_ilike).await.unwrap().len(),
        0
    );

    // html (Html field representing multiline html content) is "Foo\nBar": matches multiline like 'Bar' and ilike 'bar'
    let d_html_like = dom(json!([["html", "like", "Bar"]]));
    let d_html_ilike = dom(json!([["html", "ilike", "bar"]]));
    assert_eq!(
        rec.filtered_domain(&mut tx, &d_html_like)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        rec.filtered_domain(&mut tx, &d_html_ilike)
            .await
            .unwrap()
            .len(),
        1
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_054_empty_write_and_unlink_on_every_model() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let registry = tx.env().registry().clone();
    for spec in registry.models() {
        if spec.is_abstract() {
            continue;
        }
        let records = tx.env().records(spec.name).unwrap();
        // Model.browse().write({})
        let write_res = records.write(&mut tx, RawVals::new()).await;
        match write_res {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UserError => {}
            Err(e) => panic!("Model {} write({{}}) failed with: {e:?}", spec.name),
        }
        // Model.browse().unlink()
        let unlink_res = records.unlink(&mut tx).await;
        match unlink_res {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UserError => {}
            Err(e) => panic!("Model {} unlink() failed with: {e:?}", spec.name),
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_055_default_get_empty_on_every_model() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let registry = tx.env().registry().clone();
    for spec in registry.models() {
        if spec.kind == erp_registry::ModelKind::Transient || spec.is_abstract() {
            continue;
        }
        let records = tx.env().records(spec.name).unwrap();
        // Model.browse().default_get([])
        let defaults_res = records.default_get(&mut tx, &[]).await;
        match defaults_res {
            Ok(defaults) => {
                assert!(
                    defaults.is_empty(),
                    "Model {} default_get([]) returned non-empty: {defaults:?}",
                    spec.name
                );
            }
            Err(e) if e.kind() == ErrorKind::UserError => {}
            Err(e) => panic!("Model {} default_get([]) failed with: {e:?}", spec.name),
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_rset_056_multi_record_write_override_batch() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let m = tx.env().model::<test_orm::WriteOverride>().unwrap();

    // 1) partners.write({'active': False}) on batch of 3 records
    let r1 = m
        .create(
            &mut tx,
            vec![
                Vals::new().set(test_orm::WriteOverride::PRIORITY, 10),
                Vals::new().set(test_orm::WriteOverride::PRIORITY, 20),
                Vals::new().set(test_orm::WriteOverride::PRIORITY, 30),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();

    test_orm::WRITE_BATCH_SIZES.lock().unwrap().clear();
    r1.write(
        &mut tx,
        Vals::new().set(test_orm::WriteOverride::ACTIVE, false),
    )
    .await
    .unwrap();
    tx.flush().await.unwrap();

    assert_eq!(*test_orm::WRITE_BATCH_SIZES.lock().unwrap(), vec![3]);
    for p in r1.iter() {
        assert!(
            !p.get(&mut tx, test_orm::WriteOverride::ACTIVE)
                .await
                .unwrap()
        );
    }

    // 2) for p in partners: p.write({'active': False})
    let r2 = m
        .create(
            &mut tx,
            vec![
                Vals::new().set(test_orm::WriteOverride::PRIORITY, 10),
                Vals::new().set(test_orm::WriteOverride::PRIORITY, 20),
                Vals::new().set(test_orm::WriteOverride::PRIORITY, 30),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();

    test_orm::WRITE_BATCH_SIZES.lock().unwrap().clear();
    for p in r2.iter() {
        p.write(
            &mut tx,
            Vals::new().set(test_orm::WriteOverride::ACTIVE, false),
        )
        .await
        .unwrap();
    }
    tx.flush().await.unwrap();

    assert_eq!(*test_orm::WRITE_BATCH_SIZES.lock().unwrap(), vec![1, 1, 1]);
    for p in r2.iter() {
        assert!(
            !p.get(&mut tx, test_orm::WriteOverride::ACTIVE)
                .await
                .unwrap()
        );
    }

    // 3) partners.active = False via typed set() on recordset
    let r3 = m
        .create(
            &mut tx,
            vec![
                Vals::new().set(test_orm::WriteOverride::PRIORITY, 10),
                Vals::new().set(test_orm::WriteOverride::PRIORITY, 20),
                Vals::new().set(test_orm::WriteOverride::PRIORITY, 30),
            ],
        )
        .await
        .unwrap();
    tx.flush().await.unwrap();

    test_orm::WRITE_BATCH_SIZES.lock().unwrap().clear();
    r3.set(&mut tx, test_orm::WriteOverride::ACTIVE, false)
        .await
        .unwrap();
    tx.flush().await.unwrap();

    assert_eq!(*test_orm::WRITE_BATCH_SIZES.lock().unwrap(), vec![3]);
    for p in r3.iter() {
        assert!(
            !p.get(&mut tx, test_orm::WriteOverride::ACTIVE)
                .await
                .unwrap()
        );
    }

    drop(tx);
    h.cleanup().await.unwrap();
}
