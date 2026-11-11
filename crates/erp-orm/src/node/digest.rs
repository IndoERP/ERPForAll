//! Configuration, build and addon digests (design §8, spec §4.6).
//!
//! Every part is a SHA-256 over a canonical encoding: each item is a 1-byte tag, a 4-byte
//! big-endian length and the bytes (`u` u64, `b` bool, `s` UTF-8, `d` milliseconds, `k` an HMAC of
//! a shared secret, `h` a nested digest, `l` a count followed by that many entries). Names and
//! sections are sorted bytewise. The version label in every part changes when the encoding does.
//!
//! The digests are not secret, and they hide nothing but shared secrets: a section digest (also
//! stored per section in `erp_node_detail`, readable by the heartbeat role) is a plain SHA-256
//! of the values, so a value with few possible settings (a flag, a limit) can be found by trying
//! them. Only [`SettingsWriter::shared_secret`] hides a value, through an HMAC keyed by the
//! secret, and only when the secret is a generated key, not a passphrase.

use std::fmt;
use std::io::Read;
use std::str::FromStr;
use std::time::Duration;

use hmac::{KeyInit, Mac};
use sha2::{Digest as _, Sha256};

use super::NodeError;

/// Version label of the combined digest.
pub(crate) const LABEL: &[u8] = b"erpforall/node-digest/v1";
const SECTION_LABEL: &str = "erpforall/node-digest/v1/section";
const CONFIG_LABEL: &str = "erpforall/node-digest/v1/config";
const SECRET_LABEL: &str = "erpforall/node-digest/v1/secret";
const BUILD_LABEL: &str = "erpforall/node-digest/v1/build";
const ADDONS_LABEL: &str = "erpforall/node-digest/v1/addons";

/// Shortest shared secret, in bytes (design §8.2). A length, not a strength check: a shared
/// secret must be a generated key (random bytes), never a passphrase, which could be guessed
/// offline from the digest.
pub const MIN_SHARED_SECRET_BYTES: usize = 32;
/// Longest shared string setting, in bytes.
pub const MAX_SETTING_STR_BYTES: usize = 64 * 1024;
/// Most files of one dev folder (design §8.4).
pub const MAX_TREE_FILES: u64 = 20_000;
/// Most bytes of one dev folder (design §8.4).
pub const MAX_TREE_BYTES: u64 = 1024 * 1024 * 1024;
/// Deepest directory nesting of one dev folder (design §8.4).
pub const MAX_TREE_DEPTH: usize = 32;
/// Most entries (files and directories) of one dev folder (design §8.4).
pub const MAX_TREE_ENTRIES: u64 = 40_000;
/// Most bytes of the executable hashed for a dirty or unknown build.
const MAX_EXECUTABLE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// A SHA-256 digest; `Display` is lowercase hex.
///
/// ```
/// use erp_orm::node::Digest;
/// let d: Digest = "00".repeat(32).parse()?;
/// assert_eq!(d.to_string(), "00".repeat(32));
/// assert!("0".parse::<Digest>().is_err());
/// # Ok::<(), erp_orm::node::NodeError>(())
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; 32]);

impl Digest {
    /// The digest's bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// A digest from its bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// A digest from a slice of exactly 32 bytes.
    pub(crate) fn from_slice(bytes: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(bytes).ok().map(Self)
    }

    fn of(hasher: Sha256) -> Self {
        Self(hasher.finalize().into())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({self})")
    }
}

impl FromStr for Digest {
    type Err = NodeError;

    fn from_str(s: &str) -> Result<Self, NodeError> {
        let refuse = || {
            NodeError::invalid_setting(
                "digest",
                format!("{s:?} is not a digest: 64 hexadecimal digits expected"),
            )
        };
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err(refuse());
        }
        let mut out = [0u8; 32];
        for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
            let hi = hex_value(pair[0]).ok_or_else(refuse)?;
            let lo = hex_value(pair[1]).ok_or_else(refuse)?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Self(out))
    }
}

pub(super) fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Item tags of the encoding.
pub(crate) mod tag {
    pub(crate) const U64: u8 = b'u';
    pub(crate) const BOOL: u8 = b'b';
    pub(crate) const STR: u8 = b's';
    pub(crate) const DURATION: u8 = b'd';
    pub(crate) const SECRET: u8 = b'k';
    pub(crate) const HASH: u8 = b'h';
    pub(crate) const LIST: u8 = b'l';
}

/// The canonical encoding, written into a buffer.
#[derive(Default)]
pub(crate) struct Enc {
    pub(crate) buf: Vec<u8>,
}

impl Enc {
    fn item(&mut self, tag: u8, bytes: &[u8]) -> Result<(), NodeError> {
        let len = u32::try_from(bytes.len()).map_err(|_| {
            NodeError::invalid_setting(
                "digest",
                format!("an encoded item of {} bytes is too long", bytes.len()),
            )
        })?;
        self.buf.push(tag);
        self.buf.extend_from_slice(&len.to_be_bytes());
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    pub(crate) fn str(&mut self, s: &str) -> Result<(), NodeError> {
        self.item(tag::STR, s.as_bytes())
    }

    pub(crate) fn hash(&mut self, d: &Digest) -> Result<(), NodeError> {
        self.item(tag::HASH, &d.0)
    }

    pub(crate) fn list(&mut self, count: usize) -> Result<(), NodeError> {
        let n = u32::try_from(count).map_err(|_| {
            NodeError::invalid_setting("digest", format!("a list of {count} entries is too long"))
        })?;
        self.item(tag::LIST, &n.to_be_bytes())
    }

    fn value(&mut self, value: &Value) -> Result<(), NodeError> {
        match value {
            Value::U64(v) => self.item(tag::U64, &v.to_be_bytes()),
            Value::Bool(v) => self.item(tag::BOOL, &[u8::from(*v)]),
            Value::Str(v) => self.item(tag::STR, v.as_bytes()),
            Value::Millis(v) => self.item(tag::DURATION, &v.to_be_bytes()),
            Value::Secret(v) => self.item(tag::SECRET, v),
        }
    }

    fn digest(&self) -> Digest {
        Digest::of(Sha256::new_with_prefix(&self.buf))
    }
}

/// Reads the canonical encoding back (detail payloads of peers: bounded by the table's 1 MiB
/// `CHECK`, every length checked against what is left).
pub(crate) struct Dec<'a> {
    rest: &'a [u8],
}

