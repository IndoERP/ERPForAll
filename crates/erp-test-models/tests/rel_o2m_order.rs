//! The one2many inverse read (PERF.WRITE-O2M): the per-owner order of the value is
//! the comodel's order whatever the statement shape; the statement reads the comodel table alone
//! ordered by the inverse first (a join only for an order term on another table: another
//! many2one, two levels of them, an `_inherits` parent's field); a compute-driven read fetches
//! only the dependency columns, a plain read keeps the prefetch columns, and a two-field compute
//! group loads its old values in one read; a related inverse and a private policy snapshot are
//! planned per call; and on a grown table with stale statistics the inverse index stays the access
//! path (D2.5, D3; lead decisions 1 (i)–(iii) and 2).
//!
//! Needs a PostgreSQL superuser in `DATABASE_URL` (the dev cluster in CI).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::collections::BTreeMap;

use erp_sdk::{Domain, Operator, Records, SearchOptions, Transaction, Value};
use erp_test_models::{base_users, test_o2m_order};
use erp_testkit::{DomainPolicy, Harness, Otherwise};

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_o2m_order::ADDON])
        .build()
        .await
        .expect("harness")
}

fn map(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
    support::map(pairs)
}

/// A small deterministic generator (the order of creation and the sequences are shuffled so that
/// ids and sequences do not follow each other).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, bound: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % bound
    }
}

fn ids_of(value: &Value) -> Vec<i64> {
    match value {
        Value::List(items) => items
            .iter()
            .map(|v| match v {
                Value::Int(id) => *id,
                other => panic!("an x2many id must be an integer, got {other:?}"),
            })
            .collect(),
        Value::False => Vec::new(),
        other => panic!("an x2many value must be a list, got {other:?}"),
    }
}

async fn create(tx: &mut Transaction, model: &str, rows: &[BTreeMap<String, Value>]) -> Vec<i64> {
    tx.env()
        .records(model)
        .unwrap()
        .create_values(tx, rows)
        .await
        .unwrap()
        .ids()
}

/// Owners, tags, and 48 lines in each line model with shuffled owners, sequences (with ties),
/// tags and groups (some empty), ranks (with ties), created in shuffled order. Committed.
async fn fixture(h: &Harness) -> (Vec<i64>, Vec<i64>) {
    let mut tx = h.begin().await.unwrap();
    let owners = create(
        &mut tx,
        "test_o2m_order.owner",
        &(0..6)
            .map(|n| map(&[("name", Value::str(format!("Owner {n}")))]))
            .collect::<Vec<_>>(),
    )
    .await;
    // tag names out of id order, so the tag order is not the id order
    let tags = create(
        &mut tx,
        "test_o2m_order.tag",
        &["delta", "alpha", "charlie", "bravo"]
            .iter()
            .map(|n| map(&[("name", Value::str(*n))]))
            .collect::<Vec<_>>(),
    )
    .await;
    let mut rng = Lcg(42);
    for model in [
        "test_o2m_order.lead",
        "test_o2m_order.seq",
        "test_o2m_order.mid",
    ] {
        let rows: Vec<_> = (0..48)
            .map(|n| {
                let owner = owners[usize::try_from(rng.next(6)).unwrap()];
                map(&[
                    ("owner_id", Value::Int(owner)),
                    ("sequence", Value::Int(i64::try_from(rng.next(5)).unwrap())),
                    ("amount", Value::Float(f64::from(n))),
                    ("name", Value::str(format!("L{n}"))),
                ])
            })
            .collect();
        create(&mut tx, model, &rows).await;
    }
    let rows: Vec<_> = (0..48)
        .map(|n| {
            let owner = owners[usize::try_from(rng.next(6)).unwrap()];
            let tag = match rng.next(5) {
                4 => Value::False,
                t => Value::Int(tags[usize::try_from(t).unwrap()]),
            };
            map(&[
                ("owner_id", Value::Int(owner)),
                ("tag_id", tag),
                ("name", Value::str(format!("T{n}"))),
            ])
        })
        .collect();
    create(&mut tx, "test_o2m_order.tagged", &rows).await;
    // groups ordered by their tag (names out of tag order)
    let grps = create(
        &mut tx,
        "test_o2m_order.grp",
        &[3_usize, 0, 2, 1, 0]
            .iter()
            .enumerate()
            .map(|(n, t)| {
                map(&[
                    ("tag_id", Value::Int(tags[*t])),
                    ("name", Value::str(format!("G{n}"))),
                ])
            })
            .collect::<Vec<_>>(),
    )
    .await;
    let rows: Vec<_> = (0..48)
        .map(|n| {
            let owner = owners[usize::try_from(rng.next(6)).unwrap()];
            let grp = match rng.next(6) {
                5 => Value::False,
                g => Value::Int(grps[usize::try_from(g).unwrap()]),
            };
            map(&[
                ("owner_id", Value::Int(owner)),
                ("grp_id", grp),
                ("name", Value::str(format!("D{n}"))),
            ])
        })
        .collect();
    create(&mut tx, "test_o2m_order.deep", &rows).await;
    let rows: Vec<_> = (0..48)
        .map(|n| {
            let owner = owners[usize::try_from(rng.next(6)).unwrap()];
            map(&[
                ("owner_id", Value::Int(owner)),
                ("rank", Value::Int(i64::try_from(rng.next(5)).unwrap())),
                ("name", Value::str(format!("I{n}"))),
            ])
        })
        .collect();
    create(&mut tx, "test_o2m_order.inh", &rows).await;
    // two vias per owner, the hops spread over them
    let vias = create(
        &mut tx,
        "test_o2m_order.via",
        &(0..12)
            .map(|n| map(&[("owner_id", Value::Int(owners[n % 6]))]))
            .collect::<Vec<_>>(),
    )
    .await;
    let rows: Vec<_> = (0..48)
        .map(|n| {
            let via = vias[usize::try_from(rng.next(12)).unwrap()];
            map(&[
                ("via_id", Value::Int(via)),
                ("name", Value::str(format!("H{n}"))),
            ])
        })
        .collect();
    create(&mut tx, "test_o2m_order.hop", &rows).await;
    tx.flush().await.unwrap();
    tx.commit().await.unwrap();
    (owners, tags)
}

