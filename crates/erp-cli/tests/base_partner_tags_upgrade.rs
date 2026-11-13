//! Category parent-store activation and genuine base 1.1 -> 1.8 upgrade acceptance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "fixture assertions"
)]

use erp_sdk::migrations::SemverVersion;
use erp_sdk::{Cell, ErrorKind, RawVals};
use erp_testkit::Harness;

async fn fixture() -> Harness {
    Harness::builder([&erp_test_models::base_users::ADDON, &erp_addon_base::ADDON])
        .build()
        .await
        .unwrap()
}

async fn roots(tx: &mut erp_sdk::Transaction, count: usize) -> Vec<i64> {
    tx.env()
        .records("res.partner.category")
        .unwrap()
        .create(
            tx,
            (0..count)
                .map(|index| {
                    RawVals::new().set_cell("name", Cell::Str(format!("Tenant tag {index}").into()))
                })
                .collect(),
        )
        .await
        .unwrap()
        .ids()
}

async fn paths(tx: &mut erp_sdk::Transaction, ids: &[i64]) -> Vec<Option<String>> {
    tx.query(
        "SELECT parent_path FROM res_partner_category WHERE id=ANY($1) ORDER BY id",
        &[&ids],
    )
    .await
    .unwrap()
    .into_iter()
    .map(|row| row.get(0))
    .collect()
}