impl<'a> Dec<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    fn item(&mut self, want: u8) -> Option<&'a [u8]> {
        let (&tag, rest) = self.rest.split_first()?;
        if tag != want || rest.len() < 4 {
            return None;
        }
        let len = usize::try_from(u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]])).ok()?;
        let body = rest.get(4..4usize.checked_add(len)?)?;
        self.rest = &rest[4 + len..];
        Some(body)
    }

    pub(crate) fn str(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.item(tag::STR)?).ok()
    }

    pub(crate) fn hash(&mut self) -> Option<Digest> {
        Digest::from_slice(self.item(tag::HASH)?)
    }

    pub(crate) fn list(&mut self) -> Option<usize> {
        let b = self.item(tag::LIST)?;
        let n = u32::from_be_bytes(<[u8; 4]>::try_from(b).ok()?);
        usize::try_from(n).ok()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }
}

/// The name of a settings section: `[a-z][a-z0-9_]{0,31}`, checked when the section is
/// described.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SectionName(&'static str);

impl SectionName {
    /// A section name (validated when the section is described: an invalid name is
    /// [`NodeError::InvalidSetting`]).
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// The name.
    pub fn as_str(&self) -> &'static str {
        self.0
    }

    fn check(&self) -> Result<(), NodeError> {
        let b = self.0.as_bytes();
        let ok = !b.is_empty()
            && b.len() <= 32
            && b[0].is_ascii_lowercase()
            && b.iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_');
        if ok {
            Ok(())
        } else {
            Err(NodeError::invalid_setting(
                self.0,
                format!(
                    "invalid settings section name {:?}: 1 to 32 bytes of [a-z0-9_], starting \
                     with a letter",
                    self.0
                ),
            ))
        }
    }
}

impl fmt::Display for SectionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

fn check_setting_name(name: &str) -> Result<(), NodeError> {
    let b = name.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'.' | b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(NodeError::invalid_setting(
            name,
            format!(
                "invalid setting name {name:?}: 1 to 128 bytes of [a-z0-9._-], starting with a \
                 letter"
            ),
        ))
    }
}

/// Whether a setting must be equal on every replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Scope {
    /// Equal on every replica: it enters the digest.
    Shared,
    /// May differ per process (how it reaches shared services, its own capacity and placement,
    /// its observability; design §8.2): recorded by name only.
    Node,
}

/// One setting a section declared: its name and scope, never its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredSetting {
    name: String,
    scope: Scope,
}

impl DeclaredSetting {
    /// The setting's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its scope.
    pub fn scope(&self) -> Scope {
        self.scope
    }
}

enum Value {
    U64(u64),
    Bool(bool),
    Str(String),
    Millis(u64),
    Secret([u8; 32]),
}

/// Collects the settings of one section ([`SharedSettings::describe`]). Every setting is declared
/// exactly once, with its scope; a shared secret enters as an HMAC keyed by the secret.
pub struct SettingsWriter<'a> {
    section: SectionName,
    items: &'a mut Vec<(String, Option<Value>)>,
}

