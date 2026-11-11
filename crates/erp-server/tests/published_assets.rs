//! Admission and response contracts for tenant-selected published assets.
#![allow(clippy::unwrap_used, reason = "tests")]
use erp_server::assets::{AssetGeneration, AssetSource};
use sha2::{Digest, Sha256};
fn path(bytes: &[u8]) -> String {
    format!(
        "{}.js",
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}
#[test]
fn admitted_generation_serves_only_its_declared_hashes() {
    let bytes = b"export {};".to_vec();
    let hash_path = path(&bytes);
    let generation = AssetGeneration::new("runtime-1", "a".repeat(64))
        .unwrap()
        .asset(AssetSource::new(&hash_path, "text/javascript; charset=utf-8", bytes).unwrap())
        .unwrap()
        .import("fixture", &format!("/erp/assets/{hash_path}"))
        .unwrap()
        .static_file(
            "/fixture/static/main.js",
            &format!("/erp/assets/{hash_path}"),
        )
        .unwrap()
        .build(&erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap())
        .unwrap();
    let response = generation
        .response(&hash_path, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["cache-control"],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(
        response.headers()["content-type"],
        "text/javascript; charset=utf-8"
    );
    let tag = response.headers()["etag"].to_str().unwrap().to_owned();
    assert_eq!(
        generation
            .response(&hash_path, Some(&tag), None)
            .unwrap()
            .unwrap()
            .status(),
        304
    );
    assert!(
        generation
            .response(&format!("{}.js", "b".repeat(64)), None, None)
            .unwrap()
            .is_none()
    );
    let error = generation.response("../secret", None, None).unwrap_err();
    assert_eq!(error.kind(), erp_server::assets::PublicationErrorKind::Path);
    assert_eq!(
        error.to_string(),
        "asset publication refused: expected lowercase SHA-256 and allowed extension"
    );
    assert!(generation.fixed_path("/fixture/static/main.js").is_some());
    assert!(generation.fixed_path("/other/static/main.js").is_none());
}
#[test]
fn wrong_digest_and_foreign_import_fail_admission() {
    let error = AssetSource::new(
        &format!("{}.js", "a".repeat(64)),
        "text/javascript; charset=utf-8",
        b"wrong".to_vec(),
    )
    .unwrap_err();
    assert_eq!(
        error.kind(),
        erp_server::assets::PublicationErrorKind::Digest
    );
    assert_eq!(
        error.to_string(),
        "asset publication refused: asset digest mismatch"
    );
    let error = AssetGeneration::new("runtime-1", "a".repeat(64))
        .unwrap()
        .import("fixture", "https://outside.invalid/module.js")
        .unwrap_err();
    assert_eq!(error.kind(), erp_server::assets::PublicationErrorKind::Path);
    assert_eq!(
        error.to_string(),
        "asset publication refused: expected local content-addressed asset URL"
    );
}
#[test]
fn import_map_escapes_script_terminators_and_rejects_missing_blobs() {
    let error = AssetGeneration::new("runtime-1", "a".repeat(64))
        .unwrap()
        .import("fixture", &format!("/erp/assets/{}.js", "a".repeat(64)))
        .unwrap()
        .build(&erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap())
        .unwrap_err();
    assert_eq!(
        error.kind(),
        erp_server::assets::PublicationErrorKind::Manifest
    );
    assert_eq!(
        error.to_string(),
        "asset publication refused: unreferenced asset URL in manifest"
    );
}
#[test]
fn shell_uses_a_fresh_nonce_and_exact_origin_csp() {
    let generation = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .build(&erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap())
        .unwrap();
    let a = generation.shell("https://erp.example.test").unwrap();
    let b = generation.shell("https://erp.example.test").unwrap();
    assert_ne!(a.nonce(), b.nonce());
    assert!(
        a.import_map_html()
            .contains(&format!("nonce=\"{}\"", a.nonce()))
    );
    assert!(a.csp().contains(&format!("'nonce-{}'", a.nonce())));
    assert!(
        a.csp()
            .contains("connect-src 'self' wss://erp.example.test;")
    );
    assert!(a.csp().contains("frame-ancestors 'none'"));
    assert!(!a.csp().contains("unsafe-eval"));
    assert!(!a.csp().contains("wss:;"));
}
#[test]
fn node_cache_reuses_equal_bytes_without_granting_a_foreign_manifest() {
    let bytes = b"export const cached=true;".to_vec();
    let path = path(&bytes);
    let store = erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap();
    let a = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(AssetSource::new(&path, "text/javascript; charset=utf-8", bytes.clone()).unwrap())
        .unwrap()
        .build(&store)
        .unwrap();
    let cost = store.used_bytes();
    let b = AssetGeneration::new("elang-1", "b".repeat(64))
        .unwrap()
        .asset(AssetSource::new(&path, "text/javascript; charset=utf-8", bytes).unwrap())
        .unwrap()
        .build(&store)
        .unwrap();
    assert!(store.used_bytes() > cost);
    assert!(
        store.used_bytes() < 2 * cost,
        "shared content with separately charged manifest ownership"
    );
    assert!(a.response(&path, None, None).unwrap().is_some());
    assert!(b.response(&path, None, None).unwrap().is_some());
    let foreign = AssetGeneration::new("elang-1", "b".repeat(64))
        .unwrap()
        .build(&store)
        .unwrap();
    assert!(foreign.response(&path, None, None).unwrap().is_none());
}
#[test]
fn svg_documents_cannot_execute_active_content() {
    let bytes = b"<svg xmlns='http://www.w3.org/2000/svg'><script>alert(1)</script></svg>".to_vec();
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let path = format!("{hash}.svg");
    let assets = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(AssetSource::new(&path, "image/svg+xml", bytes).unwrap())
        .unwrap()
        .build(&erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap())
        .unwrap();
    let response = assets.response(&path, None, None).unwrap().unwrap();
    assert_eq!(
        response.headers()["content-security-policy"],
        "default-src 'none'; style-src 'unsafe-inline'; sandbox"
    );
}
