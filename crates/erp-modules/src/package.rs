//! Package format (.erpaddon), storage, integrity and security gates.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::time::SystemTime;

use erp_fields::{Error as OrmError, Result as OrmResult};
use erp_orm::Transaction;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};

use crate::error::{Error, Result};
use crate::manifest::{Manifest, ModuleName};
use crate::version::ModuleVersion;

/// Largest `filesystem:temp:<N><unit>` number a manifest may request.
const MAX_TEMP_FS_UNITS: u64 = 4096;
/// Longest accepted `network:outbound:` URL pattern.
const MAX_NETWORK_PATTERN_BYTES: usize = 256;

/// Parsed WebAssembly capability (plan `m2b-wasm-addons.md` section 3).
///
/// Only the capability names the spec defines are accepted; an unknown name is refused loudly
/// until a spec adds it (no open-ended custom capability).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum WasmCapability {
    /// Outbound network access: `network:outbound:<url-pattern>` (`http(s)://host[/path][*]`).
    NetworkOutbound(String),
    /// Isolated temporary scratch storage: `filesystem:temp:<N>(kb|mb|gb)`.
    FilesystemTemp(String),
    /// Permission to execute raw SQL migration files in migrations/*.sql.
    MigrationsSql,
}

/// Strictly parse and normalise a `network:outbound:` target: `http(s)://host[:port][/path]`.
///
/// The host is an exact DNS name or an IP literal (lowercased), the port is explicit and in
/// 1..=65535, userinfo, backslashes, percent-encoding in the authority, `?`, `#` and any wildcard
/// in the host are refused, and `*` is allowed only as the whole last path segment (`/*`). The
/// returned text is the normalised form the consent text and the enforcer both use.
fn parse_network_pattern(target: &str) -> std::result::Result<String, String> {
    if target.len() > MAX_NETWORK_PATTERN_BYTES {
        return Err(format!(
            "network:outbound pattern longer than {MAX_NETWORK_PATTERN_BYTES} bytes"
        ));
    }
    if !target.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("network:outbound pattern must be printable ASCII without whitespace".into());
    }
    if target.contains('\\') {
        return Err("network:outbound pattern must not contain a backslash".into());
    }
    let (scheme, rest) = if let Some(r) = target.strip_prefix("https://") {
        ("https", r)
    } else if let Some(r) = target.strip_prefix("http://") {
        ("http", r)
    } else {
        return Err("network:outbound requires an http:// or https:// URL pattern".into());
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err("network:outbound pattern needs a host".into());
    }
    if authority.contains('@') {
        return Err("network:outbound pattern must not contain userinfo (`@`)".into());
    }
    if authority.contains('%') {
        return Err("network:outbound authority must not be percent-encoded".into());
    }
    if authority.contains('*') {
        return Err("network:outbound does not allow a wildcard in the host".into());
    }
    if path.contains(['?', '#', '%']) {
        return Err("network:outbound pattern must not contain `?`, `#` or `%`".into());
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (addr, after) = v6
            .split_once(']')
            .ok_or_else(|| "unterminated `[` in IPv6 host".to_string())?;
        addr.parse::<std::net::Ipv6Addr>()
            .map_err(|_| format!("invalid IPv6 host '{addr}'"))?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or("junk after IPv6 host")?),
        };
        (format!("[{}]", addr.to_ascii_lowercase()), port)
    } else {
        let (host, port) = match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        };
        if host.is_empty() {
            return Err("network:outbound pattern needs a host".into());
        }
        let is_ipv4 = host.parse::<std::net::Ipv4Addr>().is_ok();
        // A DNS host whose last label starts with a digit (`127.1`, `2130706433`, `1e100`) or
        // looks hexadecimal (`0x7f`) is an IPv4 spelling some resolvers accept; only a strict
        // dotted quad is an address here.
        let last_label = host.rsplit('.').next().unwrap_or("");
        let ambiguous_numeric = last_label.starts_with(|c: char| c.is_ascii_digit())
            || last_label.to_ascii_lowercase().starts_with("0x");
        if ambiguous_numeric && !is_ipv4 {
            return Err(format!(
                "host '{host}' is a non-canonical IPv4 form; write a dotted quad such as 127.0.0.1"
            ));
        }
        if !is_ipv4 {
            if host.len() > 253 {
                return Err("host longer than 253 bytes".into());
            }
            for label in host.split('.') {
                let ok = !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-');
                if !ok {
                    return Err(format!("invalid DNS name '{host}' (bad label '{label}')"));
                }
            }
        }
        (host.to_ascii_lowercase(), port)
    };
    let port_text = match port {
        None => String::new(),
        Some(p) => {
            let n: u16 = p
                .parse()
                .ok()
                .filter(|n| *n != 0 && p.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| format!("invalid port '{p}' (1-65535)"))?;
            format!(":{n}")
        }
    };
    if path.contains('*') {
        let body = path.strip_suffix("/*").unwrap_or("");
        if path.is_empty() || body.contains('*') || !path.ends_with("/*") {
            return Err(
                "network:outbound allows `*` only as the whole last path segment (`/*`)".into(),
            );
        }
    }
    if path.contains("//") || path.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(
            "network:outbound path must be canonical (no `//`, `.` or `..` segment)".into(),
        );
    }
    Ok(format!("{scheme}://{host}{port_text}{path}"))
}