impl SettingsWriter<'_> {
    /// A valid name that starts with the section and a dot, so a report that names a section
    /// names where the setting lives.
    fn check_name(&self, name: &str) -> Result<(), NodeError> {
        check_setting_name(name)?;
        let section = self.section.0;
        let in_section = name
            .strip_prefix(section)
            .is_some_and(|rest| rest.starts_with('.'));
        if in_section {
            Ok(())
        } else {
            Err(NodeError::invalid_setting(
                name,
                format!(
                    "setting {name} is declared in section {section}: a setting's name starts \
                     with its section and a dot ({section}.)"
                ),
            ))
        }
    }

    fn push(&mut self, name: &str, value: Option<Value>) -> Result<(), NodeError> {
        self.check_name(name)?;
        self.items.push((name.to_owned(), value));
        Ok(())
    }

    /// A shared unsigned integer.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for an invalid name.
    pub fn shared_u64(&mut self, name: &str, value: u64) -> Result<(), NodeError> {
        self.push(name, Some(Value::U64(value)))
    }

    /// A shared boolean.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for an invalid name.
    pub fn shared_bool(&mut self, name: &str, value: bool) -> Result<(), NodeError> {
        self.push(name, Some(Value::Bool(value)))
    }

    /// A shared string (at most [`MAX_SETTING_STR_BYTES`]).
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for an invalid name or a longer value.
    pub fn shared_str(&mut self, name: &str, value: &str) -> Result<(), NodeError> {
        if value.len() > MAX_SETTING_STR_BYTES {
            return Err(NodeError::invalid_setting(
                name,
                format!(
                    "setting {name} is {} bytes long; a shared string setting holds at most \
                     {MAX_SETTING_STR_BYTES} bytes",
                    value.len()
                ),
            ));
        }
        self.push(name, Some(Value::Str(value.to_owned())))
    }

    /// A shared duration, in whole milliseconds.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for an invalid name or a duration beyond `u64` milliseconds.
    pub fn shared_duration(&mut self, name: &str, value: Duration) -> Result<(), NodeError> {
        let ms = u64::try_from(value.as_millis()).map_err(|_| {
            NodeError::invalid_setting(name, format!("setting {name}: duration too long"))
        })?;
        self.push(name, Some(Value::Millis(ms)))
    }

    /// A shared secret (a generated key of at least [`MIN_SHARED_SECRET_BYTES`] random bytes,
    /// never a passphrase): only `HMAC-SHA256(key = secret, message = label ‖ section ‖ name)`
    /// enters the digest. The only setting kind whose value the digest hides.
    ///
    /// # Errors
    ///
    /// [`NodeError::ShortSharedSecret`] for a shorter secret; [`NodeError::InvalidSetting`] for an
    /// invalid name.
    pub fn shared_secret(&mut self, name: &str, secret: &[u8]) -> Result<(), NodeError> {
        self.check_name(name)?;
        if secret.len() < MIN_SHARED_SECRET_BYTES {
            return Err(NodeError::ShortSharedSecret {
                section: self.section.0.to_owned(),
                name: name.to_owned(),
                len: secret.len(),
            });
        }
        let mut message = Enc::default();
        message.str(SECRET_LABEL)?;
        message.str(self.section.0)?;
        message.str(name)?;
        let mut mac = <hmac::Hmac<Sha256> as KeyInit>::new_from_slice(secret).map_err(|_| {
            NodeError::invalid_setting(name, format!("setting {name}: HMAC key refused"))
        })?;
        mac.update(&message.buf);
        let tag: [u8; 32] = mac.finalize().into_bytes().into();
        self.push(name, Some(Value::Secret(tag)))
    }

    /// A node setting: recorded by name only, never in a digest.
    ///
    /// # Errors
    ///
    /// [`NodeError::InvalidSetting`] for an invalid name.
    pub fn node(&mut self, name: &str) -> Result<(), NodeError> {
        self.push(name, None)
    }
}

/// A settings type whose values are classified `shared` or `node`: the
/// extension point every crate that owns server settings implements.
///
/// Destructure the settings struct without `..` inside [`describe`](Self::describe), so a new
/// field that is not described fails to compile.
///
/// ```
/// use erp_orm::node::{NodeError, SectionName, SettingsWriter, SharedSettings};
/// struct Limits { max_rows: u64, worker_threads: u64 }
/// impl SharedSettings for Limits {
///     fn section(&self) -> SectionName { SectionName::new("limits") }
///     fn describe(&self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError> {
///         let Self { max_rows, worker_threads: _ } = self;
///         out.shared_u64("limits.max_rows", *max_rows)?;
///         out.node("limits.worker_threads")
///     }
/// }
/// let digest = erp_orm::node::SectionDigest::of(&Limits { max_rows: 10, worker_threads: 4 })?;
/// assert_eq!(digest.settings().len(), 2);
/// # Ok::<(), NodeError>(())
/// ```
pub trait SharedSettings {
    /// The section these settings belong to.
    fn section(&self) -> SectionName;

    /// Declare every setting once: `shared_*` with its value, or `node`.
    ///
    /// # Errors
    ///
    /// The writer's errors.
    fn describe(&self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError>;
}

/// The digest of one settings section and what it declared.
#[derive(Clone, PartialEq, Eq)]
pub struct SectionDigest {
    name: SectionName,
    digest: Digest,
    settings: Vec<DeclaredSetting>,
}

impl fmt::Debug for SectionDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SectionDigest")
            .field("name", &self.name.0)
            .field("digest", &self.digest)
            .field("settings", &self.settings)
            .finish()
    }
}

impl SectionDigest {
    /// Describe `settings` and digest its shared values.
    ///
    /// # Errors
    ///
    /// [`NodeError::DuplicateSetting`] when a setting is declared twice; the section name's and
    /// the writer's errors.
    pub fn of(settings: &dyn SharedSettings) -> Result<Self, NodeError> {
        let name = settings.section();
        name.check()?;
        let mut items = Vec::new();
        settings.describe(&mut SettingsWriter {
            section: name,
            items: &mut items,
        })?;
        items.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        if let Some(pair) = items.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(NodeError::DuplicateSetting {
                section: name.0.to_owned(),
                name: Some(pair[0].0.clone()),
            });
        }
        let encoded = encode_section(name, &items)?;
        Ok(Self {
            name,
            digest: encoded.digest(),
            settings: items
                .into_iter()
                .map(|(name, value)| DeclaredSetting {
                    name,
                    scope: if value.is_some() {
                        Scope::Shared
                    } else {
                        Scope::Node
                    },
                })
                .collect(),
        })
    }

    /// The section.
    pub fn name(&self) -> SectionName {
        self.name
    }

    /// Its digest.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The settings it declared, sorted by name.
    pub fn settings(&self) -> &[DeclaredSetting] {
        &self.settings
    }
}

fn encode_section(name: SectionName, items: &[(String, Option<Value>)]) -> Result<Enc, NodeError> {
    let mut enc = Enc::default();
    enc.str(SECTION_LABEL)?;
    enc.str(name.0)?;
    let shared: Vec<(&String, &Value)> = items
        .iter()
        .filter_map(|(n, v)| v.as_ref().map(|v| (n, v)))
        .collect();
    enc.list(shared.len())?;
    for (n, v) in shared {
        enc.str(n)?;
        enc.value(v)?;
    }
    Ok(enc)
}

/// The configuration digest: every described section, sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDigest {
    digest: Digest,
    sections: Vec<SectionDigest>,
}

