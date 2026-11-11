//! Deterministic install-time assembly. Inputs are declarations supplied by the host in addon
//! load order; this module performs no filesystem, database or request-time work. Prebuilt dist
//! modules retain their bytes (including legal notices), and XML is compiled as one template set.

use erp_sdk::ui_compiler::{
    self as compiler, AddonName, BuildMode, CompileErrors, CompileOptions, LimitKind, TemplatePath,
    TemplateSet, TemplateSource,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

static ASSEMBLY_CALLS: AtomicU64 = AtomicU64::new(0);

/// Diagnostic count of assembly attempts in this process, including refused inputs.
///
/// Used by isolated HTTP regressions to prove requests do not rebuild. It is never authority,
/// an admission budget or a cache key; it saturates explicitly at `u64::MAX` rather than wrapping.
pub fn assembly_calls() -> u64 {
    ASSEMBLY_CALLS.load(Ordering::Relaxed)
}

fn record_assembly_call() -> u64 {
    match ASSEMBLY_CALLS.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |calls| {
        calls.checked_add(1)
    }) {
        Ok(previous) => previous.saturating_add(1),
        // The update closure refuses only at MAX: diagnostic saturation is intentional.
        Err(saturated) => saturated,
    }
}

/// Hard admission ceiling for a declared file or generated module, in bytes.
pub const MAX_ASSET_BYTES: usize = 16 * 1024 * 1024;
/// Hard ceiling for all identity and encoded artifact bytes of one assembled bundle.
pub const MAX_BUNDLE_BYTES: usize = 384 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 128 * 1024 * 1024;
const MAX_FILES: usize = 20_000;
const MAX_ADDONS: usize = 4096;
const MAX_BLOBS: usize = 40_001;
const MAX_METADATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_SERIALIZED_BYTES: usize = 512 * 1024 * 1024;
// only newly assembled forms use these settings. Stored valid variants are
// admitted verbatim by Blob::from_stored; changing these settings also changes the bundle key.
const GZIP_LEVEL: u32 = 9;
const BROTLI_QUALITY: i32 = 11;
const BROTLI_WINDOW: i32 = 22;

/// Stable category of assembly failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AssemblyErrorKind {
    /// Invalid declaration or metadata.
    InvalidInput,
    /// Admission bound exceeded.
    Limit,
    /// Declared names, paths or content conflict.
    Conflict,
    /// Unsupported lazy group or static media.
    Unsupported,
    /// Incompatible addon runtime ABI.
    RuntimeAbi,
    /// Template compiler rejected the set.
    Compile,
    /// Encoding or serialization failed.
    Encoding,
    /// Stored artifact or metadata did not pass admission.
    CorruptBundle,
}

