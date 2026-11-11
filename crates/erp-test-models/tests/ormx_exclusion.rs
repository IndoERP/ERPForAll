//! typed exclusions over two-field periods, range fields, `WITHOUT OVERLAPS` keys,
//! `btree_gist`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::error::Error as _;
use std::time::Duration;

use erp_orm::SchemaSyncOptions;
use erp_registry::{ExclusionDecl, ModelDecl, ModelSpec, TableObjectDecl, TableObjectKind};
use erp_sdk::{
    Date, DateRange, DateTime, DatetimeRange, ErrorKind, RangeBounds, RecId, Records, Transaction,
    Vals, Value,
};
use erp_test_models::test_scale_excl::{self, Booking, Rate, Resource, Stay, Window};
use erp_test_models::{base_users, test_scale_excl::raw};
use erp_testkit::{Harness, HarnessError, assert_query_count};
use support::{dom, map, one, rows};

async fn orm() -> Harness {
    Harness::builder([&base_users::ADDON, &test_scale_excl::ADDON])
        .build()
        .await
        .expect("harness")
}

fn day(d: u32) -> Date {
    Date::from_ymd(2026, 1, d).unwrap()
}

/// 2026-01-05 at `h:m`.
fn at(h: u32, m: u32) -> DateTime {
    DateTime::from_parts(day(5), h, m, 0, 0).unwrap()
}

/// The table object declared as `name` (`_no_double_booking`) on `model`.
fn object<'a>(model: &'a ModelSpec, name: &str) -> &'a erp_registry::TableObject {
    model
        .table_objects
        .iter()
        .find(|o| o.name == name)
        .expect("declared")
}

/// The database name of the table object declared as `name`, as the registry composes it.
fn full_name(model: &ModelSpec, name: &str) -> String {
    object(model, name).full_name.clone()
}

/// The declared message of a table object, as the registry holds it.
fn declared_message(model: &ModelSpec, name: &str) -> &'static str {
    match object(model, name).kind {
        TableObjectKind::Exclusion(decl) => decl.message,
        TableObjectKind::TemporalKey(decl) => decl.message,
        other => panic!("not an exclusion: {other:?}"),
    }
}

/// `The operation cannot be completed: '<from>' must be before '<to>' on '<description>'`, from
/// the registry's labels.
fn period_message(model: &ModelSpec, from: &str, to: &str, words: &str) -> String {
    format!(
        "The operation cannot be completed: '{}' must {words} '{}' on '{}'",
        model.field(from).unwrap().string,
        model.field(to).unwrap().string,
        model.description
    )
}

async fn resource(tx: &mut Transaction, name: &str) -> RecId {
    let resources = tx.env().model::<Resource>().unwrap();
    let rec = resources
        .create(tx, vec![Vals::new().set(Resource::NAME, name)])
        .await
        .unwrap();
    RecId::from_real(rec.ids()[0]).unwrap()
}

async fn book(
    tx: &mut Transaction,
    resource: RecId,
    start: DateTime,
    stop: Option<DateTime>,
) -> erp_sdk::Result<Records> {
    let bookings = tx.env().model::<Booking>()?;
    let mut vals = Vals::new()
        .set(Booking::RESOURCE_ID, resource)
        .set(Booking::START, start);
    if let Some(stop) = stop {
        vals = vals.set(Booking::STOP, stop);
    }
    Ok(bookings.create(tx, vec![vals]).await?.as_records().clone())
}

/// The database error behind an ORM error (the constraint it names).
fn db_constraint(err: &erp_sdk::Error) -> Option<String> {
    let source = err.source()?;
    let pg = source.downcast_ref::<erp_testkit::tokio_postgres::Error>()?;
    pg.as_db_error()?.constraint().map(str::to_owned)
}

