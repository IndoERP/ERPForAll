//! Functional transport regressions; performance acceptance remains deferred.
#![allow(clippy::unwrap_used, reason = "tests")]
use erp_server::assets::{AssetGeneration, AssetSource};
use sha2::{Digest, Sha256};
fn fixture() -> (erp_server::assets::PublishedAssets, String) {
    let bytes = b"export const asset = true;".to_vec();
    let hash = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let path = format!("{hash}.js");
    let assets = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(AssetSource::new(&path, "text/javascript; charset=utf-8", bytes).unwrap())
        .unwrap()
        .build(&erp_server::assets::AssetStore::new(256 * 1024 * 1024).unwrap())
        .unwrap();
    (assets, path)
}
#[test]
fn identity_response_varies_on_encoding() {
    let (assets, path) = fixture();
    let reply = assets.response(&path, None, None).unwrap().unwrap();
    assert_eq!(
        reply.headers().get("vary").map(|v| v.to_str().unwrap()),
        Some("Accept-Encoding")
    );
}
#[test]
fn excluding_every_available_representation_returns_406() {
    let (assets, path) = fixture();
    let reply = assets
        .response(&path, None, Some("identity;q=0, *;q=0"))
        .unwrap()
        .unwrap();
    assert_eq!(reply.status(), 406);
    assert_eq!(reply.headers()["cache-control"], "no-store");
}

