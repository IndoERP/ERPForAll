//! Review round of slice S4b (2026-10-15): one test per finding about computes through
//! relations, new records and the bounded recompute paths.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

mod support;

use erp_sdk::{Cell, Command, ErrorKind, RawVals, Records, Transaction, Value};
use erp_testkit::assert_query_count;
use support::map;

async fn create(tx: &mut Transaction, model: &str, rows: &[&[(&str, Value)]]) -> Records {
    let records = tx.env().records(model).unwrap();
    let vals: Vec<_> = rows.iter().map(|r| map(r)).collect();
    records.create_values(tx, &vals).await.unwrap()
}

fn s(v: &str) -> Value {
    Value::str(v)
}

fn int(records: &Records, i: usize) -> Value {
    Value::Int(records.ids()[i])
}

fn one(records: &Records, i: usize) -> Records {
    records.browse_ids([records.rec_ids()[i]])
}

async fn write(tx: &mut Transaction, records: &Records, pairs: &[(&str, Value)]) {
    records.write_values(tx, &map(pairs)).await.unwrap();
}

async fn values(tx: &mut Transaction, records: &Records, field: &str) -> Vec<Value> {
    records.values(tx, field).await.unwrap()
}

async fn related_ids(tx: &mut Transaction, records: &Records, field: &str) -> Vec<i64> {
    let mut ids = records.related(tx, field).await.unwrap().ids();
    ids.sort_unstable();
    ids
}

// ----- item 1: a path through a non-stored related many2one --------------------------------------