#[tokio::test]
async fn spec_ormx_016_concurrent_overlapping_bookings_exactly_one_commits() {
    let h = orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = resource(&mut tx, "Room 1").await;
    tx.commit().await.unwrap();
    drop(tx);

    // A books [10:00, 11:00) and holds its row uncommitted; B, whose snapshot cannot see A's row,
    // books the same period: B's INSERT waits for A, then fails when A commits
    let mut tx_a = h.begin().await.unwrap();
    book(&mut tx_a, r, at(10, 0), Some(at(11, 0)))
        .await
        .unwrap();
    let mut tx_b = h.begin().await.unwrap();
    let (b, a) = tokio::join!(book(&mut tx_b, r, at(10, 0), Some(at(11, 0))), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx_a.commit().await
    });
    a.unwrap();
    drop(tx_a);
    let err = b.unwrap_err();
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The operation cannot be completed: {}",
            declared_message(booking, "_no_double_booking")
        )
    );
    assert!(!err.message().contains("Key ("), "no server DETAIL");
    assert_eq!(err.sqlstate(), Some("23P01"));
    drop(tx_b);

    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(&mut tx, "SELECT count(*)::text FROM scale_booking")
            .await
            .as_deref(),
        Some("1"),
        "exactly one of the two bookings committed"
    );
    // adjacent `[)` periods do not overlap; the rule costs no statement (design §12: one INSERT)
    assert_query_count!(tx, >= 1, <= 1, {
        book(&mut tx, r, at(11, 0), Some(at(12, 0))).await.unwrap()
    });
    tx.commit().await.unwrap();
    drop(tx);
    // an open-ended booking (no stop) overlaps every later period
    let mut tx = h.begin().await.unwrap();
    let err = book(&mut tx, r, at(11, 30), None).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The operation cannot be completed: {}",
            declared_message(booking, "_no_double_booking")
        )
    );
    drop(tx);
    // another resource is free for the same period
    let mut tx = h.begin().await.unwrap();
    let other = resource(&mut tx, "Room 2").await;
    book(&mut tx, other, at(10, 0), None).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // `[]` dates: two stays that share one day overlap
    let mut tx = h.begin().await.unwrap();
    let stays = tx.env().model::<Stay>().unwrap();
    let stay = |a: u32, d: u32| {
        Vals::new()
            .set(Stay::ROOM, 7)
            .set(Stay::ARRIVAL, day(a))
            .set(Stay::DEPARTURE, day(d))
    };
    stays.create(&mut tx, vec![stay(1, 3)]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let err = stays.create(&mut tx, vec![stay(3, 5)]).await.unwrap_err();
    let stay_model = registry.model("scale.stay").unwrap();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The operation cannot be completed: {}",
            declared_message(stay_model, "_no_double_stay")
        )
    );
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    stays.create(&mut tx, vec![stay(4, 5)]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(registry);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_017_empty_or_inverted_periods_are_refused() {
    let h = orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = resource(&mut tx, "Room").await;
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let expected = period_message(booking, "start", "stop", "be before");
    // inverted, then empty under `[)`: refused before any statement
    for (start, stop) in [(at(11, 0), at(10, 0)), (at(10, 0), at(10, 0))] {
        let err = assert_query_count!(tx, <= 0, {
            book(&mut tx, r, start, Some(stop)).await.unwrap_err()
        });
        assert_eq!(err.kind(), ErrorKind::Validation);
        assert_eq!(err.message(), expected);
    }
    drop(tx);

    // a write that inverts the period of a cached record is refused before the flush
    let mut tx = h.begin().await.unwrap();
    let r = resource(&mut tx, "Room").await;
    let rec = book(&mut tx, r, at(10, 0), Some(at(11, 0))).await.unwrap();
    let err = assert_query_count!(tx, <= 0, {
        rec.write_values(&mut tx, &map(&[("stop", Value::DateTime(at(9, 0)))]))
            .await
            .unwrap_err()
    });
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), expected);
    drop(tx);

    // the generated CHECK refuses the same rows from raw SQL (23514), with the same text
    let mut tx = h.begin().await.unwrap();
    let r = resource(&mut tx, "Room").await;
    let err = tx
        .batch(&format!(
            "INSERT INTO scale_booking (resource_id, start, stop, state) \
             VALUES ({}, '2026-01-05 11:00:00', '2026-01-05 10:00:00', 'confirmed')",
            r.get()
        ))
        .await
        .unwrap_err();
    assert_eq!(err.sqlstate(), Some("23514"));
    let check_name =
        erp_sql::exclusion::period_check_name(&full_name(booking, "_no_double_booking"));
    assert_eq!(check_name, "scale_booking_no_double_booking_period_check");
    assert_eq!(db_constraint(&err), Some(check_name.clone()));
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.message(), expected);
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let check = one(
        &mut tx,
        &format!(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{check_name}'"
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        check,
        "CHECK (((start IS NULL) OR (stop IS NULL) OR (start < stop)))"
    );

    // `[]` bounds accept a period of one day, and refuse an inverted one
    let stays = tx.env().model::<Stay>().unwrap();
    stays
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Stay::ROOM, 1)
                    .set(Stay::ARRIVAL, day(2))
                    .set(Stay::DEPARTURE, day(2)),
            ],
        )
        .await
        .unwrap();
    let stay = registry.model("scale.stay").unwrap();
    let err = stays
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Stay::ROOM, 2)
                    .set(Stay::ARRIVAL, day(3))
                    .set(Stay::DEPARTURE, day(2)),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        period_message(stay, "arrival", "departure", "not be after")
    );
    drop(tx);
    drop(registry);
    h.cleanup().await.unwrap();
}

/// A write whose other period end is not cached adds no query: the CHECK refuses it at flush with
/// the same message.
#[tokio::test]
async fn uncached_period_end_is_refused_by_the_check_at_flush() {
    let h = orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = resource(&mut tx, "Room").await;
    let id = book(&mut tx, r, at(10, 0), Some(at(11, 0)))
        .await
        .unwrap()
        .ids()[0];
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let rec = tx
        .env()
        .records("scale.booking")
        .unwrap()
        .browse([id])
        .unwrap();
    let written = assert_query_count!(tx, <= 0, {
        rec.write_values(&mut tx, &map(&[("stop", Value::DateTime(at(9, 0)))]))
            .await
    });
    written.unwrap();
    let err = tx.flush().await.unwrap_err();
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        period_message(booking, "start", "stop", "be before")
    );
    assert_eq!(err.sqlstate(), Some("23514"));
    drop(tx);
    drop(registry);
    h.cleanup().await.unwrap();
}

/// Catalog SQL compared by structure: no casts, quotes, parentheses or blanks.
fn structure(sql: &str) -> String {
    sql.replace("::character varying", "")
        .replace("::text", "")
        .chars()
        .filter(|c| !matches!(c, '(' | ')' | '"' | ' '))
        .collect()
}

