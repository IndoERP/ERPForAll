//! Zero-restart install, upgrade and uninstall of WebAssembly addons (
//!
//! The component here is a stand-in loader written in the test: the "wasm" bytes of a package are
//! the `describe()` text itself, and a method is a small Rust invoker speaking the wire form. That
//! proves the lifecycle (catalog, schema, registry, signal, audit, rollback) without a component;
//! `wasm_lifecycle_component.rs` in `erp-test-models` runs the same lifecycle with a real one.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use std::sync::Arc;

use common::*;
use erp_modules::error::{Error, WasmReason};
use erp_modules::package::{AdminConsent, ErpAddonPackage, ErpAddonPackageWriter};
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::wasm_lifecycle::{ComponentLoader, WasmComponent, WasmLifecycle, WasmPackages};
use erp_orm::{
    BoxFuture, Changes, DynSuper, Generations, ImplKind, MethodInvoker, Orm, Records, Refresh,
    Refreshed, SUPERUSER_ID, Signal, Transaction, WasmMethodImpl, call_method_json,
};
use erp_testkit::TestDb;
use erp_wasm_host::described_methods;
use serde_json::json;

const MODEL: &str = "test.nat";

// ---- the stand-in component ------------------------------------------------------------------

#[derive(Debug)]
struct Invoker {
    addon: String,
    method: String,
    kind: ImplKind,
}

impl MethodInvoker for Invoker {
    fn addon(&self) -> &str {
        &self.addon
    }
    fn kind(&self) -> ImplKind {
        self.kind
    }
    fn invoke<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        _records: &'a Records,
        args_json: &'a str,
        _sup: DynSuper,
    ) -> BoxFuture<'a, erp_fields::Result<String>> {
        Box::pin(async move {
            let (text,): (String,) = serde_json::from_str(args_json).unwrap();
            Ok(serde_json::to_string(&format!("{}:{}", self.method, text.to_uppercase())).unwrap())
        })
    }
}

#[derive(Debug)]
struct Stub {
    addon: String,
    describe: String,
}

impl WasmComponent for Stub {
    fn describe(&self) -> &str {
        &self.describe
    }
    fn method_impls(&self, _plain: &Orm) -> erp_modules::Result<Vec<WasmMethodImpl>> {
        Ok(described_methods(&self.addon, &self.describe)
            .unwrap()
            .into_iter()
            .map(|m| WasmMethodImpl {
                invoker: Arc::new(Invoker {
                    addon: self.addon.clone(),
                    method: m.method.clone(),
                    kind: m.kind,
                }),
                model: m.model,
                method: m.method,
            })
            .collect())
    }
}

/// The refresh hook of a server would rebuild the registry (`WasmLifecycle::load`) and swap its
/// ORM; the tests swap by hand and only need the hook to exist.
struct KeepServing;

impl Refresh for KeepServing {
    fn refresh<'a>(
        &'a self,
        _changes: &'a Changes,
    ) -> BoxFuture<'a, erp_fields::Result<Refreshed>> {
        Box::pin(async { Ok(Refreshed::InPlace) })
    }
}

struct StubLoader;

impl ComponentLoader for StubLoader {
    fn load<'a>(
        &'a self,
        module: &'a erp_modules::ModuleName,
        wasm: &'a [u8],
    ) -> BoxFuture<'a, erp_modules::Result<Arc<dyn WasmComponent>>> {
        Box::pin(async move {
            if wasm == b"TRAP" {
                return Err(Error::WasmAddon {
                    addon: module.clone(),
                    reason: WasmReason::Component("describe() trapped".to_owned()),
                });
            }
            // as the real loader does at load: a malformed method description is refused
            let text = String::from_utf8(wasm.to_vec()).unwrap();
            // (the count limit is the lifecycle's own check, tested on its own)
            if let Err(e) = described_methods(module.as_str(), &text)
                && !e.to_string().contains("method contributions; refusing")
            {
                return Err(Error::WasmAddon {
                    addon: module.clone(),
                    reason: WasmReason::Component(e.to_string()),
                });
            }
            Ok(Arc::new(Stub {
                addon: module.to_string(),
                describe: String::from_utf8(wasm.to_vec()).unwrap(),
            }) as Arc<dyn WasmComponent>)
        })
    }
}

// ---- packages --------------------------------------------------------------------------------

/// A `describe()` text: the addon entry, an extension of `test.nat` with `fields` (char), and a
/// `method` contribution per name.
fn describe(
    addon: &str,
    depends: &[&str],
    model: &str,
    fields: &[&str],
    methods: &[&str],
) -> String {
    let mut deps = vec!["base"];
    deps.extend(depends);
    let mut list = vec![
        json!({"addon": addon, "kind": "addon", "model": "", "name": addon,
        "detail": {"depends": deps}}),
    ];
    if !fields.is_empty() {
        let fields: Vec<_> = fields
            .iter()
            .map(|f| json!({"name": f, "kind": "char", "attrs": {"string": format!("\"{f}\"")}}))
            .collect();
        list.push(
            json!({"addon": addon, "kind": "extend", "model": model, "name": "Ext",
            "detail": {"target": "Nat", "args": {}, "fields": fields}}),
        );
    }
    for m in methods {
        list.push(
            json!({"addon": addon, "kind": "method", "model": model, "name": m,
            "detail": {"args": [{"name": "text", "type": "String"}]}}),
        );
    }
    json!({"schema": 1, "contributions": list}).to_string()
}

fn manifest_text(name: &str, version: &str, depends: &[&str], caps: &[&str]) -> String {
    let deps: Vec<String> = depends.iter().map(|d| format!("\"{d}\"")).collect();
    let caps: Vec<String> = caps.iter().map(|d| format!("\"{d}\"")).collect();
    format!(
        "name = \"{name}\"\nlicense = \"MIT OR Apache-2.0\"\nversion = \"{version}\"\n\
         depends = [{}]\n[wasm]\nsdk_version = \"1.0\"\ncapabilities = [{}]\n",
        deps.join(", "),
        caps.join(", ")
    )
}

fn archive(
    name: &str,
    version: &str,
    depends: &[&str],
    caps: &[&str],
    describe: &str,
    dist: Option<&str>,
) -> Vec<u8> {
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file("manifest.toml", manifest_text(name, version, depends, caps));
    writer.add_file(format!("wasm/{name}.wasm"), describe.as_bytes());
    if let Some(js) = dist {
        writer.add_file("static/dist/app.js", js.as_bytes());
    }
    writer.build_zip().unwrap()
}

fn consent(bytes: &[u8]) -> erp_modules::package::ConsentedPackage {
    let package = ErpAddonPackage::from_bytes(bytes).unwrap();
    let mut c = AdminConsent::new(package.name().clone());
    if let Some(wasm) = package.manifest().wasm() {
        for cap in &wasm.capabilities {
            c.approve_capability(cap.canonical_str());
        }
    }
    if let Some(digest) = package.client_dist_digest() {
        c.set_client_dist_digest_ack(digest);
    }
    package.consent(&c).unwrap()
}

/// `wlife` v1: `depends = [nat]`, extends `test.nat` with `note`, defines `shout`.
fn wlife(version: &str, fields: &[&str]) -> Vec<u8> {
    archive(
        "wlife",
        version,
        &["nat"],
        &[],
        &describe("wlife", &["nat"], MODEL, fields, &["shout"]),
        None,
    )
}

// ---- the world -------------------------------------------------------------------------------

struct World {
    db: TestDb,
    system: Arc<ModuleSystem>,
    life: WasmLifecycle,
    orm: Orm,
}

async fn build_system(db: &TestDb, addons: &Addons) -> (Arc<ModuleSystem>, WasmLifecycle) {
    let packages = WasmPackages::new(Arc::new(StubLoader));
    let system = Arc::new(
        builder(db, addons)
            .decls([model("nat", MODEL)])
            .signal(Signal::new(Generations::new()).with_refresh(KeepServing))
            .package_runtime(packages.clone())
            .build()
            .unwrap(),
    );
    let life = WasmLifecycle::new(Arc::clone(&system), packages);
    (system, life)
}

fn fixture_addons() -> Addons {
    let addons = Addons::new();
    addons.add("nat", "name = 'Nat'\ndepends = ['base']\n");
    addons.add("nat2", "name = 'Nat2'\ndepends = ['base']\n");
    addons
}

async fn world() -> (World, Addons) {
    let addons = fixture_addons();
    let db = TestDb::create().await.unwrap();
    let (system, life) = build_system(&db, &addons).await;
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let outcome = system
        .button_immediate_install(&su(&init.orm), &[n("nat")])
        .await
        .unwrap();
    committed_ref(&outcome);
    let orm = outcome.orm;
    (
        World {
            db,
            system,
            life,
            orm,
        },
        addons,
    )
}

fn committed_ref(outcome: &erp_modules::system::OperationOutcome) {
    assert!(outcome.post_commit.failed.is_empty(), "{outcome:?}");
}

