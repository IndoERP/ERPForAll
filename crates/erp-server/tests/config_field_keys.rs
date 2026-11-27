//! A process that already has field keys (from `erp --field-key-file`) loads a server
//! configuration only when the configuration names the same keys: the node settings record the
//! fingerprint of `field_key_file`, so keys from elsewhere would make two nodes with different
//! keys look alike. Installing keys is process-wide, hence this test binary of its own.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;

use erp_orm::field_encryption::{self, FieldKeys};
use erp_server::config::{ConfigError, ServerConfig};

/// Synthetic keys, never real ones.
const KEYS: &str = "test2026a AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=\n";
const OTHER_KEYS: &str = "test2026b HyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4=\n";

fn protected(dir: &Path, name: &str, value: &str) {
    let path = dir.join(name);
    fs::write(&path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

/// A minimal valid configuration in `dir`, plus `extra` lines.
fn config(dir: &Path, extra: &str) -> std::path::PathBuf {
    protected(
        dir,
        "runtime-url",
        "postgresql://runtime:dummy-password@127.0.0.1:5548/erpforall_keys\n",
    );
    protected(
        dir,
        "owner-url",
        "postgresql://owner:dummy-owner@127.0.0.1:5548/erpforall_keys\n",
    );
    protected(
        dir,
        "proxy-token",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
    );
    let path = dir.join("server.toml");
    fs::write(
        &path,
        format!(
            "database='erpforall_keys'\ndatabase_url_file='runtime-url'\n\
             owner_database_url_file='owner-url'\nbind='127.0.0.1:18069'\n\
             public_origin='https://erp.example.test'\nproxy_mode='authenticated'\n\
             trusted_proxies=['127.0.0.1/32']\nproxy_token_file='proxy-token'\n\
             addons_path=['addons']\nassets_manifest='manifest.json'\n{extra}"
        ),
    )
    .unwrap();
    path
}

#[test]
fn keys_installed_outside_the_configuration_must_be_named_by_it() {
    let dir = tempfile::tempdir().unwrap();
    // what `erp --field-key-file` does before the command loads its configuration
    field_encryption::install(FieldKeys::parse(KEYS).unwrap()).unwrap();

    // without field_key_file the node settings would say `none` while the ORM has keys
    let refused = ServerConfig::load(&config(dir.path(), "")).unwrap_err();
    assert!(
        matches!(refused, ConfigError::FieldKeysOutsideConfig),
        "{refused:?}"
    );
    assert!(
        refused
            .to_string()
            .contains("field_key_file in server.toml")
    );

    // a configuration naming another key set is refused by the installed keys
    protected(dir.path(), "other-keys", OTHER_KEYS);
    let other = ServerConfig::load(&config(dir.path(), "field_key_file='other-keys'\n"));
    assert!(matches!(other, Err(ConfigError::FieldKeys(_))), "{other:?}");

    // the same keys: the configuration records the fingerprint of the installed keys
    protected(dir.path(), "field-keys", KEYS);
    let loaded = ServerConfig::load(&config(dir.path(), "field_key_file='field-keys'\n")).unwrap();
    assert_eq!(
        loaded.field_keys().map(FieldKeys::fingerprint),
        field_encryption::installed().map(|keys| keys.fingerprint())
    );
}
