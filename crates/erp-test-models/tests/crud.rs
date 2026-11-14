//! CRUD cases of S1 (create, minimal write, fields_get, schema) and defaults (`default_get`).
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use support::currency_fixture;

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use erp_registry::{
    AddonDescriptor, DefaultContext, FieldDecl, ModelDecl, ModelSpec, RegistryBuilder, StaticValue,
    TableObjectDecl, TableObjectKind,
};
use erp_sdk::__private::{ImplKind, MethodFn, MethodImpl};
use erp_sdk::{
    BoxFuture, Domain, Error, ErrorKind, OnDelete, Operation, Orm, OrmConfig, RawVals, Records,
    SUPERUSER_ID, StaticPolicy, Super, Transaction, Vals, Value,
};
use erp_test_models::test_orm::{Mixed, ModelB, Partner};
use erp_test_models::{base_users, test_orm, test_web};
use erp_testkit::{Harness, assert_query_count};
use support::{map, one, rows, vmap};

#[tokio::test]
async fn spec_crud_001_create_accepts_one_or_many() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let mixed = tx.env().model::<Mixed>().unwrap();
    let one_rec = mixed
        .create(
            &mut tx,
            vec![
                Vals::new()
                    .set(Mixed::CHAR, "Foo")
                    .set(Mixed::TEXT, "TEXT EXAMPLE"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(one_rec.len(), 1);
    assert_eq!(
        one_rec.get(&mut tx, Mixed::TEXT).await.unwrap().as_deref(),
        Some("TEXT EXAMPLE")
    );
    let none = assert_query_count!(tx, <= 0, { mixed.create(&mut tx, vec![]).await.unwrap() });
    assert!(none.is_empty());
    let three = mixed
        .create(
            &mut tx,
            vec![
                Vals::new().set(Mixed::CHAR, "Foo").set(Mixed::TEXT, "T"),
                Vals::new().set(Mixed::CHAR, "Bar"),
                Vals::new().set(Mixed::CHAR, "Baz"),
            ],
        )
        .await
        .unwrap();
    let chars = three.mapped(&mut tx, Mixed::CHAR).await.unwrap();
    let chars: Vec<_> = chars.iter().map(|c| c.as_deref().unwrap()).collect();
    assert_eq!(chars, ["Foo", "Bar", "Baz"]);
    assert_eq!(
        three.values(&mut tx, "text").await.unwrap(),
        [Value::str("T"), Value::False, Value::False]
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_002_unknown_field_in_create_or_write() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_orm.partner").unwrap();
    let err = partners
        .create_values(
            &mut tx,
            &[map(&[("name", Value::str("x")), ("nope", Value::Int(1))])],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(err.message(), "Invalid field 'nope' in 'test_orm.partner'");
    let typed = tx.env().model::<Partner>().unwrap();
    let err = partners
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("nope", erp_sdk::Cell::Int(1))],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(err.message(), "Invalid field 'nope' in 'test_orm.partner'");
    assert_eq!(
        one(&mut tx, "SELECT count(*)::text FROM test_orm_partner")
            .await
            .as_deref(),
        Some("0")
    );
    let rec = typed.create(&mut tx, vec![Vals::new()]).await.unwrap();
    let err = rec
        .write_values(&mut tx, &map(&[("nope", Value::Int(1))]))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_sdk::ErrorKind::Programming);
    assert_eq!(err.message(), "Invalid field 'nope' in 'test_orm.partner'");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_003_magic_values_on_create_and_write() {
    let h = support::orm().await;
    // a regular user
    let mut tx = h.begin().await.unwrap();
    let user = tx
        .env()
        .records("res.users")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[
                ("name", Value::str("Demo")),
                ("login", Value::str("demo")),
            ])],
        )
        .await
        .unwrap();
    let uid = user.ids()[0];
    tx.commit().await.unwrap();

    let mut tx = h.begin_as(uid).await.unwrap();
    assert!(!tx.env().su());
    let partners = tx.env().records("test_orm.partner").unwrap();
    let given = map(&[
        ("id", Value::Int(999)),
        ("create_uid", Value::Int(1)),
        ("create_date", Value::str("2000-01-01")),
        ("name", Value::str("p")),
    ]);
    let recs = partners
        .create_values(&mut tx, &[given.clone(), given])
        .await
        .unwrap();
    assert!(!recs.ids().contains(&999));
    tx.invalidate_all().await.unwrap();
    // raw ids: the default load reads many2one values as `[id, display_name]`
    let read = recs
        .read_with(
            &mut tx,
            &["create_uid", "write_uid", "create_date", "write_date"],
            erp_sdk::Load::Raw,
        )
        .await
        .unwrap();
    for row in &read {
        assert_eq!(row["create_uid"], Value::Int(uid));
        assert_eq!(row["write_uid"], Value::Int(uid));
        assert_eq!(row["create_date"], row["write_date"]);
        assert_eq!(row["create_date"], read[0]["create_date"]);
        assert_ne!(row["create_date"], Value::False);
    }
    let first = recs.at(0).unwrap();
    first
        .write_values(&mut tx, &map(&[("write_uid", Value::Int(1))]))
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        first.value(&mut tx, "write_uid").await.unwrap(),
        Value::Int(uid)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// the rows of one call share one column set, NULL where a row gives no value, and
/// are inserted in bounded batches with one array parameter per column; ids follow the input
/// order, F6: one ordered INSERT per batch, no id reservation)
#[tokio::test]
async fn spec_crud_010_batched_insert_shape() {
    static NOLOG: AddonDescriptor = AddonDescriptor {
        name: "test_nolog",
        depends: &["base"],
        exact_only: false,
    };
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON, &NOLOG])
        .with_static_decls(crate::currency_fixture::decls())
        .with_decls([ModelDecl::new("test_nolog", "x.nolog").log_access(false)])
        .config(OrmConfig {
            insert_batch: 100,
            ..OrmConfig::default()
        })
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    // the second row gives an email: the call's column set includes it, NULL for the other rows
    let mut vals: Vec<Vals<Partner>> = (0..250)
        .map(|i| partners.vals().set(Partner::NAME, format!("p{i}").as_str()))
        .collect();
    vals[1] = vals[1].clone().set(Partner::EMAIL, "second@x");
    tx.capture_statements(true);
    // 250 rows in 3 batches of at most 100, no id reservation (before: the ids, 249
    // rows in 3 batches and 1 row with an email, 5 statements)
    let recs =
        assert_query_count!(tx, >= 3, <= 3, { partners.create(&mut tx, vals).await.unwrap() });
    let inserts = tx.captured().to_vec();
    assert!(
        inserts
            .iter()
            .all(|s| s.starts_with("WITH \"__r\" AS MATERIALIZED") && s.contains("INSERT INTO ")),
        "{inserts:?}"
    );
    let ids = recs.ids();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "ids follow the input order"
    );
    assert!(
        inserts.iter().all(|s| !s.contains("DEFAULT")),
        "no per-cell DEFAULT"
    );
    assert_eq!(
        inserts.iter().filter(|s| s.contains("\"email\"")).count(),
        3
    );
    assert!(inserts[0].contains(
        "INSERT INTO \"test_orm_partner\" (\"id\", \"active\", \"create_uid\", \"email\", \"name\", \"write_uid\", \"create_date\", \"write_date\")"
    ), "{}", inserts[0]);
    let ids = recs.ids();
    assert_eq!(ids.len(), 250);
    let names = recs.mapped(&mut tx, Partner::NAME).await.unwrap();
    for (i, name) in names.iter().enumerate() {
        assert_eq!(
            name.as_deref(),
            Some(format!("p{i}").as_str()),
            "row {i} got its own id"
        );
    }
    tx.invalidate_all().await.unwrap();
    let emails = recs.mapped(&mut tx, Partner::EMAIL).await.unwrap();
    assert_eq!(emails[1].as_deref(), Some("second@x"));
    assert!(
        emails
            .iter()
            .enumerate()
            .all(|(i, e)| i == 1 || e.is_none())
    );
    // a model without any stored value
    tx.capture_statements(true);
    let nolog = tx.env().records("x.nolog").unwrap();
    let three = nolog
        .create(&mut tx, vec![RawVals::new(); 3])
        .await
        .unwrap();
    assert_eq!(three.len(), 3);
    // one column set (none): one INSERT that draws the ids in input order
    assert_eq!(tx.captured().len(), 1, "{:?}", tx.captured());
    assert!(
        tx.captured()[0].contains("INSERT INTO \"x_nolog\" (\"id\") SELECT \"id\" FROM \"__v\""),
        "{:?}",
        tx.captured()
    );
    assert!(three.ids().windows(2).all(|w| w[0] < w[1]));
    // rows sharing one column set: one ordered INSERT per batch, no reservation (F6)
    tx.capture_statements(true);
    let partners = tx.env().model::<Partner>().unwrap();
    let same: Vec<Vals<Partner>> = (0..250)
        .map(|i| partners.vals().set(Partner::NAME, format!("s{i}").as_str()))
        .collect();
    let recs =
        assert_query_count!(tx, >= 3, <= 3, { partners.create(&mut tx, same).await.unwrap() });
    assert!(
        tx.captured()
            .iter()
            .all(|s| s.starts_with("WITH \"__r\" AS MATERIALIZED")
                && s.contains("nextval((SELECT \"seq\" FROM \"__s\"))")),
        "{:?}",
        tx.captured()
    );
    assert!(
        recs.ids().windows(2).all(|w| w[0] < w[1]),
        "ids follow the input order across batches"
    );
    tx.invalidate_all().await.unwrap();
    let names = recs.mapped(&mut tx, Partner::NAME).await.unwrap();
    assert!(
        names
            .iter()
            .enumerate()
            .all(|(i, n)| n.as_deref() == Some(format!("s{i}").as_str()))
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_014_missing_required_value() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .env()
        .model::<ModelB>()
        .unwrap()
        .create(&mut tx, vec![Vals::new()])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(err.sqlstate(), Some("23502"));
    assert_eq!(
        err.message(),
        "The database refused the change: Missing required field 'Name' (name) for model 'Model B' (test_rpc.model_b)"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_021_write_on_empty_recordset() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let empty = tx.env().records("test_orm.partner").unwrap();
    assert_query_count!(tx, <= 0, {
        empty.write_values(&mut tx, &map(&[("name", Value::str("x"))])).await.unwrap();
        empty.write_values(&mut tx, &map(&[("nope", Value::Int(1))])).await.unwrap();
        empty.write(&mut tx, RawVals::new().set_cell("nope", erp_sdk::Cell::Int(1))).await.unwrap();
    });
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_027_duplicate_ids_in_written_recordset() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().model::<Partner>().unwrap();
    let p1 = partners
        .create(&mut tx, vec![partners.vals().set(Partner::NAME, "Initial")])
        .await
        .unwrap();
    let dup = p1.concat(&p1).unwrap();
    assert_eq!(dup.len(), 2);
    let p1_id = p1.ids()[0];
    assert_eq!(dup.ids(), [p1_id, p1_id]);
    dup.write(&mut tx, partners.vals().set(Partner::NAME, "X"))
        .await
        .unwrap();
    assert_eq!(
        p1.get(&mut tx, Partner::NAME).await.unwrap().as_deref(),
        Some("X")
    );
    tx.invalidate_all().await.unwrap();
    assert_eq!(
        p1.get(&mut tx, Partner::NAME).await.unwrap().as_deref(),
        Some("X")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_040_fields_get() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .policy(StaticPolicy::SuperuserOnly)
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_orm.partner").unwrap();
    let all = partners.fields_get(&mut tx, None, None).await.unwrap();
    assert!(all.contains_key("name") && all.contains_key("display_name") && all.contains_key("id"));
    assert_eq!(all["name"]["readonly"], Value::Bool(false));
    let one = partners
        .fields_get(&mut tx, Some(&["name"]), Some(&["string", "type"]))
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(
        one["name"],
        map(&[("string", Value::str("Name")), ("type", Value::str("char"))])
    );
    // a user who may not write gets readonly descriptions
    let user_view = partners
        .with_user(7)
        .fields_get(&mut tx, Some(&["name"]), Some(&["readonly"]))
        .await
        .unwrap();
    assert_eq!(user_view["name"]["readonly"], Value::Bool(true));
    drop(tx);
    h.cleanup().await.unwrap();
}

static CAT: AddonDescriptor = AddonDescriptor {
    name: "test_cat",
    depends: &["base"],
    exact_only: false,
};

fn category(definition: &'static str) -> ModelDecl {
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl {
        name: "_positive_color",
        kind: TableObjectKind::Constraint {
            definition,
            message: "The color code must be positive!",
        },
    }]));
    ModelDecl::new("test_cat", "x.category")
        .fields(const { &[FieldDecl::integer("color")] })
        .table_objects(objects)
}

