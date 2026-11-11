//! The search memo (predicate text per shape) and the access-domain hook
//! of searches, fetches and reads.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use erp_registry::ModelSpec;
use erp_sdk::serde_json::json;
use erp_sdk::{
    Domain, ErrorKind, Operation, Operator, OrmConfig, Result, SearchOptions, Transaction, Value,
};
use erp_test_models::{base_users, test_orm, test_search};
use erp_testkit::{Harness, assert_query_count};
use support::{dom, map};

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Vec<i64> {
    let empty = tx.env().records(model).unwrap();
    let values: Vec<_> = rows.iter().map(|r| map(r)).collect();
    empty.create_values(tx, &values).await.unwrap().ids()
}

async fn find(tx: &mut Transaction, domain: Domain) -> Vec<i64> {
    let partners = tx.env().records("test_search.partner").unwrap();
    partners.search(tx, &domain).await.unwrap().ids()
}

/// One shape, one text: repeated searches and searches that differ only in literals (including
/// relative dates) hit the memo and send the same statement text; another transaction shares it.
#[tokio::test]
async fn memo_hit_rate_and_statement_reuse() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        "test_search.partner",
        &[&[("name", Value::str("a"))], &[("name", Value::str("b"))]],
    )
    .await;
    let before = h.orm().search_memo_stats();
    tx.capture_statements(true);
    for _ in 0..10 {
        assert_eq!(
            find(&mut tx, dom(json!([["name", "=", "a"]]))).await.len(),
            1
        );
    }
    let after = h.orm().search_memo_stats();
    assert_eq!(after.misses - before.misses, 1, "{after:?}");
    assert_eq!(after.hits - before.hits, 9, "{after:?}");
    let first = tx.captured().last().cloned().unwrap();
    assert_eq!(
        find(&mut tx, dom(json!([["name", "=", "b"]]))).await.len(),
        1
    );
    let stats = h.orm().search_memo_stats();
    assert_eq!(
        stats.misses - before.misses,
        1,
        "a new literal is the same shape"
    );
    assert_eq!(
        tx.captured().last().unwrap(),
        &first,
        "same shape, same statement text: literals are parameters"
    );
    tx.commit().await.unwrap();
    let mut tx2 = h.begin().await.unwrap();
    find(&mut tx2, dom(json!([["name", "=", "a"]]))).await;
    assert_eq!(h.orm().search_memo_stats().hits - before.hits, 11);
    // relative dates are resolved on every call and still share the shape's text
    let mixed = tx2.env().records("test_search.mixed").unwrap();
    let relative = dom(json!([["date", ">", "today"]]));
    let stats = h.orm().search_memo_stats();
    for _ in 0..3 {
        mixed.search(&mut tx2, &relative).await.unwrap();
    }
    let later = h.orm().search_memo_stats();
    assert_eq!(later.misses - stats.misses, 1);
    assert_eq!(later.hits - stats.hits, 2);
    drop(tx2);
    h.cleanup().await.unwrap();
}

/// S2 review item 2: `1`, `1.0` and `True` share an entry with their own parameters; decisions
/// that depend on the literal (`< 1` accepts NULL rows, `< -1` does not) get their own entries, so
/// a hit never returns the wrong text.
#[tokio::test]
async fn memo_numeric_literals_never_collide() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let ids = create(
        &mut tx,
        "test_search.partner",
        &[
            &[("number", Value::Int(1))],
            &[("number", Value::Int(0))],
            &[],
            &[("number", Value::Int(-5))],
        ],
    )
    .await;
    let before = h.orm().search_memo_stats();
    for literal in [json!(1), json!(1.0), json!(true)] {
        assert_eq!(
            find(&mut tx, dom(json!([["number", "=", literal]]))).await,
            [ids[0]],
            "{literal}"
        );
    }
    let stats = h.orm().search_memo_stats();
    // ids come back in id order; rows inserted in separate column groups get ids out of input order
    let mut below_one = vec![ids[1], ids[2], ids[3]];
    below_one.sort_unstable();
    assert_eq!(
        (stats.misses - before.misses, stats.hits - before.hits),
        (1, 2)
    );
    // `< 1` includes the NULL and 0 rows, `< -1` does not include them
    assert_eq!(
        find(&mut tx, dom(json!([["number", "<", 1]]))).await,
        below_one
    );
    assert_eq!(
        find(&mut tx, dom(json!([["number", "<", -1]]))).await,
        [ids[3]]
    );
    assert_eq!(
        find(&mut tx, dom(json!([["number", "<", 1]]))).await,
        below_one,
        "the earlier entry is still right after the other shape"
    );
    let stats2 = h.orm().search_memo_stats();
    assert_eq!(stats2.misses - stats.misses, 2, "two shapes");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// S2 review item 2: the memo stays within its capacity and keeps the entries in use.
