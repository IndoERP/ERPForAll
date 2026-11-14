//! Index kinds (design):
//! field-level `brin` and `gin`, typed model-level indexes with `INCLUDE`, the plans PostgreSQL
//! makes with them, idempotent schema sync, field indexes rebuilt when their kind changes, and the
//! extensions the kinds need.
//!
//! access-plan and heap-fetch pins are retained for milestone-end performance;
//! the typed index's constant `filter` is compiled's `erp_sql::constant_filter`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_registry::ident::index_name;
use erp_registry::{
    FieldDecl, IndexDecl, IndexMethod, ModelDecl, ModelSpec, RegistryBuilder, TableObjectDecl,
    TableObjectKind,
};
use erp_sdk::{ErrorKind, IndexKind, Orm, Transaction};
use erp_sql::ddl::typed_index_definition;
use erp_sql::filter::Unaccent;
use erp_test_models::{base_users, test_scale_index};
use erp_testkit::Harness;
use support::{one, rows};

async fn harness() -> Harness {
    Harness::builder([&base_users::ADDON, &test_scale_index::ADDON])
        .build()
        .await
        .unwrap()
}

/// `pg_indexes.indexdef` of every index of `table`, by name.
async fn index_defs(tx: &mut Transaction, table: &str) -> Vec<(String, String)> {
    rows(
        tx,
        &format!(
            "SELECT indexname::text, indexdef::text FROM pg_indexes WHERE schemaname = \
             current_schema() AND tablename = '{table}' ORDER BY 1"
        ),
    )
    .await
    .into_iter()
    .map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap()))
    .collect()
}

fn def_of<'a>(defs: &'a [(String, String)], name: &str) -> &'a str {
    defs.iter()
        .find(|(n, _)| n == name)
        .map(|(_, d)| d.as_str())
        .unwrap_or_else(|| panic!("no index {name}: {defs:?}"))
}

/// How the catalog prints the key column of a trigram index on `column`: folded like `ilike` when
/// the sync found the immutable accent-folding wrapper (the same rule as the sync's, so the
/// expectation follows the database's state instead of assuming it has no `unaccent`).
async fn trigram_key(tx: &mut Transaction, column: &str) -> String {
    let wrapper = one(
        tx,
        &format!(
            "SELECT to_regproc('{}')::text",
            erp_sql::filter::UNACCENT_WRAPPER
        ),
    )
    .await;
    if wrapper.is_some() {
        Unaccent::Indexable.wrap(column)
    } else {
        column.to_owned()
    }
}

/// The oid of the index `name`, to tell a rebuilt index from a kept one.
async fn index_oid(h: &Harness, name: &str) -> Option<String> {
    let mut tx = h.begin().await.unwrap();
    let oid = one(
        &mut tx,
        &format!("SELECT to_regclass({})::oid::text", erp_sql::literal(name)),
    )
    .await;
    drop(tx);
    oid
}

/// The plan of `sql` as one text.
async fn explain(tx: &mut Transaction, options: &str, sql: &str) -> String {
    rows(tx, &format!("EXPLAIN ({options}) {sql}"))
        .await
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The single statements of captured texts (a schema-sync phase is one batch joined with `;\n`).
fn statements(captured: &[String]) -> Vec<String> {
    captured
        .iter()
        .flat_map(|s| s.split(";\n"))
        .map(|s| s.trim().to_owned())
        .collect()
}

fn model<'a>(h: &'a Harness, name: &str) -> &'a ModelSpec {
    h.registry().model(name).unwrap()
}

/// The full name of a model's table object declared as `name`.
fn object_name(spec: &ModelSpec, name: &str) -> String {
    spec.table_objects
        .iter()
        .find(|o| o.name == name)
        .map(|o| o.full_name.clone())
        .unwrap_or_else(|| panic!("{} has no table object {name}", spec.name))
}

fn typed(spec: &ModelSpec, name: &str) -> IndexDecl {
    match spec
        .table_objects
        .iter()
        .find(|o| o.name == name)
        .map(|o| o.kind)
    {
        Some(TableObjectKind::TypedIndex(index)) => index,
        other => panic!("{name} is not a typed index of {}: {other:?}", spec.name),
    }
}

/// How PostgreSQL prints the compiled filter `[('state', '=', 'open')]` of the booking index
/// (a selection column is `varchar`).
const OPEN_PREDICATE: &str = "(state)::text = 'open'::text";

/// The catalog shows the declared methods, the `INCLUDE` columns and the filter.
async fn catalog_shows_method_include_and_filter(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let event = model(h, "scale.ix.event");
    let defs = index_defs(&mut tx, &event.table).await;
    for (field, using) in [
        ("create_date", "brin (create_date)"),
        ("seq", "brin (seq)"),
        ("payload", "gin (payload jsonb_path_ops)"),
    ] {
        let name = index_name(&event.table, field);
        assert_eq!(
            def_of(&defs, &name),
            format!(
                "CREATE INDEX {name} ON {schema}.{} USING {using}",
                event.table
            )
        );
    }
    let booking = model(h, "scale.ix.booking");
    let open = object_name(booking, "_open_by_date");
    let defs = index_defs(&mut tx, &booking.table).await;
    assert_eq!(
        def_of(&defs, &open),
        format!(
            "CREATE INDEX {open} ON {schema}.{} USING btree (state, start) INCLUDE (resource_id) \
             WHERE ({OPEN_PREDICATE})",
            booking.table
        )
    );
    // two key columns and one covering column; the canonical definition is in the comment
    let shape = rows(
        &mut tx,
        &format!(
            "SELECT ix.indnkeyatts::text, ix.indnatts::text, obj_description(ix.indexrelid, \
             'pg_class') FROM pg_index ix WHERE ix.indexrelid = '{open}'::regclass"
        ),
    )
    .await;
    let declared = typed(booking, "_open_by_date");
    let predicate = erp_sql::constant_filter::compile(declared.filter.unwrap(), booking).unwrap();
    let definition = typed_index_definition(&declared, Unaccent::Off, Some(&predicate));
    assert_eq!(
        shape,
        [[
            Some("2".to_owned()),
            Some("3".to_owned()),
            Some(format!("INDEX {definition}"))
        ]]
    );
    drop(tx);
}

#[tokio::test]
async fn spec_ormx_023_index_kinds_and_typed_include_filter_indexes() {
    let h = harness().await;
    catalog_shows_method_include_and_filter(&h).await;
    index_queries_return_expected_rows(&h).await;
    h.cleanup().await.unwrap();

    // the two wrong kinds and an `include` repeating a key field fail at registry build
    let build = |decl: ModelDecl| {
        RegistryBuilder::new([&base_users::ADDON, &test_scale_index::ADDON])
            .with_decls([&*Box::leak(Box::new(decl))])
            .build()
            .unwrap_err()
    };
    let wrong = |fields: &'static [FieldDecl]| {
        build(ModelDecl::new("test_scale_index", "scale.ix.wrong").fields(fields))
    };
    let err = wrong(Box::leak(Box::new([
        FieldDecl::char("code").index(Some(IndexKind::Brin))
    ])));
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "field 'scale.ix.wrong.code': index = \"brin\" is for date, datetime, integer and bigint fields \
         (this field is char)"
    );
    let err = wrong(Box::leak(Box::new([
        FieldDecl::date("day").index(Some(IndexKind::Gin))
    ])));
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "field 'scale.ix.wrong.day': index = \"gin\" is for json fields (this field is date)"
    );
    static REPEATED: [FieldDecl; 2] = [FieldDecl::char("state"), FieldDecl::datetime("start")];
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl::index(
        "_dup",
        IndexDecl::new(&["state", "start"]).include(&["start"]),
    )]));
    let err = build(
        ModelDecl::new("test_scale_index", "scale.ix.wrong")
            .fields(&REPEATED)
            .table_objects(objects),
    );
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "index '_dup' of 'scale.ix.wrong': include field 'start' is already a key field"
    );
}

#[tokio::test]
#[ignore = "DEFERRED_PERFORMANCE: physical plans and heap-fetch pins run at milestone end"]
async fn index_access_plan_performance() {
    let h = harness().await;
    brin_and_gin_plans(&h).await;
    include_gives_index_only_scan(&h).await;
    h.cleanup().await.unwrap();
}

/// Functional result checks use a small fixture and do not constrain PostgreSQL's access plan.
async fn index_queries_return_expected_rows(h: &Harness) {
    let event = model(h, "scale.ix.event");
    let booking = model(h, "scale.ix.booking");
    let resource = model(h, "scale.ix.resource");
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "INSERT INTO {e} (name, payload, seq, create_date, write_date) \
         SELECT 'f' || n, jsonb_build_object('tag', 't' || (n % 2)), n, \
                timestamp '2026-01-01' + n * interval '1 minute', \
                timestamp '2026-01-01' + n * interval '1 minute' FROM generate_series(1, 20) n; \
         INSERT INTO {r} (name) SELECT 'r' || n FROM generate_series(1, 10) n; \
         INSERT INTO {b} (resource_id, start, stop, state) \
         SELECT (SELECT min(id) FROM {r}) + n % 10, \
                timestamp '2026-01-01' + n * interval '10 minutes', \
                timestamp '2026-01-01' + n * interval '10 minutes' + interval '1 hour', \
                CASE WHEN n % 2 = 0 THEN 'open' ELSE 'cancel' END FROM generate_series(1, 40) n",
        e = event.table,
        r = resource.table,
        b = booking.table,
    ))
    .await
    .unwrap();
    let range = rows(
        &mut tx,
        &format!(
            "SELECT seq::text FROM {} e WHERE create_date >= timestamp '2026-01-01 00:05:00' \
         AND create_date < timestamp '2026-01-01 00:10:00' ORDER BY e.seq",
            event.table,
        ),
    )
    .await;
    assert_eq!(
        range,
        (5..10)
            .map(|n| vec![Some(n.to_string())])
            .collect::<Vec<_>>()
    );
    let contained = rows(
        &mut tx,
        &format!(
            "SELECT seq::text FROM {} e WHERE payload @> '{{\"tag\":\"t0\"}}'::jsonb ORDER BY e.seq",
            event.table,
        ),
    )
    .await;
    assert_eq!(
        contained,
        (2..=20)
            .step_by(2)
            .map(|n| vec![Some(n.to_string())])
            .collect::<Vec<_>>()
    );
    let resource_ids = rows(
        &mut tx,
        &format!(
            "SELECT id::text FROM {} WHERE name IN ('r3', 'r5') ORDER BY name",
            resource.table,
        ),
    )
    .await;
    assert_eq!(resource_ids.len(), 2);
    let covered = rows(&mut tx, &format!(
        "SELECT state, resource_id::text FROM {} WHERE state = 'open' \
         AND start >= timestamp '2026-01-01 00:20:00' AND start < timestamp '2026-01-01 01:00:00' ORDER BY start", booking.table,
    )).await;
    assert_eq!(
        covered,
        vec![
            vec![Some("open".to_owned()), resource_ids[0][0].clone()],
            vec![Some("open".to_owned()), resource_ids[1][0].clone()],
        ]
    );
    drop(tx);
}

