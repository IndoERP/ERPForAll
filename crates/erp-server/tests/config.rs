#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
use erp_server::config::ServerConfig;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
fn fixture() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    for (name, value) in [
        (
            "runtime-url",
            "postgresql://runtime:dummy-password@127.0.0.1:5548/erpforall_human\n",
        ),
        (
            "owner-url",
            "postgresql://owner:dummy-owner@127.0.0.1:5548/erpforall_human\n",
        ),
        (
            "proxy-token",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        ),
    ] {
        let path = dir.path().join(name);
        fs::write(&path, value).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let config = "database='erpforall_human'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\nbind='127.0.0.1:18069'\npublic_origin='https://erp.example.test'\nproxy_mode='authenticated'\ntrusted_proxies=['127.0.0.1/32','::1/128']\nproxy_token_file='proxy-token'\naddons_path=['addons']\nassets_manifest='manifest.json'\n".to_owned();
    (dir, config)
}
#[test]
fn config_requires_protected_files_and_never_formats_credentials() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, &text).unwrap();
    let config = ServerConfig::load(&path).unwrap();
    assert_eq!(config.database(), "erpforall_human");
    assert_eq!(config.bind().unwrap().port(), 18069);
    assert!(!format!("{config:?}").contains("dummy"));
    fs::set_permissions(
        dir.path().join("proxy-token"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(ServerConfig::load(&path).is_err());
    fs::set_permissions(
        dir.path().join("proxy-token"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    fs::rename(
        dir.path().join("runtime-url"),
        dir.path().join("actual-runtime-url"),
    )
    .unwrap();
    symlink("actual-runtime-url", dir.path().join("runtime-url")).unwrap();
    assert!(ServerConfig::load(&path).is_err());
}
#[test]
fn config_refuses_implicit_proxy_mode_unknown_fields_and_wrong_database() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    for invalid in [
        text.replace("proxy_mode='authenticated'\n", ""),
        format!("{text}unknown=1\n"),
        text.replace("database='erpforall_human'", "database='other'"),
    ] {
        fs::write(&path, invalid).unwrap();
        assert!(ServerConfig::load(&path).is_err());
    }
}

#[test]
fn config_refuses_distinct_owner_and_runtime_endpoints() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, text).unwrap();
    for owner in [
        "postgresql://owner:dummy-owner@127.0.0.1:5549/erpforall_human\n",
        "postgresql://owner:dummy-owner@localhost:5548/erpforall_human\n",
    ] {
        fs::write(dir.path().join("owner-url"), owner).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(!error.to_string().contains("dummy"));
    }
}

/// PERF.SERVER-IDLE-KEEPALIVE: connection limits default to 5/30/120 s, are configurable, and an
/// invalid value or order refuses the configuration.
#[test]
fn config_connection_limits_default_override_and_refuse() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, &text).unwrap();
    let limits = ServerConfig::load(&path).unwrap().connection_limits();
    assert_eq!(limits, erp_server::conn::ConnectionLimits::default());
    fs::write(
        &path,
        format!("{text}http_header_deadline_secs=4\nhttp_keep_alive_idle_secs=105\nhttp_max_connection_age_secs=600\n"),
    )
    .unwrap();
    let limits = ServerConfig::load(&path).unwrap().connection_limits();
    assert_eq!(
        (
            limits.header_deadline().as_secs(),
            limits.idle().as_secs(),
            limits.max_age().as_secs()
        ),
        (4, 105, 600)
    );
    for invalid in [
        "http_header_deadline_secs=0\n",
        "http_keep_alive_idle_secs=601\n",
        "http_keep_alive_idle_secs=200\n",
        "http_max_connection_age_secs=20\n",
        "http_keep_alive_idle_secs=-1\n",
    ] {
        fs::write(&path, format!("{text}{invalid}")).unwrap();
        assert!(ServerConfig::load(&path).is_err(), "{invalid}");
    }
}