impl WasmCapability {
    /// The capability names this build knows.
    pub const KNOWN: &'static str =
        "network:outbound:<url>, filesystem:temp:<N>(kb|mb|gb), migrations-sql";

    /// Parse and validate a capability string. The text is matched exactly (no trimming, no case
    /// folding): `Network:Outbound:x` is an unknown capability, not a spelling variant.
    pub fn parse(s: &str) -> Result<Self> {
        let invalid = |reason: String| Error::InvalidCapability {
            capability: s.to_string(),
            reason,
        };
        if s.is_empty() {
            return Err(invalid("capability string cannot be empty".to_string()));
        }
        if s == "migrations-sql" {
            return Ok(Self::MigrationsSql);
        }
        if let Some(target) = s.strip_prefix("network:outbound:") {
            return parse_network_pattern(target)
                .map(Self::NetworkOutbound)
                .map_err(invalid);
        }
        if let Some(size) = s.strip_prefix("filesystem:temp:") {
            let digits_end = size
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(size.len());
            let (digits, unit) = size.split_at(digits_end);
            let number: u64 = digits.parse().map_err(|_| {
                invalid(
                    "filesystem:temp requires a size like 50mb (digits then kb, mb or gb)"
                        .to_string(),
                )
            })?;
            if !matches!(unit, "kb" | "mb" | "gb") {
                return Err(invalid(format!(
                    "filesystem:temp unit must be kb, mb or gb (got '{unit}')"
                )));
            }
            if number == 0 || number > MAX_TEMP_FS_UNITS {
                return Err(invalid(format!(
                    "filesystem:temp size must be between 1 and {MAX_TEMP_FS_UNITS} {unit}"
                )));
            }
            return Ok(Self::FilesystemTemp(size.to_string()));
        }
        Err(invalid(format!(
            "unknown capability; known: {}",
            Self::KNOWN
        )))
    }

    /// Return the canonical string representation of this capability.
    pub fn canonical_str(&self) -> String {
        match self {
            Self::NetworkOutbound(p) => format!("network:outbound:{p}"),
            Self::FilesystemTemp(s) => format!("filesystem:temp:{s}"),
            Self::MigrationsSql => "migrations-sql".to_string(),
        }
    }
}

impl std::fmt::Display for WasmCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.canonical_str())
    }
}

/// Manifest `[wasm]` section metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct WasmSection {
    pub sdk_version: String,
    pub capabilities: Vec<WasmCapability>,
}

/// Raw representation of the `[wasm]` manifest section during deserialization.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawWasmSection {
    pub sdk_version: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// Administrator approval for an addon's requested capabilities and/or client assets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminConsent {
    pub addon_name: ModuleName,
    pub approved_capabilities: BTreeSet<String>,
    pub client_dist_digest_ack: Option<String>,
}

impl AdminConsent {
    pub fn new(addon_name: ModuleName) -> Self {
        Self {
            addon_name,
            approved_capabilities: BTreeSet::new(),
            client_dist_digest_ack: None,
        }
    }

    pub fn approve_capability(&mut self, cap: impl Into<String>) -> &mut Self {
        self.approved_capabilities.insert(cap.into());
        self
    }

    pub fn set_client_dist_digest_ack(&mut self, digest: impl Into<String>) -> &mut Self {
        self.client_dist_digest_ack = Some(digest.into());
        self
    }
}

/// Helper to format a byte slice as a lowercase hex string.
fn format_hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    s
}

/// Helper to calculate the SHA-256 hash as a 64-char lowercase hex string.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    format_hex_digest(&result)
}

/// Hard bounds applied while reading an untrusted `.erpaddon` archive (hardening).
///
/// Every bound is enforced while reading: the header size fields are never trusted for an
/// allocation, and each entry is inflated through a bounded reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PackageLimits {
    /// Largest accepted archive, in bytes.
    pub max_archive_bytes: usize,
    /// Largest accepted number of archive entries (directories included).
    pub max_entries: usize,
    /// Largest accepted uncompressed size of one entry.
    pub max_entry_bytes: u64,
    /// Largest accepted sum of the uncompressed sizes of all entries.
    pub max_total_bytes: u64,
}

/// Default archive size bound: 64 MiB.
pub const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
/// Default entry count bound.
pub const MAX_ENTRIES: usize = 4096;
/// Default per-entry uncompressed bound: 32 MiB.
pub const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
/// Default total uncompressed bound: 128 MiB.
pub const MAX_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
/// Longest accepted archive path, in bytes.
pub const MAX_PATH_BYTES: usize = 512;
/// Longest accepted path segment, in bytes.
pub const MAX_SEGMENT_BYTES: usize = 255;
/// Longest module name stored in `ir_module_package.name`.
pub const MAX_PACKAGE_NAME_CHARS: usize = 128;
/// Longest version string stored in `ir_module_package.version`.
pub const MAX_PACKAGE_VERSION_CHARS: usize = 32;

impl Default for PackageLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: MAX_ARCHIVE_BYTES,
            max_entries: MAX_ENTRIES,
            max_entry_bytes: MAX_ENTRY_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
        }
    }
}

fn limit_error(limit: &'static str, max: u64, observed_at_least: u64) -> Error {
    Error::PackageLimitExceeded {
        limit,
        max,
        observed_at_least,
    }
}