/// A second schema sync of the same registry issues no DDL: every index (field-level of every
/// kind, typed, relation tables) is recognised from the catalog.
#[tokio::test]
async fn schema_sync_with_index_kinds_is_idempotent() {
    let h = harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let ddl: Vec<String> = statements(tx.captured())
        .into_iter()
        .filter(|sql| {
            let head = sql.to_ascii_uppercase();
            ["CREATE", "ALTER", "DROP", "COMMENT"]
                .iter()
                .any(|verb| head.starts_with(verb))
        })
        .collect();
    assert!(ddl.is_empty(), "second sync issued DDL: {ddl:#?}");
    // and the sync did read the catalog (the check above is not vacuous)
    assert!(
        tx.captured()
            .iter()
            .any(|sql| sql == erp_sql::ddl::CATALOG_QUERY),
        "{:#?}",
        tx.captured()
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// 200,000 events, one a minute from 2026-01-01, `create_date` in insert order, 1,000 payload
/// tags.
async fn fill_events(tx: &mut Transaction, table: &str) {
    tx.batch(&format!(
        "INSERT INTO {table} (name, payload, seq, create_date, write_date) \
         SELECT 'e' || n, jsonb_build_object('tag', 't' || (n % 1000), 'n', n), n, \
                timestamp '2026-01-01' + n * interval '1 minute', \
                timestamp '2026-01-01' + n * interval '1 minute' \
           FROM generate_series(1, 200000) AS n; \
         ANALYZE {table}"
    ))
    .await
    .unwrap();
}

/// BRIN serves a time range on a column that follows the insert order, GIN serves `@>`.
async fn brin_and_gin_plans(h: &Harness) {
    let mut tx = h.begin().await.unwrap();
    let event = model(h, "scale.ix.event");
    fill_events(&mut tx, &event.table).await;
    let brin = index_name(&event.table, "create_date");
    let plan = explain(
        &mut tx,
        "COSTS OFF",
        &format!(
            "SELECT id FROM {} WHERE create_date >= '2026-02-01' AND create_date < '2026-02-02'",
            event.table
        ),
    )
    .await;
    assert!(
        plan.contains(&format!("Bitmap Index Scan on {brin}")),
        "BRIN not used for the range:\n{plan}"
    );
    // the same range on the integer `seq` (BRIN as well)
    let plan = explain(
        &mut tx,
        "COSTS OFF",
        &format!(
            "SELECT id FROM {} WHERE seq BETWEEN 1000 AND 2000",
            event.table
        ),
    )
    .await;
    let seq = index_name(&event.table, "seq");
    assert!(
        plan.contains(&format!("Bitmap Index Scan on {seq}")),
        "BRIN not used for the integer range:\n{plan}"
    );
    let gin = index_name(&event.table, "payload");
    let plan = explain(
        &mut tx,
        "COSTS OFF",
        &format!(
            "SELECT id FROM {} WHERE payload @> '{{\"tag\": \"t42\"}}'::jsonb",
            event.table
        ),
    )
    .await;
    assert!(
        plan.contains(&format!("Bitmap Index Scan on {gin}")),
        "GIN not used for @>:\n{plan}"
    );
    // and the index answers right: 200 events carry the tag
    let count = one(
        &mut tx,
        &format!(
            "SELECT count(*)::text FROM {} WHERE payload @> '{{\"tag\": \"t42\"}}'::jsonb",
            event.table
        ),
    )
    .await;
    assert_eq!(count.as_deref(), Some("200"));
    drop(tx);
}

/// The covering query is an index-only scan with `Heap Fetches: 0` after `VACUUM`, through the
/// filtered typed `include` index.
async fn include_gives_index_only_scan(h: &Harness) {
    let booking = model(h, "scale.ix.booking");
    let resource = model(h, "scale.ix.resource");
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "INSERT INTO {r} (name) SELECT 'r' || n FROM generate_series(1, 10) AS n; \
         INSERT INTO {b} (resource_id, start, stop, state) \
         SELECT (SELECT min(id) FROM {r}) + n % 10, \
                timestamp '2026-01-01' + n * interval '10 minutes', \
                timestamp '2026-01-01' + n * interval '10 minutes' + interval '1 hour', \
                CASE WHEN n % 2 = 0 THEN 'open' ELSE 'cancel' END \
           FROM generate_series(1, 100000) AS n",
        r = resource.table,
        b = booking.table
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // VACUUM cannot run in a transaction block: an autocommit connection of the test database.
    // Other tests share the cluster: a long transaction elsewhere can keep the visibility map from
    // covering every page, so VACUUM and the plan are retried a bounded number of times.
    let open = object_name(booking, "_open_by_date");
    let mut plan = String::new();
    for _ in 0..5 {
        let client = h.db().client().await.unwrap();
        client
            .batch_execute(&format!("VACUUM (ANALYZE) {}", booking.table))
            .await
            .unwrap();
        drop(client);
        let mut tx = h.begin().await.unwrap();
        plan = explain(
            &mut tx,
            "ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF",
            &format!(
                "SELECT state, start, resource_id FROM {} WHERE state = 'open' AND start >= \
                 '2026-01-10' AND start < '2026-01-11'",
                booking.table
            ),
        )
        .await;
        drop(tx);
        if plan.contains("Heap Fetches: 0") {
            break;
        }
    }
    assert!(
        plan.contains(&format!(
            "Index Only Scan using {open} on {}",
            booking.table
        )),
        "no index-only scan:\n{plan}"
    );
    assert!(plan.contains("Heap Fetches: 0"), "heap fetched:\n{plan}");
    drop(tx);
}

/// An addon adds typed indexes to a model it does not own and replaces one by name (design §2,
/// §9); installing it on a database synced without it rebuilds the replaced index and creates the
/// new one, and only those.
#[tokio::test]
async fn typed_indexes_from_an_extension_replace_by_name() {
    let h = harness().await;
    let booking = model(&h, "scale.ix.booking");
    let open = object_name(booking, "_open_by_date");
    // the same database with the extension addon installed
    let registry = Arc::new(
        RegistryBuilder::new([
            &base_users::ADDON,
            &test_scale_index::ADDON,
            &test_scale_index::ext::ADDON,
        ])
        .build()
        .unwrap(),
    );
    let extended = registry.model("scale.ix.booking").unwrap();
    assert_eq!(object_name(extended, "_open_by_date"), open);
    let by_stop = object_name(extended, "_by_stop");
    let orm = Orm::builder(Arc::clone(&registry), h.db().pool().clone())
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let ddl: Vec<String> = statements(tx.captured())
        .into_iter()
        .filter(|s| {
            ["CREATE INDEX", "DROP INDEX", "COMMENT ON INDEX"]
                .iter()
                .any(|verb| s.starts_with(verb))
        })
        .collect();
    let new_open = typed_index_definition(&typed(extended, "_open_by_date"), Unaccent::Off, None);
    let stop = typed_index_definition(&typed(extended, "_by_stop"), Unaccent::Off, None);
    assert_eq!(
        ddl,
        [
            format!("DROP INDEX IF EXISTS \"{open}\""),
            format!(
                "CREATE INDEX \"{open}\" ON \"{}\" {new_open}",
                extended.table
            ),
            format!("COMMENT ON INDEX \"{open}\" IS 'INDEX {new_open}'"),
            format!(
                "CREATE INDEX \"{by_stop}\" ON \"{}\" {stop}",
                extended.table
            ),
            format!("COMMENT ON INDEX \"{by_stop}\" IS 'INDEX {stop}'"),
        ]
    );
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let defs = index_defs(&mut tx, &extended.table).await;
    assert_eq!(
        def_of(&defs, &open),
        format!(
            "CREATE INDEX {open} ON {schema}.{} USING btree (state, start, stop) INCLUDE \
             (resource_id)",
            extended.table
        )
    );
    assert_eq!(
        def_of(&defs, &by_stop),
        format!(
            "CREATE INDEX {by_stop} ON {schema}.{} USING brin (stop)",
            extended.table
        )
    );
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A runtime model `scale.ix.kind` whose date field `d` has `index`.
fn kind_model(index: Option<IndexKind>) -> &'static ModelDecl {
    static FIELDS_BTREE: [FieldDecl; 1] = [FieldDecl::date("d").index(Some(IndexKind::Btree))];
    static FIELDS_BRIN: [FieldDecl; 1] = [FieldDecl::date("d").index(Some(IndexKind::Brin))];
    static FIELDS_NN: [FieldDecl; 1] = [FieldDecl::date("d").index(Some(IndexKind::BtreeNotNull))];
    let fields: &'static [FieldDecl] = match index {
        Some(IndexKind::Brin) => &FIELDS_BRIN,
        Some(IndexKind::BtreeNotNull) => &FIELDS_NN,
        _ => &FIELDS_BTREE,
    };
    Box::leak(Box::new(
        ModelDecl::new("test_scale_index", "scale.ix.kind").fields(fields),
    ))
}

async fn sync_with(
    h: &Harness,
    decl: &'static ModelDecl,
) -> (Vec<String>, Arc<erp_registry::Registry>) {
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_scale_index::ADDON])
            .with_decls([decl])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(Arc::clone(&registry), h.db().pool().clone())
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    let ddl = statements(tx.captured())
        .into_iter()
        .filter(|s| s.contains("scale_ix_kind__d_index"))
        .collect();
    tx.commit().await.unwrap();
    (ddl, registry)
}