#[tokio::test]
async fn spec_ormx_018_the_filter_excludes_cancelled_records() {
    let h = orm().await;
    let mut tx = h.begin().await.unwrap();
    let r = resource(&mut tx, "Room").await;
    let cancelled = book(&mut tx, r, at(10, 0), Some(at(11, 0))).await.unwrap();
    cancelled
        .set_value(&mut tx, "state", Value::str("cancel"))
        .await
        .unwrap();
    tx.flush().await.unwrap();
    // the cancelled booking does not count
    book(&mut tx, r, at(10, 0), Some(at(11, 0))).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    // confirming it again makes the two overlap
    let mut tx = h.begin().await.unwrap();
    let again = tx
        .env()
        .records("scale.booking")
        .unwrap()
        .browse(cancelled.ids())
        .unwrap();
    again
        .set_value(&mut tx, "state", Value::str("confirmed"))
        .await
        .unwrap();
    let err = tx.flush().await.unwrap_err();
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The operation cannot be completed: {}",
            declared_message(booking, "_no_double_booking")
        )
    );
    drop(tx);
    // the catalog's predicate is the compiled filter
    let mut tx = h.begin().await.unwrap();
    let name = full_name(booking, "_no_double_booking");
    let definition = one(
        &mut tx,
        &format!("SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{name}'"),
    )
    .await
    .unwrap();
    let (_, predicate) = definition.split_once(" WHERE ").expect("a WHERE clause");
    let compiled =
        erp_sql::constant_filter::compile("[('state', '!=', 'cancel')]", booking).unwrap();
    assert_eq!(structure(predicate), structure(&compiled));
    assert_eq!(structure(predicate), "state<>'cancel'");
    // the canonical definition is the constraint's comment
    let comment = one(
        &mut tx,
        &format!(
            "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint \
             WHERE conname = '{name}'"
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        comment,
        format!(
            "EXCLUDE USING gist (\"resource_id\" WITH =, tsrange(\"start\", \"stop\", '[)') WITH \
             &&) WHERE ({compiled})"
        )
    );
    drop(tx);
    drop(registry);
    h.cleanup().await.unwrap();
    // a filter with a path, `uid` or a non-literal operand is a registry error
    assert_non_constant_filters_are_registry_errors();
}

/// A model extension that replaces `_no_double_booking` with `filter` (later declaration wins).
fn with_filter(filter: &'static str) -> ModelDecl {
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl {
        name: "_no_double_booking",
        kind: TableObjectKind::Exclusion(
            ExclusionDecl::new(("start", "stop"), RangeBounds::HalfOpen, "taken")
                .equal(&["resource_id"])
                .filter(filter),
        ),
    }]));
    ModelDecl::extension("test_scale_excl", "scale.booking", &["scale.booking"])
        .kind_follows_model(true)
        .table_objects(objects)
        .at("tests/ormx_exclusion.rs", 1)
}

/// The registry errors for filters that are not constant domains.
fn assert_non_constant_filters_are_registry_errors() {
    let cases = [
        (
            "[('resource_id.name', '=', 'x')]",
            "paths are not allowed ('resource_id.name')",
        ),
        (
            "[('resource_id', '=', uid)]",
            "only literal values are allowed, found the name 'uid'",
        ),
        (
            "[('state', '=', context.get('state'))]",
            "only literal values are allowed, found a call",
        ),
        (
            "[('start', '<', '2026-01-01')]",
            "operator '<' is not allowed (use =, !=, in or not in)",
        ),
    ];
    for (filter, reason) in cases {
        let err = Harness::builder([&base_users::ADDON, &test_scale_excl::ADDON])
            .with_decls([with_filter(filter)])
            .check()
            .unwrap_err();
        let HarnessError::Orm(err) = err else {
            panic!("expected an ORM build error, got {err}");
        };
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(
            err.message(),
            format!("exclusion '_no_double_booking' on 'scale.booking': filter {filter}: {reason}")
        );
    }
}

