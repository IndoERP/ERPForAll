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
//! | `op` | 1 | `analyze` or `compile` |
//! | `mode` | 1 | `prod` or `dev` |
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
//! assert_eq!(response.exit_status(), EXIT_CLEAN);
//! assert!(String::from_utf8(response.into_bytes())?.contains("templates 1 passed 1 rejected 0"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use erp_ui_compiler::{
    AddonName, BuildMode, CompileOptions, DirectiveName, LimitKind, Origin, TemplatePath,
    TemplateSet, TemplateSource,
};
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
    /// [`erp_ui_compiler::compile`].
    Compile,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Operation::Analyze => "analyze",
            Operation::Compile => "compile",
        }
    }
}

/// A request: the operation, the compile options and the files, in set order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    operation: Operation,
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
        let mut stage = Stage::Directives;
        while let Some((tag, value)) = records.next()? {
            let record_stage = match tag {
                "directive" => Stage::Directives,
                "limit" => Stage::Limits,
                "file" => Stage::Files,
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
                    other => {
                        return Err(RequestError::new(format!("unknown file kind '{other}'")));
                    }
                },
            }
        }
        Ok(request)
    }
}

/// The record stages of a request, in their order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    Directives,
    Limits,
    Files,
}

impl Stage {
    fn after(self) -> &'static str {
        match self {
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

/// Runs a framed request.
///
/// # Errors
/// [`RequestError`] when the request is malformed.
pub fn respond(bytes: &[u8]) -> Result<Response, RequestError> {
    let Request {
        operation,
        options,
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
        Operation::Compile => match erp_ui_compiler::compile(&set, &options) {
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
