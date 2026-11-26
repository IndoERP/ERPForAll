// SPDX-License-Identifier: MIT OR Apache-2.0
//! Compile one framed database arch using the operator's pinned WASI artifact.
use erp_ui_compiler_cli::{ArchCompileRequest, read_request};
use erp_ui_compiler_host::{
    ArtifactDigest, CancelToken, ExecutionLimits, WasmArtifact, WasmCompilerHost,
};
use std::{error::Error, io::Write, process::ExitCode};

fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [path, digest] = args.as_slice() else {
        return Err("usage: erp-ui-compiler-host GUEST.wasm PINNED_SHA256 < request.erpui2".into());
    };
    let digest = ArtifactDigest::parse(digest.to_str().ok_or("digest must be UTF-8")?)?;
    let artifact = WasmArtifact::load(std::path::Path::new(path), &digest)?;
    let request = ArchCompileRequest::decode(&read_request(std::io::stdin().lock())?)?;
    let host = WasmCompilerHost::new(&artifact, ExecutionLimits::default());
    let result = host.compile_arch(&request, &CancelToken::new())?;
    std::io::stdout().lock().write_all(result.bytes())?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("erp-ui-compiler-host: {error}");
            ExitCode::FAILURE
        }
    }
}