#[tokio::test]
async fn field_index_rebuilt_when_its_kind_changes() {
    let h = harness().await;
    let name = "scale_ix_kind__d_index";
    let (ddl, _) = sync_with(&h, kind_model(Some(IndexKind::Btree))).await;
    assert_eq!(
        ddl,
        [format!(
            "CREATE INDEX \"{name}\" ON \"scale_ix_kind\" USING btree (\"d\")"
        )]
    );
    let (ddl, _) = sync_with(&h, kind_model(Some(IndexKind::Brin))).await;
    assert_eq!(
        ddl,
        [
            format!("DROP INDEX IF EXISTS \"{name}\""),
            format!("CREATE INDEX \"{name}\" ON \"scale_ix_kind\" USING brin (\"d\")"),
        ]
    );
    // unchanged: nothing
    let (ddl, _) = sync_with(&h, kind_model(Some(IndexKind::Brin))).await;
    assert!(ddl.is_empty(), "{ddl:?}");
    let (ddl, _) = sync_with(&h, kind_model(Some(IndexKind::BtreeNotNull))).await;
    assert_eq!(
        ddl,
        [
            format!("DROP INDEX IF EXISTS \"{name}\""),
            format!(
                "CREATE INDEX \"{name}\" ON \"scale_ix_kind\" USING btree (\"d\") WHERE \"d\" IS \
                 NOT NULL"
            ),
        ]
    );
    let (ddl, _) = sync_with(&h, kind_model(Some(IndexKind::BtreeNotNull))).await;
    assert!(ddl.is_empty(), "{ddl:?}");
    // the catalog agrees
    let mut tx = h.begin().await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let defs = index_defs(&mut tx, "scale_ix_kind").await;
    assert_eq!(
        def_of(&defs, name),
        format!(
            "CREATE INDEX {name} ON {schema}.scale_ix_kind USING btree (d) WHERE (d IS NOT NULL)"
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_ormx_024_trigram_without_pg_trgm_fails_the_install() {
    let h = Harness::builder([&base_users::ADDON, &test_scale_index::ADDON])
        .without_schema()
        .build()
        .await
        .unwrap();
    let item = model(&h, "scale.item");
    let field = item.field("name").unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("DROP EXTENSION IF EXISTS pg_trgm CASCADE")
        .await
        .unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(
        err.message(),
        format!(
            "field '{}.{}' declares index = \"trigram\", which needs the PostgreSQL extension \
             pg_trgm; it is not installed",
            item.name, field.name
        )
    );
    // nothing was created: the check runs before any DDL
    let tables: Vec<String> = h
        .registry()
        .models()
        .iter()
        .filter(|m| m.has_table())
        .map(|m| format!("'{}'", m.table))
        .collect();
    let created = one(
        &mut tx,
        &format!(
            "SELECT count(*)::text FROM pg_class WHERE relnamespace = current_schema()::regnamespace \
             AND relname IN ({})",
            tables.join(", ")
        ),
    )
    .await;
    assert_eq!(created.as_deref(), Some("0"));
    // with the extension the same install creates the trigram index
    tx.batch("CREATE EXTENSION pg_trgm").await.unwrap();
    tx.init_models(&[]).await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let name = index_name(&item.table, field.name);
    let defs = index_defs(&mut tx, &item.table).await;
    assert_eq!(
        def_of(&defs, &name),
        format!(
            "CREATE INDEX {name} ON {schema}.{} USING gin ({} gin_trgm_ops)",
            item.table,
            trigram_key(&mut tx, field.name).await
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A model `scale.ix.ext` with a char field and a typed index of `method`.
fn ext_model(method: IndexMethod) -> ModelDecl {
    static FIELDS: [FieldDecl; 2] = [FieldDecl::char("code"), FieldDecl::integer("qty")];
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl::index(
        "_by_code",
        IndexDecl::new(&["code"]).method(method),
    )]));
    ModelDecl::new("test_scale_index", "scale.ix.ext")
        .fields(&FIELDS)
        .table_objects(objects)
}

/// A typed `trigram` index needs `pg_trgm`: without it the install fails before any DDL, naming
/// the extension and the index (the one extension gate); with it the index is created.
#[tokio::test]
async fn typed_trigram_index_needs_pg_trgm() {
    let h = Harness::builder([&base_users::ADDON, &test_scale_index::ADDON])
        .with_decls([ext_model(IndexMethod::Trigram)])
        .without_schema()
        .build()
        .await
        .unwrap();
    let spec = model(&h, "scale.ix.ext");
    let mut tx = h.begin().await.unwrap();
    tx.batch("DROP EXTENSION IF EXISTS pg_trgm CASCADE")
        .await
        .unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Database);
    assert_eq!(
        err.message(),
        format!(
            "index '_by_code' of '{}' declares method = \"trigram\", which needs the PostgreSQL \
             extension pg_trgm; it is not installed",
            spec.name
        )
    );
    let created = one(
        &mut tx,
        &format!("SELECT to_regclass('{}')::text", spec.table),
    )
    .await;
    assert_eq!(created, None, "the table was created");
    tx.batch("CREATE EXTENSION pg_trgm").await.unwrap();
    tx.init_models(&[]).await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let name = object_name(spec, "_by_code");
    let defs = index_defs(&mut tx, &spec.table).await;
    assert_eq!(
        def_of(&defs, &name),
        format!(
            "CREATE INDEX {name} ON {schema}.{} USING gin ({} gin_trgm_ops)",
            spec.table,
            trigram_key(&mut tx, "code").await
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A typed `gist` index needs `btree_gist`, which schema sync installs in the install transaction
/// like it does for exclusions (the extension gate counts what the sync installs): the install
/// succeeds on a database without it and creates the index.
#[tokio::test]
async fn typed_gist_index_installs_btree_gist() {
    let h = Harness::builder([&base_users::ADDON, &test_scale_index::ADDON])
        .with_decls([ext_model(IndexMethod::Gist)])
        .without_schema()
        .build()
        .await
        .unwrap();
    let spec = model(&h, "scale.ix.ext");
    let mut tx = h.begin().await.unwrap();
    tx.batch("DROP EXTENSION IF EXISTS btree_gist CASCADE")
        .await
        .unwrap();
    tx.init_models(&[]).await.unwrap();
    let installed = one(
        &mut tx,
        "SELECT extname::text FROM pg_extension WHERE extname = 'btree_gist'",
    )
    .await;
    assert_eq!(installed.as_deref(), Some("btree_gist"));
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let name = object_name(spec, "_by_code");
    let defs = index_defs(&mut tx, &spec.table).await;
    assert_eq!(
        def_of(&defs, &name),
        format!(
            "CREATE INDEX {name} ON {schema}.{} USING gist (code)",
            spec.table
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// it does not own is created by schema sync with its canonical definition in the comment)
#[tokio::test]
async fn spec_ormx_021_data_only_addon_declares_a_brin_index() {
    let addon = erp_registry::intern_addon("test_scale_index_toml", &["test_scale_index"]).unwrap();
    let module = erp_modules::ModuleName::new("test_scale_index_toml").unwrap();
    let defs = erp_modules::parse_model_file(
        &module,
        "models/booking.toml",
        "[[model]]\nextend = \"scale.ix.booking\"\n\n[[model.index]]\nname = \"_by_date\"\n\
         fields = [\"start\"]\nmethod = \"brin\"\n",
    )
    .unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let h = Harness::builder([&base_users::ADDON, &test_scale_index::ADDON, addon])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let booking = model(&h, "scale.ix.booking");
    let name = object_name(booking, "_by_date");
    let definition = typed_index_definition(&typed(booking, "_by_date"), Unaccent::Off, None);
    let mut tx = h.begin().await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let defs = index_defs(&mut tx, &booking.table).await;
    assert_eq!(
        def_of(&defs, &name),
        format!(
            "CREATE INDEX {name} ON {schema}.{} USING brin (start)",
            booking.table
        )
    );
    let comment = one(
        &mut tx,
        &format!("SELECT obj_description('{name}'::regclass, 'pg_class')"),
    )
    .await;
    assert_eq!(comment, Some(format!("INDEX {definition}")));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A runtime model `scale.ix.big` whose date field `d` has the field index `kind` and whose typed
/// index `_by_qty` on `qty` has `method`.
fn big_model(kind: IndexKind, method: IndexMethod) -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(Box::new([
        FieldDecl::date("d").index(Some(kind)),
        FieldDecl::integer("qty"),
    ]));
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl::index(
        "_by_qty",
        IndexDecl::new(&["qty"]).method(method),
    )]));
    Box::leak(Box::new(
        ModelDecl::new("test_scale_index", "scale.ix.big")
            .fields(fields)
            .table_objects(objects),
    ))
}

fn big_orm(h: &Harness, decl: &'static ModelDecl) -> Orm {
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_scale_index::ADDON])
            .with_decls([decl])
            .build()
            .unwrap(),
    );
    Orm::builder(registry, h.db().pool().clone())
        .build()
        .unwrap()
}

/// The ORM's statements that touch `scale_ix_big` and change something.
fn big_ddl(tx: &Transaction) -> Vec<String> {
    statements(tx.captured())
        .into_iter()
        .filter(|s| s.contains("\"scale_ix_big"))
        .filter(|s| !s.starts_with("SELECT"))
        .collect()
}

/// Changing the kind of a field index or the definition of a typed index on a table above
/// `large_table_rows` does not build the new index in the schema transaction: the old index is
/// dropped there (a catalog change, no scan) and the new one is a `CREATE INDEX CONCURRENTLY`
/// step after the commit, reported in the sync; below the threshold the replacement stays
/// inline (review).
#[tokio::test]
async fn changed_index_on_a_large_table_is_a_concurrent_step() {
    use erp_orm::{SchemaStepKind, SchemaSyncOptions};
    let h = harness().await;
    let rows = 12_i64;
    let field_index = "scale_ix_big__d_index".to_owned();
    // the model, first with btree on both: created inline (the table is new)
    let orm = big_orm(&h, big_model(IndexKind::Btree, IndexMethod::Btree));
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.execute(
        "INSERT INTO scale_ix_big (d, qty) SELECT date '2026-01-01' + g::int, g::int \
           FROM generate_series(1, $1::int8) g",
        &[&rows],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx); // commit keeps the connection until the value is dropped; the pool has 4
    drop(orm);

    // both change to brin; the table has more rows than the threshold
    let orm = big_orm(&h, big_model(IndexKind::Brin, IndexMethod::Brin));
    let typed_name = {
        let spec = orm.registry().model("scale.ix.big").unwrap();
        object_name(spec, "_by_qty")
    };
    let typed_definition = typed_index_definition(
        &typed(orm.registry().model("scale.ix.big").unwrap(), "_by_qty"),
        Unaccent::Off,
        None,
    );
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    let inside = big_ddl(&tx);
    assert!(
        !inside.iter().any(|s| s.contains("CREATE")),
        "an index was built inside the transaction: {inside:#?}"
    );
    // nothing is dropped either: a plain DROP INDEX would hold ACCESS EXCLUSIVE until the commit
    assert_eq!(inside, Vec::<String>::new());
    let mut steps: Vec<_> = sync
        .steps()
        .iter()
        .map(|s| (s.kind(), s.key().to_owned(), s.table().to_owned()))
        .collect();
    steps.sort_by(|a, b| a.1.cmp(&b.1));
    let mut expected = vec![
        (
            SchemaStepKind::CreateIndex,
            format!("index.{field_index}"),
            "scale_ix_big".to_owned(),
        ),
        (
            SchemaStepKind::CreateIndex,
            format!("index.{typed_name}"),
            "scale_ix_big".to_owned(),
        ),
    ];
    expected.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(steps, expected);
    tx.commit().await.unwrap();
    drop(tx); // commit keeps the connection until the value is dropped; the pool has 4

    // after the commit the old indexes are still there: each step builds under a temporary
    // name (__swap) and swaps in one short transaction; if the build fails, the old index was
    // never dropped
    let mut tx = h.begin().await.unwrap();
    let defs = index_defs(&mut tx, "scale_ix_big").await;
    assert!(
        def_of(&defs, &field_index).ends_with("USING btree (d)"),
        "{defs:?}"
    );
    assert!(
        def_of(&defs, &typed_name).ends_with("USING btree (qty)"),
        "{defs:?}"
    );
    // a build that fails leaves the old index intact: the column is renamed away under the step
    tx.batch("ALTER TABLE scale_ix_big RENAME COLUMN qty TO qty_away")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx); // commit keeps the connection until the value is dropped; the pool has 4
    let typed_step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{typed_name}"))
        .unwrap();
    let err = orm.run_schema_step(typed_step, &options).await.unwrap_err();
    assert_eq!(
        err.message(),
        format!("schema step 'index.{typed_name}' failed")
    );
    let cause = std::error::Error::source(&err)
        .and_then(|s| s.downcast_ref::<erp_fields::Error>())
        .unwrap();
    assert_eq!(cause.kind(), erp_fields::ErrorKind::Database);
    assert_eq!(cause.sqlstate(), Some("42703")); // column "qty" does not exist
    assert_eq!(
        cause.message(),
        format!(
            "column \"qty\" does not exist: failed to build swap index {typed_name}__swap; live index {typed_name} is unchanged and still in use"
        )
    );
    // The old index is still intact and valid because it is never dropped before build.
    let mut tx = h.begin().await.unwrap();
    let defs = index_defs(&mut tx, "scale_ix_big").await;
    assert!(
        def_of(&defs, &typed_name).ends_with("USING btree (qty_away)"),
        "{defs:?}"
    );
    tx.batch("ALTER TABLE scale_ix_big RENAME COLUMN qty_away TO qty")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx); // commit keeps the connection until the value is dropped; the pool has 4

    for step in sync.steps() {
        let report = orm.run_schema_step(step, &options).await.unwrap();
        assert_eq!(
            report,
            erp_orm::SchemaStepReport::default(),
            "{}",
            step.key()
        );
    }
    let mut tx = h.begin().await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let defs = index_defs(&mut tx, "scale_ix_big").await;
    assert_eq!(
        def_of(&defs, &field_index),
        format!("CREATE INDEX {field_index} ON {schema}.scale_ix_big USING brin (d)")
    );
    assert_eq!(
        def_of(&defs, &typed_name),
        format!("CREATE INDEX {typed_name} ON {schema}.scale_ix_big USING brin (qty)")
    );
    let valid = one(
        &mut tx,
        "SELECT count(*)::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
          WHERE c.relname LIKE 'scale_ix_big%' AND NOT i.indisvalid",
    )
    .await;
    assert_eq!(valid.as_deref(), Some("0"));
    let comment = one(
        &mut tx,
        &format!("SELECT obj_description('{schema}.{typed_name}'::regclass, 'pg_class')"),
    )
    .await;
    assert_eq!(comment, Some(format!("INDEX {typed_definition}")));
    drop(tx);

    // a replacing step that already ran does not drop and build again: the index keeps its oid
    let before = index_oid(&h, &typed_name).await;
    let typed_step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{typed_name}"))
        .unwrap();
    let report = orm.run_schema_step(typed_step, &options).await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());
    assert_eq!(index_oid(&h, &typed_name).await, before);

    // converged: a second sync plans nothing and issues no DDL
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let again = tx.init_models_with(&[], &options).await.unwrap();
    assert!(again.steps().is_empty());
    assert_eq!(big_ddl(&tx), Vec::<String>::new());
    tx.commit().await.unwrap();
    drop(tx); // commit keeps the connection until the value is dropped; the pool has 4
    drop(orm);

    // back to btree with a threshold above the row count: replaced inline, no steps
    let orm = big_orm(&h, big_model(IndexKind::Btree, IndexMethod::Btree));
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let small = SchemaSyncOptions::new().large_table_rows(12);
    let sync = tx.init_models_with(&[], &small).await.unwrap();
    assert!(sync.steps().is_empty());
    let inside = big_ddl(&tx);
    assert_eq!(
        inside
            .iter()
            .filter(|s| s.starts_with("CREATE INDEX"))
            .count(),
        2,
        "{inside:#?}"
    );
    tx.commit().await.unwrap();
    drop(tx); // commit keeps the connection until the value is dropped; the pool has 4
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A runtime model `scale.ix.filt` with a typed index `_open` on `qty` filtered by `filter`.
fn filtered_model(filter: &'static str) -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(Box::new([
        FieldDecl::char("state"),
        FieldDecl::integer("qty"),
    ]));
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl::index(
        "_open",
        IndexDecl::new(&["qty"]).filter(filter),
    )]));
    Box::leak(Box::new(
        ModelDecl::new("test_scale_index", "scale.ix.filt")
            .fields(fields)
            .table_objects(objects),
    ))
}