async fn generation(db: &TestDb) -> i64 {
    let client = db.client().await.unwrap();
    client
        .query_one(
            "SELECT coalesce(sum(weight), 0)::int8 FROM erp_signal_generation WHERE key = 'registry'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

async fn shout(orm: &Orm, text: &str) -> erp_fields::Result<String> {
    let mut tx = orm.begin(SUPERUSER_ID).await?;
    let records = tx.env().records(MODEL)?.browse(Vec::<i64>::new())?;
    let args = serde_json::to_string(&(text,)).unwrap();
    let result = call_method_json(&mut tx, &records, "shout", &args).await;
    tx.rollback().await?;
    result
}

async fn has_column(orm: &Orm, column: &str) -> bool {
    !rows(
        orm,
        &format!(
            "SELECT 'x' FROM information_schema.columns WHERE table_name = 'test_nat' \
             AND column_name = '{column}'"
        ),
    )
    .await
    .is_empty()
}

async fn state_opt(orm: &Orm, module: &str) -> Option<String> {
    rows(
        orm,
        &format!("SELECT state FROM ir_module_module WHERE name = '{module}'"),
    )
    .await
    .first()
    .map(|r| r[0].clone().unwrap())
}

async fn count(orm: &Orm, sql: &str) -> i64 {
    rows(orm, &format!("SELECT ({sql})::text")).await[0][0]
        .as_ref()
        .unwrap()
        .parse()
        .unwrap()
}

/// Rows in the package store: the table is a core table every initialised database has, so
/// "nothing stored" is zero rows, not a missing table.
async fn package_rows(orm: &Orm) -> i64 {
    count(orm, "SELECT count(*) FROM ir_module_package").await
}

fn expected_shout(text: &str) -> String {
    serde_json::to_string(&format!("shout:{}", text.to_uppercase())).unwrap()
}

fn wasm_reason(err: &Error) -> &WasmReason {
    match err {
        Error::WasmAddon { reason, .. } => reason,
        other => panic!("not a WebAssembly lifecycle error: {other}"),
    }
}

// ---- tests -----------------------------------------------------------------------------------

/// (one module system throughout), the registry of each operation is the new `outcome.orm`, and
/// database B, running its own module system the whole time, never sees any of it.
#[tokio::test]
async fn install_upgrade_uninstall_without_restart_while_another_database_runs() {
    let (w, addons) = world().await;
    // database B: same addons, own module system, never touched by the lifecycle of A
    let db_b = TestDb::create().await.unwrap();
    let (system_b, _life_b) = build_system(&db_b, &addons).await;
    let init_b = system_b
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let b = system_b
        .button_immediate_install(&su(&init_b.orm), &[n("nat")])
        .await
        .unwrap()
        .orm;
    let gen_b = generation(&db_b).await;

    let env = su(&w.orm);
    let before = generation(&w.db).await;
    let installed = w
        .life
        .install(&env, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    committed_ref(&installed);
    assert_eq!(installed.installed, [n("wlife")]);
    assert_eq!(state(&installed.orm, "wlife").await, "installed");
    assert!(
        has_column(&installed.orm, "note").await,
        "schema synced in the same transaction"
    );
    assert_eq!(
        shout(&installed.orm, "hi").await.unwrap(),
        expected_shout("hi")
    );
    assert_eq!(
        generation(&w.db).await,
        before + 1,
        "one registry change signalled"
    );
    // the registry in use before the install is untouched
    assert!(shout(&w.orm, "hi").await.is_err());

    let candidate = wlife("1.1.0", &["note", "extra"]);
    let expected_identity = format!(
        "package-v1:{}",
        ErpAddonPackage::from_bytes(&candidate)
            .unwrap()
            .archive_sha256()
    );
    let env = su(&installed.orm);
    let upgraded = w.life.upgrade(&env, consent(&candidate)).await.unwrap();
    committed_ref(&upgraded);
    assert_eq!(
        rows(
            &upgraded.orm,
            "SELECT content_hash FROM installed_addon_identity WHERE name='wlife'"
        )
        .await,
        vec![vec![Some(expected_identity)]]
    );
    assert!(has_column(&upgraded.orm, "extra").await);
    assert_eq!(
        shout(&upgraded.orm, "up").await.unwrap(),
        expected_shout("up")
    );
    assert_eq!(generation(&w.db).await, before + 2);

    let env = su(&upgraded.orm);
    let removed = w.life.uninstall(&env, &n("wlife")).await.unwrap();
    committed_ref(&removed);
    assert_eq!(removed.uninstalled, [n("wlife")]);
    assert!(
        !has_column(&removed.orm, "note").await,
        "its data goes with it"
    );
    assert!(
        shout(&removed.orm, "x").await.is_err(),
        "its methods are gone"
    );
    assert_eq!(
        count(&removed.orm, "SELECT count(*) FROM ir_module_package").await,
        0
    );
    assert_eq!(generation(&w.db).await, before + 3);

    // database B ran throughout, with its own registry, its own generation and no package table
    assert_eq!(generation(&db_b).await, gen_b);
    assert!(!has_column(&b, "note").await);
    assert_eq!(package_rows(&b).await, 0);
    assert_eq!(state(&b, "nat").await, "installed");
    w.db.cleanup().await.unwrap();
    db_b.cleanup().await.unwrap();
}

/// operation's transaction, and writes one audit row per operation with the actor and versions).
#[tokio::test]
async fn install_stores_the_package_and_audits_each_operation() {
    let (w, _addons) = world().await;
    let bytes = wlife("1.0.0", &["note"]);
    let sha = ErpAddonPackage::from_bytes(&bytes)
        .unwrap()
        .archive_sha256()
        .to_owned();
    let installed = w.life.install(&su(&w.orm), consent(&bytes)).await.unwrap();
    let stored = rows(
        &installed.orm,
        "SELECT name, version, trim(sha256), (sha256 = trim(sha256))::text FROM ir_module_package",
    )
    .await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0][0].as_deref(), Some("wlife"));
    assert_eq!(stored[0][2].as_deref(), Some(sha.as_str()));

    let upgraded = w
        .life
        .upgrade(&su(&installed.orm), consent(&wlife("1.2.0", &["note"])))
        .await
        .unwrap();
    committed(upgraded);
    let removed = w
        .life
        .uninstall(&su(&installed.orm), &n("wlife"))
        .await
        .unwrap();
    committed(removed);
    let audit = rows(
        &installed.orm,
        "SELECT operation, module, version, previous_version, actor_uid::text, actor_kind, session_id::text \
         FROM ir_module_wasm_audit ORDER BY id",
    )
    .await;
    let seen: Vec<(String, String, Option<String>)> = audit
        .iter()
        .map(|r| (r[0].clone().unwrap(), r[2].clone().unwrap(), r[3].clone()))
        .collect();
    assert_eq!(
        seen,
        [
            ("install".to_owned(), "1.0.0".to_owned(), None),
            (
                "upgrade".to_owned(),
                "1.2.0".to_owned(),
                Some("1.0.0".to_owned())
            ),
            ("uninstall".to_owned(), "1.2.0".to_owned(), None),
        ]
    );
    // the operator path names its actor explicitly: the System user, kind 'operator', no session
    assert!(audit.iter().all(|r| {
        r[4].as_deref() == Some(&SUPERUSER_ID.to_string())
            && r[5].as_deref() == Some("operator")
            && r[6].is_none()
    }));
    w.db.cleanup().await.unwrap();
}

/// from the database alone (its own module system, no package in its index until it loads); it
/// follows an upgrade and an uninstall the same way.
#[tokio::test]
async fn another_node_follows_from_the_stored_packages() {
    let (w, addons) = world().await;
    let (node2, life2) = build_system(&w.db, &addons).await;
    // nothing installed yet: node 2 loads the plain set
    let plain = life2.load().await.unwrap();
    assert!(shout(&plain.orm, "a").await.is_err());
    let g0 = generation(&w.db).await;

    let installed = w
        .life
        .install(&su(&w.orm), consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    assert!(generation(&w.db).await > g0, "the install is signalled");
    // without the load, node 2's own registry has no idea of the package
    let loaded = life2.load().await.unwrap();
    assert_eq!(shout(&loaded.orm, "b").await.unwrap(), expected_shout("b"));
    assert!(has_column(&loaded.orm, "note").await);

    committed(
        w.life
            .uninstall(&su(&installed.orm), &n("wlife"))
            .await
            .unwrap(),
    );
    let after = life2.load().await.unwrap();
    assert!(shout(&after.orm, "c").await.is_err());
    assert!(life2.packages().live_modules().is_empty());
    drop(node2);
    w.db.cleanup().await.unwrap();
}

/// loads it): a corrupted blob and a different valid archive under the same row.
#[tokio::test]
async fn a_tampered_stored_package_is_refused_on_load() {
    let (w, addons) = world().await;
    committed(
        w.life
            .install(&su(&w.orm), consent(&wlife("1.0.0", &["note"])))
            .await
            .unwrap(),
    );
    let other = wlife("9.9.9", &["other"]);

    // a different valid archive in place of the stored one: the content address no longer matches
    exec_bytes(&w.orm, &other).await;
    let (_node, life2) = build_system(&w.db, &addons).await;
    let err = life2.load().await.unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::StoredPackage(m) if m.contains("hashes to")),
        "{err}"
    );

    // bytes that are not an archive
    exec_bytes(&w.orm, b"not a zip").await;
    let err = life2.load().await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::PackageIntegrity { .. } | Error::PackageFormat { .. }
        ),
        "{err}"
    );
    assert!(
        life2.packages().live_modules().is_empty(),
        "nothing half-loaded"
    );
    w.db.cleanup().await.unwrap();
}

async fn exec_bytes(orm: &Orm, bytes: &[u8]) {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    tx.execute("UPDATE ir_module_package SET package_data = $1", &[&bytes])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// is written, and nothing is installed automatically).
#[tokio::test]
async fn install_with_a_missing_dependency_is_refused() {
    let (w, _addons) = world().await;
    let bytes = archive(
        "needs2",
        "1.0.0",
        &["nat2"],
        &[],
        &describe("needs2", &["nat2"], MODEL, &["a"], &[]),
        None,
    );
    let before = generation(&w.db).await;
    let err = w
        .life
        .install(&su(&w.orm), consent(&bytes))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::DependsNotInstalled(m) if m == &["nat2"]),
        "{err}"
    );
    assert_eq!(
        state(&w.orm, "nat2").await,
        "uninstalled",
        "no automatic dependency install"
    );
    assert_eq!(package_rows(&w.orm).await, 0);
    assert_eq!(generation(&w.db).await, before);
    w.db.cleanup().await.unwrap();
}

/// different dependency list both fail loudly, before the database is touched.
#[tokio::test]
async fn describe_that_disagrees_with_the_manifest_is_refused() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let wrong_name = archive(
        "wlife",
        "1.0.0",
        &["nat"],
        &[],
        &describe("other_name", &["nat"], MODEL, &["note"], &["shout"]),
        None,
    );
    let err = w
        .life
        .install(&env, consent(&wrong_name))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::DescribeMismatch(m) if m.contains("no addon named 'wlife'")),
        "{err}"
    );

    let wrong_depends = archive(
        "wlife",
        "1.0.0",
        &["nat"],
        &[],
        &describe("wlife", &["nat2"], MODEL, &["note"], &["shout"]),
        None,
    );
    let err = w
        .life
        .install(&env, consent(&wrong_depends))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::DescribeMismatch(m) if m.contains("depends on")),
        "{err}"
    );
    assert_eq!(package_rows(&w.orm).await, 0);
    assert_eq!(state(&w.orm, "nat").await, "installed");
    w.db.cleanup().await.unwrap();
}

