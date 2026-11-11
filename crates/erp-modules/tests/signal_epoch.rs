//! Module operations and the registry generation (
//! "Registry (other changes)"): an operation that installs, upgrades or uninstalls a module bumps
//! the `registry` generation in its own transaction, so the other workers rebuild; a failed
//! operation rolls the bump back with the rest.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use erp_modules::hooks::{HookImpl, HookKind};
use erp_modules::system::InitOptions;
use erp_orm::{BoxFuture, Changes, Generations, Refresh, Refreshed, Signal, Transaction};
use erp_testkit::TestDb;

/// A refresh hook that counts its calls and updates in place.
struct Counting(Arc<AtomicUsize>);

impl Refresh for Counting {
    fn refresh<'a>(
        &'a self,
        _changes: &'a Changes,
    ) -> BoxFuture<'a, erp_fields::Result<Refreshed>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Refreshed::InPlace)
        })
    }
}

/// The `registry` generation as a second connection sees it.
async fn registry_generation(db: &TestDb) -> i64 {
    let client = db.client().await.unwrap();
    client
        .query_one(
            "SELECT coalesce(sum(weight), 0)::int8 FROM erp_signal_generation \
             WHERE key = 'registry'",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

fn fail_post_init(_tx: &mut Transaction) -> BoxFuture<'_, erp_fields::Result<()>> {
    Box::pin(async move { Err(erp_fields::Error::user("post_init of f failed")) })
}

static FAIL_POST_INIT: HookImpl =
    HookImpl::new("f", HookKind::PostInit, "fail", fail_post_init).at(file!(), line!());

/// initialize, install, upgrade and uninstall each bump the registry generation by exactly one
/// (one row per operation, the schema step's own bump included), and the process that ran the
/// operation refreshes on its next transaction (`SelfApplied::No`: it still serves the old
/// registry until the caller swaps in the outcome's ORM).
#[tokio::test]
async fn install_upgrade_uninstall_bump_the_registry_generation() {
    let addons = Addons::new();
    addons.add("b", "name = 'B'\n");
    let db = TestDb::create().await.unwrap();
    let refreshes = Arc::new(AtomicUsize::new(0));
    let signal = Signal::new(Generations::new()).with_refresh(Counting(Arc::clone(&refreshes)));
    let system = builder(&db, &addons)
        .decls([model("b", "test.b")])
        .signal(signal)
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let after_init = registry_generation(&db).await;
    assert_eq!(after_init, 1, "initialize is one registry change");
    let env = su(&init.orm);

    committed(
        system
            .button_immediate_install(&env, &[n("b")])
            .await
            .unwrap(),
    );
    let after_install = registry_generation(&db).await;
    assert_eq!(after_install, after_init + 1);
    // the next transaction of this process sees the moved generation and refreshes
    let _ = states(&init.orm).await;
    assert!(refreshes.load(Ordering::SeqCst) >= 1);

    committed(
        system
            .button_immediate_upgrade(&env, &[n("b")])
            .await
            .unwrap(),
    );
    let after_upgrade = registry_generation(&db).await;
    assert_eq!(after_upgrade, after_install + 1);

    committed(
        system
            .button_immediate_uninstall(&env, &[n("b")])
            .await
            .unwrap(),
    );
    assert_eq!(registry_generation(&db).await, after_upgrade + 1);
    db.cleanup().await.unwrap();
}

/// fails rolls back with its bump, so the other workers do not rebuild for a change that never
/// happened; a refused request (nothing runs) bumps nothing either.
#[tokio::test]
async fn failed_and_refused_operations_do_not_bump_the_registry_generation() {
    let addons = Addons::new();
    addons.add("f", "name = 'F'\npost_init_hook = 'fail'\n");
    let db = TestDb::create().await.unwrap();
    let signal =
        Signal::new(Generations::new()).with_refresh(Counting(Arc::new(AtomicUsize::new(0))));
    let system = builder(&db, &addons)
        .decls([model("f", "test.f")])
        .hooks([&FAIL_POST_INIT])
        .signal(signal)
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let before = registry_generation(&db).await;
    let env = su(&init.orm);

    // the run installs f, bumps, then its post_init hook fails: all of it rolls back
    system
        .button_immediate_install(&env, &[n("f")])
        .await
        .unwrap_err();
    assert_eq!(registry_generation(&db).await, before);

    // refused before anything runs (unknown module)
    system
        .button_immediate_install(&env, &[n("nope")])
        .await
        .unwrap_err();
    assert_eq!(registry_generation(&db).await, before);
    db.cleanup().await.unwrap();
}
