//! The framed request of `erp-ui-compile` (design note §11) and the function both the native
//! binary and its `wasm32-wasip1` build run.
//!
//! # Framing
//!
//! A request is the magic `ERPUI2\n` followed by records, each a `u32` big-endian length and that
//! many bytes of UTF-8 (the cache-key encoding). A record is a tag, one space and a
//! value. The records, in this order:
//!
//! | Tag | Count | Value |
//! |---|---|---|
//! | `op` | 1 | `analyze`, `extract` or `compile` |
//! | `mode` | 1 | `prod` or `dev` |
//! | `language` | 0..1 | an immutable translation catalogue for compilation |
//! | `term` | 0..n | addon (empty for global), followed by `original` and `translated`, sorted by addon and original |
//! | `directive` | 0..256 | a custom directive name ([`DirectiveName`]), each name once ([`erp_ui_compiler::MAX_CUSTOM_DIRECTIVES`]) |
//! | `limit` | 0..n | `name=value`: a lowered compiler limit ([`LimitKind::name`]), each limit once; the value is decimal without leading zeros, `1..=default` |
//! | `file` | 0..n | the source kind of one file: `addon_file`, followed by its `addon`, `path` and `source` records |
//!
//! Anything else (an unknown tag, a record out of order, such as a `directive` after a `limit`, a
//! repeated directive or limit, too many directives, a non-canonical number) is a malformed
//! request: decoding never drops or rewrites a record, so a
//! decoded request encodes back to the same bytes. New inputs (the restricted source kind of,
//! declared contexts) are new tags or `file` kinds, not a new magic.
//!
//! # Arch requests
//!
//! A database-stored arch travels as an [`ArchCompileRequest`] (ADR 0118 D1): the same magic and
//! record encoding, with `arch_id` as the third record and one `file arch_data`. The trusted
//! decoder ([`Request::decode`]) refuses an `arch_id` record or an `arch_data` file wherever it
//! appears. Native processing refuses database templates. The WASI guest checks the request
//! and produces a bounded, data-only card plan.
//!
//! The response is the canonical byte form of the result. The binary exits [`EXIT_CLEAN`] when the
//! set has no error, [`EXIT_ERRORS`] when the result carries errors (a rejected template, a set
//! error, a failed compile; stdout still holds the result) and [`EXIT_MALFORMED`] for a malformed
//! or oversized request or an I/O error (nothing on stdout).
//!
//! # Running untrusted input
//!
//! The compiler bounds its own work (design note §9): input is refused over [`MAX_REQUEST_BYTES`]
//! and over the set limits before anything is parsed, and every later phase is bounded by a
//! [`LimitKind`]. It still runs in the host's process, and the expression parser uses `unsafe`
//! internally, so templates that are not trusted addon files (the database-stored templates of
//! ) are compiled with the WASM build only, and the host runs it with:
//!
//! - a linear-memory cap: [`WASM_MEMORY_CAP_BYTES`] (1 GiB). The 422-template `web` corpus peaks at
//!   13.6 MB of linear memory (8 MiB of it the stack), so the cap leaves room for requests far
//!   larger, and an oversized one stops at the cap instead of growing without bound;
//! - a fuel budget or an epoch deadline, so a request cannot run unboundedly long whatever the
//!   limits miss: [`wasm_fuel_budget`] of the request size in a fuel-metering runtime (wasmi,
//!   wasmtime with `consume_fuel`), or the matching wall-clock deadline with epochs. The compiler is
//!   linear in its input, so the budget is too: the `web` corpus compiles with about 2 600 fuel
//!   units per request byte, the densest measured input (20 000 sibling elements) with about
//!   11 400, and the budget allows [`WASM_FUEL_PER_BYTE`] plus a fixed [`WASM_FUEL_BASE`]. The identity test runs every request (corpus, fixtures, inputs at the
//!   limits) under this budget and the memory cap.
//!
//! ```
//! use erp_ui_compiler::{AddonName, BuildMode, TemplatePath};
//! use erp_ui_compiler_cli::{EXIT_CLEAN, Operation, Request, respond};
//!
//! let request = Request::new(Operation::Analyze, BuildMode::Prod).with_addon_file(
//!     AddonName::new("web")?,
//!     TemplatePath::new("a.xml")?,
//!     "<templates><t t-name=\"web.A\"><b/></t></templates>",
//! );
//! let bytes = request.encode()?;
//! let response = respond(&bytes)?;
//! assert_eq!(response.exit_status(), 0);
//! assert!(String::from_utf8(response.into_bytes())?.contains("templates 1 passed 1 rejected 0"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, DirectiveName, LimitKind, Origin, TemplatePath,
    TemplateSet, TemplateSource,
};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::io::Read;

/// The request magic.
pub const MAGIC: &[u8] = b"ERPUI2\n";

/// The largest request the binary reads: the input-bytes limit plus room for the framing of the
/// largest file count (four records per file, at most 1024-byte paths and 256-byte addon names).
pub const MAX_REQUEST_BYTES: usize = LimitKind::InputBytes.default_value() + 32 * 1024 * 1024;

/// The linear-memory cap a host gives the WASM build (see "Running untrusted input").
pub const WASM_MEMORY_CAP_BYTES: u64 = 1024 * 1024 * 1024;

/// The fixed part of [`wasm_fuel_budget`] (start-up and the smallest sets; the fixture set
/// compiles with 45 million units).
pub const WASM_FUEL_BASE: u64 = 100_000_000;

/// The per-byte part of [`wasm_fuel_budget`]: 3.5 times the densest measured rate (20 000
/// sibling elements, about 11 400 units per byte), 15 times the `web` corpus' rate.
pub const WASM_FUEL_PER_BYTE: u64 = 40_000;

/// The fuel budget a fuel-metering host gives one request of `request_bytes` bytes to the WASM
/// build (see "Running untrusted input").
#[must_use]
pub const fn wasm_fuel_budget(request_bytes: usize) -> u64 {
    // usize is at most 64 bits on every supported target.
    let bytes = request_bytes as u64;
    WASM_FUEL_BASE.saturating_add(WASM_FUEL_PER_BYTE.saturating_mul(bytes))
}

