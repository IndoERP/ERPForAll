// SPDX-License-Identifier: MIT OR Apache-2.0
//! The canonical arch request framing (ADR 0118 D1) and the native refusal of arch requests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_ui_compiler::{AddonName, BuildMode, LimitKind, TemplatePath};
use erp_ui_compiler_cli::{
    ArchCompileRequest, EXIT_CLEAN, MAGIC, MAX_ARCH_FRAME_BYTES, Operation, Request, respond,
};
use sha2::{Digest, Sha256};

const SOURCE: &str = "<kanban><templates><t t-name=\"kanban-box\"><div><t t-esc=\"record.name.value\"/></div></t></templates></kanban>";

const NATIVE_REFUSAL: &str = "native processing refuses arch requests before parsing them: database-stored arch is compiled only by the WASM build in the compiler host (SPEC-UIRESTRICT-001)";

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn request_with(source: &str) -> Result<ArchCompileRequest, erp_ui_compiler_cli::RequestError> {
    ArchCompileRequest::new(
        Operation::Compile,
        BuildMode::Prod,
        "res.partner.kanban",
        sha256_hex(source),
        "en_US",
        "{\"fields\":[\"name\",\"email\"]}",
        "1",
        source,
    )
}

fn sample() -> ArchCompileRequest {
    request_with(SOURCE)
        .unwrap()
        .with_limit(LimitKind::Depth, 16)
        .unwrap()
}

fn record(tag: &str, value: &str) -> Vec<u8> {
    let len = u32::try_from(tag.len() + 1 + value.len()).unwrap();
    let mut out = len.to_be_bytes().to_vec();
    out.extend_from_slice(tag.as_bytes());
    out.push(b' ');
    out.extend_from_slice(value.as_bytes());
    out
}

fn framed(records: &[(&str, &str)]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for (tag, value) in records {
        out.extend(record(tag, value));
    }
    out
}