impl ConfigDigest {
    /// Combine section digests.
    ///
    /// # Errors
    ///
    /// [`NodeError::DuplicateSetting`] when a section appears twice.
    pub fn from_sections(mut sections: Vec<SectionDigest>) -> Result<Self, NodeError> {
        sections.sort_by(|a, b| a.name.0.as_bytes().cmp(b.name.0.as_bytes()));
        if let Some(pair) = sections.windows(2).find(|w| w[0].name == w[1].name) {
            return Err(NodeError::DuplicateSetting {
                section: pair[0].name.0.to_owned(),
                name: None,
            });
        }
        let mut enc = Enc::default();
        enc.str(CONFIG_LABEL)?;
        enc.list(sections.len())?;
        for s in &sections {
            enc.str(s.name.0)?;
            enc.hash(&s.digest)?;
        }
        Ok(Self {
            digest: enc.digest(),
            sections,
        })
    }

    /// The digest.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The sections, sorted by name.
    pub fn sections(&self) -> &[SectionDigest] {
        &self.sections
    }

    /// One section.
    pub fn section(&self, name: &str) -> Option<&SectionDigest> {
        self.sections.iter().find(|s| s.name.0 == name)
    }

    /// The `config_sections` detail payload: `l<n> ‖ (s<section> ‖ h<digest>)…`.
    pub(crate) fn detail(&self) -> Result<Vec<u8>, NodeError> {
        let mut enc = Enc::default();
        enc.list(self.sections.len())?;
        for s in &self.sections {
            enc.str(s.name.0)?;
            enc.hash(&s.digest)?;
        }
        Ok(enc.buf)
    }
}

/// How a build is identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BuildKind {
    /// A clean build of a known commit: the digest covers package, version, profile, commit and
    /// native addons.
    Commit,
    /// An unknown commit or a dirty tree: the digest is the SHA-256 of the running executable.
    Executable,
}

/// What the link root knows about its build ([`build_identity!`](crate::build_identity)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildIdentity {
    package: String,
    version: String,
    debug: bool,
    commit: Option<String>,
    dirty: Option<String>,
    native: Vec<(String, String)>,
}

impl BuildIdentity {
    /// Used by [`build_identity!`](crate::build_identity); call the macro instead.
    #[doc(hidden)]
    pub fn __from_build(
        package: &str,
        version: &str,
        debug: bool,
        commit: Option<&str>,
        dirty: Option<&str>,
    ) -> Self {
        Self {
            package: package.to_owned(),
            version: version.to_owned(),
            debug,
            commit: commit.map(str::to_owned),
            dirty: dirty.map(str::to_owned),
            native: Vec::new(),
        }
    }

    /// Add a native addon linked into the binary (name and version, from the addon set).
    #[must_use]
    pub fn native_addon(mut self, name: &str, version: &str) -> Self {
        self.native.push((name.to_owned(), version.to_owned()));
        self
    }

    fn profile(&self) -> &'static str {
        if self.debug { "debug" } else { "release" }
    }

    /// Whether the tree was dirty: `None` when the build environment did not say.
    fn is_dirty(&self) -> Result<Option<bool>, NodeError> {
        match self.dirty.as_deref() {
            None | Some("") => Ok(None),
            Some("0" | "false") => Ok(Some(false)),
            Some("1" | "true") => Ok(Some(true)),
            Some(other) => Err(NodeError::BuildIdentity {
                message: format!(
                    "ERP_BUILD_DIRTY = {other:?}: expected 0, 1, false, true or empty"
                ),
                source: None,
            }),
        }
    }

    /// Resolve the digest: the commit when the tree is clean and the commit known, the running
    /// executable's SHA-256 otherwise (read once, at most 4 GiB).
    ///
    /// # Errors
    ///
    /// [`NodeError::BuildIdentity`] for a malformed commit or dirty flag, for a commit without a
    /// dirty flag (two local builds of one commit would share a digest), or when the executable
    /// cannot be read.
    pub fn resolve(&self) -> Result<ResolvedBuild, NodeError> {
        let dirty = self.is_dirty()?;
        let commit = match self.commit.as_deref() {
            None | Some("") => None,
            Some(c) => {
                let ok = (7..=64).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_hexdigit());
                if !ok {
                    return Err(NodeError::BuildIdentity {
                        message: format!(
                            "ERP_BUILD_COMMIT = {c:?}: expected 7 to 64 hexadecimal digits"
                        ),
                        source: None,
                    });
                }
                Some(c.to_ascii_lowercase())
            }
        };
        let target = target();
        match (commit, dirty) {
            (Some(_), None) => Err(NodeError::BuildIdentity {
                message: "ERP_BUILD_COMMIT is set but ERP_BUILD_DIRTY is not: set ERP_BUILD_DIRTY \
                          to 0 for a clean tree or 1 for a dirty one (without it two local builds \
                          of one commit would share a digest)"
                    .to_owned(),
                source: None,
            }),
            (Some(commit), Some(false)) => {
                let mut native = self.native.clone();
                native.sort();
                if let Some(pair) = native.windows(2).find(|w| w[0].0 == w[1].0) {
                    return Err(NodeError::BuildIdentity {
                        message: format!("native addon {} is listed twice", pair[0].0),
                        source: None,
                    });
                }
                let mut enc = Enc::default();
                enc.str(BUILD_LABEL)?;
                enc.str(&self.package)?;
                enc.str(&self.version)?;
                enc.str(self.profile())?;
                enc.str(&commit)?;
                enc.list(native.len())?;
                for (name, version) in &native {
                    enc.str(name)?;
                    enc.str(version)?;
                }
                let short = commit.get(..12).unwrap_or(&commit);
                Ok(ResolvedBuild {
                    digest: enc.digest(),
                    label: self.label(short, target),
                    kind: BuildKind::Commit,
                    target,
                })
            }
            _ => {
                let digest = executable_digest()?;
                let hex = digest.to_string();
                let short = format!("exe:{}", hex.get(..12).unwrap_or(&hex));
                Ok(ResolvedBuild {
                    digest,
                    label: self.label(&short, target),
                    kind: BuildKind::Executable,
                    target,
                })
            }
        }
    }

    fn label(&self, id: &str, target: &str) -> String {
        let mut label = format!(
            "{} {} {} {id} {target}",
            self.package,
            self.version,
            self.profile()
        );
        // the column holds 1 to 128 bytes; a longer package name or version is cut at a char
        // boundary (the label is for people, the digest is the identity)
        while label.len() > 128 {
            label.pop();
        }
        label
    }
}