/// Review item 1 (through a non-stored hop): the records behind a non-stored
/// related many2one are found by searching it, so changing the target's field recomputes them.
#[tokio::test]
async fn review_01_path_through_a_non_stored_related_many2one() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = create(
        &mut tx,
        "test_dep.hop_order",
        &[&[("name", s("A"))], &[("name", s("B"))]],
    )
    .await;
    let lines = create(
        &mut tx,
        "test_dep.hop_line",
        &[
            &[("order_id", int(&orders, 0))],
            &[("order_id", int(&orders, 0))],
            &[("order_id", int(&orders, 1))],
        ],
    )
    .await;
    assert_eq!(
        values(&mut tx, &lines, "order_name").await,
        [s("A"), s("A"), s("B")]
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    // the last step changes: the lines are found through the related many2one
    write(&mut tx, &one(&orders, 0), &[("name", s("A2"))]).await;
    assert_eq!(
        values(&mut tx, &lines, "order_name").await,
        [s("A2"), s("A2"), s("B")]
    );
    // the first step changes
    write(&mut tx, &one(&lines, 2), &[("order_id", int(&orders, 0))]).await;
    assert_eq!(
        values(&mut tx, &lines, "order_name").await,
        [s("A2"), s("A2"), s("A2")]
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        values(&mut tx, &lines, "order_name").await,
        [s("A2"), s("A2"), s("A2")],
        "persisted"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- item 2: old owners of a stored computed many2one ------------------------------------------

/// Review item 2 (with nothing cached): when a stored computed many2one changes,
/// its old value is loaded so the old owner's one2many dependents are notified too.
#[tokio::test]
async fn review_02_stored_computed_many2one_notifies_its_old_owner() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let containers = create(
        &mut tx,
        "test_dep.container",
        &[&[("name", s("Foo"))], &[("name", s("Bar"))]],
    )
    .await;
    let member = create(&mut tx, "test_dep.member", &[&[("name", s("Foo"))]]).await;
    assert_eq!(
        values(&mut tx, &containers, "member_count").await,
        [Value::Int(1), Value::Int(0)]
    );
    tx.flush().await.unwrap();
    // a new request: the member's old container is not cached
    tx.invalidate_all().await.unwrap();
    write(&mut tx, &member, &[("name", s("Bar"))]).await;
    assert_eq!(
        values(&mut tx, &containers, "member_count").await,
        [Value::Int(0), Value::Int(1)]
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        values(&mut tx, &containers, "member_count").await,
        [Value::Int(0), Value::Int(1)],
        "persisted"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- item 3: a many2many mirror with a domain ---------------------------------------------------

/// Review item 3 (with a filtered mirror): the cached value of a mirror many2many
/// with a domain does not list every owner, so marking does not take owners from it.
#[tokio::test]
async fn review_03_mirror_with_a_domain_is_not_used_to_find_owners() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let label = create(&mut tx, "test_dep.label", &[&[("name", s("L"))]]).await;
    let tagged = create(
        &mut tx,
        "test_dep.tagged",
        &[
            &[
                ("name", s("hidden")),
                ("label_ids", Value::list([int(&label, 0)])),
            ],
            &[
                ("name", s("shown")),
                ("label_ids", Value::list([int(&label, 0)])),
            ],
        ],
    )
    .await;
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    // the mirror's cached value leaves out the hidden owner
    assert_eq!(
        related_ids(&mut tx, &label, "tagged_ids").await,
        vec![tagged.ids()[1]]
    );
    assert_eq!(
        values(&mut tx, &tagged, "label_names").await,
        [s("L"), s("L")]
    );
    write(&mut tx, &label, &[("name", s("M"))]).await;
    assert_eq!(
        values(&mut tx, &tagged, "label_names").await,
        [s("M"), s("M")],
        "both owners recomputed"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- item 4: one2many over a non-stored related many2one ----------------------------------------

/// Review item 4: a one2many whose inverse is a non-stored related many2one
/// follows writes and deletions of the lines without a cache reset.
#[tokio::test]
async fn review_04_one2many_over_a_related_many2one_follows_writes_and_unlinks() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = create(
        &mut tx,
        "test_dep.order",
        &[&[("name", s("o1"))], &[("name", s("o2"))]],
    )
    .await;
    let lines = create(
        &mut tx,
        "test_dep.line",
        &[
            &[
                ("order_id", int(&orders, 0)),
                ("price_unit", Value::Float(1.0)),
            ],
            &[
                ("order_id", int(&orders, 0)),
                ("price_unit", Value::Float(1.0)),
            ],
            &[
                ("order_id", int(&orders, 1)),
                ("price_unit", Value::Float(1.0)),
            ],
        ],
    )
    .await;
    tx.flush().await.unwrap();
    let o1 = one(&orders, 0);
    let o2 = one(&orders, 1);
    assert_eq!(
        related_ids(&mut tx, &o1, "related_line_ids").await,
        vec![lines.ids()[0], lines.ids()[1]]
    );
    assert_eq!(
        related_ids(&mut tx, &o2, "related_line_ids").await,
        vec![lines.ids()[2]]
    );
    // a line moves to the other order
    write(&mut tx, &one(&lines, 0), &[("order_id", int(&orders, 1))]).await;
    assert_eq!(
        related_ids(&mut tx, &o1, "related_line_ids").await,
        vec![lines.ids()[1]]
    );
    assert_eq!(
        related_ids(&mut tx, &o2, "related_line_ids").await,
        vec![lines.ids()[0], lines.ids()[2]]
    );
    // a line is deleted
    one(&lines, 2).unlink(&mut tx).await.unwrap();
    assert_eq!(
        related_ids(&mut tx, &o2, "related_line_ids").await,
        vec![lines.ids()[0]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- item 5: convert_to_write keeps fields that save themselves ---------------------------------

/// Review item 5: `convert_to_write` of a new line keeps a non-stored field that
/// `create` accepts (an editable related field written on the line).
#[tokio::test]
async fn review_05_convert_to_write_keeps_editable_related_fields() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let discussions = tx.env().records("test_dep.discussion").unwrap();
    let discussion = tx
        .new_record(
            &discussions,
            RawVals::new()
                .set_cell("name", Cell::Str("D".into()))
                .set_commands(
                    "message_ids",
                    [Command::create(
                        RawVals::new().set_cell("author", Cell::Str("a".into())),
                    )],
                ),
            None,
            None,
        )
        .await
        .unwrap();
    let messages = discussion.related(&mut tx, "message_ids").await.unwrap();
    write(&mut tx, &messages, &[("discussion_name", s("D2"))]).await;
    let commands = discussion
        .x2many_commands(&mut tx, "message_ids")
        .await
        .unwrap();
    let create = commands
        .iter()
        .find_map(|c| match c {
            Command::Create(vals) => Some(vals),
            _ => None,
        })
        .expect("a CREATE command");
    assert_eq!(create.get("author"), Some(&Cell::Str("a".into())));
    assert_eq!(
        create.get("discussion_name"),
        Some(&Cell::Str("D2".into())),
        "{commands:?}"
    );
    assert!(
        create.get("name").is_none(),
        "readonly stored compute left out"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- item 13: reading settles only what it reaches ----------------------------------------------

/// Batch 1,000 here, 2,010 pending lines.
#[tokio::test]
async fn review_13_read_settles_only_the_records_it_reaches() {
    let h = erp_testkit::Harness::builder([
        &erp_test_models::base_users::ADDON,
        &erp_test_models::test_depends::ADDON,
    ])
    .config(erp_sdk::OrmConfig {
        recompute_batch: 1000,
        ..erp_sdk::OrmConfig::default()
    })
    .build()
    .await
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    let orders = tx.env().records("test_dep.order").unwrap();
    let vals: Vec<RawVals> = (0..201)
        .map(|o| {
            RawVals::new()
                .set_cell("name", Cell::Str(format!("o{o}").into()))
                .set_commands(
                    "line_ids",
                    (0..10).map(|_| {
                        Command::create(RawVals::new().set_cell("price_unit", Cell::Float(1.0)))
                    }),
                )
        })
        .collect();
    let created = orders.create(&mut tx, vals).await.unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    let lines = tx
        .env()
        .records("test_dep.line")
        .unwrap()
        .search(&mut tx, &erp_sdk::Domain::TRUE)
        .await
        .unwrap();
    assert_eq!(lines.len(), 2010, "more than one batch (1,000)");
    write(&mut tx, &lines, &[("price_unit", Value::Float(2.0))]).await;
    let first = tx
        .env()
        .records("test_dep.order")
        .unwrap()
        .browse(vec![created.ids()[0]])
        .unwrap();
    let started = std::time::Instant::now();
    let total = assert_query_count!(tx, >= 1, <= 4, {
        values(&mut tx, &first, "amount_total").await
    });
    let elapsed = started.elapsed();
    assert_eq!(total, [Value::Float(20.0)]);
    eprintln!("review 13: one total read with 2,010 pending lines in {elapsed:?}");
    assert!(
        tx.fields_to_compute()
            .contains(&"test_dep.line.price_subtotal".to_owned()),
        "the other lines stay pending: {:?}",
        tx.fields_to_compute()
    );
    tx.flush().await.unwrap();
    assert!(tx.fields_to_compute().is_empty());
    tx.invalidate_all().await.unwrap();
    let totals = values(&mut tx, &created, "amount_total").await;
    assert!(
        totals.iter().all(|t| *t == Value::Float(20.0)),
        "{:?}",
        &totals[..3]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- item 16: recursion through two hops --------------------------------------------------------

/// Review item 16 (through two hops): a stored field depending on
/// `link_id.chain_id.level` is computed level by level, never all records at once (which would
/// read values being computed).
#[tokio::test]
async fn review_16_recursion_through_two_hops_is_computed_level_by_level() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let rows: Vec<Vec<(&str, Value)>> = (0..6)
        .map(|i| vec![("name", s(&format!("c{i}")))])
        .collect();
    let refs: Vec<&[(&str, Value)]> = rows.iter().map(Vec::as_slice).collect();
    let chains = create(&mut tx, "test_dep.chain", &refs).await;
    let link_rows: Vec<Vec<(&str, Value)>> = (0..5)
        .map(|i| vec![("chain_id", int(&chains, i))])
        .collect();
    let link_refs: Vec<&[(&str, Value)]> = link_rows.iter().map(Vec::as_slice).collect();
    let links = create(&mut tx, "test_dep.chain_link", &link_refs).await;
    assert_eq!(
        values(&mut tx, &chains, "level").await,
        vec![Value::Int(0); 6]
    );
    tx.flush().await.unwrap();
    // deepest first, so the pending set is not in hierarchy order
    for i in (0..5).rev() {
        write(
            &mut tx,
            &one(&chains, i + 1),
            &[("link_id", int(&links, i))],
        )
        .await;
    }
    assert_eq!(
        values(&mut tx, &chains, "level").await,
        (0..6).map(Value::Int).collect::<Vec<_>>()
    );
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        values(&mut tx, &chains, "level").await,
        (0..6).map(Value::Int).collect::<Vec<_>>(),
        "persisted"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- item 17: new records ---------------------------------------------------------------------------

/// Review item 17: a many2one of a new record accepts only new records of this transaction, and
/// a failed `new()` leaves no partial record or reference behind.
#[tokio::test]
async fn review_17_new_record_values_and_failures() {
    let h = support::dep_orm().await;
    let mut tx = h.begin().await.unwrap();
    let orders = tx.env().records("test_dep.order").unwrap();
    // a negative id this transaction never created
    let foreign = -(1_i64 << 62) - 999;
    let err = tx
        .new_record(
            &orders,
            RawVals::new().set_cell("partner_id", Cell::Id(foreign)),
            None,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation, "{err}");
    // a new partner of this transaction is accepted
    let partners = tx.env().records("test_dep.partner").unwrap();
    let partner = tx
        .new_record(&partners, RawVals::new(), None, None)
        .await
        .unwrap();
    let order = tx
        .new_record(
            &orders,
            RawVals::new().set_cell("partner_id", Cell::Id(partner.rec_ids()[0].get())),
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(order.related(&mut tx, "partner_id").await.unwrap(), partner);
    // a failure inside the commands: nothing of the record is kept, nor its reference
    let err = tx
        .new_record(
            &orders,
            RawVals::new()
                .set_cell("name", Cell::Str("x".into()))
                .set_commands(
                    "line_ids",
                    [Command::create(
                        RawVals::new().set_cell("nope", Cell::Int(1)),
                    )],
                ),
            None,
            Some("r1"),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming, "{err}");
    let again = tx
        .new_record(&orders, RawVals::new(), None, Some("r1"))
        .await
        .unwrap();
    assert_eq!(
        again.value(&mut tx, "name").await.unwrap(),
        Value::False,
        "no value left from the failed new()"
    );
    drop(tx);
    h.cleanup().await.unwrap();
}
