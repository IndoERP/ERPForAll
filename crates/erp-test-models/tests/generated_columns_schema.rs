//! Schema synchronization, introspection, and migration contract for database-generated
//! scalar fields.
//!
//! Covers acceptance through.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod support;

use std::sync::Arc;

use erp_fields::GeneratedStorage;
use erp_orm::schema_generated::{
    GENERATED_FORMAT_VERSION, TABLE_NAME as PROVENANCE_TABLE, compute_signature,
};
use erp_orm::{Orm, SUPERUSER_ID, SchemaSyncOptions, StaticPolicy};
use erp_registry::{
    AddonDescriptor, FieldDecl, ModelDecl, PartitionDecl, PartitionInterval, PartitionRetain,
    Registry, RegistryBuilder,
};
use erp_sdk::ErrorKind;
use erp_test_models::base_users;
use erp_testkit::Harness;
use support::rows;

static TEST_GEN_ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_gen",
    depends: &["base"],
    exact_only: false,
};

fn fields(f: Vec<FieldDecl>) -> &'static [FieldDecl] {
    Box::leak(f.into_boxed_slice())
}

fn model(m: ModelDecl) -> &'static ModelDecl {
    Box::leak(Box::new(m))
}

fn build_registry(
    addons: &[&'static AddonDescriptor],
    decls: Vec<&'static ModelDecl>,
) -> erp_sdk::Result<Registry> {
    RegistryBuilder::new(addons.iter().copied())
        .with_decls(decls)
        .build()
}

async fn sync_with_options(
    h: &Harness,
    decls: Vec<&'static ModelDecl>,
    options: SchemaSyncOptions,
) -> erp_sdk::Result<(Vec<String>, String)> {
    let registry = Arc::new(build_registry(&[&TEST_GEN_ADDON], decls)?);
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()?;
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    tx.capture_statements(true);
    let _ = tx.init_models_with(&[], &options).await?;
    let captured = tx.captured().to_vec();
    let ddl = captured.join(";\n");
    tx.commit().await?;
    Ok((captured, ddl))
}

async fn sync(h: &Harness, decls: Vec<&'static ModelDecl>) -> erp_sdk::Result<Vec<String>> {
    sync_with_options(h, decls, SchemaSyncOptions::inline())
        .await
        .map(|(captured, _)| captured)
}

/// Given: both kinds on an ordinary model and a partitioned model.
/// When: schema sync runs twice and records are inserted/read.
/// Then: catalog kinds/expressions match declarations on parent/leaves, generated columns
/// are omitted from assignments/default backfill, values derive from inputs, and second
/// sync emits no generated-column mutation.
#[tokio::test]
async fn spec_sqlfield_008_fresh_schema_and_repeat_sync() {
    let ord_decl = || {
        ModelDecl::new("test_gen", "test_gen.ordinary").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("v_total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
            FieldDecl::integer("s_total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Stored),
        ]))
    };

    let part_decl = || {
        ModelDecl::new("test_gen", "test_gen.partitioned")
            .partition(PartitionDecl::new(
                PartitionInterval::Day,
                2,
                PartitionRetain::All,
            ))
            .fields(fields(vec![
                FieldDecl::integer("a"),
                FieldDecl::integer("b"),
                FieldDecl::integer("v_total")
                    .generated("a + b")
                    .generated_storage(GeneratedStorage::Virtual),
                FieldDecl::integer("s_total")
                    .generated("a + b")
                    .generated_storage(GeneratedStorage::Stored),
            ]))
    };

    let h = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([ord_decl(), part_decl()])
        .build()
        .await
        .unwrap();

    let mut tx = h.begin().await.unwrap();

    // 1. Verify ordinary table catalog definition in pg_attribute and pg_attrdef
    let ord_cols = rows(
        &mut tx,
        "SELECT a.attname::text, a.attgenerated::text, pg_get_expr(ad.adbin, ad.adrelid)::text \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
         WHERE n.nspname = current_schema() AND c.relname = 'test_gen_ordinary' \
           AND a.attname IN ('v_total', 's_total') \
         ORDER BY a.attname",
    )
    .await;
    assert_eq!(
        ord_cols,
        vec![
            vec![
                Some("s_total".to_owned()),
                Some("s".to_owned()),
                Some("(a + b)".to_owned())
            ],
            vec![
                Some("v_total".to_owned()),
                Some("v".to_owned()),
                Some("(a + b)".to_owned())
            ],
        ]
    );

    // 2. Verify partitioned parent table catalog definition
    let part_cols = rows(
        &mut tx,
        "SELECT a.attname::text, a.attgenerated::text, pg_get_expr(ad.adbin, ad.adrelid)::text \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
         WHERE n.nspname = current_schema() AND c.relname = 'test_gen_partitioned' \
           AND a.attname IN ('v_total', 's_total') \
         ORDER BY a.attname",
    )
    .await;
    assert_eq!(
        part_cols,
        vec![
            vec![
                Some("s_total".to_owned()),
                Some("s".to_owned()),
                Some("(a + b)".to_owned())
            ],
            vec![
                Some("v_total".to_owned()),
                Some("v".to_owned()),
                Some("(a + b)".to_owned())
            ],
        ]
    );

    // 3. Verify provenance table erp_schema_generated
    let ord_spec = h.registry().model("test_gen.ordinary").unwrap();
    let gen_v_sql = ord_spec
        .field("v_total")
        .unwrap()
        .generated()
        .unwrap()
        .sql();
    let gen_s_sql = ord_spec
        .field("s_total")
        .unwrap()
        .generated()
        .unwrap()
        .sql();
    let sig_v = compute_signature(
        GeneratedStorage::Virtual,
        erp_fields::ColumnType::Int4,
        gen_v_sql,
    );
    let sig_s = compute_signature(
        GeneratedStorage::Stored,
        erp_fields::ColumnType::Int4,
        gen_s_sql,
    );

    let prov = rows(
        &mut tx,
        &format!(
            "SELECT table_name, column_name, format_version::text, declaration_signature, \
                    catalog_expression, catalog_type, catalog_kind \
             FROM {PROVENANCE_TABLE} \
             WHERE schema_name = current_schema() AND table_name IN ('test_gen_ordinary', 'test_gen_partitioned') \
             ORDER BY table_name, column_name"
        ),
    )
    .await;
    assert_eq!(
        prov,
        vec![
            vec![
                Some("test_gen_ordinary".to_owned()),
                Some("s_total".to_owned()),
                Some(GENERATED_FORMAT_VERSION.to_string()),
                Some(sig_s.clone()),
                Some("(a + b)".to_owned()),
                Some("\"pg_catalog\".\"int4\":-1".to_owned()),
                Some("s".to_owned()),
            ],
            vec![
                Some("test_gen_ordinary".to_owned()),
                Some("v_total".to_owned()),
                Some(GENERATED_FORMAT_VERSION.to_string()),
                Some(sig_v.clone()),
                Some("(a + b)".to_owned()),
                Some("\"pg_catalog\".\"int4\":-1".to_owned()),
                Some("v".to_owned()),
            ],
            vec![
                Some("test_gen_partitioned".to_owned()),
                Some("s_total".to_owned()),
                Some(GENERATED_FORMAT_VERSION.to_string()),
                Some(sig_s),
                Some("(a + b)".to_owned()),
                Some("\"pg_catalog\".\"int4\":-1".to_owned()),
                Some("s".to_owned()),
            ],
            vec![
                Some("test_gen_partitioned".to_owned()),
                Some("v_total".to_owned()),
                Some(GENERATED_FORMAT_VERSION.to_string()),
                Some(sig_v),
                Some("(a + b)".to_owned()),
                Some("\"pg_catalog\".\"int4\":-1".to_owned()),
                Some("v".to_owned()),
            ],
        ]
    );

    // 4. Insert and read values: omit generated outputs from assignments
    tx.execute("INSERT INTO test_gen_ordinary (a, b) VALUES (12, 34)", &[])
        .await
        .unwrap();
    let read_ord = rows(
        &mut tx,
        "SELECT a::text, b::text, v_total::text, s_total::text FROM test_gen_ordinary",
    )
    .await;
    assert_eq!(
        read_ord,
        vec![vec![
            Some("12".to_owned()),
            Some("34".to_owned()),
            Some("46".to_owned()),
            Some("46".to_owned())
        ]]
    );

    tx.commit().await.unwrap();
    drop(tx);

    // 5. Repeat sync: emits no mutation on generated columns
    let captured = sync(&h, vec![model(ord_decl()), model(part_decl())])
        .await
        .unwrap();
    for stmt in &captured {
        assert!(
            !stmt.to_uppercase().contains("SET EXPRESSION"),
            "unexpected SET EXPRESSION on repeat sync: {stmt}"
        );
        assert!(
            !stmt.to_uppercase().contains("ADD COLUMN \"V_TOTAL\""),
            "unexpected ADD COLUMN on repeat sync: {stmt}"
        );
        assert!(
            !stmt.to_uppercase().contains("ADD COLUMN \"S_TOTAL\""),
            "unexpected ADD COLUMN on repeat sync: {stmt}"
        );
    }
}