/// Validate one archive or `SHA256SUMS` path. The same rule applies to both sides, and there is
/// no normalisation: a path must already be canonical, so two distinct entries can never resolve
/// to one name. Returns the reason on refusal.
fn validate_entry_path(path: &str) -> std::result::Result<(), String> {
    if path.is_empty() {
        return Err("empty path".to_string());
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(format!("path longer than {MAX_PATH_BYTES} bytes"));
    }
    if path.starts_with('/') {
        return Err("absolute path".to_string());
    }
    if path.contains('\\') {
        return Err("backslash separator (use `/`)".to_string());
    }
    if path.contains(':') {
        return Err("`:` is not allowed (drive-letter or stream path)".to_string());
    }
    if path.chars().any(char::is_control) {
        return Err("control character in path".to_string());
    }
    for segment in path.split('/') {
        match segment {
            "" => return Err("empty path segment".to_string()),
            "." => return Err("`.` path segment (not canonical)".to_string()),
            ".." => return Err("`..` path traversal segment".to_string()),
            s if s.len() > MAX_SEGMENT_BYTES => {
                return Err(format!(
                    "path segment longer than {MAX_SEGMENT_BYTES} bytes"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn path_error(raw: &str, reason: &str) -> Error {
    Error::PackageFormat {
        message: format!("illegal path in package archive: {raw:?}: {reason}"),
    }
}

/// The total entry count in the last end-of-central-directory record; `None` when absent or
/// when the zip64 marker (`0xFFFF`) is set.
fn declared_entry_count(bytes: &[u8]) -> Option<usize> {
    let at = bytes
        .windows(4)
        .rposition(|w| w == [0x50, 0x4b, 0x05, 0x06])?;
    let field = bytes.get(at + 10..at + 12)?;
    let count = u16::from_le_bytes([field[0], field[1]]);
    (count != u16::MAX).then_some(usize::from(count))
}

/// Parse `SHA256SUMS` strictly: one `<64 lowercase hex><space><space|*><path>` per line, `\n`
/// separated, no blank or comment lines, no duplicate path, canonical paths only.
fn parse_sha256sums(text: &str) -> Result<BTreeMap<String, String>> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut sums = BTreeMap::new();
    if body.is_empty() {
        return Ok(sums);
    }
    for (index, line) in body.split('\n').enumerate() {
        let line_no = index + 1;
        let malformed = |why: &str| Error::PackageIntegrity {
            message: format!("malformed SHA256SUMS line {line_no}: {why}"),
        };
        let bytes = line.as_bytes();
        if bytes.len() < 67 {
            return Err(malformed("expected `<sha256>  <path>`"));
        }
        let (Some(hash), Some(rest)) = (line.get(..64), line.get(64..)) else {
            return Err(malformed("hash must be 64 ASCII hex characters"));
        };
        if !hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(malformed("hash must be 64 lowercase hex characters"));
        }
        let mut rest_bytes = rest.bytes();
        if rest_bytes.next() != Some(b' ') || !matches!(rest_bytes.next(), Some(b' ' | b'*')) {
            return Err(malformed(
                "hash and path must be separated by two characters (space, then space or `*`)",
            ));
        }
        let path = rest
            .get(2..)
            .ok_or_else(|| malformed("path is not valid text"))?;
        validate_entry_path(path).map_err(|reason| Error::PackageFormat {
            message: format!("illegal path in SHA256SUMS line {line_no}: {path:?}: {reason}"),
        })?;
        if path == "SHA256SUMS" {
            return Err(malformed("SHA256SUMS must not list itself"));
        }
        if sums.insert(path.to_string(), hash.to_string()).is_some() {
            return Err(malformed(&format!("duplicate entry for {path:?}")));
        }
    }
    Ok(sums)
}

/// A parsed and verified `.erpaddon` package archive.
///
/// The verified archive bytes live inside the value: storage writes exactly these bytes, so the
/// stored blob can never differ from the package that was verified.
#[derive(Debug, Clone)]
pub struct ErpAddonPackage {
    archive_sha256: String,
    archive_bytes: Vec<u8>,
    manifest: Manifest,
    files: BTreeMap<String, Vec<u8>>,
    client_dist_digest: Option<String>,
}

impl ErpAddonPackage {
    /// Load, parse and verify a `.erpaddon` archive from raw bytes (default [`PackageLimits`]).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Self::from_bytes_with(bytes, None, &PackageLimits::default())
    }

    /// Load, parse and verify a `.erpaddon` archive from raw bytes with an explicit module name.
    pub fn from_bytes_with_name(bytes: &[u8], explicit_name: Option<&str>) -> Result<Self> {
        Self::from_bytes_with(bytes, explicit_name, &PackageLimits::default())
    }

    /// Load, parse and verify an archive under explicit [`PackageLimits`].
    pub fn from_bytes_with(
        bytes: &[u8],
        explicit_name: Option<&str>,
        limits: &PackageLimits,
    ) -> Result<Self> {
        if bytes.len() > limits.max_archive_bytes {
            return Err(limit_error(
                "archive bytes",
                limits.max_archive_bytes as u64,
                bytes.len() as u64,
            ));
        }
        let archive_sha256 = sha256_hex(bytes);

        let mut archive =
            ZipArchive::new(Cursor::new(bytes)).map_err(|e| Error::PackageIntegrity {
                message: format!("invalid zip archive: {e}"),
            })?;
        // The zip crate indexes entries by name and silently collapses duplicates, so compare
        // its count with the count the end-of-central-directory record declares.
        let declared = declared_entry_count(bytes).ok_or_else(|| Error::PackageFormat {
            message: "archive has no readable end-of-central-directory record, or uses zip64 (not supported)"
                .to_string(),
        })?;
        if declared != archive.len() {
            return Err(Error::PackageFormat {
                message: format!(
                    "archive declares {declared} entries but only {} have distinct names (duplicate entry names)",
                    archive.len()
                ),
            });
        }
        if archive.len() > limits.max_entries {
            return Err(limit_error(
                "entry count",
                limits.max_entries as u64,
                archive.len() as u64,
            ));
        }

        let raw_files = read_entries(&mut archive, limits)?;
        verify_sums(&raw_files)?;

        // Parse manifest.toml
        let manifest_bytes =
            raw_files
                .get("manifest.toml")
                .ok_or_else(|| Error::PackageFormat {
                    message: "missing manifest.toml in package archive".to_string(),
                })?;
        let manifest_str =
            std::str::from_utf8(manifest_bytes).map_err(|_| Error::PackageFormat {
                message: "manifest.toml is not valid UTF-8".to_string(),
            })?;

        let mod_name = if let Some(name) = explicit_name {
            name.to_string()
        } else {
            let table: toml::Table =
                toml::from_str(manifest_str).map_err(|e| Error::PackageFormat {
                    message: format!("failed to parse manifest.toml: {e}"),
                })?;
            if let Some(tech_name) = table.get("technical_name").and_then(|v| v.as_str()) {
                tech_name.to_string()
            } else if let Some(n) = table.get("name").and_then(|v| v.as_str()) {
                ModuleName::validate(n).map_err(|e| Error::PackageFormat {
                    message: format!(
                        "manifest `name` '{n}' is not a valid technical module name ({e}); specify technical_name"
                    ),
                })?;
                n.to_string()
            } else {
                return Err(Error::PackageFormat {
                    message: "cannot determine technical module name from manifest.toml (missing name or technical_name)".to_string(),
                });
            }
        };
        ModuleName::validate(&mod_name)?;

        let manifest = Manifest::from_toml(&mod_name, manifest_str)?;

        let name_chars = manifest.name().as_str().chars().count();
        if name_chars > MAX_PACKAGE_NAME_CHARS {
            return Err(Error::PackageFormat {
                message: format!(
                    "module name has {name_chars} characters, the package store allows {MAX_PACKAGE_NAME_CHARS}"
                ),
            });
        }
        let version_chars = manifest.version().to_string().chars().count();
        if version_chars > MAX_PACKAGE_VERSION_CHARS {
            return Err(Error::PackageFormat {
                message: format!(
                    "version has {version_chars} characters, the package store allows {MAX_PACKAGE_VERSION_CHARS}"
                ),
            });
        }

        // A `[wasm]` section promises a component at `wasm/<name>.wasm`.
        if manifest.wasm().is_some() {
            let expected = format!("wasm/{}.wasm", manifest.name());
            if !raw_files.contains_key(&expected) {
                return Err(Error::PackageFormat {
                    message: format!(
                        "manifest declares [wasm] but the archive has no '{expected}'"
                    ),
                });
            }
        }

        // Compute client_dist_digest if static/dist/ files exist (BTreeMap iterates sorted).
        let dist_files: Vec<(&String, &Vec<u8>)> = raw_files
            .iter()
            .filter(|(p, _)| p.starts_with("static/dist/"))
            .collect();

        let client_dist_digest = if dist_files.is_empty() {
            None
        } else {
            let mut hasher = Sha256::new();
            hasher.update(b"erpforall/client-dist/v1\n");
            for (p, content) in dist_files {
                hasher.update(p.as_bytes());
                hasher.update(b"\n");
                hasher.update(content.len().to_string().as_bytes());
                hasher.update(b"\n");
                hasher.update(content);
                hasher.update(b"\n");
            }
            Some(format_hex_digest(&hasher.finalize()))
        };

        Ok(Self {
            archive_sha256,
            archive_bytes: bytes.to_vec(),
            manifest,
            files: raw_files,
            client_dist_digest,
        })
    }

    /// Read and parse a package from a file on disk. The file size is checked before reading.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let io_err = |source| Error::Io {
            path: path.to_path_buf(),
            source,
        };
        let len = std::fs::metadata(path).map_err(io_err)?.len();
        if len > MAX_ARCHIVE_BYTES as u64 {
            return Err(limit_error("archive bytes", MAX_ARCHIVE_BYTES as u64, len));
        }
        let bytes = std::fs::read(path).map_err(io_err)?;
        Self::from_bytes(&bytes)
    }

    pub fn archive_sha256(&self) -> &str {
        &self.archive_sha256
    }

    /// The verified archive bytes (what storage persists).
    pub fn archive_bytes(&self) -> &[u8] {
        &self.archive_bytes
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn name(&self) -> &ModuleName {
        self.manifest.name()
    }

    pub fn version(&self) -> &ModuleVersion {
        self.manifest.version()
    }

    pub fn client_dist_digest(&self) -> Option<&str> {
        self.client_dist_digest.as_deref()
    }

    pub fn client_files(&self) -> Vec<&str> {
        self.files
            .keys()
            .filter(|p| p.starts_with("static/dist/"))
            .map(String::as_str)
            .collect()
    }

    pub fn get_file(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    pub fn wasm_binary(&self) -> Option<&[u8]> {
        let expected_path = format!("wasm/{}.wasm", self.name());
        self.get_file(&expected_path)
    }

    pub fn file_paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Check administrator consent for the requested capabilities and the client code dist-digest.
    ///
    /// This only checks; it does not authorise storing or installing. Use
    /// [`ErpAddonPackage::consent`] to obtain the [`ConsentedPackage`] the store requires.
    ///
    /// Refuses loudly if the consent names another addon, if any requested capability is
    /// unapproved, if the consent approves a capability the package does not request (stale
    /// consent), or if client code is present and not acknowledged with the matching digest.
    pub fn verify_admin_consent(&self, consent: &AdminConsent) -> Result<()> {
        if consent.addon_name != *self.manifest.name() {
            return Err(Error::ConsentMismatch {
                addon: self.manifest.name().clone(),
                message: format!(
                    "consent is for addon '{}', the package is '{}'",
                    consent.addon_name,
                    self.manifest.name()
                ),
            });
        }

        // Capability consent gate
        let requested: BTreeSet<String> = match self.manifest.wasm() {
            Some(wasm) => wasm
                .capabilities
                .iter()
                .map(WasmCapability::canonical_str)
                .collect(),
            None => BTreeSet::new(),
        };
        let unapproved: Vec<String> = requested
            .difference(&consent.approved_capabilities)
            .cloned()
            .collect();
        if !unapproved.is_empty() {
            return Err(Error::UnapprovedCapabilities {
                addon: self.manifest.name().clone(),
                unapproved,
            });
        }
        let stale: Vec<&str> = consent
            .approved_capabilities
            .difference(&requested)
            .map(String::as_str)
            .collect();
        if !stale.is_empty() {
            return Err(Error::ConsentMismatch {
                addon: self.manifest.name().clone(),
                message: format!(
                    "consent approves capabilities the package does not request: [{}]",
                    stale.join(", ")
                ),
            });
        }

        // Client code acknowledgement gate
        match (&self.client_dist_digest, &consent.client_dist_digest_ack) {
            (Some(expected), Some(ack)) if ack == expected => {}
            (Some(expected), provided) => {
                return Err(Error::UnacknowledgedClientCode {
                    addon: self.manifest.name().clone(),
                    client_files: self.client_files().into_iter().map(String::from).collect(),
                    expected_digest: expected.clone(),
                    provided_digest: provided.clone(),
                });
            }
            (None, Some(_)) => {
                return Err(Error::ConsentMismatch {
                    addon: self.manifest.name().clone(),
                    message: "consent acknowledges client code but the package has no static/dist/"
                        .to_string(),
                });
            }
            (None, None) => {}
        }

        Ok(())
    }

    /// Consume the package and return the witness that storage and install require.
    ///
    /// This is the only way to build a [`ConsentedPackage`], so consent is structural: no path
    /// reaches [`store_package`] without a successful [`ErpAddonPackage::verify_admin_consent`].
    pub fn consent(self, consent: &AdminConsent) -> Result<ConsentedPackage> {
        self.verify_admin_consent(consent)?;
        Ok(ConsentedPackage { package: self })
    }
}

/// A package whose capabilities and client code the administrator has approved.
///
/// Only [`ErpAddonPackage::consent`] builds one; [`store_package`] takes it by reference.
#[derive(Debug, Clone)]
pub struct ConsentedPackage {
    package: ErpAddonPackage,
}

impl ConsentedPackage {
    /// The approved package.
    pub fn package(&self) -> &ErpAddonPackage {
        &self.package
    }
}

impl std::ops::Deref for ConsentedPackage {
    type Target = ErpAddonPackage;
    fn deref(&self) -> &ErpAddonPackage {
        &self.package
    }
}

/// Read every entry under the bounds, validating paths, entry kinds and name uniqueness.
fn read_entries(
    archive: &mut ZipArchive<Cursor<&[u8]>>,
    limits: &PackageLimits,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut folded: BTreeMap<String, String> = BTreeMap::new();
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| Error::PackageIntegrity {
            message: format!("failed to read zip entry {i}: {e}"),
        })?;
        let raw_name = file
            .name()
            .map_err(|e| Error::PackageIntegrity {
                message: format!("invalid entry name in zip entry {i}: {e}"),
            })?
            .to_string();
        let is_dir = file.is_dir() || raw_name.ends_with('/');
        let path = if is_dir {
            raw_name.trim_end_matches('/')
        } else {
            raw_name.as_str()
        };
        validate_entry_path(path).map_err(|reason| path_error(&raw_name, &reason))?;
        // The client-code consent gate keys on the exact `static/dist/` prefix; any other
        // spelling that folds to it would ship client code past the gate on a case-folding host.
        if (path.to_lowercase() + "/").starts_with("static/dist/")
            && !(path.starts_with("static/dist/") || path == "static/dist")
        {
            return Err(path_error(
                &raw_name,
                "a path that folds to `static/dist/` must be spelled exactly `static/dist/` in lowercase",
            ));
        }
        if file.is_symlink() {
            return Err(Error::PackageFormat {
                message: format!("symlink entry {raw_name:?} is not allowed in a package"),
            });
        }
        if let Some(mode) = file.unix_mode() {
            // Only regular files and directories (type bits 0o100000 / 0o040000, or unset).
            let kind = mode & 0o170_000;
            if !matches!(kind, 0 | 0o100_000 | 0o040_000) {
                return Err(Error::PackageFormat {
                    message: format!(
                        "entry {raw_name:?} has special file type {kind:#o}; only regular files are allowed"
                    ),
                });
            }
        }
        if is_dir {
            continue;
        }

        let declared = file.size();
        if declared > limits.max_entry_bytes {
            return Err(limit_error(
                "entry uncompressed bytes",
                limits.max_entry_bytes,
                declared,
            ));
        }
        // The header size is only a hint to refuse early; the bounded read below is the guard.
        let mut buf = Vec::new();
        (&mut file)
            .take(limits.max_entry_bytes.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|e| Error::PackageIntegrity {
                message: format!("failed to read file {raw_name:?}: {e}"),
            })?;
        let len = buf.len() as u64;
        if len > limits.max_entry_bytes {
            return Err(limit_error(
                "entry uncompressed bytes",
                limits.max_entry_bytes,
                len,
            ));
        }
        total = total
            .checked_add(len)
            .filter(|t| *t <= limits.max_total_bytes)
            .ok_or_else(|| {
                limit_error(
                    "total uncompressed bytes",
                    limits.max_total_bytes,
                    total.saturating_add(len),
                )
            })?;

        if let Some(previous) = folded.insert(path.to_lowercase(), path.to_string()) {
            return Err(Error::PackageFormat {
                message: if previous == path {
                    format!("duplicate entry name {path:?} in package archive")
                } else {
                    format!(
                        "entry names {previous:?} and {path:?} collide on case-insensitive filesystems"
                    )
                },
            });
        }
        files.insert(path.to_string(), buf);
    }

    // Every directory prefix takes part: folded collisions and file-versus-directory clashes
    // (`a` and `a/b`, `Data/x` and `data/y`, `Data` and `data/y`).
    let mut nodes: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for path in files.keys() {
        let mut end = 0;
        let segments: Vec<&str> = path.split('/').collect();
        for (i, segment) in segments.iter().enumerate() {
            end += segment.len();
            let is_file = i + 1 == segments.len();
            let node = &path[..end];
            let key = node.to_lowercase();
            match nodes.get(&key) {
                None => {
                    nodes.insert(key, (node.to_string(), is_file));
                }
                Some((original, was_file)) => {
                    if original != node {
                        return Err(Error::PackageFormat {
                            message: format!(
                                "entry names {original:?} and {node:?} collide on case-insensitive filesystems"
                            ),
                        });
                    }
                    if *was_file != is_file {
                        return Err(Error::PackageFormat {
                            message: format!("entry {node:?} is both a file and a directory"),
                        });
                    }
                }
            }
            end += 1;
        }
    }
    Ok(files)
}

/// Verify every file against `SHA256SUMS` (a self-contained corruption check: the list travels
/// inside the archive, so authenticity needs the externally pinned archive digest or, later, a
/// signature).
fn verify_sums(files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let sums_bytes = files
        .get("SHA256SUMS")
        .ok_or_else(|| Error::PackageIntegrity {
            message: "missing SHA256SUMS in package archive".to_string(),
        })?;
    let sums_text = std::str::from_utf8(sums_bytes).map_err(|_| Error::PackageIntegrity {
        message: "SHA256SUMS is not valid UTF-8".to_string(),
    })?;
    let expected_sums = parse_sha256sums(sums_text)?;

    for (path, content) in files {
        if path == "SHA256SUMS" {
            continue;
        }
        let expected_hash = expected_sums
            .get(path)
            .ok_or_else(|| Error::PackageIntegrity {
                message: format!("file '{path}' in archive is not recorded in SHA256SUMS"),
            })?;
        let actual_hash = sha256_hex(content);
        if actual_hash != *expected_hash {
            return Err(Error::PackageIntegrity {
                message: format!(
                    "checksum mismatch for file '{path}': expected {expected_hash}, got {actual_hash}"
                ),
            });
        }
    }
    for path in expected_sums.keys() {
        if !files.contains_key(path) {
            return Err(Error::PackageIntegrity {
                message: format!("file '{path}' listed in SHA256SUMS is missing from archive"),
            });
        }
    }
    Ok(())
}

/// Builder and writer for `.erpaddon` package archives.
#[derive(Debug, Default)]
pub struct ErpAddonPackageWriter {
    files: BTreeMap<String, Vec<u8>>,
}

impl ErpAddonPackageWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a file. The path is stored exactly as given; [`ErpAddonPackageWriter::build_zip`]
    /// refuses any path the reader would refuse.
    pub fn add_file(&mut self, path: impl Into<String>, content: impl Into<Vec<u8>>) -> &mut Self {
        self.files.insert(path.into(), content.into());
        self
    }

    /// Build a ZIP archive, automatically computing and adding `SHA256SUMS`.
    pub fn build_zip(&self) -> Result<Vec<u8>> {
        let mut with_sums = BTreeMap::new();
        let mut sums_lines = Vec::new();
        for (path, content) in &self.files {
            if path == "SHA256SUMS" {
                continue;
            }
            validate_entry_path(path).map_err(|reason| path_error(path, &reason))?;
            sums_lines.push(format!("{}  {path}\n", sha256_hex(content)));
            with_sums.insert(path.clone(), content.clone());
        }
        sums_lines.sort();
        with_sums.insert("SHA256SUMS".to_string(), sums_lines.concat().into_bytes());

        write_zip_archive(&with_sums)
    }

    /// Build a raw ZIP archive without generating `SHA256SUMS` or validating paths. Exists to
    /// produce deliberately broken archives for failure tests.
    #[doc(hidden)]
    pub fn build_raw_zip(&self) -> Result<Vec<u8>> {
        write_zip_archive(&self.files)
    }
}

