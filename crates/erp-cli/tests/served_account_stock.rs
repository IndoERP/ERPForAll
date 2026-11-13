// SPDX-License-Identifier: MIT OR Apache-2.0
//! `account` and `stock` on a served node: the real `erp` binary
//! offers them on the Addons page, installs each through the route the page calls
//! (`/erp/module/install`) with its dependencies and default data, publishes the runtime grants,
//! carries them through an upgrade of `base`, and a second node of the database sees them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "integration fixture assertions and shared helpers"
)]
#[path = "support/install_defaults.rs"]
mod install_defaults;
#[path = "support/native_http.rs"]
mod native_http;

use std::collections::BTreeSet;
use std::fs;

use native_http::*;

/// The linked addons this slice adds to the served binary.
const ADDED: [&str; 4] = ["analytic", "account", "barcodes_gs1_nomenclature", "stock"];

fn installed(catalogue: &serde_json::Value) -> BTreeSet<String> {
    catalogue["addons"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|addon| addon["state"] == "installed")
        .map(|addon| addon["name"].as_str().unwrap().to_owned())
        .collect()
}

/// A lifecycle call the Addons page makes; a whole application can take longer than the
/// fixture's ordinary 30 s reply wait on a loaded build host.
fn lifecycle(fixture: &NativeFixture, path: &str, body: &str) -> Reply {
    request_within(
        fixture.port,
        "POST",
        path,
        &[
            ("cookie", &fixture.cookies),
            ("content-type", "application/json"),
            ("sec-fetch-site", "same-origin"),
        ],
        body,
        std::time::Duration::from_secs(600),
    )
}