#[tokio::test]
async fn spec_crud_041_sql_constraint_creation_and_idempotence() {
    let h = Harness::builder([&CAT])
        .with_decls([category("CHECK(color >= 0)")])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let comment = "SELECT obj_description(oid, 'pg_constraint') FROM pg_constraint \
                   WHERE conname = 'x_category_positive_color'";
    assert_eq!(
        one(&mut tx, comment).await.as_deref(),
        Some("CHECK(color >= 0)")
    );
    tx.capture_statements(true);
    tx.init_models(&[]).await.unwrap();
    assert!(
        !tx.captured().join("").contains("positive_color"),
        "{:?}",
        tx.captured()
    );
    tx.commit().await.unwrap();

    let changed: &'static ModelDecl = Box::leak(Box::new(category("CHECK(color >= 1)")));
    let registry = std::sync::Arc::new(
        RegistryBuilder::new([&CAT])
            .with_decls([changed])
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
    assert!(
        ddl.contains("DROP CONSTRAINT IF EXISTS \"x_category_positive_color\""),
        "{ddl}"
    );
    assert!(
        ddl.contains("ADD CONSTRAINT \"x_category_positive_color\" CHECK(color >= 1)"),
        "{ddl}"
    );
    assert_eq!(
        one(&mut tx, comment).await.as_deref(),
        Some("CHECK(color >= 1)")
    );
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_042_constraint_violation_message() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let err = tx
        .env()
        .model::<ModelB>()
        .unwrap()
        .create(
            &mut tx,
            vec![Vals::new().set(ModelB::NAME, "B1").set(ModelB::VALUE, -1)],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The database refused the change: The value must be positive"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    h.cleanup().await.unwrap();
}

const PERF_LINE_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_perf_line",
    depends: &["base"],
    exact_only: false,
};

fn perf_line_decl(has_manual: bool) -> ModelDecl {
    let mut objects: Vec<TableObjectDecl> = vec![
        TableObjectDecl {
            name: "_line_uniq",
            kind: TableObjectKind::UniqueIndex {
                definition: "(base_id, value)",
                message: "base_id and value should be unique",
            },
        },
        TableObjectDecl {
            name: "_empty_idx",
            kind: TableObjectKind::Index { definition: "" },
        },
    ];
    if has_manual {
        objects.push(TableObjectDecl {
            name: "_manual_idx",
            kind: TableObjectKind::Index {
                definition: "(base_id)",
            },
        });
    }
    let objects: &'static [TableObjectDecl] = Box::leak(objects.into_boxed_slice());
    ModelDecl::new("test_perf_line", "x.perf_line")
        .fields(const { &[FieldDecl::integer("base_id"), FieldDecl::integer("value")] })
        .table_objects(objects)
}

#[tokio::test]
async fn spec_crud_046_index_and_unique_index_table_objects() {
    let h = Harness::builder([&PERF_LINE_ADDON])
        .with_decls([perf_line_decl(false)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();

    // The UniqueIndex was created with COMMENT ON INDEX matching definition
    let comment_query = "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c \
                         WHERE c.relname = 'x_perf_line_line_uniq' AND c.relkind = 'i'";
    assert_eq!(
        one(&mut tx, comment_query).await.as_deref(),
        Some("UNIQUE INDEX (base_id, value)")
    );

    // Uniqueness and column list on pg_index
    let is_unique = "SELECT i.indisunique::text FROM pg_index i \
                     JOIN pg_class c ON c.oid = i.indexrelid \
                     WHERE c.relname = 'x_perf_line_line_uniq'";
    assert_eq!(one(&mut tx, is_unique).await.as_deref(), Some("true"));

    let idx_def = "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i \
                   JOIN pg_class c ON c.oid = i.indexrelid \
                   WHERE c.relname = 'x_perf_line_line_uniq'";
    let def_str = one(&mut tx, idx_def).await.expect("index definition");
    assert!(def_str.contains("UNIQUE INDEX"), "{def_str}");
    assert!(def_str.contains("(base_id, value)"), "{def_str}");

    let empty_idx_exists = "SELECT count(*)::text FROM pg_class \
                            WHERE relname = 'x_perf_line_empty_idx' AND relkind = 'i'";
    assert_eq!(one(&mut tx, empty_idx_exists).await.as_deref(), Some("0"));

    // DBA-created manual index without comment is left untouched
    tx.batch("CREATE INDEX x_perf_line_manual_idx ON x_perf_line (base_id)")
        .await
        .unwrap();
    let manual_comment = "SELECT obj_description(c.oid, 'pg_class') FROM pg_class c \
                          WHERE c.relname = 'x_perf_line_manual_idx' AND c.relkind = 'i'";
    assert_eq!(one(&mut tx, manual_comment).await, None);
    tx.commit().await.unwrap();

    // Upgrade adding declared _manual_idx
    let upgraded_decl: &'static ModelDecl = Box::leak(Box::new(perf_line_decl(true)));
    let registry = Arc::new(
        RegistryBuilder::new([&PERF_LINE_ADDON])
            .with_decls([upgraded_decl])
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
    assert!(
        !ddl.is_empty(),
        "expected statements to be executed during init_models"
    );
    assert!(
        !ddl.contains("x_perf_line_manual_idx"),
        "manual index without comment must be left untouched: {ddl}"
    );

    // Re-verify that manual index still exists and comment is still NULL after upgrade
    let manual_exists = "SELECT count(*)::text FROM pg_class \
                         WHERE relname = 'x_perf_line_manual_idx' AND relkind = 'i'";
    assert_eq!(one(&mut tx, manual_exists).await.as_deref(), Some("1"));
    assert_eq!(one(&mut tx, manual_comment).await, None);

    // Later unique violation reports the declared message
    let recs = tx.env().records("x.perf_line").unwrap();
    recs.create(
        &mut tx,
        vec![
            RawVals::new()
                .set_cell("base_id", erp_sdk::Cell::Int(1))
                .set_cell("value", erp_sdk::Cell::Int(10)),
        ],
    )
    .await
    .unwrap();
    let err = recs
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("base_id", erp_sdk::Cell::Int(1))
                    .set_cell("value", erp_sdk::Cell::Int(10)),
            ],
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "The database refused the change: base_id and value should be unique"
    );
    tx.rollback().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_049_table_creation() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let cols = rows(
        &mut tx,
        "SELECT column_name::text FROM information_schema.columns \
         WHERE table_name = 'test_orm_schema_table' ORDER BY 1",
    )
    .await;
    let cols: Vec<_> = cols.into_iter().map(|r| r[0].clone().unwrap()).collect();
    assert_eq!(
        cols,
        [
            "boolean",
            "char",
            "create_date",
            "create_uid",
            "float",
            "id",
            "integer",
            "write_date",
            "write_uid"
        ]
    );
    let pk = rows(
        &mut tx,
        "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attidentity::text \
         FROM pg_index i JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) \
         WHERE i.indrelid = 'test_orm_schema_table'::regclass AND i.indisprimary",
    )
    .await;
    assert_eq!(
        pk,
        vec![vec![
            Some("id".into()),
            Some("bigint".into()),
            Some("d".into())
        ]]
    );
    let fks = rows(
        &mut tx,
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
         WHERE conrelid = 'test_orm_schema_table'::regclass AND contype = 'f' ORDER BY 1",
    )
    .await;
    assert_eq!(
        fks,
        vec![
            vec![Some(
                "FOREIGN KEY (create_uid) REFERENCES res_users(id) ON DELETE SET NULL".into()
            )],
            vec![Some(
                "FOREIGN KEY (write_uid) REFERENCES res_users(id) ON DELETE SET NULL".into()
            )],
        ]
    );
    assert_eq!(
        one(
            &mut tx,
            "SELECT obj_description('test_orm_schema_table'::regclass, 'pg_class')"
        )
        .await
        .as_deref(),
        Some("Test ORM Schema Table")
    );
    assert_eq!(
        one(
            &mut tx,
            "SELECT is_nullable::text FROM information_schema.columns \
                      WHERE table_name = 'test_orm_schema' AND column_name = 'required'"
        )
        .await
        .as_deref(),
        Some("NO")
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_050_field_index_kinds() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .without_schema()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    // pg_trgm is a trusted extension: the database owner may create it.
    tx.batch("CREATE EXTENSION IF NOT EXISTS pg_trgm")
        .await
        .unwrap();
    tx.init_models(&[]).await.unwrap();
    let defs = rows(
        &mut tx,
        "SELECT indexname::text, indexdef::text FROM pg_indexes \
         WHERE tablename IN ('test_orm_schema', 'test_orm_schema_index') ORDER BY 1",
    )
    .await;
    let def = |name: &str| {
        defs.iter()
            .find(|r| r[0].as_deref() == Some(name))
            .and_then(|r| r[1].clone())
            .unwrap_or_else(|| panic!("no index {name}: {defs:?}"))
    };
    assert!(def("test_orm_schema__index_btree_index").contains("USING btree (index_btree)"));
    assert!(
        def("test_orm_schema__index_btree_not_null_index").contains(
            "USING btree (index_btree_not_null) WHERE (index_btree_not_null IS NOT NULL)"
        )
    );
    let trgm = def("test_orm_schema__index_trigram_index");
    assert!(
        trgm.contains("USING gin") && trgm.contains("gin_trgm_ops"),
        "{trgm}"
    );
    assert!(def("test_orm_schema_index__btree_index").contains("USING btree (btree)"));
    let long: Vec<_> = defs
        .iter()
        .filter_map(|r| r[0].clone())
        .filter(|n| n.starts_with("test_orm_schema__very_very"))
        .collect();
    assert_eq!(long.len(), 2);
    assert_ne!(long[0], long[1]);
    assert!(long.iter().all(|n| n.len() == 63), "{long:?}");
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn schema_sync_without_pg_trgm_fails_loudly_on_trigram_field() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .without_schema()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    // Drop pg_trgm. Schema sync must fail loudly naming the extension and field, in
    // the words (`ormx_index.rs` covers that case with its own fixture).
    tx.batch("DROP EXTENSION IF EXISTS pg_trgm CASCADE")
        .await
        .unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Database);
    let spec = h.registry().model("test_orm.schema").unwrap();
    let field = spec.field("index_trigram").unwrap();
    assert_eq!(
        err.message(),
        format!(
            "field '{}.{}' declares index = \"trigram\", which needs the PostgreSQL extension \
             pg_trgm; it is not installed",
            spec.name, field.name
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

const FK_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_fk_sync",
    depends: &["base"],
    exact_only: false,
};

fn fk_models(ondelete: OnDelete) -> [ModelDecl; 2] {
    let fields: &'static [FieldDecl] = Box::leak(Box::new([
        FieldDecl::char("name"),
        FieldDecl::many2one("m2o", "x.fk_target").ondelete(ondelete),
    ]));
    [
        ModelDecl::new("test_fk_sync", "x.fk_target").fields(const { &[FieldDecl::char("name")] }),
        ModelDecl::new("test_fk_sync", "x.fk_source").fields(fields),
    ]
}

#[tokio::test]
async fn spec_crud_052_foreign_key_creation_and_update() {
    let h = Harness::builder([&FK_ADDON])
        .with_decls(fk_models(OnDelete::SetNull))
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();

    let fk_props = "SELECT confdeltype::text, confrelid::regclass::text FROM pg_constraint \
         WHERE conname = 'x_fk_source_m2o_fkey'";
    let fk_row = rows(&mut tx, fk_props).await;
    assert_eq!(
        fk_row,
        vec![vec![Some("n".into()), Some("x_fk_target".into())]] // 'n' = SET NULL
    );
    tx.commit().await.unwrap();

    // Upgrade changing ondelete to cascade
    let upgraded_decls: &'static [ModelDecl] = Box::leak(Box::new(fk_models(OnDelete::Cascade)));
    let registry = Arc::new(
        RegistryBuilder::new([&FK_ADDON])
            .with_decls(upgraded_decls.iter())
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
    assert!(
        ddl.contains("DROP CONSTRAINT IF EXISTS \"x_fk_source_m2o_fkey\""),
        "{ddl}"
    );
    assert!(ddl.contains("ON DELETE CASCADE"), "{ddl}");

    // Assert exactly one FK on x_fk_source.m2o exists after upgrade (replaced, not duplicated)
    let fk_count = "SELECT count(*)::text FROM pg_constraint c \
                    JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY(c.conkey) \
                    WHERE c.conrelid = 'x_fk_source'::regclass AND a.attname = 'm2o' AND c.contype = 'f'";
    assert_eq!(one(&mut tx, fk_count).await.as_deref(), Some("1"));

    let fk_row_after = rows(&mut tx, fk_props).await;
    assert_eq!(
        fk_row_after,
        vec![vec![Some("c".into()), Some("x_fk_target".into())]] // 'c' = CASCADE
    );
    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

const REMOVE_FIELD_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_remove_field",
    depends: &["base"],
    exact_only: false,
};

fn remove_field_model(has_old: bool) -> ModelDecl {
    let fields: &'static [FieldDecl] = if has_old {
        Box::leak(Box::new([
            FieldDecl::char("name"),
            FieldDecl::char("old").required(true),
        ]))
    } else {
        Box::leak(Box::new([FieldDecl::char("name")]))
    };
    ModelDecl::new("test_remove_field", "x.keep_col").fields(fields)
}

#[tokio::test]
async fn spec_crud_053_removed_fields_keep_their_column() {
    let h = Harness::builder([&REMOVE_FIELD_ADDON])
        .with_decls([remove_field_model(true)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();

    let nullable_query = "SELECT is_nullable::text FROM information_schema.columns \
                          WHERE table_schema = current_schema() \
                            AND table_name = 'x_keep_col' AND column_name = 'old'";
    assert_eq!(one(&mut tx, nullable_query).await.as_deref(), Some("NO"));

    // Insert with old works
    let recs = tx.env().records("x.keep_col").unwrap();
    let row = recs
        .create(
            &mut tx,
            vec![
                RawVals::new()
                    .set_cell("name", erp_sdk::Cell::Str("A".into()))
                    .set_cell("old", erp_sdk::Cell::Str("V1".into())),
            ],
        )
        .await
        .unwrap();
    assert_eq!(row.len(), 1);
    tx.commit().await.unwrap();

    // Upgrade removing the field `old` from code
    let upgraded_decl: &'static ModelDecl = Box::leak(Box::new(remove_field_model(false)));
    let registry = Arc::new(
        RegistryBuilder::new([&REMOVE_FIELD_ADDON])
            .with_decls([upgraded_decl])
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
    assert!(ddl.contains("ALTER COLUMN \"old\" DROP NOT NULL"), "{ddl}");
    // Column still exists but is nullable
    assert_eq!(one(&mut tx, nullable_query).await.as_deref(), Some("YES"));

    // Existing row A preserves its old value after upgrade
    let row_a_val = "SELECT old::text FROM x_keep_col WHERE name = 'A'";
    assert_eq!(one(&mut tx, row_a_val).await.as_deref(), Some("V1"));

    // Inserts without `old` succeed
    let recs2 = tx.env().records("x.keep_col").unwrap();
    let row2 = recs2
        .create(
            &mut tx,
            vec![RawVals::new().set_cell("name", erp_sdk::Cell::Str("B".into()))],
        )
        .await
        .unwrap();
    assert_eq!(row2.len(), 1);

    // Old column in the new row is NULL in the database (verified via count to distinguish NULL from missing row)
    let count_null = "SELECT count(*)::text FROM x_keep_col WHERE name = 'B' AND old IS NULL";
    assert_eq!(one(&mut tx, count_null).await.as_deref(), Some("1"));

    tx.commit().await.unwrap();
    drop(tx);
    drop(orm);
    h.cleanup().await.unwrap();
}

// ----- defaults (CRUD §4.2 step 3, §4.5) ------------------------------------------------------

fn s(text: &str) -> Value {
    Value::str(text)
}

/// A line's values as a dynamic map.
fn line(pairs: &[(&str, Value)]) -> Value {
    Value::Map(Arc::new(vmap(pairs)))
}

/// `[6, 0, ids]`.
fn cmd_set(ids: &[i64]) -> Value {
    Value::list([
        Value::Int(6),
        Value::Int(0),
        Value::list(ids.iter().map(|i| Value::Int(*i))),
    ])
}

/// `[0, 0, values]`.
fn cmd_create(pairs: &[(&str, Value)]) -> Value {
    Value::list([Value::Int(0), Value::Int(0), line(pairs)])
}

/// A command triple `[code, id, values-or-0]`.
fn cmd(code: i64, id: i64, values: Value) -> Value {
    Value::list([Value::Int(code), Value::Int(id), values])
}

/// A regular user (not superuser), created in the current transaction.
async fn demo_user(tx: &mut Transaction) -> i64 {
    let user = tx
        .env()
        .records("res.users")
        .unwrap()
        .create_values(tx, &[map(&[("name", s("Demo")), ("login", s("demo"))])])
        .await
        .unwrap();
    user.ids()[0]
}

async fn names_of(tx: &mut Transaction, records: &Records) -> Vec<Value> {
    records.values(tx, "name").await.unwrap()
}

static DEFAULTS: AddonDescriptor = AddonDescriptor {
    name: "test_crud_defaults",
    depends: &["base"],
    exact_only: false,
};

static SEQ: AtomicI64 = AtomicI64::new(0);

/// A function default with a visible side effect: one call per new record.
fn next_seq(_ctx: &DefaultContext) -> Value {
    Value::Int(SEQ.fetch_add(1, Ordering::SeqCst) + 1)
}

fn default_lines(_ctx: &DefaultContext) -> Value {
    Value::list([line(&[("name", s("from_fn"))])])
}

fn current_user(ctx: &DefaultContext) -> Value {
    Value::Int(ctx.uid)
}

/// `x.def_doc` with a static, a function, a many2one and a one2many default, and a many2many
/// without one; `x.def_tag` its lines.
fn defaults_decls() -> [ModelDecl; 2] {
    [
        ModelDecl::new("test_crud_defaults", "x.def_tag").fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("doc_id", "x.def_doc"),
                ]
            },
        ),
        ModelDecl::new("test_crud_defaults", "x.def_doc").fields(
            const {
                &[
                    FieldDecl::char("name").default(StaticValue::Str("default_name")),
                    FieldDecl::integer("seq").default_fn(next_seq),
                    FieldDecl::integer("count"),
                    FieldDecl::many2one("owner_id", "res.users").default_fn(current_user),
                    FieldDecl::one2many("tag_ids", "x.def_tag", "doc_id").default_fn(default_lines),
                    FieldDecl::many2many("label_ids", "x.def_tag"),
                ]
            },
        ),
    ]
}

async fn defaults_harness(policy: Option<NoTagWrites>) -> Harness {
    let builder = Harness::builder([&base_users::ADDON, &DEFAULTS]).with_decls(defaults_decls());
    match policy {
        Some(p) => builder.rules(p),
        None => builder,
    }
    .build()
    .await
    .unwrap()
}

#[tokio::test]
async fn spec_crud_005_default_get_precedence() {
    let h = Harness::builder([&base_users::ADDON, &test_orm::ADDON, &test_web::ADDON])
        .with_static_decls(crate::currency_fixture::decls())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let uid = demo_user(&mut tx).await;
    // a function default of the environment; names without any default are absent
    let messages = tx.env().records("test_web.message").unwrap().with_user(uid);
    assert_eq!(
        messages
            .default_get(&mut tx, &["discussion", "body", "author", "size"])
            .await
            .unwrap(),
        vmap(&[("author", Value::Int(uid))])
    );
    // a static default, through the typed surface; unknown names are skipped
    let mixed = tx.env().model::<Mixed>().unwrap();
    let names = [Mixed::FLOAT_DEFAULT.name(), Mixed::CHAR.name(), "nope"];
    assert_eq!(
        mixed.default_get(&mut tx, &names).await.unwrap(),
        vmap(&[("float_default", Value::Float(1.25))])
    );
    // the context key wins, converted to the field's type
    let with_ctx = mixed.with_context([("default_float_default", Value::Int(1))]);
    assert_eq!(
        with_ctx.default_get(&mut tx, &names).await.unwrap(),
        vmap(&[("float_default", Value::Float(1.0))])
    );
    // a context value of the wrong shape is an error naming the field
    let err = mixed
        .with_context([("default_float_default", s("abc"))])
        .default_get(&mut tx, &names)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(err.message().contains("float_default"), "{}", err.message());
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn spec_crud_006_context_default_for_a_non_field_name() {
    let h = support::orm().await;
    let mut tx = h.begin().await.unwrap();
    let partners = tx.env().records("test_orm.partner").unwrap();
    let with_ctx = partners.with_context([("default_not_a_field", Value::Int(5))]);
    assert_eq!(
        with_ctx
            .default_get(&mut tx, &["not_a_field"])
            .await
            .unwrap(),
        vmap(&[("not_a_field", Value::Int(5))])
    );
    // returned as given: there is no field to convert it to
    let raw = Value::list([s("x"), Value::False]);
    assert_eq!(
        partners
            .with_context([("default_not_a_field", raw.clone())])
            .default_get(&mut tx, &["not_a_field"])
            .await
            .unwrap(),
        vmap(&[("not_a_field", raw)])
    );
    // only requested names are answered
    assert_eq!(
        with_ctx.default_get(&mut tx, &["name"]).await.unwrap(),
        vmap(&[])
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

static DELEGATED: AddonDescriptor = AddonDescriptor {
    name: "test_crud_delegated",
    depends: &["base"],
    exact_only: false,
};

fn unit_lines(_ctx: &DefaultContext) -> Value {
    Value::list([line(&[("name", s("l1"))])])
}

fn delegated_decls() -> [ModelDecl; 3] {
    [
        ModelDecl::new("test_crud_delegated", "x.dg_unit").fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::char("kind").default(StaticValue::Str("unit-kind")),
                    FieldDecl::char("ro_with_default")
                        .groups(".")
                        .default(StaticValue::Str("roro")),
                    FieldDecl::char("secret")
                        .groups("base.group_system")
                        .default(StaticValue::Str("s")),
                    FieldDecl::one2many("line_ids", "x.dg_line", "unit_id").default_fn(unit_lines),
                ]
            },
        ),
        ModelDecl::new("test_crud_delegated", "x.dg_line").fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("unit_id", "x.dg_unit").ondelete(OnDelete::Cascade),
                ]
            },
        ),
        ModelDecl::new("test_crud_delegated", "x.dg_box")
            .inherits(&[("x.dg_unit", "unit_id")])
            .fields(
                const {
                    &[
                        FieldDecl::many2one("unit_id", "x.dg_unit")
                            .required(true)
                            .ondelete(OnDelete::Cascade),
                        FieldDecl::integer("val"),
                        // reads the parent's `kind`: shows onchange values reaching the parent
                        FieldDecl::char("kind_echo").related("unit_id.kind"),
                    ]
                },
            ),
    ]
}