/// compute, a field kind and an attribute outside the supported set, a trapping `describe()`, and
/// a capability the host cannot enforce.
#[tokio::test]
async fn unsupported_contributions_and_capabilities_are_refused_by_name() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let with = |extra: serde_json::Value, caps: &[&str]| {
        let mut doc: serde_json::Value =
            serde_json::from_str(&describe("wlife", &["nat"], MODEL, &["note"], &["shout"]))
                .unwrap();
        if !extra.is_null() {
            doc["contributions"].as_array_mut().unwrap().push(extra);
        }
        archive("wlife", "1.0.0", &["nat"], caps, &doc.to_string(), None)
    };
    // an async compute, and an override of a compute: named, never skipped
    let compute = json!({"addon": "wlife", "kind": "compute", "model": MODEL, "name": "_c",
        "detail": {"async": true, "depends": [], "reads": [], "depends_context": []}});
    let err = w
        .life
        .install(&env, consent(&with(compute, &[])))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::Unsupported(m) if m.contains("async compute")),
        "{err}"
    );
    let compute_override = json!({"addon": "wlife", "kind": "compute_override", "model": MODEL,
        "name": "_c", "detail": {}});
    let err = w
        .life
        .install(&env, consent(&with(compute_override, &[])))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::Unsupported(m) if m.contains("compute_override '_c'")),
        "{err}"
    );

    let relational = json!({"addon": "wlife", "kind": "extend", "model": MODEL, "name": "Ext2",
        "detail": {"args": {}, "fields": [{"name": "p", "kind": "many2one", "attrs": {}}]}});
    let err = w
        .life
        .install(&env, consent(&with(relational, &[])))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::Unsupported(m) if m.contains("many2one")),
        "{err}"
    );

    let attr = json!({"addon": "wlife", "kind": "extend", "model": MODEL, "name": "Ext3",
        "detail": {"args": {}, "fields": [{"name": "q", "kind": "char", "attrs": {"compute": "\"_c\""}}]}});
    let err = w
        .life
        .install(&env, consent(&with(attr, &[])))
        .await
        .unwrap_err();
    // a computed field whose compute the addon does not declare
    assert!(
        matches!(wasm_reason(&err), WasmReason::DescribeMismatch(m) if m.contains("names the compute '_c'")),
        "{err}"
    );
    // an attribute outside the supported set
    let attr = json!({"addon": "wlife", "kind": "extend", "model": MODEL, "name": "Ext4",
        "detail": {"args": {}, "fields": [{"name": "q", "kind": "char", "attrs": {"precompute": "true"}}]}});
    let err = w
        .life
        .install(&env, consent(&with(attr, &[])))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::Unsupported(m) if m.contains("'precompute'")),
        "{err}"
    );

    let err = w
        .life
        .install(
            &env,
            consent(&with(serde_json::Value::Null, &["migrations-sql"])),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::CapabilityUnavailable(c) if c == "migrations-sql"),
        "{err}"
    );

    let trap = archive("wlife", "1.0.0", &["nat"], &[], "TRAP", None);
    let err = w.life.install(&env, consent(&trap)).await.unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::Component(_)),
        "{err}"
    );
    assert_eq!(package_rows(&w.orm).await, 0);
    w.db.cleanup().await.unwrap();
}

/// the digest the administrator acknowledged is what the store and the audit row record): an
/// unapproved capability and unacknowledged client code never reach the lifecycle; an acknowledged
/// one installs and its digest is recorded.
#[tokio::test]
async fn consent_gates_the_install_and_the_acknowledged_digest_is_recorded() {
    let (w, _addons) = world().await;
    let d = describe("wlife", &["nat"], MODEL, &["note"], &["shout"]);

    let unacked = archive("wlife", "1.0.0", &["nat"], &[], &d, Some("console.log(1)"));
    let package = ErpAddonPackage::from_bytes(&unacked).unwrap();
    let err = package.consent(&AdminConsent::new(n("wlife"))).unwrap_err();
    assert!(
        matches!(err, Error::UnacknowledgedClientCode { .. }),
        "{err}"
    );

    let with_cap = archive("wlife", "1.0.0", &["nat"], &["migrations-sql"], &d, None);
    let package = ErpAddonPackage::from_bytes(&with_cap).unwrap();
    let err = package.consent(&AdminConsent::new(n("wlife"))).unwrap_err();
    assert!(matches!(err, Error::UnapprovedCapabilities { .. }), "{err}");
    assert_eq!(state(&w.orm, "nat").await, "installed");
    assert_eq!(
        package_rows(&w.orm).await,
        0,
        "nothing reached the database"
    );

    let digest = ErpAddonPackage::from_bytes(&unacked)
        .unwrap()
        .client_dist_digest()
        .unwrap()
        .to_owned();
    let installed = w
        .life
        .install(&su(&w.orm), consent(&unacked))
        .await
        .unwrap();
    let recorded = rows(
        &installed.orm,
        "SELECT trim(client_dist_digest) FROM ir_module_wasm_audit WHERE operation = 'install'",
    )
    .await;
    assert_eq!(recorded[0][0].as_deref(), Some(digest.as_str()));
    let stored = rows(
        &installed.orm,
        "SELECT trim(client_dist_digest) FROM ir_module_package",
    )
    .await;
    assert_eq!(stored[0][0].as_deref(), Some(digest.as_str()));
    w.db.cleanup().await.unwrap();
}

/// previous registry stays live): the registry rejects the package (an extension of a model that
/// does not exist), and a failure after the package row was written (the audit insert).
#[tokio::test]
async fn a_failure_inside_the_operation_rolls_everything_back() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let before = generation(&w.db).await;

    // 1. the registry refuses it while the operation composes it
    let bad_model = archive(
        "wlife",
        "1.0.0",
        &["nat"],
        &[],
        &describe("wlife", &["nat"], "test.missing", &["note"], &[]),
        None,
    );
    let err = w.life.install(&env, consent(&bad_model)).await.unwrap_err();
    assert!(
        !matches!(err, Error::WasmAddon { .. }),
        "the registry's own error: {err}"
    );
    assert_ne!(
        state_opt(&w.orm, "wlife").await.as_deref(),
        Some("installed")
    );
    assert_eq!(generation(&w.db).await, before);

    // 2. an audit table of another shape: the insert fails after the package row was written
    exec(
        &w.orm,
        "DROP TABLE ir_module_wasm_audit; CREATE TABLE ir_module_wasm_audit (wrong int)",
    )
    .await;
    let err = w
        .life
        .install(&env, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap_err();
    assert!(!matches!(err, Error::WasmAddon { .. }), "{err}");
    assert_eq!(
        package_rows(&w.orm).await,
        0,
        "the package table rolled back too"
    );
    assert!(!has_column(&w.orm, "note").await);
    assert_ne!(
        state_opt(&w.orm, "wlife").await.as_deref(),
        Some("installed")
    );
    assert_eq!(
        generation(&w.db).await,
        before,
        "a rolled-back operation signals nothing"
    );
    assert!(w.life.packages().live_modules().is_empty());
    // the registry in use is the one it was, and the same module installs once the fault is gone
    assert_eq!(state(&w.orm, "nat").await, "installed");
    exec(&w.orm, "DROP TABLE ir_module_wasm_audit").await;
    let again = w
        .life
        .install(&env, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    assert_eq!(shout(&again.orm, "ok").await.unwrap(), expected_shout("ok"));
    w.db.cleanup().await.unwrap();
}

/// stored package and its index entry as they were, and an upgrade that is not newer is refused).
#[tokio::test]
async fn a_failed_upgrade_leaves_the_installed_version_alone() {
    let (w, _addons) = world().await;
    let installed = w
        .life
        .install(&su(&w.orm), consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    let env = su(&installed.orm);

    let err = w
        .life
        .upgrade(&env, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::State(m) if m.contains("not newer")),
        "{err}"
    );

    let bad = archive(
        "wlife",
        "2.0.0",
        &["nat"],
        &[],
        &describe("wlife", &["nat"], "test.missing", &["note"], &[]),
        None,
    );
    w.life.upgrade(&env, consent(&bad)).await.unwrap_err();
    assert_eq!(
        count(&installed.orm, "SELECT count(*) FROM ir_module_package").await,
        1
    );
    assert_eq!(
        shout(&installed.orm, "v1").await.unwrap(),
        expected_shout("v1")
    );
    assert_eq!(state(&installed.orm, "wlife").await, "installed");
    // the index entry came back, so the addon still upgrades and uninstalls
    let ok = w
        .life
        .upgrade(&env, consent(&wlife("1.5.0", &["note"])))
        .await
        .unwrap();
    committed(w.life.uninstall(&su(&ok.orm), &n("wlife")).await.unwrap());
    w.db.cleanup().await.unwrap();
}

/// package, naming them; and a package addon cannot be changed through the plain operations).
#[tokio::test]
async fn uninstall_is_blocked_by_a_dependent_and_plain_operations_leave_packages_alone() {
    let (w, _addons) = world().await;
    let installed = w
        .life
        .install(&su(&w.orm), consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    let dependent = archive(
        "wdep",
        "1.0.0",
        &["wlife"],
        &[],
        &describe("wdep", &["wlife"], MODEL, &[], &[]),
        None,
    );
    let with_dep = w
        .life
        .install(&su(&installed.orm), consent(&dependent))
        .await
        .unwrap();
    let env = su(&with_dep.orm);

    let err = w.life.uninstall(&env, &n("wlife")).await.unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::HasDependents(d) if d == &["wdep"]),
        "{err}"
    );
    assert_eq!(state(&with_dep.orm, "wlife").await, "installed");
    assert_eq!(state(&with_dep.orm, "wdep").await, "installed");

    // the plain operation of the module system would also uninstall the dependent: the hook
    // refuses a package change that did not go through the lifecycle
    let err = w
        .system
        .button_immediate_uninstall(&env, &[n("wdep")])
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::State(m) if m.contains("only through WasmLifecycle")),
        "{err}"
    );
    assert_eq!(state(&with_dep.orm, "wdep").await, "installed");

    // in the right order both go
    let a = w.life.uninstall(&env, &n("wdep")).await.unwrap();
    committed(w.life.uninstall(&su(&a.orm), &n("wlife")).await.unwrap());
    w.db.cleanup().await.unwrap();
}

/// message that names the right call).
#[tokio::test]
async fn install_of_an_installed_addon_and_upgrade_of_a_missing_one_are_refused() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let err = w
        .life
        .upgrade(&env, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::State(m) if m.contains("use install")),
        "{err}"
    );
    let err = w.life.uninstall(&env, &n("wlife")).await.unwrap_err();
    assert!(matches!(wasm_reason(&err), WasmReason::State(_)), "{err}");
    let installed = w
        .life
        .install(&env, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    let err = w
        .life
        .install(&su(&installed.orm), consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::State(m) if m.contains("use upgrade")),
        "{err}"
    );
    w.db.cleanup().await.unwrap();
}

