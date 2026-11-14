// SPDX-License-Identifier: MIT OR Apache-2.0
//! Explicit previous-binary upgrade rehearsal; run on a build host with the pinned old CLI/source.
//! Ordinary guard regressions run in stock's nextest suite. This executable additionally proves
//! the real 1.0 -> current manifest lifecycle, with a later migration failure and a repeat upgrade.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "upgrade rehearsal assertions"
)]
#[path = "../tests/support/runtime_addons.rs"]
mod runtime_addons;
#[path = "../tests/support/company_upgrade.rs"]
mod support;
use erp_modules::{AddonIndex, ModuleName, ModuleSystem};
use erp_testkit::TestDb;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

fn identities(addons: &Path) -> BTreeMap<ModuleName, String> {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons);
    index
        .discover_entries()
        .unwrap()
        .into_iter()
        .map(|(name, entry)| {
            let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
                name.as_str(),
                Path::new(entry.source_location()),
            )
            .unwrap();
            (name, snapshot.digest().to_owned())
        })
        .collect()
}
fn system(db: &TestDb, addons: &Path, previous: BTreeMap<ModuleName, String>) -> ModuleSystem {
    let mut index = AddonIndex::new(true);
    index.add_dev_path(addons);
    ModuleSystem::builder(db.pool().clone(), index)
        .code(erp_cli::native_addons::DESCRIPTORS.iter().copied())
        .require_sql_objects(erp_cli::native_addons::SQL_MANIFESTS.iter().copied())
        .native_artifact_identities(identities(addons))
        .identity_previous(previous)
        .view_types(erp_addon_web::view_type_contributions())
        .require_core_tables(erp_security::auth::core_tables())
        .require_core_tables(erp_addon_base::partner_hierarchy::core_tables())
        .build()
        .unwrap()
}
async fn policy_image(client: &erp_testkit::tokio_postgres::Client, id: i64) -> String {
    client
        .query_one(
            "SELECT row_to_json(p)::text FROM stock_location_policy p WHERE id=$1",
            &[&id],
        )
        .await
        .unwrap()
        .get(0)
}

async fn external_id_image(
    client: &erp_testkit::tokio_postgres::Client,
) -> BTreeMap<i64, serde_json::Value> {
    let image: String = client.query_one(
        "SELECT json_agg(json_build_array(id,module,name,model,res_id,noupdate) ORDER BY id)::text FROM ir_model_data WHERE module='stock'",
        &[],
    ).await.unwrap().get(0);
    let rows: Vec<serde_json::Value> = serde_json::from_str(&image).unwrap();
    rows.into_iter()
        .map(|row| (row[0].as_i64().unwrap(), row))
        .collect()
}

// Exact default identities introduced by the merged stock access slice, plus this slice's
// fallback rows. Parse our declared inputs with the existing data readers, not a second parser.
fn new_stock_defaults() -> BTreeMap<String, String> {
    let mut expected = BTreeMap::new();
    for op in erp_data::parse_xml_str(
        include_str!("../../../addons/stock/security/stock_security.xml"),
        "stock_security.xml",
        "stock",
        false,
    )
    .unwrap()
    {
        if let erp_data::ast::DataOperation::Record(record) = op {
            expected.insert(
                record.id().unwrap().name().to_owned(),
                record.model().to_owned(),
            );
        }
    }
    let erp_data::CsvImportResult::Loaded(import) = erp_data::parse_csv(
        include_str!("../../../addons/stock/security/ir.access.csv"),
        "ir.access.csv",
        "stock",
        erp_data::ConvertMode::Init,
    )
    .unwrap() else {
        panic!("stock access data was not loaded");
    };
    let id = import
        .fields()
        .iter()
        .position(|field| field == "id")
        .unwrap();
    for row in import.rows() {
        expected.insert(row.values()[id].clone(), "ir.access".to_owned());
    }
    for name in [
        "default_property_stock_customer",
        "default_property_stock_supplier",
    ] {
        expected.insert(name.to_owned(), "ir.default".to_owned());
    }
    expected
}

