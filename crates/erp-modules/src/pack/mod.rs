// SPDX-License-Identifier: MIT OR Apache-2.0
//! Localization pack format v1 (D7; task): the strict, normalized data format
//! of a country pack, its validation, and the staging loader.
//!
//! A pack is `pack.toml` plus the data files it declares, inside an ordinary addon directory. Staging
//! reads local files only, validates the whole graph and publishes nothing on the first refusal
//! ([`PackRegistry::stage_dir`]). Activation of a pack for a company is a later task.
//! Every refusal is a [`PackError`] naming the file, the path inside it and the [`PackRule`].

/// A [`PackError`] with a formatted message: `perr!(file, path, rule, "text {}", arg)`.
macro_rules! perr {
    ($f:expr, $p:expr, $r:expr, $($m:tt)+) => {
        err($f, $p, $r, format!($($m)+))
    };
}

mod model;
mod profile;
mod stage;

use std::fmt;

pub use model::{Date, FieldSpec, FieldType, ModelSchema, SchemaSet, Value};
pub use profile::{ALLOWED_LICENCES, DependencyDecl, Origin, OriginKind, Profile, check_licence};
pub use stage::{Pack, PackRegistry, Record, StageContext, StageOutcome, TemplateData};

/// Format versions this loader reads.
pub const SUPPORTED_FORMAT_VERSIONS: &[u32] = &[1];

/// The one message of every source-grammar refusal.
pub(crate) const GRAMMAR: &str =
    "Source CSV grammar is not accepted at runtime; use the offline localization converter.";

macro_rules! rules {
    ($($v:ident => $s:literal),* $(,)?) => {
        /// The rule a refusal applies; the string form is stable and tested.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[non_exhaustive]
        pub enum PackRule { $($v),* }
        impl PackRule {
            /// Stable identifier of the rule.
            #[must_use]
            pub fn as_str(self) -> &'static str { match self { $(Self::$v => $s),* } }
        }
    };
}

rules! {
    Syntax => "syntax", UnknownField => "unknown-field", UnknownModel => "unknown-model",
    MissingFile => "missing-declared-file", DigestMismatch => "digest-mismatch",
    SchemaMismatch => "schema-hash-mismatch", FormatVersion => "format-version",

    Identity => "identity", Jurisdiction => "jurisdiction", Origin => "origin", Licence => "licence",
    Limit => "resource-limit", PathEscape => "path-escape", RemoteSchema => "offline-schema",
    DuplicateKey => "duplicate-key", EmptyRequired => "empty-required", ExactValue => "exact-value", ExactBounds => "exact-bounds", Type => "type",
    Reference => "unresolved-reference", InPlaceExtension => "in-place-extension", DatedRange => "dated-range",
    Cycle => "parent-cycle", Depth => "parent-depth", ClearFields => "clear-fields", Tombstone => "tombstone",
    Padding => "account-code", SourceGrammar => "source-grammar", DuplicateHeader => "duplicate-header",
    EmptyOverride => "empty-override", Translation => "translation",
    ReleaseImmutable => "release-immutable", Upgrade => "upgrade", Settings => "setting",
}

/// A refusal of a pack: where, and under which rule. Nothing of the pack is published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackError {
    /// File name inside the pack (`pack.toml`, `data/accounts.csv`), or the setting key.
    pub file: String,
    /// Position inside the file: `line 4`, `row 7`, `record[2].fields.rate`, `[origin].licence`.
    pub path: String,
    pub rule: PackRule,
    pub message: String,
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}: [{}] {}",
            self.file,
            self.path,
            self.rule.as_str(),
            self.message
        )
    }
}

impl std::error::Error for PackError {}

pub(crate) fn err(
    file: &str,
    path: impl Into<String>,
    rule: PackRule,
    message: impl Into<String>,
) -> PackError {
    PackError {
        file: file.to_owned(),
        path: path.into(),
        rule,
        message: message.into(),
    }
}

pub(crate) type Res<T> = std::result::Result<T, PackError>;

/// Positive caps on pack input. Each value is a system parameter with a default,
/// a floor and a hard bound; a value outside them is refused by name, and zero never means unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackLimits {
    pub max_total_bytes: u64,
    pub max_file_bytes: u64,
    pub max_files: u64,
    pub max_rows: u64,
    pub max_fields_per_row: u64,
    pub max_string_bytes: u64,
    pub max_ref_edges: u64,
    pub max_parent_depth: u64,
    pub max_code_digits: u64,
    pub max_templates: u64,
    pub max_dependencies: u64,
}

/// (system parameter, default, floor, hard bound) in the field order of [`PackLimits`].
pub const LIMIT_PARAMS: [(&str, u64, u64, u64); 11] = [
    ("l10n.pack.max_total_bytes", 8 << 20, 1024, 64 << 20),
    ("l10n.pack.max_file_bytes", 2 << 20, 1024, 16 << 20),
    ("l10n.pack.max_files", 64, 1, 512),
    ("l10n.pack.max_rows", 100_000, 1, 1_000_000),
    ("l10n.pack.max_fields_per_row", 64, 1, 256),
    ("l10n.pack.max_string_bytes", 4096, 16, 65_536),
    ("l10n.pack.max_ref_edges", 200_000, 1, 2_000_000),
    ("l10n.pack.max_parent_depth", 8, 1, 32),
    ("l10n.pack.max_code_digits", 16, 1, 256),
    ("l10n.pack.max_templates", 64, 1, 1024),
    ("l10n.pack.max_dependencies", 16, 1, 64),
];

impl Default for PackLimits {
    fn default() -> Self {
        let d = LIMIT_PARAMS.map(|p| p.1);
        Self::from_array(d)
    }
}

impl PackLimits {
    fn from_array(v: [u64; 11]) -> Self {
        Self {
            max_total_bytes: v[0],
            max_file_bytes: v[1],
            max_files: v[2],
            max_rows: v[3],
            max_fields_per_row: v[4],
            max_string_bytes: v[5],
            max_ref_edges: v[6],
            max_parent_depth: v[7],
            max_code_digits: v[8],
            max_templates: v[9],
            max_dependencies: v[10],
        }
    }

    /// Read the limits through `get` (a system-parameter lookup by key; `None` keeps the default).
    /// Errors: [`PackRule::Settings`] naming the key when a value is not an integer or is outside its floor and
    /// hard bound.
    pub fn from_params(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, PackError> {
        let mut v = [0u64; 11];
        for (slot, (key, default, floor, hard)) in v.iter_mut().zip(LIMIT_PARAMS) {
            *slot = match get(key) {
                None => default,
                Some(text) => {
                    let n: u64 = text.trim().parse().map_err(|_| {
                        err(
                            key,
                            "value",
                            PackRule::Settings,
                            format!("{key} must be an integer, got {text:?}."),
                        )
                    })?;
                    if !(floor..=hard).contains(&n) {
                        return Err(err(
                            key,
                            "value",
                            PackRule::Settings,
                            format!("{key} must be between {floor} and {hard}, got {n}."),
                        ));
                    }
                    n
                }
            };
        }
        Ok(Self::from_array(v))
    }
}