#[test]
fn startup_limits_defaults_and_valid_overrides() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, &text).unwrap();
    let config = ServerConfig::load(&path).unwrap();
    assert_eq!(config.startup_limits().max_connections(), 8192);
    assert_eq!(config.startup_limits().inflight_bytes(), 512 * 1024 * 1024);
    assert_eq!(config.node_name(), "erp-server");
    assert_eq!(config.shutdown_deadline().as_secs(), 45);
    fs::write(&path, format!("{text}node_name='app-2'\nshutdown_deadline_secs=5\n[startup_limits]\n\"http.max_connections\"=64\n\"buffers.thread_cache_bytes\"=0\n\"admission.inflight_bytes\"=88080384\n")).unwrap();
    let config = ServerConfig::load(&path).unwrap();
    assert_eq!(config.startup_limits().max_connections(), 64);
    assert_eq!(config.startup_limits().thread_cache_bytes(), 0);
    assert_eq!(config.startup_limits().inflight_bytes(), 88_080_384);
    assert_eq!(config.node_name(), "app-2");
    assert_eq!(config.shutdown_deadline().as_secs(), 5);
}

#[test]
fn startup_limits_reject_zero_out_of_bounds_and_unknowns() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    for name in [
        "http.max_connections",
        "http.keepalive_idle_secs",
        "http.header_read_secs",
        "http.body_read_secs",
        "http.body_min_bytes_per_sec",
        "http.response_min_bytes_per_sec",
        "db.cancel_concurrency",
        "limits.header_bytes",
        "limits.header_count",
        "admission.inflight_bytes",
        "assets.memory_bytes",
        "rpc.spec_cache_bytes",
        "limits.rate_table_entries",
        "session.touch_queue_entries",
        "ws.read_buffer_bytes",
        "admission.ws_inflight_bytes",
        "ws.message_read_secs",
        "ws.min_bytes_per_sec",
        "session.touch_interval_secs",
    ] {
        fs::write(&path, format!("{text}[startup_limits]\n\"{name}\"=0\n")).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(
            matches!(error, erp_server::config::ConfigError::Limit(_)),
            "{name}: {error}"
        );
        assert_eq!(
            error.to_string(),
            format!("server startup limit {name} is outside its bounds")
        );
    }
    for invalid in [
        "\"http.max_connections\"=1048577",
        "\"buffers.thread_cache_bytes\"=268435457",
        "\"http.body_read_secs\"=601",
        "\"limits.header_count\"=1001",
        "\"session.touch_interval_secs\"=3601",
        "\"limits.header_bytes\"=8191",
        "\"assets.memory_bytes\"=16777215",
    ] {
        fs::write(&path, format!("{text}[startup_limits]\n{invalid}\n")).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(matches!(error, erp_server::config::ConfigError::Limit(_)));
        let name = invalid.split('"').nth(1).unwrap();
        assert_eq!(
            error.to_string(),
            format!("server startup limit {name} is outside its bounds")
        );
    }
    fs::write(&path, format!("{text}[startup_limits]\nunknown=1\n")).unwrap();
    let error = ServerConfig::load(&path).unwrap_err();
    assert!(matches!(error, erp_server::config::ConfigError::Invalid));
    assert_eq!(
        error.to_string(),
        "server configuration or protected secret file is invalid"
    );
    for invalid in [
        "\"http.max_connections\"='unlimited'",
        "\"http.max_connections\"=18446744073709551615",
    ] {
        fs::write(&path, format!("{text}[startup_limits]\n{invalid}\n")).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(matches!(error, erp_server::config::ConfigError::Invalid));
        assert_eq!(
            error.to_string(),
            "server configuration or protected secret file is invalid"
        );
    }
}

#[test]
fn startup_limits_refuse_inflight_and_websocket_conflicts() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    for (settings, expected) in [
        (
            "\"admission.inflight_bytes\"=88080383",
            "server startup limits require admission.inflight_bytes >= 2 * limits.rpc_body_bytes + limits.rpc_result_bytes + admission.ws_inflight_bytes",
        ),
        (
            "\"admission.ws_inflight_bytes\"=1048575",
            "server startup limits require admission.ws_inflight_bytes >= ws.max_message_bytes",
        ),
        (
            "\"process.memory_soft_bytes\"=268435456",
            "server startup limits require admission.inflight_bytes <= process.memory_soft_bytes",
        ),
        (
            "\"limits.rpc_body_bytes\"=9223372036854775807",
            "server startup limit limits.rpc_body_bytes is outside its bounds",
        ),
    ] {
        fs::write(&path, format!("{text}[startup_limits]\n{settings}\n")).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(matches!(error, erp_server::config::ConfigError::Limit(_)));
        assert_eq!(error.to_string(), expected);
    }
}

