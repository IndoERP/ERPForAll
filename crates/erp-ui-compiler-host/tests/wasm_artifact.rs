// SPDX-License-Identifier: MIT OR Apache-2.0
//! The shipped compiler artefact through the host.
//!
//! Admits the `wasm32-wasip1` build of `erp-ui-compile` (which proves its imports are within the
//! host's eight-function allowlist), runs trusted requests through it and compares them byte for
//! byte with the native `respond()`, and checks the data plan and diagnostics produced by the guest.
//!
//! Build the release WASI compiler, set `ERP_UI_COMPILE_WASM` to its path, and run this target
//! with the `test-raw-input` feature and the test harness flag `--ignored`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests"
)]

use erp_ui_compiler::{AddonName, BuildMode, TemplatePath};
use erp_ui_compiler_cli::{
    ArchCompileRequest, EXIT_CLEAN, EXIT_ERRORS, Operation, Request, respond,
};
use erp_ui_compiler_host::{
    ArtifactDigest, CancelToken, CompilerHostError, ExecutionLimits, HostExecutionError,
    WasmArtifact, WasmCompilerHost,
};
use sha2::{Digest, Sha256};
use std::path::Path;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The host over the artefact named by `ERP_UI_COMPILE_WASM`, admitted under its own digest
/// (computed here, as the trusted assembly step would pin it).
fn host() -> WasmCompilerHost {
    let path = std::env::var_os("ERP_UI_COMPILE_WASM").unwrap_or_else(|| {
        panic!("ERP_UI_COMPILE_WASM is not set: build the artefact for wasm32-wasip1 and point it at the .wasm file")
    });
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", Path::new(&path).display()));
    let digest = ArtifactDigest::parse(&hex(&Sha256::digest(&bytes))).unwrap();
    let artifact = WasmArtifact::admit(bytes, &digest)
        .unwrap_or_else(|e| panic!("the compiler artefact is not admitted: {e}"));
    WasmCompilerHost::new(&artifact, ExecutionLimits::default())
}

fn trusted(operation: Operation, xml: &str) -> Request {
    Request::new(operation, BuildMode::Prod).with_addon_file(
        AddonName::new("web").unwrap(),
        TemplatePath::new("a.xml").unwrap(),
        xml,
    )
}

#[test]
#[ignore = "requires ERP_UI_COMPILE_WASM built for wasm32-wasip1"]
fn trusted_requests_through_the_host_match_native() {
    let host = host();
    let clean = "<templates><t t-name=\"web.A\"><div><t t-out=\"this.x\"/></div></t></templates>";
    let broken = "<templates><t t-name=\"web.A\"><b t-esc=\"(\"/></t></templates>";
    for (operation, xml, status) in [
        (Operation::Analyze, clean, EXIT_CLEAN),
        (Operation::Compile, clean, EXIT_CLEAN),
        (Operation::Analyze, broken, EXIT_ERRORS),
        (Operation::Compile, broken, EXIT_ERRORS),
    ] {
        let bytes = trusted(operation, xml).encode().unwrap();
        let native = respond(&bytes).unwrap();
        let wasm = host.run_raw_for_tests(&bytes, &CancelToken::new()).unwrap();
        eprintln!(
            "{operation:?} status {status}: {} request bytes, fuel {}",
            bytes.len(),
            wasm.fuel_used()
        );
        assert_eq!(native.exit_status(), status);
        assert_eq!(wasm.exit_status(), status, "{operation:?}");
        assert!(!native.bytes().is_empty());
        assert_eq!(
            wasm.bytes(),
            native.bytes(),
            "{operation:?}: outputs differ"
        );
    }
}

#[test]
#[ignore = "requires ERP_UI_COMPILE_WASM built for wasm32-wasip1"]
fn restricted_guest_emits_a_plan_or_an_original_xml_diagnostic() {
    let host = host();
    for (projection, accepted) in [("name", true), ("missing", false)] {
        let source = format!(
            r#"<templates>
<t t-name="card"><strong><field name="{projection}"/></strong></t>
</templates>"#
        );
        let request = ArchCompileRequest::new(
            Operation::Compile,
            BuildMode::Prod,
            "res.partner.kanban",
            hex(&Sha256::digest(source.as_bytes())),
            "en_US",
            r#"{"fields":["name"]}"#,
            "1",
            source,
        )
        .unwrap();
        let result = host.compile_arch(&request, &CancelToken::new());
        if accepted {
            let response = result.unwrap();
            assert_eq!(response.exit_status(), EXIT_CLEAN);
            let plan = std::str::from_utf8(response.bytes()).unwrap();
            assert!(plan.contains("erpforall.card.v1") && plan.contains(request.source_digest()));
        } else {
            let err = result.unwrap_err();
            assert!(
                matches!(err, CompilerHostError::HostExecution(
                HostExecutionError::ExitStatus {status:2,ref stderr}
            ) if stderr.contains("res.partner.kanban:2:26:")),
                "{err}"
            );
        }
    }
}
