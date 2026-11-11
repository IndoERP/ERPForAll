//! Regression tests: every seed in `fuzz/corpus/erpui2_decode` runs through `Request::decode`
//! and `respond`, verifying decode-encode identity, exit status contract, and determinism.

use erp_ui_compiler_cli::{EXIT_CLEAN, EXIT_MALFORMED, Request, respond};
use std::fs;
use std::path::Path;

#[test]
fn seed_corpus_decodes_and_responds_deterministically() {
    let corpus_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("fuzz/corpus/erpui2_decode");

    assert!(
        corpus_dir.is_dir(),
        "corpus dir must exist: {}",
        corpus_dir.display()
    );

    let mut entries: Vec<_> = fs::read_dir(&corpus_dir)
        .expect("read corpus dir")
        .map(|res| res.expect("read dir entry").path())
        .filter(|p| p.is_file() && !p.file_name().unwrap().to_str().unwrap().starts_with('.'))
        .collect();
    entries.sort();

    assert!(
        !entries.is_empty(),
        "seed corpus must contain at least one file"
    );

    for entry in &entries {
        let bytes = fs::read(entry).expect("read seed file");

        // Decode must succeed on all seed inputs
        let request = Request::decode(&bytes).expect("decode seed request");

        // Encode round-trip property
        let encoded = request.encode().expect("encode request");
        let re_decoded = Request::decode(&encoded).expect("re-decode encoded request");
        assert_eq!(
            request,
            re_decoded,
            "round-trip identity failure for {}",
            entry.display()
        );

        // Respond contract: every curated seed is clean
        let response = respond(&bytes).expect("respond to seed");
        assert!(
            response.is_clean(),
            "all curated seeds must yield clean responses: {}",
            entry.display()
        );
        assert_eq!(
            response.exit_status(),
            EXIT_CLEAN,
            "clean response must have exit status EXIT_CLEAN (0) for {}",
            entry.display()
        );

        // Response determinism
        let response2 = respond(&bytes).expect("respond again to seed");
        assert_eq!(
            response.bytes(),
            response2.bytes(),
            "respond output must be deterministic for {}",
            entry.display()
        );
        assert_eq!(
            response.exit_status(),
            response2.exit_status(),
            "exit status must be deterministic for {}",
            entry.display()
        );
    }
}

#[test]
fn malformed_requests_fail_with_exit_malformed() {
    let malformed_cases: &[(&[u8], &str)] = &[
        (b"", "empty input"),
        (b"INVALID\n", "invalid magic"),
        (b"ERPUI2\n\x00\x00", "truncated framing length"),
        (b"ERPUI2\n\x00\x00\x00\x05bogus", "unknown operation"),
    ];

    for (bytes, desc) in malformed_cases {
        let decode_res = Request::decode(bytes);
        assert!(
            decode_res.is_err(),
            "expected Request::decode error for {desc}"
        );

        let respond_res = respond(bytes);
        assert!(respond_res.is_err(), "expected respond error for {desc}");
        let err = respond_res.unwrap_err();
        assert!(
            !err.message().is_empty(),
            "RequestError message must not be empty for {desc}"
        );

        // The contract in main.rs maps RequestError to EXIT_MALFORMED (2)
        assert_eq!(
            EXIT_MALFORMED, 2,
            "EXIT_MALFORMED constant must remain 2 per protocol contract"
        );
    }
}