#[test]
fn node_connection_requires_distinct_protected_role() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, &text).unwrap();
    let initial = ServerConfig::load(&path).unwrap();
    assert!(initial.configured_node_connection().is_none());
    let error = initial.node_connection().unwrap_err();
    assert!(matches!(
        error,
        erp_server::config::ConfigError::NodeConnection
    ));
    assert_eq!(
        error.to_string(),
        "server requires node_database_url_file with a dedicated heartbeat role"
    );
    let node_url = dir.path().join("node-url");
    fs::write(
        &node_url,
        "postgresql://heartbeat:dummy-node@127.0.0.1:5548/erpforall_human\n",
    )
    .unwrap();
    fs::set_permissions(&node_url, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, format!("{text}node_database_url_file='node-url'\n")).unwrap();
    let config = ServerConfig::load(&path).unwrap();
    assert_eq!(
        config.node_connection().unwrap().get_user(),
        Some("heartbeat")
    );
    assert_eq!(
        config.configured_node_connection().unwrap().get_user(),
        Some("heartbeat")
    );
    assert!(!format!("{config:?}").contains("dummy-node"));
    for invalid in [
        "postgresql://runtime:dummy-node@127.0.0.1:5548/erpforall_human",
        "postgresql://owner:dummy-node@127.0.0.1:5548/erpforall_human",
        "postgresql://heartbeat:dummy-node@127.0.0.1:5549/erpforall_human",
    ] {
        fs::write(&node_url, invalid).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(matches!(
            error,
            erp_server::config::ConfigError::NodeConnection
        ));
        assert_eq!(
            error.to_string(),
            "server requires node_database_url_file with a dedicated heartbeat role"
        );
    }
}

#[test]
fn lifecycle_options_refuse_invalid_values() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    for invalid in [
        "node_name=''",
        "node_name='a b'",
        "node_name='é'",
        "shutdown_deadline_secs=0",
        "shutdown_deadline_secs=3601",
    ] {
        fs::write(&path, format!("{text}{invalid}\n")).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(matches!(error, erp_server::config::ConfigError::Invalid));
        assert_eq!(
            error.to_string(),
            "server configuration or protected secret file is invalid"
        );
    }
}

#[test]
fn server_configuration_identity_distinguishes_shared_and_node_settings() {
    use erp_orm::node::SectionDigest;
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, &text).unwrap();
    let config = ServerConfig::load(&path).unwrap();
    let original = SectionDigest::of(&config).unwrap();
    fs::write(&path, text.replace("127.0.0.1:18069", "127.0.0.1:18070")).unwrap();
    let local = SectionDigest::of(&ServerConfig::load(&path).unwrap()).unwrap();
    assert_eq!(original.digest(), local.digest());
    fs::write(
        &path,
        text.replace("https://erp.example.test", "https://node-b.example.test"),
    )
    .unwrap();
    let policy = SectionDigest::of(&ServerConfig::load(&path).unwrap()).unwrap();
    assert_ne!(original.digest(), policy.digest());
    fs::write(&path, &text).unwrap();
    fs::write(dir.path().join("proxy-token"), "ab".repeat(32)).unwrap();
    let changed = SectionDigest::of(&ServerConfig::load(&path).unwrap()).unwrap();
    assert_ne!(original.digest(), changed.digest());
}

#[test]
fn startup_limits_reject_configured_reservations_above_soft_memory() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    // The inflight budget alone fits, but assets and the remaining configured reservations do not.
    fs::write(
        &path,
        format!("{text}[startup_limits]\n\"process.memory_soft_bytes\"=536870912\n"),
    )
    .unwrap();
    let error = ServerConfig::load(&path).unwrap_err();
    assert!(matches!(error, erp_server::config::ConfigError::Limit(_)));
    assert_eq!(
        error.to_string(),
        "server startup configured memory sum (http.max_connections, admission.inflight_bytes, admission.rpc_concurrency, assets.memory_bytes, buffers.thread_cache_bytes, rpc.spec_cache_bytes, limits.rate_table_entries, session.touch_queue_entries, admission.hash_concurrency, admission.hash_memory_kib) exceeds process.memory_soft_bytes"
    );
}