/// Owned `(tag, value)` records of a frame.
type RecordList = Vec<(&'static str, String)>;

/// The canonical records of `sample()`, with `digest` as the source_digest value.
fn sample_records_with(digest: &str) -> RecordList {
    vec![
        ("op", "compile".to_owned()),
        ("mode", "prod".to_owned()),
        ("arch_id", "res.partner.kanban".to_owned()),
        ("source_digest", digest.to_owned()),
        ("language", "en_US".to_owned()),
        (
            "host_schema",
            "{\"fields\":[\"name\",\"email\"]}".to_owned(),
        ),
        ("compiler_policy", "1".to_owned()),
        ("limit", "depth=16".to_owned()),
        ("file", "arch_data".to_owned()),
        ("source", SOURCE.to_owned()),
    ]
}

fn frame_of(records: &[(&str, String)]) -> Vec<u8> {
    let refs: Vec<(&str, &str)> = records.iter().map(|(t, v)| (*t, v.as_str())).collect();
    framed(&refs)
}

fn decode_error(records: &[(&str, String)]) -> String {
    ArchCompileRequest::decode(&frame_of(records))
        .unwrap_err()
        .message()
        .to_owned()
}

/// SPEC-UIRESTRICT-017 canonical arch framing and malformed request diagnostics.
#[test]
fn arch_request_round_trips_byte_for_byte() {
    let request = sample();
    let bytes = request.encode().unwrap();
    assert_eq!(bytes, frame_of(&sample_records_with(&sha256_hex(SOURCE))));
    let decoded = ArchCompileRequest::decode(&bytes).unwrap();
    assert_eq!(decoded, request);
    assert_eq!(decoded.encode().unwrap(), bytes);
    assert_eq!(decoded.operation(), Operation::Compile);
    assert_eq!(decoded.mode(), BuildMode::Prod);
    assert_eq!(decoded.arch_id(), "res.partner.kanban");
    assert_eq!(decoded.source_digest(), sha256_hex(SOURCE));
    assert_eq!(decoded.language(), "en_US");
    assert_eq!(decoded.host_schema(), "{\"fields\":[\"name\",\"email\"]}");
    assert_eq!(decoded.compiler_policy(), "1");
    assert_eq!(decoded.limits(), &[(LimitKind::Depth, 16)]);
    assert_eq!(decoded.source(), SOURCE);
}

/// SPEC-UIRESTRICT-017 canonical arch framing and malformed request diagnostics.
#[test]
fn source_digest_must_be_the_sha256_of_the_source() {
    let wrong = sha256_hex("<kanban/>");
    let err = ArchCompileRequest::new(
        Operation::Compile,
        BuildMode::Prod,
        "res.partner.kanban",
        wrong.as_str(),
        "en_US",
        "{}",
        "1",
        SOURCE,
    )
    .unwrap_err();
    let expected = format!(
        "source_digest {wrong} is not the SHA-256 of the source ({})",
        sha256_hex(SOURCE)
    );
    assert_eq!(err.message(), expected);
    // The same check on the decode path: a frame whose digest does not match its source.
    assert_eq!(decode_error(&sample_records_with(&wrong)), expected);
    // One flipped source byte after encoding is caught too.
    let mut bytes = sample().encode().unwrap();
    let last = bytes.len() - 2;
    bytes[last] = b'X';
    let err = ArchCompileRequest::decode(&bytes).unwrap_err();
    assert!(
        err.message().starts_with(&format!(
            "source_digest {} is not the SHA-256 of the source (",
            sha256_hex(SOURCE)
        )),
        "{}",
        err.message()
    );
}

/// SPEC-UIRESTRICT-017 canonical arch framing and malformed request diagnostics.
#[test]
fn malformed_arch_requests_fail_loudly() {
    let digest = sha256_hex(SOURCE);
    let base = sample_records_with(&digest);
    let with = |edit: &dyn Fn(&mut RecordList)| {
        let mut records = base.clone();
        edit(&mut records);
        decode_error(&records)
    };
    // Field validation.
    assert_eq!(
        with(&|r| r[3].1 = "not-a-valid-sha256".to_owned()),
        "source_digest must be a 64-character lowercase hexadecimal SHA-256 digest, got 'not-a-valid-sha256'"
    );
    assert_eq!(
        with(&|r| r[3].1 = digest.to_uppercase()),
        format!(
            "source_digest must be a 64-character lowercase hexadecimal SHA-256 digest, got '{}'",
            digest.to_uppercase()
        )
    );
    assert_eq!(with(&|r| r[2].1 = String::new()), "arch_id cannot be empty");
    assert_eq!(
        with(&|r| r[4].1 = String::new()),
        "language cannot be empty"
    );
    assert_eq!(
        with(&|r| r[5].1 = String::new()),
        "host_schema cannot be empty"
    );
    assert_eq!(
        with(&|r| r[2].1 = "a\nb".to_owned()),
        "arch_id cannot contain control characters"
    );
    assert_eq!(
        with(&|r| r[6].1 = "p".repeat(33)),
        "compiler_policy length must be 1..=32, got 33"
    );
    // Duplicate, unknown and out-of-order records.
    assert_eq!(
        with(&|r| r.insert(8, ("limit", "depth=8".to_owned()))),
        "limit depth is given twice"
    );
    assert_eq!(
        with(&|r| r[7].1 = "depth=0".to_owned()),
        "limit depth cannot be set to 0: allowed 1..=64 (limits can only be lowered)"
    );
    assert_eq!(
        with(&|r| r[7].1 = "speed=1".to_owned()),
        "unknown limit 'speed'"
    );
    assert_eq!(
        with(&|r| r.push(("limit", "files=1".to_owned()))),
        "record 11: 'limit' after the arch file"
    );
    assert_eq!(
        with(&|r| {
            r.push(("file", "arch_data".to_owned()));
            r.push(("source", SOURCE.to_owned()));
        }),
        "record 11: 'file' after the arch file"
    );
    assert_eq!(
        with(&|r| r[8].1 = "addon_file".to_owned()),
        "record 9: unknown file kind 'addon_file' in an arch request"
    );
    // Untrusted values are echoed bounded: 80 characters, then the full byte length.
    assert_eq!(
        with(&|r| r[3].1 = "g".repeat(5000)),
        format!(
            "source_digest must be a 64-character lowercase hexadecimal SHA-256 digest, got '{}... (5000 bytes)'",
            "g".repeat(80)
        )
    );
    assert_eq!(
        with(&|r| r[8].1 = "k".repeat(81)),
        format!(
            "record 9: unknown file kind '{}... (81 bytes)' in an arch request",
            "k".repeat(80)
        )
    );
    assert_eq!(
        with(&|r| r[8].1 = "k".repeat(80)),
        format!(
            "record 9: unknown file kind '{}' in an arch request",
            "k".repeat(80)
        )
    );
    assert_eq!(
        with(&|r| r.insert(7, ("directive", "click".to_owned()))),
        "record 8: unexpected tag 'directive' in an arch request"
    );
    assert_eq!(
        with(&|r| {
            r.pop();
            r.pop();
        }),
        "missing 'file arch_data' record"
    );
    assert_eq!(
        with(&|r| {
            r.pop();
        }),
        "missing 'source' record"
    );
    assert_eq!(
        with(&|r| r.swap(3, 4)),
        "record 4: expected 'source_digest', got 'language'"
    );
    assert_eq!(
        ArchCompileRequest::decode(b"ERPUI1\n")
            .unwrap_err()
            .message(),
        "missing ERPUI2 magic"
    );
}

/// SPEC-UIRESTRICT-017 canonical arch framing and malformed request diagnostics.
#[test]
fn the_whole_frame_is_bounded_by_the_input_bytes_ceiling() {
    assert_eq!(MAX_ARCH_FRAME_BYTES, LimitKind::InputBytes.default_value());
    // A source exactly at the ceiling passes no per-field check that matters: the frame around
    // it is what goes over.
    let source = "a".repeat(MAX_ARCH_FRAME_BYTES);
    let err = request_with(&source).unwrap_err();
    let frame = sample_frame_len_for(&source);
    assert_eq!(
        err.message(),
        format!(
            "the arch request frame of {frame} bytes exceeds the input-bytes ceiling of {MAX_ARCH_FRAME_BYTES} bytes"
        )
    );
    // Decoding refuses an oversized frame before reading any record (the bytes here are not
    // even a valid frame after the magic).
    let mut bytes = MAGIC.to_vec();
    bytes.resize(MAX_ARCH_FRAME_BYTES + 1, 0);
    assert_eq!(
        ArchCompileRequest::decode(&bytes).unwrap_err().message(),
        format!(
            "the arch request frame of {} bytes exceeds the input-bytes ceiling of {MAX_ARCH_FRAME_BYTES} bytes",
            MAX_ARCH_FRAME_BYTES + 1
        )
    );
    // A lowered limit adds a record, so it is checked against the ceiling too.
    let fits = "a".repeat(MAX_ARCH_FRAME_BYTES - sample_frame_len_for(""));
    let request = request_with(&fits).unwrap();
    assert_eq!(request.encode().unwrap().len(), MAX_ARCH_FRAME_BYTES);
    let err = request.with_limit(LimitKind::Depth, 8).unwrap_err();
    assert_eq!(
        err.message(),
        format!(
            "the arch request frame of {} bytes exceeds the input-bytes ceiling of {MAX_ARCH_FRAME_BYTES} bytes",
            MAX_ARCH_FRAME_BYTES + record("limit", "depth=8").len()
        )
    );
}

/// The frame length of `request_with(source)` (no limits), computed from the record encoding.
fn sample_frame_len_for(source: &str) -> usize {
    let digest = sha256_hex(source);
    let records = [
        ("op", "compile"),
        ("mode", "prod"),
        ("arch_id", "res.partner.kanban"),
        ("source_digest", digest.as_str()),
        ("language", "en_US"),
        ("host_schema", "{\"fields\":[\"name\",\"email\"]}"),
        ("compiler_policy", "1"),
        ("file", "arch_data"),
    ];
    let head: usize = records.iter().map(|(t, v)| 4 + t.len() + 1 + v.len()).sum();
    MAGIC.len() + head + 4 + "source".len() + 1 + source.len()
}

/// SPEC-UIRESTRICT-017 canonical arch framing and malformed request diagnostics.
#[test]
fn the_trusted_decoder_refuses_arch_records_anywhere() {
    let error_of = |records: &[(&str, &str)]| {
        Request::decode(&framed(records))
            .unwrap_err()
            .message()
            .to_owned()
    };
    // A complete arch request.
    assert_eq!(
        Request::decode(&sample().encode().unwrap())
            .unwrap_err()
            .message(),
        "record 3: unexpected tag 'arch_id' (use ArchCompileRequest to decode arch requests)"
    );
    // arch_id as the first or second record.
    assert_eq!(
        error_of(&[("arch_id", "x"), ("op", "compile"), ("mode", "prod")]),
        "record 1: expected 'op', got 'arch_id'"
    );
    assert_eq!(
        error_of(&[("op", "compile"), ("arch_id", "x"), ("mode", "prod")]),
        "record 2: expected 'mode', got 'arch_id'"
    );
    // arch_id after a directive, after a limit and after a file.
    assert_eq!(
        error_of(&[
            ("op", "compile"),
            ("mode", "prod"),
            ("directive", "click"),
            ("arch_id", "x"),
        ]),
        "record 4: unexpected tag 'arch_id' (use ArchCompileRequest to decode arch requests)"
    );
    assert_eq!(
        error_of(&[
            ("op", "compile"),
            ("mode", "prod"),
            ("limit", "depth=8"),
            ("arch_id", "x"),
        ]),
        "record 4: unexpected tag 'arch_id' (use ArchCompileRequest to decode arch requests)"
    );
    assert_eq!(
        error_of(&[
            ("op", "compile"),
            ("mode", "prod"),
            ("file", "addon_file"),
            ("addon", "web"),
            ("path", "a.xml"),
            ("source", "<templates/>"),
            ("arch_id", "x"),
        ]),
        "record 7: unexpected tag 'arch_id' (use ArchCompileRequest to decode arch requests)"
    );
    // The arch_data file kind in a trusted request.
    assert_eq!(
        error_of(&[
            ("op", "compile"),
            ("mode", "prod"),
            ("file", "arch_data"),
            ("source", "<kanban/>"),
        ]),
        "record 3: unexpected file kind 'arch_data' in addon Request (use ArchCompileRequest)"
    );
    // Mixed: an addon file, then an arch file.
    assert_eq!(
        error_of(&[
            ("op", "compile"),
            ("mode", "prod"),
            ("file", "addon_file"),
            ("addon", "web"),
            ("path", "a.xml"),
            ("source", "<templates/>"),
            ("file", "arch_data"),
            ("source", "<kanban/>"),
        ]),
        "record 7: unexpected file kind 'arch_data' in addon Request (use ArchCompileRequest)"
    );
}

/// SPEC-UIRESTRICT-001 (partial: native processing refuses arch requests before XML parsing; trusted requests unaffected; view adapter not implemented)
#[test]
fn native_processing_refuses_arch_requests_before_parsing() {
    assert_eq!(
        respond(&sample().encode().unwrap()).unwrap_err().message(),
        NATIVE_REFUSAL
    );
    // Malformed XML gets the same refusal, not a parse diagnostic: nothing was parsed.
    let broken = "<kanban><t t-esc=\"(\"></kanban";
    let request = request_with(broken).unwrap();
    assert_eq!(
        respond(&request.encode().unwrap()).unwrap_err().message(),
        NATIVE_REFUSAL
    );
    // So does a frame whose later records are invalid: the native build does not decode it.
    let mut records = sample_records_with(&"0".repeat(64));
    records[5].1 = String::new();
    assert_eq!(
        respond(&frame_of(&records)).unwrap_err().message(),
        NATIVE_REFUSAL
    );
}

/// SPEC-UIRESTRICT-001 (partial: native processing refuses arch requests before XML parsing; trusted requests unaffected; view adapter not implemented)
#[test]
fn trusted_requests_still_answer() {
    let request = Request::new(Operation::Analyze, BuildMode::Prod).with_addon_file(
        AddonName::new("web").unwrap(),
        TemplatePath::new("a.xml").unwrap(),
        "<templates><t t-name=\"web.A\"><b/></t></templates>",
    );
    let response = respond(&request.encode().unwrap()).unwrap();
    assert_eq!(response.exit_status(), EXIT_CLEAN);
    let text = String::from_utf8(response.into_bytes()).unwrap();
    assert!(text.contains("templates 1 passed 1 rejected 0"), "{text}");
}
