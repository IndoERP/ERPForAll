//! Genuine old-binary UoM adoption; no version rewinds.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration assertions"
)]
#[path = "support/runtime_addons.rs"]
mod runtime_addons;
use erp_testkit::{TestDb, tokio_postgres::config::Host};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};
fn private(path: &Path, value: &str) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn encoded(bytes: &[u8]) -> String {
    let mut result = String::new();
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
            result.push(char::from(*byte));
        } else {
            result.push_str(&format!("%{byte:02X}"));
        }
    }
    result
}
fn url(config: &erp_testkit::tokio_postgres::Config, role: Option<&str>) -> String {
    let Host::Tcp(host) = &config.get_hosts()[0] else {
        panic!("native HTTP fixture requires the configured TCP test cluster")
    };
    let user = role.unwrap_or(config.get_user().unwrap());
    let password = if role.is_some() {
        b"fixture-runtime-password".as_slice()
    } else {
        config.get_password().unwrap()
    };
    format!(
        "postgresql://{}:{}@{}:{}/{}",
        encoded(user.as_bytes()),
        encoded(password),
        host,
        config.get_ports()[0],
        config.get_dbname().unwrap()
    )
}
fn config(dir: &Path, db: &TestDb, port: u16, role: &str, source: &Path) -> PathBuf {
    private(&dir.join("owner-url"), &url(&db.config(), None));
    private(&dir.join("runtime-url"), &url(&db.config(), Some(role)));
    let addons = dir.join("runtime-addons");
    runtime_addons::stage_runtime_addons(source, &addons)
        .expect("stage admitted runtime addons before native server startup");
    // Physical path: addon identity admission refuses the macOS /var -> /private/var link.
    let addons = addons.canonicalize().unwrap();
    let text = format!(
        "database='{}'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nbind='127.0.0.1:{port}'\npublic_origin='http://127.0.0.1:{port}'\nproxy_mode='direct'\naddons_path=['{}']\nassets_manifest='manifest.json'\n",
        db.name(),
        addons.display()
    );
    fs::write(dir.join("server.toml"), text).unwrap();
    let digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let assets:Vec<_>=[("js","text/javascript; charset=utf-8"),("css","text/css; charset=utf-8")].into_iter().map(|(ext,mime)|{let path=format!("client.{digest}.{ext}");fs::write(dir.join(&path),b"").unwrap();serde_json::json!({"name":format!("client.{ext}"),"path":path,"sha256":digest,"content_type":mime})}).collect();
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema":1,"assets":assets})).unwrap(),
    )
    .unwrap();
    dir.join("server.toml")
}

fn run(binary: &Path, args: &[&str], db: &TestDb) -> std::process::Output {
    Command::new(binary)
        .args(args)
        .env("DATABASE_URL", url(&db.config(), None))
        .output()
        .unwrap()
}
fn succeeded(output: std::process::Output, step: &str) {
    eprintln!(
        "step={step} status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{step} failed");
}

