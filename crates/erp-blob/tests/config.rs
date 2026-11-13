//! The configuration of the store: bounds, unknown keys and the kinds that
//! are not built yet fail naming the setting or the task.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_blob::{AttachmentConfig, AttachmentRuntime, StoreConfig};

const GOOD: &str = r#"
[server]
anything = "ignored"

[attachments]
write_store = "main"
inline_max_bytes = 4096
gc_grace_secs = 7200

[attachments.stores.main]
kind = "local"
path = "/var/lib/erp/attachments"
"#;

fn error(text: &str) -> String {
    AttachmentConfig::from_toml(text).unwrap_err().to_string()
}

#[test]
fn a_good_configuration_parses_with_defaults_and_overrides() {
    let config = AttachmentConfig::from_toml(GOOD).unwrap();
    assert_eq!(config.write_store, "main");
    assert_eq!(config.limits.inline_max_bytes, 4096);
    assert_eq!(config.limits.gc_grace_secs, 7200);
    assert_eq!(config.limits.max_file_bytes, 128 * 1024 * 1024);
    assert_eq!(
        config.stores.get("main"),
        Some(&StoreConfig::Local {
            path: "/var/lib/erp/attachments".into(),
            shared_filesystem: false
        })
    );
}

#[test]
fn missing_and_unknown_settings_fail_naming_the_key() {
    assert!(
        error("").starts_with("attachment store configuration: the [attachments] table is missing")
    );
    assert_eq!(
        error("[attachments]\n"),
        "attachment store configuration: attachments.write_store is not set"
    );
    assert_eq!(
        error("[attachments]\nwrite_store = \"x\"\n"),
        "attachment store configuration: attachments.write_store = \"x\" names no store in attachments.stores"
    );
    assert!(
        error("[attachments]\nwrite_store = \"x\"\nlocation = \"db\"\n")
            .contains("unknown field `location`")
    );
}

#[test]
fn kinds_that_are_not_built_name_their_task() {
    let with_kind = |kind: &str| {
        format!(
            "[attachments]\nwrite_store = \"a\"\n[attachments.stores.a]\nkind = \"{kind}\"\npath = \"/x\"\n"
        )
    };
    assert_eq!(
        error(&with_kind("s3")),
        "attachment store configuration: attachments.stores.a.bucket is not set"
    );
    assert_eq!(
        error(&with_kind("postgres")),
        "attachment store configuration: attachments.stores.a.kind = \"postgres\" is not built yet: it is the BASE-B postgres-store follow-up"
    );
    assert!(error(&with_kind("azure")).contains("a later release"));
    assert!(error(&with_kind("ftp")).contains("is unknown (local, s3, postgres)"));
}

#[test]
fn encryption_and_scanning_are_refused_until_their_tasks() {
    let base = "[attachments]\nwrite_store = \"a\"\n";
    let store = "[attachments.stores.a]\nkind = \"local\"\npath = \"/x\"\n";
    assert_eq!(
        error(&format!("{base}encryption = \"on\"\n{store}")),
        "attachment store configuration: attachments.encryption = \"on\" is not built yet: it needs a later release"
    );
    assert!(
        error(&format!("{base}scan = {{ mode = \"block\" }}\n{store}"))
            .contains("attachments.scan.mode other than \"off\"")
    );
    // a shared filesystem is the operator's statement and is accepted
    let shared =
        AttachmentConfig::from_toml(&format!("{base}{store}shared_filesystem = true\n")).unwrap();
    assert_eq!(
        shared.stores.get("a"),
        Some(&StoreConfig::Local {
            path: "/x".into(),
            shared_filesystem: true
        })
    );
}

#[test]
fn values_outside_their_bounds_fail_naming_the_key() {
    let store = "[attachments.stores.a]\nkind = \"local\"\npath = \"/x\"\n";
    let with = |line: &str| format!("[attachments]\nwrite_store = \"a\"\n{line}\n{store}");
    assert_eq!(
        error(&with("gc_grace_secs = 60")),
        "attachment store configuration: attachments.gc_grace_secs = 60 is outside 3600 to 2592000"
    );
    assert_eq!(
        error(&with("inline_max_bytes = 70000")),
        "attachment store configuration: attachments.inline_max_bytes = 70000 is outside 0 to 65536"
    );
    assert!(
        error(&with("max_file_bytes = 10")).contains("attachments.max_file_bytes = 10 is outside")
    );
}

#[test]
fn a_relative_store_path_is_refused() {
    let text = "[attachments]\nwrite_store = \"a\"\n[attachments.stores.a]\nkind = \"local\"\npath = \"rel/dir\"\n";
    assert_eq!(
        error(text),
        "attachment store configuration: attachments.stores.a.path must be an absolute directory"
    );
}

