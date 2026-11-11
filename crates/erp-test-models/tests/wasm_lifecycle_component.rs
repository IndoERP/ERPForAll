//! Runtime install, upgrade and uninstall of a real WebAssembly component (
//! `.erpaddon`, installed into a running database through `erp-modules` and `erp-wasm-host`, its
//! method called through the ORM, then upgraded and uninstalled. The same lifecycle with a
//! stand-in component, and every failure boundary, are in `erp-modules/tests/wasm_lifecycle.rs`.
//!
//! The test is `#[ignore]`d in the workspace run and fails, never skips, when it runs without the
//! artefact: `scripts/ci.sh` step `wasm-dual-describe` builds the component (fat LTO) and sets
//! `ERP_WASM_LIFE_COMPONENT`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use std::fs;
use std::sync::Arc;

use erp_modules::discovery::AddonIndex;
use erp_modules::manifest::ModuleName;
use erp_modules::package::{AdminConsent, ErpAddonPackage, ErpAddonPackageWriter};
use erp_modules::system::{InitOptions, ModuleSystem};
use erp_modules::wasm_lifecycle::{HostLoader, WasmLifecycle, WasmPackages};
use erp_orm::{
    BoxFuture, Changes, Context, Generations, Orm, Refresh, Refreshed, SUPERUSER_ID, Signal,
    call_method_json,
};
use erp_sdk::Value;
use erp_testkit::TestDb;
use erp_wasm_host::{Limits, WasmHost};

const MODEL: &str = "test.wasm.life";
/// `erp_addon_test_wasm_life::MARK`. The fixture crate is deliberately not linked into this test:
/// a linked copy would also register `test.wasm.life` natively, and the point is that the model
/// arrives from the package alone.
const MARK: &str = "life";

struct KeepServing;

impl Refresh for KeepServing {
    fn refresh<'a>(&'a self, _: &'a Changes) -> BoxFuture<'a, erp_fields::Result<Refreshed>> {
        Box::pin(async { Ok(Refreshed::InPlace) })
    }
}

fn component_bytes() -> Vec<u8> {
    let path = std::env::var("ERP_WASM_LIFE_COMPONENT").unwrap_or_else(|_| {
        panic!("ERP_WASM_LIFE_COMPONENT must name the test_wasm_life component")
    });
    fs::read(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

fn package(version: &str) -> erp_modules::package::ConsentedPackage {
    let mut writer = ErpAddonPackageWriter::new();
    writer.add_file(
        "manifest.toml",
        format!(
            "name = \"test_wasm_life\"\nlicense = \"MIT OR Apache-2.0\"\nversion = \"{version}\"\n\
             [wasm]\nsdk_version = \"1.0\"\ncapabilities = []\n"
        ),
    );
    writer.add_file("wasm/test_wasm_life.wasm", component_bytes());
    let package = ErpAddonPackage::from_bytes(&writer.build_zip().unwrap()).unwrap();
    let consent = AdminConsent::new(package.name().clone());
    package.consent(&consent).unwrap()
}

async fn shout(orm: &Orm, text: &str) -> erp_fields::Result<String> {
    // a guest call runs for a caller with a company
    let ctx = Context::from_pairs([("allowed_company_ids", Value::list([Value::Int(1)]))]);
    let mut tx = orm.begin_with(SUPERUSER_ID, ctx).await?;
    let records = tx.env().records(MODEL)?.browse(Vec::<i64>::new())?;
    let args = serde_json::to_string(&(text,)).unwrap();
    let result = call_method_json(&mut tx, &records, "shout", &args).await;
    tx.rollback().await?;
    result
}

async fn table_exists(orm: &Orm) -> bool {
    let mut tx = orm.begin(SUPERUSER_ID).await.unwrap();
    let rows = tx
        .query("SELECT (to_regclass('test_wasm_life') IS NOT NULL)", &[])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    rows[0].get(0)
}

/// uninstalls without a restart; its model's table appears and disappears with it)
#[tokio::test]
#[ignore = "needs the wasm32-wasip2 build of test_wasm_life; run by scripts/ci.sh"]
async fn a_real_component_installs_upgrades_and_uninstalls_at_runtime() {
    let db = TestDb::create().await.unwrap();
    let dir = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let base = dir.path().join("base");
    fs::create_dir_all(&base).unwrap();
    fs::write(
        base.join("manifest.toml"),
        "name = 'Base'\nauto_install = true\ncategory = 'Hidden'\n",
    )
    .unwrap();
    let mut index = AddonIndex::new(true);
    index.add_dev_path(dir.path());

    let host = Arc::new(WasmHost::new().unwrap());
    let packages = WasmPackages::new(Arc::new(HostLoader::new(host, Limits::default())));
    let system = Arc::new(
        ModuleSystem::builder(db.pool().clone(), index)
            .linked_hooks(false)
            .linked_migrations(false)
            .signal(Signal::new(Generations::new()).with_refresh(KeepServing))
            .package_runtime(packages.clone())
            .build()
            .unwrap(),
    );
    let life = WasmLifecycle::new(Arc::clone(&system), packages);
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let env = |orm: &Orm| orm.env(SUPERUSER_ID, Context::new());

    let installed = life
        .install(&env(&init.orm), package("1.0.0"))
        .await
        .unwrap();
    assert!(installed.post_commit.failed.is_empty(), "{installed:?}");
    assert!(table_exists(&installed.orm).await);
    let expected = serde_json::to_string(&format!("{}{MARK}", "hi".to_uppercase())).unwrap();
    assert_eq!(shout(&installed.orm, "hi").await.unwrap(), expected);
    assert!(
        shout(&init.orm, "hi").await.is_err(),
        "the registry before the install is untouched"
    );

    let upgraded = life
        .upgrade(&env(&installed.orm), package("1.1.0"))
        .await
        .unwrap();
    assert_eq!(shout(&upgraded.orm, "up").await.unwrap(), {
        serde_json::to_string(&format!("{}{MARK}", "up".to_uppercase())).unwrap()
    });

    let removed = life
        .uninstall(
            &env(&upgraded.orm),
            &ModuleName::new("test_wasm_life").unwrap(),
        )
        .await
        .unwrap();
    assert!(!table_exists(&removed.orm).await, "its data goes with it");
    assert!(shout(&removed.orm, "x").await.is_err());
    db.cleanup().await.unwrap();
}