/// Given: an installed field whose actual expression/type/kind changes outside sync while
/// provenance/signature stays unchanged.
/// When: schema sync runs again.
/// Then: catalog-drift Programming error occurs before any repair; a missing provenance
/// row also refuses; actual catalog is never trusted solely through declaration hash.
#[tokio::test]
async fn spec_sqlfield_009_catalog_drift_detection() {
    let decl = || {
        ModelDecl::new("test_gen", "test_gen.drift").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
        ]))
    };

    let h = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([decl()])
        .build()
        .await
        .unwrap();

    // 1. Out-of-band alteration: change expression in PostgreSQL directly
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "ALTER TABLE test_gen_drift ALTER COLUMN total SET EXPRESSION AS (a + 999)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // Run schema sync again: must refuse with catalog drift
    let err = sync(&h, vec![model(decl())]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Generated field 'test_gen.drift.total': catalog drift; explicit migration required"
    );

    // Revert the out-of-band change back
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "ALTER TABLE test_gen_drift ALTER COLUMN total SET EXPRESSION AS (a + b)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // Now delete the provenance row: missing provenance row also refuses
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        &format!(
            "DELETE FROM {PROVENANCE_TABLE} \
             WHERE schema_name = current_schema() AND table_name = 'test_gen_drift' AND column_name = 'total'"
        ),
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    let err = sync(&h, vec![model(decl())]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Generated field 'test_gen.drift.total': catalog drift; explicit migration required"
    );
}