/// The lines of `owners` (through the inverse `inverse`, within `extra`) by today's search in the
/// comodel's order (the joined statement of `search`), grouped per owner in that order.
async fn searched(
    tx: &mut Transaction,
    model: &str,
    inverse: &str,
    extra: Domain,
    owners: &[i64],
) -> BTreeMap<i64, Vec<i64>> {
    let lines = tx
        .env()
        .records(model)
        .unwrap()
        .search_with(
            tx,
            &(Domain::condition(inverse, Operator::In, owners.to_vec()).unwrap() & extra),
            &SearchOptions::new().active_test(false),
        )
        .await
        .unwrap();
    let owner_of = lines.values(tx, inverse).await.unwrap();
    let mut out: BTreeMap<i64, Vec<i64>> = owners.iter().map(|o| (*o, Vec::new())).collect();
    for (line, owner) in lines.ids().into_iter().zip(owner_of) {
        let owner = match owner {
            Value::Int(id) => id,
            Value::List(pair) => match pair.first() {
                Some(Value::Int(id)) => *id,
                other => panic!("owner_id read as {other:?}"),
            },
            other => panic!("owner_id read as {other:?}"),
        };
        out.get_mut(&owner).unwrap().push(line);
    }
    out
}

/// The one2many value of `owners` read through the field (the inverse read under test).
async fn field_values(
    tx: &mut Transaction,
    owners: &Records,
    field: &str,
) -> BTreeMap<i64, Vec<i64>> {
    let values = owners.values(tx, field).await.unwrap();
    owners
        .ids()
        .into_iter()
        .zip(values.iter().map(ids_of))
        .collect()
}