// ---- security review r1 ---------------------------------

use erp_modules::install::{InstallAuthorization, InstallAuthorizer};
use erp_modules::wasm_lifecycle::{
    MAX_DESCRIBED_FIELDS, MAX_DESCRIBED_MODELS, MAX_PACKAGE_METHODS,
    revoke_wasm_lifecycle_runtime_writes,
};
use erp_orm::{GroupSet, PrincipalBuilder, Resolution, UserId};
use erp_testkit::tokio_postgres::error::SqlState;
use std::num::NonZeroU64;

const SESSION: &str = "019a0000-0000-7000-8000-0000000000aa";

/// The trusted native authorizer, as the tests see it: it admits the users in `admins` (the right to
/// administer modules) and refuses everyone else. It builds the principal the way the real one does.
struct Admins {
    user: i64,
    admins: &'static [i64],
}

impl InstallAuthorizer for Admins {
    fn authorize<'a>(
        &'a self,
        _tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>> {
        Box::pin(async move {
            if !self.admins.contains(&self.user) {
                return Err(erp_fields::Error::access(format!(
                    "user {} has no module administration right",
                    self.user
                )));
            }
            let principal = PrincipalBuilder::user(UserId::new(self.user).unwrap())
                .resolution(
                    Resolution::in_id_order(GroupSet::default(), [], NonZeroU64::MIN).unwrap(),
                )
                .build()
                .unwrap();
            InstallAuthorization::new(principal, SESSION.into())
        })
    }
}

async fn audit_rows(orm: &Orm) -> Vec<Vec<Option<String>>> {
    rows(
        orm,
        "SELECT operation, actor_uid::text, actor_kind, session_id::text \
         FROM ir_module_wasm_audit ORDER BY id",
    )
    .await
}

/// grants, can read the package store but not write it, and can do nothing on the audit (load() reads it as the owner); the owner
/// role cannot change or remove an audit row either, and the rows survive every attempt.
#[tokio::test]
async fn the_audit_is_insert_only_and_the_runtime_role_cannot_write_the_lifecycle_tables() {
    let (w, _addons) = world().await;
    committed(
        w.life
            .install(&su(&w.orm), consent(&wlife("1.0.0", &["note"])))
            .await
            .unwrap(),
    );
    let before = audit_rows(&w.orm).await;
    assert_eq!(before.len(), 1);

    // a runtime role as the release creates it: everything granted, then the lifecycle revokes
    let role = format!("erp_wl_runtime_{}", std::process::id());
    let admin = w.db.client().await.unwrap();
    admin
        .batch_execute(&format!(
            "DROP ROLE IF EXISTS {role}; CREATE ROLE {role} NOLOGIN; \
             GRANT USAGE ON SCHEMA public TO {role}; \
             GRANT ALL ON ALL TABLES IN SCHEMA public TO {role}; \
             GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO {role}"
        ))
        .await
        .unwrap();
    let mut tx = w.orm.begin(SUPERUSER_ID).await.unwrap();
    revoke_wasm_lifecycle_runtime_writes(&mut tx, &role)
        .await
        .unwrap();
    // idempotent
    revoke_wasm_lifecycle_runtime_writes(&mut tx, &role)
        .await
        .unwrap();
    // the owner of the tables cannot be the runtime role
    let owner: String = tx
        .query(
            "SELECT pg_get_userbyid(relowner)::text FROM pg_class \
             WHERE oid = 'ir_module_wasm_audit'::regclass",
            &[],
        )
        .await
        .unwrap()[0]
        .get(0);
    let err = revoke_wasm_lifecycle_runtime_writes(&mut tx, &owner)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        err.message(),
        format!("role {owner:?} owns ir_module_wasm_audit: the runtime role must be another role")
    );
    tx.commit().await.unwrap();

    // as the runtime role: statements are refused by privilege, not by luck
    admin
        .batch_execute(&format!("SET ROLE {role}"))
        .await
        .unwrap();
    let denied = |what: &'static str, result: Result<u64, erp_testkit::tokio_postgres::Error>| {
        let err = result.expect_err(what);
        assert_eq!(
            err.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{what}: {err}"
        );
    };
    denied(
        "audit select",
        admin
            .execute("SELECT 1 FROM ir_module_wasm_audit", &[])
            .await,
    );
    denied(
        "the read load() makes",
        admin
            .execute(erp_modules::wasm_lifecycle::INSTALLED_PACKAGES_SQL, &[])
            .await,
    );
    denied(
        "audit insert",
        admin
            .execute(
                "INSERT INTO ir_module_wasm_audit \
                 (operation, module, version, capabilities, actor_uid, actor_kind) \
                 VALUES ('install', 'forged', '1.0.0', '[]', 1, 'operator')",
                &[],
            )
            .await,
    );
    denied(
        "audit update",
        admin
            .execute("UPDATE ir_module_wasm_audit SET module = 'x'", &[])
            .await,
    );
    denied(
        "audit removal",
        admin.execute("DELETE FROM ir_module_wasm_audit", &[]).await,
    );
    denied(
        "audit truncate",
        admin.execute("TRUNCATE ir_module_wasm_audit", &[]).await,
    );
    // the package store: readable, never written
    admin
        .execute("SELECT 1 FROM ir_module_package", &[])
        .await
        .expect("the runtime role reads the package store");
    denied(
        "package insert",
        admin
            .execute(
                "INSERT INTO ir_module_package (name, version, sha256, package_data, capabilities) \
                 VALUES ('forged', '1.0.0', repeat('a', 64), '\\x00', '[]')",
                &[],
            )
            .await,
    );
    denied(
        "package update",
        admin
            .execute("UPDATE ir_module_package SET version = '9.9.9'", &[])
            .await,
    );
    denied(
        "package removal",
        admin.execute("DELETE FROM ir_module_package", &[]).await,
    );
    denied(
        "package truncate",
        admin.execute("TRUNCATE ir_module_package", &[]).await,
    );
    admin.batch_execute("RESET ROLE").await.unwrap();
    drop(admin);

    // the owner role: the trigger refuses what the grants would allow it
    for (what, sql, op) in [
        (
            "update",
            "UPDATE ir_module_wasm_audit SET module = 'x'",
            "UPDATE",
        ),
        ("removal", "DELETE FROM ir_module_wasm_audit", "DELETE"),
        ("truncate", "TRUNCATE ir_module_wasm_audit", "TRUNCATE"),
    ] {
        let mut tx = w.orm.begin(SUPERUSER_ID).await.unwrap();
        let err = tx.execute(sql, &[]).await.expect_err(what);
        assert!(
            err.to_string().contains(&format!(
                "ir_module_wasm_audit is insert-only ({op} refused)"
            )),
            "{what}: {err}"
        );
        tx.rollback().await.unwrap();
    }
    assert_eq!(
        audit_rows(&w.orm).await,
        before,
        "no attempt changed the audit"
    );
    w.db.cleanup().await.unwrap();
}

/// own uid and session id are recorded for install, upgrade and uninstall, with kind `session`.
#[tokio::test]
async fn an_authorized_administrators_uid_and_session_are_audited() {
    let (w, _addons) = world().await;
    let admin = Admins {
        user: 2,
        admins: &[2],
    };
    let installed = w
        .life
        .install_authorized(&admin, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    committed_ref(&installed);
    assert_eq!(
        shout(&installed.orm, "a").await.unwrap(),
        expected_shout("a")
    );
    let upgraded = w
        .life
        .upgrade_authorized(&admin, consent(&wlife("1.1.0", &["note"])))
        .await
        .unwrap();
    committed_ref(&upgraded);
    let removed = w
        .life
        .uninstall_authorized(&admin, &n("wlife"))
        .await
        .unwrap();
    committed_ref(&removed);
    let expected = |op: &str| {
        vec![
            Some(op.to_owned()),
            Some("2".to_owned()),
            Some("session".to_owned()),
            Some(SESSION.to_owned()),
        ]
    };
    assert_eq!(
        audit_rows(&removed.orm).await,
        [
            expected("install"),
            expected("upgrade"),
            expected("uninstall")
        ]
    );
    w.db.cleanup().await.unwrap();
}

/// administration right is refused by the authorizer inside the operation, for install, upgrade and
/// uninstall, and nothing persists (no package, no audit row, no signal, no live package); an
/// ordinary user's Env is refused on the operator path before the package is looked at.
#[tokio::test]
async fn callers_without_the_right_are_refused_and_nothing_persists() {
    let (w, _addons) = world().await;
    let before = generation(&w.db).await;
    let nobody = Admins {
        user: 3,
        admins: &[2],
    };
    let refused = "user 3 has no module administration right";

    let err = w
        .life
        .install_authorized(&nobody, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap_err();
    assert!(err.to_string().contains(refused), "{err}");
    assert_eq!(package_rows(&w.orm).await, 0);
    assert!(audit_rows(&w.orm).await.is_empty());
    assert_eq!(generation(&w.db).await, before);
    assert!(w.life.packages().live_modules().is_empty());
    assert_ne!(
        state_opt(&w.orm, "wlife").await.as_deref(),
        Some("installed")
    );

    // an administrator installs it; the user without the right cannot upgrade or uninstall it
    let admin = Admins {
        user: 2,
        admins: &[2],
    };
    let installed = w
        .life
        .install_authorized(&admin, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    let err = w
        .life
        .upgrade_authorized(&nobody, consent(&wlife("1.1.0", &["note"])))
        .await
        .unwrap_err();
    assert!(err.to_string().contains(refused), "{err}");
    let err = w
        .life
        .uninstall_authorized(&nobody, &n("wlife"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains(refused), "{err}");
    assert_eq!(state(&installed.orm, "wlife").await, "installed");
    assert_eq!(
        audit_rows(&installed.orm).await.len(),
        1,
        "only the install is audited"
    );
    assert_eq!(package_rows(&installed.orm).await, 1);

    // the operator path: an ordinary user's Env (and a package that would trap if it were looked at)
    let user = installed
        .orm
        .env(UserId::new(2).unwrap(), erp_orm::Context::new());
    let trap = archive("wtrap", "1.0.0", &["nat"], &[], "TRAP", None);
    let err = w.life.install(&user, consent(&trap)).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::AccessDenied {
                method: "WasmLifecycle::install",
                uid: 2
            }
        ),
        "{err}"
    );
    let err = w
        .life
        .upgrade(&user, consent(&wlife("1.1.0", &["note"])))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::AccessDenied {
                method: "WasmLifecycle::upgrade",
                uid: 2
            }
        ),
        "{err}"
    );
    let err = w.life.uninstall(&user, &n("wlife")).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::AccessDenied {
                method: "WasmLifecycle::uninstall",
                uid: 2
            }
        ),
        "{err}"
    );
    // sudo of another user is not the superuser either
    let sudo = user.with_user(SUPERUSER_ID);
    let err = w.life.uninstall(&sudo, &n("wlife")).await.unwrap_err();
    assert!(matches!(err, Error::AccessDenied { uid: 2, .. }), "{err}");
    assert_eq!(audit_rows(&installed.orm).await.len(), 1);
    w.db.cleanup().await.unwrap();
}