/// Exit status: the result has no error.
pub const EXIT_CLEAN: u8 = 0;
/// Exit status: the result carries errors (stdout holds it).
pub const EXIT_ERRORS: u8 = 1;
/// Exit status: malformed or oversized request, or an I/O error (nothing on stdout).
pub const EXIT_MALFORMED: u8 = 2;

/// What to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Operation {
    /// [`erp_ui_compiler::analyze`].
    Analyze,
    /// Extract source strings and their original locations as a JSON catalogue.
    Extract,
    /// [`erp_ui_compiler::compile`].
    Compile,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Operation::Analyze => "analyze",
            Operation::Compile => "compile",
            Operation::Extract => "extract",
        }
    }
}

/// A request: the operation, the compile options and the files, in set order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    operation: Operation,
    catalogue: Option<erp_ui_compiler::TranslationCatalogue>,
    /// The options the records build (mode, directives, limits).
    options: CompileOptions,
    /// The directive records, in request order (the options keep them sorted).
    custom_directives: Vec<DirectiveName>,
    /// The same names, for the duplicate check.
    directive_set: BTreeSet<DirectiveName>,
    /// The limit records, in request order.
    limits: Vec<(LimitKind, usize)>,
    files: Vec<TemplateSource>,
}

/// A malformed request, or one this framing cannot carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestError {
    message: String,
}

impl RequestError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// What is wrong.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid request: {}", self.message)
    }
}

impl std::error::Error for RequestError {}

