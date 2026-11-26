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
#[path = "support/actions_open.rs"]
mod actions_open;
#[path = "support/install_defaults.rs"]
mod install_defaults;
#[path = "support/native_http.rs"]
mod native_http;

use std::collections::BTreeSet;
use std::fs;

use actions_open::assert_actions_open;
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

/// A healthy listener belonging to a different fixture must never receive the admin login.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_fixture_listener_is_refused_before_login() {
    let neighbor = NativeFixture::start().await;
    let accepted = fixture_login_page(neighbor.port, "different_fixture_database").is_ok();
    let still_ready = request(neighbor.port, "GET", "/erp/health/ready", &[], "").status;
    neighbor.cleanup().await;
    assert!(!accepted, "foreign fixture login page was accepted");
    assert_eq!(still_ready, 200);
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
    assert_eq!(
        state_of(&start, "sale_stock").as_deref(),
        Some("uninstalled"),
        "the Sales delivery bridge is offered for explicit installation"
    );
    assert_eq!(
        state_of(&start, "stock_sms").as_deref(),
        Some("uninstalled"),
        "stock_sms is now offered for explicit installation"
    );
    assert!(!log.contains("no Rust sources"), "{log}");
    // A folder with a crate this binary does not link says so (the fixture's `unlinked_native`).
    assert!(
        log.contains(
            "addon catalogue: unlinked_native is not offered for installation: \
             the folder holds a Rust crate (Cargo.toml or src/) of a native addon that this binary \
             does not link"
        ),
        "{log}"
    );

    let account = install(&fixture, "account");
    let stock = install(&fixture, "stock");
    eprintln!("installing account pulled in {account:?}; stock pulled in {stock:?}");
    assert_eq!(account, ["account", "analytic", "product", "uom"]);
    assert_eq!(
        stock,
        [
            "barcodes",
            "barcodes_gs1_nomenclature",
            "reporting",
            "stock"
        ]
    );
    assert_eq!(install(&fixture, "stock_account"), ["stock_account"]);
    check_defaults(&fixture, install_defaults::ACCOUNT).await;
    check_defaults(&fixture, install_defaults::STOCK).await;
    // purchase on top of account: its order screens, line sub-views and settings page open too
    assert_eq!(install(&fixture, "purchase"), ["purchase"]);
    assert_actions_open(&fixture, &["account", "stock", "purchase"]).await;
    // mail on top brings the bridges that give entries, orders and transfers their chatter; every
    // screen still opens with the chatter in its form
    assert_eq!(
        install(&fixture, "mail"),
        [
            "account_mail",
            "mail",
            "product_mail",
            "purchase_mail",
            "stock_mail"
        ]
    );
    assert_actions_open(&fixture, &["account", "stock", "purchase"]).await;

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
    assert_eq!(
        state_of(&after, "sale_stock").as_deref(),
        Some("uninstalled"),
        "installing Accounting and Inventory and upgrading Base leaves Sales delivery uninstalled"
    );
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