fn target() -> &'static str {
    // `<arch>-<os>` for the targets we ship; others report the architecture. Reported, never
    // compared (lead decision 1)
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-linux",
        ("aarch64", "linux") => "aarch64-linux",
        ("aarch64", "macos") => "aarch64-macos",
        ("x86_64", "macos") => "x86_64-macos",
        (arch, _) => arch,
    }
}

/// The SHA-256 of the running executable.
pub(crate) fn executable_digest() -> Result<Digest, NodeError> {
    let io = |what: &str, e: std::io::Error| NodeError::BuildIdentity {
        message: format!("{what}: {e}"),
        source: Some(e),
    };
    let path =
        std::env::current_exe().map_err(|e| io("cannot locate the running executable", e))?;
    let file = std::fs::File::open(&path)
        .map_err(|e| io(&format!("cannot open the executable {}", path.display()), e))?;
    let mut hasher = Sha256::new();
    let mut reader = file.take(MAX_EXECUTABLE_BYTES + 1);
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| io(&format!("cannot read the executable {}", path.display()), e))?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if total > MAX_EXECUTABLE_BYTES {
            return Err(NodeError::BuildIdentity {
                message: format!(
                    "the executable {} is larger than {MAX_EXECUTABLE_BYTES} bytes",
                    path.display()
                ),
                source: None,
            });
        }
        hasher.update(&buf[..n]);
    }
    Ok(Digest::of(hasher))
}

/// A resolved build identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBuild {
    digest: Digest,
    label: String,
    kind: BuildKind,
    target: &'static str,
}

impl ResolvedBuild {
    /// The build digest.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The label recorded in `erp_node.build_label` (package, version, profile, commit or
    /// executable digest prefix, target).
    pub fn label(&self) -> &str {
        &self.label
    }

    /// How the build is identified.
    pub fn kind(&self) -> BuildKind {
        self.kind
    }

    /// The target (`<arch>-<os>`), reported and never compared.
    pub fn target(&self) -> &'static str {
        self.target
    }

    /// The `build` detail payload: `s<label> ‖ s<target> ‖ s<kind>`.
    pub(crate) fn detail(&self) -> Result<Vec<u8>, NodeError> {
        let mut enc = Enc::default();
        enc.str(&self.label)?;
        enc.str(self.target)?;
        enc.str(match self.kind {
            BuildKind::Commit => "commit",
            BuildKind::Executable => "executable",
        })?;
        Ok(enc.buf)
    }
}

/// Where an addon's content comes from (design §8.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AddonContent {
    /// Compiled into the binary: covered by the build digest.
    Embedded,
    /// A package read from a local file, by its SHA-256.
    Package(Digest),
    /// An `erp dev` folder, by the digest of its files ([`TreeHasher`]).
    DevFolder(Digest),
}

impl AddonContent {
    fn kind(&self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Package(_) => "package",
            Self::DevFolder(_) => "dev_folder",
        }
    }
}

/// One addon of an [`AddonsDigest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonEntryDigest {
    name: String,
    version: String,
    content: AddonContent,
}

impl AddonEntryDigest {
    /// The addon's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its manifest version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Its content.
    pub fn content(&self) -> AddonContent {
        self.content
    }
}

/// The digest of every addon source the process holds locally
/// (`erp_modules::AddonIndex::digest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonsDigest {
    digest: Digest,
    entries: Vec<AddonEntryDigest>,
}

impl AddonsDigest {
    /// Start an empty list.
    pub fn builder() -> AddonsDigestBuilder {
        AddonsDigestBuilder {
            entries: Vec::new(),
        }
    }

    /// The digest.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The addons, sorted by name.
    pub fn entries(&self) -> &[AddonEntryDigest] {
        &self.entries
    }

    /// The `addons` detail payload.
    pub(crate) fn detail(&self) -> Result<Vec<u8>, NodeError> {
        let mut enc = Enc::default();
        encode_addons(&mut enc, &self.entries)?;
        Ok(enc.buf)
    }
}

fn encode_addons(enc: &mut Enc, entries: &[AddonEntryDigest]) -> Result<(), NodeError> {
    enc.list(entries.len())?;
    for e in entries {
        enc.str(&e.name)?;
        enc.str(&e.version)?;
        enc.str(e.content.kind())?;
        match &e.content {
            AddonContent::Embedded => enc.str("")?,
            AddonContent::Package(d) | AddonContent::DevFolder(d) => enc.hash(d)?,
        }
    }
    Ok(())
}

/// Builds an [`AddonsDigest`].
#[derive(Debug)]
pub struct AddonsDigestBuilder {
    entries: Vec<AddonEntryDigest>,
}

