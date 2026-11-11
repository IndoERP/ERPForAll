//! Transactional lifecycle admission after package-runtime admission.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use common::*;
use erp_fields::ErrorKind;
use erp_modules::system::InitOptions;
use erp_modules::{LifecycleChange, LifecycleHook, ModuleName, PackageChange, PackageRuntime};
use erp_orm::{BoxFuture, Orm, Transaction, WasmMethodImpl};
use erp_registry::ModelDef;
use erp_testkit::TestDb;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seen {
    name: &'static str,
    active: Vec<String>,
    installed: Vec<String>,
    upgraded: Vec<String>,
    uninstalled: Vec<String>,
}

struct Admission {
    name: &'static str,
    fail: AtomicBool,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Admission {
    fn new(name: &'static str, seen: &Arc<Mutex<Vec<Seen>>>) -> Arc<Self> {
        Arc::new(Self {
            name,
            fail: AtomicBool::new(false),
            seen: Arc::clone(seen),
        })
    }

    async fn record(
        &self,
        tx: &mut Transaction,
        change: PackageChange<'_>,
        active: Vec<String>,
    ) -> erp_modules::Result<()> {
        self.seen.lock().unwrap().push(Seen {
            name: self.name,
            active,
            installed: change.installed.iter().map(ToString::to_string).collect(),
            upgraded: change.upgraded.iter().map(ToString::to_string).collect(),
            uninstalled: change.uninstalled.iter().map(ToString::to_string).collect(),
        });
        tx.execute(
            "INSERT INTO ui3_admission_marker (stage) VALUES ($1)",
            &[&self.name],
        )
        .await?;
        if self.fail.load(Ordering::SeqCst) {
            return Err(
                erp_fields::Error::validation(format!("{} admission refused", self.name)).into(),
            );
        }
        Ok(())
    }
}

impl LifecycleHook for Admission {
    fn before_commit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        context: LifecycleChange<'a>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move {
            let active = context
                .active_addons()
                .iter()
                .map(|entry| {
                    assert!(entry.source_location().ends_with(entry.name().as_str()));
                    entry.name().to_string()
                })
                .collect();
            self.record(tx, context.change(), active).await
        })
    }
}

impl PackageRuntime for Admission {
    fn model_defs(&self, _module: &ModuleName) -> erp_modules::Result<Vec<ModelDef>> {
        Ok(Vec::new())
    }
    fn wasm_methods(
        &self,
        _plain: &Orm,
        _modules: &[ModuleName],
    ) -> erp_modules::Result<Vec<WasmMethodImpl>> {
        Ok(Vec::new())
    }
    fn before_commit<'a>(
        &'a self,
        tx: &'a mut Transaction,
        change: PackageChange<'a>,
    ) -> BoxFuture<'a, erp_modules::Result<()>> {
        Box::pin(async move { self.record(tx, change, Vec::new()).await })
    }
}

async fn setup() -> (TestDb, Addons, Orm) {
    let db = TestDb::create().await.unwrap();
    let addons = Addons::new();
    addons.add("a", "name = 'A'\ndepends = ['base']\n");
    addons.add("z", "name = 'Z'\ndepends = ['a']\n");
    let initial = builder(&db, &addons)
        .build()
        .unwrap()
        .initialize(InitOptions::new())
        .await
        .unwrap();
    let mut tx = initial.orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.batch("CREATE TABLE ui3_admission_marker (stage TEXT NOT NULL)")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (db, addons, initial.orm)
}

