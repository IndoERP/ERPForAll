//! Slice S7a: table fill factor, keyset pagination, prepared-statement shapes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
use erp_sdk::serde_json::json;
use erp_sdk::{
    Cursor, Domain, ErrorKind, Orm, Records, SUPERUSER_ID, SearchOptions, StaticPolicy,
    Transaction, Value,
};
use erp_test_models::test_depends;
use erp_testkit::{Harness, assert_query_count};
use support::{dom, one};

static FF: AddonDescriptor = AddonDescriptor {
    name: "test_ff",
    depends: &["base"],
    exact_only: false,
};

static FF_EXT: AddonDescriptor = AddonDescriptor {
    name: "test_ff_ext",
    depends: &["test_ff"],
    exact_only: false,
};

fn hot(fillfactor: Option<u8>) -> ModelDecl {
    let decl = ModelDecl::new("test_ff", "x.hot").fields(const { &[FieldDecl::integer("n")] });
    match fillfactor {
        Some(ff) => decl.fillfactor(ff),
        None => decl,
    }
}

const RELOPTIONS: &str =
    "SELECT array_to_string(reloptions, ',') FROM pg_class WHERE relname = 'x_hot'";

/// Schema sync of the same database with another version of the model.
async fn resync(h: &Harness, decl: ModelDecl) -> (String, Option<String>) {
    let decl: &'static ModelDecl = Box::leak(Box::new(decl));
    let registry = Arc::new(
        RegistryBuilder::new([&FF])
            .with_decls([decl])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let ddl = tx.captured().join("\n");
    let options = one(&mut tx, RELOPTIONS).await;
    tx.commit().await.unwrap();
    (ddl, options)
}

/// `fillfactor` is a table attribute: set at creation, kept in sync when the
/// declaration changes, and left alone when the model does not declare one.
#[tokio::test]
async fn fillfactor_is_a_table_attribute() {
    let h = Harness::builder([&FF])
        .with_decls([hot(Some(70))])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(&mut tx, RELOPTIONS).await.as_deref(),
        Some("fillfactor=70")
    );
    tx.commit().await.unwrap();
    // idempotent
    let (ddl, options) = resync(&h, hot(Some(70))).await;
    assert!(!ddl.contains("SET (fillfactor"), "{ddl}");
    assert_eq!(options.as_deref(), Some("fillfactor=70"));
    // a changed declaration is applied
    let (ddl, options) = resync(&h, hot(Some(85))).await;
    assert!(
        ddl.contains("ALTER TABLE \"x_hot\" SET (fillfactor = 85)"),
        "{ddl}"
    );
    assert_eq!(options.as_deref(), Some("fillfactor=85"));
    // no declaration: whatever the database has stays (a DBA may tune it)
    let (ddl, options) = resync(&h, hot(None)).await;
    assert!(!ddl.contains("SET (fillfactor"), "{ddl}");
    assert_eq!(options.as_deref(), Some("fillfactor=85"));
    h.cleanup().await.unwrap();
}

/// the newest declaration wins (like `_order`); PostgreSQL's range is checked when
/// the registry is built; the macro attribute reaches the table.
#[tokio::test]
async fn fillfactor_resolution_and_range() {
    let base: &'static ModelDecl = Box::leak(Box::new(hot(Some(70))));
    let ext: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("test_ff_ext", "x.hot")
            .inherit(&["x.hot"])
            .fillfactor(90),
    ));
    let registry = RegistryBuilder::new([&FF, &FF_EXT])
        .with_decls([base, ext])
        .build()
        .unwrap();
    assert_eq!(registry.model("x.hot").unwrap().fillfactor, Some(90));
    let plain: &'static ModelDecl = Box::leak(Box::new(hot(None)));
    let registry = RegistryBuilder::new([&FF])
        .with_decls([plain])
        .build()
        .unwrap();
    assert_eq!(registry.model("x.hot").unwrap().fillfactor, None);
    for bad in [0, 9, 101] {
        let decl: &'static ModelDecl = Box::leak(Box::new(hot(Some(bad))));
        let err = RegistryBuilder::new([&FF])
            .with_decls([decl])
            .build()
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert!(err.message().contains("fillfactor"), "{}", err.message());
    }
    // `#[model(..., fillfactor = 80)]` on the bench-shaped line model
    let h = Harness::builder([&test_depends::ADDON])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        one(
            &mut tx,
            "SELECT array_to_string(reloptions, ',') FROM pg_class WHERE relname = \
             'test_dep_line'"
        )
        .await
        .as_deref(),
        Some("fillfactor=80")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// searches filtered by domain literals run as unnamed statements (planned for their
