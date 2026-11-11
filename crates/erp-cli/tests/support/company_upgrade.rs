//! Shared private runtime preparation for the company previous-binary rehearsal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test fixture"
)]
use erp_modules::AddonIndex;
use erp_testkit::{
    TestDb,
    tokio_postgres::{Client, config::Host},
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};
pub fn private(path: &Path, value: &str) {
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
        panic!("native fixture requires configured TCP test cluster")
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

pub fn config(directory: &Path, db: &TestDb, role: &str, runtime_addons: &Path) -> PathBuf {
    private(&directory.join("owner-url"), &url(&db.config(), None));
    private(
        &directory.join("runtime-url"),
        &url(&db.config(), Some(role)),
    );
    let text = format!(
        "database='{}'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nbind='127.0.0.1:18093'\npublic_origin='http://127.0.0.1:18093'\nproxy_mode='direct'\naddons_path=['{}']\nassets_manifest='manifest.json'\n",
        db.name(),
        runtime_addons.display()
    );
    fs::write(directory.join("server.toml"), text).unwrap();
    let digest = erp_modules::package::sha256_hex(b"");
    let assets: Vec<_> = [("js", "text/javascript; charset=utf-8"), ("css", "text/css; charset=utf-8")].into_iter().map(|(extension, content_type)| {
        let path = format!("client.{digest}.{extension}");
        fs::write(directory.join(&path), b"").unwrap();
        serde_json::json!({"name": format!("client.{extension}"), "path": path, "sha256": digest, "content_type": content_type})
    }).collect();
    fs::write(
        directory.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"schema": 1, "assets": assets})).unwrap(),
    )
    .unwrap();
    directory.join("server.toml")
}

pub fn run(binary: &Path, arguments: &[&str], db: &TestDb) -> Output {
    Command::new(binary)
        .args(arguments)
        .env("DATABASE_URL", url(&db.config(), None))
        .output()
        .unwrap()
}

pub fn succeeded(output: Output, step: &str) {
    eprintln!(
        "step={step} status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{step} failed");
}

pub async fn release_inventory(root: &Path, source: &str, installed_only: Option<&Client>) {
    let mut identities = BTreeMap::new();
    let mut hashes = BTreeMap::new();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(root.join("addons"));
    for (name, entry) in index.discover_entries().unwrap() {
        let snapshot = erp_modules::artifact_identity::ArtifactSnapshot::capture(
            name.as_str(),
            Path::new(entry.source_location()),
        )
        .unwrap();
        if let Some(client) = installed_only {
            let installed = client.query_opt("SELECT content_hash FROM installed_addon_identity WHERE name=$1 AND source_kind <> 'package'", &[&name.as_str()]).await.unwrap();
            let Some(installed) = installed else { continue };
            assert_eq!(
                snapshot.digest(),
                installed.get::<_, String>(0),
                "previous artifact {name} must match the actual installed identity"
            );
        }
        for (path, bytes) in snapshot.files() {
            hashes.insert(
                format!("addons/{name}/{path}"),
                erp_modules::package::sha256_hex(bytes),
            );
        }
        identities.insert(name.to_string(), snapshot.digest().to_owned());
    }
    fs::write(root.join("REVISION"), source).unwrap();
    fs::write(root.join("PACKAGE.json"), serde_json::to_vec(&serde_json::json!({"source": source, "sha256": hashes, "addon_identities": identities})).unwrap()).unwrap();
    erp_cli::identity::release_identities(root).unwrap();
}