#[tokio::test]
async fn spec_crud_007_defaults_of_delegated_fields() {
    let h = Harness::builder([&base_users::ADDON, &DELEGATED])
        .with_decls(delegated_decls())
        .policy(erp_testkit::FieldPolicy::new().enforce_groups())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let uid = demo_user(&mut tx).await;
    let units = tx.env().records("x.dg_unit").unwrap().with_user(uid);
    let boxes = tx.env().records("x.dg_box").unwrap().with_user(uid);
    let lines = Value::list([cmd_set(&[]), cmd_create(&[("name", s("l1"))])]);
    // the parent model answers with its own defaults, x2many lines normalized
    assert_eq!(
        units
            .default_get(&mut tx, &["ro_with_default", "kind", "line_ids"])
            .await
            .unwrap(),
        vmap(&[
            ("ro_with_default", s("roro")),
            ("kind", s("unit-kind")),
            ("line_ids", lines.clone()),
        ])
    );
    // the child defers to the parent, except for the field the user may not write
    assert_eq!(
        boxes
            .default_get(&mut tx, &["ro_with_default", "kind", "line_ids", "val"])
            .await
            .unwrap(),
        vmap(&[("kind", s("unit-kind")), ("line_ids", lines)])
    );
    assert_eq!(
        boxes
            .sudo(true)
            .default_get(&mut tx, &["ro_with_default"])
            .await
            .unwrap(),
        vmap(&[("ro_with_default", s("roro"))])
    );
    // the child's context key comes first
    assert_eq!(
        boxes
            .with_context([("default_kind", s("ctx"))])
            .default_get(&mut tx, &["kind"])
            .await
            .unwrap(),
        vmap(&[("kind", s("ctx"))])
    );
    // a field restricted to a group the user lacks is not delegated either
    assert_eq!(
        boxes.default_get(&mut tx, &["secret"]).await.unwrap(),
        vmap(&[])
    );
    assert_eq!(
        units.default_get(&mut tx, &["secret"]).await.unwrap(),
        vmap(&[("secret", s("s"))])
    );
    // creating a box creates its unit through the unit's own `create`: the unit's defaults
    // apply, the inaccessible field and the default x2many lines included
    let created = boxes
        .create_values(&mut tx, &[map(&[("val", Value::Int(1))])])
        .await
        .unwrap();
    let unit = created
        .sudo(true)
        .related(&mut tx, "unit_id")
        .await
        .unwrap();
    assert_eq!(unit.len(), 1);
    assert_eq!(
        unit.value(&mut tx, "ro_with_default").await.unwrap(),
        s("roro")
    );
    assert_eq!(unit.value(&mut tx, "kind").await.unwrap(), s("unit-kind"));
    let unit_lines = unit.related(&mut tx, "line_ids").await.unwrap();
    assert_eq!(names_of(&mut tx, &unit_lines).await, [s("l1")]);
    // with the delegate given, the given unit gets no defaults: no second line
    let given = units
        .create_values(&mut tx, &[map(&[("name", s("u"))])])
        .await
        .unwrap();
    assert_eq!(given.related(&mut tx, "line_ids").await.unwrap().len(), 1);
    boxes
        .create_values(
            &mut tx,
            &[map(&[
                ("val", Value::Int(2)),
                ("unit_id", Value::Int(given.ids()[0])),
            ])],
        )
        .await
        .unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(given.related(&mut tx, "line_ids").await.unwrap().len(), 1);
    let with_ctx = boxes.with_context([(
        "default_line_ids",
        Value::list([line(&[("name", s("from_ctx"))])]),
    )]);
    let created = with_ctx
        .create_values(&mut tx, &[map(&[("val", Value::Int(3))])])
        .await
        .unwrap();
    let unit = created
        .sudo(true)
        .related(&mut tx, "unit_id")
        .await
        .unwrap();
    let unit_lines = unit.related(&mut tx, "line_ids").await.unwrap();
    assert_eq!(names_of(&mut tx, &unit_lines).await, [s("from_ctx")]);
    drop(tx);
    h.cleanup().await.unwrap();
}