/// Given: installed VIRTUAL/STORED fields with rows and a changed expression of the same output type/kind.
/// When: upgrade commits, and a separate upgrade is forced to abort.
/// Then: committed reads reflect the new expression; abort preserves old definition, values,
/// provenance and visible schema generation; retry is idempotent; STORED statistics maintenance is reported.
#[tokio::test]
async fn spec_sqlfield_010_expression_change_and_rollback() {
    let decl_v1 = || {
        ModelDecl::new("test_gen", "test_gen.upgrade").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("v_calc")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
            FieldDecl::integer("s_calc")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Stored),
        ]))
    };

    let h = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([decl_v1()])
        .build()
        .await
        .unwrap();

    // Seed rows
    let mut tx = h.begin().await.unwrap();
    tx.execute("INSERT INTO test_gen_upgrade (a, b) VALUES (10, 20)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // Declaration with updated expression: "a + b + 10"
    let decl_v2 = || {
        ModelDecl::new("test_gen", "test_gen.upgrade").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("v_calc")
                .generated("a + b + 10")
                .generated_storage(GeneratedStorage::Virtual),
            FieldDecl::integer("s_calc")
                .generated("a + b + 10")
                .generated_storage(GeneratedStorage::Stored),
        ]))
    };

    // 1. Upgrade commits: verify reads reflect new expression and provenance updated
    let captured = sync(&h, vec![model(decl_v2())]).await.unwrap();
    let has_set_v = captured
        .iter()
        .any(|s| s.contains("v_calc") && s.contains("SET EXPRESSION"));
    let has_set_s = captured
        .iter()
        .any(|s| s.contains("s_calc") && s.contains("SET EXPRESSION"));
    assert!(has_set_v, "v_calc did not run SET EXPRESSION: {captured:?}");
    assert!(has_set_s, "s_calc did not run SET EXPRESSION: {captured:?}");

    let mut tx = h.begin().await.unwrap();
    let row = rows(
        &mut tx,
        "SELECT a::text, b::text, v_calc::text, s_calc::text FROM test_gen_upgrade",
    )
    .await;
    assert_eq!(
        row,
        vec![vec![
            Some("10".to_owned()),
            Some("20".to_owned()),
            Some("40".to_owned()), // 10 + 20 + 10
            Some("40".to_owned()),
        ]]
    );

    // Verify provenance updated
    let prov = rows(
        &mut tx,
        &format!(
            "SELECT column_name, catalog_expression FROM {PROVENANCE_TABLE} \
             WHERE schema_name = current_schema() AND table_name = 'test_gen_upgrade' \
             ORDER BY column_name"
        ),
    )
    .await;
    assert_eq!(
        prov,
        vec![
            vec![Some("s_calc".to_owned()), Some("((a + b) + 10)".to_owned())],
            vec![Some("v_calc".to_owned()), Some("((a + b) + 10)".to_owned())],
        ]
    );
    tx.commit().await.unwrap();
    drop(tx);

    // 2. Aborted upgrade preserves old definition, values and provenance
    let decl_v3 = || {
        ModelDecl::new("test_gen", "test_gen.upgrade").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("v_calc")
                .generated("a + b + 50")
                .generated_storage(GeneratedStorage::Virtual),
            FieldDecl::integer("s_calc")
                .generated("a + b + 50")
                .generated_storage(GeneratedStorage::Stored),
        ]))
    };

    let reg_v3 = Arc::new(build_registry(&[&TEST_GEN_ADDON], vec![model(decl_v3())]).unwrap());
    let orm_v3 = Orm::builder(reg_v3, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();
    let mut abort_tx = orm_v3.begin(SUPERUSER_ID).await.unwrap();
    let _ = abort_tx
        .init_models_with(&[], &SchemaSyncOptions::inline())
        .await
        .unwrap();
    // Force abort by rolling back instead of committing
    abort_tx.rollback().await.unwrap();
    drop(abort_tx);
    drop(orm_v3);

    // Inspect state after abort: should still be v2 (40, not 80)
    let mut tx = h.begin().await.unwrap();
    let row_after_abort = rows(
        &mut tx,
        "SELECT v_calc::text, s_calc::text FROM test_gen_upgrade",
    )
    .await;
    assert_eq!(
        row_after_abort,
        vec![vec![Some("40".to_owned()), Some("40".to_owned())]]
    );
    let prov_after_abort = rows(
        &mut tx,
        &format!(
            "SELECT column_name, catalog_expression FROM {PROVENANCE_TABLE} \
             WHERE schema_name = current_schema() AND table_name = 'test_gen_upgrade' \
             ORDER BY column_name"
        ),
    )
    .await;
    assert_eq!(
        prov_after_abort,
        vec![
            vec![Some("s_calc".to_owned()), Some("((a + b) + 10)".to_owned())],
            vec![Some("v_calc".to_owned()), Some("((a + b) + 10)".to_owned())],
        ]
    );
    tx.commit().await.unwrap();
    drop(tx);

    // 3. Retry is idempotent and commits cleanly
    sync(&h, vec![model(decl_v3())]).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let row_v3 = rows(
        &mut tx,
        "SELECT v_calc::text, s_calc::text FROM test_gen_upgrade",
    )
    .await;
    assert_eq!(
        row_v3,
        vec![vec![Some("80".to_owned()), Some("80".to_owned())]]
    );
    tx.commit().await.unwrap();
    drop(tx);
}