/// values, nothing cached on the connection); searches with only `LIMIT`/`OFFSET` parameters and
/// reads by id are prepared once and reused. Results and statement counts are unchanged.
#[tokio::test]
async fn literal_searches_are_planned_per_call() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    let rows: Vec<_> = (0..20)
        .map(|i| {
            support::map(&[
                ("name", Value::str(format!("p{i:02}"))),
                ("number", Value::Int(i % 3)),
            ])
        })
        .collect();
    let created = partners.create_values(&mut tx, &rows).await.unwrap();
    tx.commit().await.unwrap();

    let before = tx.prepared_statements();
    for n in 0..3 {
        let found = assert_query_count!(tx, >= 1, <= 1, {
            partners
                .search(
                    &mut tx,
                    &dom(json!([["number", "=", n], ["name", "ilike", "p"]])),
                )
                .await
                .unwrap()
        });
        assert_eq!(found.len(), [7, 7, 6][n as usize], "{found:?}");
    }
    assert_eq!(
        tx.prepared_statements(),
        before,
        "literal searches are not cached"
    );

    // only LIMIT/OFFSET parameters: prepared once, reused
    let mut pages = Vec::new();
    for offset in [0, 5, 10] {
        let page = partners
            .search_with(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::new().order("name").limit(5).offset(offset),
            )
            .await
            .unwrap();
        pages.extend(page.ids());
    }
    assert_eq!(pages.len(), 15);
    let after_pages = tx.prepared_statements();
    // two shapes: without OFFSET (first page) and with it
    assert_eq!(after_pages - before, 2, "{before} -> {after_pages}");
    // reads by id are cached statements
    tx.invalidate_records(&created, None).await.unwrap();
    assert_eq!(created.values(&mut tx, "name").await.unwrap().len(), 20);
    assert_eq!(tx.prepared_statements(), after_pages + 1);
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- keyset pagination ------------------------------------------------------------

/// 30 `test_search.mixed` records with repeated values and NULLs in every column.
async fn mixed_rows(tx: &mut Transaction) -> Records {
    let mixed = tx.env().records("test_search.mixed").unwrap();
    let mut rows = Vec::new();
    for i in 0..30i64 {
        let mut row: Vec<(&str, Value)> = Vec::new();
        if i % 5 != 0 {
            row.push(("number", Value::Int(i % 4)));
        }
        if i % 7 != 0 {
            row.push(("x", Value::Float((i % 3) as f64 / 2.0)));
        }
        row.push(("amount", Value::Int(i % 5)));
        if i % 6 != 0 {
            row.push(("name", Value::str(format!("n{}", i % 5))));
        }
        if i % 8 != 0 {
            row.push(("date", Value::str(format!("2024-01-0{}", 1 + i % 4))));
            row.push(("moment", Value::str(format!("2024-01-01 0{}:00:00", i % 3))));
        }
        if i % 10 != 0 {
            row.push(("flag", Value::Bool(i % 2 == 0)));
        }
        if i % 9 != 0 {
            row.push(("state", Value::str(["a", "b", "c"][(i % 3) as usize])));
        }
        rows.push(support::map(&row));
    }
    let created = mixed.create_values(tx, &rows).await.unwrap();
    // parents: every record after the tenth points at one of the first three
    for (n, id) in created.ids().into_iter().enumerate().skip(10) {
        let _ = id;
        created
            .at(isize::try_from(n).unwrap())
            .unwrap()
            .set_value(tx, "parent_id", Value::Int(created.ids()[n % 3]))
            .await
            .unwrap();
    }
    tx.flush().await.unwrap();
    created
}

