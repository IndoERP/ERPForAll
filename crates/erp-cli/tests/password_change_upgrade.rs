//! Opt-in genuine retained-release upgrade; never rewinds a current version marker.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration fixture assertions"
)]
#[path = "support/native_http.rs"]
mod native_http;
use erp_testkit::TestDb;
use std::{fs, path::Path, process::Command};

#[tokio::test]
#[ignore = "requires an explicitly selected genuine retained release on an approved build host"]
async fn retained_release_upgrade_preserves_credentials_and_is_repeatable() {
    let previous = std::path::PathBuf::from(
        std::env::var("ERP_PASSWORD_PREVIOUS_RELEASE")
            .expect("ERP_PASSWORD_PREVIOUS_RELEASE required"),
    )
    .canonicalize()
    .unwrap();
    erp_cli::identity::release_identities(&previous).unwrap();
    eprintln!(
        "previous_revision={} previous_binary_sha256={}",
        fs::read_to_string(previous.join("REVISION"))
            .unwrap()
            .trim(),
        erp_modules::package::sha256_hex(&fs::read(previous.join("bin/erp")).unwrap())
    );
    let db = TestDb::create().await.unwrap();
    let client = db.client().await.unwrap();
    let role = format!("{}_pwd_upgrade", db.name());
    client.batch_execute(&format!("CREATE ROLE {} LOGIN PASSWORD 'fixture-runtime-password' NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION", erp_sql::quote(&role))).await.unwrap();
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let config = native_http::config(dir.path(), &db, 18091, &role, &previous.join("addons"));
    // The retained release predates the separate node role; no server is started here.
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.lines()
            .filter(|line| !line.starts_with("node_database_url_file="))
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    native_http::private(&dir.path().join("password"), "preserve-existing-password");
    let initialized = Command::new(previous.join("bin/erp"))
        .args(["db", "init", "--config"])
        .arg(&config)
        .args(["--admin-login", "preserved-user", "--admin-password-file"])
        .arg(dir.path().join("password"))
        .env("DATABASE_URL", native_http::url(&db.config(), None))
        .output()
        .unwrap();
    assert!(
        initialized.status.success(),
        "old init: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let version: String = client
        .query_one(
            "SELECT latest_version FROM ir_module_module WHERE name='base'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        version.starts_with("1.1"),
        "expected genuine base 1.1, found {version}"
    );
    assert!(
        client
            .query_one("SELECT to_regclass('change_password_wizard') IS NULL", &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    let before = client
        .query_one(
            "SELECT id::bigint,password FROM res_users WHERE login='preserved-user'",
            &[],
        )
        .await
        .unwrap();
    let uid: i64 = before.get(0);
    let hash: String = before.get(1);
    let users: i64 = client
        .query_one("SELECT count(*) FROM res_users", &[])
        .await
        .unwrap()
        .get(0);
    let current = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("addons");
    for pass in 1..=2 {
        let mut command = Command::new(env!("CARGO_BIN_EXE_erp"));
        command
            .args([
                "module",
                "upgrade",
                "base",
                "web",
                "-d",
                db.name(),
                "--addons-path",
            ])
            .arg(&current)
            .env("DATABASE_URL", native_http::url(&db.config(), None));
        // Forward rehearsal retains old identities until separate operator activation.
        command.arg("--identity-previous").arg(&previous);
        let upgraded = command.output().unwrap();
        eprintln!(
            "upgrade_pass={pass} status={} stdout={} stderr={}",
            upgraded.status,
            String::from_utf8_lossy(&upgraded.stdout),
            String::from_utf8_lossy(&upgraded.stderr)
        );
        assert!(upgraded.status.success());
        let after = client
            .query_one(
                "SELECT id::bigint,password FROM res_users WHERE login='preserved-user'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(after.get::<_, i64>(0), uid);
        assert_eq!(after.get::<_, String>(1), hash);
        assert_eq!(
            client
                .query_one("SELECT count(*) FROM res_users", &[])
                .await
                .unwrap()
                .get::<_, i64>(0),
            users
        );
        assert!(client.query_one("SELECT to_regclass('change_password_wizard') IS NOT NULL AND EXISTS(SELECT 1 FROM ir_model_data WHERE module='base' AND name='change_password_wizard_view')", &[]).await.unwrap().get::<_, bool>(0));
    }
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