/// install is staged and waiting for the operation lock, a plain operation of the same process
/// installs another addon without being refused, and the staged install still completes.
#[tokio::test]
async fn a_plain_operation_during_a_staged_install_is_not_refused() {
    let addons = fixture_addons();
    let db = TestDb::create().await.unwrap();
    let (system, life) = build_system(&db, &addons).await;
    let staged = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let life = Arc::new(life.with_after_stage_hook({
        let (staged, release) = (Arc::clone(&staged), Arc::clone(&release));
        move || {
            let (staged, release) = (Arc::clone(&staged), Arc::clone(&release));
            Box::pin(async move {
                staged.notify_one();
                release.notified().await;
            })
        }
    }));
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let base = system
        .button_immediate_install(&su(&init.orm), &[n("nat")])
        .await
        .unwrap();

    let env = su(&base.orm);
    let installing = tokio::spawn({
        let life = Arc::clone(&life);
        let env = env.clone();
        async move {
            life.install(&env, consent(&wlife("1.0.0", &["note"])))
                .await
        }
    });
    staged.notified().await;
    // the install is staged (its manifest is in the index); a plain operation runs meanwhile
    let plain = system
        .button_immediate_install(&env, &[n("nat2")])
        .await
        .expect("an unrelated plain operation must not be refused by a staged package");
    committed_ref(&plain);
    assert_eq!(state(&plain.orm, "nat2").await, "installed");
    assert!(
        !has_column(&plain.orm, "note").await,
        "the staged package is not in its registry"
    );
    release.notify_one();
    let outcome = installing.await.unwrap().unwrap();
    committed_ref(&outcome);
    assert_eq!(state(&outcome.orm, "wlife").await, "installed");
    assert_eq!(shout(&outcome.orm, "x").await.unwrap(), expected_shout("x"));
    db.cleanup().await.unwrap();
}

/// refused as reserved, a package named like an installed native addon is refused as installed, a
/// package named like a native addon that is not installed yet is refused because its source takes
/// precedence in discovery (found by this test: it used to be stored and audited while the
/// development folder ran), and none of
/// them persists anything or leaves the addon index changed (the native addon still installs).
#[tokio::test]
async fn a_package_named_like_a_native_addon_is_refused() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let as_package = |name: &str| {
        archive(
            name,
            "1.0.0",
            &[],
            &[],
            &describe(name, &[], MODEL, &[], &[]),
            None,
        )
    };
    for reserved in ["base", "web"] {
        let err = w
            .life
            .install(&env, consent(&as_package(reserved)))
            .await
            .unwrap_err();
        assert!(
            matches!(wasm_reason(&err), WasmReason::State(m)
                if m == &format!("'{reserved}' is a native core addon; a package cannot take its name")),
            "{err}"
        );
    }
    let err = w
        .life
        .install(&env, consent(&as_package("nat")))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::State(m) if m.starts_with("already installed")),
        "{err}"
    );
    // a native addon that exists but is not installed: in discovery the development folder takes
    // precedence over a package, so the package would be stored but never run; refused
    let err = w
        .life
        .install(&env, consent(&as_package("nat2")))
        .await
        .unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::State(m)
            if m.starts_with("another addon source takes precedence over the package (dev-folder:")),
        "{err}"
    );
    assert_eq!(package_rows(&w.orm).await, 0);
    assert!(audit_rows(&w.orm).await.is_empty());
    assert!(w.life.packages().live_modules().is_empty());
    // the refused package left no entry in the index: the native addon installs as before
    let native = w
        .system
        .button_immediate_install(&env, &[n("nat2")])
        .await
        .unwrap();
    committed_ref(&native);
    assert_eq!(state(&native.orm, "nat2").await, "installed");
    w.db.cleanup().await.unwrap();
}

/// not write (no matching install or upgrade audit row) is not loaded, even though it is a valid,
/// self-consistent archive and the newest row of its name.
#[tokio::test]
async fn a_package_row_without_an_audit_row_is_not_loaded() {
    let (w, addons) = world().await;
    committed(
        w.life
            .install(&su(&w.orm), consent(&wlife("1.0.0", &["note"])))
            .await
            .unwrap(),
    );
    // written outside the lifecycle: a valid newer archive of the same addon, no audit row
    let forged = consent(&wlife("9.9.9", &["other"]));
    let mut tx = w.orm.begin(SUPERUSER_ID).await.unwrap();
    erp_modules::package::store_package(&mut tx, &forged)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let (_node, life2) = build_system(&w.db, &addons).await;
    let err = life2.load().await.unwrap_err();
    assert!(
        matches!(wasm_reason(&err), WasmReason::StoredPackage(m)
            if m == "no install or upgrade row of the lifecycle audit records this archive"),
        "{err}"
    );
    assert!(
        life2.packages().live_modules().is_empty(),
        "nothing half-loaded"
    );
    w.db.cleanup().await.unwrap();
}

/// model too many or one field too many is refused as too large before anything is built or
/// persisted, and installs that follow work.
#[tokio::test]
async fn a_package_over_the_limits_is_refused_and_later_installs_work() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let before = generation(&w.db).await;
    let oversized = |extra: Vec<serde_json::Value>| {
        let mut doc: serde_json::Value =
            serde_json::from_str(&describe("wlife", &["nat"], MODEL, &["note"], &[])).unwrap();
        doc["contributions"].as_array_mut().unwrap().extend(extra);
        archive("wlife", "1.0.0", &["nat"], &[], &doc.to_string(), None)
    };
    let methods: Vec<_> = (0..=MAX_PACKAGE_METHODS)
        .map(|i| json!({"addon": "wlife", "kind": "method", "model": MODEL, "name": format!("m_{i}"), "detail": {"args": []}}))
        .collect();
    let models: Vec<_> = (0..=MAX_DESCRIBED_MODELS)
        .map(|i| {
            json!({"addon": "wlife", "kind": "model", "model": format!("wlife.m{i}"), "name": "M",
            "detail": {"args": {}, "fields": []}})
        })
        .collect();
    let fields = vec![
        json!({"addon": "wlife", "kind": "extend", "model": MODEL, "name": "Big",
        "detail": {"args": {}, "fields": (0..=MAX_DESCRIBED_FIELDS)
            .map(|i| json!({"name": format!("g{i}"), "kind": "char", "attrs": {}})).collect::<Vec<_>>()}}),
    ];
    for (extra, what) in [
        (methods, format!("more than {MAX_PACKAGE_METHODS} methods")),
        (models, format!("more than {MAX_DESCRIBED_MODELS} models")),
        (fields, format!("more than {MAX_DESCRIBED_FIELDS} fields")),
    ] {
        let err = w
            .life
            .install(&env, consent(&oversized(extra)))
            .await
            .unwrap_err();
        assert!(
            matches!(wasm_reason(&err), WasmReason::TooLarge(m) if m == &what),
            "{what}: {err}"
        );
    }
    assert_eq!(package_rows(&w.orm).await, 0);
    assert_eq!(generation(&w.db).await, before);
    let ok = w
        .life
        .install(&env, consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    assert_eq!(
        shout(&ok.orm, "fine").await.unwrap(),
        expected_shout("fine")
    );
    w.db.cleanup().await.unwrap();
}

/// a reference that matches the installed model installs and runs; a missing field, a missing
/// model and a handle of another kind are refused by name, with nothing written, and the install
/// that follows works.
#[tokio::test]
async fn typed_model_references_are_checked_at_install() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let before = generation(&w.db).await;
    let with_ref = |model: &str, field: serde_json::Value| {
        let mut doc: serde_json::Value =
            serde_json::from_str(&describe("wlife", &["nat"], MODEL, &["note"], &["shout"]))
                .unwrap();
        doc["contributions"].as_array_mut().unwrap().push(
            json!({"addon": "wlife", "kind": "model_ref", "model": model, "name": "NatRef",
                   "detail": {"fields": [field]}}),
        );
        archive("wlife", "1.0.0", &["nat"], &[], &doc.to_string(), None)
    };
    let char_field = |name: &str| json!({"name": name, "kinds": ["char", "text"], "comodel": null});
    for (model, field, needle) in [
        (
            MODEL,
            char_field("nope"),
            "model 'test.nat' has no field 'nope'",
        ),
        (
            "test.missing",
            char_field("name"),
            "model 'test.missing' is not in the registry",
        ),
        (
            MODEL,
            json!({"name": "name", "kinds": ["integer"], "comodel": null}),
            "test.nat.name is a char field but the handle's Rust type holds [integer]",
        ),
    ] {
        let err = w
            .life
            .install(&env, consent(&with_ref(model, field)))
            .await
            .unwrap_err();
        assert!(
            matches!(wasm_reason(&err), WasmReason::ModelRef(m) if m.contains(needle)),
            "{needle}: {err}"
        );
        assert!(err.to_string().contains("wlife"), "names the addon: {err}");
    }
    assert_eq!(package_rows(&w.orm).await, 0);
    assert_eq!(generation(&w.db).await, before);
    assert_eq!(state_opt(&w.orm, "wlife").await, None);
    let ok = w
        .life
        .install(&env, consent(&with_ref(MODEL, char_field("name"))))
        .await
        .unwrap();
    assert_eq!(
        shout(&ok.orm, "fine").await.unwrap(),
        expected_shout("fine")
    );
    w.db.cleanup().await.unwrap();
}