fn write_zip_archive(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = ZipWriter::new(&mut cursor);
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (path, content) in files {
            zip.start_file(path, options)
                .map_err(|e| Error::PackageFormat {
                    message: format!("failed to start zip file '{path}': {e}"),
                })?;
            zip.write_all(content).map_err(|e| Error::PackageFormat {
                message: format!("failed to write zip file '{path}': {e}"),
            })?;
        }
        zip.finish().map_err(|e| Error::PackageFormat {
            message: format!("failed to finish zip archive: {e}"),
        })?;
    }
    Ok(cursor.into_inner())
}

// ---- Database Store: ir_module_package -----------------------------------

/// Most rows [`list_packages`] returns per call.
pub const MAX_LIST_PACKAGES: u32 = 1000;

/// The `CREATE TABLE` statement for `ir_module_package` as one literal, for the core DDL registry
/// ([`crate::wasm_lifecycle`] registers it, so the schema owner creates the table with the rest of
/// the core tables and the runtime role never owns it). A unit test keeps it equal to
/// [`create_package_table_sql`], which is built from the size constants.
pub(crate) const PACKAGE_TABLE_DDL: &str = "
CREATE TABLE IF NOT EXISTS ir_module_package (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name VARCHAR(128) NOT NULL,
    version VARCHAR(32) NOT NULL,
    sha256 CHAR(64) NOT NULL,
    package_data BYTEA NOT NULL,
    capabilities JSONB NOT NULL,
    client_dist_digest CHAR(64),
    installed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT ir_module_package_sha256_key UNIQUE (sha256),
    CONSTRAINT ir_module_package_name_version_key UNIQUE (name, version),
    CONSTRAINT ir_module_package_sha256_hex CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    CONSTRAINT ir_module_package_dist_hex
        CHECK (client_dist_digest IS NULL OR client_dist_digest ~ '^[0-9a-f]{64}$'),
    CONSTRAINT ir_module_package_data_size
        CHECK (octet_length(package_data) BETWEEN 1 AND 67108864),
    CONSTRAINT ir_module_package_caps_array CHECK (jsonb_typeof(capabilities) = 'array')
);
REVOKE ALL ON ir_module_package FROM PUBLIC
";