fn spec_of(pairs: &[(&str, Value)]) -> erp_orm::Spec {
    erp_orm::Spec::from_value(&line(pairs)).unwrap()
}

/// The `value` part of an onchange result.
fn value_of(result: &Value) -> erp_sdk::ValueMap {
    let value = match result {
        Value::Map(m) => match m.get("value") {
            Some(Value::Map(v)) => Some(v.as_ref().clone()),
            _ => None,
        },
        _ => None,
    };
    value.expect("an onchange result with a value map")
}

#[tokio::test]
async fn spec_crud_007_first_onchange_returns_delegated_defaults() {
    let h = Harness::builder([&base_users::ADDON, &DELEGATED])
        .with_decls(delegated_decls())
        .policy(erp_testkit::FieldPolicy::new().enforce_groups())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let uid = demo_user(&mut tx).await;
    let boxes = tx.env().records("x.dg_box").unwrap().with_user(uid);
    let empty = || Value::Map(Arc::new(vmap(&[])));
    let spec = spec_of(&[
        ("kind", empty()),
        ("kind_echo", empty()),
        ("val", empty()),
        ("line_ids", line(&[("fields", line(&[("name", empty())]))])),
    ]);
    let result = boxes
        .onchange(&mut tx, erp_sdk::ValueMap::new(), Vec::new(), spec.clone())
        .await
        .unwrap();
    let value = value_of(&result);
    assert_eq!(value["kind"], s("unit-kind"), "{value:?}");
    // the default reached the in-memory parent, which the related field reads
    assert_eq!(value["kind_echo"], s("unit-kind"), "{value:?}");
    assert_eq!(value["val"], Value::Int(0), "{value:?}");
    // the field the user may not write gets no delegated default: it is absent from the result,
    // and a form that names it is refused as a read of it would be (clients strip such fields
    // from the view)
    assert!(!value.contains_key("ro_with_default"), "{value:?}");
    let named = spec_of(&[("ro_with_default", empty())]);
    let error = boxes
        .onchange(&mut tx, erp_sdk::ValueMap::new(), Vec::new(), named)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Access);
    assert_eq!(
        error.to_string(),
        "You do not have enough rights to access the field \"ro_with_default\" on x.dg_box \
         (x.dg_box). Please contact your system administrator.\n\nOperation: read"
    );
    // the parent's default line, as a CREATE command with its values
    let Value::List(commands) = &value["line_ids"] else {
        panic!("{value:?}")
    };
    assert_eq!(commands.len(), 1, "{commands:?}");
    let Value::List(create) = &commands[0] else {
        panic!("{commands:?}")
    };
    assert_eq!(create[0], Value::Int(0), "{create:?}");
    assert_eq!(create[2], line(&[("name", s("l1"))]), "{create:?}");
    // a delegated field the client changes goes to the in-memory parent: the field
    // reading the parent's value follows
    let result = boxes
        .onchange(
            &mut tx,
            vmap(&[("kind", s("k")), ("kind_echo", s("unit-kind"))]),
            vec!["kind".to_owned()],
            spec,
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result), vmap(&[("kind_echo", s("k"))]));
    drop(tx);
    h.cleanup().await.unwrap();
}