#[test]
fn startup_limits_accept_exact_configured_reservation_and_reject_one_byte_less() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, &text).unwrap();
    let initial = ServerConfig::load(&path).unwrap();
    let limits = initial.startup_limits();
    let total = limits.configured_reservation_bytes();
    assert_eq!(
        limits.hash_reserved_bytes(),
        erp_security::auth::HashPool::global()
            .config()
            .reserved_bytes()
    );
    assert!(total > limits.inflight_bytes());
    fs::write(
        &path,
        format!("{text}[startup_limits]\n\"process.memory_soft_bytes\"={total}\n"),
    )
    .unwrap();
    let boundary = ServerConfig::load(&path).unwrap();
    assert_eq!(
        boundary.startup_limits().configured_reservation_bytes(),
        total
    );
    assert_eq!(boundary.startup_limits().memory_soft_bytes(), total);
    fs::write(
        &path,
        format!(
            "{text}[startup_limits]\n\"process.memory_soft_bytes\"={}\n",
            total.checked_sub(1).unwrap()
        ),
    )
    .unwrap();
    let error = ServerConfig::load(&path).unwrap_err();
    assert!(matches!(error, erp_server::config::ConfigError::Limit(_)));
    assert_eq!(
        error.to_string(),
        "server startup configured memory sum (http.max_connections, admission.inflight_bytes, admission.rpc_concurrency, assets.memory_bytes, buffers.thread_cache_bytes, rpc.spec_cache_bytes, limits.rate_table_entries, session.touch_queue_entries, admission.hash_concurrency, admission.hash_memory_kib) exceeds process.memory_soft_bytes"
    );
}

#[test]
fn startup_limits_charge_each_configured_allowance_against_exact_soft_cap() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, &text).unwrap();
    let initial = ServerConfig::load(&path).unwrap();
    let limits = initial.startup_limits();
    let total = limits.configured_reservation_bytes();
    for (name, value) in [
        ("http.max_connections", limits.max_connections()),
        ("admission.inflight_bytes", limits.inflight_bytes()),
        ("assets.memory_bytes", limits.assets_memory_bytes()),
        ("buffers.thread_cache_bytes", limits.thread_cache_bytes()),
        ("rpc.spec_cache_bytes", limits.spec_cache_bytes()),
        ("limits.rate_table_entries", limits.rate_table_entries()),
        ("session.touch_queue_entries", limits.touch_queue_entries()),
    ] {
        fs::write(
            &path,
            format!(
                "{text}[startup_limits]\n\"process.memory_soft_bytes\"={total}\n\"{name}\"={}\n",
                value.checked_add(1).unwrap()
            ),
        )
        .unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(
            matches!(error, erp_server::config::ConfigError::Limit(_)),
            "{name}"
        );
        assert_eq!(
            error.to_string(),
            "server startup configured memory sum (http.max_connections, admission.inflight_bytes, admission.rpc_concurrency, assets.memory_bytes, buffers.thread_cache_bytes, rpc.spec_cache_bytes, limits.rate_table_entries, session.touch_queue_entries, admission.hash_concurrency, admission.hash_memory_kib) exceeds process.memory_soft_bytes",
            "{name}"
        );
    }
}
#[test]
fn attachments_default_to_a_local_store_next_to_the_configuration() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, &text).unwrap();
    let config = ServerConfig::load(&path).unwrap();
    let parsed: toml::Value = toml::from_str(config.attachments_toml()).unwrap();
    let store = &parsed["attachments"]["stores"]["main"];
    assert_eq!(parsed["attachments"]["write_store"].as_str(), Some("main"));
    assert_eq!(store["kind"].as_str(), Some("local"));
    assert_eq!(
        store["path"].as_str().map(std::path::Path::new),
        Some(dir.path().join("attachments").as_path())
    );
    // a table the operator wrote is used as it is, and its keys are the blob crate's to judge
    let own = format!(
        "{text}[attachments]\nwrite_store='disk'\n[attachments.stores.disk]\nkind='local'\npath='/srv/erp-files'\n"
    );
    fs::write(&path, &own).unwrap();
    let config = ServerConfig::load(&path).unwrap();
    assert_eq!(config.attachments_toml(), own);
}