/// `btree_gist`; the upsert refusal of a `without_overlaps` key arrives with)
#[tokio::test]
async fn spec_ormx_019_without_overlaps_keys_on_range_fields() {
    let h = orm().await;
    let mut tx = h.begin().await.unwrap();
    let p = resource(&mut tx, "Product").await;
    tx.commit().await.unwrap();
    drop(tx);
    let rate = |from: u32, to: u32| {
        Vals::new().set(Rate::PRODUCT_ID, p).set(
            Rate::VALIDITY,
            DateRange::new(Some(day(from)), Some(day(to))),
        )
    };
    let mut tx_a = h.begin().await.unwrap();
    let rates_a = tx_a.env().model::<Rate>().unwrap();
    rates_a.create(&mut tx_a, vec![rate(1, 10)]).await.unwrap();
    let mut tx_b = h.begin().await.unwrap();
    let rates_b = tx_b.env().model::<Rate>().unwrap();
    let (b, a) = tokio::join!(rates_b.create(&mut tx_b, vec![rate(5, 20)]), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx_a.commit().await
    });
    a.unwrap();
    drop(tx_a);
    let err = b.unwrap_err();
    let registry = std::sync::Arc::clone(h.registry());
    let model = registry.model("scale.rate").unwrap();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The operation cannot be completed: {}",
            declared_message(model, "_rate_key")
        )
    );
    assert_eq!(err.sqlstate(), Some("23P01"));
    drop(tx_b);

    // an empty range is refused by the ORM, naming the field's label, before any statement
    let mut tx = h.begin().await.unwrap();
    let rates = tx.env().model::<Rate>().unwrap();
    let err = assert_query_count!(tx, <= 0, {
        rates.create(&mut tx, vec![rate(1, 1)]).await.unwrap_err()
    });
    drop(tx);
    let validity = model.field("validity").unwrap();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The operation cannot be completed: '{}' (scale.rate.validity) is an empty range: \
             its lower end must be before its upper end",
            validity.string
        )
    );
    // the key and the extension in the catalog
    let mut tx = h.begin().await.unwrap();
    let rates = tx.env().model::<Rate>().unwrap();
    let key = one(
        &mut tx,
        &format!(
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname = '{}'",
            full_name(model, "_rate_key")
        ),
    )
    .await
    .unwrap();
    assert_eq!(key, "UNIQUE (product_id, validity WITHOUT OVERLAPS)");
    assert_eq!(
        one(
            &mut tx,
            "SELECT extname::text FROM pg_extension WHERE extname = 'btree_gist'"
        )
        .await
        .as_deref(),
        Some("btree_gist")
    );
    // adjacent `[)` validities of one product, and any validity of another, are fine
    rates.create(&mut tx, vec![rate(10, 12)]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(registry);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_020_range_values_round_trip_and_operators() {
    let h = orm().await;
    let (d0, d1, d2) = (day(1), day(10), day(20));
    let mut tx = h.begin().await.unwrap();
    let p = resource(&mut tx, "Product").await;
    let q = resource(&mut tx, "Other").await;
    let rates = tx.env().model::<Rate>().unwrap();
    // typed write of an open-ended range
    let typed = rates
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Rate::PRODUCT_ID, p)
                    .set(Rate::VALIDITY, DateRange::new(Some(d1), None)),
            ],
        )
        .await
        .unwrap();
    // dynamic write of `[d1, false]`
    let dynamic = tx
        .env()
        .records("scale.rate")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[
                ("product_id", Value::Int(q.get())),
                ("validity", Value::list([Value::Date(d1), Value::False])),
            ])],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // read back from the database (fresh cache): typed and dynamic reads agree
    let mut tx = h.begin().await.unwrap();
    let rates = tx.env().model::<Rate>().unwrap();
    let typed_back = rates
        .browse(typed.ids())
        .unwrap()
        .get(&mut tx, Rate::VALIDITY)
        .await
        .unwrap();
    assert_eq!(typed_back, Some(DateRange::new(Some(d1), None)));
    let dynamic_back = tx
        .env()
        .records("scale.rate")
        .unwrap()
        .browse(dynamic.ids())
        .unwrap()
        .value(&mut tx, "validity")
        .await
        .unwrap();
    assert_eq!(dynamic_back, Value::from(typed_back.unwrap()));
    assert_eq!(
        erp_fields::convert::value_to_json(&dynamic_back).unwrap(),
        erp_fields::convert::value_to_json(&Value::list([Value::Date(d1), Value::False])).unwrap(),
        "`[d1, false]` on the dynamic surface"
    );
    // `overlaps` and `contains` find both rates
    let all = tx.env().records("scale.rate").unwrap();
    let mut both = typed.ids();
    both.extend(dynamic.ids());
    both.sort_unstable();
    let found = |records: Records| {
        let mut ids = records.ids();
        ids.sort_unstable();
        ids
    };
    let overlapping = all
        .search(
            &mut tx,
            &dom(erp_sdk::serde_json::json!([[
                "validity",
                "overlaps",
                [d0.to_string(), d2.to_string()]
            ]])),
        )
        .await
        .unwrap();
    assert_eq!(found(overlapping), both);
    let containing = all
        .search(
            &mut tx,
            &dom(erp_sdk::serde_json::json!([[
                "validity",
                "contains",
                d1.to_string()
            ]])),
        )
        .await
        .unwrap();
    assert_eq!(found(containing.clone()), both);
    // in memory, the same answers
    let in_memory = containing
        .filtered_domain(
            &mut tx,
            &dom(erp_sdk::serde_json::json!([[
                "validity",
                "contains",
                d1.to_string()
            ]])),
        )
        .await
        .unwrap();
    assert_eq!(found(in_memory), both);
    let none = all
        .search(
            &mut tx,
            &dom(erp_sdk::serde_json::json!([[
                "validity",
                "overlaps",
                [d0.to_string(), d1.to_string()]
            ]])),
        )
        .await
        .unwrap();
    assert!(none.ids().is_empty(), "[d0, d1) ends where the rates start");
    let unset = all
        .search(
            &mut tx,
            &dom(erp_sdk::serde_json::json!([["validity", "=", false]])),
        )
        .await
        .unwrap();
    assert!(unset.ids().is_empty());

    // an inverted range is refused with the field's label
    let rec = all.browse(typed.ids()).unwrap();
    let err = rec
        .write_values(
            &mut tx,
            &map(&[("validity", Value::list([Value::Date(d2), Value::Date(d1)]))]),
        )
        .await
        .unwrap_err();
    let registry = std::sync::Arc::clone(h.registry());
    let label = &registry
        .model("scale.rate")
        .unwrap()
        .field("validity")
        .unwrap()
        .string;
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The operation cannot be completed: '{label}' (scale.rate.validity) is an empty \
             range: its lower end must be before its upper end"
        )
    );
    drop(tx);
    // a 3-element list is the dynamic layer's wrong-type error, naming the expected shape
    let mut tx = h.begin().await.unwrap();
    let three = Value::list([Value::Date(d0), Value::Date(d1), Value::Date(d2)]);
    let err = tx
        .env()
        .records("scale.rate")
        .unwrap()
        .browse(typed.ids())
        .unwrap()
        .write_values(&mut tx, &map(&[("validity", three.clone())]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "Invalid value for scale.rate.validity: {three} (expected a range [lower, upper] of \
             dates, with false for an unbounded end)"
        )
    );
    drop(tx);

    // a datetime range and a `[]` date range: what is written is what is read
    let mut tx = h.begin().await.unwrap();
    let windows = tx.env().model::<Window>().unwrap();
    let opening = DatetimeRange::new(Some(at(9, 0)), Some(at(17, 30)));
    let days = DateRange::new(Some(d0), Some(d1));
    let w = windows
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Window::OPENING, opening)
                    .set(Window::DAYS, days),
            ],
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let windows = tx.env().model::<Window>().unwrap();
    let w = windows.browse(w.ids()).unwrap();
    assert_eq!(
        w.get(&mut tx, Window::OPENING).await.unwrap(),
        Some(opening)
    );
    assert_eq!(w.get(&mut tx, Window::DAYS).await.unwrap(), Some(days));
    // PostgreSQL keeps the `[]` daterange as `[d0, d1 + 1)`
    assert_eq!(
        one(&mut tx, "SELECT days::text FROM scale_window")
            .await
            .as_deref(),
        Some("[2026-01-01,2026-01-11)")
    );
    // `[]` holds its upper end
    let all = tx.env().records("scale.window").unwrap();
    let hit = all
        .search(
            &mut tx,
            &dom(erp_sdk::serde_json::json!([[
                "opening",
                "contains",
                at(17, 30).to_string()
            ]])),
        )
        .await
        .unwrap();
    assert_eq!(hit.ids(), w.ids());
    // an unbounded operand overlaps every set range, in SQL and in memory alike
    let everything = dom(erp_sdk::serde_json::json!([[
        "opening",
        "overlaps",
        [false, false]
    ]]));
    let in_sql = all.search(&mut tx, &everything).await.unwrap();
    assert_eq!(in_sql.ids(), w.ids());
    let in_memory = all
        .browse(w.ids())
        .unwrap()
        .filtered_domain(&mut tx, &everything)
        .await
        .unwrap();
    assert_eq!(in_memory.ids(), w.ids());
    drop(tx);
    drop(registry);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_022_btree_gist_is_installed_by_schema_sync() {
    // a database without btree_gist: the sync installs it in its own transaction
    let h = Harness::builder([&base_users::ADDON, &test_scale_excl::ADDON])
        .without_schema()
        .build()
        .await
        .unwrap();
    let has_btree_gist = "SELECT count(*)::text FROM pg_extension WHERE extname = 'btree_gist'";
    let mut tx = h.begin().await.unwrap();
    assert_eq!(one(&mut tx, has_btree_gist).await.as_deref(), Some("0"));
    tx.init_models(&[]).await.unwrap();
    assert_eq!(one(&mut tx, has_btree_gist).await.as_deref(), Some("1"));
    tx.rollback().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(&mut tx, has_btree_gist).await.as_deref(),
        Some("0"),
        "installed in the schema transaction (rolled back with it)"
    );
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(one(&mut tx, has_btree_gist).await.as_deref(), Some("1"));
    let booking_key = full_name(
        h.registry().model("scale.booking").unwrap(),
        "_no_double_booking",
    );
    assert_eq!(
        one(
            &mut tx,
            &format!("SELECT count(*)::text FROM pg_constraint WHERE conname = '{booking_key}'")
        )
        .await
        .as_deref(),
        Some("1")
    );
    drop(tx);
    h.cleanup().await.unwrap();

    // a role that may not create extensions: the install fails naming the extension and the
    // object, and nothing of it remains
    let h = Harness::builder([&base_users::ADDON, &test_scale_excl::ADDON])
        .without_schema()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let role = format!("erp_noext_{}", std::process::id());
    tx.batch(&format!(
        "CREATE ROLE {role} NOLOGIN; GRANT USAGE, CREATE ON SCHEMA public TO {role}; \
         SET LOCAL ROLE {role}"
    ))
    .await
    .unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Database);
    let server = err
        .source()
        .and_then(|s| s.downcast_ref::<erp_sdk::Error>())
        .map(|inner| inner.message().to_owned())
        .expect("the server's error is the source");
    assert_eq!(
        err.message(),
        format!(
            "model 'scale.booking' needs the PostgreSQL extension btree_gist for exclusion \
             '_no_double_booking'; it could not be installed: {server}; ask the database \
             administrator to run CREATE EXTENSION btree_gist"
        )
    );
    assert_eq!(err.sqlstate(), Some("42501"));
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(one(&mut tx, has_btree_gist).await.as_deref(), Some("0"));
    assert_eq!(
        one(
            &mut tx,
            &format!("SELECT count(*)::text FROM pg_roles WHERE rolname = '{role}'")
        )
        .await
        .as_deref(),
        Some("0"),
        "rolled back with the install"
    );
    drop(tx);
    h.cleanup().await.unwrap();

    // a native raw constraint stating `extensions = ["btree_gist"]` gets the same install
    let h = Harness::builder([&base_users::ADDON, &raw::ADDON])
        .without_schema()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(one(&mut tx, has_btree_gist).await.as_deref(), Some("1"));
    let raw_name = full_name(
        h.registry().model("scale.raw_booking").unwrap(),
        "_raw_no_overlap",
    );
    let raw_def = rows(
        &mut tx,
        &format!(
            "SELECT conname::text, contype::text FROM pg_constraint WHERE conname = '{raw_name}'"
        ),
    )
    .await;
    assert_eq!(
        raw_def,
        vec![vec![Some(raw_name.clone()), Some("x".to_owned())]]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// The data-only addon: its manifest, descriptor and model files.
fn data_only_addon() -> (
    &'static erp_registry::AddonDescriptor,
    std::path::PathBuf,
    erp_modules::ModuleName,
) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../erp-modules/tests/fixtures/test_scale_excl_data");
    let name = erp_modules::ModuleName::new("test_scale_excl_data").unwrap();
    let manifest = erp_modules::read_manifest_from_dir(&dir, &name)
        .unwrap()
        .unwrap();
    (erp_modules::addon_descriptor(&manifest).unwrap(), dir, name)
}