/// Data an administrator creates through the JSON-2 API on a served node whose request role may
/// not create objects in the schema, as on a deployment: an operation type made with a prefix gets
/// gapless numbering and its transfer a number, while the types made by the install keep their
/// standard sequences; no user creates a sequence through the API (access, first); an account is
/// found by its code; a condition on a field that cannot be searched names it in plain words.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn served_demo_data_under_a_request_role_without_schema_rights() {
    let fixture = NativeFixture::start().await;
    let (port, cookies) = (fixture.port, fixture.cookies.clone());
    let can_create: bool = fixture
        .client
        .query_one(
            "SELECT has_schema_privilege($1, current_schema(), 'CREATE')",
            &[&fixture.role],
        )
        .await
        .unwrap()
        .get(0);
    assert!(
        !can_create,
        "the request role must not create objects, or this test proves nothing"
    );
    install(&fixture, "account");
    install(&fixture, "stock");
    let post =
        |path: &str, body: serde_json::Value| json_post(port, &cookies, path, &body.to_string());
    let ok = |reply: Reply, what: &str| -> serde_json::Value {
        assert_eq!(reply.status, 200, "{what}: {}", reply.body);
        serde_json::from_str(&reply.body).unwrap()
    };

    // an operation type with a prefix, and a transfer of it
    let warehouse: i64 = fixture
        .client
        .query_one("SELECT min(id)::int8 FROM stock_warehouse", &[])
        .await
        .unwrap()
        .get(0);
    let created = ok(
        post(
            "/erp/json/2/stock.picking.type/create",
            serde_json::json!({"vals_list":[{"name":"Demo receipts","code":"incoming",
                "sequence_code":"DEMO/IN/","warehouse_id":warehouse}]}),
        ),
        "operation type create",
    );
    let kind = created[0].as_i64().unwrap();
    let numbering: (String, String) = {
        let row = fixture
            .client
            .query_one(
                "SELECT s.implementation, s.wait_mode FROM stock_picking_type t \
                 JOIN ir_sequence s ON s.id = t.sequence_id WHERE t.id = $1",
                &[&kind],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1))
    };
    assert_eq!(numbering, ("no_gap".to_owned(), "lock_timeout".to_owned()));
    let installed: Vec<String> = fixture
        .client
        .query(
            "SELECT DISTINCT s.implementation FROM stock_picking_type t \
             JOIN ir_sequence s ON s.id = t.sequence_id WHERE t.id <> $1",
            &[&kind],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(installed, ["standard"], "the install's numbering is kept");
    let picking = ok(
        post(
            "/erp/json/2/stock.picking/create",
            serde_json::json!({"vals_list":[{"picking_type_id":kind}]}),
        ),
        "transfer create",
    )[0]
    .as_i64()
    .unwrap();
    let name: String = fixture
        .client
        .query_one("SELECT name FROM stock_picking WHERE id = $1", &[&picking])
        .await
        .unwrap()
        .get(0);
    assert_eq!(name, "DEMO/IN/00001");

    // No user creates a sequence through the API, the administrator included (an `erp db init`
    // administrator is a Settings user, not the superuser): users only read sequences, so the
    // refusal is access, before the database. The named refusal of a standard sequence under such
    // a role, met by server code acting as the superuser, is in `base_sequences.rs`.
    let count_sql = "SELECT count(*) FROM ir_sequence";
    let before: i64 = fixture
        .client
        .query_one(count_sql, &[])
        .await
        .unwrap()
        .get(0);
    for implementation in ["standard", "no_gap"] {
        let refused = post(
            "/erp/json/2/ir.sequence/create",
            serde_json::json!({"vals_list":[{"name":"By hand","implementation":implementation}]}),
        );
        assert_eq!(refused.status, 403, "{implementation}: {}", refused.body);
        assert!(
            refused.body.contains("erp.exceptions.AccessError")
                && refused
                    .body
                    .contains("do not let you create 'Sequence' (ir.sequence) records"),
            "{implementation}: {}",
            refused.body
        );
    }
    let after: i64 = fixture
        .client
        .query_one(count_sql, &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(after, before, "no sequence was made");

    // an account by its code: a condition and a many2one search
    let account = ok(
        post(
            "/erp/json/2/account.account/create",
            serde_json::json!({"vals_list":[{"name":"Demo bank","code":"111201",
                "account_type":"asset_current"}]}),
        ),
        "account create",
    )[0]
    .as_i64()
    .unwrap();
    for domain in [
        serde_json::json!([["code", "in", ["111201"]]]),
        serde_json::json!([["code", "=", "111201"]]),
        serde_json::json!([["code", "=like", "1112%"]]),
    ] {
        let found = ok(
            post(
                "/erp/json/2/account.account/search_read",
                serde_json::json!({"domain":domain,"fields":["code"]}),
            ),
            "account search",
        );
        let ids: Vec<i64> = found
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect();
        assert!(ids.contains(&account), "{domain}: {found}");
        assert!(
            found
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["code"].as_str().unwrap().starts_with("1112")),
            "{domain}: {found}"
        );
    }
    let named = ok(
        post(
            "/erp/json/2/account.account/name_search",
            serde_json::json!({"name":"111201"}),
        ),
        "account name search",
    );
    let pairs = named.as_array().unwrap();
    assert!(
        pairs.iter().any(|pair| pair[0].as_i64() == Some(account)),
        "{named}"
    );

    // a field that cannot be searched is named, without the condition's internal form
    let refused = post(
        "/erp/json/2/res.company/search_read",
        serde_json::json!({"domain":[["root_id","in",[1]]],"fields":["name"]}),
    );
    assert_eq!(refused.status, 422, "{}", refused.body);
    assert!(
        refused
            .body
            .contains("the field root_id of res.company: this field cannot be searched"),
        "{}",
        refused.body
    );
    assert!(!refused.body.contains("OrderedSet"), "{}", refused.body);
    fixture.cleanup().await;
}