#[test]
fn audit_reader_configuration_accepts_only_a_distinct_protected_endpoint() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, &text).unwrap();
    let initial = ServerConfig::load(&path).unwrap();
    assert!(initial.configured_audit_reader_connection().is_none());
    assert_eq!(initial.request_pool_size(), 8);
    let digest = erp_orm::node::SectionDigest::of(&initial).unwrap();
    let reader = dir.path().join("reader-url");
    fs::write(
        &reader,
        "postgresql://audit_reader:reader-canary@127.0.0.1:5548/erpforall_human\n",
    )
    .unwrap();
    fs::set_permissions(&reader, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &path,
        format!("{text}audit_reader_database_url_file='reader-url'\n"),
    )
    .unwrap();
    let config = ServerConfig::load(&path).unwrap();
    assert_eq!(
        config
            .configured_audit_reader_connection()
            .unwrap()
            .get_user(),
        Some("audit_reader")
    );
    assert_eq!(config.request_pool_size(), 7);
    assert!(!format!("{config:?}").contains("reader-canary"));
    assert_eq!(
        digest.digest(),
        erp_orm::node::SectionDigest::of(&config).unwrap().digest()
    );
    for url in [
        "postgresql://runtime:reader-canary@127.0.0.1:5548/erpforall_human",
        "postgresql://owner:reader-canary@127.0.0.1:5548/erpforall_human",
        "postgresql://audit_reader:reader-canary@127.0.0.1:5549/erpforall_human",
        "postgresql://audit_reader:reader-canary@localhost:5548/erpforall_human",
        "postgresql://audit_reader:reader-canary@127.0.0.1:5548/other",
        "reader-canary",
        "",
    ] {
        fs::write(&reader, url).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(matches!(
            error,
            erp_server::config::ConfigError::AuditReaderConnection
        ));
        assert!(!format!("{error:?}").contains("reader-canary"));
        assert_eq!(
            error.to_string(),
            "audit reader requires a protected connection to the same database with a distinct role"
        );
    }
}

#[test]
fn audit_reader_configuration_refuses_unprotected_or_mistyped_input() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    let reader = dir.path().join("reader-url");
    fs::write(
        &reader,
        "postgresql://audit_reader:reader-canary@127.0.0.1:5548/erpforall_human",
    )
    .unwrap();
    fs::set_permissions(&reader, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(
        &path,
        format!("{text}audit_reader_database_url_file='reader-url'\n"),
    )
    .unwrap();
    let error = ServerConfig::load(&path).unwrap_err();
    assert!(matches!(
        error,
        erp_server::config::ConfigError::AuditReaderConnection
    ));
    assert_eq!(
        error.to_string(),
        "audit reader requires a protected connection to the same database with a distinct role"
    );
    fs::set_permissions(&reader, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&reader, dir.path().join("actual-reader-url")).unwrap();
    symlink("actual-reader-url", &reader).unwrap();
    let error = ServerConfig::load(&path).unwrap_err();
    assert!(matches!(
        error,
        erp_server::config::ConfigError::AuditReaderConnection
    ));
    assert_eq!(
        error.to_string(),
        "audit reader requires a protected connection to the same database with a distinct role"
    );
    for bytes in [vec![b'x'; 4097], vec![0xff], Vec::new()] {
        fs::write(dir.path().join("actual-reader-url"), bytes).unwrap();
        // Keep testing the protected-file reader through a regular path, not the symlink.
        fs::write(
            &path,
            format!("{text}audit_reader_database_url_file='actual-reader-url'\n"),
        )
        .unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(matches!(
            error,
            erp_server::config::ConfigError::AuditReaderConnection
        ));
        assert_eq!(
            error.to_string(),
            "audit reader requires a protected connection to the same database with a distinct role"
        );
    }
    fs::write(&path, format!("{text}audit_reader_database_url_file=3\n")).unwrap();
    let error = ServerConfig::load(&path).unwrap_err();
    assert!(matches!(error, erp_server::config::ConfigError::Invalid));
    assert_eq!(
        error.to_string(),
        "server configuration or protected secret file is invalid"
    );
}

