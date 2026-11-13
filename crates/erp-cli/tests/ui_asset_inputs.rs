//! Host-side declared asset collection and monotonic runtime admission.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]
use erp_addon_web::assets::assembly::{AssemblyOptions, assemble};
use erp_cli::ui_assets::{RUNTIME_ABI, RuntimeAdmission, collect_entries, runtime_admission};
use erp_fields::ErrorKind;
use erp_modules::{AddonEntry, AddonSourceKind, Manifest};
use erp_sdk::ui_compiler::{BuildMode, CompileOptions};
use std::{fs, path::Path, sync::Arc};

fn assert_validation_message(error: &erp_fields::Error, expected: &str) {
    assert_eq!(
        (error.kind(), error.message()),
        (ErrorKind::Validation, expected)
    );
}

// Only the operating-system errno display/number varies between supported Unix hosts. Assert
// the exact semantic errno separately; the entire relative path and message wrapper stay exact.
fn assert_os_read_error(error: &erp_fields::Error, path: &str, expected_errno: rustix::io::Errno) {
    assert_eq!(
        std::error::Error::source(error)
            .and_then(|source| source.downcast_ref::<rustix::io::Errno>()),
        Some(&expected_errno)
    );
    assert_validation_message(
        error,
        &format!("addon 'demo' asset '{path}': cannot read declared file: {expected_errno}"),
    );
}

const OLDER_RUNTIME_ERROR: &str = "published UI runtime 'elang-2' is newer than running runtime 'elang-1'; incompatible node cannot reassemble it";

fn entry(root: &Path, name: &str) -> AddonEntry {
    let manifest = Manifest::from_toml(name, &format!("name='{name}'\nversion='1.0'\n")).unwrap();
    AddonEntry::new(
        Arc::new(manifest),
        AddonSourceKind::DevFolder(root.to_owned()),
        root.display().to_string(),
    )
}
fn file(root: &Path, path: &str, bytes: &[u8]) {
    let target = root.join(path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(target, bytes).unwrap();
}
fn manifest(root: &Path, body: &str) {
    file(root, "static/dist/manifest.json", body.as_bytes());
}
fn options() -> AssemblyOptions {
    AssemblyOptions::new(
        "compiler-test",
        RUNTIME_ABI,
        "en_US",
        "main",
        CompileOptions::new(BuildMode::Prod),
    )
    .unwrap()
}

#[test]
fn declared_files_keep_addon_order_and_absent_manifest_means_no_client_declarations() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let c = tempfile::tempdir().unwrap();
    manifest(
        a.path(),
        &format!(
            r#"{{"schema":1,"runtime_abi":"{RUNTIME_ABI}","entry":"@first/main","modules":[{{"path":"static/dist/main.js","specifier":"@first/main"}}],"templates":[],"static_files":["static/icon.svg"]}}"#
        ),
    );
    file(
        a.path(),
        "static/dist/main.js",
        b"/*! NOTICE first */ export function register() {}\n",
    );
    file(a.path(), "static/icon.svg", b"<svg/>");
    manifest(
        b.path(),
        &format!(
            r#"{{"schema":1,"runtime_abi":"{RUNTIME_ABI}","entry":"@second/main","modules":[{{"path":"static/dist/main.js","specifier":"@second/main"}}],"templates":[],"static_files":[]}}"#
        ),
    );
    file(
        b.path(),
        "static/dist/main.js",
        b"/*! NOTICE second */ export function register() {}\n",
    );
    // Files without a declaration are never exposed, even if the path looks like an asset.
    file(c.path(), "static/hidden.svg", b"not declared");
    let first = entry(a.path(), "first");
    let second = entry(b.path(), "second");
    let absent = entry(c.path(), "absent");
    let inputs = collect_entries(&[&first, &second, &absent], RUNTIME_ABI).unwrap();
    assert_eq!(
        inputs.iter().map(|a| a.name()).collect::<Vec<_>>(),
        ["first", "second", "absent"]
    );
    let bundle = assemble(&inputs, &options()).unwrap();
    let register = std::str::from_utf8(bundle.register_module().bytes()).unwrap();
    assert!(register.find("@first/main").unwrap() < register.find("@second/main").unwrap());
    assert!(bundle.static_files().contains_key("/first/static/icon.svg"));
    assert!(
        !bundle
            .static_files()
            .contains_key("/absent/static/hidden.svg")
    );
}