impl Request {
    /// A request with no custom directive, the default limits and no file.
    #[must_use]
    pub fn new(operation: Operation, mode: BuildMode) -> Self {
        Self {
            operation,
            catalogue: None,
            options: CompileOptions::new(mode),
            custom_directives: Vec::new(),
            directive_set: BTreeSet::new(),
            limits: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Adds a custom directive name.
    ///
    /// # Errors
    /// [`RequestError`] when the name is already in the request, or the request already holds
    /// [`erp_ui_compiler::MAX_CUSTOM_DIRECTIVES`] names.
    pub fn with_custom_directive(mut self, name: DirectiveName) -> Result<Self, RequestError> {
        if self.directive_set.contains(&name) {
            return Err(RequestError::new(format!(
                "custom directive '{name}' is given twice"
            )));
        }
        self.options = self
            .options
            .with_custom_directive(name.as_str())
            .map_err(|e| RequestError::new(e.to_string()))?;
        self.directive_set.insert(name.clone());
        self.custom_directives.push(name);
        Ok(self)
    }

    /// Lowers one compiler limit.
    ///
    /// # Errors
    /// [`RequestError`] when the limit is already in the request, or `value` is zero or above its
    /// default.
    pub fn with_limit(mut self, kind: LimitKind, value: usize) -> Result<Self, RequestError> {
        if self.limits.iter().any(|(k, _)| *k == kind) {
            return Err(RequestError::new(format!("limit {kind} is given twice")));
        }
        let limits = self
            .options
            .limits()
            .lowered(kind, value)
            .map_err(|e| RequestError::new(e.to_string()))?;
        self.options = self.options.with_limits(limits);
        self.limits.push((kind, value));
        Ok(self)
    }

    /// Adds an addon template file.
    #[must_use]
    pub fn with_addon_file(
        mut self,
        addon: AddonName,
        path: TemplatePath,
        xml: impl Into<Box<str>>,
    ) -> Self {
        self.files
            .push(TemplateSource::addon_file(addon, path, xml));
        self
    }

    /// The operation.
    #[must_use]
    pub fn operation(&self) -> Operation {
        self.operation
    }

    /// The build mode.
    #[must_use]
    pub fn mode(&self) -> BuildMode {
        self.options.mode()
    }

    /// The custom directive names, in request order.
    #[must_use]
    pub fn custom_directives(&self) -> &[DirectiveName] {
        &self.custom_directives
    }

    /// The lowered limits, in request order.
    #[must_use]
    pub fn limits(&self) -> &[(LimitKind, usize)] {
        &self.limits
    }

    /// The files, in set order.
    #[must_use]
    pub fn files(&self) -> &[TemplateSource] {
        &self.files
    }

    /// The compile options of this request.
    #[must_use]
    pub fn options(&self) -> &CompileOptions {
        &self.options
    }

    /// The framed bytes of this request.
    ///
    /// # Errors
    /// [`RequestError`] for a build mode or source kind this framing has no name for, or a record
    /// longer than `u32::MAX` bytes.
    pub fn encode(&self) -> Result<Vec<u8>, RequestError> {
        let mut out = MAGIC.to_vec();
        push_record(&mut out, "op", self.operation.name())?;
        let mode = match self.options.mode() {
            BuildMode::Prod => "prod",
            BuildMode::Dev => "dev",
            other => {
                return Err(RequestError::new(format!(
                    "build mode {other:?} has no request name"
                )));
            }
        };
        push_record(&mut out, "mode", mode)?;
        if let Some(catalogue) = &self.catalogue {
            push_record(&mut out, "language", catalogue.language())?;
            for (addon, source, value) in catalogue.entries() {
                push_record(&mut out, "term", addon)?;
                push_record(&mut out, "original", source)?;
                push_record(&mut out, "translated", value)?;
            }
        }
        for d in &self.custom_directives {
            push_record(&mut out, "directive", d.as_str())?;
        }
        for (kind, value) in &self.limits {
            push_record(&mut out, "limit", &format!("{}={value}", kind.name()))?;
        }
        for file in &self.files {
            match file.origin() {
                Origin::AddonFile { addon, path } => {
                    push_record(&mut out, "file", "addon_file")?;
                    push_record(&mut out, "addon", addon.as_str())?;
                    push_record(&mut out, "path", path.as_str())?;
                    push_record(&mut out, "source", file.xml())?;
                }
                other => {
                    return Err(RequestError::new(format!(
                        "source kind {:?} has no request name",
                        other.kind()
                    )));
                }
            }
        }
        Ok(out)
    }

    /// Decodes framed bytes.
    ///
    /// # Errors
    /// [`RequestError`] for a wrong magic, a truncated record, invalid UTF-8, an unknown or
    /// misplaced tag, an invalid name, a repeated directive or limit, or a non-canonical value.
    pub fn decode(bytes: &[u8]) -> Result<Self, RequestError> {
        let rest = bytes
            .strip_prefix(MAGIC)
            .ok_or_else(|| RequestError::new("missing ERPUI2 magic"))?;
        let mut records = Records { rest, index: 0 };
        let operation = match records.expect("op")? {
            "analyze" => Operation::Analyze,
            "compile" => Operation::Compile,
            "extract" => Operation::Extract,
            other => return Err(RequestError::new(format!("unknown operation '{other}'"))),
        };
        let mode = match records.expect("mode")? {
            "prod" => BuildMode::Prod,
            "dev" => BuildMode::Dev,
            other => return Err(RequestError::new(format!("unknown mode '{other}'"))),
        };
        let mut request = Request::new(operation, mode);
        // Stages, in this order: directives, limits, files. A record of an earlier stage after a
        // later one is out of order, so every accepted request is in `encode`'s order.
        let mut stage = Stage::Catalogue;
        let mut previous_term: Option<(&str, &str)> = None;
        while let Some((tag, value)) = records.next()? {
            let record_stage = match tag {
                "language" | "term" => Stage::Catalogue,
                "directive" => Stage::Directives,
                "limit" => Stage::Limits,
                "file" => Stage::Files,
                "arch_id" => {
                    return Err(RequestError::new(format!(
                        "record {}: unexpected tag 'arch_id' (use ArchCompileRequest to decode arch requests)",
                        records.index
                    )));
                }
                _ => {
                    return Err(RequestError::new(format!(
                        "record {}: unexpected tag '{tag}'",
                        records.index
                    )));
                }
            };
            if record_stage < stage {
                return Err(RequestError::new(format!(
                    "record {}: '{tag}' after {}",
                    records.index,
                    stage.after()
                )));
            }
            stage = record_stage;
            match tag {
                "language" => {
                    if operation != Operation::Compile || request.catalogue.is_some() {
                        return Err(RequestError::new(
                            "a single language is allowed for compilation",
                        ));
                    }
                    request.catalogue = Some(
                        erp_ui_compiler::TranslationCatalogue::new(value)
                            .map_err(|e| RequestError::new(e.to_string()))?,
                    );
                }
                "term" => {
                    let catalogue = request
                        .catalogue
                        .take()
                        .ok_or_else(|| RequestError::new("a term requires a preceding language"))?;
                    let source = records.expect("original")?;
                    let translated = records.expect("translated")?;
                    if previous_term.is_some_and(|last| last >= (value, source)) {
                        return Err(RequestError::new(
                            "catalogue entries must be unique and sorted by addon and source",
                        ));
                    }
                    previous_term = Some((value, source));
                    request.catalogue = Some(
                        if value.is_empty() {
                            catalogue.with_global_term(source, translated)
                        } else {
                            let addon = AddonName::new(value)
                                .map_err(|e| RequestError::new(e.to_string()))?;
                            catalogue.with_term(addon, source, translated)
                        }
                        .map_err(|e| RequestError::new(e.to_string()))?,
                    );
                }
                "directive" => {
                    let name = DirectiveName::new(value)
                        .map_err(|e| RequestError::new(format!("record {}: {e}", records.index)))?;
                    request = request.with_custom_directive(name)?;
                }
                "limit" => {
                    let (kind, value) = parse_limit(value)?;
                    request = request.with_limit(kind, value)?;
                }
                _ => match value {
                    "addon_file" => {
                        let addon = AddonName::new(records.expect("addon")?)
                            .map_err(|e| RequestError::new(e.to_string()))?;
                        let path = TemplatePath::new(records.expect("path")?)
                            .map_err(|e| RequestError::new(e.to_string()))?;
                        let source = records.expect("source")?;
                        request = request.with_addon_file(addon, path, source);
                    }
                    "arch_data" => {
                        return Err(RequestError::new(format!(
                            "record {}: unexpected file kind 'arch_data' in addon Request (use ArchCompileRequest)",
                            records.index
                        )));
                    }
                    other => {
                        return Err(RequestError::new(format!("unknown file kind '{other}'")));
                    }
                },
            }
        }
        Ok(request)
    }
}

/// The largest encoded arch request: the whole frame (magic, every record and its length field)
/// is bounded by the compiler's input-bytes ceiling ([`LimitKind::InputBytes`]), not only each
/// field.
pub const MAX_ARCH_FRAME_BYTES: usize = LimitKind::InputBytes.default_value();

/// An immutable request to compile database-stored arch data (ADR 0118 D1).
///
/// The canonical records, in this order: `op`, `mode`, `arch_id`, `source_digest`, `language`,
/// `host_schema`, `compiler_policy`, `limit` (0..n, each limit once), `file arch_data`, `source`.
/// The fields are validated on construction and private, and the source kind alone selects
/// restricted mode: there is no trust switch.
///
/// `source_digest` must be the SHA-256 of `source`: [`ArchCompileRequest::new`] and
/// [`ArchCompileRequest::decode`] recompute it and refuse a mismatch. That catches damaged bytes
/// only; it does not authenticate the source. The consumer binds the digest to its
/// authorization-filtered arch generation.
///
/// Native [`respond`] refuses the request before parsing any XML. The WASI guest validates
/// the source against the supplied field schema and emits a restricted card plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchCompileRequest {
    operation: Operation,
    mode: BuildMode,
    arch_id: Box<str>,
    source_digest: Box<str>,
    language: Box<str>,
    host_schema: Box<str>,
    compiler_policy: Box<str>,
    limits: Vec<(LimitKind, usize)>,
    source: Box<str>,
}

impl ArchCompileRequest {
    /// Construct a policy-1 production card request from the caller-authorized schema.
    ///
    /// # Errors
    /// Returns the same identity and framing errors as [`Self::new`].
    pub fn card(
        arch: String,
        language: String,
        fields: &[String],
        source: String,
    ) -> Result<Self, RequestError> {
        let digest = sha256_hex(source.as_bytes());
        let schema = serde_json::json!({ "fields": fields }).to_string();
        Self::new(
            Operation::Compile,
            BuildMode::Prod,
            arch,
            digest,
            language,
            schema,
            "1",
            source,
        )
    }