/// OID and comment of a constraint.
async fn constraint(tx: &mut Transaction, name: &str) -> Vec<Option<String>> {
    rows(
        tx,
        &format!(
            "SELECT oid::text, obj_description(oid, 'pg_constraint'), pg_get_constraintdef(oid) \
             FROM pg_constraint WHERE conname = '{name}'"
        ),
    )
    .await
    .into_iter()
    .next()
    .unwrap_or_default()
}

/// `models/*.toml`, the changed exclusion re-created, strict files; business keys, typed indexes,
/// `row_version` and the `partition` refusal from model files arrive with, IDX and
/// PART)
#[tokio::test]
async fn spec_ormx_021_typed_objects_from_model_files() {
    let (addon, dir, name) = data_only_addon();
    let mut overlay = erp_registry::Overlay::new();
    overlay
        .add_addon_defs(erp_modules::read_model_files(&dir, &name).unwrap())
        .unwrap();
    let h = Harness::builder([&base_users::ADDON, &test_scale_excl::ADDON, addon])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.hold").unwrap();
    let season = registry.model("scale.season").unwrap();
    let booking_key = full_name(booking, "_no_double_hold");
    let season_key = full_name(season, "_one_season");
    let mut tx = h.begin().await.unwrap();
    // the model file's exclusion: its filter and its message
    let filter =
        erp_sql::constant_filter::compile("[('state', '=', 'confirmed')]", booking).unwrap();
    let before = constraint(&mut tx, &booking_key).await;
    assert_eq!(
        before[1].as_deref(),
        Some(
            format!(
                "EXCLUDE USING gist (\"resource_id\" WITH =, tsrange(\"start\", \"stop\", '[)') \
                 WITH &&) WHERE ({filter})"
            )
            .as_str()
        )
    );
    let r = resource(&mut tx, "Room").await;
    let holds = tx.env().records("scale.hold").unwrap();
    let hold_vals = |start: DateTime, stop: Option<DateTime>| {
        map(&[
            ("resource_id", Value::Int(r.get())),
            ("start", Value::DateTime(start)),
            ("stop", stop.map_or(Value::False, Value::DateTime)),
        ])
    };
    holds
        .create_values(&mut tx, &[hold_vals(at(10, 0), Some(at(11, 0)))])
        .await
        .unwrap();
    let err = holds
        .create_values(&mut tx, &[hold_vals(at(10, 30), None)])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The operation cannot be completed: {}",
            declared_message(booking, "_no_double_hold")
        )
    );
    assert_eq!(
        declared_message(booking, "_no_double_hold"),
        "This resource is already held (model file)."
    );
    drop(tx);
    // the range field and the WITHOUT OVERLAPS key of the new model
    let mut tx = h.begin().await.unwrap();
    let season_before = constraint(&mut tx, &season_key).await;
    assert_eq!(
        season_before[2].as_deref(),
        Some("UNIQUE (resource_id, dates WITHOUT OVERLAPS)")
    );
    // the canonical definition in the comment, as the ORM compiles it
    let TableObjectKind::TemporalKey(key_decl) = object(season, "_one_season").kind else {
        panic!("_one_season is a temporal key");
    };
    assert_eq!(
        season_before[1].as_deref(),
        Some(erp_sql::exclusion::temporal_key_definition(&key_decl).as_str())
    );
    let r = resource(&mut tx, "Hall").await;
    let seasons = tx.env().records("scale.season").unwrap();
    let season_vals = |from: u32, to: u32| {
        map(&[
            ("resource_id", Value::Int(r.get())),
            (
                "dates",
                Value::list([Value::Date(day(from)), Value::Date(day(to))]),
            ),
        ])
    };
    seasons
        .create_values(&mut tx, &[season_vals(1, 10)])
        .await
        .unwrap();
    // `[]` dates: a season starting on the last day of another overlaps it
    let err = seasons
        .create_values(&mut tx, &[season_vals(10, 20)])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        format!(
            "The operation cannot be completed: {}",
            declared_message(season, "_one_season")
        )
    );
    drop(tx);

    // a second install with a changed period: the exclusion is dropped and re-created
    let changed = std::fs::read_to_string(dir.join("models/10_booking.toml"))
        .unwrap()
        .replace("bounds = \"[)\"", "bounds = \"[]\"");
    let mut defs =
        erp_modules::parse_model_file(&name, "models/10_booking.toml", &changed).unwrap();
    defs.extend(
        erp_modules::parse_model_file(
            &name,
            "models/20_season.toml",
            &std::fs::read_to_string(dir.join("models/20_season.toml")).unwrap(),
        )
        .unwrap(),
    );
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let registry2 = std::sync::Arc::new(
        erp_registry::RegistryBuilder::new([&base_users::ADDON, &test_scale_excl::ADDON, addon])
            .overlay(&overlay)
            .build()
            .unwrap(),
    );
    let orm2 = erp_sdk::Orm::builder(std::sync::Arc::clone(&registry2), h.db().pool().clone())
        .policy(erp_sdk::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let report = orm2.sync_schema().await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());
    let mut tx = h.begin().await.unwrap();
    let after = constraint(&mut tx, &booking_key).await;
    assert_ne!(
        after[0], before[0],
        "a new constraint (dropped and re-created)"
    );
    assert_eq!(
        after[1].as_deref(),
        Some(
            format!(
                "EXCLUDE USING gist (\"resource_id\" WITH =, tsrange(\"start\", \"stop\", '[]') \
                 WITH &&) WHERE ({filter})"
            )
            .as_str()
        )
    );
    // the unchanged key was left alone: same constraint, same comment
    assert_eq!(constraint(&mut tx, &season_key).await, season_before);
    drop(tx);
    drop(orm2);
    drop(registry);
    h.cleanup().await.unwrap();
}