#[test]
fn manifest_schema_is_strict_and_runtime_abi_is_not_defaulted() {
    let dir = tempfile::tempdir().unwrap();
    let addon = entry(dir.path(), "demo");
    for (body, expected) in [
        (
            r#"{"schema":1,"runtime_abi":"elang-1","modules":[],"templates":[],"static_files":[],"unknown":true}"#,
            "addon 'demo' asset 'static/dist/manifest.json': unknown field `unknown`, expected one of `schema`, `runtime_abi`, `entry`, `modules`, `templates`, `static_files` at line 1 column 91",
        ),
        (
            r#"{"schema":2,"runtime_abi":"elang-1","modules":[],"templates":[],"static_files":[]}"#,
            "addon 'demo' asset 'static/dist/manifest.json': unsupported schema 2",
        ),
        (
            r#"{"schema":1,"modules":[],"templates":[],"static_files":[]}"#,
            "addon 'demo' asset 'static/dist/manifest.json': missing field `runtime_abi` at line 1 column 58",
        ),
        (
            r#"{"schema":1,"runtime_abi":false,"modules":[],"templates":[],"static_files":[]}"#,
            "addon 'demo' asset 'static/dist/manifest.json': invalid type: boolean `false`, expected a string at line 1 column 31",
        ),
        (
            r#"{"schema":1,"runtime_abi":"elang-2","modules":[],"templates":[],"static_files":[]}"#,
            "addon 'demo' asset 'static/dist/manifest.json': runtime ABI 'elang-2' does not match running 'elang-1'",
        ),
    ] {
        manifest(dir.path(), body);
        let err = collect_entries(&[&addon], RUNTIME_ABI).unwrap_err();
        assert_validation_message(&err, expected);
    }
}

#[test]
fn declared_paths_are_normalized_bounded_and_must_exist() {
    let dir = tempfile::tempdir().unwrap();
    let addon = entry(dir.path(), "demo");
    for path in [
        "static/../secret.txt",
        "/static/icon.svg",
        "static//icon.svg",
        "static/./icon.svg",
        "static/a\\b.svg",
        "static/%2e%2e/secret.txt",
    ] {
        manifest(dir.path(),&serde_json::json!({"schema":1,"runtime_abi":RUNTIME_ABI,"modules":[],"templates":[],"static_files":[path]}).to_string());
        let err = collect_entries(&[&addon], RUNTIME_ABI).unwrap_err();
        assert_validation_message(
            &err,
            &format!(
                "addon 'demo' asset '{path}': expected a bounded normalized relative static path"
            ),
        );
    }
    manifest(dir.path(),&serde_json::json!({"schema":1,"runtime_abi":RUNTIME_ABI,"modules":[],"templates":[],"static_files":["static/missing.svg"]}).to_string());
    let err = collect_entries(&[&addon], RUNTIME_ABI).unwrap_err();
    assert_os_read_error(&err, "static/missing.svg", rustix::io::Errno::NOENT);
}