/// package addon, never a native or data-only addon): a native install of it is refused with the
/// reason, the catalogue lists it as refused, and the package of the same name still installs and
/// runs (the package wins over the folder), also after the module set is loaded again.
#[tokio::test]
async fn a_dev_folder_with_a_wasm_section_is_never_installed_natively_and_the_package_wins() {
    let (w, addons) = world().await;
    addons.add(
        "wlife",
        "name = 'WLife'\ndepends = ['nat']\n[wasm]\nsdk_version = '1.0'\n",
    );
    w.system.with_index(|index| index.invalidate_cache());

    let err = w
        .system
        .button_immediate_install(&su(&w.orm), &[n("wlife")])
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::WasmAddon { reason: WasmReason::State(text), .. }
            if text.contains("erp addon install")),
        "{err}"
    );
    let catalog = w.system.human_catalog().await.unwrap();
    assert!(catalog.offered.iter().all(|m| m.name().as_str() != "wlife"));
    let refused = catalog
        .refused
        .iter()
        .find(|(name, _)| name.as_str() == "wlife")
        .expect("the folder is listed as refused");
    assert!(refused.1.contains("[wasm] section"), "{}", refused.1);
    assert_eq!(
        w.db.client()
            .await
            .unwrap()
            .query_one(
                "SELECT count(*) FROM ir_module_module WHERE name = 'wlife' AND state <> 'uninstalled'",
                &[]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
        0,
        "nothing was installed natively"
    );

    // the package of the same name installs normally and is what runs
    let outcome = w
        .life
        .install(&su(&w.orm), consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    committed_ref(&outcome);
    assert_eq!(
        shout(&outcome.orm, "hi").await.unwrap(),
        expected_shout("hi")
    );
    let loaded = w.life.load().await.unwrap();
    assert_eq!(
        shout(&loaded.orm, "again").await.unwrap(),
        expected_shout("again")
    );
}

// ---- data files of a package ------------------------------------------------------

/// A `wlife` package whose manifest lists `data` and `demo` files; `files` are the archive's extra
/// entries (path, text).
fn wlife_with_data(data: &[&str], demo: &[&str], files: &[(&str, &str)]) -> Vec<u8> {
    let list = |paths: &[&str]| {
        paths
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file(
        "manifest.toml",
        // the list keys come before the [wasm] table the manifest text ends with
        format!("data = [{}]\ndemo = [{}]\n", list(data), list(demo))
            + &manifest_text("wlife", "1.0.0", &["nat"], &[]),
    );
    writer.add_file(
        "wasm/wlife.wasm",
        describe("wlife", &["nat"], MODEL, &["note"], &["shout"]).as_bytes(),
    );
    for (path, text) in files {
        writer.add_file(*path, text.as_bytes());
    }
    writer.build_zip().unwrap()
}

/// package refuses by name and nothing is installed: demo data, SQL files, a listed file the
/// archive does not have.
#[tokio::test]
async fn data_files_a_package_cannot_carry_are_refused_by_name() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let cases: [(Vec<u8>, &str); 6] = [
        (
            wlife_with_data(&["other/v.xml"], &[], &[("other/v.xml", "<erp/>")]),
            "live in views/, data/ or security/",
        ),
        (
            wlife_with_data(&["v.xml"], &[], &[("v.xml", "<erp/>")]),
            "live in views/, data/ or security/",
        ),
        (
            wlife_with_data(&["views/v.json"], &[], &[("views/v.json", "{}")]),
            "views/v.json",
        ),
        (
            wlife_with_data(
                &["views/v.xml"],
                &["demo/d.xml"],
                &[("views/v.xml", "<erp/>"), ("demo/d.xml", "<erp/>")],
            ),
            "demo data",
        ),
        (
            wlife_with_data(&["data/x.sql"], &[], &[("data/x.sql", "SELECT 1")]),
            "data/x.sql",
        ),
        (
            wlife_with_data(&["views/missing.xml"], &[], &[]),
            "views/missing.xml",
        ),
    ];
    for (bytes, needle) in cases {
        let err = w.life.install(&env, consent(&bytes)).await.unwrap_err();
        assert!(
            matches!(&err, Error::WasmAddon { .. }) && err.to_string().contains(needle),
            "{needle}: {err}"
        );
        assert_eq!(state_opt(&w.orm, "wlife").await, None, "{needle}");
        assert_eq!(package_rows(&w.orm).await, 0, "{needle}");
    }
}

/// but views, window actions, menus and access rows of the package's own models fails the install,
/// which rolls back whole (no catalog row, no package row, no registry change, no new column).
#[tokio::test]
async fn a_data_file_outside_what_a_package_may_declare_rolls_the_install_back() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let generation_before = generation(&w.db).await;
    let cases: [(&str, &str, &str); 4] = [
        (
            "views/users.xml",
            r#"<erp><record id="u" model="res.partner"><field name="name">x</field></record></erp>"#,
            "'res.partner' is not allowed",
        ),
        (
            "views/fn.xml",
            r#"<erp><function model="res.partner" name="unlink"/></erp>"#,
            "<function> is not allowed",
        ),
        (
            "views/del.xml",
            r#"<erp><delete model="ir.ui.view" search="[]"/></erp>"#,
            "<delete> is not allowed",
        ),
        (
            "security/ir.access.csv",
            "id,name,model_id/id,group_id/id,operation\n\
             grab,Grab,base.model_res_users,base.group_user,r\n",
            "may only grant access on models it declares itself",
        ),
    ];
    for (path, text, needle) in cases {
        let bytes = wlife_with_data(&[path], &[], &[(path, text)]);
        let err = w.life.install(&env, consent(&bytes)).await.unwrap_err();
        assert!(err.to_string().contains(needle), "{path}: {err}");
        assert!(
            err.to_string().contains(path),
            "{path}: names the file: {err}"
        );
        assert_eq!(state_opt(&w.orm, "wlife").await, None, "{path}");
        assert_eq!(package_rows(&w.orm).await, 0, "{path}");
        assert!(!has_column(&w.orm, "note").await, "{path}");
        assert_eq!(generation(&w.db).await, generation_before, "{path}");
    }
}

/// argument list, or with an argument that has no name or type, is refused at install naming the
/// method and the field; nothing is defaulted and nothing is installed.
#[tokio::test]
async fn a_method_description_without_arguments_is_refused_at_install() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    for (detail, needle) in [
        (json!({}), "no `args` list"),
        (
            json!({"args": [{"type": "String"}]}),
            "argument 0 has no `name`",
        ),
        (
            json!({"args": [{"name": "text"}]}),
            "argument 0 has no `type`",
        ),
    ] {
        let mut list = vec![
            json!({"addon": "wlife", "kind": "addon", "model": "", "name": "wlife",
                   "detail": {"depends": ["base", "nat"]}}),
        ];
        list.push(
            json!({"addon": "wlife", "kind": "method", "model": MODEL, "name": "shout",
                         "detail": detail}),
        );
        let text = json!({"schema": 1, "contributions": list}).to_string();
        let bytes = archive("wlife", "1.0.0", &["nat"], &[], &text, None);
        let err = w.life.install(&env, consent(&bytes)).await.unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.contains(needle) && shown.contains("test.nat.shout"),
            "{needle}: {shown}"
        );
        assert_eq!(state_opt(&w.orm, "wlife").await, None, "{needle}");
        assert_eq!(package_rows(&w.orm).await, 0, "{needle}");
    }
}

/// nor a method is refused at install (it would only claim a model of another addon); one that adds a
/// method is fine.
#[tokio::test]
async fn an_empty_extension_is_refused_at_install() {
    let (w, _addons) = world().await;
    let env = su(&w.orm);
    let text = |methods: &[&str]| {
        let mut list = vec![
            json!({"addon": "wlife", "kind": "addon", "model": "", "name": "wlife",
                   "detail": {"depends": ["base", "nat"]}}),
            json!({"addon": "wlife", "kind": "extend", "model": MODEL, "name": "Ext",
                   "detail": {"target": "Nat", "args": {}, "fields": []}}),
        ];
        for m in methods {
            list.push(
                json!({"addon": "wlife", "kind": "method", "model": MODEL, "name": m,
                             "detail": {"args": [{"name": "text", "type": "String"}]}}),
            );
        }
        json!({"schema": 1, "contributions": list}).to_string()
    };
    let empty = archive("wlife", "1.0.0", &["nat"], &[], &text(&[]), None);
    let err = w.life.install(&env, consent(&empty)).await.unwrap_err();
    assert!(
        err.to_string()
            .contains("extends 'test.nat' but adds no field and no method"),
        "{err}"
    );
    assert_eq!(state_opt(&w.orm, "wlife").await, None);
    assert_eq!(package_rows(&w.orm).await, 0);
    let with_method = archive("wlife", "1.0.0", &["nat"], &[], &text(&["shout"]), None);
    let outcome = w.life.install(&env, consent(&with_method)).await.unwrap();
    committed_ref(&outcome);
    assert_eq!(
        w.life.extended_models(&n("wlife")).unwrap(),
        vec![MODEL.to_owned()]
    );
}

/// An unknown package is an error, not an empty list (security review r2 L1).
#[tokio::test]
async fn the_exposure_of_an_unknown_package_is_an_error() {
    let (w, _addons) = world().await;
    assert!(w.life.exposed_methods(&n("nothing")).is_err());
    assert!(w.life.extended_models(&n("nothing")).is_err());
}

// Native-upgrade package-view checks run in ordinary CI as well as the real-component PoC.
static PACKAGE_VIEW_CHECKS: std::sync::Mutex<Vec<Vec<i64>>> = std::sync::Mutex::new(Vec::new());
static REFUSE_PACKAGE_VIEW: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn check_package_views<'a>(
    _tx: &'a mut Transaction,
    records: &'a Records,
    (): (),
    _sup: erp_orm::Super<(), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async move {
        assert!(!records.env().context().contains("install_module"));
        if records.is_empty() {
            return Ok(());
        }
        PACKAGE_VIEW_CHECKS.lock().unwrap().push(records.ids());
        if REFUSE_PACKAGE_VIEW.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(erp_fields::Error::validation(
                "package view no longer applies",
            ));
        }
        Ok(())
    })
}

fn check_package_views_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static F: erp_orm::MethodFn<(), ()> = check_package_views;
    &F
}

static PACKAGE_VIEW_CHECK: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: "nat",
    model: "ir.ui.view",
    method: "_check_xml",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<erp_orm::MethodFn<(), ()>>,
    entry: check_package_views_entry,
    file: file!(),
    line: line!(),
};

// The native upgrade's existing noupdate check is a separate contract from _check_xml.
fn native_view_check<'a>(
    _tx: &'a mut Transaction,
    _records: &'a Records,
    _args: (String, Vec<String>, Vec<String>),
    _sup: erp_orm::Super<(String, Vec<String>, Vec<String>), ()>,
) -> BoxFuture<'a, erp_fields::Result<()>> {
    Box::pin(async { Ok(()) })
}