#[tokio::test]
async fn memo_eviction_is_bounded_and_keeps_hot_entries() {
    let h = Harness::builder([&base_users::ADDON, &test_search::ADDON])
        .config(OrmConfig {
            search_memo_size: 4,
            ..OrmConfig::default()
        })
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let hot = dom(json!([["name", "=", "hot"]]));
    find(&mut tx, hot.clone()).await;
    let shapes = [
        json!([["email", "=", "x"]]),
        json!([["website", "=", "x"]]),
        json!([["number", ">", 1]]),
        json!([["number", "<", -1]]),
        json!([["email", "like", "x"]]),
        json!([["website", "ilike", "x"]]),
        json!([["number", ">=", 3]]),
        json!([["email", "!=", false]]),
        json!([["website", "=", false]]),
        json!([["name", "like", "x"]]),
    ];
    for shape in shapes {
        find(&mut tx, dom(shape)).await;
        let before = h.orm().search_memo_stats();
        find(&mut tx, hot.clone()).await;
        let after = h.orm().search_memo_stats();
        assert!(after.entries <= 4, "{after:?}");
        assert_eq!(after.hits - before.hits, 1, "hot entry evicted: {after:?}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// S2 review item 8: a connection keeps at most `statement_cache_size` prepared statements, however
/// many order specifications clients send.
#[tokio::test]
async fn statement_cache_is_bounded() {
    let h = Harness::builder([&base_users::ADDON, &test_search::ADDON])
        .config(OrmConfig {
            statement_cache_size: 4,
            ..OrmConfig::default()
        })
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    for order in [
        "name",
        "email",
        "website",
        "number",
        "name desc",
        "email desc",
        "number desc",
    ] {
        partners
            .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order(order))
            .await
            .unwrap();
        assert!(
            tx.prepared_statements() <= 4,
            "{}",
            tx.prepared_statements()
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Record rules for the hook tests: partners with `number > 20`, no country, everything else.
struct NumberRule;

impl erp_testkit::TestRules for NumberRule {
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
            "test_search.partner" => Domain::condition("number", Operator::Gt, 20).unwrap(),
            "test_search.country" => Domain::FALSE,
            _ => Domain::TRUE,
        })
    }
}

async fn rule_harness() -> (Harness, Vec<i64>) {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON, &test_search::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .rules(NumberRule)
        .build()
        .await
        .unwrap();
    let mut su = h.begin().await.unwrap();
    // the user the record checks run for (its name goes into the access evidence)
    su.batch("INSERT INTO res_users (id, name, login, active) VALUES (2, 'demo', 'demo', true)")
        .await
        .unwrap();
    let ids = create(
        &mut su,
        "test_search.partner",
        &[
            &[("number", Value::Int(10)), ("name", Value::str("hidden"))],
            &[("number", Value::Int(30)), ("name", Value::str("thirty"))],
            &[("number", Value::Int(50)), ("name", Value::str("fifty"))],
        ],
    )
    .await;
    create(
        &mut su,
        "test_search.country",
        &[&[("name", Value::str("BE"))]],
    )
    .await;
    su.commit().await.unwrap();
    (h, ids)
}

/// the policy's access domain is ANDed into searches and fetches, in SQL, outside
/// superuser mode; a `FALSE` access domain runs no query; hidden existing records are an
/// `Access` error for `fetch` and for field reads.
#[tokio::test]
async fn access_domain_is_evaluated_in_sql() {
    let (h, ids) = rule_harness().await;
    let mut su = h.begin().await.unwrap();
    let all = su.env().records("test_search.partner").unwrap();
    assert_eq!(all.search(&mut su, &Domain::TRUE).await.unwrap().ids(), ids);

    let mut tx = h.begin_as(2).await.unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    tx.capture_statements(true);
    let visible = partners.search(&mut tx, &Domain::TRUE).await.unwrap();
    assert_eq!(visible.ids(), &ids[1..]);
    let sql = tx.captured().last().cloned().unwrap();
    assert!(
        sql.contains("\"test_search_partner\".\"number\" > $1::int8"),
        "{sql}"
    );
    assert_eq!(
        partners
            .search_count(&mut tx, &Domain::TRUE, None)
            .await
            .unwrap(),
        2
    );
    let below = dom(json!([["number", "<", 40]]));
    assert_eq!(
        partners
            .search_with(&mut tx, &below, &SearchOptions::new())
            .await
            .unwrap()
            .ids(),
        [ids[1]]
    );
    let countries = tx.env().records("test_search.country").unwrap();
    assert_query_count!(tx, <= 0, {
        assert!(
            countries
                .search(&mut tx, &Domain::TRUE)
                .await
                .unwrap()
                .is_empty()
        );
    });
    partners
        .browse([ids[2]])
        .unwrap()
        .fetch(&mut tx, Some(&["number"]))
        .await
        .unwrap();
    let err = partners
        .browse([ids[0], ids[2]])
        .unwrap()
        .fetch(&mut tx, Some(&["name"]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    drop(tx);
    // field reads (cache misses through the prefetch set) apply the same rule
    let mut tx = h.begin_as(2).await.unwrap();
    let both = tx
        .env()
        .records("test_search.partner")
        .unwrap()
        .browse([ids[0], ids[2]])
        .unwrap();
    tx.capture_statements(true);
    assert_eq!(
        both.at(1).unwrap().value(&mut tx, "number").await.unwrap(),
        Value::Int(50)
    );
    assert!(
        tx.captured()[0].contains("\"number\" > $1::int8"),
        "{:?}",
        tx.captured()
    );
    assert!(!tx.is_cached(&both.at(0).unwrap(), "number").unwrap());
    let err = both
        .at(0)
        .unwrap()
        .value(&mut tx, "number")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    drop(tx);
    drop(su);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn record_rules_apply_on_cache_hits() {
    let (h, ids) = rule_harness().await;
    let mut tx = h.begin_as(2).await.unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    // a superuser search fills the cache of every partner, the hidden one included
    let everything = partners
        .sudo(true)
        .search_with(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().fetch_prefetchable(),
        )
        .await
        .unwrap();
    assert_eq!(everything.ids(), ids);
    let hidden = partners.browse([ids[0]]).unwrap();
    assert!(tx.is_cached(&hidden, "number").unwrap());
    for fields in [&["number"][..], &["id"][..], &["display_name"][..]] {
        let err = assert_query_count!(tx, >= 1, <= 2, {
            hidden.read(&mut tx, fields).await.unwrap_err()
        });
        assert_eq!(err.kind(), ErrorKind::Access, "{fields:?}");
    }
    let visible = partners.browse([ids[1]]).unwrap();
    let rows = assert_query_count!(tx, >= 1, <= 1, {
        visible.read(&mut tx, &["number"]).await.unwrap()
    });
    assert_eq!(rows[0]["number"], Value::Int(30));
    // superuser: no rule, no query
    assert_query_count!(tx, <= 0, {
        hidden.sudo(true).read(&mut tx, &["number"]).await.unwrap();
    });
    drop(tx);
    h.cleanup().await.unwrap();
}
