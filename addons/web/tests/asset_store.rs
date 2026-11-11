//! Tenant-local publication and rollback contracts.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
use erp_addon_web::assets::{
    assembly::{AddonAssets, AssembledBundle, AssemblyOptions, AssetInput, assemble},
    store,
};
use erp_sdk::ui_compiler::{BuildMode, CompileOptions};
use erp_testkit::Harness;

async fn database() -> Harness {
    let h = Harness::builder([&erp_addon_base::ADDON, &erp_addon_web::ADDON])
        .without_schema()
        .build()
        .await
        .unwrap();
    let mut tx = h.begin().await.unwrap();
    store::initialize(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    h
}
fn bundle(text: &[u8]) -> AssembledBundle {
    let mut a = AddonAssets::new("web", "1", "runtime-1").unwrap();
    a.push(AssetInput::module("static/dist/main.js", "@erpforall/web", text.to_vec()).unwrap())
        .unwrap();
    a.set_entry("@erpforall/web").unwrap();
    assemble(
        &[a],
        &AssemblyOptions::new(
            "compiler-1",
            "runtime-1",
            "en_US",
            "main",
            CompileOptions::new(BuildMode::Prod),
        )
        .unwrap(),
    )
    .unwrap()
}
#[tokio::test]
async fn rollback_keeps_previous_manifest_and_exact_bytes() {
    let h = database().await;
    let first = bundle(b"export function register() {} // first");
    let mut tx = h.begin().await.unwrap();
    store::publish(&mut tx, &first).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    store::publish(&mut tx, &bundle(b"export function register() {} // next"))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let actual = store::current(&mut tx, "en_US", "main", "runtime-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual, first);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn equal_inputs_have_equal_bytes_in_independent_databases() {
    let a = database().await;
    let b = database().await;
    let expected = bundle(b"export function register() {} // same");
    for h in [&a, &b] {
        let mut tx = h.begin().await.unwrap();
        store::publish(&mut tx, &expected).await.unwrap();
        tx.commit().await.unwrap();
    }
    let mut tx = a.begin().await.unwrap();
    store::publish(&mut tx, &bundle(b"export function register() {} // other"))
        .await
        .unwrap();
    tx.execute("UPDATE ir_ui_asset_bundle SET expires_at = CURRENT_TIMESTAMP - INTERVAL '1 second' WHERE expires_at IS NOT NULL", &[]).await.unwrap();
    store::collect(&mut tx).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = b.begin().await.unwrap();
    assert_eq!(
        store::current(&mut tx, "en_US", "main", "runtime-1")
            .await
            .unwrap()
            .unwrap(),
        expected
    );
    let count: i64 = tx
        .query("SELECT count(*) FROM ir_ui_asset_blob", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(count as usize, expected.blobs().len());
    tx.rollback().await.unwrap();
    a.cleanup().await.unwrap();
    b.cleanup().await.unwrap();
}
#[tokio::test]
async fn refuses_corrupt_bytes_and_incompatible_runtime() {
    let h = database().await;
    let expected = bundle(b"export function register() {}");
    let mut tx = h.begin().await.unwrap();
    store::publish(&mut tx, &expected).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let error = store::current(&mut tx, "en_US", "main", "runtime-2")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.to_string(),
        "asset runtime 'runtime-1' does not match 'runtime-2'"
    );
    tx.execute(
        "UPDATE ir_ui_asset_blob SET bytes=$1",
        &[&b"corrupt".to_vec()],
    )
    .await
    .unwrap();
    let error = store::current(&mut tx, "en_US", "main", "runtime-1")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.to_string(),
        "invalid asset blob or manifest: stored UI bundle is corrupt: blob SHA-256 mismatch"
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn compatible_previous_hashes_remain_referenced_until_grace_expiry() {
    let h = database().await;
    let first = bundle(b"export function register() {} // before");
    let next = bundle(b"export function register() {} // after");
    let mut tx = h.begin().await.unwrap();
    store::publish(&mut tx, &first).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    store::publish(&mut tx, &next).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let retained = store::retained(&mut tx, "en_US", "main", "runtime-1")
        .await
        .unwrap();
    assert!(retained.iter().any(|b| b == &first));
    assert!(retained.iter().any(|b| b == &next));
    tx.execute("UPDATE ir_ui_asset_bundle SET expires_at=CURRENT_TIMESTAMP - INTERVAL '1 second' WHERE expires_at IS NOT NULL",&[]).await.unwrap();
    store::collect(&mut tx).await.unwrap();
    assert_eq!(
        store::retained(&mut tx, "en_US", "main", "runtime-1")
            .await
            .unwrap(),
        vec![next]
    );
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}
#[tokio::test]
async fn one_raw_row_supports_multiple_declared_media_types() {
    let h = database().await;
    let bytes = b"/*! valid empty JS and CSS */".to_vec();
    let mut addon = AddonAssets::new("web", "1", "runtime-1").unwrap();
    addon
        .push(AssetInput::module("static/dist/empty.js", "shared", bytes.clone()).unwrap())
        .unwrap();
    addon
        .push(AssetInput::static_file("static/empty.css", bytes).unwrap())
        .unwrap();
    let expected = assemble(
        &[addon],
        &AssemblyOptions::new(
            "compiler-1",
            "runtime-1",
            "en_US",
            "main",
            CompileOptions::new(BuildMode::Prod),
        )
        .unwrap(),
    )
    .unwrap();
    let mut tx = h.begin().await.unwrap();
    store::publish(&mut tx, &expected).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = h.begin().await.unwrap();
    let current = store::current(&mut tx, "en_US", "main", "runtime-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current, expected);
    let hashes: std::collections::BTreeSet<_> =
        expected.blobs().iter().map(|b| b.sha256()).collect();
    let actual: i64 = tx
        .query("SELECT count(*) FROM ir_ui_asset_blob", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(actual as usize, hashes.len());
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn republishing_a_corrupt_current_reference_set_refuses_before_garbage_collection() {
    let h = database().await;
    let expected = bundle(b"export function register() {} // refset corruption");
    let mut tx = h.begin().await.unwrap();
    store::publish(&mut tx, &expected).await.unwrap();
    let original: i64 = tx
        .query("SELECT count(*) FROM ir_ui_asset_blob", &[])
        .await
        .unwrap()[0]
        .get(0);
    tx.execute(
        "UPDATE ir_ui_asset_bundle SET blob_hashes=$1",
        &[&vec!["a".repeat(64)]],
    )
    .await
    .unwrap();
    let error = store::publish(&mut tx, &expected).await.unwrap_err();
    assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation);
    assert_eq!(
        error.to_string(),
        "asset bundle key conflicts with stored manifest identity or references"
    );
    let after: i64 = tx
        .query("SELECT count(*) FROM ir_ui_asset_blob", &[])
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(after, original);
    tx.rollback().await.unwrap();
    h.cleanup().await.unwrap();
}

#[tokio::test]
async fn republishing_a_corrupt_current_identity_refuses_before_garbage_collection() {
    let h = database().await;
    let expected = bundle(b"export function register() {} // identity corruption");
    for column in [
        "compiler_version",
        "runtime_version",
        "language",
        "lazy_group",
    ] {
        let mut tx = h.begin().await.unwrap();
        store::publish(&mut tx, &expected).await.unwrap();
        // Column names are the closed fixture list above; values remain parameters.
        tx.execute(
            &format!("UPDATE ir_ui_asset_bundle SET {column}=$1"),
            &[&"corrupt"],
        )
        .await
        .unwrap();
        let error = store::publish(&mut tx, &expected).await.unwrap_err();
        assert_eq!(error.kind(), erp_sdk::ErrorKind::Validation, "{column}");
        assert_eq!(
            error.to_string(),
            "asset bundle key conflicts with stored manifest identity or references",
            "{column}"
        );
        tx.rollback().await.unwrap();
    }
    h.cleanup().await.unwrap();
}