impl AddonsDigestBuilder {
    /// Add an addon.
    ///
    /// # Errors
    ///
    /// [`NodeError::AddonSource`] for an empty or over-long name (128 bytes) or version (64
    /// bytes).
    pub fn addon(
        mut self,
        name: &str,
        version: &str,
        content: AddonContent,
    ) -> Result<Self, NodeError> {
        if name.is_empty() || name.len() > 128 {
            return Err(NodeError::AddonSource {
                addon: name.to_owned(),
                message: "the name must be 1 to 128 bytes".to_owned(),
            });
        }
        if version.is_empty() || version.len() > 64 {
            return Err(NodeError::AddonSource {
                addon: name.to_owned(),
                message: format!("the version {version:?} must be 1 to 64 bytes"),
            });
        }
        self.entries.push(AddonEntryDigest {
            name: name.to_owned(),
            version: version.to_owned(),
            content,
        });
        Ok(self)
    }

    /// Sort, check and digest.
    ///
    /// # Errors
    ///
    /// [`NodeError::AddonSource`] when an addon is listed twice.
    pub fn build(mut self) -> Result<AddonsDigest, NodeError> {
        self.entries
            .sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        if let Some(pair) = self.entries.windows(2).find(|w| w[0].name == w[1].name) {
            return Err(NodeError::AddonSource {
                addon: pair[0].name.clone(),
                message: "listed twice".to_owned(),
            });
        }
        let mut enc = Enc::default();
        enc.str(ADDONS_LABEL)?;
        encode_addons(&mut enc, &self.entries)?;
        Ok(AddonsDigest {
            digest: enc.digest(),
            entries: self.entries,
        })
    }
}

/// The content digest of a dev folder: every regular file as (relative path, size, bytes), fed
/// in strictly increasing path order; at most [`MAX_TREE_FILES`] files and [`MAX_TREE_BYTES`]
/// bytes. The walker (in `erp-modules`) refuses symlinks and enforces [`MAX_TREE_DEPTH`].
pub struct TreeHasher {
    hasher: Sha256,
    files: u64,
    bytes: u64,
    last: Option<String>,
    addon: String,
}

impl TreeHasher {
    /// Start hashing the folder of `addon` (named in errors).
    pub fn new(addon: &str) -> Self {
        Self {
            hasher: Sha256::new_with_prefix(b"erpforall/node-digest/v1/tree"),
            files: 0,
            bytes: 0,
            last: None,
            addon: addon.to_owned(),
        }
    }

    fn refuse(&self, message: String) -> NodeError {
        NodeError::AddonSource {
            addon: self.addon.clone(),
            message,
        }
    }

    /// Hash one file: `relative` uses `/` separators; exactly `size` bytes are read from
    /// `content`.
    ///
    /// # Errors
    ///
    /// [`NodeError::AddonSource`] when a bound is exceeded, the order is not strictly increasing,
    /// or the file does not hold `size` bytes (it changed while it was read).
    pub fn file(
        &mut self,
        relative: &str,
        size: u64,
        content: &mut dyn Read,
    ) -> Result<(), NodeError> {
        if let Some(last) = &self.last
            && last.as_bytes() >= relative.as_bytes()
        {
            return Err(self.refuse(format!(
                "dev folder files must be hashed in increasing path order ({last} before \
                 {relative})"
            )));
        }
        self.files += 1;
        if self.files > MAX_TREE_FILES {
            return Err(self.refuse(format!(
                "the dev folder holds more than {MAX_TREE_FILES} files"
            )));
        }
        self.bytes = self.bytes.saturating_add(size);
        if self.bytes > MAX_TREE_BYTES {
            return Err(self.refuse(format!(
                "the dev folder holds more than {MAX_TREE_BYTES} bytes"
            )));
        }
        let mut head = Enc::default();
        head.str(relative)?;
        head.item(tag::U64, &size.to_be_bytes())?;
        self.hasher.update(&head.buf);
        let mut reader = content.take(size.saturating_add(1));
        let mut buf = [0u8; 16 * 1024];
        let mut read: u64 = 0;
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| self.refuse(format!("cannot read {relative}: {e}")))?;
            if n == 0 {
                break;
            }
            read = read.saturating_add(n as u64);
            if read > size {
                break;
            }
            self.hasher.update(&buf[..n]);
        }
        if read != size {
            return Err(self.refuse(format!(
                "{relative} changed while it was hashed ({size} bytes expected, {read} read)"
            )));
        }
        self.last = Some(relative.to_owned());
        Ok(())
    }

    /// The folder's digest.
    pub fn finish(self) -> Digest {
        Digest::of(self.hasher)
    }
}