/// Lead decision 1 (i): for an order that starts with the inverse (`owner_id, sequence, id`),
/// one without it (`sequence, id`), one with it in the middle and descending (`sequence,
/// owner_id desc, id`), one ordered by another many2one (`tag_id, id`), by two levels of them
/// (`grp_id, id`, the group ordered by its tag) and by an `_inherits` parent's field (`rank,
/// id`), with a field domain (`sequence > 1`), and over a related inverse of two hops, the
/// per-owner value equals today's joined search grouped per owner, on shuffled ids and sequences
/// with ties.
#[tokio::test]
async fn the_per_owner_order_is_the_comodel_order_for_every_order_shape() {
    let h = harness().await;
    let (owners, _) = fixture(&h).await;
    for (field, model, inverse, extra) in [
        ("lead_ids", "test_o2m_order.lead", "owner_id", Domain::TRUE),
        ("seq_ids", "test_o2m_order.seq", "owner_id", Domain::TRUE),
        ("mid_ids", "test_o2m_order.mid", "owner_id", Domain::TRUE),
        (
            "tagged_ids",
            "test_o2m_order.tagged",
            "owner_id",
            Domain::TRUE,
        ),
        ("deep_ids", "test_o2m_order.deep", "owner_id", Domain::TRUE),
        ("inh_ids", "test_o2m_order.inh", "owner_id", Domain::TRUE),
        (
            "high_seq_ids",
            "test_o2m_order.seq",
            "owner_id",
            Domain::condition("sequence", Operator::Gt, 1).unwrap(),
        ),
        ("hop_ids", "test_o2m_order.hop", "owner_rel", Domain::TRUE),
    ] {
        let mut tx = h.begin().await.unwrap();
        let expected = searched(&mut tx, model, inverse, extra, &owners).await;
        tx.invalidate_all().await.unwrap();
        let records = tx
            .env()
            .records("test_o2m_order.owner")
            .unwrap()
            .browse(owners.iter().copied())
            .unwrap();
        let actual = field_values(&mut tx, &records, field).await;
        assert_eq!(actual, expected, "{field}: per-owner order differs");
        assert!(
            expected.values().filter(|v| v.len() > 1).count() >= 3,
            "{field}: the fixture must give several owners more than one line"
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// The `SELECT … FROM <table>` list of a statement, unqualified and unquoted.
fn select_list(sql: &str) -> Vec<String> {
    let start = sql.find("SELECT ").expect("a SELECT") + "SELECT ".len();
    let end = sql[start..].find(" FROM ").expect("a FROM") + start;
    sql[start..end]
        .split(", ")
        .map(|c| c.rsplit('.').next().unwrap().trim_matches('"').to_owned())
        .collect()
}

fn order_by(sql: &str) -> &str {
    let at = sql.find(" ORDER BY ").expect("an ORDER BY") + " ORDER BY ".len();
    let rest = &sql[at..];
    rest.find(" LIMIT ").map_or(rest, |end| &rest[..end])
}

/// The one statement of `captured` that reads `table` (as `FROM "table"`).
fn reading<'a>(captured: &'a [String], table: &str) -> &'a str {
    let from = format!("FROM \"{table}\"");
    let found: Vec<&String> = captured
        .iter()
        .filter(|s| s.starts_with("SELECT") && s.contains(&from))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "one read of {table} expected: {captured:#?}"
    );
    found[0]
}