#[test]
fn a_model_file_cannot_replace_a_native_exclusion() {
    let (addon, _dir, name) = data_only_addon();
    let text = "[[model]]\nextend = \"scale.booking\"\n\n[[model.exclude]]\n\
                name = \"_no_double_booking\"\nequal = [\"resource_id\"]\n\
                period = { from = \"start\", to = \"stop\", bounds = \"[)\" }\n\
                filter = \"[('id', '=', False)]\"\nmessage = \"never\"\n";
    let defs = erp_modules::parse_model_file(&name, "models/evil.toml", text).unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let err =
        erp_registry::RegistryBuilder::new([&base_users::ADDON, &test_scale_excl::ADDON, addon])
            .overlay(&overlay)
            .build()
            .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "SQL object 'scale_booking_no_double_booking' of model 'scale.booking' is declared by \
         Rust code in addon 'test_scale_excl' (crates/erp-test-models/src/test_scale_excl.rs:23) \
         and again at runtime in addon 'test_scale_excl_data' (models/evil.toml:1); a model file \
         or overlay cannot replace a native object, choose another name"
    );
}

/// S1 delta). A table object named like the foreign key of `resource_id` must not drop that key.
#[tokio::test]
async fn an_exclusion_named_like_a_foreign_key_does_not_replace_it() {
    let h = orm().await;
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl {
        name: "_resource_id_fkey",
        kind: TableObjectKind::Exclusion(
            ExclusionDecl::new(("start", "stop"), RangeBounds::HalfOpen, "never")
                .equal(&["resource_id"]),
        ),
    }]));
    let decl: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::extension("test_scale_excl", "scale.booking", &["scale.booking"])
            .kind_follows_model(true)
            .table_objects(objects)
            .at("tests/ormx_exclusion.rs", 1),
    ));
    let registry = std::sync::Arc::new(
        erp_registry::RegistryBuilder::new([&base_users::ADDON, &test_scale_excl::ADDON])
            .with_decls([decl])
            .build()
            .unwrap(),
    );
    let orm2 = erp_sdk::Orm::builder(std::sync::Arc::clone(&registry), h.db().pool().clone())
        .policy(erp_sdk::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let err = orm2.sync_schema().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "exclusion '_resource_id_fkey' on 'scale.booking': the table 'scale_booking' already has \
         a constraint named 'scale_booking_resource_id_fkey' (foreign key) that the ORM did not \
         create for this object; it is not replaced, rename the object"
    );
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT count(*)::text FROM pg_constraint WHERE contype = 'f' \
             AND conname = 'scale_booking_resource_id_fkey'"
        )
        .await
        .as_deref(),
        Some("1")
    );
    drop(tx);
    drop(orm2);
    h.cleanup().await.unwrap();
}

