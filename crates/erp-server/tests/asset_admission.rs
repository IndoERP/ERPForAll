//! Resident admission is a safety contract, independent of product performance measurements.
#![allow(clippy::unwrap_used, reason = "tests")]
use erp_server::assets::{AssetGeneration, AssetSource, AssetStore, PublicationErrorKind};
use sha2::{Digest, Sha256};
use std::sync::Arc;
fn source(bytes: &[u8]) -> AssetSource {
    let hash = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    AssetSource::new(
        &format!("{hash}.js"),
        "text/javascript; charset=utf-8",
        bytes.to_vec(),
    )
    .unwrap()
}
fn generation(store: &Arc<AssetStore>, bytes: &[u8]) -> erp_server::assets::PublishedAssets {
    AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(source(bytes))
        .unwrap()
        .build(store)
        .unwrap()
}
#[tokio::test]
async fn pinned_bodies_remain_charged_until_their_last_owner_drops() {
    let store = AssetStore::new(1_000_000).unwrap();
    let bytes = b"export const pinned = true;";
    let published = generation(&store, bytes);
    let cost = store.used_bytes();
    assert!(cost > bytes.len() as u64);
    let path = format!(
        "{}.js",
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let body = published
        .response(&path, None, None)
        .unwrap()
        .unwrap()
        .into_body();
    drop(published);
    store.clear_idle().unwrap();
    assert!(store.used_bytes() > 0);
    assert!(
        store.used_bytes() < cost,
        "publication metadata releases while body payload stays charged"
    );
    drop(body);
    store.clear_idle().unwrap();
    assert_eq!(store.used_bytes(), 0);
    let exact = AssetStore::new(cost).unwrap();
    let accepted = generation(&exact, bytes);
    assert_eq!(exact.used_bytes(), cost);
    let one_under = AssetStore::new(cost - 1).unwrap();
    let error = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(source(bytes))
        .unwrap()
        .build(&one_under)
        .unwrap_err();
    assert_eq!(error.kind(), PublicationErrorKind::Limit);
    assert_eq!(
        error.to_string(),
        "asset publication refused: assets.memory_bytes admission limit exceeded"
    );
    assert_eq!(one_under.used_bytes(), 0);
    drop(accepted);
}
#[test]
fn shared_content_does_not_double_charge_or_evict_live_snapshots() {
    let probe = AssetStore::new(1_000_000).unwrap();
    let first = generation(&probe, b"export const a=1;");
    let cost = probe.used_bytes();
    drop(first);
    let store = AssetStore::new(2 * cost - 1).unwrap();
    let a = generation(&store, b"export const a=1;");
    let same = generation(&store, b"export const a=1;");
    let shared = store.used_bytes();
    assert!(shared > cost, "each snapshot owns metadata");
    assert!(
        shared < 2 * cost,
        "resident payload and headers are deduplicated"
    );
    let error = AssetGeneration::new("elang-1", "b".repeat(64))
        .unwrap()
        .asset(source(b"export const b=2;"))
        .unwrap()
        .build(&store)
        .unwrap_err();
    assert_eq!(error.kind(), PublicationErrorKind::Limit);
    assert_eq!(
        error.to_string(),
        "asset publication refused: assets.memory_bytes admission limit exceeded"
    );
    assert_eq!(store.used_bytes(), shared);
    drop(a);
    drop(same);
    store.clear_idle().unwrap();
    assert_eq!(store.used_bytes(), 0);
    let b = generation(&store, b"export const b=2;");
    assert_eq!(store.used_bytes(), cost);
    drop(b);
}
#[test]
fn concurrent_candidates_cannot_spend_the_same_reservation() {
    let probe = AssetStore::new(1_000_000).unwrap();
    let _one = generation(&probe, b"export const a=1;");
    let store = AssetStore::new(probe.used_bytes()).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let threads = [
            b"export const a=1;".as_slice(),
            b"export const b=2;".as_slice(),
        ]
        .map(|bytes| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                AssetGeneration::new("elang-1", "c".repeat(64))
                    .unwrap()
                    .asset(source(bytes))
                    .unwrap()
                    .build(&store)
            })
        });
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    for result in &results {
        if let Err(error) = result {
            assert_eq!(error.kind(), PublicationErrorKind::Limit);
            assert_eq!(
                error.to_string(),
                "asset publication refused: assets.memory_bytes admission limit exceeded"
            );
        }
    }
    assert_eq!(store.used_bytes(), store.limit());
    drop(results);
    store.clear_idle().unwrap();
    assert_eq!(store.used_bytes(), 0);
}
#[test]
fn failed_generation_keeps_no_partial_candidate_reservation() {
    let store = AssetStore::new(10_000).unwrap();
    let error = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(source(b"export const first=1;"))
        .unwrap()
        .asset(source(b"export const second=2;"))
        .unwrap()
        .build(&store)
        .unwrap_err();
    assert_eq!(error.kind(), PublicationErrorKind::Limit);
    assert_eq!(
        error.to_string(),
        "asset publication refused: assets.memory_bytes admission limit exceeded"
    );
    assert_eq!(store.used_bytes(), 0);
}
#[test]
fn declared_alias_metadata_cannot_bypass_the_resident_allowance() {
    let store = AssetStore::new(10_000).unwrap();
    let bytes = b"export const aliases=1;";
    let path = format!(
        "{}.js",
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let url = format!("/erp/assets/{path}");
    let mut candidate = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(source(bytes))
        .unwrap();
    for n in 0..32 {
        candidate = candidate
            .import(&format!("alias{n}"), &url)
            .unwrap()
            .static_file(&format!("/fixture/static/alias{n}.js"), &url)
            .unwrap();
    }
    let error = candidate.build(&store).unwrap_err();
    assert_eq!(error.kind(), PublicationErrorKind::Limit);
    assert_eq!(
        error.to_string(),
        "asset publication refused: assets.memory_bytes admission limit exceeded"
    );
    assert_eq!(store.used_bytes(), 0);
}
#[test]
fn release_and_tenant_publication_share_one_admission_budget() {
    use erp_server::assets::{AssetBundle, AssetError};
    let dir = tempfile::tempdir().unwrap();
    let mut entries = Vec::new();
    for (name, extension, mime, bytes) in [
        (
            "client.js",
            "js",
            "text/javascript; charset=utf-8",
            b"export {};".as_slice(),
        ),
        (
            "client.css",
            "css",
            "text/css; charset=utf-8",
            b"body{}".as_slice(),
        ),
    ] {
        let hash = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let path = format!("client.{hash}.{extension}");
        std::fs::write(dir.path().join(&path), bytes).unwrap();
        entries
            .push(serde_json::json!({"name":name,"path":path,"sha256":hash,"content_type":mime}));
    }
    let manifest = dir.path().join("manifest.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({"schema":1,"assets":entries})).unwrap(),
    )
    .unwrap();
    let probe = AssetStore::new(1_000_000).unwrap();
    let release = AssetBundle::load(&manifest, &probe).unwrap();
    let release_cost = probe.used_bytes();
    let probe_tenant = AssetStore::new(1_000_000).unwrap();
    let tenant = generation(&probe_tenant, b"export const tenant=1;");
    let tenant_cost = probe_tenant.used_bytes();
    let store = AssetStore::new(release_cost + tenant_cost - 1).unwrap();
    let _release = AssetBundle::load(&manifest, &store).unwrap();
    let error = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(source(b"export const tenant=1;"))
        .unwrap()
        .build(&store)
        .unwrap_err();
    assert_eq!(error.kind(), PublicationErrorKind::Limit);
    assert_eq!(
        error.to_string(),
        "asset publication refused: assets.memory_bytes admission limit exceeded"
    );
    assert_eq!(store.used_bytes(), release_cost);
    let too_small = AssetStore::new(release_cost - 1).unwrap();
    let error = match AssetBundle::load(&manifest, &too_small) {
        Ok(_) => panic!("release exceeds allowance"),
        Err(error) => error,
    };
    match error {
        AssetError::Admission(error) => {
            assert_eq!(error.kind(), PublicationErrorKind::Limit);
            assert_eq!(
                error.to_string(),
                "asset publication refused: assets.memory_bytes admission limit exceeded"
            );
        }
        other => panic!("unexpected error: {other}"),
    }
    assert_eq!(too_small.used_bytes(), 0);
    drop(release);
    drop(tenant);
}

async fn detached_dynamic_headers_keep_their_resident_charge(conditional: bool) {
    let bytes = b"export const detached=1;";
    let path = format!(
        "{}.js",
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let probe = AssetStore::new(1_000_000).unwrap();
    let probe_publication = generation(&probe, bytes);
    let complete_cost = probe.used_bytes();
    drop(probe_publication);
    probe.clear_idle().unwrap();
    assert_eq!(probe.used_bytes(), 0);

    let store = AssetStore::new(complete_cost).unwrap();
    let published = generation(&store, bytes);
    assert_eq!(store.used_bytes(), complete_cost);
    let validator = published
        .response(&path, None, None)
        .unwrap()
        .unwrap()
        .headers()["etag"]
        .to_str()
        .unwrap()
        .to_owned();
    let response = published
        .response(&path, conditional.then_some(validator.as_str()), None)
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), if conditional { 304 } else { 200 });
    let detached_etag = response.headers()["etag"].clone();
    let detached_media = response.headers()["content-type"].clone();
    assert_eq!(detached_etag, validator);
    assert_eq!(detached_media, "text/javascript; charset=utf-8");
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    if conditional {
        assert!(body.is_empty());
    } else {
        assert_eq!(body.as_ref(), bytes);
    }
    // Consume/drop the 200 body (or the genuinely empty 304 body), all response-owned
    // headers and publication ownership. Only detached dynamic HeaderValues remain.
    drop(body);
    drop(published);
    store.clear_idle().unwrap();
    let pinned_charge = store.used_bytes();
    assert!(
        pinned_charge > 0,
        "detached resident header storage must retain its admission charge"
    );
    assert!(
        pinned_charge < complete_cost,
        "publication metadata releases independently from shared header storage"
    );
    let error = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(source(b"export const detached=2;"))
        .unwrap()
        .build(&store)
        .unwrap_err();
    assert_eq!(error.kind(), PublicationErrorKind::Limit);
    assert_eq!(
        error.to_string(),
        "asset publication refused: assets.memory_bytes admission limit exceeded"
    );
    assert_eq!(store.used_bytes(), pinned_charge);
    drop(detached_etag);
    assert_eq!(
        store.used_bytes(),
        pinned_charge,
        "the independently detached dynamic media header still owns the charge"
    );
    drop(detached_media);
    store.clear_idle().unwrap();
    assert_eq!(store.used_bytes(), 0);
}

#[tokio::test]
async fn detached_200_headers_pin_resident_admission_until_their_final_drop() {
    detached_dynamic_headers_keep_their_resident_charge(false).await;
}

#[tokio::test]
async fn detached_304_headers_pin_resident_admission_until_their_final_drop() {
    detached_dynamic_headers_keep_their_resident_charge(true).await;
}
