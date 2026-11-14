// SPDX-License-Identifier: MIT OR Apache-2.0
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod common;
use common::*;
use erp_modules::system::InitOptions;
use erp_testkit::TestDb;

fn model_file(kind: &str) -> String {
    format!(
        "[[model]]\nname = 'bigint.lifecycle'\n[[model.field]]\nname = 'counter'\ntype = '{kind}'\ndefault = 7\nindex = 'btree'\n"
    )
}
#[tokio::test]
async fn bigint_module_upgrade_preserves_data_and_rolls_back_a_later_failure() {
    let addons = Addons::new();
    addons
        .add(
            "bigint_fixture",
            "name='Bigint fixture'\nversion='1.0.0'\ndepends=['base']\n",
        )
        .file(
            "bigint_fixture",
            "models/counter.toml",
            &model_file("integer"),
        );
    let db = TestDb::create().await.unwrap();
    let system = builder(&db, &addons).build().unwrap();
    let init = system
        .initialize(InitOptions::new().with_skip_auto_install(true))
        .await
        .unwrap();
    let installed = system
        .button_immediate_install(&su(&init.orm), &[n("bigint_fixture")])
        .await
        .unwrap();
    exec(
        &installed.orm,
        "INSERT INTO bigint_lifecycle (counter) VALUES (NULL),(0),(-2147483648),(2147483647)",
    )
    .await;
    let before = rows(
        &installed.orm,
        "SELECT counter::text FROM bigint_lifecycle ORDER BY id",
    )
    .await;
    // The new release first widens schema, then fails loading an intentionally malformed data file.
    addons
        .add(
            "bigint_fixture",
            "name='Bigint fixture'\nversion='2.0.0'\ndepends=['base']\ndata=['broken.xml']\n",
        )
        .file(
            "bigint_fixture",
            "models/counter.toml",
            &model_file("bigint"),
        )
        .file("bigint_fixture", "broken.xml", "<odoo><record");
    let bad = builder(&db, &addons).build().unwrap();
    enroll_fixture_transition(&bad, &["bigint_fixture"]).await;
    let error = bad
        .button_immediate_upgrade(&su_of(&bad), &[n("bigint_fixture")])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("broken.xml"), "{error}");
    assert_eq!(rows(&installed.orm, "SELECT atttypid::regtype::text FROM pg_attribute WHERE attrelid='bigint_lifecycle'::regclass AND attname='counter'").await[0][0].as_deref(), Some("integer"));
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT counter::text FROM bigint_lifecycle ORDER BY id"
        )
        .await,
        before
    );
    assert_eq!(
        rows(
            &installed.orm,
            "SELECT latest_version FROM ir_module_module WHERE name='bigint_fixture'"
        )
        .await[0][0]
            .as_deref(),
        Some("1.0.0")
    );
    addons.add(
        "bigint_fixture",
        "name='Bigint fixture'\nversion='2.0.0'\ndepends=['base']\n",
    );
    let good = builder(&db, &addons).build().unwrap();
    enroll_fixture_transition(&good, &["bigint_fixture"]).await;
    let outcome = good
        .button_immediate_upgrade(&su_of(&good), &[n("bigint_fixture")])
        .await
        .unwrap();
    assert_eq!(rows(&outcome.orm, "SELECT atttypid::regtype::text FROM pg_attribute WHERE attrelid='bigint_lifecycle'::regclass AND attname='counter'").await[0][0].as_deref(), Some("bigint"));
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT counter::text FROM bigint_lifecycle ORDER BY id"
        )
        .await,
        before
    );
    assert_eq!(
        rows(
            &outcome.orm,
            "SELECT latest_version FROM ir_module_module WHERE name='bigint_fixture'"
        )
        .await[0][0]
            .as_deref(),
        Some("2.0.0")
    );
    exec(
        &outcome.orm,
        "INSERT INTO bigint_lifecycle (counter) VALUES (9223372036854775807)",
    )
    .await;
    let repeated = good
        .button_immediate_upgrade(&su_of(&good), &[n("bigint_fixture")])
        .await
        .unwrap();
    committed(repeated);
    db.cleanup().await.unwrap();
}