/// Every page after the previous page's cursor: one statement per page, two for at most one page
/// (where a nullable leading term reaches its NULL rows).
async fn walk(tx: &mut Transaction, model: &Records, order: &str, limit: u64) -> Vec<i64> {
    let opts = SearchOptions::new().order(order).limit(limit);
    let mut out = Vec::new();
    let mut cursor: Option<Cursor> = None;
    let mut two_statement_pages = 0;
    for _ in 0..100 {
        tx.invalidate_all().await.unwrap();
        let before = tx.query_count();
        let page = assert_query_count!(tx, >= 1, <= 2, {
            model
                .search_after(tx, &Domain::TRUE, &opts, cursor.as_ref())
                .await
                .unwrap()
        });
        if tx.query_count() - before == 2 {
            two_statement_pages += 1;
            assert!(two_statement_pages <= 1, "order {order:?}");
        }
        assert!(page.records.len() as u64 <= limit);
        out.extend(page.records.ids());
        match page.next {
            Some(next) => {
                assert_eq!(page.records.len() as u64, limit);
                cursor = Some(next);
            }
            None => return out,
        }
    }
    panic!("no last page for {order}");
}

/// walking pages with cursors returns exactly the full search's records in its
/// order, for orders mixing directions, NULLS clauses, nullable columns of every type, booleans,
/// selections by declaration order and a many2one by id.
#[tokio::test]
async fn keyset_pages_follow_the_order() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    mixed_rows(&mut tx).await;
    let mixed = tx.env().records("test_search.mixed").unwrap();
    for order in [
        "",
        "id desc",
        "number desc, name",
        "name, number desc",
        "x desc nulls last",
        "amount, date desc",
        "moment nulls first, id desc",
        "flag desc, state",
        "state desc, number",
        "parent_id, number desc nulls last",
        "date, x, name desc nulls first",
    ] {
        let full = mixed
            .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order(order))
            .await
            .unwrap()
            .ids();
        assert_eq!(full.len(), 30);
        for limit in [4, 7, 30, 31] {
            assert_eq!(
                walk(&mut tx, &mixed, order, limit).await,
                full,
                "order {order:?}, limit {limit}"
            );
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// rows inserted before the cursor do not shift the next page (an `OFFSET` page
/// would repeat a record); the domain applies on every page; a same-direction order of NOT NULL
/// terms is one row comparison.
#[tokio::test]
async fn keyset_pages_are_stable_and_filtered() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    mixed_rows(&mut tx).await;
    let mixed = tx.env().records("test_search.mixed").unwrap();
    let flagged = dom(json!([["flag", "=", true]]));
    let opts = SearchOptions::new().order("name, id").limit(4);
    let full = mixed
        .search_with(&mut tx, &flagged, &SearchOptions::new().order("name, id"))
        .await
        .unwrap()
        .ids();
    assert_eq!(full.len(), 12);
    let first = mixed
        .search_after(&mut tx, &flagged, &opts, None)
        .await
        .unwrap();
    assert_eq!(first.records.ids(), full[..4]);
    let next = first.next.clone().unwrap();
    // a record sorting before the cursor
    mixed
        .create_values(
            &mut tx,
            &[support::map(&[
                ("name", Value::str("a")),
                ("flag", Value::Bool(true)),
            ])],
        )
        .await
        .unwrap();
    let second = mixed
        .search_after(&mut tx, &flagged, &opts, Some(&next))
        .await
        .unwrap();
    assert_eq!(second.records.ids(), full[4..8]);
    // the cursor is the last record's key, `id` last
    assert_eq!(next.order(), "name, id");
    assert_eq!(next.values().len(), 2);
    assert_eq!(next.values()[1], Value::Int(full[3]));

    tx.capture_statements(true);
    let by_id = mixed
        .search_after(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("id desc").limit(5),
            None,
        )
        .await
        .unwrap();
    mixed
        .search_after(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("id desc").limit(5),
            by_id.next.as_ref(),
        )
        .await
        .unwrap();
    let sql = tx.captured().last().cloned().unwrap();
    assert!(
        sql.contains("\"test_search_mixed\".\"id\" < $1::int8"),
        "{sql}"
    );
    assert!(!sql.contains("OFFSET"), "{sql}");
    drop(tx);
    h.cleanup().await.unwrap();
}

static KS: AddonDescriptor = AddonDescriptor {
    name: "test_ks",
    depends: &["base"],
    exact_only: false,
};

