// SPDX-License-Identifier: MIT OR Apache-2.0
//! Crash-safety regression, not a future-size performance ratchet.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]

mod common;

use common::{Addons, builder, model, n, rows, state, su};
use erp_modules::system::InitOptions;
use erp_testkit::TestDb;

#[test]
fn initialize_install_and_upgrade_on_two_mib_stack() {
    std::thread::Builder::new()
        .name("module-lifecycle-2mib".to_owned())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            // Construct and pin on this stack. Passing a small Pin<&mut _> to block_on
            // prevents its automatic boxing of a large root future hiding this regression.
            let scenario = std::pin::pin!(async {
                let addons = Addons::new();
                addons
                    .add("probe", "name='Probe'\nversion='1.2.0'\ndata=['probe.xml']\n")
                    .file("probe", "probe.xml", "<odoo><record id=\"seed\" model=\"stack.probe\"><field name=\"name\">Seed</field></record></odoo>");
                let db = TestDb::create().await.unwrap();
                let system = builder(&db, &addons)
                    .decls([model("probe", "stack.probe")])
                    .build()
                    .unwrap();
                let initialized = system
                    .initialize(InitOptions::new().with_skip_auto_install(true))
                    .await
                    .unwrap();
                assert!(initialized.post_commit.is_clean());
                assert_eq!(state(&initialized.orm, "probe").await, "uninstalled");
                let env = su(&initialized.orm);
                let names = [n("probe")];
                let installed = system.button_immediate_install(&env, &names).await.unwrap();
                assert_eq!(installed.installed, names);
                assert!(installed.post_commit.is_clean());
                assert_eq!(state(&installed.orm, "probe").await, "installed");
                assert_eq!(rows(&installed.orm, "SELECT name FROM stack_probe").await,
                    vec![vec![Some("Seed".to_owned())]]);
                let client = db.client().await.unwrap();
                // Current-schema upgrade fixture, not evidence of a previous-binary upgrade.
                client.batch_execute("UPDATE ir_module_module SET latest_version='1.1.0' WHERE name='probe'; UPDATE stack_probe SET name='Old value'").await.unwrap();
                drop(client);
                let upgraded = system.button_immediate_upgrade(&env, &names).await.unwrap();
                assert_eq!(upgraded.upgraded, names);
                assert!(upgraded.post_commit.is_clean());
                assert_eq!(rows(&upgraded.orm, "SELECT latest_version FROM ir_module_module WHERE name='probe'").await,
                    vec![vec![Some("1.2.0".to_owned())]]);
                assert_eq!(rows(&upgraded.orm, "SELECT name FROM stack_probe").await,
                    vec![vec![Some("Seed".to_owned())]]);
                db.cleanup().await.unwrap();
            });
            runtime.block_on(scenario);
        })
        .unwrap()
        .join()
        .unwrap();
}

/// Prints actual unboxed future layouts; no size threshold or performance claim.
#[test]
fn report_lifecycle_future_sizes() {
    // Only the measurement uses a larger stack, so baseline construction can be observed.
    std::thread::Builder::new()
        .name("module-future-diagnostic".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let db = runtime.block_on(TestDb::create()).unwrap();
            let addons = Addons::new();
            let system = builder(&db, &addons).build().unwrap();
            let env = system.system_env(erp_orm::Context::new());
            let names = [n("base")];
            macro_rules! measure {
                ($name:literal, $future:expr) => {{
                    let future = $future;
                    eprintln!(
                        "future_size name={} bytes={}",
                        $name,
                        std::mem::size_of_val(&future)
                    );
                    drop(future);
                }};
            }
            measure!("initialize", system.initialize(InitOptions::new()));
            measure!("install", system.button_immediate_install(&env, &names));
            measure!("upgrade", system.button_immediate_upgrade(&env, &names));
            measure!(
                "upgrade_restoring",
                system.button_immediate_upgrade_restoring(&env, &names)
            );
            measure!("load", system.load());
            runtime.block_on(db.cleanup()).unwrap();
        })
        .unwrap()
        .join()
        .unwrap();
}