type DefaultGetFn = MethodFn<(Vec<String>,), erp_sdk::ValueMap>;

/// An addon override of `default_get` on `x.def_doc`: `name` defaults to "overridden".
fn override_default_get<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (Vec<String>,),
    sup: Super<(Vec<String>,), erp_sdk::ValueMap>,
) -> BoxFuture<'a, erp_sdk::Result<erp_sdk::ValueMap>> {
    Box::pin(async move {
        let wants_name = args.0.iter().any(|n| n == "name");
        let mut out = sup.call(tx, records, args).await?;
        if wants_name {
            out.insert("name".to_owned(), s("overridden"));
        }
        Ok(out)
    })
}

fn override_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static F: DefaultGetFn = override_default_get;
    &F
}

static OVERRIDE: MethodImpl = MethodImpl {
    addon: "test_crud_defaults",
    model: "x.def_doc",
    method: "default_get",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<DefaultGetFn>,
    entry: override_entry,
    file: file!(),
    line: line!(),
};

/// CRUD §4.5: `default_get` is a model method with an override chain; `create` and the first
/// `onchange` go through it, so an addon override applies everywhere.
#[tokio::test]
async fn crud_default_get_override_applies_to_create_and_onchange() {
    let h = Harness::builder([&base_users::ADDON, &DEFAULTS])
        .with_decls(defaults_decls())
        .with_impls([&OVERRIDE])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let docs = tx.env().records("x.def_doc").unwrap();
    assert_eq!(
        docs.default_get(&mut tx, &["name"]).await.unwrap(),
        vmap(&[("name", s("overridden"))])
    );
    let rec = docs.create_values(&mut tx, &[map(&[])]).await.unwrap();
    assert_eq!(rec.value(&mut tx, "name").await.unwrap(), s("overridden"));
    // Batch create of ≥2 records: override must be honoured on every record
    let batch = docs
        .create_values(&mut tx, &[map(&[]), map(&[]), map(&[])])
        .await
        .unwrap();
    assert_eq!(batch.len(), 3);
    for r in batch.iter() {
        assert_eq!(r.value(&mut tx, "name").await.unwrap(), s("overridden"));
    }
    // Mixed rows with override: given in some rows, defaulted in others
    let mixed = docs
        .create_values(
            &mut tx,
            &[
                map(&[("name", s("row0_given"))]),
                map(&[]),
                map(&[("name", s("row2_given"))]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        mixed.at(0).unwrap().value(&mut tx, "name").await.unwrap(),
        s("row0_given")
    );
    assert_eq!(
        mixed.at(1).unwrap().value(&mut tx, "name").await.unwrap(),
        s("overridden")
    );
    assert_eq!(
        mixed.at(2).unwrap().value(&mut tx, "name").await.unwrap(),
        s("row2_given")
    );
    // given values still win over the override's defaults
    let given = docs
        .create_values(&mut tx, &[map(&[("name", s("given"))])])
        .await
        .unwrap();
    assert_eq!(given.value(&mut tx, "name").await.unwrap(), s("given"));
    let result = docs
        .onchange(
            &mut tx,
            erp_sdk::ValueMap::new(),
            Vec::new(),
            spec_of(&[("name", Value::Map(Arc::new(vmap(&[]))))]),
        )
        .await
        .unwrap();
    assert_eq!(value_of(&result)["name"], s("overridden"));
    drop(tx);
    h.cleanup().await.unwrap();
}

/// defaults, defaults are not applied by `write`, wrong-shaped defaults are errors, and the
/// `default_*` stripping of `sudo()` agrees with `reference_crud_008`)
#[tokio::test]
async fn spec_crud_008_given_values_beat_defaults_and_x2many_normalization() {
    let h = defaults_harness(None).await;
    let mut tx = h.begin().await.unwrap();
    let uid = demo_user(&mut tx).await;
    let docs = tx.env().records("x.def_doc").unwrap();
    let tags = tx
        .env()
        .records("x.def_tag")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[("name", s("t1"))]), map(&[("name", s("t2"))])],
        )
        .await
        .unwrap();
    let (t1, t2) = (tags.ids()[0], tags.ids()[1]);

    // 1. static, function, many2one and one2many defaults
    let rec = docs
        .with_user(uid)
        .create_values(&mut tx, &[map(&[])])
        .await
        .unwrap();
    assert_eq!(rec.value(&mut tx, "name").await.unwrap(), s("default_name"));
    assert_eq!(
        rec.value(&mut tx, "owner_id").await.unwrap(),
        Value::Int(uid)
    );
    let lines = rec.related(&mut tx, "tag_ids").await.unwrap();
    assert_eq!(names_of(&mut tx, &lines).await, [s("from_fn")]);
    assert!(rec.related(&mut tx, "label_ids").await.unwrap().is_empty());

    // 2. several records in one call: function defaults run once per record, given values skip
    // them
    let three = docs
        .create_values(
            &mut tx,
            &[map(&[]), map(&[]), map(&[("seq", Value::Int(100))])],
        )
        .await
        .unwrap();
    let seqs = three.values(&mut tx, "seq").await.unwrap();
    let Value::Int(first) = seqs[0] else {
        panic!("{seqs:?}")
    };
    assert_eq!(
        seqs,
        [Value::Int(first), Value::Int(first + 1), Value::Int(100)]
    );
    let mut line_ids = Vec::new();
    for record in three.iter() {
        let lines = record.related(&mut tx, "tag_ids").await.unwrap();
        assert_eq!(names_of(&mut tx, &lines).await, [s("from_fn")]);
        line_ids.extend(lines.ids());
    }
    line_ids.sort_unstable();
    line_ids.dedup();
    assert_eq!(line_ids.len(), 3, "each record gets its own default line");

    // 3. context keys beat field defaults; a many2many list of ids becomes one SET, a one2many
    // list of dicts one CREATE per line
    let ctx_docs = docs.with_user(uid).with_context([
        ("default_name", s("ctx_name")),
        (
            "default_label_ids",
            Value::list([Value::Int(t1), Value::Int(t2)]),
        ),
        (
            "default_tag_ids",
            Value::list([line(&[("name", s("from_ctx"))])]),
        ),
    ]);
    assert_eq!(
        ctx_docs
            .default_get(&mut tx, &["name", "label_ids", "tag_ids"])
            .await
            .unwrap(),
        vmap(&[
            ("name", s("ctx_name")),
            ("label_ids", Value::list([cmd_set(&[t1, t2])])),
            (
                "tag_ids",
                Value::list([cmd_set(&[]), cmd_create(&[("name", s("from_ctx"))])])
            ),
        ])
    );
    let rec2 = ctx_docs.create_values(&mut tx, &[map(&[])]).await.unwrap();
    assert_eq!(rec2.value(&mut tx, "name").await.unwrap(), s("ctx_name"));
    let mut labels = rec2.related(&mut tx, "label_ids").await.unwrap().ids();
    labels.sort_unstable();
    assert_eq!(labels, [t1, t2]);
    let lines = rec2.related(&mut tx, "tag_ids").await.unwrap();
    assert_eq!(names_of(&mut tx, &lines).await, [s("from_ctx")]);
    // Batch create of ≥2 records with context defaults: applied to every record
    let ctx_batch = ctx_docs
        .create_values(&mut tx, &[map(&[]), map(&[])])
        .await
        .unwrap();
    assert_eq!(ctx_batch.len(), 2);
    for r in ctx_batch.iter() {
        assert_eq!(r.value(&mut tx, "name").await.unwrap(), s("ctx_name"));
        let mut r_labels = r.related(&mut tx, "label_ids").await.unwrap().ids();
        r_labels.sort_unstable();
        assert_eq!(r_labels, [t1, t2]);
        let r_lines = r.related(&mut tx, "tag_ids").await.unwrap();
        assert_eq!(names_of(&mut tx, &r_lines).await, [s("from_ctx")]);
    }

    // 4. given values beat defaults, field by field
    let rec4 = ctx_docs
        .create_values(
            &mut tx,
            &[map(&[
                ("name", s("given")),
                (
                    "label_ids",
                    Value::list([cmd(6, 0, Value::list([Value::Int(t1)]))]),
                ),
            ])],
        )
        .await
        .unwrap();
    assert_eq!(rec4.value(&mut tx, "name").await.unwrap(), s("given"));
    assert_eq!(
        rec4.related(&mut tx, "label_ids").await.unwrap().ids(),
        [t1]
    );
    let lines = rec4.related(&mut tx, "tag_ids").await.unwrap();
    assert_eq!(names_of(&mut tx, &lines).await, [s("from_ctx")]);

    let none_docs = docs.with_user(uid).with_context([
        ("default_tag_ids", Value::False),
        ("default_label_ids", Value::False),
    ]);
    assert_eq!(
        none_docs
            .default_get(&mut tx, &["tag_ids", "label_ids"])
            .await
            .unwrap(),
        vmap(&[
            ("tag_ids", Value::list([cmd_set(&[])])),
            ("label_ids", Value::list([cmd_set(&[])])),
        ])
    );
    tx.capture_statements(true);
    let rec5 = none_docs.create_values(&mut tx, &[map(&[])]).await.unwrap();
    let statements = tx.captured().to_vec();
    tx.capture_statements(false);
    assert!(
        statements.iter().all(|q| !q.contains("x_def_tag")),
        "{statements:?}"
    );
    assert!(rec5.related(&mut tx, "tag_ids").await.unwrap().is_empty());
    assert!(rec5.related(&mut tx, "label_ids").await.unwrap().is_empty());

    // 6. `write` applies no default, whatever the context says
    rec4.with_context([
        ("default_name", s("w")),
        ("default_tag_ids", Value::list([line(&[("name", s("w"))])])),
    ])
    .write_values(&mut tx, &map(&[("count", Value::Int(7))]))
    .await
    .unwrap();
    tx.flush().await.unwrap();
    tx.invalidate_all().await.unwrap();
    assert_eq!(rec4.value(&mut tx, "name").await.unwrap(), s("given"));
    assert_eq!(rec4.value(&mut tx, "count").await.unwrap(), Value::Int(7));
    let lines = rec4.related(&mut tx, "tag_ids").await.unwrap();
    assert_eq!(names_of(&mut tx, &lines).await, [s("from_ctx")]);

    // 7. `sudo()` strips `default_*` keys when it switches superuser mode on (ENV §4.2); keys
    // added in superuser mode stay, which is what `reference_crud_008` relies on
    let su_docs = ctx_docs.sudo(true);
    let rec7 = su_docs.create_values(&mut tx, &[map(&[])]).await.unwrap();
    assert_eq!(
        rec7.value(&mut tx, "name").await.unwrap(),
        s("default_name")
    );
    let lines = rec7.related(&mut tx, "tag_ids").await.unwrap();
    assert_eq!(names_of(&mut tx, &lines).await, [s("from_fn")]);
    let rec7b = su_docs
        .with_context([("default_name", s("after_sudo"))])
        .create_values(&mut tx, &[map(&[])])
        .await
        .unwrap();
    assert_eq!(rec7b.value(&mut tx, "name").await.unwrap(), s("after_sudo"));

    // 8. a default of the wrong shape is an error naming the field, for `create` and
    // `default_get` alike (no row is inserted)
    let before = one(&mut tx, "SELECT count(*)::text FROM x_def_doc").await;
    for (key, bad, field, message) in [
        (
            "default_count",
            s("abc"),
            "count",
            "Invalid value for x.def_doc.count: 'abc'",
        ),
        (
            "default_tag_ids",
            s("x"),
            "tag_ids",
            "Wrong value for x.def_doc.tag_ids: 'x'",
        ),
    ] {
        let bad_docs = docs.with_user(uid).with_context([(key, bad)]);
        let err = bad_docs
            .create_values(&mut tx, &[map(&[])])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{err:?}");
        assert_eq!(err.message(), message);
        let err = bad_docs.default_get(&mut tx, &[field]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation, "{err:?}");
        assert_eq!(err.message(), message);
    }
    assert_eq!(
        one(&mut tx, "SELECT count(*)::text FROM x_def_doc").await,
        before
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// Users may do anything except write or delete `x.def_tag` records.
struct NoTagWrites;

impl erp_testkit::TestRules for NoTagWrites {
    fn check_model(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        model: &ModelSpec,
        op: Operation,
    ) -> erp_sdk::Result<()> {
        if model.name == "x.def_tag" && matches!(op, Operation::Write | Operation::Unlink) {
            return Err(Error::access(format!(
                "Your access rights do not let you {} '{}' ({}) records.",
                op.as_str(),
                model.description,
                model.name
            )));
        }
        Ok(())
    }

    fn access_domain(
        &self,
        _b: &erp_orm::PrincipalBinding<'_>,
        _model: &ModelSpec,
        _op: Operation,
    ) -> erp_sdk::Result<Domain> {
        Ok(Domain::TRUE)
    }
}

#[tokio::test]
async fn crud_default_x2many_commands_are_access_checked() {
    let h = defaults_harness(Some(NoTagWrites)).await;
    let mut tx = h.begin().await.unwrap();
    assert!(tx.env().su());
    let uid = demo_user(&mut tx).await;
    let docs = tx.env().records("x.def_doc").unwrap();
    let tag = tx
        .env()
        .records("x.def_tag")
        .unwrap()
        .create_values(&mut tx, &[map(&[("name", s("t"))])])
        .await
        .unwrap();
    let t = tag.ids()[0];
    let user_docs = docs.with_user(uid);
    for command in [
        cmd(1, t, line(&[("name", s("renamed"))])),
        cmd(2, t, Value::Int(0)),
        cmd(3, t, Value::Int(0)),
        cmd(4, t, Value::Int(0)),
    ] {
        let with_ctx =
            user_docs.with_context([("default_tag_ids", Value::list([command.clone()]))]);
        let err = with_ctx
            .default_get(&mut tx, &["tag_ids"])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{command}: {err:?}");
        let err = with_ctx
            .create_values(&mut tx, &[map(&[])])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Access, "{command}: {err:?}");
    }
    // many2many: a DELETE deletes the record, so it needs unlink access; an UNLINK only removes
    // the relation row, so it is not checked
    let err = user_docs
        .with_context([("default_label_ids", Value::list([cmd(2, t, Value::Int(0))]))])
        .default_get(&mut tx, &["label_ids"])
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Access, "{err:?}");
    assert_eq!(
        user_docs
            .with_context([("default_label_ids", Value::list([cmd(3, t, Value::Int(0))]))])
            .default_get(&mut tx, &["label_ids"])
            .await
            .unwrap(),
        vmap(&[("label_ids", Value::list([cmd_set(&[])]))])
    );
    // a many2many link writes nothing on the comodel: no check
    assert_eq!(
        user_docs
            .with_context([("default_label_ids", Value::list([cmd(4, t, Value::Int(0))]))])
            .default_get(&mut tx, &["label_ids"])
            .await
            .unwrap(),
        vmap(&[("label_ids", Value::list([cmd_set(&[t])]))])
    );
    // superuser mode: no check; the UPDATE links the line without writing its values
    let su_update = docs.with_context([(
        "default_tag_ids",
        Value::list([cmd(1, t, line(&[("name", s("renamed"))]))]),
    )]);
    assert_eq!(
        su_update.default_get(&mut tx, &["tag_ids"]).await.unwrap(),
        vmap(&[("tag_ids", Value::list([cmd_set(&[t])]))])
    );
    let rec = su_update.create_values(&mut tx, &[map(&[])]).await.unwrap();
    assert_eq!(rec.related(&mut tx, "tag_ids").await.unwrap().ids(), [t]);
    tx.invalidate_all().await.unwrap();
    assert_eq!(tag.value(&mut tx, "name").await.unwrap(), s("t"));
    // a DELETE in a default deletes nothing and adds no line
    let rec = docs
        .with_context([("default_tag_ids", Value::list([cmd(2, t, Value::Int(0))]))])
        .create_values(&mut tx, &[map(&[])])
        .await
        .unwrap();
    assert!(rec.related(&mut tx, "tag_ids").await.unwrap().is_empty());
    assert_eq!(tag.exists(&mut tx).await.unwrap().ids(), [t]);
    drop(tx);
    h.cleanup().await.unwrap();
}

