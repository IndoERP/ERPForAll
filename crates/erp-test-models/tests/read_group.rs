//! GRP cases of slice S5, grouped reads: `_read_group`, `read_group`, `_read_grouping_sets`
//! (fixtures A, T and F of the GRP spec).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_registry::ModelSpec;
use erp_sdk::serde_json::json;
use erp_sdk::{
    Date, DateTime, Decimal, Domain, ErrorKind, GroupValue, Groups, Operation, Operator, ReadGroup,
    ReadGroupingSets, Records, RecordsAs, Result, Transaction, Value,
};
use erp_test_models::{base_users, test_read_group};
use erp_testkit::{Harness, assert_query_count};
use support::{assert_sql, dom, map};

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_read_group::ADDON])
        .build()
        .await
        .expect("harness")
}

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    let created = records.create_values(tx, &vals).await.unwrap();
    tx.flush().await.unwrap();
    created
}

fn id(records: &Records, i: usize) -> i64 {
    records.ids()[i]
}

fn int(i: i64) -> Value {
    Value::Int(i)
}

fn s(text: &str) -> Value {
    Value::str(text)
}

const F: Value = Value::False;

/// Tuples with relational group values as ids and recordset aggregates as id lists.
fn flat(groups: &Groups) -> Vec<Vec<Value>> {
    let n = groups.groupby().len();
    groups
        .rows()
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, v)| v.to_value(if i < n { RecordsAs::Id } else { RecordsAs::Ids }))
                .collect()
        })
        .collect()
}

async fn group(tx: &mut Transaction, model: &str, rg: ReadGroup) -> Vec<Vec<Value>> {
    let records = tx.env().records(model).unwrap();
    let groups = records._read_group(tx, rg).await.unwrap();
    flat(&groups)
}

fn rg(groupby: &[&str], aggregates: &[&str]) -> ReadGroup {
    ReadGroup::new(Domain::TRUE)
        .groupby(groupby.iter().copied())
        .aggregates(aggregates.iter().copied())
}

const AGG: &str = "test_read_group.aggregate";
const PT: &str = "test_read_group.partner";

/// Fixture A: partners p1 `z_one` and p2 `a_two`, and the nine aggregate rows.
async fn fixture_a(tx: &mut Transaction) -> (i64, i64) {
    let partners = create(tx, PT, &[&[("name", s("z_one"))], &[("name", s("a_two"))]]).await;
    let (p1, p2) = (id(&partners, 0), id(&partners, 1));
    let rows: Vec<Vec<(&str, Value)>> = [
        (Some(1), Some(p1), Some(1)),
        (Some(1), Some(p1), Some(2)),
        (Some(1), Some(p2), Some(3)),
        (Some(2), Some(p2), Some(4)),
        (Some(2), Some(p2), None),
        (Some(2), None, Some(5)),
        (None, Some(p2), Some(5)),
        (None, None, Some(6)),
        (None, None, None),
    ]
    .into_iter()
    .map(|(k, p, v)| {
        let mut row = Vec::new();
        if let Some(k) = k {
            row.push(("key", int(k)));
        }
        if let Some(p) = p {
            row.push(("partner_id", int(p)));
        }
        if let Some(v) = v {
            row.push(("value", int(v)));
        }
        row
    })
    .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(tx, AGG, &refs).await;
    (p1, p2)
}