fn encoded_fixture() -> (
    erp_server::assets::PublishedAssets,
    String,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
) {
    use std::io::Write;
    let identity = b"export const encoded = 'correct bytes';".repeat(32);
    let hash = Sha256::digest(&identity)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let path = format!("{hash}.js");
    let mut encoder = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::best());
    encoder.write_all(&identity).unwrap();
    let gzip = encoder.finish().unwrap();
    let mut brotli = Vec::new();
    brotli::BrotliCompress(
        &mut identity.as_slice(),
        &mut brotli,
        &brotli::enc::BrotliEncoderParams::default(),
    )
    .unwrap();
    let assets = AssetGeneration::new("elang-1", "c".repeat(64))
        .unwrap()
        .asset(
            AssetSource::with_encodings(
                &path,
                "text/javascript; charset=utf-8",
                identity.clone(),
                gzip.clone(),
                brotli.clone(),
            )
            .unwrap(),
        )
        .unwrap()
        .build(&erp_server::assets::AssetStore::new(1_000_000).unwrap())
        .unwrap();
    (assets, path, identity, gzip, brotli)
}
#[tokio::test]
async fn encoding_matrix_selects_only_an_acceptable_verified_form() {
    let (assets, path, identity, gzip, brotli) = encoded_fixture();
    for (value, expected) in [
        (None, None),
        (Some(""), None),
        (Some(", ,"), None),
        (Some("br"), Some("br")),
        (Some("GZIP"), Some("gzip")),
        (Some("gzip,br"), Some("br")),
        (Some("br;q=0.2,gzip;q=0.9"), Some("gzip")),
        (Some("br;q=0,gzip;q=0.5"), Some("gzip")),
        (Some("*"), Some("br")),
        (Some("br;q=0,*;q=1"), Some("gzip")),
        (Some("identity;q=1,br;q=0.5"), None),
        (Some("unknown"), None),
        (Some("*;q=0,identity;q=1"), None),
    ] {
        let reply = assets.response(&path, None, value).unwrap().unwrap();
        assert_eq!(reply.status(), 200, "{value:?}");
        assert_eq!(
            reply
                .headers()
                .get("content-encoding")
                .map(|v| v.to_str().unwrap()),
            expected,
            "{value:?}"
        );
        assert_eq!(reply.headers()["vary"], "Accept-Encoding");
        let bytes = axum::body::to_bytes(reply.into_body(), 1_000_000)
            .await
            .unwrap();
        let expected_bytes = match expected {
            Some("br") => &brotli,
            Some("gzip") => &gzip,
            None => &identity,
            _ => unreachable!(),
        };
        assert_eq!(bytes.as_ref(), expected_bytes.as_slice(), "{value:?}");
    }
    for value in [
        "identity;q=0,br;q=0,gzip;q=0",
        "*;q=0",
        "identity;q=0,unknown",
    ] {
        let reply = assets.response(&path, None, Some(value)).unwrap().unwrap();
        assert_eq!(reply.status(), 406, "{value}");
        assert_eq!(reply.headers()["cache-control"], "no-store");
    }
    for value in [
        "br;q=1.1",
        "br;q=.5",
        "br;q=-1",
        "br;q=0.1234",
        "br;q=wat",
        "br;q=1;q=0",
        "br,BR",
        "gzip;what=1",
        "br;q=0.5;foo=bar",
        "\u{00a0}",
    ] {
        let error = assets.response(&path, None, Some(value)).unwrap_err();
        assert_eq!(
            error.kind(),
            erp_server::assets::PublicationErrorKind::Encoding,
            "{value}"
        );
        assert_eq!(
            error.to_string(),
            "asset publication refused: invalid Accept-Encoding"
        );
    }
}
#[tokio::test]
async fn validators_belong_to_actual_selected_representation_bytes() {
    let (assets, path, _, _, _) = encoded_fixture();
    let mut tags = Vec::new();
    for encoding in ["identity", "gzip", "br"] {
        let response = assets
            .response(&path, None, Some(encoding))
            .unwrap()
            .unwrap();
        let tag = response.headers()["etag"].to_str().unwrap().to_owned();
        assert!(!tags.contains(&tag));
        for validator in [
            tag.clone(),
            format!("W/{tag}"),
            format!("\"other\", {tag}"),
            "*".into(),
        ] {
            let reply = assets
                .response(&path, Some(&validator), Some(encoding))
                .unwrap()
                .unwrap();
            assert_eq!(reply.status(), 304);
            assert_eq!(reply.headers()["etag"], tag);
            assert_eq!(reply.headers()["vary"], "Accept-Encoding");
            assert_eq!(
                reply.headers()["cache-control"],
                "private, max-age=31536000, immutable"
            );
            assert!(
                axum::body::to_bytes(reply.into_body(), 100)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        tags.push(tag);
    }
    let reply = assets
        .response(&path, Some(&tags[0]), Some("br"))
        .unwrap()
        .unwrap();
    assert_eq!(reply.status(), 200);
}
#[test]
fn corrupt_representations_are_not_admitted() {
    use std::io::Write;
    let bytes = b"export const safe = true;".to_vec();
    let path = format!(
        "{}.js",
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let mut gzip = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::best());
    gzip.write_all(&bytes).unwrap();
    let gzip = gzip.finish().unwrap();
    let mut br = Vec::new();
    brotli::BrotliCompress(
        &mut bytes.as_slice(),
        &mut br,
        &brotli::enc::BrotliEncoderParams::default(),
    )
    .unwrap();
    let mut trailing_gzip = gzip.clone();
    trailing_gzip.push(0);
    let mut trailing_br = br.clone();
    trailing_br.push(0);
    for (gz, br, expected) in [
        (trailing_gzip, br.clone(), "gzip trailing data"),
        (gzip.clone(), trailing_br, "Brotli trailing data"),
        (b"bad".to_vec(), br, "invalid gzip representation"),
        (gzip, b"bad".to_vec(), "invalid Brotli representation"),
    ] {
        let error = AssetSource::with_encodings(
            &path,
            "text/javascript; charset=utf-8",
            bytes.clone(),
            gz,
            br,
        )
        .unwrap_err();
        assert_eq!(
            error.kind(),
            erp_server::assets::PublicationErrorKind::Representation
        );
        assert_eq!(
            error.to_string(),
            format!("asset publication refused: {expected}")
        );
    }
}
#[test]
fn conflicting_forms_on_the_same_url_are_refused() {
    use std::io::Write;
    let bytes = b"export const same=1;".to_vec();
    let path = format!(
        "{}.js",
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    let plain = AssetSource::new(&path, "text/javascript; charset=utf-8", bytes.clone()).unwrap();
    let mut gz = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::best());
    gz.write_all(&bytes).unwrap();
    let gz = gz.finish().unwrap();
    let mut br = Vec::new();
    brotli::BrotliCompress(
        &mut bytes.as_slice(),
        &mut br,
        &brotli::enc::BrotliEncoderParams::default(),
    )
    .unwrap();
    let encoded =
        AssetSource::with_encodings(&path, "text/javascript; charset=utf-8", bytes, gz, br)
            .unwrap();
    let error = AssetGeneration::new("elang-1", "a".repeat(64))
        .unwrap()
        .asset(plain)
        .unwrap()
        .asset(encoded)
        .unwrap_err();
    assert_eq!(error.kind(), erp_server::assets::PublicationErrorKind::Path);
    assert_eq!(
        error.to_string(),
        "asset publication refused: conflicting asset path"
    );
}

#[tokio::test]
async fn distinct_valid_gzip_forms_conflict_within_a_generation_and_keep_separate_validators() {
    use std::io::Write;
    let (_, path, identity, gzip, brotli) = encoded_fixture();
    let mut encoder = flate2::GzBuilder::new()
        .mtime(1)
        .write(Vec::new(), flate2::Compression::best());
    encoder.write_all(&identity).unwrap();
    let alternative_gzip = encoder.finish().unwrap();
    assert_ne!(gzip, alternative_gzip);
    let source = |gzip: Vec<u8>| {
        AssetSource::with_encodings(
            &path,
            "text/javascript; charset=utf-8",
            identity.clone(),
            gzip,
            brotli.clone(),
        )
        .unwrap()
    };
    for (first, second) in [
        (gzip.clone(), alternative_gzip.clone()),
        (alternative_gzip.clone(), gzip.clone()),
    ] {
        let error = AssetGeneration::new("elang-1", "d".repeat(64))
            .unwrap()
            .asset(source(first))
            .unwrap()
            .asset(source(second))
            .unwrap_err();
        assert_eq!(error.kind(), erp_server::assets::PublicationErrorKind::Path);
        assert_eq!(
            error.to_string(),
            "asset publication refused: conflicting asset path"
        );
    }
    let store = erp_server::assets::AssetStore::new(1_000_000).unwrap();
    let first = AssetGeneration::new("elang-1", "d".repeat(64))
        .unwrap()
        .asset(source(gzip.clone()))
        .unwrap()
        .build(&store)
        .unwrap();
    let second = AssetGeneration::new("elang-1", "e".repeat(64))
        .unwrap()
        .asset(source(alternative_gzip.clone()))
        .unwrap()
        .build(&store)
        .unwrap();
    let first_reply = first.response(&path, None, Some("gzip")).unwrap().unwrap();
    let first_tag = first_reply.headers()["etag"].to_str().unwrap().to_owned();
    assert_eq!(first_reply.headers()["content-encoding"], "gzip");
    assert_eq!(
        axum::body::to_bytes(first_reply.into_body(), 1_000_000)
            .await
            .unwrap()
            .as_ref(),
        gzip
    );
    let second_reply = second
        .response(&path, Some(&first_tag), Some("gzip"))
        .unwrap()
        .unwrap();
    assert_eq!(second_reply.status(), 200);
    let second_tag = second_reply.headers()["etag"].to_str().unwrap().to_owned();
    assert_ne!(second_tag, first_tag);
    assert_eq!(second_reply.headers()["content-encoding"], "gzip");
    assert_eq!(
        axum::body::to_bytes(second_reply.into_body(), 1_000_000)
            .await
            .unwrap()
            .as_ref(),
        alternative_gzip
    );
}
