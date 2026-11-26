// SPDX-License-Identifier: MIT OR Apache-2.0
//! Admission of the compiler artifact: pinned digest, import allowlist, required exports.

use crate::error::ArtifactError;
use sha2::{Digest, Sha256};
use std::fmt;
use wasmi::{CompilationMode, Config, Engine, ExternType, Module};

/// The one import module the sandbox provides.
pub const ALLOWED_WASI_MODULE: &str = "wasi_snapshot_preview1";

/// The WASI preview 1 functions the host provides: standard input and output, exit, empty
/// arguments and environment, and a deterministic `random_get`. An artifact importing anything
/// else (a file, socket, clock or `env` function, or a non-function import) is refused.
pub const ALLOWED_WASI_FUNCTIONS: [&str; 8] = [
    "fd_write",
    "fd_read",
    "proc_exit",
    "args_sizes_get",
    "args_get",
    "environ_sizes_get",
    "environ_get",
    "random_get",
];

/// A pinned artifact digest: the SHA-256 of the artifact as 64 lowercase hexadecimal characters.
///
/// The value comes from the trusted assembly result that built the artifact, never from the
/// artifact or its metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtifactDigest(String);

impl ArtifactDigest {
    /// Parses a pinned digest.
    ///
    /// # Errors
    /// [`ArtifactError::InvalidDigest`] unless `hex` is exactly 64 characters of `0-9a-f`.
    pub fn parse(hex: &str) -> Result<Self, ArtifactError> {
        if hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(hex.to_owned()))
        } else {
            Err(ArtifactError::InvalidDigest {
                value: hex.to_owned(),
            })
        }
    }

    /// The digest as 64 lowercase hexadecimal characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ArtifactDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lowercase hexadecimal SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

/// The engine configuration every admitted module is compiled and run with: fuel metering on
/// (the host slices it), eager compilation (no compilation fuel charged during the run), no
/// `start` section (it would run un-sliced during instantiation), and a call stack as deep as the
/// compiler's 8 MiB linear-memory stack needs (the values the native/WASM identity test runs
/// the web corpus with).
fn engine_config() -> Config {
    let mut config = Config::default();
    config.consume_fuel(true);
    config.compilation_mode(CompilationMode::Eager);
    config.allow_start_fn(false);
    config.set_max_recursion_depth(64 * 1024);
    config.set_max_stack_height(256 * 1024 * 1024);
    config
}

/// An admitted compiler artifact: digest verified, module compiled with the host's engine
/// configuration, imports and exports checked.
#[derive(Clone)]
pub struct WasmArtifact {
    engine: Engine,
    module: Module,
    digest: ArtifactDigest,
}

impl fmt::Debug for WasmArtifact {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmArtifact")
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

impl WasmArtifact {
    /// Read and admit an operator-provisioned guest under a trusted digest.
    ///
    /// # Errors
    /// Fails when the file cannot be read, exceeds 64 MiB, or fails artifact admission.
    pub fn load(path: &std::path::Path, expected: &ArtifactDigest) -> Result<Self, ArtifactError> {
        use std::io::Read as _;
        const MAX_GUEST: u64 = 64 * 1024 * 1024;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .and_then(|file| file.take(MAX_GUEST + 1).read_to_end(&mut bytes))
            .map_err(|err| {
                ArtifactError::InvalidModule(format!("cannot read guest file ({})", err.kind()))
            })?;
        if bytes.len() as u64 > MAX_GUEST {
            return Err(ArtifactError::InvalidModule("guest exceeds 64 MiB".into()));
        }
        Self::admit(bytes, expected)
    }

    /// Admits `bytes` as the compiler artifact pinned by `expected`.
    ///
    /// In this order: the SHA-256 of `bytes` must equal `expected` (before any parsing); the
    /// bytes must parse and validate as a module without a `start` section; every import must be
    /// a function of [`ALLOWED_WASI_MODULE`] named in [`ALLOWED_WASI_FUNCTIONS`]; the module must
    /// export a function `_start` of type `() -> ()` and a memory `memory`.
    ///
    /// # Errors
    /// [`ArtifactError::DigestMismatch`], [`ArtifactError::InvalidModule`],
    /// [`ArtifactError::ForbiddenImport`] or [`ArtifactError::MissingExport`], in that order.
    pub fn admit(bytes: Vec<u8>, expected: &ArtifactDigest) -> Result<Self, ArtifactError> {
        let actual = sha256_hex(&bytes);
        if actual != expected.as_str() {
            return Err(ArtifactError::DigestMismatch {
                expected: expected.as_str().to_owned(),
                actual,
            });
        }
        let engine = Engine::new(&engine_config());
        let module = Module::new(&engine, &bytes)
            .map_err(|e| ArtifactError::InvalidModule(e.to_string()))?;
        for import in module.imports() {
            let allowed = import.module() == ALLOWED_WASI_MODULE
                && ALLOWED_WASI_FUNCTIONS.contains(&import.name())
                && matches!(import.ty(), ExternType::Func(_));
            if !allowed {
                return Err(ArtifactError::ForbiddenImport {
                    module: import.module().to_owned(),
                    name: import.name().to_owned(),
                });
            }
        }
        let has_start = module.exports().any(|e| {
            e.name() == "_start"
                && matches!(e.ty(), ExternType::Func(f) if f.params().is_empty() && f.results().is_empty())
        });
        if !has_start {
            return Err(ArtifactError::MissingExport {
                name: "_start",
                kind: "function () -> ()",
            });
        }
        let has_memory = module
            .exports()
            .any(|e| e.name() == "memory" && matches!(e.ty(), ExternType::Memory(_)));
        if !has_memory {
            return Err(ArtifactError::MissingExport {
                name: "memory",
                kind: "memory",
            });
        }
        Ok(Self {
            engine,
            module,
            digest: expected.clone(),
        })
    }

    /// The pinned digest the artifact was admitted under.
    #[must_use]
    pub fn digest(&self) -> &ArtifactDigest {
        &self.digest
    }

    pub(crate) fn engine(&self) -> &Engine {
        &self.engine
    }

    pub(crate) fn module(&self) -> &Module {
        &self.module
    }
}