fn assert_external_id_preservation(
    before: &BTreeMap<i64, serde_json::Value>,
    after: &BTreeMap<i64, serde_json::Value>,
) {
    for (id, row) in before {
        assert_eq!(
            after.get(id),
            Some(row),
            "existing external-ID row {id} changed or disappeared"
        );
    }
    // A genuine old binary lacks fields introduced by later staging slices. Their metadata
    // identities are new; every old identity and every default-data mapping must survive.
    for (id, row) in after {
        if !before.contains_key(id) {
            assert!(
                matches!(
                    row[3].as_str(),
                    Some("ir.model" | "ir.model.fields" | "ir.model.fields.selection")
                ) || new_stock_defaults()
                    .get(row[2].as_str().unwrap())
                    .map(String::as_str)
                    == row[3].as_str(),
                "unexpected new default-data external ID: {row}"
            );
        }
    }
}

async fn exercise(old: PathBuf, binary: PathBuf) {
    let source = fs::read_to_string(old.join("REVISION")).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/stock_guards_previous.json")).unwrap();
    assert_eq!(source.trim(), expected["revision"].as_str().unwrap());
    eprintln!(
        "previous_source={} previous_binary_sha256={}",
        source.trim(),
        erp_modules::package::sha256_hex(&fs::read(&binary).unwrap())
    );
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    let role = format!("{}_guard_upgrade", db.name());
    client.batch_execute(&format!("CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION", erp_sql::quote(&role))).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path().canonicalize().unwrap();
    let previous = dir.join("previous");
    fs::create_dir(&previous).unwrap();
    runtime_addons::stage_runtime_addons(&old.join("addons"), &previous.join("addons")).unwrap();
    let config = support::config(&dir, &db, &role, &previous.join("addons"));
    support::private(&dir.join("password"), "fixture-administrator-password");
    support::succeeded(
        support::run(
            &binary,
            &[
                "db",
                "init",
                "--config",
                config.to_str().unwrap(),
                "--admin-login",
                "guard-upgrade-admin",
                "--admin-password-file",
                dir.join("password").to_str().unwrap(),
            ],
            &db,
        ),
        "previous-init",
    );
    support::succeeded(
        support::run(
            &binary,
            &[
                "module",
                "install",
                "stock",
                "-d",
                db.name(),
                "--addons-path",
                previous.join("addons").to_str().unwrap(),
            ],
            &db,
        ),
        "previous-stock-install",
    );
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='stock'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "1.0.0"
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM pg_trigger WHERE tgname='stock_location_policy_insert'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    // This partner is created by the genuine predecessor, before either property exists.
    let legacy_partner: i64 = client
        .query_one(
            "INSERT INTO res_partner(name) VALUES ('Pre-link contact') RETURNING id",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(client.query_one("SELECT count(*) FROM information_schema.columns WHERE table_name='res_partner' AND column_name IN ('property_stock_customer','property_stock_supplier')", &[]).await.unwrap().get::<_, i64>(0), 0);
    let gap: i64 = client.query_one("INSERT INTO stock_location(name,usage,active) VALUES ('Legacy raw gap','view',false) RETURNING id", &[]).await.unwrap().get(0);
    let kept = client
        .query_one(
            "SELECT id,location_id FROM stock_location_policy ORDER BY id LIMIT 1",
            &[],
        )
        .await
        .unwrap();
    let kept_id: i64 = kept.get(0);
    let kept_location: i64 = kept.get(1);
    client
        .execute(
            "UPDATE stock_location_policy SET forbid_negative=true WHERE id=$1",
            &[&kept_id],
        )
        .await
        .unwrap();
    let preserved_policy = policy_image(&client, kept_id).await;
    let previous_product: String = client
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name='product'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let externals: i64 = client
        .query_one(
            "SELECT count(*) FROM ir_model_data WHERE module='stock' AND model NOT IN ('ir.model','ir.model.fields','ir.model.fields.selection')",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let external_id_rows = external_id_image(&client).await;
    support::release_inventory(&previous, source.trim(), Some(&client)).await;
    let old_ids = erp_cli::identity::release_identities(&previous).unwrap();
    let candidate = dir.join("candidate");
    fs::create_dir(&candidate).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    runtime_addons::stage_runtime_addons(&root.join("addons"), &candidate.join("addons")).unwrap();
    let fail = candidate.join("addons/stock/migrations/1.2/post-zzz-refuse.sql");
    fs::write(
        &fail,
        "DO $$ BEGIN RAISE EXCEPTION 'Guard upgrade rollback probe'; END $$;",
    )
    .unwrap();
    // Upgrade every changed installed native prerequisite as the release route requires.
    // In particular, a concurrent staging product upgrade is not hidden by rewriting receipts.
    let candidate_ids = identities(&candidate.join("addons"));
    let upgrades: Vec<ModuleName> = old_ids
        .iter()
        .filter(|(name, digest)| candidate_ids.get(*name) != Some(*digest))
        .map(|(name, _)| name.clone())
        .collect();
    assert!(upgrades.iter().any(|name| name.as_str() == "stock"));
    eprintln!("upgrade_modules={upgrades:?}");
    let sys = system(&db, &candidate.join("addons"), old_ids.clone());
    let error = match sys
        .button_immediate_upgrade(&sys.system_env(erp_orm::Context::new()), &upgrades)
        .await
    {
        Ok(_) => panic!("injected migration failure was accepted"),
        Err(error) => error,
    };
    let error = match error {
        erp_modules::Error::StepFailed { source, .. } => *source,
        error => error,
    };
    let erp_modules::Error::MigrationFailed { source, .. } = error else {
        panic!("unexpected error: {error:?}")
    };
    assert_eq!(source.kind(), erp_fields::ErrorKind::Database);
    assert_eq!(source.message(), "Guard upgrade rollback probe");
    assert_eq!(source.sqlstate(), Some("P0001"));
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM stock_location_policy WHERE location_id=$1",
                &[&gap]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM pg_trigger WHERE tgname='stock_location_policy_insert'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='stock'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "1.0.0"
    );
    let provenance_exists: bool = client
        .query_one(
            "SELECT to_regclass('erp_schema_sql_object') IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    if provenance_exists {
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM erp_schema_sql_object WHERE addon='stock'",
                    &[]
                )
                .await
                .unwrap()
                .get::<_, i64>(0),
            0
        );
    }
    assert_eq!(policy_image(&client, kept_id).await, preserved_policy);
    assert_eq!(external_id_image(&client).await, external_id_rows);
    assert_eq!(client.query_one("SELECT count(*) FROM information_schema.columns WHERE table_name='res_partner' AND column_name IN ('property_stock_customer','property_stock_supplier')", &[]).await.unwrap().get::<_, i64>(0), 0);
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='product'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        previous_product
    );
    eprintln!("step=later-failure rollback=data,DDL,provenance,version");
    drop(sys);
    fs::remove_file(fail).unwrap();
    let sys = system(&db, &candidate.join("addons"), old_ids);
    let outcome = sys
        .button_immediate_upgrade(&sys.system_env(erp_orm::Context::new()), &upgrades)
        .await
        .unwrap();
    assert!(outcome.post_commit.is_clean());
    let upgraded_orm = outcome.orm.clone();
    drop(outcome);
    let mut tx = upgraded_orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let company: i64 = tx
        .query("SELECT id::int8 FROM res_company ORDER BY id LIMIT 1", &[])
        .await
        .unwrap()[0]
        .get(0);
    let env = tx.env().with_company(company).unwrap();
    let contact = env
        .records("res.partner")
        .unwrap()
        .browse([legacy_partner])
        .unwrap();
    for (field, xid) in [
        ("property_stock_customer", "stock_location_customers"),
        ("property_stock_supplier", "stock_location_suppliers"),
    ] {
        let id: i64 = tx
            .query(
                "SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name=$1",
                &[&xid],
            )
            .await
            .unwrap()[0]
            .get(0);
        assert_eq!(
            contact.cell(&mut tx, field).await.unwrap(),
            erp_fields::Cell::Id(id)
        );
    }
    tx.rollback().await.unwrap();
    drop(tx);
    eprintln!("step=topology-links-upgrade legacy-partner=defaulted rollback=columns-absent");
    let candidate_manifest: toml::Table =
        toml::from_str(&fs::read_to_string(candidate.join("addons/stock/manifest.toml")).unwrap())
            .unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='stock'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        candidate_manifest["version"].as_str().unwrap()
    );
    let backfilled = client
        .query_one(
            "SELECT id,forbid_negative,create_uid,write_uid,create_date IS NOT NULL,write_date IS NOT NULL FROM stock_location_policy WHERE location_id=$1",
            &[&gap],
        )
        .await
        .unwrap();
    let new_id: i64 = backfilled.get(0);
    assert!(!backfilled.get::<_, bool>(1));
    assert_eq!(backfilled.get::<_, Option<i64>>(2), None);
    assert_eq!(backfilled.get::<_, Option<i64>>(3), None);
    assert!(backfilled.get::<_, bool>(4));
    assert!(backfilled.get::<_, bool>(5));
    let kept = client
        .query_one(
            "SELECT id,forbid_negative FROM stock_location_policy WHERE location_id=$1",
            &[&kept_location],
        )
        .await
        .unwrap();
    assert_eq!(kept.get::<_, i64>(0), kept_id);
    assert!(kept.get::<_, bool>(1));
    assert_eq!(policy_image(&client, kept_id).await, preserved_policy);
    let upgraded_external_ids = external_id_image(&client).await;
    assert_external_id_preservation(&external_id_rows, &upgraded_external_ids);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM ir_model_data WHERE module='stock' AND model NOT IN ('ir.model','ir.model.fields','ir.model.fields.selection')",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        externals + i64::try_from(new_stock_defaults().keys().filter(|name| !external_id_rows.values().any(|row| row[2].as_str() == Some(name.as_str()))).count()).unwrap()
    );
    let objects: i64 = client
        .query_one(
            "SELECT count(*) FROM erp_schema_sql_object WHERE addon='stock'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(objects > 0);
    // An administrator changes the fallback and explicitly clears one company property.
    let mut tx = upgraded_orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let custom_default: i64 = tx.query("SELECT res_id::int8 FROM ir_model_data WHERE module='stock' AND name='stock_location_suppliers'", &[]).await.unwrap()[0].get(0);
    erp_orm::ir_default::set(
        &mut tx,
        &env,
        "res.partner",
        "property_stock_customer",
        &erp_fields::Value::Int(custom_default),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    env.records("res.partner")
        .unwrap()
        .browse([legacy_partner])
        .unwrap()
        .write(
            &mut tx,
            erp_orm::RawVals::new().set_cell("property_stock_supplier", erp_fields::Cell::Null),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    drop(tx);
    let outcome = sys
        .button_immediate_upgrade(&sys.system_env(erp_orm::Context::new()), &upgrades)
        .await
        .unwrap();
    assert!(outcome.post_commit.is_clean());
    let repeated_orm = outcome.orm.clone();
    drop(outcome);
    assert_eq!(
        client
            .query_one(
                "SELECT id FROM stock_location_policy WHERE location_id=$1",
                &[&gap]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        new_id
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM erp_schema_sql_object WHERE addon='stock'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        objects
    );
    assert_eq!(policy_image(&client, kept_id).await, preserved_policy);
    assert_eq!(external_id_image(&client).await, upgraded_external_ids);
    let mut tx = repeated_orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let env = tx.env().with_company(company).unwrap();
    let contact = env
        .records("res.partner")
        .unwrap()
        .browse([legacy_partner])
        .unwrap();
    assert_eq!(
        contact
            .cell(&mut tx, "property_stock_customer")
            .await
            .unwrap(),
        erp_fields::Cell::Id(custom_default)
    );
    assert_eq!(
        contact
            .cell(&mut tx, "property_stock_supplier")
            .await
            .unwrap(),
        erp_fields::Cell::Null
    );
    tx.rollback().await.unwrap();
    drop(tx);
    eprintln!("step=topology-links-repeat preserved=custom-default,explicit-null");
    let fresh: i64 = client
        .query_one(
            "INSERT INTO stock_location(name,usage) VALUES ('After upgrade','view') RETURNING id",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM stock_location_policy WHERE location_id=$1",
                &[&fresh]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    client.batch_execute("BEGIN").await.unwrap();
    let blocked = client
        .execute(
            "DELETE FROM stock_location_policy WHERE location_id=$1",
            &[&fresh],
        )
        .await
        .unwrap_err();
    assert_eq!(blocked.as_db_error().unwrap().code().code(), "ERP03");
    assert_eq!(
        blocked.as_db_error().unwrap().constraint(),
        Some("stock_location_policy_required")
    );
    client.batch_execute("ROLLBACK").await.unwrap();
    eprintln!(
        "step=upgrade-and-repeat preserved=policy-id,flag,audit-fields,external-ids backfill=complete new-write=guarded"
    );
    client
        .batch_execute(&format!(
            "DROP OWNED BY {}; DROP ROLE {}",
            erp_sql::quote(&role),
            erp_sql::quote(&role)
        ))
        .await
        .unwrap();
    drop(sys);
    drop(client);
    db.cleanup().await.unwrap();
}
fn main() {
    let mut args = std::env::args().skip(1);
    let old = PathBuf::from(args.next().expect("old source directory required"));
    let binary = PathBuf::from(args.next().expect("old CLI path required"));
    assert!(args.next().is_none());
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
                .block_on(exercise(old, binary));
        })
        .unwrap()
        .join()
        .unwrap();
}