/// The constant filter of a typed index is compiled when the ORM is built: a filter over an
/// unknown field fails there, naming the index and the model; changing a valid one replaces the
/// index, and an unchanged one issues no DDL.
#[tokio::test]
async fn typed_index_filter_is_compiled_and_part_of_the_definition() {
    let h = harness().await;
    let registry = |filter| {
        Arc::new(
            RegistryBuilder::new([&base_users::ADDON, &test_scale_index::ADDON])
                .with_decls([filtered_model(filter)])
                .build()
                .unwrap(),
        )
    };
    let Err(bad) = Orm::builder(registry("[('nope', '=', 'x')]"), h.db().pool().clone()).build()
    else {
        panic!("a filter over an unknown field was accepted");
    };
    assert_eq!(bad.kind(), ErrorKind::Programming);
    assert_eq!(
        bad.message(),
        "index '_open' on 'scale.ix.filt': filter [('nope', '=', 'x')]: field 'nope' does not exist on 'scale.ix.filt'"
    );

    // the compiled text is what the stored definitions compare: pinned, so a change of the
    // compiler's output shows up as a failing test, not as every filtered index being replaced
    let spec = registry("[('state', '=', 'open')]");
    assert_eq!(
        erp_sql::constant_filter::compile(
            "[('state', '=', 'open')]",
            spec.model("scale.ix.filt").unwrap()
        )
        .unwrap(),
        "\"state\" = 'open'"
    );

    let mut previous = None;
    for (filter, expected) in [
        (
            "[('state', '=', 'open')]",
            "WHERE ((state)::text = 'open'::text)",
        ),
        (
            "[('state', '!=', 'open')]",
            "WHERE (((state)::text <> 'open'::text) OR (state IS NULL))",
        ),
    ] {
        let orm = Orm::builder(registry(filter), h.db().pool().clone())
            .build()
            .unwrap();
        let name = object_name(orm.registry().model("scale.ix.filt").unwrap(), "_open");
        let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        tx.capture_statements(true);
        tx.init_models(&[]).await.unwrap();
        let ddl: Vec<String> = statements(tx.captured())
            .into_iter()
            .filter(|s| s.contains(&name))
            .collect();
        // the first filter creates the index, the second drops and creates it again
        assert_eq!(
            ddl.iter().filter(|s| s.starts_with("DROP INDEX")).count(),
            usize::from(previous.is_some()),
            "{ddl:#?}"
        );
        assert_eq!(
            ddl.iter().filter(|s| s.starts_with("CREATE INDEX")).count(),
            1,
            "{ddl:#?}"
        );
        let defs = index_defs(&mut tx, "scale_ix_filt").await;
        assert!(def_of(&defs, &name).ends_with(expected), "{defs:#?}");
        // unchanged: a second sync issues nothing for the index
        tx.capture_statements(true);
        tx.init_models(&[]).await.unwrap();
        let again: Vec<String> = statements(tx.captured())
            .into_iter()
            .filter(|s| s.contains(&name))
            .collect();
        assert!(again.is_empty(), "{again:#?}");
        tx.commit().await.unwrap();
        previous = Some(filter);
    }
    h.cleanup().await.unwrap();
}

/// A runtime model `scale.ix.left` with a typed btree index `_by_qty`.
fn left_model() -> &'static ModelDecl {
    let fields: &'static [FieldDecl] = Box::leak(Box::new([FieldDecl::integer("qty")]));
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl::index(
        "_by_qty",
        IndexDecl::new(&["qty"]),
    )]));
    Box::leak(Box::new(
        ModelDecl::new("test_scale_index", "scale.ix.left")
            .fields(fields)
            .table_objects(objects),
    ))
}

/// What an earlier sync can leave behind for a typed index, and what the next sync does: an index
/// of the declared definition without its comment gets the comment only; an INVALID leftover (an
/// interrupted `CREATE INDEX CONCURRENTLY`) and a foreign valid index of another method are
/// rebuilt by the step.
#[tokio::test]
async fn typed_index_leftovers_are_repaired() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_scale_index::ADDON])
            .with_decls([left_model()])
            .build()
            .unwrap(),
    );
    let spec = registry.model("scale.ix.left").unwrap();
    let name = object_name(spec, "_by_qty");
    let comment = format!(
        "INDEX {}",
        typed_index_definition(&typed(spec, "_by_qty"), Unaccent::Off, None)
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .build()
        .unwrap();
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.execute(
        "INSERT INTO scale_ix_left (qty) SELECT 1 FROM generate_series(1, 12)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // 1. valid, declared definition, no comment: only the comment is written
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.batch(&format!("COMMENT ON INDEX \"{name}\" IS NULL"))
        .await
        .unwrap();
    tx.capture_statements(true);
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    assert!(sync.steps().is_empty());
    let ddl: Vec<String> = statements(tx.captured())
        .into_iter()
        .filter(|s| s.contains(&name))
        .collect();
    assert_eq!(ddl, [format!("COMMENT ON INDEX \"{name}\" IS '{comment}'")]);
    tx.commit().await.unwrap();
    drop(tx);

    // 2. an INVALID leftover: a unique build over duplicate values fails and leaves it
    let client = h.db().client().await.unwrap();
    client
        .batch_execute(&format!("DROP INDEX \"{name}\""))
        .await
        .unwrap();
    client
        .batch_execute(&format!(
            "CREATE UNIQUE INDEX CONCURRENTLY \"{name}\" ON scale_ix_left (qty)"
        ))
        .await
        .unwrap_err();
    drop(client);
    for (case, leftover) in [("invalid", None), ("foreign", Some("hash"))] {
        if let Some(method) = leftover {
            let client = h.db().client().await.unwrap();
            client
                .batch_execute(&format!(
                    "DROP INDEX \"{name}\"; CREATE INDEX \"{name}\" ON scale_ix_left USING {method} (qty)"
                ))
                .await
                .unwrap();
        }
        let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        let sync = tx.init_models_with(&[], &options).await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let keys: Vec<&str> = sync.steps().iter().map(|s| s.key()).collect();
        assert_eq!(keys, [format!("index.{name}")], "{case}");
        for step in sync.steps() {
            let report = orm.run_schema_step(step, &options).await.unwrap();
            assert_eq!(report, erp_orm::SchemaStepReport::default(), "{case}");
        }
        let mut tx = h.begin().await.unwrap();
        let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
        let defs = index_defs(&mut tx, "scale_ix_left").await;
        assert_eq!(
            def_of(&defs, &name),
            format!("CREATE INDEX {name} ON {schema}.scale_ix_left USING btree (qty)"),
            "{case}"
        );
        let found = one(
            &mut tx,
            &format!(
                "SELECT obj_description('{schema}.{name}'::regclass, 'pg_class') || ' ' || \
                 (SELECT indisvalid FROM pg_index WHERE indexrelid = '{schema}.{name}'::regclass)"
            ),
        )
        .await;
        assert_eq!(found, Some(format!("{comment} true")), "{case}");
        drop(tx);
    }
    drop(orm);
    h.cleanup().await.unwrap();
}

/// An interrupted build of a model-file index leaves a valid index without its comment. Whatever
/// the index's shape (a filter, a trigram operator class), the next sync recognises it by structure
/// and writes the comment only; it does not refuse, drop or rebuild it, on a large table either.
#[tokio::test]
async fn interrupted_model_file_indexes_are_repaired_by_comment() {
    use erp_orm::SchemaSyncOptions;
    let addon = erp_registry::intern_addon("test_scale_index_toml2", &[]).unwrap();
    let module = erp_modules::ModuleName::new("test_scale_index_toml2").unwrap();
    let defs = erp_modules::parse_model_file(
        &module,
        "models/rec.toml",
        "[[model]]\nname = \"scale.ix.rec\"\n\n[[model.field]]\nname = \"code\"\ntype = \"char\"\n\n\
         [[model.field]]\nname = \"qty\"\ntype = \"integer\"\n\n[[model.field]]\nname = \"state\"\n\
         type = \"char\"\n\n[[model.index]]\nname = \"_open\"\nfields = [\"qty\"]\n\
         filter = \"[('state', '=', 'open')]\"\n\n[[model.index]]\nname = \"_code_trgm\"\n\
         fields = [\"code\"]\nmethod = \"trigram\"\n",
    )
    .unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let h = Harness::builder([&base_users::ADDON, addon])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let rec = model(&h, "scale.ix.rec");
    let names = [object_name(rec, "_open"), object_name(rec, "_code_trgm")];
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO scale_ix_rec (code, qty, state) SELECT 'c' || g, g, 'open' FROM \
         generate_series(1, 12) g",
        &[],
    )
    .await
    .unwrap();
    let before = rows(
        &mut tx,
        "SELECT c.relname::text, obj_description(c.oid, 'pg_class') FROM pg_class c WHERE \
         c.relname IN ('scale_ix_rec_open', 'scale_ix_rec_code_trgm') ORDER BY 1",
    )
    .await;
    assert_eq!(before.len(), 2, "{before:?}");
    for name in &names {
        tx.batch(&format!("COMMENT ON INDEX \"{name}\" IS NULL"))
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    tx.capture_statements(true);
    let sync = tx
        .init_models_with(&[], &SchemaSyncOptions::new().large_table_rows(10))
        .await
        .unwrap();
    assert!(sync.steps().is_empty());
    let mut ddl: Vec<String> = statements(tx.captured())
        .into_iter()
        .filter(|s| names.iter().any(|n| s.contains(n.as_str())))
        .collect();
    ddl.sort();
    assert_eq!(ddl.len(), 2, "{ddl:#?}");
    assert!(
        ddl.iter().all(|s| s.starts_with("COMMENT ON INDEX")),
        "{ddl:#?}"
    );
    tx.commit().await.unwrap();
    drop(tx);
    let mut tx = h.begin().await.unwrap();
    let after = rows(
        &mut tx,
        "SELECT c.relname::text, obj_description(c.oid, 'pg_class') FROM pg_class c WHERE \
         c.relname IN ('scale_ix_rec_open', 'scale_ix_rec_code_trgm') ORDER BY 1",
    )
    .await;
    assert_eq!(after, before);
    drop(tx);
    h.cleanup().await.unwrap();
}

/// A unique index under a field index's generated name is refused, never rebuilt non-unique.
#[tokio::test]
async fn a_unique_index_under_a_field_index_name_is_refused() {
    let h = harness().await;
    let name = "scale_ix_kind__d_index";
    sync_with(&h, kind_model(Some(IndexKind::Btree))).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "DROP INDEX \"{name}\"; CREATE UNIQUE INDEX \"{name}\" ON scale_ix_kind (d)"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &test_scale_index::ADDON])
            .with_decls([kind_model(Some(IndexKind::Brin))])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .build()
        .unwrap();
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "index {name} on table scale_ix_kind is unique, but that is the generated name of the \
             index of field scale.ix.kind.d, which is never unique; it is not rebuilt (a \
             uniqueness rule would be lost). Rename the unique index or drop it"
        )
    );
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// When an interrupted build left an invalid swap index, or a valid swap index before the swap
/// transaction, the step heals itself: invalid temporary indexes are dropped and rebuilt,
/// valid ones are swapped without re-building, and an already completed swap is idempotent.
#[tokio::test]
async fn interrupted_index_swap_step_repair() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;
    let rows = 12_i64;
    let field_index = "scale_ix_big__d_index".to_owned();

    // 1. Initial table with btree index and 12 rows
    let orm = big_orm(&h, big_model(IndexKind::Btree, IndexMethod::Btree));
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.execute(
        "INSERT INTO scale_ix_big (d, qty) SELECT date '2026-01-01' + g::int, g::int \
           FROM generate_series(1, $1::int8) g",
        &[&rows],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);

    // 2. Change to brin; table has more rows than threshold (10)
    let orm = big_orm(&h, big_model(IndexKind::Brin, IndexMethod::Brin));
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let field_step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{field_index}"))
        .unwrap();

    // 3. Simulate an interrupted build leaving an INVALID swap index
    let swap_name = format!("{field_index}__swap");
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "CREATE INDEX \"{swap_name}\" ON scale_ix_big (d); \
         UPDATE pg_index SET indisvalid = false WHERE indexrelid = '\"{swap_name}\"'::regclass;"
    ))
    .await
    .unwrap();
    let valid = one(
        &mut tx,
        &format!(
            "SELECT indisvalid::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
              WHERE c.relname = '{swap_name}'"
        ),
    )
    .await;
    assert_eq!(
        valid.as_deref(),
        Some("false"),
        "swap index must be invalid"
    );
    tx.commit().await.unwrap();
    drop(tx);

    // 4. Run the step: it drops the invalid swap index, rebuilds, and swaps
    let report = orm.run_schema_step(field_step, &options).await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());

    let mut tx = h.begin().await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let defs = index_defs(&mut tx, "scale_ix_big").await;
    // Swap index must be gone
    assert!(
        !defs.iter().any(|(n, _)| n == &swap_name),
        "swap index must not exist after swap: {defs:?}"
    );
    // Canonical index must now be BRIN and valid
    assert_eq!(
        def_of(&defs, &field_index),
        format!("CREATE INDEX {field_index} ON {schema}.scale_ix_big USING brin (d)")
    );
    let valid = one(
        &mut tx,
        &format!(
            "SELECT indisvalid::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
              WHERE c.relname = '{field_index}'"
        ),
    )
    .await;
    assert_eq!(valid.as_deref(), Some("true"));
    drop(tx);

    // 5. Test valid leftover swap index with matching marker:
    // Change model to btree
    let orm2 = big_orm(&h, big_model(IndexKind::Btree, IndexMethod::Btree));
    let mut tx = orm2.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync2 = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let step2 = sync2
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{field_index}"))
        .unwrap();

    // Pre-create a VALID swap index with matching comment marker
    let mut tx = h.begin().await.unwrap();
    let create = erp_sql::ddl::create_field_index(
        "scale_ix_big",
        "d",
        &field_index,
        IndexKind::Btree,
        |_| false,
        Unaccent::Off,
    )
    .unwrap();
    let statement =
        erp_sql::ddl::create_swap_index_concurrently(&create, &field_index, &swap_name).unwrap();
    tx.batch(&format!(
        "CREATE INDEX \"{swap_name}\" ON scale_ix_big USING btree (d); {}",
        erp_sql::ddl::partitioned_build_comment(&swap_name, &statement)
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let swap_oid = index_oid(&h, &swap_name).await;
    assert!(swap_oid.is_some());

    // Run the step: it sees the valid swap index with matching marker, does NOT rebuild, and swaps it in
    let report = orm2.run_schema_step(step2, &options).await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());

    let final_oid = index_oid(&h, &field_index).await;
    assert_eq!(
        final_oid, swap_oid,
        "valid swap index must be adopted directly into the live index"
    );

    // 6. Run the step again: must be an idempotent no-op
    let before_oid = index_oid(&h, &field_index).await;
    let report = orm2.run_schema_step(step2, &options).await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());
    let after_oid = index_oid(&h, &field_index).await;
    assert_eq!(
        before_oid, after_oid,
        "idempotent step must not drop or rebuild the index"
    );

    drop(orm2);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// While a replaced UniqueIndex on a large table is planned for swap, the existing unique index