fn native_view_check_entry() -> &'static (dyn std::any::Any + Send + Sync) {
    static F: erp_orm::MethodFn<(String, Vec<String>, Vec<String>), ()> = native_view_check;
    &F
}

static NATIVE_VIEW_CHECK: erp_orm::MethodImpl = erp_orm::MethodImpl {
    addon: "nat",
    model: "ir.ui.view",
    method: "_validate_module_views",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<erp_orm::MethodFn<(String, Vec<String>, Vec<String>), ()>>,
    entry: native_view_check_entry,
    file: file!(),
    line: line!(),
};

async fn package_view_world(validator: bool) -> (World, Addons) {
    let addons = fixture_addons();
    let db = TestDb::create().await.unwrap();
    let packages = WasmPackages::new(Arc::new(StubLoader));
    let mut builder = builder(&db, &addons)
        .decls([model("nat", MODEL), model("nat", "ir.ui.view")])
        .impls([&NATIVE_VIEW_CHECK])
        .package_runtime(packages.clone());
    if validator {
        builder = builder.impls([&PACKAGE_VIEW_CHECK]);
    }
    let system = Arc::new(builder.build().unwrap());
    let life = WasmLifecycle::new(Arc::clone(&system), packages);
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let native = system
        .button_immediate_install(&su(&init.orm), &[n("nat")])
        .await
        .unwrap();
    let installed = life
        .install(&su(&native.orm), consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    (
        World {
            db,
            system,
            life,
            orm: installed.orm,
        },
        addons,
    )
}

/// actual view composition and a second running node are covered by the CLI PoC)
#[tokio::test]
async fn native_upgrade_checks_every_package_view_in_bounded_windows() {
    PACKAGE_VIEW_CHECKS.lock().unwrap().clear();
    REFUSE_PACKAGE_VIEW.store(false, std::sync::atomic::Ordering::SeqCst);
    let (w, _addons) = package_view_world(true).await;
    let empty = w
        .system
        .button_immediate_upgrade(&su(&w.orm), &[n("nat")])
        .await
        .unwrap();
    assert!(PACKAGE_VIEW_CHECKS.lock().unwrap().is_empty());
    // 257 crosses the implementation's 256-id window. Aliases must not duplicate work;
    // unrelated native views and external ids of other model kinds must not be selected.
    exec(
        &empty.orm,
        "INSERT INTO ir_ui_view (id, name) SELECT i, 'view' FROM generate_series(1, 259) i;
        INSERT INTO ir_model_data (module, name, model, res_id)
        SELECT 'wlife', 'view_' || i, 'ir.ui.view', i FROM generate_series(1, 257) i;
        INSERT INTO ir_model_data (module, name, model, res_id) VALUES
        ('wlife', 'alias', 'ir.ui.view', 1), ('nat', 'native', 'ir.ui.view', 258),
        ('wlife', 'other', 'test.nat', 259)",
    )
    .await;
    let expected: Vec<i64> =
        w.db.client()
            .await
            .unwrap()
            .query("SELECT id FROM ir_ui_view WHERE id <= 257 ORDER BY id", &[])
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
    let upgraded = w
        .system
        .button_immediate_upgrade(&su(&empty.orm), &[n("nat")])
        .await
        .unwrap();
    let calls = PACKAGE_VIEW_CHECKS.lock().unwrap().clone();
    assert_eq!(calls.iter().map(Vec::len).collect::<Vec<_>>(), [256, 1]);
    assert_eq!(calls.into_iter().flatten().collect::<Vec<_>>(), expected);
    REFUSE_PACKAGE_VIEW.store(true, std::sync::atomic::Ordering::SeqCst);
    let error = w
        .system
        .button_immediate_upgrade(&su(&upgraded.orm), &[n("nat")])
        .await
        .unwrap_err();
    let Error::Orm(error) = error else {
        panic!("wrong error: {error}")
    };
    assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
    assert_eq!(error.to_string(), "package view no longer applies");
    assert_eq!(state(&upgraded.orm, "nat").await, "installed");
    w.db.cleanup().await.unwrap();
}

#[tokio::test]
async fn native_upgrade_refuses_a_missing_package_view_validator() {
    let (w, _addons) = package_view_world(false).await;
    let error = w
        .system
        .button_immediate_upgrade(&su(&w.orm), &[n("nat")])
        .await
        .unwrap_err();
    let Error::Orm(error) = error else {
        panic!("wrong error: {error}")
    };
    assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        "'ir.ui.view' object has no attribute '_check_xml'"
    );
    w.db.cleanup().await.unwrap();
}

#[tokio::test]
async fn installed_package_admission_does_not_replay_pending_work_and_enables_legacy_enrollment() {
    let (w, addons) = world().await;
    let bytes = wlife("1.0.0", &["note"]);
    let sha = ErpAddonPackage::from_bytes(&bytes)
        .unwrap()
        .archive_sha256()
        .to_owned();
    let installed = w.life.install(&su(&w.orm), consent(&bytes)).await.unwrap();
    committed_ref(&installed);
    let (fresh_system, fresh_life) = build_system(&w.db, &addons).await;
    assert!(fresh_life.packages().live_modules().is_empty());
    assert!(
        !fresh_system
            .with_index(|index| index.discover_entries().unwrap().contains_key(&n("wlife")))
    );
    exec(
        &installed.orm,
        "UPDATE ir_module_module SET state='to install' WHERE name='nat2'",
    )
    .await;
    let catalog = rows(
        &installed.orm,
        "SELECT row_to_json(m)::text FROM ir_module_module m ORDER BY name",
    )
    .await;
    let audit = audit_rows(&installed.orm).await;
    let identity_audit = rows(
        &installed.orm,
        "SELECT row_to_json(a)::text FROM installed_addon_identity_audit a ORDER BY id",
    )
    .await;
    let before_generation = generation(&w.db).await;

    fresh_life.admit_installed_packages().await.unwrap();

    assert_eq!(fresh_life.packages().live_modules(), vec![n("wlife")]);
    assert!(
        fresh_system
            .with_index(|index| index.discover_entries().unwrap().contains_key(&n("wlife")))
    );
    assert_eq!(state(&installed.orm, "nat2").await, "to install");
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(m)::text FROM ir_module_module m ORDER BY name"
        )
        .await,
        catalog
    );
    assert_eq!(audit_rows(&installed.orm).await, audit);
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(a)::text FROM installed_addon_identity_audit a ORDER BY id"
        )
        .await,
        identity_audit
    );
    assert_eq!(generation(&w.db).await, before_generation);

    // Explicit operator recovery is independent of ordinary load: reset the pending fixture,
    // then simulate the missing receipt of a legacy database without altering its package row.
    exec(&installed.orm, "UPDATE ir_module_module SET state='uninstalled' WHERE name='nat2'; DELETE FROM installed_addon_identity WHERE name='wlife'").await;
    fresh_system
        .identity_enroll(
            &[n("wlife")],
            "package-operator@example.test",
            "Legacy package archive and approval reviewed",
        )
        .await
        .unwrap();
    let expected_hash = format!("package-v1:{sha}");
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT name,content_hash,source_kind FROM installed_addon_identity WHERE name='wlife'"
        )
        .await,
        vec![vec![
            Some("wlife".into()),
            Some(expected_hash.clone()),
            Some("package".into())
        ]]
    );
    assert_eq!(rows(&installed.orm, "SELECT operator,note,operation,content_hashes->>'wlife',(created_at IS NOT NULL)::text FROM installed_addon_identity_audit ORDER BY id").await,
        vec![vec![Some("package-operator@example.test".into()), Some("Legacy package archive and approval reviewed".into()), Some("enroll".into()), Some(expected_hash), Some("true".into())]]);
    assert_eq!(audit_rows(&installed.orm).await, audit);
    assert_eq!(state(&installed.orm, "nat2").await, "uninstalled");
    w.db.cleanup().await.unwrap();
}

#[tokio::test]
async fn installed_package_admission_refuses_corrupt_archive_without_registering_it() {
    let (w, addons) = world().await;
    let bytes = wlife("1.0.0", &["note"]);
    let original_sha = ErpAddonPackage::from_bytes(&bytes)
        .unwrap()
        .archive_sha256()
        .to_owned();
    let installed = w.life.install(&su(&w.orm), consent(&bytes)).await.unwrap();
    committed_ref(&installed);
    let replacement = wlife("9.9.9", &["other"]);
    let replacement_sha = ErpAddonPackage::from_bytes(&replacement)
        .unwrap()
        .archive_sha256()
        .to_owned();
    exec_bytes(&installed.orm, &replacement).await;
    let catalog = rows(
        &installed.orm,
        "SELECT row_to_json(m)::text FROM ir_module_module m ORDER BY name",
    )
    .await;
    let audit = audit_rows(&installed.orm).await;
    let (fresh_system, fresh_life) = build_system(&w.db, &addons).await;
    let error = fresh_life.admit_installed_packages().await.unwrap_err();
    match error {
        Error::WasmAddon {
            addon,
            reason: WasmReason::StoredPackage(message),
        } => {
            assert_eq!(addon, n("wlife"));
            assert_eq!(
                message,
                format!(
                    "the stored archive hashes to {replacement_sha} but its row says {original_sha}"
                )
            );
        }
        other => panic!("expected stored-package integrity refusal, got {other:?}"),
    }
    assert!(fresh_life.packages().live_modules().is_empty());
    assert!(
        !fresh_system
            .with_index(|index| index.discover_entries().unwrap().contains_key(&n("wlife")))
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(m)::text FROM ir_module_module m ORDER BY name"
        )
        .await,
        catalog
    );
    assert_eq!(audit_rows(&installed.orm).await, audit);
    w.db.cleanup().await.unwrap();
}