    /// Creates a validated arch compile request with the default limits.
    ///
    /// # Errors
    /// [`RequestError`] when an identifier is empty, too long or holds a control character, the
    /// digest is not 64 lowercase hexadecimal characters or is not the SHA-256 of `source`, the
    /// build mode has no request name, or the encoded frame would exceed
    /// [`MAX_ARCH_FRAME_BYTES`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation: Operation,
        mode: BuildMode,
        arch_id: impl Into<Box<str>>,
        source_digest: impl Into<Box<str>>,
        language: impl Into<Box<str>>,
        host_schema: impl Into<Box<str>>,
        compiler_policy: impl Into<Box<str>>,
        source: impl Into<Box<str>>,
    ) -> Result<Self, RequestError> {
        if operation == Operation::Extract {
            return Err(RequestError::new(
                "restricted arch requests support analyze and compile",
            ));
        }
        mode_name(mode)?;
        let arch_id = arch_id.into();
        validate_identifier("arch_id", &arch_id, 256)?;
        let source_digest = source_digest.into();
        validate_sha256("source_digest", &source_digest)?;
        let language = language.into();
        validate_identifier("language", &language, 256)?;
        let host_schema = host_schema.into();
        if host_schema.is_empty() {
            return Err(RequestError::new("host_schema cannot be empty"));
        }
        let compiler_policy = compiler_policy.into();
        validate_identifier("compiler_policy", &compiler_policy, 32)?;
        let source = source.into();
        let request = Self {
            operation,
            mode,
            arch_id,
            source_digest,
            language,
            host_schema,
            compiler_policy,
            limits: Vec::new(),
            source,
        };
        // The frame bound first: it also bounds the digest's work.
        request.check_frame()?;
        let computed = sha256_hex(request.source.as_bytes());
        if *request.source_digest != *computed {
            return Err(RequestError::new(format!(
                "source_digest {} is not the SHA-256 of the source ({computed})",
                request.source_digest
            )));
        }
        Ok(request)
    }

    /// Lowers one compiler limit.
    ///
    /// # Errors
    /// [`RequestError`] when the limit is already in the request, `value` is zero or above the
    /// limit's default, or the record would push the frame over [`MAX_ARCH_FRAME_BYTES`].
    pub fn with_limit(mut self, kind: LimitKind, value: usize) -> Result<Self, RequestError> {
        if self.limits.iter().any(|(k, _)| *k == kind) {
            return Err(RequestError::new(format!("limit {kind} is given twice")));
        }
        // The same rule as the trusted decoder: limits can only be lowered (one source of truth).
        CompileOptions::new(self.mode)
            .limits()
            .lowered(kind, value)
            .map_err(|e| RequestError::new(e.to_string()))?;
        self.limits.push((kind, value));
        self.check_frame()?;
        Ok(self)
    }

    /// The operation.
    #[must_use]
    pub fn operation(&self) -> Operation {
        self.operation
    }

    /// The build mode.
    #[must_use]
    pub fn mode(&self) -> BuildMode {
        self.mode
    }

    /// The arch identifier.
    #[must_use]
    pub fn arch_id(&self) -> &str {
        &self.arch_id
    }

    /// The SHA-256 of the source (64 lowercase hexadecimal characters).
    #[must_use]
    pub fn source_digest(&self) -> &str {
        &self.source_digest
    }

    /// The language code.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// The declared host schema.
    #[must_use]
    pub fn host_schema(&self) -> &str {
        &self.host_schema
    }

    /// The compiler policy.
    #[must_use]
    pub fn compiler_policy(&self) -> &str {
        &self.compiler_policy
    }

    /// The lowered limits, in request order.
    #[must_use]
    pub fn limits(&self) -> &[(LimitKind, usize)] {
        &self.limits
    }

    /// The arch source text.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The canonical records of this request, in frame order.
    fn records(&self) -> Result<Vec<(&'static str, Cow<'_, str>)>, RequestError> {
        let mut records = vec![
            ("op", Cow::Borrowed(self.operation.name())),
            ("mode", Cow::Borrowed(mode_name(self.mode)?)),
            ("arch_id", Cow::Borrowed(&*self.arch_id)),
            ("source_digest", Cow::Borrowed(&*self.source_digest)),
            ("language", Cow::Borrowed(&*self.language)),
            ("host_schema", Cow::Borrowed(&*self.host_schema)),
            ("compiler_policy", Cow::Borrowed(&*self.compiler_policy)),
        ];
        for (kind, value) in &self.limits {
            records.push(("limit", Cow::Owned(format!("{}={value}", kind.name()))));
        }
        records.push(("file", Cow::Borrowed("arch_data")));
        records.push(("source", Cow::Borrowed(&*self.source)));
        Ok(records)
    }

    /// Refuses a request whose encoded frame exceeds [`MAX_ARCH_FRAME_BYTES`].
    fn check_frame(&self) -> Result<(), RequestError> {
        let mut len = MAGIC.len();
        for (tag, value) in self.records()? {
            len = 4usize
                .checked_add(tag.len())
                .and_then(|n| n.checked_add(1))
                .and_then(|n| n.checked_add(value.len()))
                .and_then(|n| n.checked_add(len))
                .ok_or_else(|| RequestError::new("the arch request frame length overflows"))?;
        }
        if len > MAX_ARCH_FRAME_BYTES {
            return Err(RequestError::new(format!(
                "the arch request frame of {len} bytes exceeds the input-bytes ceiling of {MAX_ARCH_FRAME_BYTES} bytes"
            )));
        }
        Ok(())
    }

    /// The canonical framed bytes of this request.
    ///
    /// # Errors
    /// [`RequestError`] when a record is longer than the `u32` length field (unreachable for a
    /// request within [`MAX_ARCH_FRAME_BYTES`]).
    pub fn encode(&self) -> Result<Vec<u8>, RequestError> {
        let mut out = MAGIC.to_vec();
        for (tag, value) in self.records()? {
            push_record(&mut out, tag, &value)?;
        }
        Ok(out)
    }

