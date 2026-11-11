//! The bounded single-record x2many read of data expressions (`Records::read_x2many_capped`,
//! review blocker EXPR-CLOSE-1): an accepted value equals what
//! `read_with(Load::Raw)` returns for the field in the same environment (field domain, active
//! test, order), and a value over the cap is refused after at most `cap + 1` rows were read,
//! without caching a partial value.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use erp_orm::CappedIds;
use erp_sdk::{ErrorKind, Load, Records, Transaction, Value};
use erp_test_models::{base_users, test_o2m_order};
use erp_testkit::{Harness, assert_query_count};
use support::map;

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap().sudo(true);
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

/// The raw ids `read_with(Load::Raw)` gives for x2many `field` of the singleton `record`.
async fn read_ids(tx: &mut Transaction, record: &Records, field: &str) -> Vec<i64> {
    let rows = record.read_with(tx, &[field], Load::Raw).await.unwrap();
    assert_eq!(rows.len(), 1);
    match rows[0].get(field) {
        Some(Value::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::Int(i) => *i,
                other => panic!("not an id: {other:?}"),
            })
            .collect(),
        other => panic!("not a list: {other:?}"),
    }
}

/// The accepted ids of a capped read, failing on an over-cap answer.
fn within(found: CappedIds) -> Vec<i64> {
    match found {
        CappedIds::Within(ids) => ids,
        CappedIds::OverCap => panic!("expected a value within the cap"),
    }
}

/// How many of `records` have `field` in the cache.
fn cached_count(tx: &Transaction, records: &Records, field: &str) -> usize {
    records
        .ids()
        .into_iter()
        .filter(|id| {
            tx.is_cached(&records.browse([*id]).unwrap(), field)
                .unwrap()
        })
        .count()
}

/// A parent `test_rel.active` with `active` children and `archived` archived ones (in that
/// creation order) as children (one2many `children_ids`) and relatives (many2many
/// `relatives_ids`), named `{prefix}{n}`.
async fn active_family(
    tx: &mut Transaction,
    active: usize,
    archived: usize,
    prefix: &str,
) -> (Records, Records) {
    let parent = create(tx, "test_rel.active", &[&[("name", Value::str("P"))]]).await;
    let pid = parent.ids()[0];
    let rows: Vec<Vec<(&str, Value)>> = (0..active + archived)
        .map(|n| {
            vec![
                ("name", Value::str(format!("{prefix}{n}"))),
                ("parent_id", Value::Int(pid)),
                ("active", Value::Bool(n < active)),
            ]
        })
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let kids = create(tx, "test_rel.active", &refs).await;
    parent
        .set_value(
            tx,
            "relatives_ids",
            Value::list(kids.ids().into_iter().map(Value::Int)),
        )
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    (parent, kids)
}