/// Given: ordinary/generated conversion, storage-kind switch, output-type change and removal.
/// When: automatic schema sync is requested.
/// Then: automatic-conversion Programming error leaves original values/definition intact;
/// no DROP/CASCADE/recreate or implicit data adoption.
#[tokio::test]
async fn spec_sqlfield_011_destructive_conversion_refusal() {
    // Case 1: Ordinary to generated conversion
    let ord_decl = || {
        ModelDecl::new("test_gen", "test_gen.ord_to_gen").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total"),
        ]))
    };
    let h1 = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([ord_decl()])
        .build()
        .await
        .unwrap();

    let gen_decl1 = || {
        ModelDecl::new("test_gen", "test_gen.ord_to_gen").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
        ]))
    };
    let err1 = sync(&h1, vec![model(gen_decl1())]).await.unwrap_err();
    assert_eq!(err1.kind(), ErrorKind::Programming);
    assert_eq!(
        err1.message(),
        "Generated field 'test_gen.ord_to_gen.total': automatic conversion is refused; explicit migration required"
    );

    // Case 2: Generated to ordinary conversion
    let gen_decl2 = || {
        ModelDecl::new("test_gen", "test_gen.gen_to_ord").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
        ]))
    };
    let h2 = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([gen_decl2()])
        .build()
        .await
        .unwrap();

    let ord_decl2 = || {
        ModelDecl::new("test_gen", "test_gen.gen_to_ord").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total"),
        ]))
    };
    let err2 = sync(&h2, vec![model(ord_decl2())]).await.unwrap_err();
    assert_eq!(err2.kind(), ErrorKind::Programming);
    assert_eq!(
        err2.message(),
        "Generated field 'test_gen.gen_to_ord.total': automatic conversion is refused; explicit migration required"
    );

    // Case 3: Virtual to Stored conversion
    let v_decl = || {
        ModelDecl::new("test_gen", "test_gen.v_to_s").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
        ]))
    };
    let h3 = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([v_decl()])
        .build()
        .await
        .unwrap();

    let s_decl = || {
        ModelDecl::new("test_gen", "test_gen.v_to_s").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Stored),
        ]))
    };
    let err3 = sync(&h3, vec![model(s_decl())]).await.unwrap_err();
    assert_eq!(err3.kind(), ErrorKind::Programming);
    assert_eq!(
        err3.message(),
        "Generated field 'test_gen.v_to_s.total': automatic conversion is refused; explicit migration required"
    );

    // Case 4: Stored to Virtual conversion
    let s_decl4 = || {
        ModelDecl::new("test_gen", "test_gen.s_to_v").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Stored),
        ]))
    };
    let h4 = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([s_decl4()])
        .build()
        .await
        .unwrap();

    let v_decl4 = || {
        ModelDecl::new("test_gen", "test_gen.s_to_v").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
        ]))
    };
    let err4 = sync(&h4, vec![model(v_decl4())]).await.unwrap_err();
    assert_eq!(err4.kind(), ErrorKind::Programming);
    assert_eq!(
        err4.message(),
        "Generated field 'test_gen.s_to_v.total': automatic conversion is refused; explicit migration required"
    );

    // Case 5: Output type change
    let int_decl = || {
        ModelDecl::new("test_gen", "test_gen.type_change").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
        ]))
    };
    let h5 = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([int_decl()])
        .build()
        .await
        .unwrap();

    let float_decl = || {
        ModelDecl::new("test_gen", "test_gen.type_change").fields(fields(vec![
            FieldDecl::float("a"),
            FieldDecl::float("b"),
            FieldDecl::float("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
        ]))
    };
    let err5 = sync(&h5, vec![model(float_decl())]).await.unwrap_err();
    assert_eq!(err5.kind(), ErrorKind::Programming);
    assert_eq!(
        err5.message(),
        "Generated field 'test_gen.type_change.total': automatic conversion is refused; explicit migration required"
    );

    // Case 6: Generation removal
    let with_gen = || {
        ModelDecl::new("test_gen", "test_gen.removal").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Virtual),
        ]))
    };
    let h6 = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([with_gen()])
        .build()
        .await
        .unwrap();

    let without_gen = || {
        ModelDecl::new("test_gen", "test_gen.removal").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
        ]))
    };
    let err6 = sync(&h6, vec![model(without_gen())]).await.unwrap_err();
    assert_eq!(err6.kind(), ErrorKind::Programming);
    assert_eq!(
        err6.message(),
        "Generated field 'test_gen.removal.total': automatic conversion is refused; explicit migration required"
    );
}

