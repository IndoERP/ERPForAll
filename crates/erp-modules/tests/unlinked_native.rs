// SPDX-License-Identifier: MIT OR Apache-2.0
//! A folder of the addon directory that holds a Rust crate the binary does not link is a native
//! addon without its code: every install route refuses it by name, with one deterministic reason.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]
mod common;
use common::*;
use erp_modules::InitOptions;
use erp_modules::error::Error;
use erp_testkit::TestDb;

/// `native_x` holds a crate; `uses_x` is data-only and depends on it.
fn fixture() -> Addons {
    let addons = Addons::new();
    addons
        .add("native_x", "name='Native X'\ndepends=['base']\n")
        .file("native_x", "Cargo.toml", "[package]\nname='native_x'\n")
        .file("native_x", "src/lib.rs", "")
        .add("uses_x", "name='Uses X'\ndepends=['native_x']\n");
    addons
}

/// The operator route (`erp module install`, `button_immediate_install`) refuses the unlinked
/// native addon, asked by name or pulled in as a dependency, and installs nothing.
#[tokio::test]
async fn operator_install_refuses_an_unlinked_native_addon() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    for requested in ["native_x", "uses_x"] {
        let error = system
            .button_immediate_install(&su(&init.orm), &[n(requested)])
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::AddonCodeMismatch { module, message }
                if module.as_str() == "native_x" && message.contains("this binary does not link")),
            "{requested}: {error}"
        );
    }
    let installed = rows(
        &init.orm,
        "SELECT name FROM ir_module_module WHERE name IN ('native_x', 'uses_x') \
         AND state <> 'uninstalled'",
    )
    .await;
    assert!(installed.is_empty(), "{installed:?}");
    db.cleanup().await.unwrap();
}

const GENERIC: &str = "the folder holds a Rust crate (Cargo.toml or src/) of a native addon that this binary does not link";

/// The reason registered with `not_linked` reaches the catalogue and the by-name refusal; a crate
/// folder without one gets one fixed sentence whatever the directory listing order; a registered
/// name whose folder holds no crate keeps its own data-only refusal.
#[tokio::test]
async fn refusals_carry_the_registered_reason_only_for_crate_folders() {
    let db = TestDb::create().await.unwrap();
    let addons = fixture();
    addons
        .add("other_native", "name='Other'\ndepends=['base']\n")
        .file("other_native", "src/lib.rs", "")
        .file(
            "other_native",
            "Cargo.toml",
            "[package]\nname='other_native'\n",
        )
        .file("other_native", "build.rs", "")
        .add("noted", "name='Noted'\ndepends=['base']\n")
        .file("noted", "migrations/1.0.0/pre-x.sql", "SELECT 1;");
    let registered = "declared, no behaviour yet: fixture";
    let system = builder(&db, &addons)
        .not_linked([("native_x", registered), ("noted", registered)])
        .build()
        .unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let catalog = system.human_catalog().await.unwrap();
    let reason = |name: &str| {
        catalog
            .refused
            .iter()
            .find(|(refused, _)| refused.as_str() == name)
            .map(|(_, reason)| reason.clone())
            .unwrap_or_else(|| panic!("{name} is not refused"))
    };
    assert_eq!(reason("native_x"), registered);
    assert_eq!(reason("other_native"), GENERIC);
    let noted = reason("noted");
    assert!(noted.contains("migrations"), "{noted}");

    for (requested, expected) in [("native_x", registered), ("other_native", GENERIC)] {
        let error = system
            .button_immediate_install(&su(&init.orm), &[n(requested)])
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::AddonCodeMismatch { module, message }
                if module.as_str() == requested
                    && *message == format!("this binary does not link the addon's native code: {expected}")),
            "{requested}: {error}"
        );
    }
    db.cleanup().await.unwrap();
}