#[tokio::test]
async fn capped_one2many_at_the_cap_equals_read_with_and_one_more_is_over_cap() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    // three active children and one archived: the active test leaves exactly the cap
    let (parent, _) = active_family(&mut tx, 3, 1, "c").await;
    let found = within(
        parent
            .read_x2many_capped(&mut tx, "children_ids", 3)
            .await
            .unwrap(),
    );
    tx.invalidate_all().await.unwrap();
    let expected = read_ids(&mut tx, &parent, "children_ids").await;
    assert_eq!(expected.len(), 3);
    assert_eq!(found, expected);
    // without the active test the archived child counts: four rows, over a cap of three
    tx.invalidate_all().await.unwrap();
    let all = parent.with_context([("active_test", Value::Bool(false))]);
    assert!(matches!(
        all.read_x2many_capped(&mut tx, "children_ids", 3)
            .await
            .unwrap(),
        CappedIds::OverCap
    ));
    tx.invalidate_all().await.unwrap();
    let four = within(
        all.read_x2many_capped(&mut tx, "children_ids", 4)
            .await
            .unwrap(),
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(four, read_ids(&mut tx, &all, "children_ids").await);
    assert_eq!(four.len(), 4);
    // the field's own context wins over the reader's (`all_children_ids`: active_test false)
    tx.invalidate_all().await.unwrap();
    assert!(matches!(
        parent
            .read_x2many_capped(&mut tx, "all_children_ids", 3)
            .await
            .unwrap(),
        CappedIds::OverCap
    ));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn capped_many2many_at_the_cap_equals_read_with_and_one_more_is_over_cap() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let (parent, _) = active_family(&mut tx, 3, 1, "r").await;
    let found = within(
        parent
            .read_x2many_capped(&mut tx, "relatives_ids", 3)
            .await
            .unwrap(),
    );
    tx.invalidate_all().await.unwrap();
    let expected = read_ids(&mut tx, &parent, "relatives_ids").await;
    assert_eq!(expected.len(), 3);
    assert_eq!(found, expected);
    tx.invalidate_all().await.unwrap();
    let all = parent.with_context([("active_test", Value::Bool(false))]);
    assert!(matches!(
        all.read_x2many_capped(&mut tx, "relatives_ids", 3)
            .await
            .unwrap(),
        CappedIds::OverCap
    ));
    tx.invalidate_all().await.unwrap();
    let four = within(
        all.read_x2many_capped(&mut tx, "relatives_ids", 4)
            .await
            .unwrap(),
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(four, read_ids(&mut tx, &all, "relatives_ids").await);
    assert_eq!(four.len(), 4);
    tx.invalidate_all().await.unwrap();
    assert!(matches!(
        parent
            .read_x2many_capped(&mut tx, "all_relatives_ids", 3)
            .await
            .unwrap(),
        CappedIds::OverCap
    ));
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn capped_reads_count_only_the_rows_the_field_domain_keeps() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    // relatives a0..a2 match `a_relatives_ids`' domain (`name =ilike 'a%'`); b0..b2 do not
    let (parent, _) = active_family(&mut tx, 3, 0, "a").await;
    let others = create(
        &mut tx,
        "test_rel.active",
        &[
            &[("name", Value::str("b0"))],
            &[("name", Value::str("b1"))],
            &[("name", Value::str("b2"))],
        ],
    )
    .await;
    let mut all: Vec<i64> = read_ids(&mut tx, &parent, "relatives_ids").await;
    all.extend(others.ids());
    parent
        .set_value(
            &mut tx,
            "relatives_ids",
            Value::list(all.iter().copied().map(Value::Int)),
        )
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    let found = within(
        parent
            .read_x2many_capped(&mut tx, "a_relatives_ids", 3)
            .await
            .unwrap(),
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(found, read_ids(&mut tx, &parent, "a_relatives_ids").await);
    assert_eq!(found.len(), 3);
    tx.invalidate_all().await.unwrap();
    assert!(matches!(
        parent
            .read_x2many_capped(&mut tx, "relatives_ids", 3)
            .await
            .unwrap(),
        CappedIds::OverCap
    ));

    // a one2many domain (`important_messages`: important = true)
    let disc = create(
        &mut tx,
        "test_rel.discussion",
        &[&[("name", Value::str("D"))]],
    )
    .await;
    let did = disc.ids()[0];
    let rows: Vec<Vec<(&str, Value)>> = (0..6)
        .map(|n| {
            vec![
                ("discussion", Value::Int(did)),
                ("body", Value::str(format!("m{n}"))),
                ("important", Value::Bool(n % 2 == 0)),
            ]
        })
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, "test_rel.message", &refs).await;
    tx.invalidate_all().await.unwrap();
    let important = within(
        disc.read_x2many_capped(&mut tx, "important_messages", 3)
            .await
            .unwrap(),
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        important,
        read_ids(&mut tx, &disc, "important_messages").await
    );
    assert_eq!(important.len(), 3);
    tx.invalidate_all().await.unwrap();
    assert!(matches!(
        disc.read_x2many_capped(&mut tx, "messages", 3)
            .await
            .unwrap(),
        CappedIds::OverCap
    ));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Ten related rows, a cap of three: one bounded statement, at most four line records reach the
/// cache, and the owner's value is not cached (a fully read and then rejected value would have
/// cached ten lines and the owner's cell).
#[tokio::test]
async fn an_over_cap_value_is_refused_after_cap_plus_one_rows_and_is_not_cached() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let (parent, kids) = active_family(&mut tx, 10, 0, "k").await;

    // one2many
    tx.capture_statements(true);
    let over = assert_query_count!(tx, >= 1, <= 1, {
        parent
            .read_x2many_capped(&mut tx, "children_ids", 3)
            .await
            .unwrap()
    });
    assert!(matches!(over, CappedIds::OverCap));
    let sql = tx.captured().last().unwrap().clone();
    assert!(sql.contains("LIMIT"), "the line read is bounded: {sql}");
    assert!(
        !tx.is_cached(&parent, "children_ids").unwrap(),
        "no partial value is cached"
    );
    let lines = cached_count(&tx, &kids, "active");
    assert!(
        lines <= 4,
        "at most cap + 1 lines were read, {lines} are cached"
    );

    // many2many
    tx.invalidate_all().await.unwrap();
    let over = assert_query_count!(tx, >= 1, <= 1, {
        parent
            .read_x2many_capped(&mut tx, "relatives_ids", 3)
            .await
            .unwrap()
    });
    assert!(matches!(over, CappedIds::OverCap));
    let sql = tx.captured().last().unwrap().clone();
    assert!(sql.contains("LIMIT"), "the relation read is bounded: {sql}");
    assert!(
        !tx.is_cached(&parent, "relatives_ids").unwrap(),
        "no partial value is cached"
    );
    let targets = cached_count(&tx, &kids, "active");
    assert!(
        targets <= 4,
        "at most cap + 1 targets were read, {targets} are cached"
    );
    tx.capture_statements(false);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A value already in the cache is answered from it (no query), as the normal read would, and a
/// cached value over the cap is refused too.
#[tokio::test]
async fn a_cached_value_is_answered_from_the_cache() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let (parent, _) = active_family(&mut tx, 3, 1, "c").await;
    let expected = read_ids(&mut tx, &parent, "children_ids").await;
    assert!(tx.is_cached(&parent, "children_ids").unwrap());
    let found = assert_query_count!(tx, <= 0, {
        parent
            .read_x2many_capped(&mut tx, "children_ids", 3)
            .await
            .unwrap()
    });
    assert_eq!(within(found), expected);
    let over = parent
        .read_x2many_capped(&mut tx, "children_ids", 2)
        .await
        .unwrap();
    assert!(matches!(over, CappedIds::OverCap));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A pending (unflushed) write of the lines' inverse is seen as the normal read sees it.
#[tokio::test]
async fn a_pending_inverse_write_is_seen() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let (parent, kids) = active_family(&mut tx, 2, 0, "c").await;
    let other = create(&mut tx, "test_rel.active", &[&[("name", Value::str("O"))]]).await;
    tx.invalidate_all().await.unwrap();
    let moved = kids.browse([kids.ids()[0]]).unwrap();
    moved
        .set_value(&mut tx, "parent_id", Value::Int(other.ids()[0]))
        .await
        .unwrap();
    let found = within(
        parent
            .read_x2many_capped(&mut tx, "children_ids", 3)
            .await
            .unwrap(),
    );
    assert_eq!(found, [kids.ids()[1]]);
    let theirs = within(
        other
            .read_x2many_capped(&mut tx, "children_ids", 3)
            .await
            .unwrap(),
    );
    assert_eq!(theirs, [kids.ids()[0]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The comodel order (`sequence, id` lines; `name, id` tags), not the creation order.
#[tokio::test]
async fn capped_reads_keep_the_comodel_order() {
    let h = Harness::builder([&base_users::ADDON, &test_o2m_order::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let owner = create(
        &mut tx,
        "test_o2m_order.owner",
        &[&[("name", Value::str("O"))]],
    )
    .await;
    let oid = owner.ids()[0];
    create(
        &mut tx,
        "test_o2m_order.seq",
        &[
            &[("owner_id", Value::Int(oid)), ("sequence", Value::Int(3))],
            &[("owner_id", Value::Int(oid)), ("sequence", Value::Int(1))],
            &[("owner_id", Value::Int(oid)), ("sequence", Value::Int(2))],
        ],
    )
    .await;
    let tags = create(
        &mut tx,
        "test_o2m_order.tag",
        &[
            &[("name", Value::str("c"))],
            &[("name", Value::str("a"))],
            &[("name", Value::str("b"))],
        ],
    )
    .await;
    owner
        .set_value(
            &mut tx,
            "tag_ids",
            Value::list(tags.ids().into_iter().map(Value::Int)),
        )
        .await
        .unwrap();
    for field in ["seq_ids", "tag_ids"] {
        tx.invalidate_all().await.unwrap();
        let found = within(owner.read_x2many_capped(&mut tx, field, 3).await.unwrap());
        tx.invalidate_all().await.unwrap();
        let expected = read_ids(&mut tx, &owner, field).await;
        let mut sorted = expected.clone();
        sorted.sort_unstable();
        assert_ne!(
            expected, sorted,
            "{field}: the fixture's order is not the id order"
        );
        assert_eq!(found, expected, "{field}");
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_capped_read_refuses_a_non_superuser_environment_and_a_non_singleton() {
    let h = support::rel_orm().await;
    let mut tx = h.begin().await.unwrap();
    let (parent, kids) = active_family(&mut tx, 1, 0, "c").await;
    let user = parent.with_user(2);
    assert!(!user.env().su());
    let err = user
        .read_x2many_capped(&mut tx, "children_ids", 3)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access);
    assert_eq!(
        err.message(),
        "Records::read_x2many_capped: only a superuser-mode environment may read a capped \
         x2many value (this one acts as user 2)"
    );
    let both = parent.browse([parent.ids()[0], kids.ids()[0]]).unwrap();
    let err = both
        .read_x2many_capped(&mut tx, "children_ids", 3)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("Records::read_x2many_capped: expected a singleton, got {both}")
    );
    let err = parent
        .read_x2many_capped(&mut tx, "name", 3)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Records::read_x2many_capped: test_rel.active.name is not a one2many or many2many field"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