/// (lead decision 1 (iii)): the inverse read of the three orders without another
/// many2one reads the line table alone, ordered by the inverse then the rest of the comodel
/// order without its inverse terms; the read of `tag_id, id` keeps the join to the tag table.
#[tokio::test]
async fn the_inverse_read_orders_by_the_inverse_first_and_joins_only_for_other_many2ones() {
    let h = harness().await;
    let (owners, _) = fixture(&h).await;
    let cases = [
        (
            "lead_ids",
            "test_o2m_order_lead",
            "\"test_o2m_order_lead\".\"owner_id\", \"test_o2m_order_lead\".\"sequence\", \
             \"test_o2m_order_lead\".\"id\"",
        ),
        (
            "seq_ids",
            "test_o2m_order_seq",
            "\"test_o2m_order_seq\".\"owner_id\", \"test_o2m_order_seq\".\"sequence\", \
             \"test_o2m_order_seq\".\"id\"",
        ),
        (
            "mid_ids",
            "test_o2m_order_mid",
            "\"test_o2m_order_mid\".\"owner_id\", \"test_o2m_order_mid\".\"sequence\", \
             \"test_o2m_order_mid\".\"id\"",
        ),
    ];
    for (field, table, order) in cases {
        let mut tx = h.begin().await.unwrap();
        let records = tx
            .env()
            .records("test_o2m_order.owner")
            .unwrap()
            .browse(owners.iter().copied())
            .unwrap();
        tx.capture_statements(true);
        records.values(&mut tx, field).await.unwrap();
        let captured = tx.captured().to_vec();
        let sql = reading(&captured, table);
        assert!(!sql.contains(" JOIN "), "{field}: no join expected: {sql}");
        assert_eq!(order_by(sql), order, "{field}: {sql}");
        // D2.5: the id list reaches the planner only behind a sub-select
        assert!(
            sql.contains(&format!(
                " WHERE \"{table}\".\"owner_id\" = ANY((SELECT $1::int8[])::int8[]) ORDER BY "
            )),
            "{field}: {sql}"
        );
        drop(tx);
    }
    // the statements that keep joins: another many2one, two levels of them, the `_inherits`
    // parent (the inverse still first, the id list still behind the sub-select)
    for (field, table, joined) in [
        (
            "tagged_ids",
            "test_o2m_order_tagged",
            &["test_o2m_order_tag"][..],
        ),
        (
            "deep_ids",
            "test_o2m_order_deep",
            &["test_o2m_order_grp", "test_o2m_order_tag"][..],
        ),
        (
            "inh_ids",
            "test_o2m_order_inh",
            &["test_o2m_order_ranked"][..],
        ),
    ] {
        let mut tx = h.begin().await.unwrap();
        let records = tx
            .env()
            .records("test_o2m_order.owner")
            .unwrap()
            .browse(owners.iter().copied())
            .unwrap();
        tx.capture_statements(true);
        records.values(&mut tx, field).await.unwrap();
        let captured = tx.captured().to_vec();
        let sql = reading(&captured, table);
        for other in joined {
            assert!(
                sql.contains(&format!("JOIN \"{other}\"")),
                "{field}: the order needs its join to {other}: {sql}"
            );
        }
        assert!(
            order_by(sql).starts_with(&format!("\"{table}\".\"owner_id\", ")),
            "{field}: the inverse comes first: {sql}"
        );
        assert!(
            sql.contains(&format!(
                "\"{table}\".\"owner_id\" = ANY((SELECT $1::int8[])::int8[])"
            )),
            "{field}: {sql}"
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}

/// Whether `sql` is a prepared statement of the connection `tx` runs on.
async fn prepared(tx: &mut Transaction, sql: &str) -> bool {
    let n: i64 = tx
        .query(
            "SELECT count(*) FROM pg_prepared_statements WHERE statement = $1",
            &[&sql],
        )
        .await
        .unwrap()[0]
        .get(0);
    n > 0
}

/// Review round 1 (major 2): a one2many over a non-stored related many2one (two hops) with no
/// field domain keeps today's search, planned per call: never a cached id-list fetch, whose shape
/// (D2) the related search does not have.
#[tokio::test]
async fn a_related_inverse_read_is_planned_per_call() {
    let h = harness().await;
    let (owners, _) = fixture(&h).await;
    let mut tx = h.begin().await.unwrap();
    let expected = searched(
        &mut tx,
        "test_o2m_order.hop",
        "owner_rel",
        Domain::TRUE,
        &owners,
    )
    .await;
    tx.invalidate_all().await.unwrap();
    let records = tx
        .env()
        .records("test_o2m_order.owner")
        .unwrap()
        .browse(owners.iter().copied())
        .unwrap();
    tx.capture_statements(true);
    let actual = field_values(&mut tx, &records, "hop_ids").await;
    let captured = tx.captured().to_vec();
    assert_eq!(actual, expected);
    let sql = reading(&captured, "test_o2m_order_hop");
    assert!(!sql.contains("(SELECT $1::int8[])"), "{sql}");
    assert!(!prepared(&mut tx, sql).await, "a per-call statement: {sql}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review round 1: under a private policy snapshot every statement is planned per call, the
/// id-list fetch too: the value is right and nothing is prepared.
#[tokio::test]
async fn under_a_private_policy_snapshot_the_inverse_read_is_planned_per_call() {
    let h = Harness::builder([&base_users::ADDON, &test_o2m_order::ADDON])
        .policy(
            DomainPolicy::builder(Otherwise::Allow)
                .private()
                .build()
                .unwrap(),
        )
        .build()
        .await
        .expect("harness");
    let (owners, _) = fixture(&h).await;
    let mut tx = h.begin().await.unwrap();
    let expected = searched(
        &mut tx,
        "test_o2m_order.seq",
        "owner_id",
        Domain::TRUE,
        &owners,
    )
    .await;
    tx.invalidate_all().await.unwrap();
    let records = tx
        .env()
        .records("test_o2m_order.owner")
        .unwrap()
        .browse(owners.iter().copied())
        .unwrap();
    tx.capture_statements(true);
    let actual = field_values(&mut tx, &records, "seq_ids").await;
    let captured = tx.captured().to_vec();
    assert_eq!(actual, expected);
    let sql = reading(&captured, "test_o2m_order_seq");
    assert!(sql.contains("(SELECT $1::int8[])"), "the D2.5 text: {sql}");
    assert!(!prepared(&mut tx, sql).await, "a per-call statement: {sql}");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn a_two_field_compute_group_loads_its_old_values_in_one_read() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let pairs = create(
        &mut tx,
        "test_o2m_order.pair",
        &(0..3)
            .map(|n| map(&[("name", Value::str(format!("P{n}")))]))
            .collect::<Vec<_>>(),
    )
    .await;
    tx.flush().await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<_> = pairs
        .iter()
        .flat_map(|p| {
            [1.5, 4.0].map(|amount| {
                map(&[
                    ("pair_id", Value::Int(*p)),
                    ("amount", Value::Float(amount)),
                ])
            })
        })
        .collect();
    tx.capture_statements(true);
    create(&mut tx, "test_o2m_order.pair_line", &rows).await;
    tx.flush().await.unwrap();
    let captured = tx.captured().to_vec();
    let update = captured
        .iter()
        .position(|s| s.starts_with("UPDATE \"test_o2m_order_pair\""))
        .unwrap_or_else(|| panic!("the group is written: {captured:#?}"));
    let reads: Vec<&String> = captured[..update]
        .iter()
        .filter(|s| s.starts_with("SELECT") && s.contains("FROM \"test_o2m_order_pair\""))
        .collect();
    assert_eq!(reads.len(), 1, "one old-value read: {captured:#?}");
    assert_eq!(
        select_list(reads[0]),
        ["id", "sum_amount", "max_amount"],
        "{}",
        reads[0]
    );
    tx.invalidate_all().await.unwrap();
    let records = tx
        .env()
        .records("test_o2m_order.pair")
        .unwrap()
        .browse(pairs.iter().copied())
        .unwrap();
    let sums = records.values(&mut tx, "sum_amount").await.unwrap();
    let maxes = records.values(&mut tx, "max_amount").await.unwrap();
    assert!(sums.iter().all(|v| *v == Value::Float(5.5)), "{sums:?}");
    assert!(maxes.iter().all(|v| *v == Value::Float(4.0)), "{maxes:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review round 2 (N4): a partly cached group still costs one old-value read. One owner has
/// `sum_amount` cached and lacks `max_amount`, the other the reverse; the read selects every
/// record that lacks any group field, keeps the cached values, and the compute writes both.
#[tokio::test]
async fn a_partly_cached_compute_group_still_loads_its_old_values_in_one_read() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    let pairs = create(
        &mut tx,
        "test_o2m_order.pair",
        &(0..2)
            .map(|n| map(&[("name", Value::str(format!("Q{n}")))]))
            .collect::<Vec<_>>(),
    )
    .await;
    tx.flush().await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let model = tx.env().records("test_o2m_order.pair").unwrap();
    // one field each, without the prefetch group: the group is partly cached on both records
    for (pair, field) in [(pairs[0], "sum_amount"), (pairs[1], "max_amount")] {
        let one = model
            .browse([pair])
            .unwrap()
            .with_context([("prefetch_fields", Value::Bool(false))]);
        assert_eq!(one.values(&mut tx, field).await.unwrap().len(), 1);
    }
    let rows: Vec<_> = pairs
        .iter()
        .flat_map(|p| {
            [1.5, 4.0].map(|amount| {
                map(&[
                    ("pair_id", Value::Int(*p)),
                    ("amount", Value::Float(amount)),
                ])
            })
        })
        .collect();
    tx.capture_statements(true);
    create(&mut tx, "test_o2m_order.pair_line", &rows).await;
    tx.flush().await.unwrap();
    let captured = tx.captured().to_vec();
    let update = captured
        .iter()
        .position(|s| s.starts_with("UPDATE \"test_o2m_order_pair\""))
        .unwrap_or_else(|| panic!("the group is written: {captured:#?}"));
    let reads: Vec<&String> = captured[..update]
        .iter()
        .filter(|s| s.starts_with("SELECT") && s.contains("FROM \"test_o2m_order_pair\""))
        .collect();
    assert_eq!(reads.len(), 1, "one old-value read: {captured:#?}");
    tx.invalidate_all().await.unwrap();
    let records = model.browse(pairs.iter().copied()).unwrap();
    let sums = records.values(&mut tx, "sum_amount").await.unwrap();
    let maxes = records.values(&mut tx, "max_amount").await.unwrap();
    assert!(sums.iter().all(|v| *v == Value::Float(5.5)), "{sums:?}");
    assert!(maxes.iter().all(|v| *v == Value::Float(4.0)), "{maxes:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Lead decision 2 / perf-write option 2: the recompute of `total` reads `lead_ids` with the
/// inverse and the dependency column only, and loads the old `total` alone (option 5, W12); a
/// plain read of `lead_ids` keeps every prefetch column.
#[tokio::test]
async fn a_compute_driven_read_fetches_the_dependency_columns_only() {
    let h = harness().await;
    let (owners, _) = fixture(&h).await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<_> = owners
        .iter()
        .map(|o| {
            map(&[
                ("owner_id", Value::Int(*o)),
                ("sequence", Value::Int(9)),
                ("amount", Value::Float(1.5)),
                ("name", Value::str("added")),
            ])
        })
        .collect();
    tx.capture_statements(true);
    create(&mut tx, "test_o2m_order.lead", &rows).await;
    tx.flush().await.unwrap();
    let captured = tx.captured().to_vec();
    let lines = reading(&captured, "test_o2m_order_lead");
    assert_eq!(
        select_list(lines),
        ["id", "owner_id", "amount"],
        "the recompute's line read: {lines}"
    );
    let old = reading(&captured, "test_o2m_order_owner");
    assert_eq!(
        select_list(old),
        ["id", "total"],
        "the old value of total: {old}"
    );
    tx.rollback().await.unwrap();

    let mut tx = h.begin().await.unwrap();
    let records = tx
        .env()
        .records("test_o2m_order.owner")
        .unwrap()
        .browse(owners.iter().copied())
        .unwrap();
    tx.capture_statements(true);
    records.values(&mut tx, "lead_ids").await.unwrap();
    let captured = tx.captured().to_vec();
    let plain = select_list(reading(&captured, "test_o2m_order_lead"));
    for column in ["id", "owner_id", "sequence", "amount", "name"] {
        assert!(
            plain.iter().any(|c| c == column),
            "a plain read keeps the prefetch column {column}: {plain:?}"
        );
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

async fn scans(tx: &mut Transaction, tables: &[&str]) -> BTreeMap<String, (i64, i64)> {
    let names: Vec<String> = tables.iter().map(|t| (*t).to_owned()).collect();
    let rows = tx
        .query(
            "SELECT relname::text, COALESCE(seq_scan, 0), COALESCE(idx_scan, 0) \
             FROM pg_stat_xact_user_tables WHERE relname = ANY($1)",
            &[&names],
        )
        .await
        .unwrap();
    rows.iter()
        .map(|r| (r.get::<_, String>(0), (r.get(1), r.get(2))))
        .collect()
}

fn scan_delta(
    after: &BTreeMap<String, (i64, i64)>,
    before: &BTreeMap<String, (i64, i64)>,
    table: &str,
) -> (i64, i64) {
    let a = after.get(table).copied().unwrap_or((0, 0));
    let b = before.get(table).copied().unwrap_or((0, 0));
    (a.0 - b.0, a.1 - b.1)
}

/// state (b) for the prefixed shape (lead decision 1 (ii)) and the many2many owner
/// read (decision 2): tables analyzed while empty, then grown to 100,000 lines and relation rows
/// without `ANALYZE`; ten reads of 100 owners each on the harness's pooled connection, which may
/// already hold the statements from building the fixture, so each read is planned as `auto`
/// decides (per call for a statement's first five executions on a connection). With D2.5 every
/// such plan is the generic plan's, so the checks hold whichever kind runs: the line table and the
/// relation table are never scanned sequentially, and the inverse read is a cached prepared
/// statement.
#[tokio::test]
async fn stale_statistics_keep_the_inverse_index_for_the_prefixed_order_and_many2many() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    for table in [
        "test_o2m_order_owner",
        "test_o2m_order_seq",
        "test_o2m_order_tag",
        "test_o2m_order_owner_tag_rel",
    ] {
        tx.execute(
            &format!("ALTER TABLE \"{table}\" SET (autovacuum_enabled = false)"),
            &[],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "ANALYZE test_o2m_order_owner, test_o2m_order_seq, test_o2m_order_tag, \
         test_o2m_order_owner_tag_rel",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO test_o2m_order_owner (name) SELECT 'O' || g FROM generate_series(1, 5000) g",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO test_o2m_order_tag (name) SELECT 'T' || g FROM generate_series(1, 5000) g",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO test_o2m_order_seq (owner_id, sequence, amount, name) \
         SELECT o.id, (g * 7919) % 13, g, 'S' || g \
         FROM generate_series(1, 100000) g \
         JOIN test_o2m_order_owner o ON o.name = 'O' || (1 + (g * 31) % 5000)",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO test_o2m_order_owner_tag_rel (owner_id, tag_id) \
         SELECT DISTINCT o.id, t.id FROM generate_series(1, 100000) g \
         JOIN test_o2m_order_owner o ON o.name = 'O' || (1 + g % 5000) \
         JOIN test_o2m_order_tag t ON t.name = 'T' || (1 + ((g / 5000) * 97 + g) % 5000)",
        &[],
    )
    .await
    .unwrap();
    let owners: Vec<i64> = tx
        .query("SELECT id FROM test_o2m_order_owner ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    tx.commit().await.unwrap();
    assert_eq!(owners.len(), 5000);

    let tables = [
        "test_o2m_order_seq",
        "test_o2m_order_owner_tag_rel",
        "test_o2m_order_tag",
    ];
    for round in 0..10 {
        let mut tx = h.begin().await.unwrap();
        let chunk: Vec<i64> = owners[round * 400..round * 400 + 100].to_vec();
        let records = tx
            .env()
            .records("test_o2m_order.owner")
            .unwrap()
            .browse(chunk)
            .unwrap();
        let before = scans(&mut tx, &tables).await;
        tx.capture_statements(true);
        let lines = records.values(&mut tx, "seq_ids").await.unwrap();
        let tags = records.values(&mut tx, "tag_ids").await.unwrap();
        let after = scans(&mut tx, &tables).await;
        assert!(lines.iter().map(ids_of).map(|v| v.len()).sum::<usize>() >= 1_000);
        assert!(tags.iter().map(ids_of).map(|v| v.len()).sum::<usize>() >= 1_000);
        let captured = tx.captured().to_vec();
        let seq_sql = reading(&captured, "test_o2m_order_seq").to_owned();
        let tag_sql = reading(&captured, "test_o2m_order_tag");
        assert!(
            tag_sql.contains(
                "\"test_o2m_order_owner_tag_rel\".\"owner_id\" = ANY((SELECT $1::int8[])::int8[])"
            ),
            "D2.5: the many2many read binds its id list behind a sub-select: {tag_sql}"
        );
        // the line and relation tables grow with the owners and must stay on their index; the
        // tag table is the comodel the many2many read joins for its order (`name, id`): 5,000
        // rows, hashed whole, its size independent of the relations read
        for table in ["test_o2m_order_seq", "test_o2m_order_owner_tag_rel"] {
            let (seq, idx) = scan_delta(&after, &before, table);
            assert_eq!(
                seq, 0,
                "round {round}: {table} was scanned sequentially:\n{captured:#?}"
            );
            assert!(idx > 0, "round {round}: {table} was not read by an index");
        }
        // the inverse read is a named prepared statement on this connection
        let prepared: i64 = tx
            .query(
                "SELECT count(*) FROM pg_prepared_statements WHERE statement = $1",
                &[&seq_sql],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(
            prepared, 1,
            "round {round}: the inverse read must be a cached prepared statement: {seq_sql}"
        );
        drop(tx);
    }
    h.cleanup().await.unwrap();
}