/// remains active: duplicate inserts fail before the step, and after the swap the new index continues
/// to enforce uniqueness.
#[tokio::test]
async fn unique_index_swap_preserves_uniqueness_during_replace() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;
    let rows = 12_i64;

    // A model with a raw UniqueIndex
    fn uq_model(definition: &'static str) -> &'static ModelDecl {
        let fields: &'static [FieldDecl] = Box::leak(Box::new([
            FieldDecl::char("code"),
            FieldDecl::integer("val"),
        ]));
        let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl {
            name: "_by_code",
            kind: erp_registry::TableObjectKind::UniqueIndex {
                definition,
                message: "code must be unique",
            },
        }]));
        Box::leak(Box::new(
            ModelDecl::new("test_scale_index", "scale.ix.uq")
                .fields(fields)
                .table_objects(objects),
        ))
    }

    let orm = big_orm(&h, uq_model("(\"code\")"));
    let uq_name = object_name(orm.registry().model("scale.ix.uq").unwrap(), "_by_code");
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.execute(
        "INSERT INTO scale_ix_uq (code, val) SELECT 'c_' || g::text, g::int \
           FROM generate_series(1, $1::int8) g",
        &[&rows],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);

    // Duplicate code must be rejected by the initial unique index
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .execute(
            "INSERT INTO scale_ix_uq (code, val) VALUES ('c_1', 999)",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23505"));
    assert_eq!(
        err.message(),
        format!(
            "The database refused the change: duplicate key value violates unique constraint \"{uq_name}\""
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // Plan replacement of the unique index with include column
    let orm = big_orm(&h, uq_model("(\"code\") INCLUDE (\"val\")"));
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let uq_name = {
        let spec = orm.registry().model("scale.ix.uq").unwrap();
        object_name(spec, "_by_code")
    };
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{uq_name}"))
        .unwrap();

    // WHILE THE STEP HAS NOT RUN, the old unique index still enforces uniqueness
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .execute(
            "INSERT INTO scale_ix_uq (code, val) VALUES ('c_1', 999)",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23505"));
    assert_eq!(
        err.message(),
        format!(
            "The database refused the change: duplicate key value violates unique constraint \"{uq_name}\""
        )
    );
    tx.rollback().await.unwrap();
    drop(tx);

    // Run the swap step
    let report = orm.run_schema_step(step, &options).await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());

    // AFTER THE SWAP, the new unique index is in place and still enforces uniqueness
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .execute(
            "INSERT INTO scale_ix_uq (code, val) VALUES ('c_1', 999)",
            &[],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23505"));
    assert_eq!(
        err.message(),
        format!(
            "The database refused the change: duplicate key value violates unique constraint \"{uq_name}\""
        )
    );
    tx.rollback().await.unwrap();

    let defs = index_defs(&mut tx, "scale_ix_uq").await;
    assert!(
        def_of(&defs, &uq_name).contains("INCLUDE (val)"),
        "index must have new definition with INCLUDE: {defs:?}"
    );
    drop(tx);

    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn large_table_field_index_rebuilt_from_btree_to_btree_not_null() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;
    let rows = 12_i64;
    let field_index = "scale_ix_big__d_index".to_owned();

    // 1. Initial table with btree index and 12 rows (> threshold 10)
    let orm = big_orm(&h, big_model(IndexKind::Btree, IndexMethod::Btree));
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.execute(
        "INSERT INTO scale_ix_big (d, qty) SELECT date '2026-01-01' + g::int, g::int \
           FROM generate_series(1, $1::int8) g",
        &[&rows],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);

    // 2. Change to btree_not_null
    let orm = big_orm(&h, big_model(IndexKind::BtreeNotNull, IndexMethod::Btree));
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let field_step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{field_index}"))
        .unwrap();

    let report = orm.run_schema_step(field_step, &options).await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());

    let mut tx = h.begin().await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let defs = index_defs(&mut tx, "scale_ix_big").await;
    assert_eq!(
        def_of(&defs, &field_index),
        format!(
            "CREATE INDEX {field_index} ON {schema}.scale_ix_big USING btree (d) WHERE (d IS NOT NULL)"
        )
    );
    drop(tx);

    // Idempotent rerun
    let before_oid = index_oid(&h, &field_index).await;
    let report = orm.run_schema_step(field_step, &options).await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());
    let after_oid = index_oid(&h, &field_index).await;
    assert_eq!(before_oid, after_oid);

    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn large_table_index_rebuilt_between_gin_and_trigram() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;
    let rows = 12_i64;

    fn gin_trgm_model(definition: &'static str) -> &'static ModelDecl {
        let fields: &'static [FieldDecl] = Box::leak(Box::new([
            FieldDecl::json("data"),
            FieldDecl::integer("qty"),
        ]));
        let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl {
            name: "_by_data",
            kind: erp_registry::TableObjectKind::Index { definition },
        }]));
        Box::leak(Box::new(
            ModelDecl::new("test_scale_index", "scale.ix.gin_trgm")
                .fields(fields)
                .table_objects(objects),
        ))
    }

    // 1. Initial table with gin index (default opclass) and 12 rows
    let orm = big_orm(&h, gin_trgm_model("USING gin (\"data\")"));
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.execute(
        "INSERT INTO scale_ix_gin_trgm (data, qty) SELECT '{\"a\": 1}'::jsonb, g::int \
           FROM generate_series(1, $1::int8) g",
        &[&rows],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);

    let idx_name = {
        let temp_orm = big_orm(&h, gin_trgm_model("USING gin (\"data\")"));
        let spec = temp_orm.registry().model("scale.ix.gin_trgm").unwrap();
        object_name(spec, "_by_data")
    };

    // 2. Change to jsonb_path_ops (different opclass, same access method USING gin)
    let orm = big_orm(&h, gin_trgm_model("USING gin (\"data\" jsonb_path_ops)"));
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{idx_name}"))
        .unwrap();

    // Strip comment from existing index right before running the step to simulate a comment-less index
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!("COMMENT ON INDEX \"{idx_name}\" IS NULL"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let report = orm.run_schema_step(step, &options).await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());

    let mut tx = h.begin().await.unwrap();
    let schema = one(&mut tx, "SELECT current_schema()::text").await.unwrap();
    let defs = index_defs(&mut tx, "scale_ix_gin_trgm").await;
    assert_eq!(
        def_of(&defs, &idx_name),
        format!(
            "CREATE INDEX {idx_name} ON {schema}.scale_ix_gin_trgm USING gin (data jsonb_path_ops)"
        )
    );
    drop(tx);

    // Idempotent rerun
    let before_oid = index_oid(&h, &idx_name).await;
    let report = orm.run_schema_step(step, &options).await.unwrap();
    assert_eq!(report, erp_orm::SchemaStepReport::default());
    let after_oid = index_oid(&h, &idx_name).await;
    assert_eq!(before_oid, after_oid);

    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn large_table_failed_unique_swap_cleans_up_swap_index() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;

    fn uq_fail_model(definition: &'static str) -> &'static ModelDecl {
        let fields: &'static [FieldDecl] = Box::leak(Box::new([
            FieldDecl::char("code"),
            FieldDecl::integer("seq"),
        ]));
        let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl {
            name: "_by_code_seq",
            kind: erp_registry::TableObjectKind::UniqueIndex {
                definition,
                message: "duplicate code",
            },
        }]));
        Box::leak(Box::new(
            ModelDecl::new("test_scale_index", "scale.ix.uq_fail")
                .fields(fields)
                .table_objects(objects),
        ))
    }

    // 1. Initial composite unique index ("code", "seq")
    let orm = big_orm(&h, uq_fail_model("(\"code\", \"seq\")"));
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    // Insert duplicate codes with different seq values (valid under composite key)
    tx.execute(
        "INSERT INTO scale_ix_uq_fail (code, seq) VALUES \
           ('dup', 1), ('dup', 2), ('c3', 3), ('c4', 4), ('c5', 5), ('c6', 6), \
           ('c7', 7), ('c8', 8), ('c9', 9), ('c10', 10), ('c11', 11), ('c12', 12)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);

    let uq_name = {
        let temp_orm = big_orm(&h, uq_fail_model("(\"code\", \"seq\")"));
        let spec = temp_orm.registry().model("scale.ix.uq_fail").unwrap();
        object_name(spec, "_by_code_seq")
    };
    let old_oid = index_oid(&h, &uq_name).await;

    // 2. Change unique index to just ("code"): the data violates uniqueness!
    let orm = big_orm(&h, uq_fail_model("(\"code\")"));
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{uq_name}"))
        .unwrap();

    let err = orm.run_schema_step(step, &options).await.unwrap_err();
    assert_eq!(
        err.message(),
        format!("schema step 'index.{uq_name}' failed")
    );
    let cause = std::error::Error::source(&err)
        .and_then(|s| s.downcast_ref::<erp_fields::Error>())
        .unwrap();
    assert_eq!(cause.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(cause.sqlstate(), Some("23505"));
    let temp_name = erp_sql::ddl::swap_index_name(&uq_name);
    assert_eq!(
        cause.message(),
        format!(
            "The database refused the change: could not create unique index \"{temp_name}\": failed to build swap index {temp_name}; live index {uq_name} is unchanged and still in use"
        )
    );

    // 3. Verify no swap index is left behind
    let mut tx = h.begin().await.unwrap();
    let defs = index_defs(&mut tx, "scale_ix_uq_fail").await;
    assert!(
        !defs.iter().any(|(n, _)| n == &temp_name),
        "swap index must have been cleaned up: {defs:?}"
    );

    // 4. Verify old live index is valid and unchanged (same OID)
    let current_oid = index_oid(&h, &uq_name).await;
    assert_eq!(old_oid, current_oid, "old index OID must be preserved");
    let valid = one(
        &mut tx,
        &format!(
            "SELECT indisvalid::text FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
              WHERE c.relname = '{uq_name}'"
        ),
    )
    .await;
    assert_eq!(valid.as_deref(), Some("true"));
    drop(tx);

    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_partial_index_with_regrouped_filter_is_refused() {
    use erp_orm::SchemaSyncOptions;
    let addon = erp_registry::intern_addon("test_scale_index_regrouped", &[]).unwrap();
    let module = erp_modules::ModuleName::new("test_scale_index_regrouped").unwrap();
    let defs = erp_modules::parse_model_file(
        &module,
        "models/rec.toml",
        "[[model]]\nname = \"scale.ix.rec_regroup\"\n\n[[model.field]]\nname = \"qty\"\ntype = \"integer\"\n\n\
         [[model.field]]\nname = \"state\"\ntype = \"char\"\n\n[[model.index]]\nname = \"_grouped\"\n\
         fields = [\"qty\"]\n\
         filter = \"['|', ('state', '=', 'open'), '&', ('state', '=', 'pending'), ('qty', '=', 0)]\"\n",
    )
    .unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let h = Harness::builder([&base_users::ADDON, addon])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let rec = model(&h, "scale.ix.rec_regroup");
    let name = object_name(rec, "_grouped");

    // Replace the index with a regrouped filter without comment
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "DROP INDEX \"{name}\"; CREATE INDEX \"{name}\" ON scale_ix_rec_regroup (qty) \
         WHERE ((state = 'open' OR state = 'pending') AND qty = 0)"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let oid_before = index_oid(&h, &name).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let def_before = one(
        &mut tx,
        &format!("SELECT pg_get_indexdef('{name}'::regclass)"),
    )
    .await
    .unwrap();
    let err = tx
        .init_models_with(&[], &SchemaSyncOptions::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "index '_grouped' of 'scale.ix.rec_regroup': the table already has an index named \
             {name} that the ORM did not create (its comment is not the ORM's) and that differs \
             from the declaration; a model file does not replace it. If it is the leftover of an \
             interrupted build, dropping it (DROP INDEX {name}) is safe and the next sync builds \
             it; otherwise rename the declared index"
        )
    );
    drop(tx);

    // M3: Assert index was untouched in catalog after refusal
    let oid_after = index_oid(&h, &name).await.unwrap();
    assert_eq!(
        oid_before, oid_after,
        "refused index must not be dropped or recreated"
    );
    let mut tx = h.begin().await.unwrap();
    let def_after = one(
        &mut tx,
        &format!("SELECT pg_get_indexdef('{name}'::regclass)"),
    )
    .await
    .unwrap();
    assert_eq!(
        def_before, def_after,
        "refused index definition must be unchanged"
    );
    drop(tx);

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_partial_index_with_in_and_boolean_filter_is_adopted() {
    use erp_orm::SchemaSyncOptions;
    let addon = erp_registry::intern_addon("test_scale_index_in_bool", &[]).unwrap();
    let module = erp_modules::ModuleName::new("test_scale_index_in_bool").unwrap();
    let defs = erp_modules::parse_model_file(
        &module,
        "models/rec.toml",
        "[[model]]\nname = \"scale.ix.rec_in_bool\"\n\n[[model.field]]\nname = \"qty\"\ntype = \"integer\"\n\n\
         [[model.field]]\nname = \"state\"\ntype = \"char\"\n\n[[model.field]]\nname = \"active\"\ntype = \"boolean\"\n\n\
         [[model.index]]\nname = \"_in_bool\"\nfields = [\"qty\"]\n\
         filter = \"[('state', 'in', ['open', 'pending']), ('active', '=', True)]\"\n",
    )
    .unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let h = Harness::builder([&base_users::ADDON, addon])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let rec = model(&h, "scale.ix.rec_in_bool");
    let name = object_name(rec, "_in_bool");

    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!("COMMENT ON INDEX \"{name}\" IS NULL"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let oid_before = index_oid(&h, &name).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let comment_before = one(
        &mut tx,
        &format!("SELECT obj_description('{name}'::regclass, 'pg_class')"),
    )
    .await;
    assert!(comment_before.is_none());

    tx.capture_statements(true);
    let sync = tx
        .init_models_with(&[], &SchemaSyncOptions::new())
        .await
        .unwrap();
    assert!(sync.steps().is_empty());
    let ddl: Vec<String> = statements(tx.captured())
        .into_iter()
        .filter(|s| s.contains(name.as_str()))
        .collect();
    assert_eq!(ddl.len(), 1, "{ddl:#?}");
    assert!(ddl[0].starts_with("COMMENT ON INDEX"), "{ddl:#?}");
    tx.commit().await.unwrap();
    drop(tx);

    // M3: Assert catalog state proves adoption without rebuild
    let oid_after = index_oid(&h, &name).await.unwrap();
    assert_eq!(
        oid_before, oid_after,
        "index must be adopted without drop/recreate"
    );
    let mut tx = h.begin().await.unwrap();
    let comment_after = one(
        &mut tx,
        &format!("SELECT obj_description('{name}'::regclass, 'pg_class')"),
    )
    .await;
    assert!(
        comment_after
            .as_deref()
            .is_some_and(|c| c.starts_with("INDEX USING ")),
        "ORM comment must be restored on adopted index: {comment_after:?}"
    );
    drop(tx);

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_partial_index_with_negative_literal_filter_is_adopted() {
    use erp_orm::SchemaSyncOptions;
    let addon = erp_registry::intern_addon("test_scale_index_neg_lit", &[]).unwrap();
    let module = erp_modules::ModuleName::new("test_scale_index_neg_lit").unwrap();
    let defs = erp_modules::parse_model_file(
        &module,
        "models/rec.toml",
        "[[model]]\nname = \"scale.ix.rec_neg_lit\"\n\n[[model.field]]\nname = \"qty\"\ntype = \"integer\"\n\n\
         [[model.index]]\nname = \"_neg_lit\"\nfields = [\"qty\"]\n\
         filter = \"[('qty', '=', -5)]\"\n",
    )
    .unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let h = Harness::builder([&base_users::ADDON, addon])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let rec = model(&h, "scale.ix.rec_neg_lit");
    let name = object_name(rec, "_neg_lit");

    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!("COMMENT ON INDEX \"{name}\" IS NULL"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let oid_before = index_oid(&h, &name).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let comment_before = one(
        &mut tx,
        &format!("SELECT obj_description('{name}'::regclass, 'pg_class')"),
    )
    .await;
    assert!(comment_before.is_none());

    tx.capture_statements(true);
    let sync = tx
        .init_models_with(&[], &SchemaSyncOptions::new())
        .await
        .unwrap();
    assert!(sync.steps().is_empty());
    let ddl: Vec<String> = statements(tx.captured())
        .into_iter()
        .filter(|s| s.contains(name.as_str()))
        .collect();
    assert_eq!(ddl.len(), 1, "{ddl:#?}");
    assert!(ddl[0].starts_with("COMMENT ON INDEX"), "{ddl:#?}");
    tx.commit().await.unwrap();
    drop(tx);

    // M3: Assert catalog state proves adoption without rebuild
    let oid_after = index_oid(&h, &name).await.unwrap();
    assert_eq!(
        oid_before, oid_after,
        "index must be adopted without drop/recreate"
    );
    let mut tx = h.begin().await.unwrap();
    let comment_after = one(
        &mut tx,
        &format!("SELECT obj_description('{name}'::regclass, 'pg_class')"),
    )
    .await;
    assert!(
        comment_after
            .as_deref()
            .is_some_and(|c| c.starts_with("INDEX USING ")),
        "ORM comment must be restored on adopted index: {comment_after:?}"
    );
    drop(tx);

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_trigram_index_with_lower_wrapper_is_refused() {
    use erp_orm::SchemaSyncOptions;
    let addon = erp_registry::intern_addon("test_scale_index_trgm_lower", &[]).unwrap();
    let module = erp_modules::ModuleName::new("test_scale_index_trgm_lower").unwrap();
    let defs = erp_modules::parse_model_file(
        &module,
        "models/rec.toml",
        "[[model]]\nname = \"scale.ix.rec_trgm_lower\"\n\n[[model.field]]\nname = \"code\"\ntype = \"char\"\n\n\
         [[model.index]]\nname = \"_code_trgm\"\nfields = [\"code\"]\nmethod = \"trigram\"\n",
    )
    .unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let h = Harness::builder([&base_users::ADDON, addon])
        .overlay(overlay)
        .before_schema("CREATE EXTENSION IF NOT EXISTS unaccent")
        .config(erp_sdk::OrmConfig {
            unaccent: true,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let rec = model(&h, "scale.ix.rec_trgm_lower");
    let name = object_name(rec, "_code_trgm");

    let mut tx = h.begin().await.unwrap();
    let has_unaccent = one(
        &mut tx,
        &format!(
            "SELECT to_regproc('{}')::text",
            erp_sql::filter::UNACCENT_WRAPPER
        ),
    )
    .await
    .is_some();
    assert!(
        has_unaccent,
        "test requires {} wrapper",
        erp_sql::filter::UNACCENT_WRAPPER
    );
    let key = trigram_key(&mut tx, "code").await;
    tx.batch(&format!(
        "DROP INDEX \"{name}\"; CREATE INDEX \"{name}\" ON scale_ix_rec_trgm_lower USING gin (lower({key}) gin_trgm_ops)"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let oid_before = index_oid(&h, &name).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let def_before = one(
        &mut tx,
        &format!("SELECT pg_get_indexdef('{name}'::regclass)"),
    )
    .await
    .unwrap();
    let err = tx
        .init_models_with(&[], &SchemaSyncOptions::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "index '_code_trgm' of 'scale.ix.rec_trgm_lower': the table already has an index named \
             {name} that the ORM did not create (its comment is not the ORM's) and that differs \
             from the declaration; a model file does not replace it. If it is the leftover of an \
             interrupted build, dropping it (DROP INDEX {name}) is safe and the next sync builds \
             it; otherwise rename the declared index"
        )
    );
    drop(tx);

    // M3: Assert index was untouched in catalog after refusal
    let oid_after = index_oid(&h, &name).await.unwrap();
    assert_eq!(
        oid_before, oid_after,
        "refused index must not be dropped or recreated"
    );
    let mut tx = h.begin().await.unwrap();
    let def_after = one(
        &mut tx,
        &format!("SELECT pg_get_indexdef('{name}'::regclass)"),
    )
    .await
    .unwrap();
    assert_eq!(
        def_before, def_after,
        "refused index definition must be unchanged"
    );
    drop(tx);

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_trigram_index_with_reversed_keys_is_refused() {
    use erp_orm::SchemaSyncOptions;
    let addon = erp_registry::intern_addon("test_scale_index_trgm_rev", &[]).unwrap();
    let module = erp_modules::ModuleName::new("test_scale_index_trgm_rev").unwrap();
    let defs = erp_modules::parse_model_file(
        &module,
        "models/rec.toml",
        "[[model]]\nname = \"scale.ix.rec_trgm_rev\"\n\n[[model.field]]\nname = \"code\"\ntype = \"char\"\n\n\
         [[model.field]]\nname = \"name\"\ntype = \"char\"\n\n\
         [[model.index]]\nname = \"_multi_trgm\"\nfields = [\"code\", \"name\"]\nmethod = \"trigram\"\n",
    )
    .unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let h = Harness::builder([&base_users::ADDON, addon])
        .overlay(overlay)
        .before_schema("CREATE EXTENSION IF NOT EXISTS unaccent")
        .config(erp_sdk::OrmConfig {
            unaccent: true,
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let rec = model(&h, "scale.ix.rec_trgm_rev");
    let name = object_name(rec, "_multi_trgm");

    let mut tx = h.begin().await.unwrap();
    let has_unaccent = one(
        &mut tx,
        &format!(
            "SELECT to_regproc('{}')::text",
            erp_sql::filter::UNACCENT_WRAPPER
        ),
    )
    .await
    .is_some();
    assert!(
        has_unaccent,
        "test requires {} wrapper",
        erp_sql::filter::UNACCENT_WRAPPER
    );
    let code_key = trigram_key(&mut tx, "code").await;
    let name_key = trigram_key(&mut tx, "name").await;
    tx.batch(&format!(
        "DROP INDEX \"{name}\"; CREATE INDEX \"{name}\" ON scale_ix_rec_trgm_rev USING gin ({name_key} gin_trgm_ops, {code_key} gin_trgm_ops)"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let oid_before = index_oid(&h, &name).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let def_before = one(
        &mut tx,
        &format!("SELECT pg_get_indexdef('{name}'::regclass)"),
    )
    .await
    .unwrap();
    let err = tx
        .init_models_with(&[], &SchemaSyncOptions::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!(
            "index '_multi_trgm' of 'scale.ix.rec_trgm_rev': the table already has an index named \
             {name} that the ORM did not create (its comment is not the ORM's) and that differs \
             from the declaration; a model file does not replace it. If it is the leftover of an \
             interrupted build, dropping it (DROP INDEX {name}) is safe and the next sync builds \
             it; otherwise rename the declared index"
        )
    );
    drop(tx);

    // M3: Assert index was untouched in catalog after refusal
    let oid_after = index_oid(&h, &name).await.unwrap();
    assert_eq!(
        oid_before, oid_after,
        "refused index must not be dropped or recreated"
    );
    let mut tx = h.begin().await.unwrap();
    let def_after = one(
        &mut tx,
        &format!("SELECT pg_get_indexdef('{name}'::regclass)"),
    )
    .await
    .unwrap();
    assert_eq!(
        def_before, def_after,
        "refused index definition must be unchanged"
    );
    drop(tx);

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_partial_index_on_long_table_and_field_is_adopted() {
    use erp_orm::SchemaSyncOptions;
    // len("scale_ix_analytic_distribution_model") = 36 bytes
    // len("partner_category_id") = 19 bytes
    // 36 + 19 = 55 bytes (> 47 bytes, M2 probe truncation regression test)
    let addon = erp_registry::intern_addon("test_scale_index_long_name", &[]).unwrap();
    let module = erp_modules::ModuleName::new("test_scale_index_long_name").unwrap();
    let defs = erp_modules::parse_model_file(
        &module,
        "models/rec.toml",
        "[[model]]\nname = \"scale.ix.analytic_distribution_model\"\n\n\
         [[model.field]]\nname = \"partner_category_id\"\ntype = \"integer\"\n\n\
         [[model.index]]\nname = \"_cat_partial\"\nfields = [\"partner_category_id\"]\n\
         filter = \"[('partner_category_id', '!=', False)]\"\n",
    )
    .unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let h = Harness::builder([&base_users::ADDON, addon])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let rec = model(&h, "scale.ix.analytic_distribution_model");
    let name = object_name(rec, "_cat_partial");

    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!("COMMENT ON INDEX \"{name}\" IS NULL"))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let oid_before = index_oid(&h, &name).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let comment_before = one(
        &mut tx,
        &format!("SELECT obj_description('{name}'::regclass, 'pg_class')"),
    )
    .await;
    assert!(comment_before.is_none());

    tx.capture_statements(true);
    let sync = tx
        .init_models_with(&[], &SchemaSyncOptions::new())
        .await
        .unwrap();
    assert!(sync.steps().is_empty());
    let ddl: Vec<String> = statements(tx.captured())
        .into_iter()
        .filter(|s| s.contains(name.as_str()))
        .collect();
    assert_eq!(ddl.len(), 1, "{ddl:#?}");
    assert!(ddl[0].starts_with("COMMENT ON INDEX"), "{ddl:#?}");
    tx.commit().await.unwrap();
    drop(tx);

    let oid_after = index_oid(&h, &name).await.unwrap();
    assert_eq!(
        oid_before, oid_after,
        "index must be adopted without drop/recreate"
    );
    let mut tx = h.begin().await.unwrap();
    let comment_after = one(
        &mut tx,
        &format!("SELECT obj_description('{name}'::regclass, 'pg_class')"),
    )
    .await;
    assert!(
        comment_after
            .as_deref()
            .is_some_and(|c| c.starts_with("INDEX USING ")),
        "ORM comment must be restored on adopted index: {comment_after:?}"
    );
    drop(tx);

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn large_table_swap_applies_changed_predicate_content() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;
    let make = |filter| {
        let registry = Arc::new(
            RegistryBuilder::new([&base_users::ADDON, &test_scale_index::ADDON])
                .with_decls([filtered_model(filter)])
                .build()
                .unwrap(),
        );
        Orm::builder(registry, h.db().pool().clone())
            .build()
            .unwrap()
    };
    let old = make("[('qty', '=', 0)]");
    let mut tx = old.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.batch("INSERT INTO scale_ix_filt (qty) SELECT generate_series(1, 12)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(old);
    let orm = make("[('qty', '=', 5)]");
    let name = object_name(orm.registry().model("scale.ix.filt").unwrap(), "_open");
    let before = index_oid(&h, &name).await;
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{name}"))
        .unwrap();
    assert_eq!(
        orm.run_schema_step(step, &options).await.unwrap(),
        erp_orm::SchemaStepReport::default()
    );
    let after = index_oid(&h, &name).await;
    assert_ne!(
        before, after,
        "changed predicate must replace the old index"
    );
    let mut tx = h.begin().await.unwrap();
    let defs = index_defs(&mut tx, "scale_ix_filt").await;
    assert!(
        def_of(&defs, &name).ends_with("WHERE (qty = 5)"),
        "{defs:#?}"
    );
    drop(tx);
    assert_eq!(
        orm.run_schema_step(step, &options).await.unwrap(),
        erp_orm::SchemaStepReport::default()
    );
    assert_eq!(index_oid(&h, &name).await, after);
    drop(orm);
    h.cleanup().await.unwrap();
}

/// A raw unique index used to exercise constraint ownership acquired after planning.
fn swap_constraint_model(definition: &'static str) -> &'static ModelDecl {
    let fields = Box::leak(Box::new([
        FieldDecl::integer("code"),
        FieldDecl::integer("val"),
    ]));
    let objects = Box::leak(Box::new([TableObjectDecl {
        name: "_code",
        kind: TableObjectKind::UniqueIndex {
            definition,
            message: "code must be unique",
        },
    }]));
    Box::leak(Box::new(
        ModelDecl::new("test_scale_index", "scale.ix.constraint")
            .fields(fields)
            .table_objects(objects),
    ))
}

#[tokio::test]
async fn large_table_swap_unique_constraint_and_dependency_preflight() {
    use erp_orm::SchemaSyncOptions;
    for case in ["unique", "unique_fk", "raw_fk", "deferred"] {
        let dependent = case.ends_with("_fk");
        let h = harness().await;
        let old = big_orm(&h, swap_constraint_model("(\"code\")"));
        let name = object_name(
            old.registry().model("scale.ix.constraint").unwrap(),
            "_code",
        );
        let mut tx = old.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        tx.init_models(&[]).await.unwrap();
        tx.batch(
            "INSERT INTO scale_ix_constraint (code, val) SELECT g, g FROM generate_series(1, 12) g",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        drop(old);
        let orm = big_orm(&h, swap_constraint_model("(\"code\") INCLUDE (\"val\")"));
        let options = SchemaSyncOptions::new().large_table_rows(10);
        let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        let sync = tx.init_models_with(&[], &options).await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let step = sync
            .steps()
            .iter()
            .find(|s| s.key() == format!("index.{name}"))
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        if case != "raw_fk" {
            let deferred = if case == "deferred" {
                " DEFERRABLE INITIALLY DEFERRED"
            } else {
                ""
            };
            tx.batch(&format!("ALTER TABLE scale_ix_constraint ADD CONSTRAINT {} UNIQUE USING INDEX {}{}; COMMENT ON CONSTRAINT {} ON scale_ix_constraint IS 'kept constraint comment'", erp_sql::quote(&name), erp_sql::quote(&name), deferred, erp_sql::quote(&name))).await.unwrap();
        }
        if dependent {
            tx.batch(
                "CREATE TABLE swap_dependent (code integer REFERENCES scale_ix_constraint(code))",
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        drop(tx);
        let before = index_oid(&h, &name).await;
        if dependent {
            let err = orm.run_schema_step(step, &options).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Programming);
            assert_eq!(err.message(), format!("schema step 'index.{name}' failed"));
            assert_eq!(
                std::error::Error::source(&err).unwrap().to_string(),
                format!(
                    "index {name}: dependent foreign keys prevent replacing it; nothing was built; migrate the dependent keys before retrying"
                )
            );
            assert_eq!(index_oid(&h, &name).await, before);
        } else {
            assert_eq!(
                orm.run_schema_step(step, &options).await.unwrap(),
                erp_orm::SchemaStepReport::default()
            );
            assert_ne!(index_oid(&h, &name).await, before);
            let mut tx = h.begin().await.unwrap();
            let constraint = one(
                &mut tx,
                &format!(
                    "SELECT contype::text FROM pg_constraint WHERE conindid = '{}'::regclass",
                    name
                ),
            )
            .await;
            assert_eq!(constraint.as_deref(), Some("u"));
            let flags = one(&mut tx, &format!("SELECT condeferrable::text || ':' || condeferred::text || ':' || obj_description(oid, 'pg_constraint') FROM pg_constraint WHERE conindid = '{}'::regclass", name)).await;
            let expected = if case == "deferred" {
                "true:true:kept constraint comment"
            } else {
                "false:false:kept constraint comment"
            };
            assert_eq!(flags.as_deref(), Some(expected));
            let defs = index_defs(&mut tx, "scale_ix_constraint").await;
            assert!(def_of(&defs, &name).ends_with("INCLUDE (val)"), "{defs:?}");
            tx.batch("SET CONSTRAINTS ALL IMMEDIATE").await.unwrap();
            let err = tx
                .execute("INSERT INTO scale_ix_constraint (code) VALUES (1)", &[])
                .await
                .unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Validation);
            assert_eq!(err.sqlstate(), Some("23505"));
            assert_eq!(
                err.message(),
                format!(
                    "The database refused the change: duplicate key value violates unique constraint \"{name}\""
                )
            );
            drop(tx);
            let after = index_oid(&h, &name).await;
            assert_eq!(
                orm.run_schema_step(step, &options).await.unwrap(),
                erp_orm::SchemaStepReport::default()
            );
            assert_eq!(index_oid(&h, &name).await, after);
        }
        assert_eq!(
            index_oid(&h, &erp_sql::ddl::swap_index_name(&name)).await,
            None
        );
        drop(orm);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn large_table_plain_index_step_refuses_competing_runner() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;
    sync_with(&h, kind_model(Some(IndexKind::Btree))).await;
    let mut tx = h.begin().await.unwrap();
    tx.batch(
        "DROP INDEX scale_ix_kind__d_index; INSERT INTO scale_ix_kind (d) SELECT date '2026-01-01' + g FROM generate_series(1, 12) g",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let orm = big_orm(&h, kind_model(Some(IndexKind::Btree)));
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let name = "scale_ix_kind__d_index";
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{name}"))
        .unwrap();
    let holder = h.db().client().await.unwrap();
    let taken: bool = holder
        .query_one(erp_sql::partition::STEP_TRY_LOCK, &[&name])
        .await
        .unwrap()
        .get(0);
    assert!(taken);
    let result = orm.run_schema_step(step, &options).await;
    let released: bool = holder
        .query_one(erp_sql::partition::STEP_UNLOCK, &[&name])
        .await
        .unwrap()
        .get(0);
    assert!(released);
    let err = result.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Lock);
    assert_eq!(err.message(), format!("schema step 'index.{name}' failed"));
    let source = std::error::Error::source(&err).unwrap();
    assert_eq!(
        source.to_string(),
        format!(
            "index {name}: another runner is building this index (its per-step lock is held); run the step again when that runner has finished"
        )
    );
    assert_eq!(index_oid(&h, name).await, None);
    drop(holder);
    assert_eq!(
        orm.run_schema_step(step, &options).await.unwrap(),
        erp_orm::SchemaStepReport::default()
    );
    assert!(index_oid(&h, name).await.is_some());
    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn large_table_swap_applies_changed_storage_parameter() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;
    let old = big_orm(
        &h,
        swap_constraint_model("(\"code\") WITH (fillfactor = 70)"),
    );
    let name = object_name(
        old.registry().model("scale.ix.constraint").unwrap(),
        "_code",
    );
    let mut tx = old.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.batch(
        "INSERT INTO scale_ix_constraint (code, val) SELECT g, g FROM generate_series(1, 12) g",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(old);
    let orm = big_orm(
        &h,
        swap_constraint_model("(\"code\") WITH (fillfactor = 90)"),
    );
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{name}"))
        .unwrap();
    let before = index_oid(&h, &name).await;
    assert_eq!(
        orm.run_schema_step(step, &options).await.unwrap(),
        erp_orm::SchemaStepReport::default()
    );
    let after = index_oid(&h, &name).await;
    assert_ne!(before, after);
    let mut tx = h.begin().await.unwrap();
    let options = one(
        &mut tx,
        &format!(
            "SELECT reloptions::text FROM pg_class WHERE oid = '{}'::regclass",
            name
        ),
    )
    .await;
    assert_eq!(options.as_deref(), Some("{fillfactor=90}"));
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn large_table_swap_refuses_primary_and_exclusion_owners() {
    use erp_orm::SchemaSyncOptions;
    for kind in ["p", "x"] {
        let h = harness().await;
        let old = big_orm(&h, swap_constraint_model("(\"code\")"));
        let name = object_name(
            old.registry().model("scale.ix.constraint").unwrap(),
            "_code",
        );
        let mut tx = old.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        tx.init_models(&[]).await.unwrap();
        tx.batch(
            "INSERT INTO scale_ix_constraint (code, val) SELECT g, g FROM generate_series(1, 12) g",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        drop(old);
        let orm = big_orm(&h, swap_constraint_model("(\"code\") INCLUDE (\"val\")"));
        let options = SchemaSyncOptions::new().large_table_rows(10);
        let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
        let sync = tx.init_models_with(&[], &options).await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let step = sync
            .steps()
            .iter()
            .find(|s| s.key() == format!("index.{name}"))
            .unwrap();
        let mut tx = h.begin().await.unwrap();
        let change = if kind == "p" {
            format!(
                "ALTER TABLE scale_ix_constraint DROP CONSTRAINT scale_ix_constraint_pkey; DROP INDEX {}; ALTER TABLE scale_ix_constraint ADD CONSTRAINT {} PRIMARY KEY (code)",
                erp_sql::quote(&name),
                erp_sql::quote(&name)
            )
        } else {
            format!(
                "DROP INDEX {}; ALTER TABLE scale_ix_constraint ADD CONSTRAINT {} EXCLUDE USING btree (code WITH =)",
                erp_sql::quote(&name),
                erp_sql::quote(&name)
            )
        };
        tx.batch(&change).await.unwrap();
        tx.commit().await.unwrap();
        drop(tx);
        let before = index_oid(&h, &name).await;
        let err = orm.run_schema_step(step, &options).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Programming);
        assert_eq!(err.message(), format!("schema step 'index.{name}' failed"));
        assert_eq!(
            std::error::Error::source(&err).unwrap().to_string(),
            format!(
                "index {name}: constraint kind {kind} cannot be replaced by a unique-index swap; nothing was built"
            )
        );
        assert_eq!(index_oid(&h, &name).await, before);
        assert_eq!(
            index_oid(&h, &erp_sql::ddl::swap_index_name(&name)).await,
            None
        );
        drop(orm);
        h.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn large_table_swap_refuses_unmarked_valid_temporary_index() {
    use erp_orm::SchemaSyncOptions;
    let h = harness().await;
    let old = big_orm(&h, swap_constraint_model("(\"code\")"));
    let name = object_name(
        old.registry().model("scale.ix.constraint").unwrap(),
        "_code",
    );
    let temp = erp_sql::ddl::swap_index_name(&name);
    let mut tx = old.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.batch(
        "INSERT INTO scale_ix_constraint (code, val) SELECT g, g FROM generate_series(1, 12) g",
    )
    .await
    .unwrap();
    tx.batch(&format!(
        "CREATE UNIQUE INDEX {} ON scale_ix_constraint (val)",
        erp_sql::quote(&temp)
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    drop(old);
    let orm = big_orm(&h, swap_constraint_model("(\"code\") INCLUDE (\"val\")"));
    let options = SchemaSyncOptions::new().large_table_rows(10);
    let mut tx = orm.begin(erp_sdk::SUPERUSER_ID).await.unwrap();
    let sync = tx.init_models_with(&[], &options).await.unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let step = sync
        .steps()
        .iter()
        .find(|s| s.key() == format!("index.{name}"))
        .unwrap();
    let before = index_oid(&h, &name).await;
    let occupied = index_oid(&h, &temp).await;
    let err = orm.run_schema_step(step, &options).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), format!("schema step 'index.{name}' failed"));
    assert_eq!(
        std::error::Error::source(&err).unwrap().to_string(),
        format!(
            "swap index {temp}: valid index has no ERP build marker; inspect and rename or remove it before retrying; live index {name} is unchanged"
        )
    );
    assert_eq!(index_oid(&h, &name).await, before);
    assert_eq!(index_oid(&h, &temp).await, occupied);
    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn foreign_partial_index_with_failing_probe_reports_the_probe_error() {
    use erp_orm::SchemaSyncOptions;
    let addon = erp_registry::intern_addon("test_scale_index_probe_fail", &[]).unwrap();
    let module = erp_modules::ModuleName::new("test_scale_index_probe_fail").unwrap();
    let defs = erp_modules::parse_model_file(
        &module,
        "models/rec.toml",
        "[[model]]\nname = \"scale.ix.rec_probe\"\n\n[[model.field]]\nname = \"qty\"\ntype = \"integer\"\n\n\
         [[model.field]]\nname = \"state\"\ntype = \"char\"\n\n[[model.index]]\nname = \"_open\"\n\
         fields = [\"state\"]\nfilter = \"[('qty', '=', 0)]\"\n",
    )
    .unwrap();
    let mut overlay = erp_registry::Overlay::new();
    overlay.add_addon_defs(defs).unwrap();
    let h = Harness::builder([&base_users::ADDON, addon])
        .overlay(overlay)
        .build()
        .await
        .unwrap();
    let rec = model(&h, "scale.ix.rec_probe");
    let name = object_name(rec, "_open");

    // A foreign index (no ORM comment) while the table lacks the predicate's column `qty` (the
    // index follows the renamed column), so the probe's CREATE INDEX fails on the server.
    let mut tx = h.begin().await.unwrap();
    tx.batch(&format!(
        "DROP INDEX \"{name}\"; ALTER TABLE scale_ix_rec_probe RENAME COLUMN qty TO qty_old; \
         CREATE INDEX \"{name}\" ON scale_ix_rec_probe (state) WHERE qty_old = 0"
    ))
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let mut tx = h.begin().await.unwrap();
    let err = tx
        .init_models_with(&[], &SchemaSyncOptions::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    // The probe's own error, not a rollback failure: the parts our code builds are asserted
    // exactly, the server's own sentence only by the identifier it must name. PostgreSQL
    // localises its messages through `lc_messages` and may reword them between releases, so
    // asserting the server text verbatim would tie this test to one locale and one version.
    let msg = err.message();
    assert!(
        msg.starts_with(&format!(
            "could not canonicalise index predicate '(\"qty\" = 0 OR \"qty\" IS NULL)' for table \
             'scale_ix_rec_probe' (index '{name}'): "
        )),
        "unexpected prefix: {msg}"
    );
    assert!(
        msg.ends_with(" (a later release)"),
        "unexpected suffix: {msg}"
    );
    assert!(
        msg.contains("qty"),
        "the server's message must name the column: {msg}"
    );
    drop(tx);

    h.cleanup().await.unwrap();
}