/// Given: a populated table above the configured threshold or with unknown size and a STORED add/expression change.
/// When: normal sync and separately explicit inline maintenance mode run.
/// Then: normal sync gives maintenance-window Programming error; controlled maintenance performs
/// the change within deadlines; lock/statement failure rolls back; no false online claim or ready state with stale expression.
#[tokio::test]
async fn spec_sqlfield_012_rewrite_and_lock_gate() {
    let base_decl = || {
        ModelDecl::new("test_gen", "test_gen.gate").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
        ]))
    };

    let h = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([base_decl()])
        .build()
        .await
        .unwrap();

    // Populate table with 20 rows
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO test_gen_gate (a, b) SELECT g, g * 2 FROM generate_series(1, 20) g",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    drop(tx);

    // Adding STORED generated field
    let stored_decl = || {
        ModelDecl::new("test_gen", "test_gen.gate").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("s_total")
                .generated("a + b")
                .generated_storage(GeneratedStorage::Stored),
        ]))
    };

    // 1. Normal sync with large_table_rows(10): populated table (20 rows) exceeds threshold (10)
    let normal_options = SchemaSyncOptions::new().large_table_rows(10);
    let err = sync_with_options(&h, vec![model(stored_decl())], normal_options)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Generated field 'test_gen.gate.s_total': rewrite requires a maintenance window"
    );

    // 2. Explicit inline maintenance mode: executes within deadlines and succeeds
    let inline_options = SchemaSyncOptions::inline();
    let (captured, _) = sync_with_options(&h, vec![model(stored_decl())], inline_options)
        .await
        .unwrap();
    let has_add = captured
        .iter()
        .any(|s| s.contains("s_total") && s.contains("ADD COLUMN"));
    assert!(has_add, "s_total was not added: {captured:?}");

    // Verify values were populated and ANALYZE ran
    let mut tx = h.begin().await.unwrap();
    let first_row = rows(
        &mut tx,
        "SELECT a::text, b::text, s_total::text FROM test_gen_gate WHERE id = 1",
    )
    .await;
    assert_eq!(
        first_row,
        vec![vec![
            Some("1".to_owned()),
            Some("2".to_owned()),
            Some("3".to_owned())
        ]]
    );
    tx.commit().await.unwrap();
    drop(tx);

    // 3. Changing STORED expression on large table also requires maintenance window
    let changed_stored = || {
        ModelDecl::new("test_gen", "test_gen.gate").fields(fields(vec![
            FieldDecl::integer("a"),
            FieldDecl::integer("b"),
            FieldDecl::integer("s_total")
                .generated("a + b + 5")
                .generated_storage(GeneratedStorage::Stored),
        ]))
    };

    let err = sync_with_options(
        &h,
        vec![model(changed_stored())],
        SchemaSyncOptions::new().large_table_rows(10),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Generated field 'test_gen.gate.s_total': rewrite requires a maintenance window"
    );

    // 4. In inline mode, changing STORED expression succeeds
    sync_with_options(
        &h,
        vec![model(changed_stored())],
        SchemaSyncOptions::inline(),
    )
    .await
    .unwrap();

    let mut tx = h.begin().await.unwrap();
    let first_row_v2 = rows(
        &mut tx,
        "SELECT a::text, b::text, s_total::text FROM test_gen_gate WHERE id = 1",
    )
    .await;
    assert_eq!(
        first_row_v2,
        vec![vec![
            Some("1".to_owned()),
            Some("2".to_owned()),
            Some("8".to_owned())
        ]]
    );
    tx.commit().await.unwrap();
    drop(tx);
}