#[tokio::test]
async fn opening_builds_the_stores_and_names_the_write_store() {
    let dir = tempfile::tempdir().unwrap();
    let text = format!(
        "[attachments]\nwrite_store = \"main\"\n[attachments.stores.main]\nkind = \"local\"\npath = \"{}\"\n",
        dir.path().join("files").display()
    );
    let runtime = AttachmentRuntime::open(AttachmentConfig::from_toml(&text).unwrap())
        .await
        .unwrap();
    assert_eq!(runtime.write_store_key(), "main");
    assert!(runtime.write_store().is_ok());
    assert_eq!(runtime.store_keys().collect::<Vec<_>>(), ["main"]);
}

#[test]
fn an_s3_store_needs_bucket_and_region_and_an_https_endpoint() {
    let s3 = |extra: &str| {
        format!(
            "[attachments]\nwrite_store = \"s\"\n[attachments.stores.s]\nkind = \"s3\"\nbucket = \"b\"\nregion = \"r\"\n{extra}"
        )
    };
    let ok = AttachmentConfig::from_toml(&s3("prefix = \"erp/prod\"\n")).unwrap();
    assert!(
        matches!(ok.stores.get("s"), Some(StoreConfig::S3(settings)) if settings.prefix == "erp/prod")
    );
    assert_eq!(
        error(&s3("endpoint = \"http://storage.example.com\"\n")),
        "attachment store configuration: attachments.stores.s.endpoint must be https; plain http is allowed only for a loopback address"
    );
    // a loopback development endpoint may use http
    assert!(AttachmentConfig::from_toml(&s3("endpoint = \"http://127.0.0.1:9000\"\n")).is_ok());
    assert_eq!(
        error(
            "[attachments]\nwrite_store = \"s\"\n[attachments.stores.s]\nkind = \"s3\"\nbucket = \"b\"\n"
        ),
        "attachment store configuration: attachments.stores.s.region is not set"
    );
}

#[tokio::test]
async fn a_local_store_without_the_shared_flag_is_listed_as_unshared() {
    let dir = tempfile::tempdir().unwrap();
    let text = |flag: bool| {
        format!(
            "[attachments]\nwrite_store = \"m\"\n[attachments.stores.m]\nkind = \"local\"\npath = \"{}\"\nshared_filesystem = {flag}\n",
            dir.path().join("f").display()
        )
    };
    let alone = AttachmentRuntime::open(AttachmentConfig::from_toml(&text(false)).unwrap())
        .await
        .unwrap();
    assert_eq!(alone.unshared_local_stores(), ["m"]);
    let shared = AttachmentRuntime::open(AttachmentConfig::from_toml(&text(true)).unwrap())
        .await
        .unwrap();
    assert!(shared.unshared_local_stores().is_empty());
}

/// The per-node stream admission: a full runtime refuses at once with the typed, retryable busy
/// error (no queue), and a permit returns when it is dropped, also on an early error.
#[tokio::test]
async fn a_full_stream_admission_answers_busy_and_permits_return_on_drop() {
    let limits = erp_blob::AttachmentLimits::default().with_stream_permits_for_tests(2);
    let rt = AttachmentRuntime::single(
        "main",
        std::sync::Arc::new(erp_blob::MemoryStore::new()),
        limits,
    );
    assert_eq!(rt.available_stream_permits(), 2);
    let first = rt.try_stream_permit().unwrap();
    let second = rt.try_stream_permit().unwrap();
    assert_eq!(rt.available_stream_permits(), 0);
    let busy = rt.try_stream_permit().unwrap_err();
    assert!(matches!(busy, erp_blob::BlobError::Busy { permits: 2 }));
    assert!(busy.is_retryable());
    assert_eq!(
        busy.to_string(),
        "every attachment stream of this node is in use (attachments.stream_permits_per_node = 2, \
         per node): retry shortly"
    );
    drop(first);
    assert_eq!(rt.available_stream_permits(), 1);
    // an operation that fails after taking its permit gives it back
    let failing = || -> Result<(), erp_blob::BlobError> {
        let _permit = rt.try_stream_permit()?;
        Err(erp_blob::BlobError::InvalidRange { total: 0 })
    };
    assert!(failing().is_err());
    assert_eq!(rt.available_stream_permits(), 1);
    // a cancelled future holding a permit gives it back too
    let held = async {
        let _permit = rt.try_stream_permit().unwrap();
        std::future::pending::<()>().await;
    };
    let cancelled = tokio::time::timeout(std::time::Duration::from_millis(10), held).await;
    assert!(cancelled.is_err());
    assert_eq!(rt.available_stream_permits(), 1);
    drop(second);
    assert_eq!(rt.available_stream_permits(), 2);
}