#[cfg(unix)]
#[test]
fn symlinks_at_file_directory_and_manifest_are_refused() {
    use std::os::unix::fs::symlink;
    let outside = tempfile::tempdir().unwrap();
    file(outside.path(), "secret.svg", b"<svg/>");
    for (case, path, errno) in [
        ("file", "static/link/secret.svg", rustix::io::Errno::LOOP),
        (
            "directory",
            "static/link/secret.svg",
            rustix::io::Errno::NOTDIR,
        ),
        (
            "manifest",
            "static/dist/manifest.json",
            rustix::io::Errno::LOOP,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let addon = entry(dir.path(), "demo");
        manifest(
            dir.path(),
            &format!(
                r#"{{"schema":1,"runtime_abi":"{RUNTIME_ABI}","modules":[],"templates":[],"static_files":["static/link/secret.svg"]}}"#
            ),
        );
        match case {
            "file" => {
                fs::create_dir_all(dir.path().join("static/link")).unwrap();
                symlink(
                    outside.path().join("secret.svg"),
                    dir.path().join("static/link/secret.svg"),
                )
                .unwrap();
            }
            "directory" => {
                symlink(outside.path(), dir.path().join("static/link")).unwrap();
            }
            "manifest" => {
                fs::remove_file(dir.path().join("static/dist/manifest.json")).unwrap();
                symlink(
                    outside.path().join("secret.svg"),
                    dir.path().join("static/dist/manifest.json"),
                )
                .unwrap();
            }
            _ => panic!("unexpected fixture"),
        }
        let err = collect_entries(&[&addon], RUNTIME_ABI).unwrap_err();
        assert_os_read_error(&err, path, errno);
    }
}

#[test]
fn only_monotonic_runtime_epoch_upgrade_is_admitted() {
    assert_eq!(
        runtime_admission("elang-1", "elang-1").unwrap(),
        RuntimeAdmission::Current
    );
    assert_eq!(
        runtime_admission("elang-1", "elang-2").unwrap(),
        RuntimeAdmission::Upgrade
    );
    let older = runtime_admission("elang-2", "elang-1").unwrap_err();
    assert_validation_message(&older, OLDER_RUNTIME_ERROR);
    for runtime in [
        "",
        "elang-0",
        "elang-01",
        "elang-4294967296",
        "other-1",
        "elang--1",
        "elang-1x",
    ] {
        let err = runtime_admission(runtime, "elang-1").unwrap_err();
        assert_validation_message(
            &err,
            &format!(
                "invalid UI runtime identity '{runtime}': expected elang-<positive u32> without leading zeros"
            ),
        );
    }
}

#[test]
fn verified_package_client_map_is_used_without_data_file_fallback() {
    use erp_modules::PackageData;
    use std::collections::BTreeMap;
    let meta = Manifest::from_toml("pkg", "name='Package'\nversion='1.0'\n").unwrap();
    let bytes=serde_json::json!({"schema":1,"runtime_abi":RUNTIME_ABI,"modules":[],"templates":[],"static_files":["static/icon.svg"]}).to_string().into_bytes();
    let mut client_files = BTreeMap::new();
    client_files.insert("static/dist/manifest.json".to_owned(), bytes);
    client_files.insert("static/icon.svg".to_owned(), b"<svg/>".to_vec());
    let data = PackageData {
        client_files,
        ..PackageData::default()
    };
    let entry = AddonEntry::new(
        Arc::new(meta),
        AddonSourceKind::Package {
            sha256: Some("a".repeat(64)),
        },
        "verified package",
    )
    .with_package_data(Arc::new(data));
    let inputs = collect_entries(&[&entry], RUNTIME_ABI).unwrap();
    let bundle = assemble(&inputs, &options()).unwrap();
    assert!(bundle.static_files().contains_key("/pkg/static/icon.svg"));
    let no_bytes = AddonEntry::new(
        entry.manifest().clone(),
        AddonSourceKind::Package {
            sha256: Some("a".repeat(64)),
        },
        "unavailable package",
    );
    let error = collect_entries(&[&no_bytes], RUNTIME_ABI).unwrap_err();
    assert_validation_message(
        &error,
        "addon 'pkg' asset 'static/dist/manifest.json': verified client files are unavailable; load the verified package before asset admission",
    );
}

#[test]
fn declared_file_and_manifest_admission_bounds_fail_loudly() {
    use erp_addon_web::assets::assembly::MAX_ASSET_BYTES;
    let dir = tempfile::tempdir().unwrap();
    let addon = entry(dir.path(), "demo");
    manifest(dir.path(),&serde_json::json!({"schema":1,"runtime_abi":RUNTIME_ABI,"modules":[],"templates":[],"static_files":["static/large.svg"]}).to_string());
    let large = fs::File::create(dir.path().join("static/large.svg")).unwrap();
    large.set_len((MAX_ASSET_BYTES as u64) + 1).unwrap();
    let error = collect_entries(&[&addon], RUNTIME_ABI).unwrap_err();
    assert_validation_message(
        &error,
        "addon 'demo' asset 'static/large.svg': file bytes exceed hard admission limit",
    );
    file(
        dir.path(),
        "static/dist/manifest.json",
        &vec![b' '; 1024 * 1024],
    );
    let error = collect_entries(&[&addon], RUNTIME_ABI).unwrap_err();
    assert_validation_message(
        &error,
        "addon 'demo' asset 'static/dist/manifest.json': file bytes exceed hard admission limit",
    );
}

fn web_source(root: &Path, runtime: &str, text: &[u8]) -> AddonEntry {
    manifest(root,&serde_json::json!({"schema":1,"runtime_abi":runtime,"entry":"@web/main","modules":[{"path":"static/dist/main.js","specifier":"@web/main"}],"templates":[],"static_files":[]}).to_string());
    file(root, "static/dist/main.js", text);
    entry(root, "web")
}
fn runtime_options(compiler: &str, runtime: &str) -> AssemblyOptions {
    AssemblyOptions::new(
        compiler,
        runtime,
        "en_US",
        "main",
        CompileOptions::new(BuildMode::Prod),
    )
    .unwrap()
}

// Standalone assembly fixtures still establish real signal storage and observe its durable
// state; startup has no default/unfenced baseline path, even when no module changes occur.
async fn fixture_registry_baseline(h: &erp_testkit::Harness) -> erp_orm::Generations {
    let mut tx = h.begin().await.unwrap();
    tx.install_signal().await.unwrap();
    tx.commit().await.unwrap();
    let client = h.db().client().await.unwrap();
    erp_orm::Signal::read(&client).await.unwrap()
}

/// The stand-in base of the test kit (manifest plus the records base's code needs to register).
fn base_fixture(root: &Path) {
    erp_testkit::stand_in_base::write_stand_in_base(root).unwrap();
}

#[tokio::test]
async fn startup_reassembles_changed_inputs_and_keeps_previous_generation_after_failure() {
    use erp_addon_web::assets::store;
    use erp_cli::ui_assets::{admit_startup, assemble_entries};
    let h = erp_testkit::Harness::builder([&erp_addon_base::ADDON, &erp_addon_web::ADDON])
        .without_schema()
        .build()
        .await
        .unwrap();
    let baseline = fixture_registry_baseline(&h).await;
    let dir = tempfile::tempdir().unwrap();
    let web = web_source(
        dir.path(),
        RUNTIME_ABI,
        b"/*! old */ export function register() {}",
    );
    let mut tx = h.begin().await.unwrap();
    let first = assemble_entries(&mut tx, &[&web], &options(), true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        admit_startup(h.orm(), &[&web], &options(), &baseline)
            .await
            .unwrap()
            .unwrap(),
        first
    );
    file(
        dir.path(),
        "static/dist/main.js",
        b"/*! changed */ export function register() {}",
    );
    let changed = admit_startup(h.orm(), &[&web], &options(), &baseline)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(changed.key(), first.key());
    manifest(dir.path(),&serde_json::json!({"schema":1,"runtime_abi":RUNTIME_ABI,"modules":[],"templates":["static/src/fail.xml"],"static_files":[]}).to_string());
    file(
        dir.path(),
        "static/src/fail.xml",
        br#"<templates><t t-name="web.Failure" t-inherit="web.Missing"/></templates>"#,
    );
    let error = admit_startup(h.orm(), &[&web], &options(), &baseline)
        .await
        .unwrap_err();
    assert_validation_message(
        &error,
        "UI asset assembly failed: UI template assembly failed: 1 template error(s)\n  error[missing_template] web/static/src/fail.xml:1:12 template web.Failure: template parent 'web.Missing' not found (t-inherit of 'web.Failure')",
    );
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        store::current(&mut tx, "en_US", "main", RUNTIME_ABI)
            .await
            .unwrap()
            .unwrap(),
        changed
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn startup_runtime_upgrade_is_monotonic_and_older_node_cannot_overwrite_it() {
    use erp_addon_web::assets::store;
    use erp_cli::ui_assets::{admit_startup, assemble_entries};
    let h = erp_testkit::Harness::builder([&erp_addon_base::ADDON, &erp_addon_web::ADDON])
        .without_schema()
        .build()
        .await
        .unwrap();
    let baseline = fixture_registry_baseline(&h).await;
    let dir = tempfile::tempdir().unwrap();
    let web = web_source(dir.path(), "elang-1", b"export function register() {}");
    let old = runtime_options("old-compiler", "elang-1");
    let mut tx = h.begin().await.unwrap();
    let first = assemble_entries(&mut tx, &[&web], &old, true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    web_source(dir.path(), "elang-2", b"export function register() {}");
    let next = runtime_options("next-compiler", "elang-2");
    let upgraded = admit_startup(h.orm(), &[&web], &next, &baseline)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(upgraded.runtime(), "elang-2");
    assert_ne!(upgraded.key(), first.key());
    let error = admit_startup(h.orm(), &[&web], &old, &baseline)
        .await
        .unwrap_err();
    assert_validation_message(&error, OLDER_RUNTIME_ERROR);
    let mut tx = h.begin().await.unwrap();
    assert_eq!(
        store::current(&mut tx, "en_US", "main", "elang-2")
            .await
            .unwrap()
            .unwrap(),
        upgraded
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

async fn published(orm: &erp_orm::Orm) -> erp_addon_web::assets::assembly::AssembledBundle {
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let bundle = erp_addon_web::assets::store::current(&mut tx, "en_US", "main", RUNTIME_ABI)
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    bundle
}
async fn installed_state(orm: &erp_orm::Orm, module: &str) -> String {
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query(
            "SELECT state FROM ir_module_module WHERE name=$1",
            &[&module],
        )
        .await
        .unwrap();
    let state = rows[0].get(0);
    tx.rollback().await.unwrap();
    state
}

/// Read the installed preimage from durable receipts, never from edited candidate files.
async fn artifact_receipts(
    orm: &erp_orm::Orm,
) -> (
    std::collections::BTreeMap<erp_modules::ModuleName, String>,
    Vec<String>,
) {
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let rows = tx.query("SELECT name, content_hash, row_to_json(i)::text FROM installed_addon_identity i ORDER BY name", &[]).await.unwrap();
    let previous = rows
        .iter()
        .map(|row| {
            let name: String = row.get(0);
            (erp_modules::ModuleName::new(name).unwrap(), row.get(1))
        })
        .collect();
    let complete = rows.iter().map(|row| row.get(2)).collect();
    tx.rollback().await.unwrap();
    (previous, complete)
}

#[tokio::test]
async fn real_asset_lifecycle_rejects_every_negative_install_and_upgrade_atomically() {
    use erp_cli::ui_assets::AssetLifecycle;
    use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
    let root = tempfile::tempdir().unwrap();
    base_fixture(root.path());
    file(
        root.path(),
        "web/manifest.toml",
        b"name='Web'\ndepends=['base']\nauto_install=true\n",
    );
    file(
        root.path(),
        "bad/manifest.toml",
        b"name='Bad'\ndepends=['web']\n",
    );
    let web = root.path().join("web");
    web_source(
        &web,
        RUNTIME_ABI,
        b"/*! lifecycle baseline */ export function register() {}",
    );
    manifest(&web,&serde_json::json!({"schema":1,"runtime_abi":RUNTIME_ABI,"entry":"@web/main","modules":[{"path":"static/dist/main.js","specifier":"@web/main"},{"path":"static/dist/abi.js","specifier":erp_sdk::ui_compiler::ABI_MODULE}],"templates":["static/src/parent.xml"],"static_files":[]}).to_string());
    // This transaction-level fixture declares every ABI export and fails loudly if someone
    // tries to use it as a browser runtime. Actual DOM execution belongs to runtime/browser tests.
    file(&web,"static/dist/abi.js",b"const unused = () => { throw new Error('transaction fixture ABI cannot execute DOM'); }; export { unused as template, unused as effect, unused as insert, unused as out, unused as attr, unused as cls, unused as style, unused as prop, unused as attrs, unused as interp, unused as on, unused as callHandler, unused as cond, unused as list, unused as keyed, unused as component, unused as slot };\n");
    file(
        &web,
        "static/src/parent.xml",
        br#"<templates><t t-name="web.Parent"><div><span t-anchor="slot"/></div></t></templates>"#,
    );
    let bad = root.path().join("bad");
    manifest(&bad,&serde_json::json!({"schema":1,"runtime_abi":RUNTIME_ABI,"modules":[],"templates":["static/src/bad.xml"],"static_files":[]}).to_string());
    let cases = [
        (
            "missing parent",
            r#"<templates><t t-name="bad.Child" t-inherit="web.Missing"/></templates>"#,
            "UI asset assembly failed: UI template assembly failed: 1 template error(s)\n  error[missing_template] bad/static/src/bad.xml:1:12 template bad.Child: template parent 'web.Missing' not found (t-inherit of 'bad.Child')",
        ),
        (
            "unmatched xpath",
            r#"<templates><t t-inherit="web.Parent" t-inherit-mode="extension"><xpath expr="//missing" position="inside"><i/></xpath></t></templates>"#,
            "UI asset assembly failed: UI template assembly failed: 1 template error(s)\n  error[inheritance] bad/static/src/bad.xml:1:65 template web.Parent: template inheritance failed: error in view 'extension of web.Parent' (id 2): inheritance spec 1 at entry line 1 <xpath expr=\"//missing\">: Element '<xpath expr=\"//missing\">' cannot be located in parent view",
        ),
        (
            "duplicate name",
            r#"<templates><t t-name="bad.Duplicate"><i/></t><t t-name="bad.Duplicate"><span/></t></templates>"#,
            "UI asset assembly failed: UI template assembly failed: 1 template error(s)\n  error[duplicate_template] bad/static/src/bad.xml:1:46 template bad.Duplicate: template 'bad.Duplicate' is already defined at bad/static/src/bad.xml:1:12",
        ),
        (
            "inheritance cycle",
            r#"<templates><t t-name="bad.A" t-inherit="bad.B"/><t t-name="bad.B" t-inherit="bad.A"/></templates>"#,
            "UI asset assembly failed: UI template assembly failed: 2 template error(s)\n  error[inheritance] bad/static/src/bad.xml:1:12 template bad.A: template inheritance failed: Inheritance cycle through views 2 -> 3 -> 2\n  error[inheritance] bad/static/src/bad.xml:1:49 template bad.B: template inheritance failed: Inheritance cycle through views 3 -> 2 -> 3",
        ),
        (
            "duplicate anchor",
            r#"<templates><t t-name="bad.Anchor"><span t-anchor="same"/><i t-anchor="same"/></t></templates>"#,
            "UI asset assembly failed: UI template assembly failed: 1 template error(s)\n  error[invalid_directive_value] bad/static/src/bad.xml:1:58 template bad.Anchor: duplicate t-anchor 'same': <span> and <i> declare the same anchor",
        ),
        (
            "compile expression",
            r#"<templates><t t-name="bad.Invalid"><span t-if="this."/></t></templates>"#,
            "UI asset assembly failed: UI template assembly failed: 1 template error(s)\n  error[expression_syntax] bad/static/src/bad.xml:1:42 template bad.Invalid: Unexpected token",
        ),
    ];
    let db = erp_testkit::TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(root.path().canonicalize().unwrap());
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .linked_hooks(false)
        .lifecycle_hook(Arc::new(AssetLifecycle::new(options())))
        .build()
        .unwrap();
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    assert_eq!(installed_state(&initialized.orm, "web").await, "installed");
    let first = published(&initialized.orm).await;
    let (_, initial_receipts) = artifact_receipts(&initialized.orm).await;
    let bad_name = ModuleName::new("bad").unwrap();
    for (name, xml, expected) in cases {
        file(&bad, "static/src/bad.xml", xml.as_bytes());
        let error = system
            .button_immediate_install(
                &system.system_env(erp_orm::Context::new()),
                std::slice::from_ref(&bad_name),
            )
            .await
            .unwrap_err();
        let erp_modules::Error::Orm(ref source) = error else {
            panic!("{name}: {error:?}")
        };
        assert_validation_message(source, expected);
        assert_eq!(
            installed_state(&initialized.orm, "bad").await,
            "uninstalled",
            "{name}"
        );
        assert_eq!(published(&initialized.orm).await, first, "{name}");
        assert_eq!(
            artifact_receipts(&initialized.orm).await.1,
            initial_receipts,
            "{name}: failed install changed receipts"
        );
    }
    file(
        &bad,
        "static/src/bad.xml",
        br#"<templates><t t-name="bad.Valid"><p>valid</p></t></templates>"#,
    );
    let installed = system
        .button_immediate_install(
            &system.system_env(erp_orm::Context::new()),
            std::slice::from_ref(&bad_name),
        )
        .await
        .unwrap();
    let baseline = published(&installed.orm).await;
    assert_ne!(baseline.key(), first.key());
    let (previous, baseline_receipts) = artifact_receipts(&installed.orm).await;
    for (name, xml, expected) in cases {
        file(&bad, "static/src/bad.xml", xml.as_bytes());
        // These are explicit operator-authorized forward candidates. Existing receipt bytes
        // remain the preimage; failed migration/asset admission must not re-stamp them.
        let mut candidate_index = AddonIndex::new(true);
        candidate_index.add_dev_path(root.path().canonicalize().unwrap());
        let candidate = ModuleSystem::builder(db.pool().clone(), candidate_index)
            .linked_hooks(false)
            .identity_previous(previous.clone())
            .lifecycle_hook(Arc::new(AssetLifecycle::new(options())))
            .build()
            .unwrap();
        let error = candidate
            .button_immediate_upgrade(
                &candidate.system_env(erp_orm::Context::new()),
                std::slice::from_ref(&bad_name),
            )
            .await
            .unwrap_err();
        let erp_modules::Error::Orm(ref source) = error else {
            panic!("{name}: {error:?}")
        };
        assert_validation_message(source, expected);
        assert_eq!(
            installed_state(&installed.orm, "bad").await,
            "installed",
            "{name}"
        );
        assert_eq!(published(&installed.orm).await, baseline, "{name}");
        assert_eq!(
            artifact_receipts(&installed.orm).await.1,
            baseline_receipts,
            "{name}: failed candidate changed receipts"
        );
    }
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn older_lifecycle_hook_refuses_newer_generation_before_source_collection() {
    use erp_addon_web::assets::store;
    use erp_cli::ui_assets::{AssetLifecycle, assemble_entries};
    use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
    let root = tempfile::tempdir().unwrap();
    base_fixture(root.path());
    file(
        root.path(),
        "web/manifest.toml",
        b"name='Web'\ndepends=['base']\nauto_install=true\n",
    );
    file(
        root.path(),
        "extra/manifest.toml",
        b"name='Extra'\ndepends=['web']\n",
    );
    let web = root.path().join("web");
    web_source(&web, "elang-1", b"export function register() {}");
    let old = runtime_options("compiler-old", "elang-1");
    let db = erp_testkit::TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(root.path().canonicalize().unwrap());
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .linked_hooks(false)
        .lifecycle_hook(Arc::new(AssetLifecycle::new(old.clone())))
        .build()
        .unwrap();
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    let (_, old_receipts) = artifact_receipts(&initialized.orm).await;
    // Simulate the durable asset generation already advanced by another host, while this
    // node keeps its exact old addon files/receipts. This isolates the asset lifecycle fence
    // rather than triggering the independent installed-artifact identity refusal first.
    let newer_source = tempfile::tempdir().unwrap();
    let newer_web = web_source(
        newer_source.path(),
        "elang-2",
        b"export function register() {}",
    );
    let entries = system.with_index(|index| index.discover_entries()).unwrap();
    let base = entries
        .values()
        .find(|entry| entry.name().as_str() == "base")
        .unwrap();
    let active = vec![base, &newer_web];
    let next = runtime_options("compiler-next", "elang-2");
    let mut tx = initialized.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    let upgraded = assemble_entries(&mut tx, &active, &next, false)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Ordinary source-identity admission succeeds for this node's unchanged old artifacts;
    // asset admission must still refuse the authoritative newer runtime generation.
    let error = system
        .button_immediate_install(
            &system.system_env(erp_orm::Context::new()),
            &[ModuleName::new("extra").unwrap()],
        )
        .await
        .unwrap_err();
    let erp_modules::Error::Orm(ref source) = error else {
        panic!("{error:?}")
    };
    assert_validation_message(source, OLDER_RUNTIME_ERROR);
    assert_eq!(
        installed_state(&initialized.orm, "extra").await,
        "uninstalled"
    );
    let mut tx = initialized.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    assert_eq!(
        store::current(&mut tx, "en_US", "main", "elang-2")
            .await
            .unwrap()
            .unwrap(),
        upgraded
    );
    tx.rollback().await.unwrap();
    assert_eq!(artifact_receipts(&initialized.orm).await.1, old_receipts);
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn host_collection_uses_immutable_inputs_retained_by_operation_and_load() {
    use erp_cli::ui_assets::AssetLifecycle;
    use erp_modules::{AddonIndex, InitOptions, ModuleSystem};
    let root = tempfile::tempdir().unwrap();
    base_fixture(root.path());
    file(
        root.path(),
        "web/manifest.toml",
        b"name='Web'\ndepends=['base']\nauto_install=true\n",
    );
    let web = root.path().join("web");
    web_source(
        &web,
        RUNTIME_ABI,
        b"/*! admitted */ export function register() {}",
    );
    let db = erp_testkit::TestDb::create().await.unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(root.path().canonicalize().unwrap());
    let system = ModuleSystem::builder(db.pool().clone(), index)
        .linked_hooks(false)
        .lifecycle_hook(Arc::new(AssetLifecycle::new(options())))
        .build()
        .unwrap();
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    let loaded = system.load().await.unwrap();
    let original = published(&initialized.orm).await;
    file(
        &web,
        "static/dist/main.js",
        b"/*! changed after admission */ export function register() {}",
    );
    for admitted in [initialized.admitted_addons(), loaded.admitted_addons()] {
        let entries = admitted.iter().collect::<Vec<_>>();
        let bundle =
            assemble(&collect_entries(&entries, RUNTIME_ABI).unwrap(), &options()).unwrap();
        assert_eq!(
            bundle, original,
            "host must not reread live files after lifecycle/load admission"
        );
    }
    db.cleanup().await.unwrap();
}

struct UiFixtureRefresh;
impl erp_orm::Refresh for UiFixtureRefresh {
    fn refresh<'a>(
        &'a self,
        _changes: &'a erp_orm::Changes,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_orm::Refreshed>> {
        Box::pin(async { Ok(erp_orm::Refreshed::InPlace) })
    }
}

async fn stale_startup_interleaving(uninstall: bool) {
    use erp_cli::ui_assets::{AssetLifecycle, admit_startup};
    use erp_modules::{AddonIndex, InitOptions, ModuleName, ModuleSystem};
    use erp_orm::{Generations, Signal, SignalKey};
    let root = tempfile::tempdir().unwrap();
    base_fixture(root.path());
    file(
        root.path(),
        "web/manifest.toml",
        b"name='Web'\ndepends=['base']\nauto_install=true\n",
    );
    file(
        root.path(),
        "extra/manifest.toml",
        b"name='Extra'\ndepends=['web']\n",
    );
    web_source(
        &root.path().join("web"),
        RUNTIME_ABI,
        b"export function register() {}",
    );
    let extra = root.path().join("extra");
    manifest(&extra,&serde_json::json!({"schema":1,"runtime_abi":RUNTIME_ABI,"entry":"@extra/main","modules":[{"path":"static/dist/main.js","specifier":"@extra/main"}],"templates":[],"static_files":[]}).to_string());
    file(
        &extra,
        "static/dist/main.js",
        b"/*! extra */ export function register() {}",
    );
    let db = erp_testkit::TestDb::create().await.unwrap();
    let build = |baseline: Generations| {
        let mut index = AddonIndex::new(true);
        index.add_dev_path(root.path().canonicalize().unwrap());
        ModuleSystem::builder(db.pool().clone(), index)
            .linked_hooks(false)
            .signal(Signal::new(baseline).with_refresh(UiFixtureRefresh))
            .lifecycle_hook(Arc::new(AssetLifecycle::new(options())))
            .build()
            .unwrap()
    };
    let a = build(Generations::new());
    let initial = a.initialize(InitOptions::new()).await.unwrap();
    if uninstall {
        let installed = a
            .button_immediate_install(
                &a.system_env(erp_orm::Context::new()),
                &[ModuleName::new("extra").unwrap()],
            )
            .await
            .unwrap();
        assert_eq!(installed_state(&installed.orm, "extra").await, "installed");
    }
    let client = db.client().await.unwrap();
    // This observation precedes the entire load, including discover_admitted's source capture.
    let observed = Signal::read(&client).await.unwrap();
    drop(client);
    let retained = a.load().await.unwrap();
    let b = build(observed.clone());
    let committed = if uninstall {
        b.button_immediate_uninstall(
            &b.system_env(erp_orm::Context::new()),
            &[ModuleName::new("extra").unwrap()],
        )
        .await
        .unwrap()
    } else {
        b.button_immediate_install(
            &b.system_env(erp_orm::Context::new()),
            &[ModuleName::new("extra").unwrap()],
        )
        .await
        .unwrap()
    };
    let expected = published(&committed.orm).await;
    let client = db.client().await.unwrap();
    let after = Signal::read(&client).await.unwrap();
    drop(client);
    assert_ne!(
        observed.get(&SignalKey::Registry),
        after.get(&SignalKey::Registry)
    );
    let entries = retained.admitted_addons().iter().collect::<Vec<_>>();
    let result = admit_startup(&retained.orm, &entries, &options(), &observed).await;
    let actual = published(&committed.orm).await;
    let state = installed_state(&committed.orm, "extra").await;
    db.cleanup().await.unwrap();
    let error = result.unwrap_err();
    assert_eq!(
        (error.kind(), error.message()),
        (
            ErrorKind::Concurrency,
            "UI asset startup registry generation changed; reload before admission"
        )
    );
    assert_eq!(
        actual, expected,
        "stale startup must preserve the complete committed bundle and bytes"
    );
    assert_eq!(
        state,
        if uninstall {
            "uninstalled"
        } else {
            "installed"
        }
    );
    assert_eq!(
        initial
            .admitted_addons()
            .iter()
            .map(|addon| addon.name().as_str())
            .collect::<Vec<_>>(),
        ["base", "web"]
    );
}

#[tokio::test]
async fn startup_retained_load_cannot_republish_after_same_runtime_install() {
    stale_startup_interleaving(false).await;
}

#[tokio::test]
async fn startup_retained_load_cannot_republish_after_same_runtime_uninstall() {
    stale_startup_interleaving(true).await;
}