fn identity_decl(size: u32) -> ModelDecl {
    ModelDecl::new("test_gen", "test_gen.identity").fields(fields(vec![
        FieldDecl::char("source").size(10),
        FieldDecl::char("total")
            .size(size)
            .generated("source")
            .generated_storage(GeneratedStorage::Stored),
    ]))
}

async fn identity_snapshot(h: &Harness) -> Vec<Vec<Option<String>>> {
    let mut tx = h.begin().await.unwrap();
    rows(&mut tx, "SELECT a.atttypid::text, a.atttypmod::text, a.attcollation::text, a.attgenerated::text, pg_get_expr(d.adbin,d.adrelid), (SELECT row_to_json(p)::text FROM erp_schema_generated p WHERE p.schema_name=current_schema() AND p.table_name='test_gen_identity' AND p.column_name='total'), (SELECT json_agg(total ORDER BY id)::text FROM test_gen_identity) FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE n.nspname=current_schema() AND c.relname='test_gen_identity' AND a.attname='total'").await
}

async fn identity_refusal(h: &Harness, size: u32, reason: &str) {
    let before = identity_snapshot(h).await;
    let orm = Orm::builder(
        Arc::new(build_registry(&[&TEST_GEN_ADDON], vec![model(identity_decl(size))]).unwrap()),
        h.db().pool().clone(),
    )
    .policy(StaticPolicy::AllowAll)
    .build()
    .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let err = tx
        .init_models_with(&[], &SchemaSyncOptions::inline())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("Generated field 'test_gen.identity.total': {reason}; explicit migration required")
    );
    assert!(
        !tx.captured().iter().any(|s| {
            let s = s.trim_start().to_uppercase();
            s.starts_with("ALTER TABLE") || s.starts_with("DROP ") || s.starts_with("CREATE TABLE")
        }),
        "refusal must precede repair DDL: {:?}",
        tx.captured()
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        identity_snapshot(h).await,
        before,
        "refusal changed catalog, provenance or values"
    );
}

