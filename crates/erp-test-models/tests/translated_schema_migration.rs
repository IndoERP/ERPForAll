//! Schema migration and expression index integration tests for translated fields.
//! reverse translation refusal, and trigram expression indexes).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;

use erp_registry::{
    AddonDescriptor, FieldDecl, IndexDecl, IndexMethod, ModelDecl, RegistryBuilder, TableObjectDecl,
};
use erp_sdk::{ErrorKind, IndexKind, Orm, SUPERUSER_ID, StaticPolicy};
use erp_test_models::base_users;
use erp_testkit::Harness;

static ADDON: AddonDescriptor = AddonDescriptor {
    name: "test_mig_addon",
    depends: &["base"],
    exact_only: false,
};

fn statements(captured: &[String]) -> Vec<&str> {
    captured
        .iter()
        .flat_map(|batch| batch.split(';').map(str::trim))
        .filter(|s| !s.is_empty())
        .collect()
}

#[tokio::test]
async fn test_schema_migration_text_to_translated_jsonb() {
    let v1 = ModelDecl::new("test_mig_addon", "test.mig.text2json")
        .table("test_mig_text2json")
        .fields(const { &[FieldDecl::char("name").translate(false)] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([v1])
        .build()
        .await
        .unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO test_mig_text2json (id, name) VALUES (1, 'hello'), (2, ''), (3, NULL)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let v2: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("test_mig_addon", "test.mig.text2json")
            .table("test_mig_text2json")
            .fields(const { &[FieldDecl::char("name").translate(true)] }),
    ));
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .with_decls([v2])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();

    let rows = tx
        .query(
            "SELECT id, name::text, data_type FROM test_mig_text2json \
             JOIN information_schema.columns ON table_name = 'test_mig_text2json' AND column_name = 'name' \
             ORDER BY id",
            &[],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 3);
    // Row 1: 'hello' -> {"en_US": "hello"}
    let r1_name: Option<String> = rows[0].get(1);
    assert_eq!(r1_name.as_deref(), Some("{\"en_US\": \"hello\"}"));
    let r1_type: String = rows[0].get(2);
    assert_eq!(r1_type, "jsonb"); // jsonb in information_schema

    // Row 2: '' -> {"en_US": ""}
    let r2_name: Option<String> = rows[1].get(1);
    assert_eq!(r2_name.as_deref(), Some("{\"en_US\": \"\"}"));

    // Row 3: NULL -> NULL
    let r3_name: Option<String> = rows[2].get(1);
    assert_eq!(r3_name, None);

    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_schema_migration_preexisting_jsonb_validation() {
    let v1 = ModelDecl::new("test_mig_addon", "test.mig.preexisting.jsonb")
        .table("test_mig_preexisting_jsonb")
        .fields(const { &[FieldDecl::json("name")] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([v1])
        .build()
        .await
        .unwrap();

    let mut tx = h.begin().await.unwrap();
    // Insert invalid JSONB row: an array, not an object containing 'en_US'
    tx.execute(
        "INSERT INTO test_mig_preexisting_jsonb (id, name) VALUES (1, '[1, 2, 3]'::jsonb)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let v2: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("test_mig_addon", "test.mig.preexisting.jsonb")
            .table("test_mig_preexisting_jsonb")
            .fields(const { &[FieldDecl::char("name").translate(true)] }),
    ));
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .with_decls([v2])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message().contains("translated field 'test.mig.preexisting.jsonb.name' in table \"test_mig_preexisting_jsonb\" contains invalid JSONB (must be an object containing 'en_US' key)"),
        "unexpected error message: {}",
        err.message()
    );
    tx.rollback().await.unwrap();

    // Now fix the row to valid JSONB object containing 'en_US'
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE test_mig_preexisting_jsonb SET name = '{\"en_US\": \"valid\"}'::jsonb WHERE id = 1",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Sync now succeeds
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();

    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_schema_migration_reverse_translation_refusal() {
    let v1 = ModelDecl::new("test_mig_addon", "test.mig.reverse")
        .table("test_mig_reverse")
        .fields(const { &[FieldDecl::char("name").translate(true)] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([v1])
        .build()
        .await
        .unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO test_mig_reverse (id, name) VALUES (1, '{\"en_US\": \"hello\", \"fr_FR\": \"bonjour\"}'::jsonb)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Reverse translation: translate(false) on populated table must fail before DDL
    let v2: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("test_mig_addon", "test.mig.reverse")
            .table("test_mig_reverse")
            .fields(const { &[FieldDecl::char("name").translate(false)] }),
    ));
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .with_decls([v2])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message().contains("reverse translation of field 'test.mig.reverse.name' on populated table \"test_mig_reverse\" refused: converting translated jsonb column to varchar would destroy non-English translations; manual migration required"),
        "unexpected error message: {}",
        err.message()
    );
    tx.rollback().await.unwrap();

    // Now empty the table
    let mut tx = h.begin().await.unwrap();
    tx.execute("DELETE FROM test_mig_reverse", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // On empty table, reverse translation is allowed and succeeds
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();

    let rows = tx
        .query(
            "SELECT data_type FROM information_schema.columns WHERE table_name = 'test_mig_reverse' AND column_name = 'name'",
            &[],
        )
        .await
        .unwrap();
    let col_type: String = rows[0].get(0);
    assert_eq!(col_type, "character varying");

    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_translated_field_and_model_trigram_indexes() {
    let objects: &'static [TableObjectDecl] = Box::leak(Box::new([TableObjectDecl::index(
        "_translated_model_idx",
        IndexDecl::new(&["desc"]).method(IndexMethod::Trigram),
    )]));
    let decl = ModelDecl::new("test_mig_addon", "test.mig.indexed")
        .table("test_mig_indexed")
        .fields(
            const {
                &[
                    FieldDecl::char("name")
                        .translate(true)
                        .index(Some(IndexKind::Trigram)),
                    FieldDecl::text("desc").translate(true),
                ]
            },
        )
        .table_objects(objects);

    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([decl])
        .build()
        .await
        .unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.batch("CREATE EXTENSION IF NOT EXISTS pg_trgm")
        .await
        .unwrap();
    tx.init_models(&[]).await.unwrap();

    // Check indexes in pg_indexes
    let rows = tx
        .query(
            "SELECT indexname, indexdef FROM pg_indexes WHERE tablename = 'test_mig_indexed' ORDER BY indexname",
            &[],
        )
        .await
        .unwrap();

    let defs: Vec<(String, String)> = rows
        .into_iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect();

    let field_idx_def = defs
        .iter()
        .find(|(n, _)| n == "test_mig_indexed__name_index")
        .map(|(_, d)| d.as_str())
        .expect("field trigram index not found");
    assert!(
        field_idx_def.contains("jsonb_path_query_array(name, '$.*")
            && field_idx_def.contains("gin_trgm_ops"),
        "field index def was: {field_idx_def}"
    );

    let model_idx_def = defs
        .iter()
        .find(|(n, _)| n == "test_mig_indexed_translated_model_idx")
        .map(|(_, d)| d.as_str())
        .expect("model trigram index not found");
    assert!(
        model_idx_def.contains("jsonb_path_query_array(\"desc\", '$.*")
            && model_idx_def.contains("gin_trgm_ops"),
        "model index def was: {model_idx_def}"
    );

    // Second sync: MUST BE IDEMPOTENT (no DDL)
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
        .map(str::to_owned)
        .collect();
    assert!(ddl.is_empty(), "second sync issued DDL: {ddl:#?}");

    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_schema_migration_generic_json_to_scalar_refusal_accurate_message() {
    let v1 = ModelDecl::new("test_mig_addon", "test.mig.generic.json")
        .table("test_mig_generic_json")
        .fields(const { &[FieldDecl::json("payload")] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([v1])
        .build()
        .await
        .unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO test_mig_generic_json (id, payload) VALUES (1, '{\"counter\": 42}'::jsonb)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let v2: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("test_mig_addon", "test.mig.generic.json")
            .table("test_mig_generic_json")
            .fields(const { &[FieldDecl::char("payload")] }),
    ));
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .with_decls([v2])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert!(
        err.message().contains("conversion of jsonb field 'test.mig.generic.json.payload' to varchar on populated table \"test_mig_generic_json\" refused: converting jsonb column to varchar would destroy existing JSON data; manual migration required"),
        "unexpected error message: {}",
        err.message()
    );
    assert!(
        !err.message().contains("destroy non-English translations"),
        "error message should not mention non-English translations for generic json: {}",
        err.message()
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_schema_migration_field_trigram_index_untranslated_to_translated() {
    let v1 = ModelDecl::new("test_mig_addon", "test.mig.trgm.trans")
        .table("test_mig_trgm_trans")
        .fields(const { &[FieldDecl::char("name").index(Some(IndexKind::Trigram))] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([v1])
        .build()
        .await
        .unwrap();

    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO test_mig_trgm_trans (id, name) VALUES (1, 'initial text')",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // Verify initial untranslated index exists and does NOT contain jsonb_path_query_array
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'test_mig_trgm_trans' AND indexname = 'test_mig_trgm_trans__name_index'",
            &[],
        )
        .await
        .unwrap();
    let initial_idx_def: String = rows[0].get(0);
    assert!(
        !initial_idx_def.contains("jsonb_path_query_array"),
        "initial index should not be jsonb expression: {initial_idx_def}"
    );
    tx.rollback().await.unwrap();

    // v2: field is now translated, still declaring IndexKind::Trigram
    let v2: &'static ModelDecl = Box::leak(Box::new(
        ModelDecl::new("test_mig_addon", "test.mig.trgm.trans")
            .table("test_mig_trgm_trans")
            .fields(
                const {
                    &[FieldDecl::char("name")
                        .translate(true)
                        .index(Some(IndexKind::Trigram))]
                },
            ),
    ));
    let registry = Arc::new(
        RegistryBuilder::new([&base_users::ADDON, &ADDON])
            .with_decls([v2])
            .build()
            .unwrap(),
    );
    let orm = Orm::builder(registry, h.db().pool().clone())
        .policy(StaticPolicy::AllowAll)
        .build()
        .unwrap();

    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();

    // Verify index was rebuilt as translated expression index
    let rows = tx
        .query(
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'test_mig_trgm_trans' AND indexname = 'test_mig_trgm_trans__name_index'",
            &[],
        )
        .await
        .unwrap();
    assert!(!rows.is_empty(), "index must exist");
    let migrated_idx_def: String = rows[0].get(0);
    assert!(
        migrated_idx_def.contains("jsonb_path_query_array(name, '$.*")
            && migrated_idx_def.contains("gin_trgm_ops"),
        "migrated index def was: {migrated_idx_def}"
    );

    // Verify column was converted to jsonb and data was preserved
    let val_rows = tx
        .query("SELECT name FROM test_mig_trgm_trans WHERE id = 1", &[])
        .await
        .unwrap();
    let name_json: serde_json::Value = val_rows[0].get(0);
    assert_eq!(name_json, serde_json::json!({"en_US": "initial text"}));

    // Idempotent on second sync
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
        .map(str::to_owned)
        .collect();
    assert!(ddl.is_empty(), "second sync issued DDL: {ddl:#?}");

    tx.commit().await.unwrap();
    h.cleanup().await.unwrap();
}

async fn sync(orm: &Orm) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.init_models(&[]).await.unwrap();
    tx.commit().await.unwrap();
}

