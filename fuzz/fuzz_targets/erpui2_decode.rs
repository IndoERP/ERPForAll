#![no_main]

use erp_ui_compiler_cli::{EXIT_CLEAN, EXIT_ERRORS, Request, respond};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // 1. Request::decode must never panic on arbitrary bytes
    if let Ok(request) = Request::decode(data) {
        // Round-trip property (review Round 2 M2):
        // A successfully decoded request must encode without error
        let encoded = match request.encode() {
            Ok(bytes) => bytes,
            Err(err) => panic!("successfully decoded request failed to encode: {err}"),
        };

        // And re-decoding the encoded bytes must produce an equal Request
        let re_decoded = match Request::decode(&encoded) {
            Ok(req) => req,
            Err(err) => panic!("encoded request failed to re-decode: {err}"),
        };
        assert_eq!(
            request, re_decoded,
            "Request::decode -> encode -> decode must be an identity"
        );
    }

    // 2. respond must never panic on arbitrary bytes
    if let Ok(response) = respond(data) {
        let status = response.exit_status();
        if response.is_clean() {
            assert_eq!(status, EXIT_CLEAN);
        } else {
            assert_eq!(status, EXIT_ERRORS);
        }

        // Determinism: calling respond again on the same bytes yields identical result
        if let Ok(response2) = respond(data) {
            assert_eq!(response.bytes(), response2.bytes());
            assert_eq!(response.is_clean(), response2.is_clean());
            assert_eq!(response.exit_status(), response2.exit_status());
        } else {
            panic!("respond must be deterministic for identical input");
        }
    }
});