#[tokio::test]
#[ignore = "requires separately built pre-guard native CLI/source on approved host"]
async fn genuine_pre_guard_uom_upgrade_preserves_units_and_rolls_back_failure() {
    let old =
        PathBuf::from(std::env::var("ERP_OLD_UOM_SOURCE").expect("ERP_OLD_UOM_SOURCE required"));
    let revision = fs::read_to_string(old.join("REVISION")).unwrap();
    assert_eq!(revision.trim(), "67868f460a526d7079d2628d269388970989e508");
    let old_bin = old.join("bin/erp");
    let expected_digest = fs::read_to_string(old.join("binary.sha256")).unwrap();
    assert_eq!(
        erp_modules::package::sha256_hex(&fs::read(&old_bin).unwrap()),
        expected_digest.trim()
    );
    eprintln!(
        "old-source={} old-binary-sha256={}",
        revision.trim(),
        expected_digest.trim()
    );
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    let role = format!("{}_legacy", db.name());
    client.batch_execute(&format!("CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",erp_sql::quote(&role))).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let conf = config(dir.path(), &db, 18089, &role, &old.join("addons"));
    private(
        &dir.path().join("password"),
        "fixture-administrator-password",
    );
    succeeded(
        run(
            &old_bin,
            &[
                "db",
                "init",
                "--config",
                conf.to_str().unwrap(),
                "--admin-login",
                "legacy-admin",
                "--admin-password-file",
                dir.path().join("password").to_str().unwrap(),
            ],
            &db,
        ),
        "old-db-init",
    );
    succeeded(
        run(
            &old_bin,
            &[
                "module",
                "install",
                "uom",
                "-d",
                db.name(),
                "--addons-path",
                old.join("addons").to_str().unwrap(),
            ],
            &db,
        ),
        "old-uom-install",
    );
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='uom'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "1.0.0"
    );
    assert!(client.query_one("SELECT to_regclass('public.uom_reference_fence') IS NULL AND to_regclass('public.uom_reference_use') IS NULL",&[]).await.unwrap().get::<_,bool>(0));
    let seed:i64=client.query_one("SELECT res_id::bigint FROM ir_model_data WHERE module='uom' AND name='product_uom_dozen'",&[]).await.unwrap().get(0);
    client
        .execute(
            "UPDATE uom_uom SET name='{\"en_US\":\"Tenant dozen\"}'::jsonb, active=false WHERE id=$1",
            &[&seed],
        )
        .await
        .unwrap();
    let custom:i64=client.query_one("INSERT INTO uom_uom(name,active,relative_factor,factor,parent_path) VALUES('{\"en_US\":\"Tenant root\"}'::jsonb,true,1,1,NULL) RETURNING id::bigint",&[]).await.unwrap().get(0);
    client
        .execute(
            "UPDATE uom_uom SET parent_path=id::text||'/' WHERE id=$1",
            &[&custom],
        )
        .await
        .unwrap();
    let before: i64 = client
        .query_one("SELECT count(*) FROM uom_uom", &[])
        .await
        .unwrap()
        .get(0);
    let previous = dir.path().join("previous");
    fs::create_dir(&previous).unwrap();
    let previous = previous.canonicalize().unwrap();
    runtime_addons::stage_runtime_addons(&old.join("addons"), &previous.join("addons")).unwrap();
    let mut identities = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    for row in client.query("SELECT name,content_hash FROM installed_addon_identity WHERE source_kind <> 'package' ORDER BY name",&[]).await.unwrap() {
        let name:String=row.get(0);let snapshot=erp_modules::artifact_identity::ArtifactSnapshot::capture(&name,&previous.join("addons").join(&name)).unwrap();
        assert_eq!(snapshot.digest(),row.get::<_,String>(1));
        for (path,bytes) in snapshot.files(){hashes.insert(format!("addons/{name}/{path}"),erp_modules::package::sha256_hex(bytes));}
        identities.insert(name,snapshot.digest().to_owned());
    }
    fs::write(previous.join("REVISION"), revision.trim()).unwrap();
    fs::write(previous.join("PACKAGE.json"),serde_json::to_vec(&serde_json::json!({"source":revision.trim(),"sha256":hashes,"addon_identities":identities})).unwrap()).unwrap();
    erp_cli::identity::release_identities(&previous).unwrap();
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
        "uom",
        "-d",
        db.name(),
        "--addons-path",
        current.to_str().unwrap(),
        "--identity-previous",
        previous.to_str().unwrap(),
    ];
    // Runs after schema/model init and fence initialization, proving transactional rollback.
    client.batch_execute("CREATE FUNCTION fixture_refuse_uom_version() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.name='uom' AND NEW.latest_version='1.1.0' THEN RAISE EXCEPTION 'fixture refused final UoM version' USING ERRCODE='23514'; END IF; RETURN NEW; END $$; CREATE TRIGGER fixture_refuse_uom_version BEFORE UPDATE ON ir_module_module FOR EACH ROW EXECUTE FUNCTION fixture_refuse_uom_version()").await.unwrap();
    let refused = run(current_bin, &args, &db);
    eprintln!(
        "step=later-failure-rollback status={} stderr={}",
        refused.status,
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("fixture refused final UoM version"));
    assert!(client.query_one("SELECT to_regclass('public.uom_reference_fence') IS NULL AND to_regclass('public.uom_reference_use') IS NULL",&[]).await.unwrap().get::<_,bool>(0));
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='uom'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "1.0.0"
    );
    client.batch_execute("DROP TRIGGER fixture_refuse_uom_version ON ir_module_module;DROP FUNCTION fixture_refuse_uom_version()").await.unwrap();
    succeeded(run(current_bin, &args, &db), "forward-uom-upgrade");
    let release_parent = fs::canonicalize(dir.path()).unwrap().join("releases");
    fs::create_dir(&release_parent).unwrap();
    let release = release_parent.join("candidate");
    fs::create_dir(&release).unwrap();
    runtime_addons::stage_runtime_addons(&current, &release.join("addons")).unwrap();
    let mut candidate_ids = BTreeMap::new();
    let mut candidate_hashes = BTreeMap::new();
    for (name, linked) in erp_cli::identity::native_artifact_identities().unwrap() {
        let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
            name.as_str(),
            &release.join("addons").join(name.as_str()),
        )
        .unwrap();
        assert_eq!(
            snapshot.digest(),
            linked,
            "candidate fixture must match compiled native addon"
        );
        for (path, bytes) in snapshot.files() {
            candidate_hashes.insert(
                format!("addons/{name}/{path}"),
                erp_modules::package::sha256_hex(bytes),
            );
        }
        candidate_ids.insert(name.to_string(), snapshot.digest().to_owned());
    }
    let candidate_source = fs::read_to_string(current.parent().unwrap().join("REVISION"))
        .unwrap()
        .trim()
        .to_owned();
    fs::write(release.join("REVISION"), &candidate_source).unwrap();
    fs::write(release.join("PACKAGE.json"), serde_json::to_vec(&serde_json::json!({"source":candidate_source,"sha256":candidate_hashes,"addon_identities":candidate_ids})).unwrap()).unwrap();
    let proof = dir.path().join("forward-upgrade.json");
    private(&proof, &serde_json::to_string(&serde_json::json!({"source":candidate_source,"previous":revision.trim(),"exit":0,"upgrade_before_activation":true})).unwrap());
    std::os::unix::fs::symlink(
        &release,
        fs::canonicalize(dir.path()).unwrap().join("current"),
    )
    .unwrap();
    succeeded(
        run(
            current_bin,
            &[
                "module",
                "identity-activate",
                "--config",
                conf.to_str().unwrap(),
                "--release",
                release.to_str().unwrap(),
                "--previous",
                previous.to_str().unwrap(),
                "--upgrade-receipt",
                proof.to_str().unwrap(),
            ],
            &db,
        ),
        "explicit-forward-identity-activation",
    );
    client
        .query(
            "SELECT * FROM erp_uom_register($1::bigint[])",
            &[&vec![seed]],
        )
        .await
        .unwrap();
    succeeded(
        run(
            current_bin,
            &[
                "module",
                "upgrade",
                "uom",
                "-d",
                db.name(),
                "--addons-path",
                current.to_str().unwrap(),
            ],
            &db,
        ),
        "repeat-uom-upgrade",
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM uom_uom", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        before
    );
    assert_eq!(
        client
            .query_one(
                "SELECT name->>'en_US',active FROM uom_uom WHERE id=$1",
                &[&seed]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "Tenant dozen"
    );
    assert!(
        !client
            .query_one("SELECT active FROM uom_uom WHERE id=$1", &[&seed])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    assert_eq!(client.query_one("SELECT res_id::bigint FROM ir_model_data WHERE module='uom' AND name='product_uom_dozen'",&[]).await.unwrap().get::<_,i64>(0),seed);
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM uom_uom WHERE id=$1 AND name->>'en_US'='Tenant root'",
                &[&custom]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM uom_reference_use WHERE uom_id=$1",
                &[&seed]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    assert_eq!(
        client
            .query_one(
                "SELECT count(*) FROM uom_reference_fence WHERE singleton=1",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    assert_eq!(
        client
            .query_one(
                "SELECT latest_version FROM ir_module_module WHERE name='uom'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
        "1.1.0"
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