/// Assembly failures retain the original compiler/encoding cause where applicable.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AssemblyError {
    /// Invalid bounded declaration.
    #[error("invalid {field} '{value}': {reason}")]
    InvalidInput {
        /// Field that was refused.
        field: &'static str,
        /// Bounded display of the refused value.
        value: String,
        /// Expected grammar.
        reason: &'static str,
    },
    /// Hard bound, never an operational performance gate.
    #[error("UI asset {name} exceeds hard limit {max}")]
    Limit {
        /// Bound name.
        name: &'static str,
        /// Maximum admitted value.
        max: usize,
    },
    /// Lazy groups are a completion-slice feature.
    #[error("lazy group '{group}' is not supported until a later release")]
    UnsupportedGroup {
        /// Refused group.
        group: String,
    },
    /// Explicit static-file allowlist refused this suffix.
    #[error("unsupported static media type for '{path}'")]
    UnsupportedMedia {
        /// Declared path.
        path: String,
    },
    /// A declaration has two meanings.
    #[error("duplicate {kind} '{value}'")]
    Duplicate {
        /// Addon, path or specifier.
        kind: &'static str,
        /// Duplicate identity.
        value: String,
    },
    /// A content hash resolved to different identity bytes.
    #[error("content hash '{sha256}' has conflicting identity bytes")]
    ContentConflict {
        /// Conflicting hash.
        sha256: String,
    },
    /// The runtime cannot admit the installed addon.
    #[error("addon '{addon}' runtime ABI '{actual}' does not match '{expected}'")]
    RuntimeAbi {
        /// Addon.
        addon: String,
        /// Running ABI.
        expected: String,
        /// Declared ABI.
        actual: String,
    },
    /// Register export must be in the declared set.
    #[error("addon '{addon}' entry '{specifier}' is not a declared module")]
    MissingEntry {
        /// Addon.
        addon: String,
        /// Missing module.
        specifier: String,
    },
    /// Compiling the complete set failed; no bundle was produced.
    #[error("UI template assembly failed: {source}")]
    Compile {
        /// All original diagnostics, including source locations.
        #[source]
        source: CompileErrors,
    },
    /// Compiled template modules must resolve their runtime helper import.
    #[error("compiled templates require declared runtime module '{specifier}'")]
    MissingRuntime {
        /// Required bare module specifier.
        specifier: String,
    },
    /// Compiler-generated JavaScript unexpectedly failed its production minifier.
    #[error("generated UI module '{module}' cannot be minified: {reason}")]
    GeneratedModule {
        /// Template name or registration module identity.
        module: String,
        /// Bounded parser diagnostic or internal invariant failure.
        reason: String,
    },
    /// Production output must not reveal source maps.
    #[error("production module '{addon}/{path}' contains sourceMappingURL")]
    ProductionSourceMap {
        /// Owning addon.
        addon: String,
        /// Declared path.
        path: String,
    },
    /// Serialization failure.
    #[error("UI asset JSON encoding failed: {0}")]
    Json(#[source] serde_json::Error),
    /// Compression failure.
    #[error("UI asset compression failed: {0}")]
    Compression(#[source] std::io::Error),
    /// A stored bundle cannot be served.
    #[error("stored UI bundle is corrupt: {reason}")]
    CorruptBundle {
        /// Specific failed admission invariant.
        reason: &'static str,
    },
}

impl AssemblyError {
    /// Stable error category for callers and reports.
    pub fn kind(&self) -> AssemblyErrorKind {
        match self {
            Self::InvalidInput { .. }
            | Self::MissingEntry { .. }
            | Self::MissingRuntime { .. }
            | Self::ProductionSourceMap { .. } => AssemblyErrorKind::InvalidInput,
            Self::Limit { .. } => AssemblyErrorKind::Limit,
            Self::Duplicate { .. } | Self::ContentConflict { .. } => AssemblyErrorKind::Conflict,
            Self::UnsupportedGroup { .. } | Self::UnsupportedMedia { .. } => {
                AssemblyErrorKind::Unsupported
            }
            Self::RuntimeAbi { .. } => AssemblyErrorKind::RuntimeAbi,
            Self::Compile { .. } | Self::GeneratedModule { .. } => AssemblyErrorKind::Compile,
            Self::Json(_) | Self::Compression(_) => AssemblyErrorKind::Encoding,
            Self::CorruptBundle { .. } => AssemblyErrorKind::CorruptBundle,
        }
    }
}

fn invalid(field: &'static str, value: &str, reason: &'static str) -> AssemblyError {
    // Error payloads are bounded even when the refused input is not.
    let value: String = value.chars().take(256).collect();
    AssemblyError::InvalidInput {
        field,
        value,
        reason,
    }
}
fn token(field: &'static str, value: &str, max: usize) -> Result<(), AssemblyError> {
    if value.is_empty()
        || value.len() > max
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'+'))
    {
        return Err(invalid(field, value, "expected a bounded ASCII identity"));
    }
    Ok(())
}
fn static_path(path: &str) -> Result<(), AssemblyError> {
    if path.len() > 1024
        || !path.starts_with("static/")
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'))
    {
        return Err(invalid(
            "path",
            path,
            "expected a normalized relative static path",
        ));
    }
    Ok(())
}
fn specifier(value: &str) -> Result<(), AssemblyError> {
    if value.is_empty()
        || value.len() > 256
        || value.starts_with(['.', '/'])
        || value.ends_with('/')
        || value
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'_' | b'-' | b'.' | b'/'))
    {
        return Err(invalid(
            "specifier",
            value,
            "expected a bounded bare module specifier",
        ));
    }
    Ok(())
}
fn media(path: &str) -> Result<(&'static str, &'static str), AssemblyError> {
    let pair = match path.rsplit('.').next() {
        Some("js") => ("text/javascript; charset=utf-8", "js"),
        Some("css") => ("text/css; charset=utf-8", "css"),
        Some("json") => ("application/json; charset=utf-8", "json"),
        Some("svg") => ("image/svg+xml", "svg"),
        Some("png") => ("image/png", "png"),
        Some("jpg" | "jpeg") => ("image/jpeg", "jpg"),
        Some("gif") => ("image/gif", "gif"),
        Some("webp") => ("image/webp", "webp"),
        Some("ico") => ("image/x-icon", "ico"),
        Some("woff") => ("font/woff", "woff"),
        Some("woff2") => ("font/woff2", "woff2"),
        Some("txt") => ("text/plain; charset=utf-8", "txt"),
        // Other suffixes are unsupported input, never a guessed content type.
        _ => {
            return Err(AssemblyError::UnsupportedMedia {
                path: path.to_owned(),
            });
        }
    };
    Ok(pair)
}
fn bounded_file(bytes: &[u8]) -> Result<(), AssemblyError> {
    if bytes.len() > MAX_ASSET_BYTES {
        return Err(AssemblyError::Limit {
            name: "file bytes",
            max: MAX_ASSET_BYTES,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputKind {
    Module { specifier: String },
    Template,
    Static,
}

/// One explicitly declared, validated addon asset. It is never a filesystem path to open.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AssetInput {
    path: String,
    bytes: Vec<u8>,
    kind: InputKind,
}
impl AssetInput {
    /// Declare a prebuilt JavaScript module; its legal comments are retained byte-for-byte.
    /// # Errors
    /// Invalid path/specifier, non-UTF-8 or oversized source.
    pub fn module(
        path: &str,
        module_specifier: &str,
        bytes: Vec<u8>,
    ) -> Result<Self, AssemblyError> {
        static_path(path)?;
        specifier(module_specifier)?;
        bounded_file(&bytes)?;
        if !path.starts_with("static/dist/") || !path.ends_with(".js") {
            return Err(invalid("module path", path, "expected static/dist/**/*.js"));
        }
        std::str::from_utf8(&bytes)
            .map_err(|_| invalid("module", path, "expected UTF-8 JavaScript"))?;
        Ok(Self {
            path: path.to_owned(),
            bytes,
            kind: InputKind::Module {
                specifier: module_specifier.to_owned(),
            },
        })
    }
    /// Declare XML templates to combine with the complete addon set.
    /// # Errors
    /// Invalid source path or oversized XML. XML syntax/semantics are checked by assembly.
    pub fn template(path: &str, xml: &str) -> Result<Self, AssemblyError> {
        static_path(path)?;
        bounded_file(xml.as_bytes())?;
        if !path.starts_with("static/src/") || !path.ends_with(".xml") {
            return Err(invalid(
                "template path",
                path,
                "expected static/src/**/*.xml",
            ));
        }
        Ok(Self {
            path: path.to_owned(),
            bytes: xml.as_bytes().to_vec(),
            kind: InputKind::Template,
        })
    }
    /// Declare a fixed static file with a known content type. HTML and source maps are refused.
    /// # Errors
    /// Invalid path, unknown media type or oversized file.
    pub fn static_file(path: &str, bytes: Vec<u8>) -> Result<Self, AssemblyError> {
        static_path(path)?;
        bounded_file(&bytes)?;
        media(path)?;
        Ok(Self {
            path: path.to_owned(),
            bytes,
            kind: InputKind::Static,
        })
    }
    /// Declared relative path.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Asset declarations for one active addon, in its declared file order.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AddonAssets {
    name: String,
    version: String,
    runtime_abi: String,
    inputs: Vec<AssetInput>,
    input_bytes: usize,
    entry: Option<String>,
}
impl AddonAssets {
    /// Validate addon identity, addon version and its declared runtime ABI.
    /// # Errors
    /// Invalid or oversized identities.
    pub fn new(name: &str, version: &str, runtime_abi: &str) -> Result<Self, AssemblyError> {
        AddonName::new(name).map_err(|_| {
            invalid(
                "addon",
                name,
                "expected 1 to 256 ASCII letters, digits or underscores",
            )
        })?;
        token("version", version, 128)?;
        token("runtime ABI", runtime_abi, 128)?;
        Ok(Self {
            name: name.to_owned(),
            version: version.to_owned(),
            runtime_abi: runtime_abi.to_owned(),
            inputs: Vec::new(),
            input_bytes: 0,
            entry: None,
        })
    }
    /// Append one declared file; duplicate paths are an error even across asset kinds.
    /// # Errors
    /// Duplicate path or too many declarations.
    pub fn push(&mut self, input: AssetInput) -> Result<(), AssemblyError> {
        if self.inputs.len() >= MAX_FILES {
            return Err(AssemblyError::Limit {
                name: "files",
                max: MAX_FILES,
            });
        }
        if self.inputs.iter().any(|i| i.path == input.path) {
            return Err(AssemblyError::Duplicate {
                kind: "path",
                value: input.path,
            });
        }
        let input_bytes =
            self.input_bytes
                .checked_add(input.bytes.len())
                .ok_or(AssemblyError::Limit {
                    name: "input bytes",
                    max: MAX_INPUT_BYTES,
                })?;
        if input_bytes > MAX_INPUT_BYTES {
            return Err(AssemblyError::Limit {
                name: "input bytes",
                max: MAX_INPUT_BYTES,
            });
        }
        self.inputs.push(input);
        self.input_bytes = input_bytes;
        Ok(())
    }
    /// Declare the module exporting this addon's `register(ctx)` function.
    /// # Errors
    /// Invalid specifier or conflicting repeated entry declaration.
    pub fn set_entry(&mut self, value: &str) -> Result<(), AssemblyError> {
        specifier(value)?;
        if self.entry.is_some() {
            return Err(AssemblyError::Duplicate {
                kind: "entry",
                value: value.to_owned(),
            });
        }
        self.entry = Some(value.to_owned());
        Ok(())
    }
    /// Technical addon name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// Every non-source input affecting the deterministic key.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AssemblyOptions {
    compiler: String,
    runtime: String,
    language: String,
    group: String,
    compile: CompileOptions,
}
impl AssemblyOptions {
    /// Declare the running compiler/runtime identities, language and group.
    /// # Errors
    /// Invalid identities or a group other than `main`.
    pub fn new(
        compiler: &str,
        runtime: &str,
        language: &str,
        group: &str,
        compile: CompileOptions,
    ) -> Result<Self, AssemblyError> {
        token("compiler", compiler, 128)?;
        token("runtime", runtime, 128)?;
        token("language", language, 64)?;
        if group != "main" {
            return Err(AssemblyError::UnsupportedGroup {
                group: group.chars().take(256).collect(),
            });
        }
        Ok(Self {
            compiler: compiler.to_owned(),
            runtime: runtime.to_owned(),
            language: language.to_owned(),
            group: group.to_owned(),
            compile,
        })
    }
    /// Running runtime identity.
    pub fn runtime(&self) -> &str {
        &self.runtime
    }
    /// Running compiler identity.
    pub fn compiler(&self) -> &str {
        &self.compiler
    }
    /// Selected language identity (translation pass is UI3-DONE).
    pub fn language(&self) -> &str {
        &self.language
    }
    /// Lazy group (`main` in this slice).
    pub fn group(&self) -> &str {
        &self.group
    }
}

/// Verified identity bytes and encoded forms with this manifest artifact's declared media type.
/// Distinct media/extension artifacts may share the same raw content hash. Deserialization must go
/// through [`Blob::from_stored`], never directly into the validated type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Blob {
    sha256: String,
    bytes: Vec<u8>,
    gzip: Vec<u8>,
    brotli: Vec<u8>,
    media_type: String,
    extension: String,
}
impl Blob {
    fn new(bytes: Vec<u8>, mime: &str, extension: &str) -> Result<Self, AssemblyError> {
        bounded_file(&bytes)?;
        let mut encoder = flate2::GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
        encoder
            .write_all(&bytes)
            .map_err(AssemblyError::Compression)?;
        let gzip = encoder.finish().map_err(AssemblyError::Compression)?;
        let mut brotli = Vec::new();
        let params = brotli::enc::BrotliEncoderParams {
            quality: BROTLI_QUALITY,
            lgwin: BROTLI_WINDOW,
            ..Default::default()
        };
        brotli::BrotliCompress(&mut Cursor::new(&bytes), &mut brotli, &params)
            .map_err(AssemblyError::Compression)?;
        Ok(Self {
            sha256: hash(&bytes),
            bytes,
            gzip,
            brotli,
            media_type: mime.to_owned(),
            extension: extension.to_owned(),
        })
    }
    /// Re-admit a stored row, verifying its digest and exact bounded decoded representations.
    /// Valid encoding variants from older codec versions are retained without regeneration.
    /// # Errors
    /// Invalid media identity, oversized bytes, hash mismatch or modified compressed bytes.
    pub fn from_stored(
        sha256: String,
        bytes: Vec<u8>,
        gzip: Vec<u8>,
        brotli: Vec<u8>,
        media_type: String,
        extension: String,
    ) -> Result<Self, AssemblyError> {
        bounded_file(&bytes)?;
        if extension.len() > 8 || media_type.len() > 128 {
            return Err(corrupt("unsupported blob media type"));
        }
        let (expected_mime, expected_ext) = media(&format!("asset.{extension}"))
            .map_err(|_| corrupt("unsupported blob media type"))?;
        if expected_mime != media_type || expected_ext != extension {
            return Err(corrupt("blob media type mismatch"));
        }
        if !is_hash(&sha256) || hash(&bytes) != sha256 {
            return Err(corrupt("blob SHA-256 mismatch"));
        }
        if gzip.len() > MAX_ASSET_BYTES + 1024 * 1024
            || brotli.len() > MAX_ASSET_BYTES + 1024 * 1024
        {
            return Err(corrupt("encoded blob exceeds hard limit"));
        }
        verify_gzip(&gzip, &bytes)?;
        verify_brotli(&brotli, &bytes)?;
        Ok(Self {
            sha256,
            bytes,
            gzip,
            brotli,
            media_type,
            extension,
        })
    }
    /// Lowercase SHA-256 of identity bytes.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    /// Identity representation.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Deterministic gzip representation.
    pub fn gzip(&self) -> &[u8] {
        &self.gzip
    }
    /// Deterministic Brotli representation.
    pub fn brotli(&self) -> &[u8] {
        &self.brotli
    }
    /// Admitted content type.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }
    /// Admitted hashed URL suffix.
    pub fn extension(&self) -> &str {
        &self.extension
    }
    fn retype(&self, mime: &str, extension: &str) -> Result<Self, AssemblyError> {
        if extension.len() > 8 || mime.len() > 128 {
            return Err(corrupt("unsupported artifact media type"));
        }
        let (expected_mime, expected_extension) = media(&format!("artifact.{extension}"))
            .map_err(|_| corrupt("unsupported artifact media type"))?;
        if expected_mime != mime || expected_extension != extension {
            return Err(corrupt("artifact media type mismatch"));
        }
        let mut artifact = self.clone();
        artifact.media_type = mime.to_owned();
        artifact.extension = extension.to_owned();
        Ok(artifact)
    }
    /// Content-addressed native route.
    pub fn url(&self) -> String {
        format!("/erp/assets/{}.{}", self.sha256, self.extension)
    }
}

// Compare bounded decoder windows against the already bounded authority bytes; never allocate
// from an encoded stream's advertised output length or decode a bomb into a growable Vec.
fn decoded_window(identity: &[u8], offset: usize, window: &[u8]) -> Result<usize, AssemblyError> {
    let end = offset
        .checked_add(window.len())
        .ok_or_else(|| corrupt("blob compressed bytes mismatch"))?;
    if identity.get(offset..end) != Some(window) {
        return Err(corrupt("blob compressed bytes mismatch"));
    }
    Ok(end)
}
fn verify_gzip(encoded: &[u8], identity: &[u8]) -> Result<(), AssemblyError> {
    // BufRead preserves the exact unconsumed suffix of this one member, unlike read::GzDecoder.
    let mut decoder = flate2::bufread::GzDecoder::new(encoded);
    let mut output = [0u8; 8192];
    let mut offset = 0usize;
    loop {
        let produced = decoder
            .read(&mut output)
            .map_err(|_| corrupt("invalid gzip representation"))?;
        if produced == 0 {
            break;
        }
        let window = output
            .get(..produced)
            .ok_or_else(|| corrupt("invalid gzip output"))?;
        offset = decoded_window(identity, offset, window)?;
    }
    if offset != identity.len() {
        return Err(corrupt("blob compressed bytes mismatch"));
    }
    if !decoder.get_ref().is_empty() {
        return Err(corrupt("gzip trailing data"));
    }
    Ok(())
}
fn verify_brotli(encoded: &[u8], identity: &[u8]) -> Result<(), AssemblyError> {
    // RFC-sized windows only: the large-window extension is not admitted (up to 1 GiB state).
    // The strict decoder's maximum RFC window is 16 MiB, bounded independently of output length.
    let mut decoder = brotli::BrotliState::new_strict(
        brotli::HeapAlloc::<u8>::default(),
        brotli::HeapAlloc::<u32>::default(),
        brotli::HeapAlloc::<brotli::HuffmanCode>::default(),
    );
    let mut available_in = encoded.len();
    let mut input_offset = 0usize;
    let mut total_out = 0usize;
    let mut compared = 0usize;
    let mut output = [0u8; 8192];
    loop {
        let before_input = input_offset;
        let mut available_out = output.len();
        let mut output_offset = 0usize;
        let result = brotli::BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            encoded,
            &mut available_out,
            &mut output_offset,
            &mut output,
            &mut total_out,
            &mut decoder,
        );
        let window = output
            .get(..output_offset)
            .ok_or_else(|| corrupt("invalid Brotli output"))?;
        compared = decoded_window(identity, compared, window)?;
        match result {
            brotli::BrotliResult::ResultSuccess => {
                if compared != identity.len() {
                    return Err(corrupt("blob compressed bytes mismatch"));
                }
                if input_offset != encoded.len() || available_in != 0 {
                    return Err(corrupt("Brotli trailing data"));
                }
                return Ok(());
            }
            brotli::BrotliResult::NeedsMoreOutput => {
                if before_input == input_offset && output_offset == 0 {
                    return Err(corrupt("Brotli decoder made no progress"));
                }
            }
            brotli::BrotliResult::NeedsMoreInput | brotli::BrotliResult::ResultFailure => {
                return Err(corrupt("invalid Brotli representation"));
            }
        }
    }
}

/// A complete unpublished assembly result. Callers publish it in the install transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct AssembledBundle {
    schema: u8,
    key: String,
    compiler: String,
    runtime: String,
    language: String,
    group: String,
    mode: String,
    blobs: Vec<Blob>,
    imports: BTreeMap<String, String>,
    preloads: Vec<String>,
    register_module: Blob,
    static_files: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBlob {
    sha256: String,
    bytes: Vec<u8>,
    gzip: Vec<u8>,
    brotli: Vec<u8>,
    media_type: String,
    extension: String,
}
impl RawBlob {
    fn admit(self) -> Result<Blob, AssemblyError> {
        Blob::from_stored(
            self.sha256,
            self.bytes,
            self.gzip,
            self.brotli,
            self.media_type,
            self.extension,
        )
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBundle {
    schema: u8,
    key: String,
    compiler: String,
    runtime: String,
    language: String,
    group: String,
    mode: String,
    blobs: Vec<RawBlob>,
    imports: BTreeMap<String, String>,
    preloads: Vec<String>,
    register_module: RawBlob,
    static_files: BTreeMap<String, String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactRef {
    sha256: String,
    media_type: String,
    extension: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    schema: u8,
    key: String,
    compiler: String,
    runtime: String,
    language: String,
    group: String,
    mode: String,
    hashes: Vec<String>,
    artifacts: Vec<ArtifactRef>,
    imports: BTreeMap<String, String>,
    preloads: Vec<String>,
    register_url: String,
    static_files: BTreeMap<String, String>,
}
fn corrupt(reason: &'static str) -> AssemblyError {
    AssemblyError::CorruptBundle { reason }
}
impl AssembledBundle {
    /// Bundle input identity.
    pub fn key(&self) -> &str {
        &self.key
    }
    /// Compiler identity that produced this generation.
    pub fn compiler(&self) -> &str {
        &self.compiler
    }
    /// Runtime identity required on every serving node.
    pub fn runtime(&self) -> &str {
        &self.runtime
    }
    /// Language dimension of the key.
    pub fn language(&self) -> &str {
        &self.language
    }
    /// Lazy-group dimension of the key.
    pub fn group(&self) -> &str {
        &self.group
    }
    /// Build mode carried through durable metadata (`prod` or `dev`). Production hosts reject `dev`.
    pub fn mode(&self) -> &str {
        &self.mode
    }
    /// Artifact representations, including registration. Distinct media/extension artifacts can
    /// share raw hashes; durable content rows and GC reference sets deduplicate those hashes.
    pub fn blobs(&self) -> &[Blob] {
        &self.blobs
    }
    /// Bare module specifiers and hashed URLs; never source paths.
    pub fn imports(&self) -> &BTreeMap<String, String> {
        &self.imports
    }
    /// Deterministic module preload URLs.
    pub fn preloads(&self) -> &[String] {
        &self.preloads
    }
    /// ES module exporting ordered `registrations: [{ id, register }]` with the original addon
    /// functions. The consumer passes that array to Elang's `registerAddons` API, preserving its
    /// synchronous-return checks, per-addon context and patch ownership. Importing does not register.
    pub fn register_module(&self) -> &Blob {
        &self.register_module
    }
    /// Declared fixed routes mapped to admitted hashed artifacts.
    pub fn static_files(&self) -> &BTreeMap<String, String> {
        &self.static_files
    }
    /// Encode metadata only. Blob bytes belong in content-addressed rows.
    /// # Errors
    /// Serialization failure or oversized metadata.
    pub fn encode_manifest(&self) -> Result<Vec<u8>, AssemblyError> {
        let meta = Metadata {
            schema: self.schema,
            key: self.key.clone(),
            compiler: self.compiler.clone(),
            runtime: self.runtime.clone(),
            language: self.language.clone(),
            group: self.group.clone(),
            mode: self.mode.clone(),
            hashes: self
                .blobs
                .iter()
                .map(|b| b.sha256.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            artifacts: self
                .blobs
                .iter()
                .map(|b| ArtifactRef {
                    sha256: b.sha256.clone(),
                    media_type: b.media_type.clone(),
                    extension: b.extension.clone(),
                })
                .collect(),
            imports: self.imports.clone(),
            preloads: self.preloads.clone(),
            register_url: self.register_module.url(),
            static_files: self.static_files.clone(),
        };
        let bytes = serde_json::to_vec(&meta).map_err(AssemblyError::Json)?;
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(AssemblyError::Limit {
                name: "manifest bytes",
                max: MAX_METADATA_BYTES,
            });
        }
        Ok(bytes)
    }
    // Admission-only references for storage preflight. These are not a permission decision;
    // decode_manifest subsequently checks complete metadata against the supplied raw row set.
    pub(super) fn manifest_artifact_hashes(bytes: &[u8]) -> Result<Vec<String>, AssemblyError> {
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(corrupt("manifest exceeds hard limit"));
        }
        let meta: Metadata =
            serde_json::from_slice(bytes).map_err(|_| corrupt("invalid manifest JSON"))?;
        if meta.artifacts.len() > MAX_BLOBS {
            return Err(corrupt("too many manifest references"));
        }
        let mut hashes = Vec::with_capacity(meta.artifacts.len());
        for artifact in meta.artifacts {
            if !is_hash(&artifact.sha256) {
                return Err(corrupt("invalid artifact hash"));
            }
            hashes.push(artifact.sha256);
        }
        Ok(hashes)
    }
    /// Rehydrate per-manifest artifact media from independently admitted tenant-local raw rows.
    /// Supply exactly one verified row per unique raw hash, in any order. A row's initial media
    /// descriptor is not authority for another manifest's artifact referencing the same bytes.
    /// # Errors
    /// Malformed/oversized metadata, duplicate/missing/extra raw rows, or invalid artifact references.
    pub fn decode_manifest(bytes: &[u8], raw_rows: Vec<Blob>) -> Result<Self, AssemblyError> {
        if bytes.len() > MAX_METADATA_BYTES {
            return Err(corrupt("manifest exceeds hard limit"));
        }
        let meta: Metadata =
            serde_json::from_slice(bytes).map_err(|_| corrupt("invalid manifest JSON"))?;
        if meta.hashes.len() > MAX_BLOBS
            || raw_rows.len() > MAX_BLOBS
            || meta.artifacts.len() > MAX_BLOBS
        {
            return Err(corrupt("too many blobs"));
        }
        if meta.hashes.len() != raw_rows.len() {
            return Err(corrupt("missing referenced blob"));
        }
        let mut expected_hashes = BTreeSet::new();
        for hash in &meta.hashes {
            if !is_hash(hash) {
                return Err(corrupt("invalid referenced hash"));
            }
            if !expected_hashes.insert(hash.as_str()) {
                return Err(corrupt("duplicate raw hash reference"));
            }
        }
        let mut supplied = BTreeMap::new();
        for row in raw_rows {
            if !expected_hashes.contains(row.sha256()) {
                return Err(corrupt("unreferenced raw content row"));
            }
            if supplied.insert(row.sha256.clone(), row).is_some() {
                return Err(corrupt("duplicate raw content row"));
            }
        }
        let mut blobs = Vec::with_capacity(meta.artifacts.len());
        let mut used = BTreeSet::new();
        let mut total = 0usize;
        for reference in &meta.artifacts {
            if !is_hash(&reference.sha256) {
                return Err(corrupt("invalid artifact hash"));
            }
            let row = supplied
                .get(&reference.sha256)
                .ok_or_else(|| corrupt("missing referenced blob"))?;
            // Charge the complete representation before cloning it for this artifact's metadata.
            total = charge(total, row.bytes.len())?;
            total = charge(total, row.gzip.len())?;
            total = charge(total, row.brotli.len())?;
            blobs.push(row.retype(&reference.media_type, &reference.extension)?);
            used.insert(reference.sha256.as_str());
        }
        if used != expected_hashes {
            return Err(corrupt("unreferenced raw hash in manifest"));
        }
        let register_module = blobs
            .iter()
            .find(|b| b.url() == meta.register_url)
            .ok_or_else(|| corrupt("missing registration blob"))?
            .clone();
        let bundle = Self {
            schema: meta.schema,
            key: meta.key,
            compiler: meta.compiler,
            runtime: meta.runtime,
            language: meta.language,
            group: meta.group,
            mode: meta.mode,
            blobs,
            imports: meta.imports,
            preloads: meta.preloads,
            register_module,
            static_files: meta.static_files,
        };
        bundle.validate()?;
        Ok(bundle)
    }
    /// Admit a full encoded result used by offline tools. Database publication uses metadata only.
    /// # Errors
    /// Invalid JSON, oversized data, corrupt blobs or inconsistent references.
    pub fn decode(bytes: &[u8]) -> Result<Self, AssemblyError> {
        if bytes.len() > MAX_SERIALIZED_BYTES {
            return Err(corrupt("serialized bundle exceeds hard limit"));
        }
        let raw: RawBundle =
            serde_json::from_slice(bytes).map_err(|_| corrupt("invalid bundle JSON"))?;
        if raw.blobs.len() > MAX_BLOBS {
            return Err(corrupt("too many blobs"));
        }
        let blobs = raw
            .blobs
            .into_iter()
            .map(RawBlob::admit)
            .collect::<Result<Vec<_>, _>>()?;
        let bundle = Self {
            schema: raw.schema,
            key: raw.key,
            compiler: raw.compiler,
            runtime: raw.runtime,
            language: raw.language,
            group: raw.group,
            mode: raw.mode,
            blobs,
            imports: raw.imports,
            preloads: raw.preloads,
            register_module: raw.register_module.admit()?,
            static_files: raw.static_files,
        };
        bundle.validate()?;
        Ok(bundle)
    }
    fn validate(&self) -> Result<(), AssemblyError> {
        if self.schema != 1 || !is_hash(&self.key) {
            return Err(corrupt("invalid manifest identity"));
        }
        token("compiler", &self.compiler, 128)?;
        token("runtime", &self.runtime, 128)?;
        token("language", &self.language, 64)?;
        if self.group != "main" {
            return Err(corrupt("unsupported manifest group"));
        }
        if !matches!(self.mode.as_str(), "prod" | "dev") {
            return Err(corrupt("unsupported manifest build mode"));
        }
        if self.blobs.len() > MAX_BLOBS
            || self.imports.len() > MAX_BLOBS
            || self.static_files.len() > MAX_FILES
            || self.preloads.len() > MAX_BLOBS
        {
            return Err(corrupt("too many manifest references"));
        }
        let mut urls = BTreeSet::new();
        let mut contents = BTreeMap::new();
        let mut total = 0usize;
        for blob in &self.blobs {
            total = charge(total, blob.bytes.len())?;
            total = charge(total, blob.gzip.len())?;
            total = charge(total, blob.brotli.len())?;
            if let Some(previous) = contents.insert(blob.sha256(), blob.bytes())
                && previous != blob.bytes()
            {
                return Err(corrupt("one raw hash has different identity bytes"));
            }
            if !urls.insert(blob.url()) {
                return Err(corrupt("duplicate artifact URL"));
            }
        }
        if !self.blobs.iter().any(|b| b == &self.register_module) {
            return Err(corrupt("missing registration blob"));
        }
        if self.register_module.extension() != "js"
            || self.register_module.media_type() != "text/javascript; charset=utf-8"
        {
            return Err(corrupt("registration blob must be JavaScript"));
        }
        for (name, url) in &self.imports {
            // Template names additionally permit ':'; their mapping still points only at a hash URL.
            if specifier(name).is_err() && compiler::TemplateName::new(name).is_err() {
                return Err(corrupt("invalid import specifier"));
            }
            if !urls.contains(url) {
                return Err(corrupt("missing import blob"));
            }
        }
        if self.preloads.iter().any(|u| !urls.contains(u)) {
            return Err(corrupt("missing preload blob"));
        }
        for (path, url) in &self.static_files {
            let (addon, rest) = path
                .strip_prefix('/')
                .and_then(|p| p.split_once('/'))
                .ok_or_else(|| corrupt("invalid static route"))?;
            AddonName::new(addon).map_err(|_| corrupt("invalid static route"))?;
            static_path(rest).map_err(|_| corrupt("invalid static route"))?;
            if !urls.contains(url) {
                return Err(corrupt("missing static blob"));
            }
        }
        Ok(())
    }
}
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn is_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn charge(current: usize, bytes: usize) -> Result<usize, AssemblyError> {
    let total = current.checked_add(bytes).ok_or(AssemblyError::Limit {
        name: "bundle bytes",
        max: MAX_BUNDLE_BYTES,
    })?;
    if total > MAX_BUNDLE_BYTES {
        return Err(AssemblyError::Limit {
            name: "bundle bytes",
            max: MAX_BUNDLE_BYTES,
        });
    }
    Ok(total)
}
struct Key(Sha256);
impl Key {
    fn field(&mut self, value: &[u8]) -> Result<(), AssemblyError> {
        let length = u32::try_from(value.len()).map_err(|_| AssemblyError::Limit {
            name: "key field bytes",
            max: u32::MAX as usize,
        })?;
        self.0.update(length.to_be_bytes());
        self.0.update(value);
        Ok(())
    }
    fn finish(self) -> String {
        self.0
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
struct BlobIndex {
    by_hash: BTreeMap<String, usize>,
    by_url: BTreeMap<String, usize>,
    byte_limit: usize,
    retained_bytes: usize,
}
impl Default for BlobIndex {
    fn default() -> Self {
        Self {
            by_hash: BTreeMap::new(),
            by_url: BTreeMap::new(),
            byte_limit: MAX_BUNDLE_BYTES,
            retained_bytes: 0,
        }
    }
}
impl BlobIndex {
    #[cfg(test)]
    fn with_byte_limit(limit: usize) -> Result<Self, AssemblyError> {
        if limit == 0 || limit > MAX_BUNDLE_BYTES {
            return Err(AssemblyError::Limit {
                name: "bundle bytes",
                max: MAX_BUNDLE_BYTES,
            });
        }
        Ok(Self {
            byte_limit: limit,
            ..Self::default()
        })
    }
}
fn add_blob(
    blobs: &mut Vec<Blob>,
    indexes: &mut BlobIndex,
    blob: Blob,
) -> Result<String, AssemblyError> {
    let url = blob.url();
    if let Some(index) = indexes.by_hash.get(blob.sha256()) {
        let previous = blobs
            .get(*index)
            .ok_or_else(|| corrupt("internal content index"))?;
        if previous.bytes() != blob.bytes() {
            return Err(AssemblyError::ContentConflict {
                sha256: blob.sha256.clone(),
            });
        }
    }
    if let Some(index) = indexes.by_url.get(&url) {
        let previous = blobs
            .get(*index)
            .ok_or_else(|| corrupt("internal artifact index"))?;
        if previous.media_type() != blob.media_type() {
            return Err(corrupt("artifact URL has conflicting media"));
        }
        return Ok(url);
    }
    if blobs.len() >= MAX_BLOBS {
        return Err(AssemblyError::Limit {
            name: "blobs",
            max: MAX_BLOBS,
        });
    }
    // A refused candidate never enters either index or the retained vector. Encoding this one
    // candidate is bounded by MAX_ASSET_BYTES; all previously retained representations are charged.
    let mut retained = indexes.retained_bytes;
    for bytes in [blob.bytes.len(), blob.gzip.len(), blob.brotli.len()] {
        retained = retained.checked_add(bytes).ok_or(AssemblyError::Limit {
            name: "bundle bytes",
            max: indexes.byte_limit,
        })?;
        if retained > indexes.byte_limit {
            return Err(AssemblyError::Limit {
                name: "bundle bytes",
                max: indexes.byte_limit,
            });
        }
    }
    indexes.retained_bytes = retained;
    indexes
        .by_hash
        .entry(blob.sha256.clone())
        .or_insert(blobs.len());
    indexes.by_url.insert(url.clone(), blobs.len());
    blobs.push(blob);
    Ok(url)
}
fn insert_import(
    imports: &mut BTreeMap<String, String>,
    name: &str,
    url: String,
) -> Result<(), AssemblyError> {
    if imports.contains_key(name) {
        return Err(AssemblyError::Duplicate {
            kind: "specifier",
            value: name.to_owned(),
        });
    }
    imports.insert(name.to_owned(), url);
    Ok(())
}

// Only compiler-generated modules and our registration module reach this helper. Their source
// nesting/size follows compiler admission; arbitrary dist JavaScript remains prebuilt input.
fn minify_generated_module(source: &str, module: &str) -> Result<String, AssemblyError> {
    bounded_file(source.as_bytes())?;
    let allocator = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, source, oxc_span::SourceType::mjs()).parse();
    if !parsed.diagnostics.is_empty() {
        let reason = parsed
            .diagnostics
            .iter()
            .take(8)
            .map(|d| d.to_string().chars().take(1024).collect::<String>())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(AssemblyError::GeneratedModule {
            module: module.to_owned(),
            reason,
        });
    }
    let mut program = parsed.program;
    let mut notices = Vec::new();
    // Keep legal text even when a compression transform removes the node to which it was
    // attached. Spans come from the parser's comment list, never from scanning string literals.
    for comment in &program.comments {
        let text = source
            .get(comment.span.start as usize..comment.span.end as usize)
            .ok_or_else(|| AssemblyError::GeneratedModule {
                module: module.to_owned(),
                reason: "invalid parsed comment span".to_owned(),
            })?;
        if text.starts_with("/*!")
            || text.starts_with("//!")
            || text.contains("@license")
            || text.contains("@preserve")
        {
            notices.push(text);
        }
    }
    let mut compression = oxc_minifier::CompressOptions::safest();
    compression.max_iterations = Some(2);
    compression.treeshake.annotations = false;
    compression.treeshake.invalid_import_side_effects = true;
    let result = oxc_minifier::Minifier::new(oxc_minifier::MinifierOptions {
        mangle: None,
        mangle_properties: None,
        compress: Some(compression),
    })
    .minify(&allocator, &mut program);
    let mut output = oxc_codegen::Codegen::new()
        .with_options(oxc_codegen::CodegenOptions {
            minify: true,
            comments: oxc_codegen::CommentOptions {
                normal: false,
                jsdoc: false,
                annotation: false,
                legal: oxc_codegen::LegalComment::Inline,
            },
            source_map_path: None,
            ..oxc_codegen::CodegenOptions::default()
        })
        .with_scoping(result.scoping)
        .build(&program)
        .code;
    bounded_file(output.as_bytes())?;
    for notice in notices {
        if !output.contains(notice) {
            let length = output
                .len()
                .checked_add(notice.len())
                .and_then(|v| v.checked_add(2))
                .ok_or(AssemblyError::Limit {
                    name: "file bytes",
                    max: MAX_ASSET_BYTES,
                })?;
            if length > MAX_ASSET_BYTES {
                return Err(AssemblyError::Limit {
                    name: "file bytes",
                    max: MAX_ASSET_BYTES,
                });
            }
            output.push('\n');
            output.push_str(notice);
            output.push('\n');
        }
    }
    Ok(output)
}

/// Combine and compile all declared templates, then emit one complete unpublished bundle.
/// Nothing is returned on any input, ABI, compiler or encoding error. A request must never call it.
///
/// ```
/// use erp_addon_web::assets::assembly::{AddonAssets, AssemblyOptions, AssetInput, assemble};
/// use erp_sdk::ui_compiler::{BuildMode, CompileOptions};
/// let mut addon = AddonAssets::new("web", "1.0", "runtime-1")?;
/// addon.push(AssetInput::module("static/dist/main.js", "@erpforall/web",
///     b"export function register(ctx) {}".to_vec())?)?;
/// addon.set_entry("@erpforall/web")?;
/// let options = AssemblyOptions::new("compiler-1", "runtime-1", "en_US", "main",
///     CompileOptions::new(BuildMode::Prod))?;
/// let bundle = assemble(&[addon], &options)?;
/// assert_eq!(bundle.runtime(), "runtime-1");
/// assert!(bundle.imports().contains_key("@erpforall/web"));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
/// Any invalid declaration, duplicate, admission limit, incompatible ABI, compiler or encoding failure.
pub fn assemble(
    addons: &[AddonAssets],
    options: &AssemblyOptions,
) -> Result<AssembledBundle, AssemblyError> {
    record_assembly_call();
    if addons.len() > MAX_ADDONS {
        return Err(AssemblyError::Limit {
            name: "addons",
            max: MAX_ADDONS,
        });
    }
    let mut key = Key(Sha256::new());
    for field in [
        "elang-bundle-v1",
        options.compiler(),
        options.runtime(),
        options.language(),
        options.group(),
    ] {
        key.field(field.as_bytes())?;
    }
    // Encoding settings affect emitted representation bytes even when source hashes do not.
    // Include them as length-prefixed key inputs so a codec-setting rollout never reuses the
    // earlier assembly identity. Existing durable hash rows may retain verified older forms.
    key.field(b"encoded-representations-v1")?;
    key.field(&GZIP_LEVEL.to_be_bytes())?;
    key.field(&BROTLI_QUALITY.to_be_bytes())?;
    key.field(&BROTLI_WINDOW.to_be_bytes())?;
    let mode = match options.compile.mode() {
        BuildMode::Prod => "prod",
        BuildMode::Dev => "dev",
        _ => {
            return Err(invalid(
                "build mode",
                "unknown",
                "unsupported compiler build mode",
            ));
        }
    };
    key.field(mode.as_bytes())?;
    let limits: BTreeMap<&str, usize> = LimitKind::ALL
        .iter()
        .map(|&kind| (kind.name(), options.compile.limits().get(kind)))
        .collect();
    key.field(&serde_json::to_vec(&limits).map_err(AssemblyError::Json)?)?;
    let directives: Vec<&str> = options
        .compile
        .custom_directives()
        .iter()
        .map(|d| d.as_str())
        .collect();
    key.field(&serde_json::to_vec(&directives).map_err(AssemblyError::Json)?)?;
    key.field(
        &u32::try_from(addons.len())
            .map_err(|_| AssemblyError::Limit {
                name: "addons",
                max: MAX_ADDONS,
            })?
            .to_be_bytes(),
    )?;
    let mut names = BTreeSet::new();
    let mut files = 0usize;
    let mut input_bytes = 0usize;
    let mut set = TemplateSet::new();
    let mut blobs = Vec::new();
    let mut indexes = BlobIndex::default();
    let mut imports = BTreeMap::new();
    let mut static_files = BTreeMap::new();
    let mut entries = Vec::new();
    for addon in addons {
        if !names.insert(addon.name()) {
            return Err(AssemblyError::Duplicate {
                kind: "addon",
                value: addon.name.clone(),
            });
        }
        if addon.runtime_abi != options.runtime {
            return Err(AssemblyError::RuntimeAbi {
                addon: addon.name.clone(),
                expected: options.runtime.clone(),
                actual: addon.runtime_abi.clone(),
            });
        }
        key.field(addon.name.as_bytes())?;
        key.field(addon.version.as_bytes())?;
        key.field(addon.runtime_abi.as_bytes())?;
        key.field(addon.entry.as_deref().unwrap_or("").as_bytes())?; // No entry means this addon has no register export, part of the input contract.
        key.field(
            &u32::try_from(addon.inputs.len())
                .map_err(|_| AssemblyError::Limit {
                    name: "files",
                    max: MAX_FILES,
                })?
                .to_be_bytes(),
        )?;
        for input in &addon.inputs {
            files = files.checked_add(1).ok_or(AssemblyError::Limit {
                name: "files",
                max: MAX_FILES,
            })?;
            if files > MAX_FILES {
                return Err(AssemblyError::Limit {
                    name: "files",
                    max: MAX_FILES,
                });
            }
            input_bytes = charge(input_bytes, input.bytes.len())?;
            if input_bytes > MAX_INPUT_BYTES {
                return Err(AssemblyError::Limit {
                    name: "input bytes",
                    max: MAX_INPUT_BYTES,
                });
            }
            key.field(input.path.as_bytes())?;
            key.field(hash(&input.bytes).as_bytes())?;
            match &input.kind {
                InputKind::Template => {
                    key.field(b"template")?;
                    let xml = std::str::from_utf8(&input.bytes)
                        .map_err(|_| invalid("template", &input.path, "expected UTF-8 XML"))?;
                    let name = AddonName::new(&addon.name)
                        .map_err(|_| invalid("addon", &addon.name, "invalid technical name"))?;
                    let path = TemplatePath::new(&input.path).map_err(|_| {
                        invalid("path", &input.path, "invalid compiler source path")
                    })?;
                    set.push(TemplateSource::addon_file(name, path, xml));
                }
                InputKind::Module { specifier } => {
                    key.field(b"module")?;
                    key.field(specifier.as_bytes())?;
                    if mode == "prod"
                        && input
                            .bytes
                            .windows(b"sourceMappingURL".len())
                            .any(|w| w == b"sourceMappingURL")
                    {
                        return Err(AssemblyError::ProductionSourceMap {
                            addon: addon.name.clone(),
                            path: input.path.clone(),
                        });
                    }
                    let url = add_blob(
                        &mut blobs,
                        &mut indexes,
                        Blob::new(input.bytes.clone(), "text/javascript; charset=utf-8", "js")?,
                    )?;
                    insert_import(&mut imports, specifier, url)?;
                }
                InputKind::Static => {
                    key.field(b"static")?;
                    let (mime, ext) = media(&input.path)?;
                    let url = add_blob(
                        &mut blobs,
                        &mut indexes,
                        Blob::new(input.bytes.clone(), mime, ext)?,
                    )?;
                    static_files.insert(format!("/{}/{}", addon.name, input.path), url);
                }
            }
        }
        if let Some(entry) = &addon.entry {
            if !addon
                .inputs
                .iter()
                .any(|i| matches!(&i.kind, InputKind::Module { specifier } if specifier == entry))
            {
                return Err(AssemblyError::MissingEntry {
                    addon: addon.name.clone(),
                    specifier: entry.clone(),
                });
            }
            entries.push((addon.name.as_str(), entry.as_str()));
        }
    }
    let compiled = compiler::compile(&set, &options.compile)
        .map_err(|source| AssemblyError::Compile { source })?;
    if !compiled.modules().is_empty() && !imports.contains_key(compiler::ABI_MODULE) {
        return Err(AssemblyError::MissingRuntime {
            specifier: compiler::ABI_MODULE.to_owned(),
        });
    }
    for module in compiled.modules() {
        let code = if mode == "prod" {
            minify_generated_module(module.code(), module.name().as_str())?
        } else {
            module.code().to_owned()
        };
        let url = add_blob(
            &mut blobs,
            &mut indexes,
            Blob::new(code.into_bytes(), "text/javascript; charset=utf-8", "js")?,
        )?;
        insert_import(&mut imports, module.name().as_str(), url)?;
    }
    let mut register = String::new();
    for (index, (_, entry)) in entries.iter().enumerate() {
        let quoted = serde_json::to_string(entry).map_err(AssemblyError::Json)?;
        register.push_str(&format!(
            "import {{ register as r{index} }} from {quoted};\n"
        ));
    }
    register.push_str("export const registrations = [\n");
    for (index, (addon, _)) in entries.iter().enumerate() {
        let quoted = serde_json::to_string(addon).map_err(AssemblyError::Json)?;
        register.push_str(&format!("{{ id: {quoted}, register: r{index} }},\n"));
    }
    register.push_str("];\n");
    let register = if mode == "prod" {
        minify_generated_module(&register, "registration")?
    } else {
        register
    };
    let register_module = Blob::new(
        register.into_bytes(),
        "text/javascript; charset=utf-8",
        "js",
    )?;
    add_blob(&mut blobs, &mut indexes, register_module.clone())?;
    let mut preload_urls: BTreeSet<_> = imports.values().cloned().collect();
    preload_urls.insert(register_module.url());
    let preloads = preload_urls.into_iter().collect();
    let bundle = AssembledBundle {
        schema: 1,
        key: key.finish(),
        compiler: options.compiler.clone(),
        runtime: options.runtime.clone(),
        language: options.language.clone(),
        group: options.group.clone(),
        mode: mode.to_owned(),
        blobs,
        imports,
        preloads,
        register_module,
        static_files,
    };
    bundle.validate()?;
    Ok(bundle)
}

#[cfg(test)]
mod minification_tests {
    #[test]
    fn generated_minification_strips_ordinary_comments_and_retains_all_legal_notices() {
        let source = "/*! Generated license one */\n// ordinary generated comment\nexport function register(ctx) { /*! Generated license two */ ctx.value = 1 + 1; }\n/*! Generated license three */";
        let output = super::minify_generated_module(source, "fixture.generated").unwrap();
        assert!(!output.contains("ordinary generated comment"), "{output}");
        assert!(output.contains("ctx.value=2"), "{output}");
        for notice in [
            "/*! Generated license one */",
            "/*! Generated license two */",
            "/*! Generated license three */",
        ] {
            assert!(output.contains(notice), "missing {notice}: {output}");
        }
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn representation_budget_refuses_before_retaining_or_mutating_the_index() {
        let first = Blob::new(b"first".to_vec(), "text/css; charset=utf-8", "css").unwrap();
        let second = Blob::new(
            b"second larger input".repeat(128),
            "text/css; charset=utf-8",
            "css",
        )
        .unwrap();
        let cost = |b: &Blob| b.bytes().len() + b.gzip().len() + b.brotli().len();
        let cap = cost(&first) + cost(&second) - 1;
        let mut index = BlobIndex::with_byte_limit(cap).unwrap();
        let mut artifacts = Vec::new();
        let first_url = add_blob(&mut artifacts, &mut index, first.clone()).unwrap();
        let error = add_blob(&mut artifacts, &mut index, second).unwrap_err();
        assert!(
            matches!(&error, AssemblyError::Limit { name, max } if *name == "bundle bytes" && *max == cap)
        );
        assert_eq!(
            error.to_string(),
            format!("UI asset bundle bytes exceeds hard limit {cap}")
        );
        assert_eq!(artifacts.as_slice(), std::slice::from_ref(&first));
        assert_eq!(
            add_blob(&mut artifacts, &mut index, first).unwrap(),
            first_url
        );
        let small = Blob::new(Vec::new(), "text/plain; charset=utf-8", "txt").unwrap();
        add_blob(&mut artifacts, &mut index, small.clone()).unwrap();
        assert_eq!(artifacts.last(), Some(&small));
        assert_eq!(artifacts.len(), 2);
    }
}