    /// Decodes canonical framed bytes. A decoded request encodes back to the same bytes.
    ///
    /// # Errors
    /// [`RequestError`] for a frame over [`MAX_ARCH_FRAME_BYTES`] (refused before any record is
    /// read), a wrong magic, a truncated record, a missing, unknown, repeated or out-of-order
    /// record, an invalid field, or a `source_digest` that is not the SHA-256 of the source.
    pub fn decode(bytes: &[u8]) -> Result<Self, RequestError> {
        if bytes.len() > MAX_ARCH_FRAME_BYTES {
            return Err(RequestError::new(format!(
                "the arch request frame of {} bytes exceeds the input-bytes ceiling of {MAX_ARCH_FRAME_BYTES} bytes",
                bytes.len()
            )));
        }
        let rest = bytes
            .strip_prefix(MAGIC)
            .ok_or_else(|| RequestError::new("missing ERPUI2 magic"))?;
        let mut records = Records { rest, index: 0 };
        let operation = match records.expect("op")? {
            "analyze" => Operation::Analyze,
            "compile" => Operation::Compile,
            other => {
                return Err(RequestError::new(format!(
                    "unknown operation '{}'",
                    echo(other)
                )));
            }
        };
        let mode = match records.expect("mode")? {
            "prod" => BuildMode::Prod,
            "dev" => BuildMode::Dev,
            other => return Err(RequestError::new(format!("unknown mode '{}'", echo(other)))),
        };
        let arch_id = records.expect("arch_id")?;
        let source_digest = records.expect("source_digest")?;
        let language = records.expect("language")?;
        let host_schema = records.expect("host_schema")?;
        let compiler_policy = records.expect("compiler_policy")?;

        let mut limits = Vec::new();
        let mut source = None;
        while let Some((tag, value)) = records.next()? {
            if source.is_some() {
                return Err(RequestError::new(format!(
                    "record {}: '{tag}' after the arch file",
                    records.index
                )));
            }
            match tag {
                "limit" => limits.push(parse_limit(value)?),
                "file" => match value {
                    "arch_data" => source = Some(records.expect("source")?),
                    other => {
                        return Err(RequestError::new(format!(
                            "record {}: unknown file kind '{}' in an arch request",
                            records.index,
                            echo(other)
                        )));
                    }
                },
                _ => {
                    return Err(RequestError::new(format!(
                        "record {}: unexpected tag '{}' in an arch request",
                        records.index,
                        echo(tag)
                    )));
                }
            }
        }
        let Some(source) = source else {
            return Err(RequestError::new("missing 'file arch_data' record"));
        };
        let mut request = Self::new(
            operation,
            mode,
            arch_id,
            source_digest,
            language,
            host_schema,
            compiler_policy,
            source,
        )?;
        for (kind, value) in limits {
            request = request.with_limit(kind, value)?;
        }
        Ok(request)
    }
}

/// The request name of a build mode.
fn mode_name(mode: BuildMode) -> Result<&'static str, RequestError> {
    match mode {
        BuildMode::Prod => Ok("prod"),
        BuildMode::Dev => Ok("dev"),
        other => Err(RequestError::new(format!(
            "build mode {other:?} has no request name"
        ))),
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

/// The longest untrusted value an arch-request error repeats, in characters.
const ECHO_MAX_CHARS: usize = 80;

/// An untrusted value as an arch-request error shows it: at most [`ECHO_MAX_CHARS`] characters,
/// then a marker with the full byte length, so a refusal stays small inside the WASM host's
/// output cap (ADR 0118 D1: bounded diagnostics).
fn echo(value: &str) -> Cow<'_, str> {
    match value.char_indices().nth(ECHO_MAX_CHARS) {
        None => Cow::Borrowed(value),
        Some((cut, _)) => Cow::Owned(format!("{}... ({} bytes)", &value[..cut], value.len())),
    }
}

fn validate_identifier(name: &str, value: &str, max_len: usize) -> Result<(), RequestError> {
    if value.is_empty() {
        return Err(RequestError::new(format!("{name} cannot be empty")));
    }
    if value.len() > max_len {
        return Err(RequestError::new(format!(
            "{name} length must be 1..={max_len}, got {}",
            value.len()
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(RequestError::new(format!(
            "{name} cannot contain control characters"
        )));
    }
    Ok(())
}

fn validate_sha256(name: &str, value: &str) -> Result<(), RequestError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(RequestError::new(format!(
            "{name} must be a 64-character lowercase hexadecimal SHA-256 digest, got '{}'",
            echo(value)
        )));
    }
    Ok(())
}

/// The record stages of a request, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Catalogue,
    Directives,
    Limits,
    Files,
}

impl Stage {
    fn after(self) -> &'static str {
        match self {
            Stage::Catalogue => "a catalogue record",
            Stage::Directives => "a directive",
            Stage::Limits => "a limit",
            Stage::Files => "the first file",
        }
    }
}

/// `name=value` of a `limit` record.
fn parse_limit(value: &str) -> Result<(LimitKind, usize), RequestError> {
    let (name, number) = value
        .split_once('=')
        .ok_or_else(|| RequestError::new(format!("limit record '{value}' is not name=value")))?;
    let kind = LimitKind::from_name(name)
        .ok_or_else(|| RequestError::new(format!("unknown limit '{name}'")))?;
    let canonical = !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && (number == "0" || !number.starts_with('0'));
    if !canonical {
        return Err(RequestError::new(format!(
            "limit {name}: '{number}' is not a decimal number without leading zeros"
        )));
    }
    let value = number
        .parse::<usize>()
        .map_err(|e| RequestError::new(format!("limit {name}: {e}")))?;
    Ok((kind, value))
}

fn push_record(out: &mut Vec<u8>, tag: &str, value: &str) -> Result<(), RequestError> {
    let len = tag.len() + 1 + value.len();
    let len = u32::try_from(len).map_err(|_| {
        RequestError::new(format!(
            "'{tag}' record of {len} bytes exceeds the u32 length field"
        ))
    })?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(tag.as_bytes());
    out.push(b' ');
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

/// The record reader of [`Request::decode`].
struct Records<'a> {
    rest: &'a [u8],
    /// The 1-based number of the last record read (for messages).
    index: usize,
}