/// The `CREATE TABLE` statement for `ir_module_package`.
///
/// The module system creates the table with the other core tables (`core_ddl!` in
/// [`crate::wasm_lifecycle`]); [`init_package_table`] remains for callers that store packages in a
/// database the module system never initialised (tests of the store alone).
pub fn create_package_table_sql() -> String {
    format!(
        r#"
CREATE TABLE IF NOT EXISTS ir_module_package (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name VARCHAR({MAX_PACKAGE_NAME_CHARS}) NOT NULL,
    version VARCHAR({MAX_PACKAGE_VERSION_CHARS}) NOT NULL,
    sha256 CHAR(64) NOT NULL,
    package_data BYTEA NOT NULL,
    capabilities JSONB NOT NULL,
    client_dist_digest CHAR(64),
    installed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    CONSTRAINT ir_module_package_sha256_key UNIQUE (sha256),
    CONSTRAINT ir_module_package_name_version_key UNIQUE (name, version),
    CONSTRAINT ir_module_package_sha256_hex CHECK (sha256 ~ '^[0-9a-f]{{64}}$'),
    CONSTRAINT ir_module_package_dist_hex
        CHECK (client_dist_digest IS NULL OR client_dist_digest ~ '^[0-9a-f]{{64}}$'),
    CONSTRAINT ir_module_package_data_size
        CHECK (octet_length(package_data) BETWEEN 1 AND {MAX_ARCHIVE_BYTES}),
    CONSTRAINT ir_module_package_caps_array CHECK (jsonb_typeof(capabilities) = 'array')
)
"#
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageRecord {
    pub id: i64,
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub package_data: Vec<u8>,
    pub capabilities: serde_json::Value,
    pub client_dist_digest: Option<String>,
    pub installed_at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageSummary {
    pub id: i64,
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub capabilities: serde_json::Value,
    pub client_dist_digest: Option<String>,
    pub installed_at: SystemTime,
}

const RECORD_COLUMNS: &str =
    "id, name, version, sha256, package_data, capabilities, client_dist_digest, installed_at";

/// Map a full row, re-hashing the blob: a stored archive that no longer matches its content
/// address is corruption and is refused rather than returned.
fn record_from_row(row: &tokio_postgres::Row) -> OrmResult<PackageRecord> {
    let record = PackageRecord {
        id: row.get(0),
        name: row.get(1),
        version: row.get(2),
        sha256: row.get(3),
        package_data: row.get(4),
        capabilities: row.get(5),
        client_dist_digest: row.get(6),
        installed_at: row.get(7),
    };
    let actual = sha256_hex(&record.package_data);
    if actual != record.sha256 {
        return Err(OrmError::programming(format!(
            "ir_module_package row {} is corrupt: stored sha256 {} but the blob hashes to {actual}",
            record.id, record.sha256
        )));
    }
    Ok(record)
}

/// Create the `ir_module_package` table if it does not exist (see [`create_package_table_sql`]
/// for the migration-wiring status).
pub async fn init_package_table(tx: &mut Transaction) -> OrmResult<()> {
    tx.batch(&create_package_table_sql()).await?;
    Ok(())
}

/// Store a consented `.erpaddon` package content-addressed by its SHA-256.
///
/// The blob written is the verified package's own archive bytes, and consent is required by the
/// argument type. An existing row is never modified: storing the same archive again returns the
/// existing record, and a different archive claiming an existing `(name, version)` is refused.
pub async fn store_package(
    tx: &mut Transaction,
    package: &ConsentedPackage,
) -> OrmResult<PackageRecord> {
    let name = package.name().as_str();
    let version = package.version().to_string();
    let sha256 = package.archive_sha256();
    let capability_names: Vec<String> = match package.manifest().wasm() {
        Some(wasm) => wasm
            .capabilities
            .iter()
            .map(WasmCapability::canonical_str)
            .collect(),
        None => Vec::new(),
    };
    let capabilities_json = serde_json::to_value(capability_names).map_err(|e| {
        OrmError::programming(format!("failed to serialize capabilities to JSON: {e}"))
    })?;
    let client_dist_digest = package.client_dist_digest();

    let existing = tx
        .query(
            "SELECT sha256 FROM ir_module_package WHERE name = $1 AND version = $2",
            &[&name, &version],
        )
        .await?;
    if let Some(row) = existing.first() {
        let other: String = row.get(0);
        if other != sha256 {
            return Err(OrmError::conflict(format!(
                "package {name} {version} is already stored with sha256 {other}; refusing a different archive ({sha256}) under the same name and version"
            )));
        }
    }

    let insert = format!(
        "INSERT INTO ir_module_package (name, version, sha256, package_data, capabilities, client_dist_digest) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (sha256) DO NOTHING RETURNING {RECORD_COLUMNS}"
    );
    let package_data = package.archive_bytes();
    let rows = tx
        .query(
            &insert,
            &[
                &name,
                &version,
                &sha256,
                &package_data,
                &capabilities_json,
                &client_dist_digest,
            ],
        )
        .await?;
    if let Some(row) = rows.first() {
        return record_from_row(row);
    }
    // Conflict on sha256: the archive is already stored; return it unchanged.
    get_package_by_sha256(tx, sha256).await?.ok_or_else(|| {
        OrmError::programming("ir_module_package insert conflicted but the row is not visible")
    })
}

/// Retrieve a package record by its unique SHA-256 content address.
pub async fn get_package_by_sha256(
    tx: &mut Transaction,
    sha256: &str,
) -> OrmResult<Option<PackageRecord>> {
    let sql = format!("SELECT {RECORD_COLUMNS} FROM ir_module_package WHERE sha256 = $1");
    let rows = tx.query(&sql, &[&sha256]).await?;
    rows.first().map(record_from_row).transpose()
}

/// Retrieve the package stored under an exact `(name, version)`.
pub async fn get_package_by_name_version(
    tx: &mut Transaction,
    name: &str,
    version: &str,
) -> OrmResult<Option<PackageRecord>> {
    let sql =
        format!("SELECT {RECORD_COLUMNS} FROM ir_module_package WHERE name = $1 AND version = $2");
    let rows = tx.query(&sql, &[&name, &version]).await?;
    rows.first().map(record_from_row).transpose()
}

/// Retrieve the most recently stored package (highest row id) for a module technical name.
///
/// "Latest" is defined by insertion order, which re-storing an archive never changes; use
/// [`get_package_by_name_version`] to address a specific version.
pub async fn get_package_by_name(
    tx: &mut Transaction,
    name: &str,
) -> OrmResult<Option<PackageRecord>> {
    let sql = format!(
        "SELECT {RECORD_COLUMNS} FROM ir_module_package WHERE name = $1 ORDER BY id DESC LIMIT 1"
    );
    let rows = tx.query(&sql, &[&name]).await?;
    rows.first().map(record_from_row).transpose()
}

/// List stored packages (without the BYTEA payload), ordered by name, version, id.
///
/// `limit` must be between 1 and [`MAX_LIST_PACKAGES`]; page with `offset`.
pub async fn list_packages(
    tx: &mut Transaction,
    limit: u32,
    offset: u32,
) -> OrmResult<Vec<PackageSummary>> {
    if limit == 0 || limit > MAX_LIST_PACKAGES {
        return Err(OrmError::validation(format!(
            "list_packages limit must be between 1 and {MAX_LIST_PACKAGES} (got {limit})"
        )));
    }
    let sql = r#"
SELECT id, name, version, sha256, capabilities, client_dist_digest, installed_at
FROM ir_module_package
ORDER BY name ASC, version ASC, id ASC
LIMIT $1 OFFSET $2
"#;
    let rows = tx
        .query(sql, &[&i64::from(limit), &i64::from(offset)])
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| PackageSummary {
            id: row.get(0),
            name: row.get(1),
            version: row.get(2),
            sha256: row.get(3),
            capabilities: row.get(4),
            client_dist_digest: row.get(5),
            installed_at: row.get(6),
        })
        .collect())
}

/// Delete a package record from the store by its SHA-256.
pub async fn delete_package(tx: &mut Transaction, sha256: &str) -> OrmResult<bool> {
    let sql = "DELETE FROM ir_module_package WHERE sha256 = $1";
    let count = tx.execute(sql, &[&sha256]).await?;
    Ok(count > 0)
}