async fn identity_harness() -> Harness {
    let h = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([identity_decl(10)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO test_gen_identity (source) VALUES ('hello')",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h
}

#[tokio::test]
async fn generated_collation_drift_refuses_before_repair() {
    let h = identity_harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE test_gen_identity DROP COLUMN total; ALTER TABLE test_gen_identity ADD COLUMN total varchar(10) COLLATE pg_catalog.\"C\" GENERATED ALWAYS AS (source) STORED").await.unwrap();
    tx.commit().await.unwrap();
    identity_refusal(&h, 10, "catalog drift").await;
}

#[tokio::test]
async fn generated_catalog_typmod_drift_refuses_before_repair() {
    let h = identity_harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE test_gen_identity DROP COLUMN total; ALTER TABLE test_gen_identity ADD COLUMN total varchar(20) GENERATED ALWAYS AS (source) STORED").await.unwrap();
    tx.commit().await.unwrap();
    identity_refusal(&h, 10, "catalog drift").await;
}

#[tokio::test]
async fn generated_declared_typmod_conversion_refuses_before_repair() {
    let h = identity_harness().await;
    for size in [5, 20] {
        identity_refusal(&h, size, "automatic conversion is refused").await;
    }
}

#[tokio::test]
async fn generated_unknown_provenance_format_refuses_before_repair() {
    let h = identity_harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.execute("UPDATE erp_schema_generated SET format_version = format_version + 1 WHERE table_name='test_gen_identity'", &[]).await.unwrap();
    tx.commit().await.unwrap();
    identity_refusal(&h, 10, "unsupported provenance format").await;
}

#[tokio::test]
async fn generated_catalog_type_and_kind_drift_refuses_before_repair() {
    for definition in [
        "total text GENERATED ALWAYS AS (source) STORED",
        "total varchar(10) GENERATED ALWAYS AS (source) VIRTUAL",
    ] {
        let h = identity_harness().await;
        let mut tx = h.begin().await.unwrap();
        tx.batch(&format!("ALTER TABLE test_gen_identity DROP COLUMN total; ALTER TABLE test_gen_identity ADD COLUMN {definition}")).await.unwrap();
        tx.commit().await.unwrap();
        identity_refusal(&h, 10, "catalog drift").await;
    }
}

#[tokio::test]
async fn generated_same_name_collations_keep_schema_identity() {
    let h = identity_harness().await;
    let mut tx = h.begin().await.unwrap();
    tx.batch("CREATE SCHEMA gen_left; CREATE SCHEMA gen_right; CREATE COLLATION gen_left.shared FROM pg_catalog.\"C\"; CREATE COLLATION gen_right.shared FROM pg_catalog.\"C\"; ALTER TABLE test_gen_identity DROP COLUMN total; ALTER TABLE test_gen_identity ADD COLUMN total varchar(10) COLLATE gen_left.shared GENERATED ALWAYS AS (source) STORED; UPDATE erp_schema_generated SET catalog_collation='\"gen_left\".\"shared\"' WHERE table_name='test_gen_identity' AND column_name='total'").await.unwrap();
    tx.commit().await.unwrap();
    // Owner migration established the first qualified identity. Simultaneous owner changes to
    // catalog and provenance are outside's tamper boundary; the later catalog-only
    // replacement must still refuse, even though both collation names are `shared`.
    sync(&h, vec![model(identity_decl(10))]).await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE test_gen_identity DROP COLUMN total; ALTER TABLE test_gen_identity ADD COLUMN total varchar(10) COLLATE gen_right.shared GENERATED ALWAYS AS (source) STORED").await.unwrap();
    tx.commit().await.unwrap();
    identity_refusal(&h, 10, "catalog drift").await;
}

fn later_source_decl(has_output: bool, has_later_source: bool) -> ModelDecl {
    let mut declared = Vec::new();
    // The output intentionally precedes both ordinary sources in declaration order.
    if has_output {
        declared.push(
            FieldDecl::integer("total")
                .generated(if has_later_source { "a + later" } else { "a" })
                .generated_storage(GeneratedStorage::Stored),
        );
    }
    declared.push(FieldDecl::integer("a"));
    if has_later_source {
        declared.push(FieldDecl::integer("later"));
    }
    ModelDecl::new("test_gen", "test_gen.later_source").fields(fields(declared))
}

async fn later_source_upgrade(existing_output: bool) {
    let h = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
        .with_decls([later_source_decl(existing_output, false)])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute("INSERT INTO test_gen_later_source (a) VALUES (7)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let captured = sync(&h, vec![model(later_source_decl(true, true))])
        .await
        .unwrap();
    // A schema phase is one SQL batch: compare offsets inside its captured text.
    let ddl = captured.join(";\n");
    let source = ddl
        .find("ADD COLUMN \"later\"")
        .expect("new ordinary source DDL");
    let output_operation = if existing_output {
        "SET EXPRESSION"
    } else {
        "ADD COLUMN \"total\""
    };
    let output = ddl.find(output_operation).expect("generated output DDL");
    assert!(source < output, "source must precede output: {captured:?}");

    let mut tx = h.begin().await.unwrap();
    tx.execute("UPDATE test_gen_later_source SET later = 4", &[])
        .await
        .unwrap();
    assert_eq!(
        rows(
            &mut tx,
            "SELECT a::text, later::text, total::text FROM test_gen_later_source"
        )
        .await,
        vec![vec![
            Some("7".to_owned()),
            Some("4".to_owned()),
            Some("11".to_owned())
        ]]
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn generated_add_installs_later_declared_source_first() {
    later_source_upgrade(false).await;
}

#[tokio::test]
async fn generated_expression_change_installs_later_declared_source_first() {
    later_source_upgrade(true).await;
}

// Snapshot all remaining columns and row values, rather than selecting `total`: catalog-only
// tampering may have removed that column. The parent OID also detects drop/recreation.
async fn ownership_snapshot(h: &Harness) -> Vec<Vec<Option<String>>> {
    let mut tx = h.begin().await.unwrap();
    ownership_state(&mut tx).await
}

async fn ownership_state(tx: &mut erp_sdk::Transaction) -> Vec<Vec<Option<String>>> {
    rows(tx, "SELECT c.oid::text, c.relkind::text, \
        (SELECT json_agg(row_to_json(def) ORDER BY def.attnum)::text FROM \
            (SELECT a.attnum, a.attname, a.atttypid, a.atttypmod, a.attcollation, a.attgenerated, \
                    pg_get_expr(d.adbin, d.adrelid) AS expression \
             FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum \
             WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped) def), \
        (SELECT json_agg(row_to_json(p) ORDER BY p.column_name)::text FROM erp_schema_generated p \
         WHERE p.schema_name=current_schema() AND p.table_name='test_gen_identity'), \
        (SELECT json_agg(row_to_json(t) ORDER BY t.id)::text FROM test_gen_identity t) \
        FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
        WHERE n.nspname=current_schema() AND c.relname='test_gen_identity'").await
}

async fn ownership_refusal(h: &Harness, declaration: ModelDecl, reason: &str) {
    let before = ownership_snapshot(h).await;
    assert_eq!(before.len(), 1, "original parent must exist");
    let orm = Orm::builder(
        Arc::new(build_registry(&[&TEST_GEN_ADDON], vec![model(declaration)]).unwrap()),
        h.db().pool().clone(),
    )
    .policy(StaticPolicy::AllowAll)
    .build()
    .unwrap();
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.capture_statements(true);
    let err = tx
        .init_models_with(&[], &SchemaSyncOptions::inline())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("Generated field 'test_gen.identity.total': {reason}; explicit migration required")
    );
    assert!(
        !tx.captured().iter().any(|statement| {
            let sql = statement.trim_start().to_uppercase();
            sql.starts_with("ALTER TABLE")
                || sql.starts_with("DROP ")
                || sql.starts_with("CREATE TABLE")
                || sql.contains("CASCADE")
        }),
        "refusal must precede DROP/CASCADE/recreate or repair: {:?}",
        tx.captured()
    );
    // Inspect before rollback too, so rollback cannot hide provenance or value mutations.
    assert_eq!(
        ownership_state(&mut tx).await,
        before,
        "refusal mutated catalog, provenance or remaining values inside its transaction"
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        ownership_snapshot(h).await,
        before,
        "refusal changed original catalog, provenance or remaining values"
    );
}

#[tokio::test]
async fn generated_partition_recreation_preserves_integrity_guards() {
    for transition in ["storage", "type", "removal", "drift"] {
        // No inserts or deletes: this table is eligible for the never-written recreation path.
        let h = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
            .with_decls([identity_decl(10)])
            .build()
            .await
            .unwrap();
        let mut declared = vec![FieldDecl::char("source").size(10)];
        if transition != "removal" {
            declared.push(
                FieldDecl::char("total")
                    .size(if transition == "type" { 20 } else { 10 })
                    .generated("source")
                    .generated_storage(if transition == "storage" {
                        GeneratedStorage::Virtual
                    } else {
                        GeneratedStorage::Stored
                    }),
            );
        }
        if transition == "drift" {
            let mut tx = h.begin().await.unwrap();
            tx.batch("ALTER TABLE test_gen_identity DROP COLUMN total; ALTER TABLE test_gen_identity ADD COLUMN total varchar(20) GENERATED ALWAYS AS (source) STORED")
                .await.unwrap();
            tx.commit().await.unwrap();
        }
        let partitioned = ModelDecl::new("test_gen", "test_gen.identity")
            .partition(PartitionDecl::new(
                PartitionInterval::Day,
                2,
                PartitionRetain::All,
            ))
            .fields(fields(declared));
        ownership_refusal(
            &h,
            partitioned,
            if transition == "drift" {
                "catalog drift"
            } else {
                "automatic conversion is refused"
            },
        )
        .await;
    }
}

fn ownership_decl(output: Option<bool>, another_generated: bool) -> ModelDecl {
    let mut declared = vec![FieldDecl::char("source").size(10)];
    if let Some(generated) = output {
        let field = FieldDecl::char("total").size(10);
        declared.push(if generated {
            field
                .generated("source")
                .generated_storage(GeneratedStorage::Stored)
        } else {
            field
        });
    }
    if another_generated {
        declared.push(
            FieldDecl::char("other")
                .size(10)
                .generated("source")
                .generated_storage(GeneratedStorage::Stored),
        );
    }
    ModelDecl::new("test_gen", "test_gen.identity").fields(fields(declared))
}

#[tokio::test]
async fn generated_provenance_only_ownership_refuses_before_repair() {
    for another_generated in [false, true] {
        for drop_column in [false, true] {
            for redeclare_ordinary in [false, true] {
                let h = Harness::builder([&base_users::ADDON, &TEST_GEN_ADDON])
                    .with_decls([ownership_decl(Some(true), another_generated)])
                    .build()
                    .await
                    .unwrap();
                let mut tx = h.begin().await.unwrap();
                tx.execute(
                    "INSERT INTO test_gen_identity (source) VALUES ('hello')",
                    &[],
                )
                .await
                .unwrap();
                tx.batch(if drop_column {
                    "ALTER TABLE test_gen_identity DROP COLUMN total"
                } else {
                    "ALTER TABLE test_gen_identity ALTER COLUMN total DROP EXPRESSION"
                })
                .await
                .unwrap();
                tx.commit().await.unwrap();
                ownership_refusal(
                    &h,
                    ownership_decl(
                        if redeclare_ordinary {
                            Some(false)
                        } else {
                            None
                        },
                        another_generated,
                    ),
                    "catalog drift",
                )
                .await;
            }
        }
    }
}