impl<'a> Records<'a> {
    /// The next record as `(tag, value)`, or `None` at the end.
    fn next(&mut self) -> Result<Option<(&'a str, &'a str)>, RequestError> {
        if self.rest.is_empty() {
            return Ok(None);
        }
        self.index += 1;
        let at = self.index;
        let (len, body) = self
            .rest
            .split_first_chunk::<4>()
            .ok_or_else(|| RequestError::new(format!("record {at}: truncated length")))?;
        let len = usize::try_from(u32::from_be_bytes(*len))
            .map_err(|_| RequestError::new(format!("record {at}: length does not fit")))?;
        let (record, rest) = body
            .split_at_checked(len)
            .ok_or_else(|| RequestError::new(format!("record {at}: truncated")))?;
        self.rest = rest;
        let text = std::str::from_utf8(record)
            .map_err(|e| RequestError::new(format!("record {at} is not UTF-8: {e}")))?;
        let (tag, value) = text
            .split_once(' ')
            .ok_or_else(|| RequestError::new(format!("record {at} has no 'tag value' form")))?;
        Ok(Some((tag, value)))
    }

    /// The value of the next record, which must carry `tag`.
    fn expect(&mut self, tag: &str) -> Result<&'a str, RequestError> {
        match self.next()? {
            Some((t, value)) if t == tag => Ok(value),
            Some((t, _)) => Err(RequestError::new(format!(
                "record {}: expected '{tag}', got '{t}'",
                self.index
            ))),
            None => Err(RequestError::new(format!("missing '{tag}' record"))),
        }
    }
}