fn install(fixture: &NativeFixture, name: &str) -> Vec<String> {
    let before = installed(&catalogue(fixture.port, &fixture.cookies));
    let reply = lifecycle(
        fixture,
        "/erp/module/install",
        &format!(r#"{{"modules":["{name}"]}}"#),
    );
    assert_eq!(reply.status, 200, "install {name}: {}", reply.body);
    let after = installed(&catalogue(fixture.port, &fixture.cookies));
    assert!(before.is_subset(&after), "an install removed an addon");
    after.difference(&before).cloned().collect()
}

async fn check_defaults(fixture: &NativeFixture, checks: &[(&str, &[&str])]) {
    for (sql, expected) in checks {
        let rows: Vec<String> = fixture
            .client
            .query(*sql, &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(rows, *expected, "{sql}");
    }
}

fn count(port: u16, cookies: &str, model: &str) -> serde_json::Value {
    let reply = json_post(
        port,
        cookies,
        &format!("/erp/json/2/{model}/search_count"),
        r#"{"domain":[]}"#,
    );
    assert_eq!(reply.status, 200, "{model}: {}", reply.body);
    serde_json::from_str(&reply.body).unwrap()
}

/// Every crate under `addons/` is either linked into the `erp` binary or listed as not linked
/// with its reason, never both, and the linked descriptors are the linked names in order: a new
/// addon crate cannot be left out of the served binary silently.
#[test]
fn every_addon_crate_is_linked_or_excluded_with_a_reason() {
    use erp_cli::native_addons::{DESCRIPTORS, LINKED, NOT_LINKED};
    let names: Vec<&str> = DESCRIPTORS.iter().map(|addon| addon.name).collect();
    assert_eq!(names, LINKED);
    let addons = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../addons");
    let mut crates = BTreeSet::new();
    for entry in fs::read_dir(&addons).unwrap() {
        let path = entry.unwrap().path();
        if path.join("Cargo.toml").is_file() && path.join("manifest.toml").is_file() {
            crates.insert(path.file_name().unwrap().to_str().unwrap().to_owned());
        }
    }
    let linked: BTreeSet<String> = LINKED.iter().map(|n| (*n).to_owned()).collect();
    let excluded: BTreeSet<String> = NOT_LINKED.iter().map(|(n, _)| (*n).to_owned()).collect();
    assert_eq!(linked.len(), LINKED.len(), "a name is linked twice");
    assert_eq!(excluded.len(), NOT_LINKED.len(), "a name is excluded twice");
    assert!(
        linked.is_disjoint(&excluded),
        "linked and excluded: {:?}",
        linked.intersection(&excluded)
    );
    let listed: BTreeSet<String> = linked.union(&excluded).cloned().collect();
    assert_eq!(
        crates, listed,
        "addons/ crates versus the linked and excluded lists"
    );
    for (name, reason) in NOT_LINKED {
        assert!(!reason.trim().is_empty(), "{name} has no reason");
    }
}

/// The served node offers the linked accounting and inventory addons, refuses the declared-only
/// bridges with that reason, and installs `account` then `stock` with their dependencies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_node_offers_and_installs_account_and_stock() {
    let fixture = NativeFixture::start().await;
    let port = fixture.port;
    let cookies = fixture.cookies.clone();

    // A database made without these addons starts on this binary and installs none of them.
    let start = catalogue(port, &cookies);
    for name in ADDED {
        assert_eq!(
            state_of(&start, name).as_deref(),
            Some("uninstalled"),
            "{name} is offered"
        );
    }
    let log = fs::read_to_string(&fixture.stderr_path).unwrap();
    for name in ADDED {
        assert!(
            !log.contains(&format!("addon catalogue: {name} is not offered")),
            "{name} is refused: {log}"
        );
    }
    for name in ["stock_account", "sale_stock", "stock_sms"] {
        assert_eq!(state_of(&start, name), None, "{name} is not offered");
        assert!(
            log.contains(&format!(
                "addon catalogue: {name} is not offered for installation: declared, no behaviour yet"
            )),
            "{name}: {log}"
        );
    }
    assert!(!log.contains("no Rust sources"), "{log}");
    // A folder with a crate this binary does not link says so (the fixture's `unlinked_native`).
    assert!(
        log.contains(
            "addon catalogue: unlinked_native is not offered for installation: \
             src is not allowed (rule: a native addon's Rust crate: this binary does not link the addon)"
        ) || log.contains(
            "addon catalogue: unlinked_native is not offered for installation: \
             Cargo.toml is not allowed (rule: a native addon's Rust crate: this binary does not link the addon)"
        ),
        "{log}"
    );

    let account = install(&fixture, "account");
    let stock = install(&fixture, "stock");
    eprintln!("installing account pulled in {account:?}; stock pulled in {stock:?}");
    assert_eq!(account, ["account", "analytic", "product", "uom"]);
    assert_eq!(stock, ["barcodes", "barcodes_gs1_nomenclature", "stock"]);
    check_defaults(&fixture, install_defaults::ACCOUNT).await;
    check_defaults(&fixture, install_defaults::STOCK).await;

    // The request role reads the new tables: through the ORM as the administrator (an
    // accounting manager by `base.group_system`), and by the published table grants.
    assert_eq!(count(port, &cookies, "account.account.tag"), 3);
    for table in ["account_account_tag", "stock_location", "stock_route"] {
        let granted: bool = fixture
            .client
            .query_one(
                "SELECT has_table_privilege($1, $2, 'SELECT')",
                &[&fixture.role, &table],
            )
            .await
            .unwrap()
            .get(0);
        assert!(granted, "the request role reads {table}");
    }

    // An upgrade of base carries both applications.
    let upgraded = lifecycle(&fixture, "/erp/module/upgrade", r#"{"modules":["base"]}"#);
    assert_eq!(upgraded.status, 200, "upgrade base: {}", upgraded.body);
    let after = catalogue(port, &cookies);
    for name in ADDED {
        assert_eq!(
            state_of(&after, name).as_deref(),
            Some("installed"),
            "{name}"
        );
    }
    check_defaults(&fixture, install_defaults::ACCOUNT).await;
    check_defaults(&fixture, install_defaults::STOCK).await;

    // A second node of the database sees them.
    let peer = fixture.peer();
    let seen = catalogue(peer.port, &cookies);
    for name in ADDED {
        assert_eq!(
            state_of(&seen, name).as_deref(),
            Some("installed"),
            "peer: {name}"
        );
    }
    assert_eq!(count(peer.port, &cookies, "account.account.tag"), 3);
    drop(peer);
    fixture.cleanup().await;
}