#[test]
fn the_stream_bound_is_configured_per_node_within_its_bounds() {
    let config = AttachmentConfig::from_toml(&format!("{GOOD}\n")).unwrap();
    assert_eq!(config.limits.stream_permits_per_node, 32, "the default");
    let text = GOOD.replace(
        "gc_grace_secs = 7200",
        "gc_grace_secs = 7200\nstream_permits_per_node = 8",
    );
    assert_eq!(
        AttachmentConfig::from_toml(&text)
            .unwrap()
            .limits
            .stream_permits_per_node,
        8
    );
    let text = GOOD.replace(
        "gc_grace_secs = 7200",
        "gc_grace_secs = 7200\nstream_permits_per_node = 0",
    );
    assert_eq!(
        AttachmentConfig::from_toml(&text).unwrap_err().to_string(),
        "attachment store configuration: attachments.stream_permits_per_node = 0 is outside 1 to 1024"
    );
}

#[test]
fn quota_settings_have_safe_defaults_and_reject_invalid_bounds() {
    let config = AttachmentConfig::from_toml(GOOD).unwrap();
    assert_eq!(config.limits.quota_bytes, None);
    assert_eq!(config.limits.staging_max_bytes, 4 * 1024 * 1024 * 1024);
    let with = |setting: &str| GOOD.replace("inline_max_bytes = 4096", setting);
    let config =
        AttachmentConfig::from_toml(&with("quota_bytes = 1048576\nstaging_max_bytes = 2097152"))
            .unwrap();
    assert_eq!(config.limits.quota_bytes, Some(1_048_576));
    assert_eq!(config.limits.staging_max_bytes, 2_097_152);
    // TOML parser wording varies by input type/version; pin Configuration and the setting
    // identity here, while the deterministic budget refusals have full-message assertions.
    for (setting, key) in [
        ("quota_bytes = 0", "quota_bytes"),
        ("quota_bytes = 1048575", "quota_bytes"),
        ("staging_max_bytes = 0", "staging_max_bytes"),
        ("staging_max_bytes = 1048575", "staging_max_bytes"),
        (
            "pending_uploads_per_subject = 101",
            "pending_uploads_per_subject",
        ),
        ("quota_bytes = -1", "quota_bytes"),
        ("staging_max_bytes = 'huge'", "staging_max_bytes"),
        ("quota_bytes = 18446744073709551616", "quota_bytes"),
    ] {
        let err = AttachmentConfig::from_toml(&with(setting)).unwrap_err();
        assert!(matches!(err, erp_blob::BlobError::Configuration { .. }));
        assert!(err.to_string().contains(key), "{setting}: {err}");
    }
}

#[test]
fn receive_timeouts_are_validated_independently_of_intent_lifetime() {
    let with = |setting: &str| GOOD.replace("inline_max_bytes = 4096", setting);
    for setting in [
        "upload_idle_timeout_secs = 1",
        "upload_idle_timeout_secs = 300",
        "upload_receive_timeout_secs = 10",
        "upload_receive_timeout_secs = 86400",
    ] {
        AttachmentConfig::from_toml(&with(setting)).expect("valid receive setting");
    }
    for (key, value, low, high) in [
        ("upload_idle_timeout_secs", 0, 1, 300),
        ("upload_idle_timeout_secs", 301, 1, 300),
        ("upload_receive_timeout_secs", 9, 10, 86400),
        ("upload_receive_timeout_secs", 86401, 10, 86400),
    ] {
        let error = AttachmentConfig::from_toml(&with(&format!("{key} = {value}"))).unwrap_err();
        assert!(matches!(error, erp_blob::BlobError::Configuration { .. }));
        assert_eq!(
            error.to_string(),
            format!(
                "attachment store configuration: attachments.{key} = {value} is outside {low} to {high}"
            )
        );
    }
}

#[test]
fn quota_error_sources_never_render_private_accounting() {
    let error = erp_blob::BlobError::QuotaExceeded {
        budget: "database quota",
        used: 123456,
        requested: 789,
        limit: 123457,
    };
    assert_eq!(
        error.to_string(),
        "This file cannot be uploaded because the attachment storage limit has been reached. Contact an administrator."
    );
    assert!(matches!(
        error,
        erp_blob::BlobError::QuotaExceeded {
            used: 123456,
            requested: 789,
            limit: 123457,
            ..
        }
    ));
}
