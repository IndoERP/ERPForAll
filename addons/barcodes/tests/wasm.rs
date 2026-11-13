// SPDX-License-Identifier: MIT OR Apache-2.0
//! Executes the browser artifact when the dedicated conformance script supplies its bundle.
#![allow(clippy::unwrap_used, reason = "test assertions")]
use erp_addon_barcodes::engine::Nomenclature;
#[test]
fn shared_corpus_is_native_parseable() {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/core-corpus.json")).unwrap();
    for case in corpus.as_array().unwrap() {
        let compiled = Nomenclature::from_json(&case["snapshot"].to_string());
        if let Some(expected) = case.get("compile_error") {
            let error = compiled.unwrap_err();
            assert_eq!(error.kind(), expected["kind"].as_str().unwrap());
            assert_eq!(error.to_string(), expected["message"].as_str().unwrap());
            continue;
        }
        let n = compiled.unwrap();
        let result = n.parse_json(case["code"].as_str().unwrap());
        if let Some(error) = case.get("error") {
            let result = result.unwrap_err();
            assert_eq!(result.kind(), error["kind"].as_str().unwrap());
            assert_eq!(result.to_string(), error["message"].as_str().unwrap());
        } else {
            let result: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
            assert_eq!(result, case["result"]);
        }
    }
}

#[test]
fn browser_wasm_corpus_executes() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = std::process::Command::new("bash")
        .arg("scripts/barcode-wasm-check.sh")
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "barcode WASM build or corpus failed");
}