static HIERARCHY: AddonDescriptor = AddonDescriptor {
    name: "test_crud_hierarchy",
    depends: &["base"],
    exact_only: false,
};

fn hierarchy_decls() -> [ModelDecl; 3] {
    [
        ModelDecl::new("test_crud_hierarchy", "x.hier_a").fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::integer("seq").default_fn(next_seq),
                    FieldDecl::char("a_val").default(StaticValue::Str("a_default")),
                ]
            },
        ),
        ModelDecl::new("test_crud_hierarchy", "x.hier_b")
            .inherits(&[("x.hier_a", "a_id")])
            .fields(
                const {
                    &[
                        FieldDecl::many2one("a_id", "x.hier_a")
                            .required(true)
                            .ondelete(OnDelete::Cascade),
                        FieldDecl::char("b_val").default(StaticValue::Str("b_default")),
                    ]
                },
            ),
        ModelDecl::new("test_crud_hierarchy", "x.hier_c")
            .inherits(&[("x.hier_b", "b_id")])
            .fields(
                const {
                    &[
                        FieldDecl::many2one("b_id", "x.hier_b")
                            .required(true)
                            .ondelete(OnDelete::Cascade),
                        FieldDecl::char("c_val").default(StaticValue::Str("c_default")),
                    ]
                },
            ),
    ]
}