#[tokio::test]
async fn spec_grp_001_basic_grouping_and_default_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    fixture_a(&mut tx).await;
    tx.capture_statements(true);
    let got = assert_query_count!(tx, >= 1, <= 1, {
        group(&mut tx, AGG, rg(&["key"], &["value:sum"])).await
    });
    assert_eq!(
        got,
        vec![vec![int(1), int(6)], vec![int(2), int(9)], vec![F, int(11)]]
    );
    assert_sql(
        tx.captured().last().unwrap(),
        r#"SELECT "test_read_group_aggregate"."key", SUM("test_read_group_aggregate"."value")
           FROM "test_read_group_aggregate"
           GROUP BY "test_read_group_aggregate"."key"
           ORDER BY "test_read_group_aggregate"."key" ASC"#,
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_002_many2one_groups_default_and_explicit_order() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (p1, p2) = fixture_a(&mut tx).await;
    tx.capture_statements(true);
    // default: raw partner id order, no join
    let got = group(&mut tx, AGG, rg(&["key", "partner_id"], &["value:sum"])).await;
    assert_eq!(
        got,
        vec![
            vec![int(1), int(p1), int(3)],
            vec![int(1), int(p2), int(3)],
            vec![int(2), int(p2), int(4)],
            vec![int(2), F, int(5)],
            vec![F, int(p2), int(5)],
            vec![F, F, int(6)],
        ]
    );
    assert!(!tx.captured().last().unwrap().contains("JOIN"));
    // explicit: the comodel's order (name ASC, id DESC), joined around the rows grouped by the
    // ids alone (review item 4: no comodel columns in the GROUP BY)
    let got = group(
        &mut tx,
        AGG,
        rg(&["key", "partner_id"], &["value:sum"]).order("key, partner_id"),
    )
    .await;
    assert_eq!(
        got,
        vec![
            vec![int(1), int(p2), int(3)],
            vec![int(1), int(p1), int(3)],
            vec![int(2), int(p2), int(4)],
            vec![int(2), F, int(5)],
            vec![F, int(p2), int(5)],
            vec![F, F, int(6)],
        ]
    );
    assert_sql(
        tx.captured().last().unwrap(),
        r#"SELECT "__g"."c0", "__g"."c1", "__g"."c2"
           FROM (SELECT "test_read_group_aggregate"."key" AS "c0",
                        "test_read_group_aggregate"."partner_id" AS "c1",
                        SUM("test_read_group_aggregate"."value") AS "c2"
                 FROM "test_read_group_aggregate"
                 GROUP BY "test_read_group_aggregate"."key", "test_read_group_aggregate"."partner_id")
                AS "__g"
           LEFT JOIN "test_read_group_partner" AS "__g__o1" ON ("__g"."c1" = "__g__o1"."id")
           ORDER BY "__g"."c0" ASC, "__g__o1"."name" ASC, "__g__o1"."id" DESC"#,
    );
    // DESC reverses the inner terms
    let got = group(
        &mut tx,
        AGG,
        rg(&["partner_id"], &["__count"]).order("partner_id DESC"),
    )
    .await;
    assert_eq!(
        got,
        vec![
            vec![F, int(3)],
            vec![int(p1), int(2)],
            vec![int(p2), int(4)]
        ]
    );
    assert!(
        tx.captured()
            .last()
            .unwrap()
            .contains(r#""__g__o0"."name" DESC, "__g__o0"."id" ASC"#)
    );
    // group values are recordsets; the empty group is an empty recordset
    let records = tx.env().records(AGG).unwrap();
    let groups = records
        ._read_group(&mut tx, rg(&["partner_id"], &[]))
        .await
        .unwrap();
    let first = groups.column(0)[0].as_records().unwrap();
    assert_eq!(first.model_name(), PT);
    assert_eq!(first.ids(), [p1]);
    assert!(groups.column(0)[2].as_records().unwrap().is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_003_limit_and_offset_on_groups() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = [
        (Some(1), Some(1)),
        (Some(1), Some(2)),
        (Some(1), Some(3)),
        (Some(2), Some(4)),
        (Some(2), None),
        (Some(2), Some(5)),
        (None, None),
        (None, Some(6)),
    ]
    .into_iter()
    .map(|(k, v)| {
        let mut row = Vec::new();
        if let Some(k) = k {
            row.push(("key", int(k)));
        }
        if let Some(v) = v {
            row.push(("value", int(v)));
        }
        row
    })
    .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, AGG, &refs).await;
    let base = || rg(&["key"], &["value:sum"]);
    assert_eq!(
        group(&mut tx, AGG, base().limit(2)).await,
        vec![vec![int(1), int(6)], vec![int(2), int(9)]]
    );
    assert_eq!(
        group(&mut tx, AGG, base().offset(1)).await,
        vec![vec![int(2), int(9)], vec![F, int(6)]]
    );
    assert_eq!(
        group(&mut tx, AGG, base().offset(1).limit(2).order("key DESC")).await,
        vec![vec![int(2), int(9)], vec![int(1), int(6)]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_004_empty_domain_runs_no_query() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    fixture_a(&mut tx).await;
    let empty = || ReadGroup::new(dom(json!([["id", "in", []]])));
    let got = assert_query_count!(tx, <= 0, {
        group(&mut tx, AGG, empty().groupby(["partner_id"])).await
    });
    assert!(got.is_empty());
    let got = assert_query_count!(tx, <= 0, {
        group(
            &mut tx,
            AGG,
            empty().aggregates(["__count", "partner_id:count", "partner_id:count_distinct"]),
        )
        .await
    });
    assert_eq!(got, vec![vec![int(0), int(0), int(0)]]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_005_aggregate_functions() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let t = Value::Bool(true);
    let f = Value::Bool(false);
    let rows: Vec<Vec<(&str, Value)>> = [
        (1, true),
        (1, true),
        (2, true),
        (2, false),
        (3, false),
        (3, false),
        (4, true),
        (4, false),
    ]
    .into_iter()
    .map(|(k, b)| vec![("key", int(k)), ("bool_and", Value::Bool(b))])
    .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let bools = create(&mut tx, AGG, &refs).await;
    let domain = Domain::field("id").unwrap().in_(bools.ids());
    let got = group(
        &mut tx,
        AGG,
        ReadGroup::new(domain).groupby(["key"]).aggregates([
            "bool_and:bool_and",
            "bool_and:bool_or",
            "bool_and:array_agg",
        ]),
    )
    .await;
    assert_eq!(
        got,
        vec![
            vec![
                int(1),
                t.clone(),
                t.clone(),
                Value::list([t.clone(), t.clone()])
            ],
            vec![
                int(2),
                f.clone(),
                t.clone(),
                Value::list([t.clone(), f.clone()])
            ],
            vec![
                int(3),
                f.clone(),
                f.clone(),
                Value::list([f.clone(), f.clone()])
            ],
            vec![
                int(4),
                f.clone(),
                t.clone(),
                Value::list([t.clone(), f.clone()])
            ],
        ]
    );
    // counts and arrays on other rows
    let partners = create(&mut tx, PT, &[&[("name", s("p1"))], &[("name", s("p2"))]]).await;
    let (p1, p2) = (id(&partners, 0), id(&partners, 1));
    let rows = create(
        &mut tx,
        AGG,
        &[
            &[("key", int(1)), ("partner_id", int(p1))],
            &[("key", int(1)), ("partner_id", int(p1))],
            &[("partner_id", int(p2))],
        ],
    )
    .await;
    let only = || ReadGroup::new(Domain::field("id").unwrap().in_(rows.ids()));
    assert_eq!(
        group(&mut tx, AGG, only().aggregates(["key:count"])).await,
        vec![vec![int(2)]]
    );
    assert_eq!(
        group(&mut tx, AGG, only().aggregates(["key:count_distinct"])).await,
        vec![vec![int(1)]]
    );
    assert_eq!(
        group(&mut tx, AGG, only().aggregates(["partner_id:array_agg"])).await,
        vec![vec![Value::list([int(p1), int(p1), int(p2)])]]
    );
    let records = tx.env().records(AGG).unwrap();
    let groups = records
        ._read_group(&mut tx, only().aggregates(["partner_id:recordset"]))
        .await
        .unwrap();
    let set = groups.column(0)[0].as_records().unwrap();
    assert_eq!(set.model_name(), PT);
    assert_eq!(set.ids(), [p1, p2]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_006_recordset_values_share_prefetch() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = create(&mut tx, PT, &[&[("name", s("p1"))], &[("name", s("p2"))]]).await;
    let (p1, p2) = (id(&partners, 0), id(&partners, 1));
    create(
        &mut tx,
        AGG,
        &[
            &[("key", int(1)), ("partner_id", int(p1))],
            &[("key", int(1)), ("partner_id", int(p2))],
        ],
    )
    .await;
    tx.invalidate_all().await.unwrap();
    let records = tx.env().records(AGG).unwrap();
    let groups = records
        ._read_group(&mut tx, rg(&["partner_id"], &[]))
        .await
        .unwrap();
    let first = groups.column(0)[0].as_records().unwrap().clone();
    let second = groups.column(0)[1].as_records().unwrap().clone();
    assert_eq!(first.value(&mut tx, "name").await.unwrap(), s("p1"));
    let name = assert_query_count!(tx, <= 0, { second.value(&mut tx, "name").await.unwrap() });
    assert_eq!(name, s("p2"));
    tx.invalidate_all().await.unwrap();
    let groups = records
        ._read_group(&mut tx, rg(&["key"], &["partner_id:recordset"]))
        .await
        .unwrap();
    let set = groups.column(1)[0].as_records().unwrap().clone();
    let one = set.at(0).unwrap();
    let two = set.at(1).unwrap();
    one.value(&mut tx, "name").await.unwrap();
    assert_query_count!(tx, <= 0, { two.value(&mut tx, "name").await.unwrap() });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_007_malformed_specs() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let lines = tx.env().records("test_read_group.order.line").unwrap();
    let mut calls: Vec<ReadGroup> = Vec::new();
    for g in [
        "create_date:bad_granularity",
        "Other stuff create_date:week",
        "create_date",
        "\"create_date:week",
    ] {
        calls.push(rg(&[g], &[]));
    }
    for a in [
        "value",
        "__count_",
        "value:__count",
        "other value:sum",
        "value:array_agg OR",
        "label:sum(value)",
        "order_id.create_date:min",
        "not_another_field:sum",
    ] {
        calls.push(rg(&[], &[a]));
    }
    calls.push(rg(&["order_id"], &[]).having(vec![Value::list([s("__count"), s(">")])]));
    calls.push(rg(&["order_id"], &[]).having(vec![s("COUNT(*) > 2")]));
    for o in ["__count DESC other", "value\" DESC", "value ASCCC"] {
        calls.push(rg(&["order_id"], &[]).order(o));
    }
    for call in calls {
        let label = format!("{call:?}");
        // client input: Validation (422); Rust code: Programming
        let external = lines
            ._read_group(&mut tx, call.clone().external(true))
            .await
            .expect_err(&label);
        assert_eq!(external.kind(), ErrorKind::Validation, "{label}");
        let internal = lines._read_group(&mut tx, call).await.expect_err(&label);
        assert_eq!(internal.kind(), ErrorKind::Programming, "{label}");
    }
    let err = lines
        ._read_group(&mut tx, rg(&[], &["not_another_field:sum"]))
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Invalid field 'not_another_field' on model 'test_read_group.order.line' for \
         'not_another_field:sum'."
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_008_having() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = [(1, 8), (1, 2), (2, 5), (3, 2), (3, 4), (3, 1)]
        .into_iter()
        .map(|(k, v)| vec![("key", int(k)), ("value", int(v))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(&mut tx, AGG, &refs).await;
    tx.capture_statements(true);
    let got = group(
        &mut tx,
        AGG,
        rg(&["key"], &["value:sum"]).having(vec![Value::list([s("value:sum"), s(">"), int(8)])]),
    )
    .await;
    assert_eq!(got, vec![vec![int(1), int(10)]]);
    assert!(
        tx.captured()
            .last()
            .unwrap()
            .contains(r#"HAVING SUM("test_read_group_aggregate"."value") > $1::int8"#)
    );
    let got = group(
        &mut tx,
        AGG,
        rg(&["key"], &["value:sum", "__count"]).having(vec![
            Value::list([s("__count"), s("<"), int(3)]),
            Value::list([s("value:sum"), s(">"), int(4)]),
        ]),
    )
    .await;
    assert_eq!(
        got,
        vec![vec![int(1), int(10), int(2)], vec![int(2), int(5), int(1)]]
    );
    assert!(tx.captured().last().unwrap().contains(
        r#"HAVING (COUNT(*) < $1::int8 AND SUM("test_read_group_aggregate"."value") > $2::int8)"#
    ));
    drop(tx);
    h.cleanup().await.unwrap();
}

const FT: &str = "test_read_group.fill_temporal";

fn date(y: i32, m: u32, d: u32) -> Date {
    Date::from_ymd(y, m, d).unwrap()
}

fn dt(y: i32, mo: u32, d: u32, h: u32, mi: u32, sec: u32) -> DateTime {
    DateTime::from_parts(date(y, mo, d), h, mi, sec, 0).unwrap()
}

/// Fixture F: the dates (and's datetimes) with one NULL row.
async fn fixture_f(tx: &mut Transaction) -> Records {
    let rows: Vec<Vec<(&str, Value)>> = vec![
        vec![],
        vec![
            ("date", Value::Date(date(2022, 1, 29))),
            ("datetime", Value::DateTime(dt(2022, 1, 29, 13, 55, 12))),
        ],
        vec![
            ("date", Value::Date(date(2022, 1, 29))),
            ("datetime", Value::DateTime(dt(2022, 1, 29, 15, 55, 13))),
        ],
        vec![
            ("date", Value::Date(date(2022, 1, 30))),
            ("datetime", Value::DateTime(dt(2022, 1, 30, 13, 54, 14))),
        ],
        vec![
            ("date", Value::Date(date(2022, 1, 31))),
            ("datetime", Value::DateTime(dt(2022, 1, 31, 15, 55, 14))),
        ],
        vec![
            ("date", Value::Date(date(2022, 2, 1))),
            ("datetime", Value::DateTime(dt(2022, 2, 1, 14, 54, 13))),
        ],
        vec![
            ("date", Value::Date(date(2022, 5, 29))),
            ("datetime", Value::DateTime(dt(2022, 5, 29, 14, 55, 13))),
        ],
        vec![
            ("date", Value::Date(date(2023, 1, 29))),
            ("datetime", Value::DateTime(dt(2023, 1, 29, 15, 55, 13))),
        ],
    ];
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    create(tx, FT, &refs).await
}

fn d(y: i32, m: u32, day: u32) -> Value {
    Value::Date(date(y, m, day))
}

#[tokio::test]
async fn spec_grp_009_date_buckets() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    fixture_f(&mut tx).await;
    let count = |g: &str| rg(&[g], &["__count"]);
    assert_eq!(
        group(&mut tx, FT, count("date:day")).await,
        vec![
            vec![d(2022, 1, 29), int(2)],
            vec![d(2022, 1, 30), int(1)],
            vec![d(2022, 1, 31), int(1)],
            vec![d(2022, 2, 1), int(1)],
            vec![d(2022, 5, 29), int(1)],
            vec![d(2023, 1, 29), int(1)],
            vec![F, int(1)],
        ]
    );
    assert_eq!(
        group(&mut tx, FT, count("date:week")).await,
        vec![
            vec![d(2022, 1, 23), int(2)],
            vec![d(2022, 1, 30), int(3)],
            vec![d(2022, 5, 29), int(1)],
            vec![d(2023, 1, 29), int(1)],
            vec![F, int(1)],
        ]
    );
    assert_eq!(
        group(&mut tx, FT, count("date:month")).await,
        vec![
            vec![d(2022, 1, 1), int(4)],
            vec![d(2022, 2, 1), int(1)],
            vec![d(2022, 5, 1), int(1)],
            vec![d(2023, 1, 1), int(1)],
            vec![F, int(1)],
        ]
    );
    assert_eq!(
        group(&mut tx, FT, count("date:quarter")).await,
        vec![
            vec![d(2022, 1, 1), int(5)],
            vec![d(2022, 4, 1), int(1)],
            vec![d(2023, 1, 1), int(1)],
            vec![F, int(1)],
        ]
    );
    assert_eq!(
        group(&mut tx, FT, count("date:year")).await,
        vec![
            vec![d(2022, 1, 1), int(6)],
            vec![d(2023, 1, 1), int(1)],
            vec![F, int(1)],
        ]
    );
    assert_eq!(
        group(&mut tx, FT, count("date:year").order("date:year DESC")).await,
        vec![
            vec![F, int(1)],
            vec![d(2023, 1, 1), int(1)],
            vec![d(2022, 1, 1), int(6)],
        ]
    );
    // array_agg keeps NULLs (in id order)
    let got = group(&mut tx, FT, rg(&[], &["date:array_agg"])).await;
    assert_eq!(
        got,
        vec![vec![Value::list([
            F,
            d(2022, 1, 29),
            d(2022, 1, 29),
            d(2022, 1, 30),
            d(2022, 1, 31),
            d(2022, 2, 1),
            d(2022, 5, 29),
            d(2023, 1, 29),
        ])]]
    );
    let records = tx.env().records(FT).unwrap();
    let groups = records
        ._read_group(&mut tx, rg(&[], &["date:array_agg"]))
        .await
        .unwrap();
    assert_eq!(
        groups.column(0)[0],
        GroupValue::List(vec![
            GroupValue::Null,
            GroupValue::Date(date(2022, 1, 29)),
            GroupValue::Date(date(2022, 1, 29)),
            GroupValue::Date(date(2022, 1, 30)),
            GroupValue::Date(date(2022, 1, 31)),
            GroupValue::Date(date(2022, 2, 1)),
            GroupValue::Date(date(2022, 5, 29)),
            GroupValue::Date(date(2023, 1, 29)),
        ])
    );
    assert_eq!(
        group(
            &mut tx,
            FT,
            rg(&["date:year"], &[]).order("__count, date:year")
        )
        .await,
        vec![vec![d(2023, 1, 1)], vec![F], vec![d(2022, 1, 1)]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_010_date_part_numbers() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    fixture_f(&mut tx).await;
    let cases: [(&str, &[(i64, i64)]); 10] = [
        ("datetime:second_number", &[(12, 1), (13, 4), (14, 2)]),
        ("datetime:minute_number", &[(54, 2), (55, 5)]),
        ("datetime:hour_number", &[(13, 2), (14, 2), (15, 3)]),
        (
            "date:day_of_year",
            &[(29, 3), (30, 1), (31, 1), (32, 1), (149, 1)],
        ),
        ("date:day_of_month", &[(1, 1), (29, 4), (30, 1), (31, 1)]),
        ("date:day_of_week", &[(0, 3), (1, 1), (2, 1), (6, 2)]),
        ("date:iso_week_number", &[(4, 4), (5, 2), (21, 1)]),
        ("date:month_number", &[(1, 5), (2, 1), (5, 1)]),
        ("date:quarter_number", &[(1, 6), (2, 1)]),
        ("date:year_number", &[(2022, 6), (2023, 1)]),
    ];
    for (spec, expected) in cases {
        let mut want: Vec<Vec<Value>> = expected
            .iter()
            .map(|(v, n)| vec![int(*v), int(*n)])
            .collect();
        want.push(vec![F, int(1)]);
        assert_eq!(
            group(&mut tx, FT, rg(&[spec], &["__count"])).await,
            want,
            "{spec}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_011_datetime_buckets_in_the_context_timezone() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        FT,
        &[
            &[
                ("datetime", Value::DateTime(dt(1916, 8, 18, 12, 30, 0))),
                ("value", int(1)),
            ],
            &[
                ("datetime", Value::DateTime(dt(1916, 8, 18, 12, 50, 0))),
                ("value", int(3)),
            ],
            &[
                ("datetime", Value::DateTime(dt(1916, 8, 19, 1, 30, 0))),
                ("value", int(7)),
            ],
            &[
                ("datetime", Value::DateTime(dt(1916, 10, 18, 23, 30, 0))),
                ("value", int(5)),
            ],
            &[("value", int(13))],
        ],
    )
    .await;
    let at = |y, mo, d, h| Value::DateTime(dt(y, mo, d, h, 0, 0));
    let mut by_tz = async |tz: &str, spec: &str| -> Vec<Vec<Value>> {
        let records = tx
            .env()
            .records(FT)
            .unwrap()
            .with_context([("tz", Value::str(tz))]);
        flat(
            &records
                ._read_group(&mut tx, rg(&[spec], &["value:sum"]))
                .await
                .unwrap(),
        )
    };
    assert_eq!(
        by_tz("UTC", "datetime:day").await,
        vec![
            vec![at(1916, 8, 18, 0), int(4)],
            vec![at(1916, 8, 19, 0), int(7)],
            vec![at(1916, 10, 18, 0), int(5)],
            vec![F, int(13)],
        ]
    );
    assert_eq!(
        by_tz("Europe/Brussels", "datetime:day").await,
        vec![
            vec![at(1916, 8, 18, 0), int(4)],
            vec![at(1916, 8, 19, 0), int(7)],
            vec![at(1916, 10, 19, 0), int(5)],
            vec![F, int(13)],
        ]
    );
    assert_eq!(
        by_tz("Europe/Brussels", "datetime:hour").await,
        vec![
            vec![at(1916, 8, 18, 14), int(4)],
            vec![at(1916, 8, 19, 3), int(7)],
            vec![at(1916, 10, 19, 0), int(5)],
            vec![F, int(13)],
        ]
    );
    assert_eq!(
        by_tz("America/Anchorage", "datetime:day").await,
        vec![
            vec![at(1916, 8, 18, 0), int(11)],
            vec![at(1916, 10, 18, 0), int(5)],
            vec![F, int(13)],
        ]
    );
    assert_eq!(
        by_tz("America/Anchorage", "datetime:hour").await,
        vec![
            vec![at(1916, 8, 18, 2), int(4)],
            vec![at(1916, 8, 18, 15), int(7)],
            vec![at(1916, 10, 18, 13), int(5)],
            vec![F, int(13)],
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

const TASK: &str = "test_read_group.task";

/// Fixture T: users Mario and Luigi, the four tasks.
async fn fixture_t(tx: &mut Transaction) -> (i64, i64, Records) {
    let users = create(
        tx,
        "test_read_group.user",
        &[&[("name", s("Mario"))], &[("name", s("Luigi"))]],
    )
    .await;
    let (mario, luigi) = (id(&users, 0), id(&users, 1));
    let tasks = create(
        tx,
        TASK,
        &[
            &[
                ("name", s("Super Mario Bros.")),
                ("user_ids", Value::list([int(mario), int(luigi)])),
            ],
            &[
                ("name", s("Paper Mario")),
                ("user_ids", Value::list([int(mario)])),
            ],
            &[
                ("name", s("Luigi's Mansion")),
                ("user_ids", Value::list([int(luigi)])),
            ],
            &[("name", s("Donkey Kong"))],
        ],
    )
    .await;
    (mario, luigi, tasks)
}

/// Users see only one user (Mario's id) through a record rule.
struct OnlyUser(i64);

impl erp_testkit::TestRules for OnlyUser {
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
            "test_read_group.user" => Domain::condition("id", Operator::Eq, self.0).unwrap(),
            _ => Domain::TRUE,
        })
    }
}

#[tokio::test]
async fn spec_grp_012_many2many_grouping() {
    // the record rule needs Mario's id before the harness exists: users are created first in an
    // empty database, so Mario is id 1
    let h = Harness::builder([&base_users::ADDON, &test_read_group::ADDON])
        .rules(OnlyUser(1))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let (mario, luigi, tasks) = fixture_t(&mut tx).await;
    assert_eq!(mario, 1);
    let by_users = || {
        ReadGroup::new(Domain::field("id").unwrap().in_(tasks.ids()))
            .groupby(["user_ids"])
            .aggregates(["name:array_agg"])
    };
    tx.capture_statements(true);
    let got = group(&mut tx, TASK, by_users()).await;
    assert_eq!(
        got,
        vec![
            vec![
                int(mario),
                Value::list([s("Super Mario Bros."), s("Paper Mario")])
            ],
            vec![
                int(luigi),
                Value::list([s("Super Mario Bros."), s("Luigi's Mansion")])
            ],
            vec![F, Value::list([s("Donkey Kong")])],
        ]
    );
    let sql = tx.captured().last().unwrap().clone();
    assert!(
        sql.contains(
            r#"LEFT JOIN "test_read_group_task_user_rel" AS "test_read_group_task__user_ids__rel" ON ("test_read_group_task"."id" = "test_read_group_task__user_ids__rel"."task_id")"#
        ),
        "{sql}"
    );
    assert!(sql.contains(r#"ORDER BY "test_read_group_task__user_ids__rel"."user_id" ASC"#));
    let got = group(&mut tx, TASK, by_users().order("user_ids DESC")).await;
    assert_eq!(got[0][0], F);
    // a user who sees only Mario: hidden links fall into the empty group
    let as_user = tx.env().with_user(2).records(TASK).unwrap();
    let got = flat(
        &as_user
            ._read_group(&mut tx, rg(&["user_ids"], &["name:array_agg"]))
            .await
            .unwrap(),
    );
    assert_eq!(
        got,
        vec![
            vec![
                int(mario),
                Value::list([s("Super Mario Bros."), s("Paper Mario")])
            ],
            vec![F, Value::list([s("Luigi's Mansion"), s("Donkey Kong")])],
        ]
    );
    let sql = tx.captured().last().unwrap().clone();
    assert!(
        sql.contains(r#""test_read_group_task__user_ids__rel"."user_id" IN (SELECT "test_read_group_user"."id" FROM "test_read_group_user" WHERE"#),
        "{sql}"
    );
    // the active test of the comodel applies; the field context can turn it off
    let tags = create(
        &mut tx,
        "test_read_group.tag",
        &[
            &[("name", s("live"))],
            &[("name", s("archived")), ("active", Value::Bool(false))],
        ],
    )
    .await;
    let (live, archived) = (id(&tags, 0), id(&tags, 1));
    let tagged = create(
        &mut tx,
        TASK,
        &[
            &[("name", s("a")), ("tag_ids", Value::list([int(live)]))],
            &[("name", s("b")), ("tag_ids", Value::list([int(archived)]))],
        ],
    )
    .await;
    let only = || ReadGroup::new(Domain::field("id").unwrap().in_(tagged.ids()));
    assert_eq!(
        group(
            &mut tx,
            TASK,
            only().groupby(["tag_ids"]).aggregates(["name:array_agg"])
        )
        .await,
        vec![
            vec![int(live), Value::list([s("a")])],
            vec![F, Value::list([s("b")])],
        ]
    );
    assert_eq!(
        group(
            &mut tx,
            TASK,
            only()
                .groupby(["all_tag_ids"])
                .aggregates(["name:array_agg"])
        )
        .await,
        vec![
            vec![int(live), Value::list([s("a")])],
            vec![int(archived), Value::list([s("b")])],
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Chain records only visible to users when their foo is named "visible".
struct VisibleFoo;

impl erp_testkit::TestRules for VisibleFoo {
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
            "test_read_group.foo" => Domain::condition("name", Operator::Eq, "visible").unwrap(),
            _ => Domain::TRUE,
        })
    }
}

#[tokio::test]
async fn spec_grp_013_many2one_path_grouping_and_access() {
    let h = Harness::builder([&base_users::ADDON, &test_read_group::ADDON])
        .rules(VisibleFoo)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let bars = create(
        &mut tx,
        "test_read_group.bar",
        &[&[("name", s("bar_a"))], &[]],
    )
    .await;
    let (bar_a, bar_false) = (id(&bars, 0), id(&bars, 1));
    let foos = create(
        &mut tx,
        "test_read_group.foo",
        &[
            &[("name", s("visible")), ("bar_id", int(bar_a))],
            &[("name", s("hidden")), ("bar_id", int(bar_a))],
            &[("name", s("visible")), ("bar_id", int(bar_false))],
            &[("name", s("hidden"))],
        ],
    )
    .await;
    let foo = |i| int(id(&foos, i));
    create(
        &mut tx,
        "test_read_group.chain",
        &[
            &[("foo_id", foo(0))],
            &[("foo_id", foo(1))],
            &[("foo_id", foo(1))],
            &[("foo_id", foo(2))],
            &[("foo_id", foo(3))],
        ],
    )
    .await;
    const CHAIN: &str = "test_read_group.chain";
    assert_eq!(
        group(&mut tx, CHAIN, rg(&["foo_id.bar_id"], &["__count"])).await,
        vec![
            vec![int(bar_a), int(3)],
            vec![int(bar_false), int(1)],
            vec![F, int(1)],
        ]
    );
    assert_eq!(
        group(&mut tx, CHAIN, rg(&["foo_id.bar_id.name"], &["__count"])).await,
        vec![vec![s("bar_a"), int(3)], vec![F, int(2)]]
    );
    // a user: the join target is the visible part of foo; hidden links fall into the empty group
    tx.capture_statements(true);
    let as_user = tx.env().with_user(2).records(CHAIN).unwrap();
    assert_eq!(
        flat(
            &as_user
                ._read_group(&mut tx, rg(&["foo_id.bar_id"], &["__count"]))
                .await
                .unwrap()
        ),
        vec![
            vec![int(bar_a), int(1)],
            vec![int(bar_false), int(1)],
            vec![F, int(3)],
        ]
    );
    let sql = tx.captured().last().unwrap().clone();
    assert!(
        sql.contains(
            r#"LEFT JOIN (SELECT "test_read_group_foo".* FROM "test_read_group_foo" WHERE"#
        ) && sql.contains(r#"AS "test_read_group_chain__foo_id__2""#),
        "{sql}"
    );
    let err = tx
        .env()
        .records("test_read_group.order")
        .unwrap()
        ._read_group(&mut tx, rg(&["line_ids.value"], &["__count"]))
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Only many2one path is accepted for the 'line_ids.value' groupby spec"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_014_selection_and_boolean_groups() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    const SEL: &str = "test_read_group.selection";
    create(
        &mut tx,
        SEL,
        &[
            &[("sel", s("a")), ("flag", Value::Bool(true))],
            &[("sel", s("b"))],
            &[("sel", s("c")), ("flag", Value::Bool(false))],
        ],
    )
    .await;
    // the second row's boolean is NULL in the table
    tx.execute(
        "UPDATE test_read_group_selection SET flag = NULL WHERE sel = 'b'",
        &[],
    )
    .await
    .unwrap();
    let want = vec![vec![s("c")], vec![s("b")], vec![s("a")]];
    assert_eq!(group(&mut tx, SEL, rg(&["sel"], &[])).await, want);
    assert_eq!(
        group(&mut tx, SEL, rg(&["sel"], &[]).order("sel ASC")).await,
        want
    );
    assert_eq!(
        group(&mut tx, SEL, rg(&["flag"], &["__count"])).await,
        vec![
            vec![Value::Bool(false), int(2)],
            vec![Value::Bool(true), int(1)]
        ]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_016_flush_before_grouping() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows = create(
        &mut tx,
        AGG,
        &[
            &[("key", int(1)), ("value", int(5))],
            &[("key", int(1)), ("value", int(5))],
        ],
    )
    .await;
    let (a, b) = (rows.at(0).unwrap(), rows.at(1).unwrap());
    a.set_value(&mut tx, "key", int(2)).await.unwrap();
    assert!(tx.is_dirty(&a, "key").unwrap());
    assert_eq!(
        group(
            &mut tx,
            AGG,
            ReadGroup::new(Domain::field("key").unwrap().gt(1))
                .groupby(["key"])
                .aggregates(["value:sum"])
        )
        .await,
        vec![vec![int(2), int(5)]]
    );
    a.set_value(&mut tx, "key", int(3)).await.unwrap();
    assert_eq!(
        group(&mut tx, AGG, rg(&["key"], &["value:sum"])).await,
        vec![vec![int(1), int(5)], vec![int(3), int(5)]]
    );
    b.set_value(&mut tx, "value", int(8)).await.unwrap();
    assert_eq!(
        group(&mut tx, AGG, rg(&["key"], &["value:sum"])).await,
        vec![vec![int(1), int(8)], vec![int(3), int(5)]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_017_repeated_specs_and_non_aggregatable_fields() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create(&mut tx, AGG, &[&[("key", int(1)), ("value", int(5))]]).await;
    assert_eq!(
        group(
            &mut tx,
            AGG,
            rg(&["key", "key"], &["value:sum", "value:sum", "key:sum"])
        )
        .await,
        vec![vec![int(1), int(1), int(5), int(5), int(1)]]
    );
    let err = tx
        .env()
        .records(TASK)
        .unwrap()
        ._read_group(&mut tx, rg(&[], &["user_ids:array_agg"]))
        .await
        .unwrap_err();
    assert!(
        err.message()
            .starts_with("Cannot generate SQL for multi-relational field"),
        "{}",
        err.message()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_020_public_read_group() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (mario, luigi, _) = fixture_t(&mut tx).await;
    let tasks = tx.env().records(TASK).unwrap();
    assert_eq!(
        tasks
            .read_group(&mut tx, rg(&["user_ids"], &["__count"]))
            .await
            .unwrap(),
        vec![
            vec![int(mario), int(2)],
            vec![int(luigi), int(2)],
            vec![F, int(1)]
        ]
    );
    let o1 = create(
        &mut tx,
        "test_read_group.order",
        &[&[("name", s("order 1"))]],
    )
    .await;
    let o1 = id(&o1, 0);
    create(
        &mut tx,
        "test_read_group.order",
        &[
            &[("name", s("order 2")), ("many2one_id", int(o1))],
            &[("name", s("order 3")), ("many2one_id", int(o1))],
        ],
    )
    .await;
    let orders = tx.env().records("test_read_group.order").unwrap();
    assert_eq!(
        orders
            .read_group(
                &mut tx,
                rg(&["many2one_id"], &["many2one_id:array_agg", "name:min"])
            )
            .await
            .unwrap(),
        vec![
            vec![int(o1), Value::list([int(o1), int(o1)]), s("order 2")],
            vec![F, Value::list([F]), s("order 1")],
        ]
    );
    // `read_group` is final: it is not a method of the dispatch tables, `_read_group` is
    let model = h.registry().model_id("test_read_group.order").unwrap();
    assert!(
        h.orm()
            .dispatch()
            .implementations(model, "read_group")
            .is_empty()
    );
    assert_eq!(
        h.orm()
            .dispatch()
            .implementations(model, "_read_group")
            .len(),
        1
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_021_grouping_sets_in_one_query() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    fixture_a(&mut tx).await;
    let sets = vec![
        vec!["key", "partner_id"],
        vec!["key"],
        vec!["partner_id"],
        vec![],
    ];
    let records = tx.env().records(AGG).unwrap();
    for order in [None, Some("partner_id, key")] {
        let mut args = ReadGroupingSets::new(Domain::TRUE, sets.clone()).aggregates(["value:sum"]);
        if let Some(order) = order {
            args = args.order(order);
        }
        tx.capture_statements(true);
        let got = assert_query_count!(tx, >= 1, <= 1, {
            records._read_grouping_sets(&mut tx, args).await.unwrap()
        });
        let sql = tx.captured().last().unwrap().clone();
        assert!(sql.contains("GROUPING SETS"), "{sql}");
        for (set, groups) in sets.iter().zip(&got) {
            let mut one = rg(set, &["value:sum"]);
            if let Some(order) = order {
                let restricted: Vec<&str> = order
                    .split(',')
                    .map(str::trim)
                    .filter(|t| set.contains(t))
                    .collect();
                if !restricted.is_empty() {
                    one = one.order(restricted.join(", "));
                }
            }
            let expected = group(&mut tx, AGG, one).await;
            assert_eq!(flat(groups), expected, "{set:?} order {order:?}");
        }
    }
    let err = records
        ._read_grouping_sets(
            &mut tx,
            ReadGroupingSets::new(Domain::TRUE, Vec::<Vec<&str>>::new()),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "The 'grouping_sets' parameter cannot be empty."
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_022_grouping_sets_with_many2many() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let (mario, _, tasks) = fixture_t(&mut tx).await;
    for (i, task) in tasks.iter().enumerate() {
        let mut vals = map(&[
            ("key", int(i as i64 % 2)),
            ("integer", int(10 * (i as i64 + 1))),
        ]);
        if i < 2 {
            vals.insert("customer_ids".to_owned(), Value::list([int(mario)]));
        }
        task.write_values(&mut tx, &vals).await.unwrap();
    }
    tx.flush().await.unwrap();
    let records = tx.env().records(TASK).unwrap();
    let sets = vec![
        vec!["user_ids", "key"],
        vec!["key"],
        vec!["user_ids"],
        vec![],
    ];
    // a duplicate-sensitive aggregate: sets with and without user_ids are computed apart
    let aggregates = ["name:array_agg", "__count", "integer:sum"];
    let got = assert_query_count!(tx, >= 2, <= 2, {
        records
            ._read_grouping_sets(
                &mut tx,
                ReadGroupingSets::new(Domain::TRUE, sets.clone()).aggregates(aggregates),
            )
            .await
            .unwrap()
    });
    for (set, groups) in sets.iter().zip(&got) {
        assert_eq!(
            flat(groups),
            group(&mut tx, TASK, rg(set, &aggregates)).await,
            "{set:?}"
        );
    }
    // `__count` only: one statement counting distinct ids
    let got = assert_query_count!(tx, >= 1, <= 1, {
        records
            ._read_grouping_sets(
                &mut tx,
                ReadGroupingSets::new(Domain::TRUE, sets.clone()).aggregates(["__count"]),
            )
            .await
            .unwrap()
    });
    for (set, groups) in sets.iter().zip(&got) {
        assert_eq!(
            flat(groups),
            group(&mut tx, TASK, rg(set, &["__count"])).await,
            "{set:?}"
        );
    }
    // two many2many group-bys: up to one statement per combination
    let sets = vec![
        vec!["user_ids", "customer_ids"],
        vec!["user_ids"],
        vec!["customer_ids"],
        vec![],
    ];
    let got = assert_query_count!(tx, >= 4, <= 4, {
        records
            ._read_grouping_sets(
                &mut tx,
                ReadGroupingSets::new(Domain::TRUE, sets.clone()).aggregates(["integer:sum"]),
            )
            .await
            .unwrap()
    });
    for (set, groups) in sets.iter().zip(&got) {
        assert_eq!(
            flat(groups),
            group(&mut tx, TASK, rg(set, &["integer:sum"])).await,
            "{set:?}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_055_aggregates_keep_value_types() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let rows = create(
        &mut tx,
        FT,
        &[
            &[],
            &[("datetime", Value::DateTime(dt(1916, 8, 18, 1, 50, 0)))],
            &[("datetime", Value::DateTime(dt(1916, 8, 19, 1, 30, 0)))],
            &[("datetime", Value::DateTime(dt(1916, 10, 18, 2, 30, 0)))],
        ],
    )
    .await;
    let records = tx.env().records(FT).unwrap();
    let only = || ReadGroup::new(Domain::field("id").unwrap().in_(rows.ids()));
    let groups = records
        ._read_group(&mut tx, only().aggregates(["datetime:max"]))
        .await
        .unwrap();
    assert_eq!(
        groups.column(0)[0],
        GroupValue::DateTime(dt(1916, 10, 18, 2, 30, 0))
    );
    let groups = records
        ._read_group(&mut tx, only().aggregates(["datetime:array_agg"]))
        .await
        .unwrap();
    assert_eq!(
        groups.column(0)[0],
        GroupValue::List(vec![
            GroupValue::Null,
            GroupValue::DateTime(dt(1916, 8, 18, 1, 50, 0)),
            GroupValue::DateTime(dt(1916, 8, 19, 1, 30, 0)),
            GroupValue::DateTime(dt(1916, 10, 18, 2, 30, 0)),
        ])
    );
    // numeric values stay exact decimals in the typed API (divergent)
    let agg = create(&mut tx, AGG, &[&[("numeric_value", Value::Float(42.42))]]).await;
    let groups = tx
        .env()
        .records(AGG)
        .unwrap()
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::field("id").unwrap().in_(agg.ids()))
                .aggregates(["numeric_value:array_agg"]),
        )
        .await
        .unwrap();
    assert_eq!(
        groups.column(0)[0],
        GroupValue::List(vec![GroupValue::Decimal(
            "42.42".parse::<Decimal>().unwrap()
        )])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_056_x2many_conditions_do_not_multiply_groups() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let orders = create(
        &mut tx,
        "test_read_group.order",
        &[&[("name", s("o1"))], &[("name", s("o2"))]],
    )
    .await;
    create(
        &mut tx,
        "test_read_group.order.line",
        &[
            &[("order_id", int(id(&orders, 0))), ("value", int(1))],
            &[("order_id", int(id(&orders, 0))), ("value", int(2))],
            &[("order_id", int(id(&orders, 1))), ("value", int(1))],
        ],
    )
    .await;
    let records = tx.env().records("test_read_group.order").unwrap();
    for condition in [
        json!(["line_ids.value", "=", 1]),
        json!(["line_ids.value", ">", 0]),
    ] {
        let domain = dom(json!([["id", "in", orders.ids()], condition]));
        let got = flat(
            &records
                ._read_group(
                    &mut tx,
                    ReadGroup::new(domain.clone()).aggregates(["__count"]),
                )
                .await
                .unwrap(),
        );
        let n = records.search_count(&mut tx, &domain, None).await.unwrap();
        assert_eq!(got, vec![vec![int(2)]]);
        assert_eq!(n, 2);
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_grp_057_grouping_on_inherits_fields() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let bases = create(
        &mut tx,
        "test_read_group.related_base",
        &[
            &[("name", s("a")), ("value", int(1))],
            &[("name", s("a")), ("value", int(2))],
            &[("name", s("b")), ("value", int(3))],
            &[("value", int(4))],
        ],
    )
    .await;
    let base = |i| int(id(&bases, i));
    create(
        &mut tx,
        "test_read_group.related_inherits",
        &[
            &[("base_id", base(0))],
            &[("base_id", base(0))],
            &[("base_id", base(1))],
            &[("base_id", base(2))],
            &[("base_id", base(3))],
        ],
    )
    .await;
    const INH: &str = "test_read_group.related_inherits";
    tx.capture_statements(true);
    assert_eq!(
        group(&mut tx, INH, rg(&["name"], &["__count"])).await,
        vec![vec![s("a"), int(3)], vec![s("b"), int(1)], vec![F, int(1)]]
    );
    assert_sql(
        tx.captured().last().unwrap(),
        r#"SELECT "test_read_group_related_inherits__base_id"."name", COUNT(*)
           FROM "test_read_group_related_inherits"
           JOIN "test_read_group_related_base" AS "test_read_group_related_inherits__base_id"
             ON ("test_read_group_related_inherits"."base_id" = "test_read_group_related_inherits__base_id"."id")
           GROUP BY "test_read_group_related_inherits__base_id"."name"
           ORDER BY "test_read_group_related_inherits__base_id"."name" ASC"#,
    );
    assert_eq!(
        group(&mut tx, INH, rg(&["name"], &["value:sum"])).await,
        vec![vec![s("a"), int(4)], vec![s("b"), int(3)], vec![F, int(4)]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 7: an override of `_read_group` answering another request is a programming error,
/// never silently truncated by its callers.
#[tokio::test]
async fn read_group_override_results_are_checked() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records("test_read_group.bad_override").unwrap();
    let err = records
        ._read_group(&mut tx, rg(&["name"], &["__count"]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains("_read_group returned"),
        "{}",
        err.message()
    );
    // the request it does answer passes
    records
        ._read_group(&mut tx, rg(&[], &["__count"]))
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 8: an order term naming a bad aggregate reports the aggregate's own error.
#[tokio::test]
async fn read_group_order_reports_the_aggregate_error() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let records = tx.env().records(AGG).unwrap();
    let err = records
        ._read_group(&mut tx, rg(&["key"], &[]).order("value:nope"))
        .await
        .unwrap_err();
    assert_eq!(
        err.message(),
        "Invalid aggregate method 'nope' for 'value:nope'."
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn read_group_array_nulls_follow_reference() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let partners = create(&mut tx, PT, &[&[("name", s("p"))]]).await;
    let p = id(&partners, 0);
    let rows = create(
        &mut tx,
        AGG,
        &[
            &[("partner_id", int(p))],
            &[],
            &[("partner_id", int(p))],
            &[],
        ],
    )
    .await;
    let records = tx.env().records(AGG).unwrap();
    let groups = records
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::field("id").unwrap().in_(rows.ids())).aggregates([
                "partner_id:array_agg",
                "partner_id:array_agg_distinct",
                "partner_id:recordset",
            ]),
        )
        .await
        .unwrap();
    let pi = GroupValue::Int(p);
    assert_eq!(
        groups.column(0)[0],
        GroupValue::List(vec![
            pi.clone(),
            GroupValue::Null,
            pi.clone(),
            GroupValue::Null
        ])
    );
    assert_eq!(
        groups.column(1)[0],
        GroupValue::List(vec![pi, GroupValue::Null])
    );
    assert_eq!(groups.column(2)[0].as_records().unwrap().ids(), [p]);
    // all NULL: an empty recordset
    let empty = records
        ._read_group(
            &mut tx,
            ReadGroup::new(Domain::field("id").unwrap().in_(vec![id(&rows, 1)]))
                .aggregates(["partner_id:recordset"]),
        )
        .await
        .unwrap();
    assert!(empty.column(0)[0].as_records().unwrap().is_empty());
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 2: grouping sets take at most 31 distinct group-by specs (one `GROUPING()` bit
/// each), a client error beyond.
#[tokio::test]
async fn grouping_sets_reject_more_than_31_specs() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let grans = [
        "hour",
        "day",
        "week",
        "month",
        "quarter",
        "year",
        "year_number",
        "quarter_number",
        "month_number",
        "iso_week_number",
        "day_of_year",
        "day_of_month",
        "day_of_week",
        "hour_number",
        "minute_number",
        "second_number",
    ];
    let specs: Vec<String> = ["date", "datetime"]
        .iter()
        .flat_map(|f| grans.iter().map(move |g| format!("{f}:{g}")))
        .collect();
    assert_eq!(specs.len(), 32);
    let records = tx.env().records(FT).unwrap();
    let err = records
        ._read_grouping_sets(
            &mut tx,
            ReadGroupingSets::new(Domain::TRUE, [specs.clone(), Vec::new()])
                .aggregates(["__count"])
                .external(true),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    // 31 work
    let got = records
        ._read_grouping_sets(
            &mut tx,
            ReadGroupingSets::new(Domain::TRUE, [specs[..31].to_vec(), Vec::new()])
                .aggregates(["__count"]),
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 2);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review item 11: a time zone PostgreSQL does not know logs a warning and groups in UTC.
#[tokio::test]
async fn unknown_timezone_groups_in_utc() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    create(
        &mut tx,
        FT,
        &[&[("datetime", Value::DateTime(dt(2022, 3, 1, 23, 30, 0)))]],
    )
    .await;
    let records = tx
        .env()
        .records(FT)
        .unwrap()
        .with_context([("tz", Value::str("Mars/Olympus_Mons"))]);
    let got = flat(
        &records
            ._read_group(&mut tx, rg(&["datetime:day"], &["__count"]))
            .await
            .unwrap(),
    );
    assert_eq!(
        got,
        vec![vec![Value::DateTime(dt(2022, 3, 1, 0, 0, 0)), int(1)]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