#[test]
fn a_model_file_cannot_share_the_table_of_a_native_model() {
    let (addon, _dir, name) = data_only_addon();
    let text = "[[model]]\nname = \"scale.shadow\"\ndescription = \"Shadow\"\n\
                table = \"scale_booking\"\n\n\
                [[model.field]]\nname = \"resource_id\"\ntype = \"many2one\"\n\
                comodel = \"scale.resource\"\n\n\
                [[model.field]]\nname = \"start\"\ntype = \"datetime\"\n\n\
                [[model.field]]\nname = \"stop\"\ntype = \"datetime\"\n\n\
                [[model.exclude]]\nname = \"_no_double_booking\"\n\
                equal = [\"resource_id\"]\n\
                period = { from = \"start\", to = \"stop\", bounds = \"[)\" }\n\
                message = \"never\"\n";
    let defs = erp_modules::parse_model_file(&name, "models/shadow.toml", text).unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let err =
        erp_registry::RegistryBuilder::new([&base_users::ADDON, &test_scale_excl::ADDON, addon])
            .overlay(&overlay)
            .build()
            .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "model 'scale.shadow' (addon 'test_scale_excl_data', models/shadow.toml:1) declared at \
         runtime takes the table 'scale_booking' of model 'scale.booking' \
         (crates/erp-test-models/src/test_scale_excl.rs:23); a model declared at runtime needs a \
         table of its own"
    );
}