#[tokio::test]
async fn hooks_follow_package_admission_and_see_final_active_addons() {
    let (db, addons, _) = setup().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let package = Admission::new("package", &seen);
    let first = Admission::new("first", &seen);
    let second = Admission::new("second", &seen);
    let system = builder(&db, &addons)
        .package_runtime(package)
        .lifecycle_hook(first)
        .lifecycle_hook(second)
        .build()
        .unwrap();
    let loaded = system.load().await.unwrap();
    assert!(
        seen.lock().unwrap().is_empty(),
        "plain startup load has separate admission"
    );
    let installed = system
        .button_immediate_install(&su(&loaded.orm), &[n("z")])
        .await
        .unwrap();
    let calls = std::mem::take(&mut *seen.lock().unwrap());
    assert_eq!(
        calls.iter().map(|c| c.name).collect::<Vec<_>>(),
        ["package", "first", "second"]
    );
    for call in &calls[1..] {
        assert_eq!(call.active, ["base", "a", "z"]);
        assert_eq!(call.installed, ["a", "z"]);
        assert!(call.upgraded.is_empty());
        assert!(call.uninstalled.is_empty());
    }
    let upgraded = system
        .button_immediate_upgrade(&su(&installed.orm), &[n("a")])
        .await
        .unwrap();
    let calls = std::mem::take(&mut *seen.lock().unwrap());
    for call in &calls[1..] {
        assert_eq!(call.active, ["base", "a", "z"]);
        assert!(call.installed.is_empty());
        assert_eq!(call.upgraded, ["a", "z"]);
    }
    let removed = system
        .button_immediate_uninstall(&su(&upgraded.orm), &[n("a")])
        .await
        .unwrap();
    let calls = std::mem::take(&mut *seen.lock().unwrap());
    for call in &calls[1..] {
        assert_eq!(call.active, ["base"]);
        assert_eq!(call.uninstalled, ["a", "z"]);
    }
    assert_eq!(state(&removed.orm, "a").await, "uninstalled");
    assert_eq!(state(&removed.orm, "z").await, "uninstalled");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn hook_runs_without_a_package_runtime_and_refusal_rolls_back_every_stage() {
    let (db, addons, _) = setup().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook = Admission::new("asset", &seen);
    hook.fail.store(true, Ordering::SeqCst);
    let system = builder(&db, &addons)
        .lifecycle_hook(hook.clone())
        .build()
        .unwrap();
    let loaded = system.load().await.unwrap();
    let error = system
        .button_immediate_install(&su(&loaded.orm), &[n("a")])
        .await
        .unwrap_err();
    assert!(
        matches!(&error, erp_modules::Error::Orm(e) if e.kind() == ErrorKind::Validation && e.message() == "asset admission refused"),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "asset admission refused");
    assert_eq!(state(&loaded.orm, "a").await, "uninstalled");
    assert!(
        rows(&loaded.orm, "SELECT stage FROM ui3_admission_marker")
            .await
            .is_empty()
    );
    assert_eq!(seen.lock().unwrap()[0].active, ["base", "a"]);
    hook.fail.store(false, Ordering::SeqCst);
    let installed = system
        .button_immediate_install(&su(&loaded.orm), &[n("a")])
        .await
        .unwrap();
    assert_eq!(state(&installed.orm, "a").await, "installed");
    assert_eq!(
        rows(&installed.orm, "SELECT stage FROM ui3_admission_marker").await,
        vec![vec![Some("asset".to_owned())]]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn package_refusal_prevents_later_hooks_and_rolls_back() {
    let (db, addons, _) = setup().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let package = Admission::new("package", &seen);
    package.fail.store(true, Ordering::SeqCst);
    let hook = Admission::new("asset", &seen);
    let system = builder(&db, &addons)
        .package_runtime(package)
        .lifecycle_hook(hook)
        .build()
        .unwrap();
    let loaded = system.load().await.unwrap();
    let error = system
        .button_immediate_install(&su(&loaded.orm), &[n("a")])
        .await
        .unwrap_err();
    assert!(
        matches!(&error, erp_modules::Error::Orm(e) if e.kind() == ErrorKind::Validation && e.message() == "package admission refused"),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "package admission refused");
    assert_eq!(state(&loaded.orm, "a").await, "uninstalled");
    assert!(
        rows(&loaded.orm, "SELECT stage FROM ui3_admission_marker")
            .await
            .is_empty()
    );
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .map(|c| c.name)
            .collect::<Vec<_>>(),
        ["package"]
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn later_hook_refusal_rolls_back_package_and_prior_hook_writes() {
    let (db, addons, _) = setup().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let package = Admission::new("package", &seen);
    let first = Admission::new("first", &seen);
    let last = Admission::new("last", &seen);
    last.fail.store(true, Ordering::SeqCst);
    let system = builder(&db, &addons)
        .package_runtime(package)
        .lifecycle_hook(first)
        .lifecycle_hook(last)
        .build()
        .unwrap();
    let loaded = system.load().await.unwrap();
    let error = system
        .button_immediate_install(&su(&loaded.orm), &[n("a")])
        .await
        .unwrap_err();
    assert!(
        matches!(&error, erp_modules::Error::Orm(e) if e.kind() == ErrorKind::Validation && e.message() == "last admission refused"),
        "{error:?}"
    );
    assert_eq!(error.to_string(), "last admission refused");
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .map(|c| c.name)
            .collect::<Vec<_>>(),
        ["package", "first", "last"]
    );
    assert_eq!(state(&loaded.orm, "a").await, "uninstalled");
    assert!(
        rows(&loaded.orm, "SELECT stage FROM ui3_admission_marker")
            .await
            .is_empty()
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn initialize_admits_the_complete_auto_installed_set() {
    let db = TestDb::create().await.unwrap();
    // The hook's isolated marker table exists before module initialization.
    db.pool()
        .get()
        .await
        .unwrap()
        .batch_execute("CREATE TABLE ui3_admission_marker (stage TEXT NOT NULL)")
        .await
        .unwrap();
    let addons = Addons::new();
    addons.add(
        "autoapp",
        "name = 'Automatic app'\ndepends = ['base']\nauto_install = true\n",
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let package = Admission::new("package", &seen);
    let hook = Admission::new("asset", &seen);
    let system = builder(&db, &addons)
        .package_runtime(package)
        .lifecycle_hook(hook)
        .build()
        .unwrap();
    let initialized = system.initialize(InitOptions::new()).await.unwrap();
    {
        let calls = seen.lock().unwrap();
        assert_eq!(
            calls.iter().map(|c| c.name).collect::<Vec<_>>(),
            ["package", "asset"]
        );
        assert_eq!(calls[1].active, ["base", "autoapp"]);
        assert_eq!(calls[1].installed, ["base", "autoapp"]);
    }
    assert_eq!(state(&initialized.orm, "autoapp").await, "installed");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn operation_and_plain_load_retain_ordered_admitted_source_files() {
    let (db, addons, _) = setup().await;
    let system = builder(&db, &addons).build().unwrap();
    let first = system.load().await.unwrap();
    assert_eq!(
        first
            .admitted_addons()
            .iter()
            .map(|entry| entry.name().as_str())
            .collect::<Vec<_>>(),
        ["base"]
    );
    let installed = system
        .button_immediate_install(&su(&first.orm), &[n("z")])
        .await
        .unwrap();
    assert_eq!(
        installed
            .admitted_addons()
            .iter()
            .map(|entry| entry.name().as_str())
            .collect::<Vec<_>>(),
        ["base", "a", "z"]
    );
    let loaded = system.load().await.unwrap();
    assert_eq!(
        loaded
            .admitted_addons()
            .iter()
            .map(|entry| entry.name().as_str())
            .collect::<Vec<_>>(),
        ["base", "a", "z"]
    );
    for entry in loaded.admitted_addons() {
        assert_eq!(
            entry
                .admitted_files()
                .unwrap()
                .get("manifest.toml")
                .unwrap(),
            &std::fs::read(
                addons
                    .path()
                    .join(entry.name().as_str())
                    .join("manifest.toml")
            )
            .unwrap()
        );
    }
    let a = loaded
        .admitted_addons()
        .iter()
        .find(|entry| entry.name().as_str() == "a")
        .unwrap();
    let admitted_manifest = a
        .admitted_files()
        .unwrap()
        .get("manifest.toml")
        .unwrap()
        .clone();
    std::fs::write(
        addons.path().join("a/manifest.toml"),
        "name='Edited after successful load'\ndepends=['base']\n",
    )
    .unwrap();
    assert_eq!(
        a.admitted_files().unwrap().get("manifest.toml").unwrap(),
        &admitted_manifest
    );
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn lifecycle_hook_reads_precaptured_bytes_after_live_source_mutation() {
    struct MutateSource {
        root: std::path::PathBuf,
        seen: AtomicBool,
    }
    impl LifecycleHook for MutateSource {
        fn before_commit<'a>(
            &'a self,
            _tx: &'a mut Transaction,
            context: LifecycleChange<'a>,
        ) -> BoxFuture<'a, erp_modules::Result<()>> {
            Box::pin(async move {
                let base = context
                    .active_addons()
                    .iter()
                    .find(|entry| entry.name().as_str() == "base")
                    .unwrap();
                let before = base
                    .admitted_files()
                    .unwrap()
                    .get("manifest.toml")
                    .unwrap()
                    .clone();
                std::fs::write(
                    self.root.join("base/manifest.toml"),
                    "name='Edited during callback'\nauto_install=true\n",
                )
                .unwrap();
                assert_ne!(
                    std::fs::read(self.root.join("base/manifest.toml")).unwrap(),
                    before
                );
                assert_eq!(
                    base.admitted_files().unwrap().get("manifest.toml").unwrap(),
                    &before
                );
                self.seen.store(true, Ordering::SeqCst);
                // Refuse after observing the immutable input; the fixture never publishes a
                // changed live artifact or rewrites its existing identity receipt.
                Err(erp_fields::Error::validation("snapshot observation fixture refused").into())
            })
        }
    }
    let (db, addons, _) = setup().await;
    let hook = Arc::new(MutateSource {
        root: addons.path().to_owned(),
        seen: AtomicBool::new(false),
    });
    let system = builder(&db, &addons)
        .lifecycle_hook(hook.clone())
        .build()
        .unwrap();
    let loaded = system.load().await.unwrap();
    let error = system
        .button_immediate_install(&su(&loaded.orm), &[n("a")])
        .await
        .unwrap_err();
    assert!(
        matches!(&error, erp_modules::Error::Orm(source) if source.kind() == ErrorKind::Validation && source.message() == "snapshot observation fixture refused"),
        "{error:?}"
    );
    assert!(hook.seen.load(Ordering::SeqCst));
    assert_eq!(state(&loaded.orm, "a").await, "uninstalled");
    db.cleanup().await.unwrap();
}

#[tokio::test]
async fn forced_update_load_keeps_the_committed_operation_admitted_entries() {
    let (db, addons, orm) = setup().await;
    let system = builder(&db, &addons).build().unwrap();
    let mut tx = orm.begin(erp_orm::SUPERUSER_ID).await.unwrap();
    tx.execute(
        "UPDATE ir_module_module SET state='to install' WHERE name='a'",
        &[],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let loaded = system.load().await.unwrap();
    assert!(loaded.forced_update.is_some());
    assert_eq!(
        loaded
            .admitted_addons()
            .iter()
            .map(|entry| entry.name().as_str())
            .collect::<Vec<_>>(),
        ["base", "a"]
    );
    assert!(
        loaded
            .admitted_addons()
            .iter()
            .all(|entry| entry.admitted_files().is_some())
    );
    assert_eq!(state(&loaded.orm, "a").await, "installed");
    db.cleanup().await.unwrap();
}