/// 3-level _inherits hierarchy (C inherits B inherits A) where A has a function
/// default: `all_defaults_static_in_hierarchy` follows `FieldSpec::inherited` up to root A,
/// detects the Fn default, and dispatches per record so function defaults run once per record.
#[tokio::test]
async fn crud_three_level_inherits_function_default_runs_per_record() {
    let h = Harness::builder([&base_users::ADDON, &HIERARCHY])
        .with_decls(hierarchy_decls())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let c = tx.env().records("x.hier_c").unwrap();

    // Batch create 2 records on C
    let batch = c
        .create_values(&mut tx, &[map(&[]), map(&[])])
        .await
        .unwrap();
    assert_eq!(batch.len(), 2);

    let seqs = batch.values(&mut tx, "seq").await.unwrap();
    let Value::Int(s0) = seqs[0] else {
        panic!("expected Value::Int for seqs[0], got {:?}", seqs[0]);
    };
    let Value::Int(s1) = seqs[1] else {
        panic!("expected Value::Int for seqs[1], got {:?}", seqs[1]);
    };
    assert_eq!(
        s1,
        s0 + 1,
        "function default on grandparent must run per record across batch"
    );

    // Static defaults on all levels are also properly resolved
    assert_eq!(
        batch.at(0).unwrap().value(&mut tx, "a_val").await.unwrap(),
        s("a_default")
    );
    assert_eq!(
        batch.at(1).unwrap().value(&mut tx, "a_val").await.unwrap(),
        s("a_default")
    );
    assert_eq!(
        batch.at(0).unwrap().value(&mut tx, "b_val").await.unwrap(),
        s("b_default")
    );
    assert_eq!(
        batch.at(1).unwrap().value(&mut tx, "b_val").await.unwrap(),
        s("b_default")
    );
    assert_eq!(
        batch.at(0).unwrap().value(&mut tx, "c_val").await.unwrap(),
        s("c_default")
    );
    assert_eq!(
        batch.at(1).unwrap().value(&mut tx, "c_val").await.unwrap(),
        s("c_default")
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

fn static_hierarchy_decls() -> [ModelDecl; 3] {
    [
        ModelDecl::new("test_crud_hierarchy", "x.static_hier_a").fields(
            const {
                &[
                    FieldDecl::char("a_val").default(StaticValue::Str("a_default")),
                    FieldDecl::integer("a_num").default(StaticValue::Int(100)),
                ]
            },
        ),
        ModelDecl::new("test_crud_hierarchy", "x.static_hier_b")
            .inherits(&[("x.static_hier_a", "a_id")])
            .fields(
                const {
                    &[
                        FieldDecl::many2one("a_id", "x.static_hier_a")
                            .required(true)
                            .ondelete(OnDelete::Cascade),
                        FieldDecl::char("b_val").default(StaticValue::Str("b_default")),
                    ]
                },
            ),
        ModelDecl::new("test_crud_hierarchy", "x.static_hier_c")
            .inherits(&[("x.static_hier_b", "b_id")])
            .fields(
                const {
                    &[
                        FieldDecl::many2one("b_id", "x.static_hier_b")
                            .required(true)
                            .ondelete(OnDelete::Cascade),
                        FieldDecl::char("c_val").default(StaticValue::Str("c_default")),
                    ]
                },
            ),
    ]
}

/// 3-level _inherits hierarchy where all defaults are static takes the fast path,
/// resolving defaults once per batch and applying static defaults across all 3 levels.
#[tokio::test]
async fn crud_three_level_inherits_all_static_defaults_fast_path() {
    let h = Harness::builder([&base_users::ADDON, &HIERARCHY])
        .with_decls(static_hierarchy_decls())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let c = tx.env().records("x.static_hier_c").unwrap();

    let rows = [map(&[]), map(&[("c_val", s("c_given"))])];

    let count_before_fast = tx.default_get_count();
    let batch = c.create_values(&mut tx, &rows).await.unwrap();
    let fast_resolutions = tx.default_get_count() - count_before_fast;
    assert_eq!(batch.len(), 2);

    // Fallback path: same rows with default_* context key triggering per-record resolution
    let fallback_c = c.with_context([("default_ctx_trigger", Value::str("fallback"))]);
    let count_before_fallback = tx.default_get_count();
    let fallback_batch = fallback_c.create_values(&mut tx, &rows).await.unwrap();
    let fallback_resolutions = tx.default_get_count() - count_before_fallback;
    assert_eq!(fallback_batch.len(), 2);

    // Fast path resolves hierarchy once per batch (6 resolutions across C, B, A parent batches),
    // while fallback path resolves per record for each level (10 resolutions: C (2*3=6) + B (2*1=2) + A (2*1=2)).
    assert_eq!(
        fast_resolutions, 6,
        "fast path must resolve hierarchy once per batch"
    );
    assert_eq!(
        fallback_resolutions, 10,
        "fallback path must resolve hierarchy per record"
    );

    // Compare all fields across fast path and fallback path
    assert_eq!(
        batch.values(&mut tx, "a_val").await.unwrap(),
        fallback_batch.values(&mut tx, "a_val").await.unwrap()
    );
    assert_eq!(
        batch.values(&mut tx, "a_num").await.unwrap(),
        fallback_batch.values(&mut tx, "a_num").await.unwrap()
    );
    assert_eq!(
        batch.values(&mut tx, "b_val").await.unwrap(),
        fallback_batch.values(&mut tx, "b_val").await.unwrap()
    );
    assert_eq!(
        batch.values(&mut tx, "c_val").await.unwrap(),
        fallback_batch.values(&mut tx, "c_val").await.unwrap()
    );

    // Row 0: all static defaults across A, B, C
    assert_eq!(
        batch.at(0).unwrap().value(&mut tx, "a_val").await.unwrap(),
        s("a_default")
    );
    assert_eq!(
        batch.at(0).unwrap().value(&mut tx, "a_num").await.unwrap(),
        Value::Int(100)
    );
    assert_eq!(
        batch.at(0).unwrap().value(&mut tx, "b_val").await.unwrap(),
        s("b_default")
    );
    assert_eq!(
        batch.at(0).unwrap().value(&mut tx, "c_val").await.unwrap(),
        s("c_default")
    );

    // Row 1: A and B defaulted, C given
    assert_eq!(
        batch.at(1).unwrap().value(&mut tx, "a_val").await.unwrap(),
        s("a_default")
    );
    assert_eq!(
        batch.at(1).unwrap().value(&mut tx, "a_num").await.unwrap(),
        Value::Int(100)
    );
    assert_eq!(
        batch.at(1).unwrap().value(&mut tx, "b_val").await.unwrap(),
        s("b_default")
    );
    assert_eq!(
        batch.at(1).unwrap().value(&mut tx, "c_val").await.unwrap(),
        s("c_given")
    );

    drop(tx);
    h.cleanup().await.unwrap();
}

static STATIC_DOC: AddonDescriptor = AddonDescriptor {
    name: "test_static_defaults",
    depends: &["base"],
    exact_only: false,
};

fn static_defaults_decls() -> [ModelDecl; 2] {
    [
        ModelDecl::new("test_static_defaults", "x.static_tag")
            .fields(const { &[FieldDecl::char("name")] }),
        ModelDecl::new("test_static_defaults", "x.static_doc").fields(
            const {
                &[
                    FieldDecl::char("name").default(StaticValue::Str("default_name")),
                    FieldDecl::integer("count").default(StaticValue::Int(10)),
                    FieldDecl::boolean("active").default(StaticValue::Bool(true)),
                    FieldDecl::char("tag"),
                    FieldDecl::many2many("tags", "x.static_tag").default(StaticValue::False),
                ]
            },
        ),
    ]
}

/// batch create with mixed rows on an all-static model exercises the batch fast path
/// caching loop, correctly applying static defaults only to the missing fields of each record.
#[tokio::test]
async fn crud_batch_create_mixed_rows_static_defaults() {
    let h = Harness::builder([&base_users::ADDON, &STATIC_DOC])
        .with_decls(static_defaults_decls())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let docs = tx.env().records("x.static_doc").unwrap();
    let mixed = docs
        .create_values(
            &mut tx,
            &[
                map(&[("name", s("given_name_0"))]),
                map(&[]),
                map(&[("name", s("given_name_2")), ("count", Value::Int(42))]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(mixed.len(), 3);
    // Row 0: given name, defaulted count (10) and active (true)
    assert_eq!(
        mixed.at(0).unwrap().value(&mut tx, "name").await.unwrap(),
        s("given_name_0")
    );
    assert_eq!(
        mixed.at(0).unwrap().value(&mut tx, "count").await.unwrap(),
        Value::Int(10)
    );
    assert_eq!(
        mixed.at(0).unwrap().value(&mut tx, "active").await.unwrap(),
        Value::Bool(true)
    );
    // Row 1: all defaulted
    assert_eq!(
        mixed.at(1).unwrap().value(&mut tx, "name").await.unwrap(),
        s("default_name")
    );
    assert_eq!(
        mixed.at(1).unwrap().value(&mut tx, "count").await.unwrap(),
        Value::Int(10)
    );
    assert_eq!(
        mixed.at(1).unwrap().value(&mut tx, "active").await.unwrap(),
        Value::Bool(true)
    );
    // Row 2: given name and given count, defaulted active
    assert_eq!(
        mixed.at(2).unwrap().value(&mut tx, "name").await.unwrap(),
        s("given_name_2")
    );
    assert_eq!(
        mixed.at(2).unwrap().value(&mut tx, "count").await.unwrap(),
        Value::Int(42)
    );
    assert_eq!(
        mixed.at(2).unwrap().value(&mut tx, "active").await.unwrap(),
        Value::Bool(true)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

/// batch create on an all-static model exercises the batch fast path, and
/// comparing fast path (no context) with fallback path (with default_* context key)
/// across all fields (name, count, active, tag) proves the results are strictly identical.
#[tokio::test]
async fn crud_batch_create_static_defaults_fast_path_equals_fallback() {
    let h = Harness::builder([&base_users::ADDON, &STATIC_DOC])
        .with_decls(static_defaults_decls())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    let docs = tx.env().records("x.static_doc").unwrap();

    let rows = [
        map(&[("name", s("given_0"))]),
        map(&[]),
        map(&[("count", Value::Int(99)), ("tag", s("tagged"))]),
    ];

    // Fast path: batch creation without context keys (probed: resolves defaults exactly once for the batch)
    let count_before_fast = tx.default_get_count();
    let fast_batch = docs.create_values(&mut tx, &rows).await.unwrap();
    let fast_resolutions = tx.default_get_count() - count_before_fast;
    assert_eq!(
        fast_resolutions, 1,
        "fast path must resolve defaults exactly once for the whole batch"
    );

    // Fallback path: same rows with default_* context key triggering per-record resolution
    let fallback_docs = docs.with_context([("default_ctx_trigger", Value::str("fallback"))]);
    let count_before_fallback = tx.default_get_count();
    let fallback_batch = fallback_docs.create_values(&mut tx, &rows).await.unwrap();
    let fallback_resolutions = tx.default_get_count() - count_before_fallback;
    assert_eq!(
        fallback_resolutions,
        rows.len() as u64,
        "fallback path must resolve defaults per record in the batch"
    );

    assert_eq!(fast_batch.len(), fallback_batch.len());

    // Compare all fields (scalar + x2many) across fast path and fallback path
    let fast_names = fast_batch.values(&mut tx, "name").await.unwrap();
    let fallback_names = fallback_batch.values(&mut tx, "name").await.unwrap();
    assert_eq!(fast_names, fallback_names);

    let fast_counts = fast_batch.values(&mut tx, "count").await.unwrap();
    let fallback_counts = fallback_batch.values(&mut tx, "count").await.unwrap();
    assert_eq!(fast_counts, fallback_counts);

    let fast_actives = fast_batch.values(&mut tx, "active").await.unwrap();
    let fallback_actives = fallback_batch.values(&mut tx, "active").await.unwrap();
    assert_eq!(fast_actives, fallback_actives);

    let fast_tags = fast_batch.values(&mut tx, "tag").await.unwrap();
    let fallback_tags = fallback_batch.values(&mut tx, "tag").await.unwrap();
    assert_eq!(fast_tags, fallback_tags);

    let fast_rel_tags = fast_batch.values(&mut tx, "tags").await.unwrap();
    let fallback_rel_tags = fallback_batch.values(&mut tx, "tags").await.unwrap();
    assert_eq!(fast_rel_tags, fallback_rel_tags);

    drop(tx);
    h.cleanup().await.unwrap();
}

static X2MANY_DEFAULTS: AddonDescriptor = AddonDescriptor {
    name: "test_crud_x2many_defaults",
    depends: &["base"],
    exact_only: false,
};

static X2M_TAG1: AtomicI64 = AtomicI64::new(0);
static X2M_TAG2: AtomicI64 = AtomicI64::new(0);
static X2M_LINE_COUNTER: AtomicI64 = AtomicI64::new(0);

fn default_m2m_tags(_ctx: &DefaultContext) -> Value {
    Value::list([
        Value::Int(X2M_TAG1.load(Ordering::SeqCst)),
        Value::Int(X2M_TAG2.load(Ordering::SeqCst)),
    ])
}

fn varying_default_o2m_lines(_ctx: &DefaultContext) -> Value {
    let seq = X2M_LINE_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
    Value::list([line(&[("name", Value::str(format!("line_{seq}")))])])
}

fn x2many_defaults_decls() -> [ModelDecl; 3] {
    [
        ModelDecl::new("test_crud_x2many_defaults", "x.x2m_tag")
            .fields(const { &[FieldDecl::char("name")] }),
        ModelDecl::new("test_crud_x2many_defaults", "x.x2m_line").fields(
            const {
                &[
                    FieldDecl::char("name"),
                    FieldDecl::many2one("doc_id", "x.x2m_doc"),
                ]
            },
        ),
        ModelDecl::new("test_crud_x2many_defaults", "x.x2m_doc").fields(
            const {
                &[
                    FieldDecl::char("name").default(StaticValue::Str("default_doc")),
                    FieldDecl::many2many("tag_ids", "x.x2m_tag").default_fn(default_m2m_tags),
                    FieldDecl::one2many("line_ids", "x.x2m_line", "doc_id")
                        .default_fn(varying_default_o2m_lines),
                ]
            },
        ),
    ]
}

/// batch create on a model with a varying x2many function default dispatches
/// default_get per record: the callable default is called once per record (counter),
/// each record gets its own value (distinct line names line_1, line_2, line_3 and distinct line IDs),
/// and all fields (name, tag_ids, line_ids, and line names) are asserted.
#[tokio::test]
async fn crud_batch_create_varying_x2many_function_default_runs_per_record() {
    let h = Harness::builder([&base_users::ADDON, &X2MANY_DEFAULTS])
        .with_decls(x2many_defaults_decls())
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();

    // 1. Create two existing tags to link via many2many default
    let tags = tx
        .env()
        .records("x.x2m_tag")
        .unwrap()
        .create_values(
            &mut tx,
            &[map(&[("name", s("t1"))]), map(&[("name", s("t2"))])],
        )
        .await
        .unwrap();
    let (t1, t2) = (tags.ids()[0], tags.ids()[1]);
    X2M_TAG1.store(t1, Ordering::SeqCst);
    X2M_TAG2.store(t2, Ordering::SeqCst);
    X2M_LINE_COUNTER.store(0, Ordering::SeqCst);

    let docs = tx.env().records("x.x2m_doc").unwrap();

    // 2. Batch create 3 records without explicit values
    let batch = docs
        .create_values(&mut tx, &[map(&[]), map(&[]), map(&[])])
        .await
        .unwrap();
    assert_eq!(batch.len(), 3);

    // 3. Assert callable default was executed exactly once per record (counter == 3)
    let call_count = X2M_LINE_COUNTER.load(Ordering::SeqCst);
    assert_eq!(
        call_count, 3,
        "varying x2many function default must be called once per record across batch"
    );

    // 4. Assert each record gets its own distinct value and links
    let mut all_line_ids = Vec::new();
    let mut all_line_names = Vec::new();
    for (i, rec) in batch.iter().enumerate() {
        assert_eq!(rec.value(&mut tx, "name").await.unwrap(), s("default_doc"));

        let mut linked_tags = rec.related(&mut tx, "tag_ids").await.unwrap().ids();
        linked_tags.sort_unstable();
        assert_eq!(linked_tags, [t1, t2], "each record links existing tags");

        let lines = rec.related(&mut tx, "line_ids").await.unwrap();
        assert_eq!(lines.len(), 1, "each record gets one default line");
        let expected_line_name = format!("line_{}", i + 1);
        let actual_line_name = lines.value(&mut tx, "name").await.unwrap();
        assert_eq!(
            actual_line_name,
            s(&expected_line_name),
            "record {i} gets its own distinct line name from varying default_fn"
        );
        all_line_names.push(actual_line_name);
        all_line_ids.extend(lines.ids());
    }

    assert_eq!(all_line_names, [s("line_1"), s("line_2"), s("line_3")]);
    all_line_ids.sort_unstable();
    all_line_ids.dedup();
    assert_eq!(
        all_line_ids.len(),
        3,
        "each record gets its own distinct created line record in database"
    );

    drop(tx);
    h.cleanup().await.unwrap();
}