#[tokio::test]
async fn package_upgrade_requires_the_current_durable_preimage() {
    let (w, _addons) = world().await;
    let installed = w
        .life
        .install(&su(&w.orm), consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    let env = su(&installed.orm);
    let original = rows(
        &installed.orm,
        "SELECT content_hash FROM installed_addon_identity WHERE name='wlife'",
    )
    .await[0][0]
        .clone()
        .unwrap();
    let before_package = rows(
        &installed.orm,
        "SELECT sha256::text FROM ir_module_package ORDER BY id",
    )
    .await;
    let before_audit = audit_rows(&installed.orm).await;
    let before_generation = generation(&w.db).await;
    for (mutation, message) in [
        (
            "DELETE FROM installed_addon_identity WHERE name='wlife'".to_owned(),
            "Addon 'wlife' identity unknown, run identity-enroll first",
        ),
        (
            "UPDATE installed_addon_identity SET content_hash='bad' WHERE name='wlife'".to_owned(),
            "Addon 'wlife' installed artifact identity is malformed",
        ),
        (
            format!(
                "UPDATE installed_addon_identity SET content_hash='package-v1:{}' WHERE name='wlife'",
                "0".repeat(64)
            ),
            "Addon 'wlife' installed artifact identity does not match available content",
        ),
    ] {
        exec(&installed.orm, &mutation).await;
        let error = w
            .life
            .upgrade(&env, consent(&wlife("2.0.0", &["note", "extra"])))
            .await
            .unwrap_err();
        match error {
            Error::Orm(error) => {
                assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
                assert_eq!(error.message(), message);
            }
            other => panic!("expected identity validation refusal, got {other:?}"),
        }
        assert_eq!(
            rows(
                &installed.orm,
                "SELECT sha256::text FROM ir_module_package ORDER BY id"
            )
            .await,
            before_package
        );
        assert_eq!(audit_rows(&installed.orm).await, before_audit);
        assert_eq!(generation(&w.db).await, before_generation);
        assert!(!has_column(&installed.orm, "extra").await);
        exec(&installed.orm, &format!("DELETE FROM installed_addon_identity WHERE name='wlife'; INSERT INTO installed_addon_identity(name,content_hash,installed_version,source_kind) VALUES ('wlife','{original}','1.0.0','package')")).await;
    }
    // The process still has the approved old archive. Corrupting storage must refuse even
    // when the receipt and stored digest column retain that old hash.
    exec_bytes(&installed.orm, &wlife("9.0.0", &["other"])).await;
    let error = w
        .life
        .upgrade(&env, consent(&wlife("2.0.0", &["note"])))
        .await
        .unwrap_err();
    match error {
        Error::Orm(error) => {
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Addon 'wlife' installed artifact identity does not match available content"
            );
        }
        other => panic!("expected corrupt-preimage refusal, got {other:?}"),
    }
    assert_eq!(audit_rows(&installed.orm).await, before_audit);
    assert_eq!(generation(&w.db).await, before_generation);
    w.db.cleanup().await.unwrap();
}

#[tokio::test]
async fn package_upgrade_receipt_is_atomic_and_rechecked_on_another_node() {
    let (w, addons) = world().await;
    let installed = w
        .life
        .install(&su(&w.orm), consent(&wlife("1.0.0", &["note"])))
        .await
        .unwrap();
    let (other_system, other_life) = build_system(&w.db, &addons).await;
    let other = other_life.load().await.unwrap();
    let receipt = rows(
        &installed.orm,
        "SELECT row_to_json(i)::text FROM installed_addon_identity i ORDER BY name",
    )
    .await;
    let audit = audit_rows(&installed.orm).await;
    let package = rows(
        &installed.orm,
        "SELECT sha256::text FROM ir_module_package ORDER BY id",
    )
    .await;
    exec(&installed.orm, "CREATE FUNCTION reject_package_upgrade() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.operation='upgrade' THEN RAISE EXCEPTION 'package audit failure fixture'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_package_upgrade BEFORE INSERT ON ir_module_wasm_audit FOR EACH ROW EXECUTE FUNCTION reject_package_upgrade()").await;
    let error = w
        .life
        .upgrade(
            &su(&installed.orm),
            consent(&wlife("2.0.0", &["note", "extra"])),
        )
        .await
        .unwrap_err();
    // PostgreSQL's error wrapper includes source details; assert the typed SQL failure and
    // stable server message rather than a host-dependent rendering of that wrapper.
    assert!(
        matches!(&error, Error::Orm(inner) if inner.kind() == erp_fields::ErrorKind::Database),
        "{error:?}"
    );
    assert!(
        format!("{error:?}").contains("package audit failure fixture"),
        "{error:?}"
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT row_to_json(i)::text FROM installed_addon_identity i ORDER BY name"
        )
        .await,
        receipt
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT sha256::text FROM ir_module_package ORDER BY id"
        )
        .await,
        package
    );
    assert_eq!(audit_rows(&installed.orm).await, audit);
    assert_eq!(state(&installed.orm, "wlife").await, "installed");
    exec(&installed.orm, "DROP TRIGGER reject_package_upgrade ON ir_module_wasm_audit; DROP FUNCTION reject_package_upgrade()").await;
    let upgraded = w
        .life
        .upgrade(
            &su(&installed.orm),
            consent(&wlife("2.0.0", &["note", "extra"])),
        )
        .await
        .unwrap();
    let error = other_life
        .upgrade(&su(&other.orm), consent(&wlife("3.0.0", &["note"])))
        .await
        .unwrap_err();
    match error {
        Error::Orm(error) => {
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Addon 'wlife' installed artifact identity does not match available content"
            );
        }
        other => panic!("expected stale-node refusal, got {other:?}"),
    }
    let fresh = other_life.load().await.unwrap();
    assert_eq!(
        rows(
            &fresh.orm,
            "SELECT content_hash FROM installed_addon_identity WHERE name='wlife'"
        )
        .await,
        rows(
            &upgraded.orm,
            "SELECT content_hash FROM installed_addon_identity WHERE name='wlife'"
        )
        .await
    );
    committed(
        other_life
            .uninstall(&su(&fresh.orm), &n("wlife"))
            .await
            .unwrap(),
    );
    drop(other_system);
    w.db.cleanup().await.unwrap();
}

#[tokio::test]
async fn verified_client_files_reach_transactional_lifecycle_admission() {
    use erp_modules::{LifecycleChange, LifecycleHook};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct VerifyClient {
        seen: AtomicBool,
        expected: BTreeMap<String, Vec<u8>>,
    }
    impl LifecycleHook for VerifyClient {
        fn before_commit<'a>(
            &'a self,
            _tx: &'a mut Transaction,
            context: LifecycleChange<'a>,
        ) -> BoxFuture<'a, erp_modules::Result<()>> {
            Box::pin(async move {
                if let Some(entry) = context
                    .active_addons()
                    .iter()
                    .find(|e| e.name().as_str() == "wlife")
                {
                    let data = entry.package_data().expect("verified package data");
                    assert_eq!(data.client_files, self.expected);
                    assert!(
                        data.files.is_empty(),
                        "client assets do not become loader data files"
                    );
                    self.seen.store(true, Ordering::SeqCst);
                }
                Ok(())
            })
        }
    }
    let expected: BTreeMap<String, Vec<u8>> = [
        ("static/dist/manifest.json", br#"{"schema":1,"runtime_abi":"elang-1","modules":[],"templates":["static/src/test.xml"],"static_files":["static/icon.svg"]}"#.as_slice()),
        ("static/src/test.xml", b"<templates/>".as_slice()),
        ("static/icon.svg", b"<svg/>".as_slice()),
        ("i18n/en.po", b"msgid \"hello\"\nmsgstr \"hello\"\n".as_slice()),
    ].into_iter().map(|(p,b)| (p.to_owned(), b.to_vec())).collect();
    let verify = Arc::new(VerifyClient {
        seen: AtomicBool::new(false),
        expected: expected.clone(),
    });
    let addons = fixture_addons();
    let db = TestDb::create().await.unwrap();
    let packages = WasmPackages::new(Arc::new(StubLoader));
    let system = Arc::new(
        builder(&db, &addons)
            .decls([model("nat", MODEL)])
            .signal(Signal::new(Generations::new()).with_refresh(KeepServing))
            .package_runtime(packages.clone())
            .lifecycle_hook(verify.clone())
            .build()
            .unwrap(),
    );
    let initialized = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let native = system
        .button_immediate_install(&su(&initialized.orm), &[n("nat")])
        .await
        .unwrap();
    let life = WasmLifecycle::new(Arc::clone(&system), packages);
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file(
        "manifest.toml",
        manifest_text("wlife", "1.0.0", &["nat"], &[]),
    );
    writer.add_file(
        "wasm/wlife.wasm",
        describe("wlife", &["nat"], MODEL, &["note"], &["shout"]),
    );
    for (path, bytes) in expected {
        writer.add_file(path, bytes);
    }
    let bytes = writer.build_zip().unwrap();
    let installed = life
        .install(&su(&native.orm), consent(&bytes))
        .await
        .unwrap();
    committed_ref(&installed);
    assert!(verify.seen.load(Ordering::SeqCst));
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn legacy_bootstrap_preserves_verified_installed_package_identity() {
    let (w, addons) = world().await;
    let bytes = wlife("1.0.0", &["note"]);
    let digest = format!(
        "package-v1:{}",
        ErpAddonPackage::from_bytes(&bytes)
            .unwrap()
            .archive_sha256()
    );
    let installed = w.life.install(&su(&w.orm), consent(&bytes)).await.unwrap();
    committed_ref(&installed);
    let before = rows(
        &installed.orm,
        "SELECT name,content_hash,source_kind FROM installed_addon_identity ORDER BY name",
    )
    .await;
    let files = before
        .iter()
        .filter(|row| row[0].as_deref() != Some("wlife"))
        .map(|row| (n(row[0].as_deref().unwrap()), row[1].clone().unwrap()))
        .collect();
    exec(&installed.orm, "DELETE FROM installed_addon_identity").await;
    let (system, life) = build_system(&w.db, &addons).await;
    life.admit_installed_packages().await.unwrap();
    system
        .identity_enroll_legacy_installed(
            &files,
            "package-operator",
            "reviewed legacy files and stored archive",
        )
        .await
        .unwrap();
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT name,content_hash,source_kind FROM installed_addon_identity ORDER BY name"
        )
        .await,
        before
    );
    assert_eq!(rows(&installed.orm, "SELECT content_hashes->>'wlife' FROM installed_addon_identity_audit WHERE operation='enroll'").await, vec![vec![Some(digest)]]);
    system
        .identity_activate_enrolled("operator", "unchanged package forward activation")
        .await
        .unwrap();
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT name,content_hash,source_kind FROM installed_addon_identity ORDER BY name"
        )
        .await,
        before
    );
    assert!(
        rows(
            &installed.orm,
            "SELECT operation FROM installed_addon_identity_audit WHERE operation='activate'"
        )
        .await
        .is_empty()
    );
    w.db.cleanup().await.unwrap();
}