/// Reads a request from `input`, refusing more than [`MAX_REQUEST_BYTES`].
///
/// # Errors
/// [`RequestError`] when the input is larger than the cap or cannot be read.
pub fn read_request(input: impl Read) -> Result<Vec<u8>, RequestError> {
    let cap = u64::try_from(MAX_REQUEST_BYTES)
        .map_err(|_| RequestError::new("the request cap does not fit in u64"))?;
    let mut bytes = Vec::new();
    input
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|e| RequestError::new(format!("cannot read the request: {e}")))?;
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(RequestError::new(format!(
            "the request is larger than {MAX_REQUEST_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

/// The result of a request: the canonical bytes and whether they carry an error.
///
/// Only [`respond`] builds one: no caller can construct a compiler response. The WASM host
/// (`erp-ui-compiler-host`) returns its own response type built from the guest's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    bytes: Vec<u8>,
    clean: bool,
}

impl Response {
    /// The canonical bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The canonical bytes, owned.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Whether the result has no error: no rejected template and no set error (analyze), or a
    /// compiled set (compile).
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.clean
    }

    /// The binary's exit status for this result: [`EXIT_CLEAN`] or [`EXIT_ERRORS`].
    #[must_use]
    pub fn exit_status(&self) -> u8 {
        if self.clean { EXIT_CLEAN } else { EXIT_ERRORS }
    }
}

/// Whether `bytes` frame an arch request: the third record carries the `arch_id` tag. Reads only
/// record headers, never a value's XML. A frame that is not an arch request by this test, but
/// carries an `arch_id` record or an `arch_data` file anywhere else, is refused by
/// [`Request::decode`].
fn is_arch_request(bytes: &[u8]) -> bool {
    let Some(rest) = bytes.strip_prefix(MAGIC) else {
        return false;
    };
    let mut records = Records { rest, index: 0 };
    for tag in ["op", "mode", "arch_id"] {
        match records.next() {
            Ok(Some((t, _))) if t == tag => {}
            // Not an arch request; the trusted decoder reports what is wrong with it.
            Ok(_) | Err(_) => return false,
        }
    }
    true
}

/// The native refusal of an arch request (ADR 0118 D1, SPEC-UIRESTRICT-001).
#[cfg(not(target_arch = "wasm32"))]
fn respond_arch(_bytes: &[u8]) -> Result<Response, RequestError> {
    Err(RequestError::new(
        "native processing refuses arch requests before parsing them: database-stored arch is compiled only by the WASM build in the compiler host (SPEC-UIRESTRICT-001)",
    ))
}

#[cfg(any(target_arch = "wasm32", test))]
mod restricted;

/// Only the WASI guest accepts a database arch. Native reference compilation is test-only.
#[cfg(target_arch = "wasm32")]
fn respond_arch(bytes: &[u8]) -> Result<Response, RequestError> {
    restricted::compile(bytes)
}

/// Runs a framed request.
///
/// Trusted addon files can be analysed, compiled or extracted. Restricted database cards
/// can be analysed or compiled only in the WASI guest.
///
/// # Errors
/// [`RequestError`] for an invalid request or restricted XML, or a native arch request.
pub fn respond(bytes: &[u8]) -> Result<Response, RequestError> {
    if is_arch_request(bytes) {
        return respond_arch(bytes);
    }

    let Request {
        operation,
        options,
        catalogue,
        files,
        ..
    } = Request::decode(bytes)?;
    let mut set = TemplateSet::new();
    for file in files {
        set.push(file);
    }
    let mut out = String::new();
    let clean = match operation {
        Operation::Analyze => {
            let analysis = erp_ui_compiler::analyze(&set, &options);
            analysis.write_canonical(&mut out);
            analysis.is_clean()
        }
        Operation::Extract => match erp_ui_compiler::extract_translations(&set, &options) {
            Ok(extracted) => {
                let terms: Vec<_> = extracted.terms().iter().map(|term| serde_json::json!({
                    "addon": term.addon().as_str(), "source": term.source(),
                    "locations": term.locations().iter().map(ToString::to_string).collect::<Vec<_>>(),
                })).collect();
                out = serde_json::to_string(&terms)
                    .map_err(|error| RequestError::new(error.to_string()))?;
                true
            }
            Err(errors) => {
                errors.write_canonical(&mut out);
                false
            }
        },
        Operation::Compile => match match catalogue {
            Some(catalogue) => erp_ui_compiler::compile_localized(&set, &options, &catalogue),
            None => erp_ui_compiler::compile(&set, &options),
        } {
            Ok(compiled) => {
                compiled.write_canonical(&mut out);
                true
            }
            Err(errors) => {
                errors.write_canonical(&mut out);
                false
            }
        },
    };
    Ok(Response {
        bytes: out.into_bytes(),
        clean,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use super::*;
    use proptest::prelude::*;

    #[test]
    fn catalogue_extract_and_compile_use_the_same_framed_contract() {
        let xml = "<templates><t t-name='web.X'><p>Hello</p></t></templates>";
        let inputs = [
            ("file", "addon_file"),
            ("addon", "web"),
            ("path", "a.xml"),
            ("source", xml),
        ];
        let mut records = vec![("op", "extract"), ("mode", "prod")];
        records.extend(inputs);
        let terms: serde_json::Value =
            serde_json::from_slice(respond(&framed(&records)).unwrap().bytes()).unwrap();
        assert_eq!(terms[0]["source"], "Hello");
        assert_eq!(terms[0]["addon"], "web");
        let mut records = vec![
            ("op", "compile"),
            ("mode", "prod"),
            ("language", "fr_FR"),
            ("term", "web"),
            ("original", "Hello"),
            ("translated", "Bonjour"),
        ];
        records.extend(inputs);
        let payload = framed(&records);
        assert_eq!(
            Request::decode(&payload).unwrap().encode().unwrap(),
            payload
        );
        let response = respond(&payload).unwrap();
        assert_eq!(response.exit_status(), 0);
        assert!(
            String::from_utf8(response.into_bytes())
                .unwrap()
                .contains("Bonjour")
        );
    }

    fn sample() -> Request {
        Request::new(Operation::Compile, BuildMode::Dev)
            .with_custom_directive(DirectiveName::new("click").unwrap())
            .unwrap()
            .with_custom_directive(DirectiveName::new("portal").unwrap())
            .unwrap()
            .with_limit(LimitKind::Depth, 32)
            .unwrap()
            .with_addon_file(
                AddonName::new("web").unwrap(),
                TemplatePath::new("a.xml").unwrap(),
                "<templates/>",
            )
            .with_addon_file(
                AddonName::new("web").unwrap(),
                TemplatePath::new("b.xml").unwrap(),
                "",
            )
    }

    fn record(tag: &str, value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        push_record(&mut out, tag, value).unwrap();
        out
    }

    fn framed(records: &[(&str, &str)]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        for (tag, value) in records {
            out.extend(record(tag, value));
        }
        out
    }

    fn error_of(bytes: &[u8]) -> String {
        Request::decode(bytes).unwrap_err().message().to_owned()
    }

    #[test]
    fn requests_round_trip_byte_for_byte() {
        let r = sample();
        let bytes = r.encode().unwrap();
        let back = Request::decode(&bytes).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.encode().unwrap(), bytes);
        assert_eq!(back.limits(), [(LimitKind::Depth, 32)]);
    }

    #[test]
    fn malformed_requests_name_the_problem() {
        assert!(error_of(b"nope").contains("missing ERPUI2 magic"));
        let mut truncated = sample().encode().unwrap();
        truncated.pop();
        assert!(error_of(&truncated).contains("truncated"));
        let base = [("op", "analyze"), ("mode", "prod")];
        let with = |extra: &[(&str, &str)]| {
            let mut all = base.to_vec();
            all.extend_from_slice(extra);
            error_of(&framed(&all))
        };
        assert!(with(&[("directive", "")]).contains("record 3"));
        assert!(with(&[("directive", "a"), ("directive", "a")]).contains("given twice"));
        assert!(with(&[("limit", "depth=8"), ("limit", "depth=9")]).contains("given twice"));
        assert!(with(&[("limit", "depth=08")]).contains("leading zeros"));
        assert!(with(&[("limit", "depth=0")]).contains("cannot be set to 0"));
        assert!(with(&[("limit", "depth=65")]).contains("cannot be set to 65"));
        assert!(with(&[("limit", "speed=1")]).contains("unknown limit"));
        assert!(with(&[("limit", "depth")]).contains("not name=value"));
        assert!(with(&[("file", "db_view")]).contains("unknown file kind"));
        assert!(with(&[("file", "addon_file"), ("path", "a.xml")]).contains("expected 'addon'"));
        assert!(with(&[("file", "addon_file"), ("addon", "web")]).contains("missing 'path'"));
        assert!(
            with(&[
                ("file", "addon_file"),
                ("addon", "web"),
                ("path", "a.xml"),
                ("source", ""),
                ("directive", "a")
            ])
            .contains("after the first file")
        );
        assert!(with(&[("files", "x")]).contains("unexpected tag 'files'"));
        // Round-two review M2: stages are ordered, so decode-encode is the identity.
        assert!(
            with(&[("limit", "depth=8"), ("directive", "a")]).contains("'directive' after a limit")
        );
        // Round-two review M3: the directive count is capped.
        let many: Vec<String> = (0..=erp_ui_compiler::MAX_CUSTOM_DIRECTIVES)
            .map(|i| format!("d{i}"))
            .collect();
        let records: Vec<(&str, &str)> = many.iter().map(|d| ("directive", d.as_str())).collect();
        assert!(with(&records).contains("more than 256 custom directives"));
        assert!(error_of(&framed(&[("mode", "prod")])).contains("expected 'op'"));
        assert!(error_of(&framed(&[("op", "run")])).contains("unknown operation"));
        let mut no_space = MAGIC.to_vec();
        no_space.extend(4u32.to_be_bytes());
        no_space.extend(b"opxx");
        assert!(error_of(&no_space).contains("'tag value' form"));
    }

    #[test]
    fn oversized_input_is_refused() {
        let reader = std::io::repeat(0).take(MAX_REQUEST_BYTES as u64 + 10);
        let err = read_request(reader).unwrap_err();
        assert!(err.message().contains("larger than"));
        let ok = read_request(&b"ERPUI2\n"[..]).unwrap();
        assert_eq!(ok, b"ERPUI2\n");
    }

    #[test]
    fn exit_status_follows_the_result() {
        let ok = Request::new(Operation::Compile, BuildMode::Prod).with_addon_file(
            AddonName::new("web").unwrap(),
            TemplatePath::new("a.xml").unwrap(),
            "<templates><t t-name=\"web.A\"><b/></t></templates>",
        );
        let clean = respond(&ok.encode().unwrap()).unwrap();
        assert_eq!(clean.exit_status(), EXIT_CLEAN);
        let broken = Request::new(Operation::Compile, BuildMode::Prod).with_addon_file(
            AddonName::new("web").unwrap(),
            TemplatePath::new("a.xml").unwrap(),
            "<templates><t t-name=\"web.A\"><b t-esc=\"(\"/></t></templates>",
        );
        let response = respond(&broken.encode().unwrap()).unwrap();
        assert_eq!(response.exit_status(), EXIT_ERRORS);
        assert!(!response.bytes().is_empty());
        let analysed = Request {
            operation: Operation::Analyze,
            ..broken
        };
        assert_eq!(
            respond(&analysed.encode().unwrap()).unwrap().exit_status(),
            EXIT_ERRORS
        );
    }

    #[test]
    fn request_limits_reach_the_compiler() {
        let request = Request::new(Operation::Analyze, BuildMode::Prod)
            .with_limit(LimitKind::Files, 1)
            .unwrap()
            .with_addon_file(
                AddonName::new("web").unwrap(),
                TemplatePath::new("a.xml").unwrap(),
                "<templates/>",
            )
            .with_addon_file(
                AddonName::new("web").unwrap(),
                TemplatePath::new("b.xml").unwrap(),
                "<templates/>",
            );
        let response = respond(&request.encode().unwrap()).unwrap();
        assert_eq!(response.exit_status(), EXIT_ERRORS);
        let text = String::from_utf8(response.into_bytes()).unwrap();
        assert!(
            text.contains("files limit exceeded: maximum 1, got 2"),
            "{text}"
        );
    }

    fn arch_request(digest: Option<&str>) -> Vec<u8> {
        let source = "<kanban><templates><t t-name=\"card\"><div/></t></templates></kanban>";
        let computed = sha256_hex(source.as_bytes());
        let digest = digest.unwrap_or(&computed);
        framed(&[
            ("op", "compile"),
            ("mode", "prod"),
            ("arch_id", "res.partner.kanban"),
            ("source_digest", digest),
            ("language", "en_US"),
            ("host_schema", r#"{"fields":[]}"#),
            ("compiler_policy", "1"),
            ("file", "arch_data"),
            ("source", source),
        ])
    }

    /// The guest emits a declarative plan only after checking source identity.
    #[test]
    fn guest_checks_source_identity_before_emitting_a_card_plan() {
        let bytes = arch_request(None);
        assert!(is_arch_request(&bytes));
        let result = restricted::compile(&bytes).unwrap();
        let plan: serde_json::Value = serde_json::from_slice(result.bytes()).unwrap();
        assert_eq!(plan["abi"], "erpforall.card.v1");
        assert_eq!(plan["templates"]["card"]["children"][0]["tag"], "div");

        let zeros = "0".repeat(64);
        let err = restricted::compile(&arch_request(Some(&zeros))).unwrap_err();
        assert!(
            err.message().starts_with(&format!(
                "source_digest {zeros} is not the SHA-256 of the source ("
            )),
            "{}",
            err.message()
        );
    }

    /// The native arm refuses the same request without decoding it.
    ///
    /// SPEC-UIRESTRICT-001 (partial: native processing refuses arch requests before XML parsing; trusted requests unaffected; view adapter not implemented)
    #[test]
    fn native_arm_refuses_without_decoding() {
        let zeros = "0".repeat(64);
        let err = respond(&arch_request(Some(&zeros))).unwrap_err();
        assert_eq!(
            err.message(),
            "native processing refuses arch requests before parsing them: database-stored arch is compiled only by the WASM build in the compiler host (SPEC-UIRESTRICT-001)"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Arbitrary bytes after the magic never panic the decoder, and whatever decodes encodes
        /// back to the same bytes (decoding drops and rewrites nothing).
        #[test]
        fn decoding_is_total_and_canonical(body in proptest::collection::vec(any::<u8>(), 0..256)) {
            let mut bytes = MAGIC.to_vec();
            bytes.extend(body);
            if let Ok(request) = Request::decode(&bytes) {
                prop_assert_eq!(request.encode().unwrap(), bytes);
            }
        }

        /// Round-two review M2: valid directive and limit records in any order and with repeats:
        /// whatever decodes encodes back to the same bytes, and a sorted, distinct draw always
        /// decodes.
        #[test]
        fn valid_header_records_round_trip(
            records in proptest::collection::vec(
                prop_oneof![
                    "[a-z0-9_]{1,8}".prop_map(|d| ("directive", d)),
                    (0..LimitKind::ALL.len(), 1usize..=64)
                        .prop_map(|(k, v)| ("limit", format!("{}={v}", LimitKind::ALL[k].name()))),
                ],
                0..10,
            ),
        ) {
            let mut all = vec![("op", "analyze".to_owned()), ("mode", "prod".to_owned())];
            all.extend(records.iter().map(|(t, v)| (*t, v.clone())));
            let refs: Vec<(&str, &str)> = all.iter().map(|(t, v)| (*t, v.as_str())).collect();
            let bytes = framed(&refs);
            if let Ok(request) = Request::decode(&bytes) {
                prop_assert_eq!(request.encode().unwrap(), bytes);
            }
            // The same records in encode's order, without repeats, always decode.
            let mut directives: Vec<&String> =
                records.iter().filter(|(t, _)| *t == "directive").map(|(_, v)| v).collect();
            directives.sort();
            directives.dedup();
            let mut limits: Vec<&String> = Vec::new();
            for (t, v) in &records {
                let name = v.split('=').next();
                if *t == "limit" && !limits.iter().any(|l| l.split('=').next() == name) {
                    limits.push(v);
                }
            }
            let mut ordered = vec![("op", "analyze"), ("mode", "prod")];
            ordered.extend(directives.iter().map(|d| ("directive", d.as_str())));
            ordered.extend(limits.iter().map(|l| ("limit", l.as_str())));
            let bytes = framed(&ordered);
            let request = Request::decode(&bytes);
            prop_assert!(request.is_ok(), "{:?}", request.err());
            prop_assert_eq!(request.unwrap().encode().unwrap(), bytes);
        }

        /// Well-framed records with arbitrary tags and values: the same property, reaching the
        /// tag and value checks rather than failing on the framing.
        #[test]
        fn decoding_framed_records_is_canonical(
            records in proptest::collection::vec(
                (
                    prop::sample::select(vec![
                        "op", "mode", "directive", "limit", "file", "addon", "path", "source", "x",
                    ]),
                    "[a-z0-9_=./<>]{0,12}",
                ),
                0..12,
            ),
        ) {
            let records: Vec<(&str, &str)> =
                records.iter().map(|(t, v)| (*t, v.as_str())).collect();
            let bytes = framed(&records);
            if let Ok(request) = Request::decode(&bytes) {
                prop_assert_eq!(request.encode().unwrap(), bytes);
            }
        }
    }
}