#[test]
fn audit_reader_configuration_refuses_heartbeat_role_reuse() {
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    for name in ["node-url", "reader-url"] {
        let secret = dir.path().join(name);
        fs::write(
            &secret,
            "postgresql://node:reader-canary@127.0.0.1:5548/erpforall_human",
        )
        .unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fs::write(
        &path,
        format!(
            "{text}node_database_url_file='node-url'\naudit_reader_database_url_file='reader-url'\n"
        ),
    )
    .unwrap();
    let error = ServerConfig::load(&path).unwrap_err();
    assert!(matches!(
        error,
        erp_server::config::ConfigError::AuditReaderConnection
    ));
    assert_eq!(
        error.to_string(),
        "audit reader requires a protected connection to the same database with a distinct role"
    );
}

#[test]
fn read_audit_policy_configuration_is_strict_and_additive() {
    use erp_rpc::ReadAuditPolicy;
    let (dir, text) = fixture();
    let path = dir.path().join("server.toml");
    fs::write(&path, format!("{text}audit_read_policy=true\n")).unwrap();
    let error = ServerConfig::load(&path).unwrap_err();
    assert!(matches!(
        error,
        erp_server::config::ConfigError::AuditReadPolicy(_)
    ));
    assert_eq!(error.to_string(), "audit_read_policy must be a table");
    for (suffix, want) in [
        ("", ReadAuditPolicy::default()),
        (
            "[audit_read_policy]\nall_users=false\n",
            ReadAuditPolicy::default(),
        ),
        (
            "[audit_read_policy]\nuser_groups=[]\n",
            ReadAuditPolicy::default(),
        ),
        (
            "[audit_read_policy]\nall_users=true\n",
            ReadAuditPolicy::all_users(),
        ),
        (
            "[audit_read_policy]\nuser_groups=['base.group_user']\n",
            ReadAuditPolicy::user_groups(&["base.group_user"]).unwrap(),
        ),
    ] {
        fs::write(&path, format!("{text}{suffix}")).unwrap();
        assert_eq!(
            ServerConfig::load(&path).unwrap().read_audit_policy(),
            &want
        );
    }
    for suffix in [
        "all_users=false\nuser_groups=[]\n",
        "all_users=true\nuser_groups=['base.group_user']\n",
        "",
    ] {
        fs::write(&path, format!("{text}[audit_read_policy]\n{suffix}")).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(matches!(
            error,
            erp_server::config::ConfigError::AuditReadPolicy(_)
        ));
        assert_eq!(
            error.to_string(),
            "audit_read_policy requires exactly one of all_users or user_groups"
        );
    }
    for (suffix, message) in [
        ("unknown=true\n", "audit_read_policy contains unknown keys"),
        (
            "all_users='true'\n",
            "audit_read_policy.all_users must be a boolean",
        ),
        (
            "user_groups='base.group_user'\n",
            "audit_read_policy.user_groups must be a list of positive group XML IDs",
        ),
        (
            "user_groups=[1]\n",
            "audit_read_policy.user_groups must be a list of positive group XML IDs",
        ),
    ] {
        fs::write(&path, format!("{text}[audit_read_policy]\n{suffix}")).unwrap();
        let error = ServerConfig::load(&path).unwrap_err();
        assert!(matches!(
            error,
            erp_server::config::ConfigError::AuditReadPolicy(_)
        ));
        assert_eq!(error.to_string(), message);
    }
    fs::write(
        &path,
        format!("{text}[audit_read_policy]\nuser_groups=['!base.group_user']\n"),
    )
    .unwrap();
    let error = ServerConfig::load(&path).unwrap_err();
    assert!(matches!(
        error,
        erp_server::config::ConfigError::AuditReadPolicy(_)
    ));
    assert_eq!(
        error.to_string(),
        "audit_read_policy.user_groups must contain positive qualified group XML IDs without expression syntax (maximum 256 UTF-8 bytes each)"
    );
}