/// review S1 delta)
#[tokio::test]
async fn a_hand_commented_constraint_is_not_replaced() {
    let h = orm().await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "ALTER TABLE scale_booking ADD CONSTRAINT scale_booking_hand EXCLUDE USING gist \
         (resource_id WITH =, tsrange(start, stop) WITH &&); \
         COMMENT ON CONSTRAINT scale_booking_hand ON scale_booking IS 'kept by hand'",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl {
        name: "_hand",
        kind: TableObjectKind::Exclusion(
            ExclusionDecl::new(("start", "stop"), RangeBounds::HalfOpen, "never")
                .equal(&["resource_id"]),
        ),
    }]));
    let decl: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::extension("test_scale_excl", "scale.booking", &["scale.booking"])
            .kind_follows_model(true)
            .table_objects(objects)
            .at("tests/ormx_exclusion.rs", 1),
    ));
    let registry = std::sync::Arc::new(
        erp_registry::RegistryBuilder::new([&base_users::ADDON, &test_scale_excl::ADDON])
            .with_decls([decl])
            .build()
            .unwrap(),
    );
    let orm2 = erp_sdk::Orm::builder(std::sync::Arc::clone(&registry), h.db().pool().clone())
        .policy(erp_sdk::StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let err = orm2.sync_schema().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "exclusion '_hand' on 'scale.booking': the table 'scale_booking' already has a \
         constraint named 'scale_booking_hand' (exclusion constraint) that the ORM did not create \
         for this object; it is not replaced, rename the object"
    );
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint \
             WHERE conname = 'scale_booking_hand'"
        )
        .await
        .as_deref(),
        Some("kept by hand")
    );
    drop(tx);
    drop(orm2);
    h.cleanup().await.unwrap();
}

/// Rows seeded before the objects are declared; the threshold of the large-table tests is below.
const SEEDED: i64 = 12;

/// `exclusion '<name>'` or `key '<name>'` of the declared object, the refusal's wording.
fn refusal(model: &ModelSpec, name: &str, what: &str, kind: &str) -> String {
    let (m, t) = (model.name, &model.table);
    format!(
        "cannot add {what} to '{m}' during the install: table '{t}' has more than 10 rows and \
         {kind} has no online build. Run `erp db build-object -d <db> --model {m} --object {name}` \
         in a maintenance window (it holds ACCESS EXCLUSIVE on the table for the build), then \
         retry the install"
    )
}

/// The constraints of `names` that exist.
async fn present(tx: &mut Transaction, names: &[String]) -> Vec<String> {
    let list = names
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut found: Vec<String> = rows(
        tx,
        &format!("SELECT conname FROM pg_constraint WHERE conname IN ({list}) ORDER BY 1"),
    )
    .await
    .into_iter()
    .map(|row| row.into_iter().next().flatten().unwrap())
    .collect();
    found.sort();
    found
}

/// table above `large_table_rows` are refused with the `UserError` naming `erp db build-object`
/// and leave nothing behind, below the threshold they install as before; the command is covered
/// whole by `erp-cli/tests/test_db.rs` and in detail by `ormx_build_object.rs`)
#[tokio::test]
async fn spec_ormx_032_exclusions_on_a_large_table_are_refused_in_the_install_path() {
    let h = orm().await;
    let registry = std::sync::Arc::clone(h.registry());
    let booking = registry.model("scale.booking").unwrap();
    let rate = registry.model("scale.rate").unwrap();
    let names = [
        full_name(booking, "_no_double_booking"),
        erp_sql::exclusion::period_check_name(&full_name(booking, "_no_double_booking")),
        full_name(rate, "_rate_key"),
    ];
    // a database where the objects are not installed yet, and tables above the threshold
    let mut tx = h.begin().await.unwrap();
    for (table, name) in [
        (&booking.table, &names[1]),
        (&booking.table, &names[0]),
        (&rate.table, &names[2]),
    ] {
        tx.execute(&format!("ALTER TABLE {table} DROP CONSTRAINT {name}"), &[])
            .await
            .unwrap();
    }
    tx.execute(
        "INSERT INTO scale_resource (name) SELECT 'r' || g FROM generate_series(1, $1::int8) g",
        &[&SEEDED],
    )
    .await
    .unwrap();
    // disjoint periods: every row could be indexed, only the size refuses
    tx.execute(
        "INSERT INTO scale_booking (resource_id, start, stop, state) \
         SELECT id, timestamp '2026-01-05' + id * interval '1 hour', \
                timestamp '2026-01-05' + id * interval '1 hour' + interval '30 minutes', 'confirmed' \
           FROM scale_resource",
        &[],
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO scale_rate (product_id, validity, amount) \
         SELECT id, daterange(date '2026-01-01', date '2026-02-01'), 1 FROM scale_resource",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    assert!(present_none(&h, &names).await);

    let large = SchemaSyncOptions::new().large_table_rows(10);
    for (model, name, what, kind) in [
        (
            booking,
            "_no_double_booking",
            "exclusion '_no_double_booking'",
            "an exclusion constraint",
        ),
        (
            rate,
            "_rate_key",
            "key '_rate_key'",
            "a WITHOUT OVERLAPS key",
        ),
    ] {
        let mut tx = h.begin().await.unwrap();
        let err = tx.init_models_with(&[model.id], &large).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UserError, "{what}");
        assert_eq!(err.message(), refusal(model, name, what, kind));
        tx.rollback().await.unwrap();
        drop(tx);
        assert!(
            present_none(&h, &names).await,
            "nothing left behind: {what}"
        );
    }

    // below the threshold the install is unchanged
    let mut tx = h.begin().await.unwrap();
    let sync = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(1000))
        .await
        .unwrap();
    assert!(sync.is_complete(), "no deferred work below the threshold");
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let mut expected = names.to_vec();
    expected.sort();
    assert_eq!(present(&mut tx, &names).await, expected);
    drop(tx);
    // and a database that has them is left alone whatever the size
    let mut tx = h.begin().await.unwrap();
    let sync = tx.init_models_with(&[], &large).await.unwrap();
    assert!(sync.is_complete());
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

/// None of the constraints of `names` exists.
async fn present_none(h: &Harness, names: &[String]) -> bool {
    let mut tx = h.begin().await.unwrap();
    present(&mut tx, names).await.is_empty()
}