/// misuse fails loudly: a cursor of another order or with values that do not fit,
/// an offset, no limit (`Validation`); ordering through a comodel's order (`Programming`).
#[tokio::test]
async fn keyset_misuse_is_an_error() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    mixed_rows(&mut tx).await;
    let mixed = tx.env().records("test_search.mixed").unwrap();
    let opts = SearchOptions::new().order("number").limit(3);
    let page = mixed
        .search_after(&mut tx, &Domain::TRUE, &opts, None)
        .await
        .unwrap();
    let next = page.next.unwrap();
    let errors = [
        mixed
            .search_after(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::new().order("name").limit(3),
                Some(&next),
            )
            .await,
        mixed
            .search_after(
                &mut tx,
                &Domain::TRUE,
                &opts,
                Some(&Cursor::new("number", vec![Value::Int(1)])),
            )
            .await,
        mixed
            .search_after(
                &mut tx,
                &Domain::TRUE,
                &opts,
                Some(&Cursor::new("number", vec![Value::str("x"), Value::Int(1)])),
            )
            .await,
        // review M1: NULL for a term that cannot be NULL (the id)
        mixed
            .search_after(
                &mut tx,
                &Domain::TRUE,
                &opts,
                Some(&Cursor::new("number", vec![Value::Int(1), Value::False])),
            )
            .await,
        mixed
            .search_after(&mut tx, &Domain::TRUE, &opts.clone().offset(3), None)
            .await,
        mixed
            .search_after(
                &mut tx,
                &Domain::TRUE,
                &SearchOptions::new().order("number"),
                None,
            )
            .await,
    ];
    for result in errors {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Validation);
    }
    drop(tx);
    h.cleanup().await.unwrap();

    let parent = ModelDecl::new("test_ks", "x.parent")
        .order("name")
        .fields(const { &[FieldDecl::char("name")] });
    let child = ModelDecl::new("test_ks", "x.child")
        .fields(const { &[FieldDecl::many2one("parent_id", "x.parent")] });
    let h = Harness::builder([&KS])
        .with_decls([parent, child])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let children = tx.env().records("x.child").unwrap();
    let err = children
        .search_after(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("parent_id").limit(3),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(err.message().contains("parent_id.id"), "{}", err.message());
    // by id it works
    children
        .search_after(
            &mut tx,
            &Domain::TRUE,
            &SearchOptions::new().order("parent_id.id").limit(3),
            None,
        )
        .await
        .unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

// ----- INSERT shapes -----------------------------------------------------------------

/// one create whose rows give every subset of six optional columns (64 column sets) is
/// one INSERT and one prepared statement (before: an id reservation plus one INSERT and one
/// prepared statement per column set, 65 statements); ids follow the input order and absent
/// columns read NULL; a second call with other sets reuses nothing it does not need.
#[tokio::test]
async fn mixed_column_sets_are_one_insert() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let mixed = tx.env().records("test_search.mixed").unwrap();
    let columns: [(&str, Value); 6] = [
        ("number", Value::Int(7)),
        ("x", Value::Float(0.5)),
        ("name", Value::str("n")),
        ("date", Value::str("2024-01-02")),
        ("flag", Value::Bool(true)),
        ("state", Value::str("b")),
    ];
    let rows: Vec<_> = (0..64usize)
        .map(|mask| {
            let pairs: Vec<(&str, Value)> = columns
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, (k, v))| (*k, v.clone()))
                .collect();
            support::map(&pairs)
        })
        .collect();
    // warm the connection with something unrelated, then count what the create adds
    mixed.search(&mut tx, &Domain::TRUE).await.unwrap();
    let prepared = tx.prepared_statements();
    tx.capture_statements(true);
    let created = assert_query_count!(tx, >= 1, <= 1, {
        mixed.create_values(&mut tx, &rows).await.unwrap()
    });
    assert!(
        tx.captured()[0].contains("INSERT INTO"),
        "{:?}",
        tx.captured()
    );
    tx.capture_statements(false);
    assert_eq!(tx.prepared_statements(), prepared + 1);
    let ids = created.ids();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "ids follow the input order"
    );
    // the values as stored (read back from the database)
    tx.invalidate_all().await.unwrap();
    for (mask, id) in ids.iter().enumerate() {
        let names = support::rows(
            &mut tx,
            &format!(
                "SELECT number::text, x::text, name, date::text, flag::text, state \
                 FROM test_search_mixed WHERE id = {id}"
            ),
        )
        .await;
        let row = &names[0];
        let expect = ["7", "0.5", "n", "2024-01-02", "true", "b"];
        for (i, cell) in row.iter().enumerate() {
            if mask & (1 << i) != 0 {
                assert_eq!(cell.as_deref(), Some(expect[i]), "row {mask} column {i}");
            } else {
                assert_eq!(cell.as_deref(), None, "row {mask} column {i}");
            }
        }
    }
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review M2: a cursor survives a JSON round trip (dates, datetimes with microseconds, decimals,
/// floats, NULLs) and continues exactly where the page ended; NaN is refused at both ends.
#[tokio::test]
async fn keyset_cursor_json_round_trip() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    mixed_rows(&mut tx).await;
    // microseconds on the timestamps
    tx.execute(
        "UPDATE test_search_mixed SET moment = moment + (id * interval '1 microsecond')",
        &[],
    )
    .await
    .unwrap();
    tx.invalidate_all().await.unwrap();
    let mixed = tx.env().records("test_search.mixed").unwrap();
    for order in [
        "moment desc, amount",
        "date, x desc, amount desc",
        "amount, name",
    ] {
        let full = mixed
            .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().order(order))
            .await
            .unwrap()
            .ids();
        let opts = SearchOptions::new().order(order).limit(4);
        let mut got = Vec::new();
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = mixed
                .search_after(&mut tx, &Domain::TRUE, &opts, cursor.as_ref())
                .await
                .unwrap();
            got.extend(page.records.ids());
            match page.next {
                Some(next) => {
                    let text = next.to_json().unwrap().to_string();
                    let parsed: erp_sdk::serde_json::Value =
                        erp_sdk::serde_json::from_str(&text).unwrap();
                    cursor = Some(Cursor::from_json(&parsed).unwrap());
                }
                None => break,
            }
        }
        assert_eq!(got, full, "{order}");
    }
    let err = Cursor::new("x", vec![Value::Float(f64::NAN)])
        .to_json()
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    let opts = SearchOptions::new().order("x").limit(3);
    let err = mixed
        .search_after(
            &mut tx,
            &Domain::TRUE,
            &opts,
            Some(&Cursor::new(
                "x",
                vec![Value::Float(f64::NAN), Value::Int(1)],
            )),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review M3: `create` pads the columns some rows do not give with NULL, which equals omitting
/// them only while ORM columns have no database default; schema sync refuses a table whose ORM
/// column has one (set outside the ORM), naming the column.
#[tokio::test]
async fn database_defaults_on_orm_columns_are_refused() {
    let h = Harness::builder([&FF])
        .with_decls([hot(None)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.batch("ALTER TABLE x_hot ALTER COLUMN n SET DEFAULT 5")
        .await
        .unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert!(
        err.message().contains("x_hot") && err.message().contains("\"n\""),
        "{}",
        err.message()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Review LOW: a cursor of a search without order carries the model's order, so it continues a
/// search that names that order explicitly (and the other way round).
#[tokio::test]
async fn keyset_default_order_is_the_model_order() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    mixed_rows(&mut tx).await;
    let mixed = tx.env().records("test_search.mixed").unwrap();
    let model_order = mixed.spec().order;
    let first = mixed
        .search_after(&mut tx, &Domain::TRUE, &SearchOptions::new().limit(5), None)
        .await
        .unwrap();
    let next = first.next.unwrap();
    assert_eq!(next.order(), model_order);
    let explicit = SearchOptions::new().order(model_order).limit(5);
    let second = mixed
        .search_after(&mut tx, &Domain::TRUE, &explicit, Some(&next))
        .await
        .unwrap();
    let full = mixed
        .search_with(&mut tx, &Domain::TRUE, &SearchOptions::new().limit(10))
        .await
        .unwrap()
        .ids();
    assert_eq!(second.records.ids(), full[5..10]);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Note: this query-count test verifies that batch creation with defaulted fields executes
/// exactly 1 INSERT statement. It is a baseline invariant check rather than a guard for
/// (which optimizes in-memory CPU and allocation overhead of default resolution,
/// not SQL query counts).
#[tokio::test]
async fn batch_create_defaults_query_count_is_one() {
    let h = support::search_orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_search.partner").unwrap();
    // Warm up connection
    partners.search(&mut tx, &Domain::TRUE).await.unwrap();
    let rows: Vec<_> = (0..10)
        .map(|i| support::map(&[("name", Value::str(format!("def_{i}")))]))
        .collect();
    let created = assert_query_count!(tx, >= 1, <= 1, {
        partners.create_values(&mut tx, &rows).await.unwrap()
    });
    assert_eq!(created.len(), 10);
    let actives = created.values(&mut tx, "active").await.unwrap();
    assert_eq!(actives, vec![Value::Bool(true); 10]);
    drop(tx);
    h.cleanup().await.unwrap();
}