async fn verified_marks(h: &Harness, table: &str) -> i64 {
    let mut tx = h.begin().await.unwrap();
    let rows = tx
        .query(
            "SELECT count(*)::int8 FROM erp_translated_verified WHERE table_name = $1",
            &[&table],
        )
        .await
        .unwrap();
    let n: i64 = rows[0].get(0);
    tx.rollback().await.unwrap();
    n
}

#[tokio::test]
async fn recreated_translated_column_is_validated_again() {
    let table = "test_mig_recreated_jsonb";
    let model = "test.mig.recreated.jsonb";
    let decl = ModelDecl::new("test_mig_addon", model)
        .table(table)
        .fields(const { &[FieldDecl::char("name").translate(true)] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([decl])
        .build()
        .await
        .unwrap();
    assert_eq!(verified_marks(&h, table).await, 1);
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE test_mig_recreated_jsonb DROP COLUMN name; ALTER TABLE test_mig_recreated_jsonb ADD COLUMN name jsonb; INSERT INTO test_mig_recreated_jsonb (name) VALUES ('[1]'::jsonb)").await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "cannot update the schema: translated field 'test.mig.recreated.jsonb.name' in table \"test_mig_recreated_jsonb\" contains invalid JSONB (must be an object containing 'en_US' key)"
    );
    tx.rollback().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("UPDATE test_mig_recreated_jsonb SET name = '{\"en_US\":\"fixed\"}'::jsonb")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    sync(h.orm()).await;
    let mut tx = h.begin().await.unwrap();
    let rows = tx.query("SELECT m.table_oid = c.oid AND m.column_attnum = a.attnum FROM erp_translated_verified m JOIN pg_class c ON c.relname = m.table_name JOIN pg_namespace n ON n.oid = c.relnamespace AND n.nspname = current_schema() JOIN pg_attribute a ON a.attrelid = c.oid AND a.attname = m.column_name WHERE m.table_name = $1 AND m.column_name = 'name'", &[&table]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get::<_, bool>(0));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn recreated_translated_table_is_validated_again() {
    let table = "test_mig_recreated_table";
    let decl = ModelDecl::new("test_mig_addon", "test.mig.recreated.table")
        .table(table)
        .fields(const { &[FieldDecl::char("name").translate(true)] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([decl])
        .build()
        .await
        .unwrap();
    assert_eq!(verified_marks(&h, table).await, 1);
    let mut tx = h.begin().await.unwrap();
    tx.batch("DROP TABLE test_mig_recreated_table; CREATE TABLE test_mig_recreated_table (id bigint PRIMARY KEY, name jsonb); INSERT INTO test_mig_recreated_table VALUES (1, '[1]'::jsonb)").await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "cannot update the schema: translated field 'test.mig.recreated.table.name' in table \"test_mig_recreated_table\" contains invalid JSONB (must be an object containing 'en_US' key)"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_mark_does_not_skip_validation() {
    let table = "test_mig_legacy_mark";
    let decl = ModelDecl::new("test_mig_addon", "test.mig.legacy.mark")
        .table(table)
        .fields(const { &[FieldDecl::char("name").translate(true)] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([decl])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE erp_translated_verified DROP COLUMN table_oid, DROP COLUMN column_attnum; INSERT INTO test_mig_legacy_mark (name) VALUES ('[1]'::jsonb)").await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "cannot update the schema: translated field 'test.mig.legacy.mark.name' in table \"test_mig_legacy_mark\" contains invalid JSONB (must be an object containing 'en_US' key)"
    );
    tx.rollback().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("UPDATE test_mig_legacy_mark SET name = '{\"en_US\":\"ok\"}'::jsonb")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    sync(h.orm()).await;
    let mut tx = h.begin().await.unwrap();
    let rows = tx.query("SELECT table_oid IS NOT NULL AND column_attnum IS NOT NULL FROM erp_translated_verified WHERE table_name = $1 AND column_name = 'name'", &[&table]).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get::<_, bool>(0));
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn partial_mark_identity_is_refused() {
    let table = "test_mig_partial_mark";
    let decl = ModelDecl::new("test_mig_addon", "test.mig.partial.mark")
        .table(table)
        .fields(const { &[FieldDecl::char("name").translate(true)] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([decl])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("ALTER TABLE erp_translated_verified DROP COLUMN column_attnum")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Translation verification identity metadata is incomplete"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn fresh_column_refuses_partial_mark_identity() {
    let decl = ModelDecl::new("test_mig_addon", "test.mig.fresh.mark")
        .table("test_mig_fresh_mark")
        .fields(const { &[FieldDecl::char("name").translate(true)] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([decl])
        .without_schema()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.batch("CREATE TABLE erp_translated_verified (table_name text NOT NULL, column_name text NOT NULL, table_oid oid, PRIMARY KEY (table_name, column_name))").await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let err = tx.init_models(&[]).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Programming);
    assert_eq!(
        err.message(),
        "Translation verification identity metadata is incomplete"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn test_schema_migration_verified_jsonb_is_not_rescanned() {
    let table = "test_mig_verified_jsonb";
    let v1 = ModelDecl::new("test_mig_addon", "test.mig.verified.jsonb")
        .table(table)
        .fields(const { &[FieldDecl::json("name")] });
    let h = Harness::builder([&base_users::ADDON, &ADDON])
        .with_decls([v1])
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "INSERT INTO test_mig_verified_jsonb (id, name) VALUES (1, '{\"en_US\": \"ok\"}'::jsonb)",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let orm_for = || {
        let decl: &'static ModelDecl = Box::leak(Box::new(
            ModelDecl::new("test_mig_addon", "test.mig.verified.jsonb")
                .table(table)
                .fields(const { &[FieldDecl::char("name").translate(true)] }),
        ));
        let registry = Arc::new(
            RegistryBuilder::new([&base_users::ADDON, &ADDON])
                .with_decls([decl])
                .build()
                .unwrap(),
        );
        Orm::builder(registry, h.db().pool().clone())
            .policy(StaticPolicy::AllowAll)
            .build()
            .unwrap()
    };

    // first sync as translated: the scan passes and the column is marked
    let translated = orm_for();
    sync(&translated).await;
    assert_eq!(verified_marks(&h, table).await, 1);

    // a row that would fail the scan, written behind the ORM's back: a marked column is not scanned
    let mut tx = h.begin().await.unwrap();
    tx.execute(
        "UPDATE test_mig_verified_jsonb SET name = '[1]'::jsonb WHERE id = 1",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    sync(&translated).await;
    assert_eq!(verified_marks(&h, table).await, 1);

    h.cleanup().await.unwrap();
}