/// The combined digest (design §8.1).
pub(crate) fn combine(config: &Digest, build: &Digest, addons: &Digest) -> Digest {
    let mut hasher = Sha256::new_with_prefix(LABEL);
    hasher.update(config.0);
    hasher.update(build.0);
    hasher.update(addons.0);
    Digest::of(hasher)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        section: &'static str,
        items: Vec<(&'static str, Option<u64>)>,
        secret: Option<(&'static str, Vec<u8>)>,
    }

    impl SharedSettings for Fixed {
        fn section(&self) -> SectionName {
            SectionName::new(self.section)
        }
        fn describe(&self, out: &mut SettingsWriter<'_>) -> Result<(), NodeError> {
            for (name, value) in &self.items {
                match value {
                    Some(v) => out.shared_u64(name, *v)?,
                    None => out.node(name)?,
                }
            }
            if let Some((name, secret)) = &self.secret {
                out.shared_secret(name, secret)?;
            }
            Ok(())
        }
    }

    fn fixed(section: &'static str, items: &[(&'static str, Option<u64>)]) -> Fixed {
        Fixed {
            section,
            items: items.to_vec(),
            secret: None,
        }
    }

    fn encoded(settings: &dyn SharedSettings) -> Vec<u8> {
        let name = settings.section();
        let mut items = Vec::new();
        settings
            .describe(&mut SettingsWriter {
                section: name,
                items: &mut items,
            })
            .unwrap();
        items.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        encode_section(name, &items).unwrap().buf
    }

    #[test]
    fn digest_encoding_is_canonical_and_never_carries_a_secret() {
        // order of settings does not matter
        let a =
            SectionDigest::of(&fixed("rpc", &[("rpc.a", Some(1)), ("rpc.b", Some(2))])).unwrap();
        let b =
            SectionDigest::of(&fixed("rpc", &[("rpc.b", Some(2)), ("rpc.a", Some(1))])).unwrap();
        assert_eq!(a.digest(), b.digest());
        // a value change does
        let c =
            SectionDigest::of(&fixed("rpc", &[("rpc.a", Some(1)), ("rpc.b", Some(3))])).unwrap();
        assert_ne!(a.digest(), c.digest());
        // order of sections does not matter
        let lim = SectionDigest::of(&fixed("limits", &[("limits.x", Some(5))])).unwrap();
        let one = ConfigDigest::from_sections(vec![a.clone(), lim.clone()]).unwrap();
        let two = ConfigDigest::from_sections(vec![lim.clone(), a.clone()]).unwrap();
        assert_eq!(one.digest(), two.digest());
        assert_eq!(
            one.sections()
                .iter()
                .map(|s| s.name().as_str())
                .collect::<Vec<_>>(),
            ["limits", "rpc"]
        );
        // a node setting changes no digest
        let with_node = SectionDigest::of(&fixed(
            "rpc",
            &[
                ("rpc.a", Some(1)),
                ("rpc.b", Some(2)),
                ("rpc.threads", None),
            ],
        ))
        .unwrap();
        assert_eq!(with_node.digest(), a.digest());
        assert_eq!(
            with_node
                .settings()
                .iter()
                .map(|s| (s.name(), s.scope()))
                .collect::<Vec<_>>(),
            [
                ("rpc.a", Scope::Shared),
                ("rpc.b", Scope::Shared),
                ("rpc.threads", Scope::Node)
            ]
        );
        // duplicates refused
        let err =
            SectionDigest::of(&fixed("rpc", &[("rpc.x", Some(1)), ("rpc.x", None)])).unwrap_err();
        assert!(matches!(err, NodeError::DuplicateSetting { .. }));
        assert_eq!(
            err.to_string(),
            "setting rpc.x is described twice in section rpc"
        );
        let err = ConfigDigest::from_sections(vec![a.clone(), c.clone()]).unwrap_err();
        assert!(matches!(
            err,
            NodeError::DuplicateSetting { name: None, .. }
        ));
        assert_eq!(err.to_string(), "settings section rpc is described twice");
        // a secret never appears in the encoding; changing it changes the digest
        let key: Vec<u8> = (0u8..32)
            .map(|i| i.wrapping_mul(7).wrapping_add(0xA5))
            .collect();
        let with_secret = Fixed {
            section: "auth",
            items: vec![],
            secret: Some(("auth.session_key", key.clone())),
        };
        let bytes = encoded(&with_secret);
        assert!(
            !bytes.windows(key.len()).any(|w| w == key.as_slice()),
            "the secret's bytes are in the encoding"
        );
        let d1 = SectionDigest::of(&with_secret).unwrap();
        assert!(!format!("{d1:?}").contains(&format!("{:?}", key)));
        let mut other = key.clone();
        other[31] ^= 1;
        let d2 = SectionDigest::of(&Fixed {
            section: "auth",
            items: vec![],
            secret: Some(("auth.session_key", other)),
        })
        .unwrap();
        assert_ne!(d1.digest(), d2.digest());
        // a 31-byte shared secret refused
        let err = SectionDigest::of(&Fixed {
            section: "auth",
            items: vec![],
            secret: Some(("auth.session_key", key[..31].to_vec())),
        })
        .unwrap_err();
        assert!(matches!(err, NodeError::ShortSharedSecret { len: 31, .. }));
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(
            err.to_string(),
            "shared secret auth.session_key is 31 bytes: a shared secret must be a generated key \
             of at least 32 bytes (passwords are node settings)"
        );
        // invalid names refused
        let err = SectionDigest::of(&fixed("Rpc", &[])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid settings section name \"Rpc\": 1 to 32 bytes of [a-z0-9_], starting with a \
             letter"
        );
        let err = SectionDigest::of(&fixed("rpc", &[("rpc x", Some(1))])).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid setting name \"rpc x\": 1 to 128 bytes of [a-z0-9._-], starting with a letter"
        );
        let err = SectionDigest::of(&fixed("rpc", &[("", Some(1))])).unwrap_err();
        assert!(matches!(err, NodeError::InvalidSetting { .. }));
    }

    /// Not a spec case: a setting's name starts with its section, so a mismatch report that names
    /// the section names where the setting lives.
    #[test]
    fn a_setting_name_starts_with_its_section() {
        let err = SectionDigest::of(&fixed("rpc", &[("limits.json_depth", Some(64))])).unwrap_err();
        assert!(matches!(err, NodeError::InvalidSetting { .. }));
        assert_eq!(
            err.to_string(),
            "setting limits.json_depth is declared in section rpc: a setting's name starts with \
             its section and a dot (rpc.)"
        );
        // a name equal to the section, without the dot, is refused too
        let err = SectionDigest::of(&fixed("rpc", &[("rpcx", Some(1))])).unwrap_err();
        assert!(matches!(err, NodeError::InvalidSetting { .. }));
        assert!(SectionDigest::of(&fixed("rpc", &[("rpc.x", Some(1))])).is_ok());
    }

    #[test]
    fn build_digest_is_the_commit_when_clean_and_the_executable_otherwise() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let clean = |c: &str| BuildIdentity::__from_build("erp", "0.1.0", true, Some(c), Some("0"));
        let a = clean(commit).resolve().unwrap();
        assert_eq!(a.kind(), BuildKind::Commit);
        // a commit without a dirty flag is refused: two local builds of one commit would share
        // a digest
        for dirty in [None, Some("")] {
            let err = BuildIdentity::__from_build("erp", "0.1.0", true, Some(commit), dirty)
                .resolve()
                .unwrap_err();
            assert_eq!(
                err.to_string(),
                "build identity: ERP_BUILD_COMMIT is set but ERP_BUILD_DIRTY is not: set \
                 ERP_BUILD_DIRTY to 0 for a clean tree or 1 for a dirty one (without it two \
                 local builds of one commit would share a digest)"
            );
        }
        // the target is reported, not compared
        assert!(a.label().ends_with(a.target()), "{}", a.label());
        assert_eq!(
            a.label(),
            format!("erp 0.1.0 debug 0123456789ab {}", a.target())
        );
        // commit, profile and native addons each change the digest
        let other_commit = clean("1123456789abcdef0123456789abcdef01234567")
            .resolve()
            .unwrap();
        assert_ne!(a.digest(), other_commit.digest());
        let release =
            BuildIdentity::__from_build("erp", "0.1.0", false, Some(commit), Some("false"))
                .resolve()
                .unwrap();
        assert_ne!(a.digest(), release.digest());
        let with_addon = clean(commit).native_addon("sale", "1.0").resolve().unwrap();
        assert_ne!(a.digest(), with_addon.digest());
        let addons_ab = clean(commit)
            .native_addon("a", "1")
            .native_addon("b", "1")
            .resolve()
            .unwrap();
        let addons_ba = clean(commit)
            .native_addon("b", "1")
            .native_addon("a", "1")
            .resolve()
            .unwrap();
        assert_eq!(addons_ab.digest(), addons_ba.digest());
        // dirty or unknown: the SHA-256 of the running executable
        let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let expected = Digest::from_bytes(Sha256::digest(&exe).into());
        for dirty in [
            BuildIdentity::__from_build("erp", "0.1.0", true, Some(commit), Some("1")),
            BuildIdentity::__from_build("erp", "0.1.0", true, Some(commit), Some("true")),
            BuildIdentity::__from_build("erp", "0.1.0", true, None, None),
            BuildIdentity::__from_build("erp", "0.1.0", true, Some(""), Some("0")),
        ] {
            let r = dirty.resolve().unwrap();
            assert_eq!(r.kind(), BuildKind::Executable);
            assert_eq!(r.digest(), expected);
            assert!(
                r.label()
                    .contains(&format!("exe:{}", &expected.to_string()[..12]))
            );
        }
        // malformed inputs refused
        let err = BuildIdentity::__from_build("erp", "0.1.0", true, Some(commit), Some("yes"))
            .resolve()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "build identity: ERP_BUILD_DIRTY = \"yes\": expected 0, 1, false, true or empty"
        );
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        for bad in ["abc12", "0123456789abcdefg", &"a".repeat(65)] {
            let err = clean(bad).resolve().unwrap_err();
            assert_eq!(
                err.to_string(),
                format!(
                    "build identity: ERP_BUILD_COMMIT = {bad:?}: expected 7 to 64 hexadecimal digits"
                )
            );
        }
    }

    #[test]
    fn addon_and_tree_digests() {
        let d = |n| Digest::from_bytes([n; 32]);
        let one = AddonsDigest::builder()
            .addon("b", "1.0", AddonContent::Package(d(1)))
            .unwrap()
            .addon("a", "1.0", AddonContent::Embedded)
            .unwrap()
            .build()
            .unwrap();
        let two = AddonsDigest::builder()
            .addon("a", "1.0", AddonContent::Embedded)
            .unwrap()
            .addon("b", "1.0", AddonContent::Package(d(1)))
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(one.digest(), two.digest());
        assert_eq!(one.entries()[0].name(), "a");
        let err = AddonsDigest::builder()
            .addon("a", "1.0", AddonContent::Embedded)
            .unwrap()
            .addon("a", "2.0", AddonContent::Embedded)
            .unwrap()
            .build()
            .unwrap_err();
        assert_eq!(err.to_string(), "addon a: listed twice");
        let mut t = TreeHasher::new("x");
        t.file("a.txt", 3, &mut &b"abc"[..]).unwrap();
        let err = t.file("a.txt", 1, &mut &b"z"[..]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "addon x: dev folder files must be hashed in increasing path order (a.txt before a.txt)"
        );
        let mut t = TreeHasher::new("x");
        let err = t.file("b.txt", 4, &mut &b"abc"[..]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "addon x: b.txt changed while it was hashed (4 bytes expected, 3 read)"
        );
    }

    #[test]
    fn decoder_reads_what_the_encoder_wrote_and_rejects_truncation() {
        let mut enc = Enc::default();
        enc.list(1).unwrap();
        enc.str("rpc").unwrap();
        enc.hash(&Digest::from_bytes([9; 32])).unwrap();
        let mut dec = Dec::new(&enc.buf);
        assert_eq!(dec.list(), Some(1));
        assert_eq!(dec.str(), Some("rpc"));
        assert_eq!(dec.hash(), Some(Digest::from_bytes([9; 32])));
        assert!(dec.is_empty());
        let mut short = Dec::new(&enc.buf[..enc.buf.len() - 1]);
        assert_eq!(short.list(), Some(1));
        assert_eq!(short.str(), Some("rpc"));
        assert_eq!(short.hash(), None);
    }
}
