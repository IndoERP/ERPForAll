//! Content detection and the declared-type rule (and the
//! detector half/093, which are divergent: the stored type is the detected one
//! for every user).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_blob::{ContentClass, DetectError, detect, resolve};

const PNG: &[u8] =
    b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01\x00\x00\x00\x01\x08\x06\x00\x00\x00";
const JPEG: &[u8] = b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00";
const PDF: &[u8] = b"%PDF-1.7\n1 0 obj\n<< >>\nendobj\n";
const ZIP: &[u8] = b"PK\x03\x04\x14\x00\x00\x00\x08\x00";

#[test]
fn binaries_are_detected_by_magic_number_and_are_passive() {
    for (bytes, mimetype) in [
        (PNG, "image/png"),
        (JPEG, "image/jpeg"),
        (PDF, "application/pdf"),
        (ZIP, "application/zip"),
    ] {
        let found = detect(bytes, Some("whatever.txt"));
        assert_eq!(found.mimetype, mimetype);
        assert_eq!(found.class, ContentClass::Passive);
    }
}

#[test]
fn markup_is_active_whatever_the_name_says() {
    for (bytes, mimetype) in [
        (
            &b"<!DOCTYPE html><html><body>x</body></html>"[..],
            "text/html",
        ),
        (b"  \n<html><head></head></html>", "text/html"),
        (b"<script>alert(1)</script>", "text/html"),
        (
            b"<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            "image/svg+xml",
        ),
        (
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
            "image/svg+xml",
        ),
        (b"<?xml version=\"1.0\"?><root/>", "text/xml"),
        (b"\xef\xbb\xbf<html></html>", "text/html"),
        (b"#!/bin/sh\necho hi\n", "text/x-shellscript"),
    ] {
        let found = detect(bytes, Some("photo.png"));
        assert_eq!(
            found.mimetype,
            mimetype,
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
        assert_eq!(found.class, ContentClass::Active);
    }
}

#[test]
fn text_takes_a_passive_type_from_a_fixed_list_else_plain() {
    for (name, mimetype) in [
        ("a.csv", "text/csv"),
        ("A.CSV", "text/csv"),
        ("a.json", "application/json"),
        ("a.md", "text/markdown"),
        ("a.tsv", "text/tab-separated-values"),
        ("a.html", "text/plain"),
        ("a.exe", "text/plain"),
        ("noextension", "text/plain"),
    ] {
        let found = detect(b"hello, world\n1,2,3\n", Some(name));
        assert_eq!(found.mimetype, mimetype, "{name}");
        assert_eq!(found.class, ContentClass::Passive);
    }
}

#[test]
fn unknown_binary_is_an_octet_stream() {
    let found = detect(&[0, 159, 146, 150, 0, 1, 2, 3], Some("a.bin"));
    assert_eq!(found.mimetype, "application/octet-stream");
    assert_eq!(found.class, ContentClass::Passive);
}

#[test]
fn a_head_that_ends_inside_a_character_is_still_text() {
    let mut bytes = "caf\u{e9}".as_bytes().to_vec();
    bytes.pop();
    assert_eq!(detect(&bytes, None).mimetype, "text/plain");
}

#[test]
fn detection_reads_only_the_first_8_kib() {
    let mut bytes = vec![b'a'; 8192];
    bytes.extend_from_slice(b"<html><script>");
    assert_eq!(detect(&bytes, None).mimetype, "text/plain");
}

#[test]
fn a_passive_declared_type_on_active_content_is_refused_naming_both_types() {
    let err = resolve(
        Some("image/png"),
        b"<html><body>x</body></html>",
        Some("a.png"),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "declared content type image/png does not match the content, which is text/html"
    );
    assert!(matches!(err, DetectError::Mismatch { .. }));
    let err = resolve(Some("text/plain; charset=utf-8"), b"<svg/>", None).unwrap_err();
    assert_eq!(
        err.to_string(),
        "declared content type text/plain does not match the content, which is image/svg+xml"
    );
    // application/octet-stream is passive too
    assert!(resolve(Some("application/octet-stream"), b"<html></html>", None).is_err());
}

#[test]
fn an_active_declared_type_on_active_content_and_any_declared_type_on_passive_content_pass() {
    let html = resolve(Some("text/html"), b"<html></html>", Some("a.html")).unwrap();
    assert_eq!(html.mimetype, "text/html");
    assert_eq!(html.class, ContentClass::Active);
    // the stored type is the detected one, not the declared one
    let png = resolve(Some("application/pdf"), PNG, None).unwrap();
    assert_eq!(png.mimetype, "image/png");
    assert_eq!(resolve(None, PNG, None).unwrap().mimetype, "image/png");
}

#[test]
fn a_malformed_declared_type_is_refused() {
    for bad in [
        "",
        "png",
        "image/",
        "/png",
        "image/p ng",
        "a/b/c",
        &"a/".repeat(80),
    ] {
        let err = resolve(Some(bad), b"hello", None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the declared content type is not a valid media type",
            "{bad:?}"
        );
    }
}
