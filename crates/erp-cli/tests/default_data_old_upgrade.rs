//! Opt-in genuine b678 installation to the current native CLI upgrade; no marker rewinds.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "fixture assertions"
)]
#[path = "support/original_currency_ids.rs"]
mod original_currency_ids;
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
#[ignore = "requires pinned retained old native CLI/source on approved host"]
async fn genuine_old_cli_to_current_default_data_upgrade() {
    let old = PathBuf::from(std::env::var("ERP_OLD_SOURCE").expect("ERP_OLD_SOURCE required"));
    assert_eq!(
        fs::read_to_string(old.join("REVISION")).unwrap().trim(),
        "b6780f923109f3a8212de11f276952cd9f0bfd73"
    );
    assert_eq!(
        erp_modules::package::sha256_hex(&fs::read(old.join("Cargo.lock")).unwrap()),
        "1d3a0d822ed23f68f230b6e487054c09a0c4b49cf5856486b75804d5afc5331d"
    );
    let old_bin = old.join("target/debug/erp");
    let old_binary_digest = erp_modules::package::sha256_hex(&fs::read(&old_bin).unwrap());
    // Preserve the historical artifact and separately admit the lead-approved locked b678 rebuild.
    // Both exact receipts are retained;
    // the differing build digest's cause is unverified. No environment-selected hash is accepted.
    let accepted_old_artifacts = [
        "78330d0ae1c280d1fe5d88af3e26f63e7f879db46397d3d7055534c0861fa925",
        "408fdb6141fa25a89497cf0a7420c64913fe44df389a9ba4ba60024ba9117735",
    ];
    assert!(
        accepted_old_artifacts.contains(&old_binary_digest.as_str()),
        "old b678 binary digest {old_binary_digest} is not one of {accepted_old_artifacts:?}"
    );
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    let role = format!("{}_legacy", db.name());
    client.batch_execute(&format!("CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",erp_sql::quote(&role))).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path(), &db, 18089, &role, &old.join("addons"));
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
                config.to_str().unwrap(),
                "--admin-login",
                "legacy-admin",
                "--admin-password-file",
                dir.path().join("password").to_str().unwrap(),
            ],
            &db,
        ),
        "old-db-init",
    );
    // Verify genuine old schema, not a current database with a rewound version marker.
    let old_version: String = client
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name='base'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(old_version.starts_with("1.0"), "{old_version}");
    let absent:bool=client.query_one("SELECT NOT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='res_company' AND column_name='partner_id')",&[]).await.unwrap().get(0);
    assert!(
        absent,
        "old source unexpectedly has new representative field"
    );
    let original_currency_ids = original_currency_ids::snapshot(&client).await;
    let branch: i64 = client
        .query_one(
            "INSERT INTO res_company(name,currency_id) SELECT 'Legacy branch',currency_id FROM res_company WHERE id=(SELECT res_id FROM ir_model_data WHERE module='base' AND name='main_company') RETURNING id",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let contact:i64=client.query_one("INSERT INTO res_partner(name,company_id,active) VALUES('Tenant unrelated customer',$1,true) RETURNING id",&[&branch]).await.unwrap().get(0);
    let currency:i64=client.query_one("SELECT res_id FROM ir_model_data WHERE module='base' AND model='res.currency' ORDER BY name LIMIT 1",&[]).await.unwrap().get(0);
    client
        .execute(
            "UPDATE res_currency SET symbol='TENANT' WHERE id=$1",
            &[&currency],
        )
        .await
        .unwrap();
    let language:i64=client.query_one("SELECT res_id FROM ir_model_data WHERE module='base' AND model='res.lang' ORDER BY name LIMIT 1",&[]).await.unwrap().get(0);
    client
        .execute(
            "UPDATE res_lang SET date_format='%Y.%m.%d' WHERE id=$1",
            &[&language],
        )
        .await
        .unwrap();
    let previous = dir.path().join("previous");
    fs::create_dir(&previous).unwrap();
    let previous = fs::canonicalize(previous).unwrap();
    runtime_addons::stage_runtime_addons(&old.join("addons"), &previous.join("addons")).unwrap();
    let mut identities = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    for row in client.query("SELECT name,content_hash FROM installed_addon_identity WHERE source_kind <> 'package' ORDER BY name",&[]).await.unwrap() {
        let name:String=row.get(0);
        let snapshot=erp_modules::artifact_identity::ArtifactSnapshot::capture(&name,&previous.join("addons").join(&name)).unwrap();
        assert_eq!(snapshot.digest(),row.get::<_,String>(1),"old installed receipt differs for {name}");
        for (path,bytes) in snapshot.files() {hashes.insert(format!("addons/{name}/{path}"),erp_modules::package::sha256_hex(bytes));}
        identities.insert(name,snapshot.digest().to_owned());
    }
    fs::write(
        previous.join("REVISION"),
        "b6780f923109f3a8212de11f276952cd9f0bfd73",
    )
    .unwrap();
    fs::write(previous.join("PACKAGE.json"),serde_json::to_vec(&serde_json::json!({"source":"b6780f923109f3a8212de11f276952cd9f0bfd73","sha256":hashes,"addon_identities":identities})).unwrap()).unwrap();
    erp_cli::identity::release_identities(&previous).unwrap();
    let current = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons");
    let current_bin = Path::new(env!("CARGO_BIN_EXE_erp"));
    let old_currency_count: i64 = client
        .query_one("SELECT count(*) FROM res_currency", &[])
        .await
        .unwrap()
        .get(0);
    let old_language_count: i64 = client
        .query_one("SELECT count(*) FROM res_lang", &[])
        .await
        .unwrap()
        .get(0);
    let old_currency_flag: bool = client.query_one("SELECT noupdate FROM ir_model_data WHERE module='base' AND model='res.currency' AND res_id=$1", &[&currency]).await.unwrap().get(0);
    assert!(
        !old_currency_flag,
        "old writable seed must exercise the flag transition"
    );
    client.batch_execute("CREATE FUNCTION fixture_refuse_company_partner() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.name='Legacy branch' THEN RAISE EXCEPTION 'fixture blocked representative' USING ERRCODE='23514'; END IF; RETURN NEW; END $$; CREATE TRIGGER fixture_refuse_company_partner BEFORE INSERT ON res_partner FOR EACH ROW EXECUTE FUNCTION fixture_refuse_company_partner()").await.unwrap();
    let refused = run(
        current_bin,
        &[
            "module",
            "upgrade",
            "base",
            "-d",
            db.name(),
            "--addons-path",
            current.to_str().unwrap(),
            "--identity-previous",
            previous.to_str().unwrap(),
        ],
        &db,
    );
    eprintln!(
        "step=injected-migration-rollback status={} stderr={}",
        refused.status,
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !refused.status.success(),
        "injected representative failure must abort upgrade"
    );
    let failure = String::from_utf8_lossy(&refused.stderr);
    assert!(
        failure.contains("fixture blocked representative"),
        "injected trigger must be the cause: {failure}"
    );
    assert!(
        failure.contains("stage post") && failure.contains("migrate_base_1_1"),
        "must reach the late company Post migration: {failure}"
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
    assert!(!client.query_one("SELECT noupdate FROM ir_model_data WHERE module='base' AND model='res.currency' AND res_id=$1", &[&currency]).await.unwrap().get::<_,bool>(0), "pre-migration flag must roll back too");
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM res_currency", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        old_currency_count
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM res_lang", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        old_language_count
    );
    assert_eq!(
        client
            .query_one("SELECT symbol FROM res_currency WHERE id=$1", &[&currency])
            .await
            .unwrap()
            .get::<_, String>(0),
        "TENANT"
    );
    assert_eq!(
        client
            .query_one("SELECT date_format FROM res_lang WHERE id=$1", &[&language])
            .await
            .unwrap()
            .get::<_, String>(0),
        "%Y.%m.%d",
        "language customization must roll back intact"
    );
    assert!(client.query_one("SELECT NOT EXISTS(SELECT 1 FROM information_schema.columns WHERE table_name='res_company' AND column_name='partner_id')", &[]).await.unwrap().get::<_,bool>(0), "schema change must roll back");
    client.batch_execute("DROP TRIGGER fixture_refuse_company_partner ON res_partner; DROP FUNCTION fixture_refuse_company_partner()").await.unwrap();
    succeeded(
        run(
            current_bin,
            &[
                "module",
                "upgrade",
                "base",
                "-d",
                db.name(),
                "--addons-path",
                current.to_str().unwrap(),
                "--identity-previous",
                previous.to_str().unwrap(),
            ],
            &db,
        ),
        "new-base-upgrade",
    );
    assert_eq!(
        client
            .query_one("SELECT symbol FROM res_currency WHERE id=$1", &[&currency])
            .await
            .unwrap()
            .get::<_, String>(0),
        "TENANT",
        "first forward upgrade must preserve existing tenant currency"
    );
    assert_eq!(
        client
            .query_one("SELECT date_format FROM res_lang WHERE id=$1", &[&language])
            .await
            .unwrap()
            .get::<_, String>(0),
        "%Y.%m.%d",
        "first forward upgrade must preserve existing tenant language"
    );
    assert!(
        client
            .query_one("SELECT count(*) FROM res_currency", &[])
            .await
            .unwrap()
            .get::<_, i64>(0)
            > old_currency_count,
        "missing current defaults must still be added"
    );
    assert!(
        client
            .query_one("SELECT count(*) FROM res_lang", &[])
            .await
            .unwrap()
            .get::<_, i64>(0)
            > old_language_count,
        "missing current languages must still be added"
    );
    let version: String = client
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name='base'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let manifest = erp_modules::manifest::Manifest::from_toml(
        "base",
        &fs::read_to_string(current.join("base/manifest.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(version, manifest.version().to_string());
    let representative: i64 = client
        .query_one("SELECT partner_id FROM res_company WHERE id=$1", &[&branch])
        .await
        .unwrap()
        .get(0);
    assert_ne!(representative, contact);
    let row = client
        .query_one(
            "SELECT name,company_id,active,commercial_partner_id FROM res_partner WHERE id=$1",
            &[&representative],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "Legacy branch");
    assert_eq!(row.get::<_, i64>(1), branch);
    assert!(row.get::<_, bool>(2));
    assert_eq!(row.get::<_, i64>(3), representative);
    let row = client
        .query_one(
            "SELECT name,company_id,active FROM res_partner WHERE id=$1",
            &[&contact],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, String>(0), "Tenant unrelated customer");
    assert_eq!(row.get::<_, i64>(1), branch);
    assert!(row.get::<_, bool>(2));
    let count: i64 = client
        .query_one("SELECT count(*) FROM res_partner", &[])
        .await
        .unwrap()
        .get(0);
    // Forward migration deliberately retains old installed identities until explicit
    // operator activation. An ordinary repeat before publication must still refuse.
    let refused = run(
        current_bin,
        &[
            "module",
            "upgrade",
            "base",
            "-d",
            db.name(),
            "--addons-path",
            current.to_str().unwrap(),
        ],
        &db,
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("Addon 'base' installed artifact identity does not match available content")
    );
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
    private(&proof, &serde_json::to_string(&serde_json::json!({"source":candidate_source,"previous":"b6780f923109f3a8212de11f276952cd9f0bfd73","exit":0,"upgrade_before_activation":true})).unwrap());
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
                config.to_str().unwrap(),
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
    succeeded(
        run(
            current_bin,
            &[
                "module",
                "upgrade",
                "base",
                "-d",
                db.name(),
                "--addons-path",
                current.to_str().unwrap(),
            ],
            &db,
        ),
        "new-base-upgrade-repeat",
    );
    assert_eq!(
        client
            .query_one("SELECT partner_id FROM res_company WHERE id=$1", &[&branch])
            .await
            .unwrap()
            .get::<_, i64>(0),
        representative
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM res_partner", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        count
    );
    assert_eq!(
        client
            .query_one("SELECT symbol FROM res_currency WHERE id=$1", &[&currency])
            .await
            .unwrap()
            .get::<_, String>(0),
        "TENANT"
    );
    assert_eq!(
        client
            .query_one("SELECT date_format FROM res_lang WHERE id=$1", &[&language])
            .await
            .unwrap()
            .get::<_, String>(0),
        "%Y.%m.%d"
    );
    assert_eq!(
        original_currency_ids::snapshot(&client).await,
        original_currency_ids
    );
    let writable:i64=client.query_one("SELECT count(*) FROM ir_model_data WHERE module='base' AND model IN ('res.currency','res.lang') AND NOT noupdate",&[]).await.unwrap().get(0);
    assert_eq!(writable, 0);
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