// Direct helper tests exercise migration admission boundaries. They do not claim genuine
// previous-version upgrade evidence; the opt-in old-binary test below supplies that separately.
#[tokio::test]
async fn category_migration_backfills_existing_column_and_is_idempotent() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let ids = roots(&mut tx, 3).await;
    tx.flush().await.unwrap();
    tx.execute("UPDATE res_partner_category SET parent_id=CASE WHEN id=$2 THEN $1 WHEN id=$3 THEN $2 ELSE NULL END, parent_path=NULL WHERE id=ANY($4)", &[&ids[0], &ids[1], &ids[2], &ids]).await.unwrap();
    erp_addon_base::migrations::prepare_base_1_8_category_paths(
        &mut tx,
        &SemverVersion::new(1, 1, 0),
    )
    .await
    .unwrap();
    let expected = vec![
        Some(format!("{}/", ids[0])),
        Some(format!("{}/{}/", ids[0], ids[1])),
        Some(format!("{}/{}/{}/", ids[0], ids[1], ids[2])),
    ];
    assert_eq!(paths(&mut tx, &ids).await, expected);
    erp_addon_base::migrations::prepare_base_1_8_category_paths(
        &mut tx,
        &SemverVersion::new(1, 1, 0),
    )
    .await
    .unwrap();
    assert_eq!(paths(&mut tx, &ids).await, expected);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_migration_refuses_cycles_without_partially_filling_paths() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let ids = roots(&mut tx, 3).await;
    tx.flush().await.unwrap();
    tx.execute("UPDATE res_partner_category SET parent_id=CASE WHEN id=$1 THEN $2 WHEN id=$2 THEN $1 ELSE NULL END, parent_path=NULL WHERE id=ANY($3)", &[&ids[0], &ids[1], &ids]).await.unwrap();
    let err = erp_addon_base::migrations::prepare_base_1_8_category_paths(
        &mut tx,
        &SemverVersion::new(1, 1, 0),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "base 1.8 category migration refused: category hierarchy contains a cycle or a missing parent"
    );
    assert_eq!(paths(&mut tx, &ids).await, vec![None, None, None]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_migration_refuses_depth_over_256_without_partial_update() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let ids = roots(&mut tx, 257).await;
    tx.flush().await.unwrap();
    tx.execute("WITH ordered AS (SELECT id, lag(id) OVER (ORDER BY position) AS parent FROM unnest($1::int8[]) WITH ORDINALITY AS fixture(id, position)) UPDATE res_partner_category category SET parent_id=ordered.parent,parent_path=NULL FROM ordered WHERE category.id=ordered.id", &[&ids]).await.unwrap();
    let err = erp_addon_base::migrations::prepare_base_1_8_category_paths(
        &mut tx,
        &SemverVersion::new(1, 1, 0),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "base 1.8 category migration refused: category hierarchy exceeds depth 256"
    );
    assert_eq!(paths(&mut tx, &ids).await, vec![None; ids.len()]);
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_migration_accepts_empty_store_and_absent_legacy_table() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    erp_addon_base::migrations::prepare_base_1_8_category_paths(
        &mut tx,
        &SemverVersion::new(1, 1, 0),
    )
    .await
    .unwrap();
    tx.batch("DROP TABLE res_partner_category CASCADE")
        .await
        .unwrap();
    erp_addon_base::migrations::prepare_base_1_8_category_paths(
        &mut tx,
        &SemverVersion::new(1, 0, 0),
    )
    .await
    .unwrap();
    assert!(
        !tx.query(
            "SELECT to_regclass('public.res_partner_category') IS NOT NULL",
            &[]
        )
        .await
        .unwrap()[0]
            .get::<_, bool>(0)
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn category_migration_accepts_exact_depth_256() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let ids = roots(&mut tx, 256).await;
    tx.flush().await.unwrap();
    tx.execute("WITH ordered AS (SELECT id, lag(id) OVER (ORDER BY position) AS parent FROM unnest($1::int8[]) WITH ORDINALITY AS fixture(id, position)) UPDATE res_partner_category category SET parent_id=ordered.parent,parent_path=NULL FROM ordered WHERE category.id=ordered.id", &[&ids]).await.unwrap();
    erp_addon_base::migrations::prepare_base_1_8_category_paths(
        &mut tx,
        &SemverVersion::new(1, 1, 0),
    )
    .await
    .unwrap();
    assert_eq!(
        paths(&mut tx, &ids).await.last().unwrap().as_deref(),
        Some(
            ids.iter()
                .map(|id| format!("{id}/"))
                .collect::<String>()
                .as_str()
        )
    );
    drop(tx);
    h.cleanup().await.unwrap();
}

#[path = "support/runtime_addons.rs"]
mod runtime_addons;

use erp_testkit::{TestDb, tokio_postgres::config::Host};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const OLD_SOURCE: &str = "d63b60d7489c1deee4ee8ec1a7f47bbb7fbb293e";

fn private(path: &Path, value: &str) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn encoded(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
            out.push(char::from(*byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn url(db: &TestDb, role: Option<&str>) -> String {
    let config = db.config();
    let Host::Tcp(host) = &config.get_hosts()[0] else {
        panic!("genuine CLI upgrade requires a TCP test cluster")
    };
    let (user, password) = match role {
        Some(role) => (role, b"fixture-runtime-password".as_slice()),
        None => (config.get_user().unwrap(), config.get_password().unwrap()),
    };
    format!(
        "postgresql://{}:{}@{}:{}/{}",
        encoded(user.as_bytes()),
        encoded(password),
        host,
        config.get_ports()[0],
        db.name()
    )
}

fn run(binary: &Path, args: &[&str], db: &TestDb) -> Output {
    Command::new(binary)
        .args(args)
        .env("DATABASE_URL", url(db, None))
        .output()
        .unwrap()
}

fn succeeded(output: Output, step: &str) {
    eprintln!(
        "step={step} status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{step} failed");
}

fn previous_release(old: &Path, dir: &Path, rows: &[(String, String)]) -> PathBuf {
    let previous = dir.join("previous");
    fs::create_dir(&previous).unwrap();
    let previous = previous.canonicalize().unwrap();
    runtime_addons::stage_runtime_addons(&old.join("addons"), &previous.join("addons")).unwrap();
    let mut identities = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    for (name, installed_digest) in rows {
        let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
            name,
            &previous.join("addons").join(name),
        )
        .unwrap();
        assert_eq!(
            snapshot.digest(),
            installed_digest,
            "old installed native receipt for {name}"
        );
        for (path, bytes) in snapshot.files() {
            hashes.insert(
                format!("addons/{name}/{path}"),
                erp_modules::package::sha256_hex(bytes),
            );
        }
        identities.insert(name, snapshot.digest().to_owned());
    }
    fs::write(previous.join("REVISION"), OLD_SOURCE).unwrap();
    fs::write(
        previous.join("PACKAGE.json"),
        serde_json::to_vec(
            &serde_json::json!({"source":OLD_SOURCE,"sha256":hashes,"addon_identities":identities}),
        )
        .unwrap(),
    )
    .unwrap();
    erp_cli::identity::release_identities(&previous).unwrap();
    previous
}

/// Genuine prior binary creates schema/data; neither the module version nor the schema is rewound.
#[tokio::test]
#[ignore = "requires retained locked d63b60d74 native CLI/source on an approved build host"]
async fn genuine_base_1_1_category_upgrade_preserves_data_and_rolls_back_late_failure() {
    let old = PathBuf::from(
        std::env::var("ERP_PARTNER_TAGS_OLD_SOURCE").expect("ERP_PARTNER_TAGS_OLD_SOURCE required"),
    );
    assert_eq!(
        fs::read_to_string(old.join("REVISION")).unwrap().trim(),
        OLD_SOURCE
    );
    let old_bin = old.join("target/debug/erp");
    eprintln!(
        "old_source={OLD_SOURCE} old_binary_sha256={} old_lock_sha256={}",
        erp_modules::package::sha256_hex(&fs::read(&old_bin).unwrap()),
        erp_modules::package::sha256_hex(&fs::read(old.join("Cargo.lock")).unwrap())
    );
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    let role = format!("{}_tags", db.name());
    client.batch_execute(&format!("CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION", erp_sql::quote(&role))).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    private(&dir.path().join("owner-url"), &url(&db, None));
    private(&dir.path().join("runtime-url"), &url(&db, Some(&role)));
    private(
        &dir.path().join("password"),
        "fixture-administrator-password",
    );
    runtime_addons::stage_runtime_addons(&old.join("addons"), &dir.path().join("runtime-addons"))
        .unwrap();
    let addons = dir.path().join("runtime-addons").canonicalize().unwrap();
    let config = dir.path().join("server.toml");
    fs::write(&config, format!("database='{}'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nbind='127.0.0.1:18091'\npublic_origin='http://127.0.0.1:18091'\nproxy_mode='direct'\naddons_path=['{}']\nassets_manifest='manifest.json'\n",db.name(),addons.display())).unwrap();
    let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let assets: Vec<_> = [("js", "text/javascript; charset=utf-8"), ("css", "text/css; charset=utf-8")].into_iter().map(|(extension,mime)| {
        let path = format!("client.{digest}.{extension}");
        fs::write(dir.path().join(&path), b"").unwrap();
        serde_json::json!({"name":format!("client.{extension}"),"path":path,"sha256":digest,"content_type":mime})
    }).collect();
    fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema":1,"assets":assets})).unwrap(),
    )
    .unwrap();
    succeeded(
        run(
            &old_bin,
            &[
                "db",
                "init",
                "--config",
                config.to_str().unwrap(),
                "--admin-login",
                "category-upgrade-admin",
                "--admin-password-file",
                dir.path().join("password").to_str().unwrap(),
            ],
            &db,
        ),
        "old-base-1.1-init",
    );
    let old_version: String = client
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name='base'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(old_version, "1.1.0");
    assert_eq!(client.query_one("SELECT data_type FROM information_schema.columns WHERE table_name='res_partner_category' AND column_name='name'",&[]).await.unwrap().get::<_,String>(0),"character varying");
    let root: i64 = client.query_one("INSERT INTO res_partner_category(name,color,active) VALUES('Tenant / Customer',9,true) RETURNING id",&[]).await.unwrap().get(0);
    let child: i64 = client.query_one("INSERT INTO res_partner_category(name,color,active,parent_id) VALUES('VIP Ω',0,false,$1) RETURNING id",&[&root]).await.unwrap().get(0);
    let partner: i64 = client
        .query_one(
            "SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_partner'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    client.execute("INSERT INTO res_partner_res_partner_category_rel(partner_id,category_id) VALUES($1,$2)",&[&partner,&child]).await.unwrap();
    client.execute("INSERT INTO ir_model_data(module,name,model,res_id,noupdate) VALUES('tenant_upgrade','archived_tag','res.partner.category',$1,true)",&[&child]).await.unwrap();
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM res_partner_category WHERE parent_path IS NOT NULL",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    let receipts = client.query("SELECT name,content_hash FROM installed_addon_identity WHERE source_kind <> 'package' ORDER BY name",&[]).await.unwrap().into_iter().map(|row|(row.get(0),row.get(1))).collect::<Vec<_>>();
    let previous = previous_release(&old, dir.path(), &receipts);
    let current = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons");
    let current_bin = Path::new(env!("CARGO_BIN_EXE_erp"));
    let args = [
        "module",
        "upgrade",
        "base",
        "-d",
        db.name(),
        "--addons-path",
        current.to_str().unwrap(),
        "--identity-previous",
        previous.to_str().unwrap(),
    ];
    // A fixture-owned event trigger fails after the new translated-name DDL has run. The
    // isolated database has no other project workload, and the trigger is dropped below.
    client.batch_execute("CREATE FUNCTION fixture_refuse_category_translation() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS (SELECT 1 FROM pg_event_trigger_ddl_commands() WHERE object_identity='public.res_partner_category') AND EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name='res_partner_category' AND column_name='name' AND data_type='jsonb') THEN RAISE EXCEPTION 'fixture late category schema refusal' USING ERRCODE='23514'; END IF; END $$; CREATE EVENT TRIGGER fixture_refuse_category_translation ON ddl_command_end WHEN TAG IN ('ALTER TABLE') EXECUTE FUNCTION fixture_refuse_category_translation()").await.unwrap();
    let refused = run(current_bin, &args, &db);
    eprintln!(
        "step=late-category-schema-failure status={} stderr={}",
        refused.status,
        String::from_utf8_lossy(&refused.stderr)
    );
    // CLI boundary: assert its exact database-failure exit class and complete diagnostic.
    // Direct migration tests separately assert the typed Validation boundary.
    assert_eq!(refused.status.code(), Some(4));
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        "Error: Module \"base\": schema update failed: The operation cannot be completed: fixture late category schema refusal: db error: ERROR: fixture late category schema refusal\n"
    );
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='base'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        old_version
    );
    assert_eq!(client.query_one("SELECT data_type FROM information_schema.columns WHERE table_name='res_partner_category' AND column_name='name'",&[]).await.unwrap().get::<_,String>(0),"character varying");
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM res_partner_category WHERE parent_path IS NOT NULL",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    client.batch_execute("DROP EVENT TRIGGER fixture_refuse_category_translation; DROP FUNCTION fixture_refuse_category_translation()").await.unwrap();
    succeeded(run(current_bin, &args, &db), "base-1.8-category-upgrade");
    let row=client.query_one("SELECT name->>'en_US',color,active,parent_id,parent_path FROM res_partner_category WHERE id=$1",&[&child]).await.unwrap();
    assert_eq!(row.get::<_, String>(0), "VIP Ω");
    assert_eq!(row.get::<_, i32>(1), 0);
    assert!(!row.get::<_, bool>(2));
    assert_eq!(row.get::<_, i64>(3), root);
    assert_eq!(row.get::<_, String>(4), format!("{root}/{child}/"));
    assert_eq!(
        client
            .query_one(
                "SELECT parent_path FROM res_partner_category WHERE id=$1",
                &[&root]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        format!("{root}/")
    );
    assert_eq!(client.query_one("SELECT res_id FROM ir_model_data WHERE module='tenant_upgrade' AND name='archived_tag'",&[]).await.unwrap().get::<_,i64>(0),child);
    assert_eq!(client.query_one("SELECT category_id FROM res_partner_res_partner_category_rel WHERE partner_id=$1 AND category_id=$2",&[&partner,&child]).await.unwrap().get::<_,i64>(0),child);
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='base'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "1.9.0"
    );
    succeeded(
        run(current_bin, &args, &db),
        "base-1.8-category-upgrade-repeat-with-retained-previous-identity",
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM res_partner_category WHERE id=ANY($1)",
                &[&vec![root, child]]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    drop(client);
    let mut admin = db.config();
    admin.dbname("postgres");
    db.cleanup().await.unwrap();
    let (client, connection) = admin
        .connect(erp_testkit::tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(async move { connection.await.unwrap() });
    client
        .batch_execute(&format!("DROP ROLE {}", erp_sql::quote(&role)))
        .await
        .unwrap();
}

#[tokio::test]
async fn category_migration_refuses_missing_parent_without_partial_update() {
    let h = fixture().await;
    let mut tx = h.begin().await.unwrap();
    let ids = roots(&mut tx, 2).await;
    tx.flush().await.unwrap();
    let constraints = tx.query("SELECT constraint_row.conname FROM pg_constraint constraint_row JOIN pg_attribute attribute ON attribute.attrelid=constraint_row.conrelid AND attribute.attname='parent_id' WHERE constraint_row.conrelid='res_partner_category'::regclass AND constraint_row.contype='f' AND constraint_row.conkey=ARRAY[attribute.attnum]", &[]).await.unwrap();
    assert_eq!(constraints.len(), 1);
    let constraint: String = constraints[0].get(0);
    tx.batch(&format!(
        "ALTER TABLE res_partner_category DROP CONSTRAINT {}",
        erp_sql::quote(&constraint)
    ))
    .await
    .unwrap();
    tx.execute("UPDATE res_partner_category SET parent_id=CASE WHEN id=$1 THEN 987654321 ELSE NULL END, parent_path=NULL WHERE id=ANY($2)", &[&ids[0], &ids]).await.unwrap();
    let err = erp_addon_base::migrations::prepare_base_1_8_category_paths(
        &mut tx,
        &SemverVersion::new(1, 1, 0),
    )
    .await
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Validation);
    assert_eq!(
        err.message(),
        "base 1.8 category migration refused: category hierarchy contains a cycle or a missing parent"
    );
    assert_eq!(paths(&mut tx, &ids).await, vec![None, None]);
    drop(tx);
    h.cleanup().await.unwrap();
}
