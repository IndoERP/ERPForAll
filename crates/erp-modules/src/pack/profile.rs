// SPDX-License-Identifier: MIT OR Apache-2.0
//! `pack.toml`: the pack profile, its declared files, templates, origin and licence record.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

use super::model::Date;
use super::{PackRule, Res, SUPPORTED_FORMAT_VERSIONS, StageContext, err};
use crate::country::CountryCode;

/// File name of the profile inside a pack.
pub const PROFILE_FILE: &str = "pack.toml";

/// SPDX terms a shipped pack may use: the permissive list of `deny.toml`. A test keeps the
/// two lists equal.
pub const ALLOWED_LICENCES: &[&str] = &[
    "MIT",
    "MIT-0",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Zlib",
    "Unicode-3.0",
    "MPL-2.0",
];

/// Check an SPDX expression against the licence policy: `A OR B` needs one allowed term, `A AND B`
/// needs all of them; parentheses and unknown terms are refused.
/// Errors: The reason, as a sentence naming the offending term.
pub fn check_licence(expression: &str) -> Result<(), String> {
    if expression.contains(['(', ')']) {
        return Err("Licence expressions with parentheses are not supported; name one SPDX term or a flat OR / AND list.".into());
    }
    let allowed = |t: &str| ALLOWED_LICENCES.contains(&t.trim());
    for conjunct in expression.split(" AND ") {
        if !conjunct.split(" OR ").any(allowed) {
            return Err(format!(
                "Licence {:?} is not permissive under the project policy; allowed: {}.",
                conjunct.trim(),
                ALLOWED_LICENCES.join(", ")
            ));
        }
    }
    Ok(())
}

/// How the data of a pack came to be. Converted and mixed packs are a follow-up; they are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OriginKind {
    Authored,
}

/// Origin and licence record of a pack release: written from primary sources.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub kind: OriginKind,
    pub publisher: String,
    /// SPDX expression of the shipped data; permissive terms only.
    pub licence: String,
    pub source_document: Option<String>,
    pub source_version: Option<String>,
    /// Retrieval or verification date.
    pub verified_on: Option<toml::Value>,
    pub reviewer: Option<String>,
    /// `pending` or `reviewed`.
    pub review_status: Option<String>,
}

/// A declared data file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDecl {
    pub path: String,
    pub sha256: String,
    /// `records` or `translations`.
    pub kind: String,
    /// The model of every record that names none (CSV record files: the model of every row).
    pub model: Option<String>,
}

/// A chart template of the pack.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateDecl {
    pub key: String,
    /// Key of a template of this pack that this one extends (`key` or `this.pack/key`).
    pub parent: Option<String>,
    /// Account codes are padded with zeros on the right to this width; a child inherits it.
    pub code_digits: Option<u32>,
    /// An ISO 3166 alpha-2 code listed in the pack's `jurisdictions`.
    pub country: Option<String>,
    pub files: Vec<String>,
}

/// A pinned release of another pack: its parents and qualified references resolve only there.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyDecl {
    pub release: String,
    /// The digest the staged release must have.
    pub sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileToml {
    #[allow(dead_code, reason = "checked before the typed read")]
    format_version: u32,
    pack_id: String,
    profile_id: String,
    rule_release_id: String,
    pack_version: u32,
    release_date: toml::Value,
    jurisdictions: Vec<String>,
    #[serde(default)]
    schemas: BTreeMap<String, String>,
    origin: Origin,
    #[serde(default)]
    dependencies: BTreeMap<String, DependencyDecl>,
    files: Vec<FileDecl>,
    #[serde(default)]
    template: Vec<TemplateDecl>,
}

/// A validated pack profile.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub pack_id: String,
    pub profile_id: String,
    /// Immutable rule release id (never the package semver).
    pub rule_release_id: String,
    /// The pack's own version: a positive integer that grows with every release of the pack.
    pub pack_version: u32,
    /// The release date; a newer release of the pack may not be older.
    pub release_date: Date,
    pub jurisdictions: Vec<String>,
    pub schemas: BTreeMap<String, String>,
    pub origin: Origin,
    /// Pinned releases of the packs this one builds on, by pack id.
    pub dependencies: BTreeMap<String, DependencyDecl>,
    pub files: Vec<FileDecl>,
    pub templates: Vec<TemplateDecl>,
}

/// The profile id is the pack id or the pack id, a dot and more.
fn profile_in_pack(profile: &str, pack: &str) -> bool {
    profile
        .strip_prefix(pack)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

pub(crate) fn ident(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.split('.').all(|p| {
            p.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                && p.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        })
}

fn line_of(src: &str, e: &toml::de::Error) -> String {
    let at = e.span().map_or(0, |s| s.start);
    format!(
        "line {}",
        src[..at.min(src.len())].matches('\n').count() + 1
    )
}

pub(crate) fn toml_date(v: &toml::Value) -> Option<Date> {
    match v {
        toml::Value::Datetime(d) => Date::parse(&d.to_string()),
        toml::Value::String(s) => Date::parse(s),
        _ => None,
    }
}

impl Profile {
    /// Parse and validate `pack.toml`.
    /// Errors: The first refusal: unsupported versions, unknown fields, identity, jurisdiction,
    /// origin and licence rules (each names `pack.toml` and the path).
    pub fn parse(src: &str, ctx: &StageContext<'_>) -> Res<Self> {
        let f = PROFILE_FILE;
        let table: toml::Table = src.parse().map_err(|e: toml::de::Error| {
            err(
                f,
                line_of(src, &e),
                PackRule::Syntax,
                e.message().to_owned(),
            )
        })?;
        let pack = table
            .get("pack_id")
            .and_then(toml::Value::as_str)
            .unwrap_or("?")
            .to_owned();
        let version = table
            .get("format_version")
            .and_then(toml::Value::as_integer);
        if !version.is_some_and(|v| SUPPORTED_FORMAT_VERSIONS.iter().any(|s| i64::from(*s) == v)) {
            return Err(err(
                f,
                "format_version",
                PackRule::FormatVersion,
                format!(
                    "Pack {pack} format version {} is unsupported; supported versions: {}.",
                    version.map_or_else(|| "(missing)".into(), |v| v.to_string()),
                    join(SUPPORTED_FORMAT_VERSIONS)
                ),
            ));
        }
        let t: ProfileToml = toml::from_str(src).map_err(|e| {
            let rule = if e.message().starts_with("unknown field") {
                PackRule::UnknownField
            } else {
                PackRule::Syntax
            };
            err(
                f,
                line_of(src, &e),
                rule,
                format!("Unknown or invalid pack.toml content: {}", e.message()),
            )
        })?;
        let bad = |path: &str, rule, msg: String| err(f, path, rule, msg);
        if !ident(&t.pack_id)
            || !ident(&t.profile_id)
            || !profile_in_pack(&t.profile_id, &t.pack_id)
        {
            return Err(perr!(
                f,
                "pack_id",
                PackRule::Identity,
                "pack_id and profile_id must be namespaced lower-case dotted names and the profile id must start with the pack id."
            ));
        }
        let rel = &t.rule_release_id;
        if rel.is_empty()
            || rel.len() > 64
            || !rel
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        {
            return Err(perr!(
                f,
                "rule_release_id",
                PackRule::Identity,
                "rule_release_id must be 1 to 64 characters of letters, digits, dot, dash or underscore."
            ));
        }
        if t.pack_version == 0 {
            return Err(bad(
                "pack_version",
                PackRule::Identity,
                "pack_version must be a positive integer.".into(),
            ));
        }
        let release_date = toml_date(&t.release_date).ok_or_else(|| {
            bad(
                "release_date",
                PackRule::Identity,
                "release_date must be a date YYYY-MM-DD.".into(),
            )
        })?;
        let mut jurisdictions = BTreeSet::new();
        for (i, j) in t.jurisdictions.iter().enumerate() {
            let code = CountryCode::parse(j).map_err(|_| {
                bad(
                    &format!("jurisdictions[{i}]"),
                    PackRule::Jurisdiction,
                    format!("{j:?} is not an ISO 3166 alpha-2 code."),
                )
            })?;
            if !jurisdictions.insert(code.as_str().to_owned()) {
                return Err(bad(
                    &format!("jurisdictions[{i}]"),
                    PackRule::Jurisdiction,
                    format!("Jurisdiction {j} is listed twice."),
                ));
            }
        }
        if jurisdictions.is_empty() {
            return Err(bad(
                "jurisdictions",
                PackRule::Jurisdiction,
                "At least one jurisdiction is required.".into(),
            ));
        }
        for (i, c) in t.template.iter().enumerate() {
            let Some(country) = &c.country else { continue };
            let listed =
                CountryCode::parse(country).is_ok_and(|c| jurisdictions.contains(c.as_str()));
            if !listed {
                return Err(bad(
                    &format!("template[{i}].country"),
                    PackRule::Jurisdiction,
                    format!("Template country {country:?} is not one of the pack's jurisdictions."),
                ));
            }
        }
        if t.dependencies.len() as u64 > ctx.limits.max_dependencies {
            let msg = "Pack input exceeds configured resource limit.";
            return Err(bad("dependencies", PackRule::Limit, msg.into()));
        }
        for (id, d) in &t.dependencies {
            let hex = d.sha256.len() == 64
                && d.sha256
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
            if !ident(id) || id == &t.pack_id || d.release.is_empty() || !hex {
                let msg = format!(
                    "Dependency {id} needs another pack's id, a release and a 64 digit lower-case sha256."
                );
                return Err(bad(&format!("dependencies.{id}"), PackRule::Reference, msg));
            }
        }
        if t.template.len() as u64 > ctx.limits.max_templates {
            return Err(bad(
                "template",
                PackRule::Limit,
                "Pack input exceeds configured resource limit.".into(),
            ));
        }
        if t.files.len() as u64 > ctx.limits.max_files {
            return Err(bad(
                "files",
                PackRule::Limit,
                "Pack input exceeds configured resource limit.".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        for (i, fd) in t.files.iter().enumerate() {
            if !fd.path.contains("://") && fd.path.split('/').any(|c| c.is_empty() || c == ".") {
                let msg = "Pack path escapes package root.".to_owned();
                return Err(bad(&format!("files[{i}].path"), PackRule::PathEscape, msg));
            }
            if !seen.insert(&fd.path) {
                return Err(bad(
                    &format!("files[{i}].path"),
                    PackRule::DuplicateKey,
                    format!("File {} is declared twice.", fd.path),
                ));
            }
            if !matches!(fd.kind.as_str(), "records" | "translations") {
                let msg = "File kind must be records or translations; notice files are a follow-up (a later release).";
                let path = format!("files[{i}].kind");
                return Err(err(f, path, PackRule::Type, msg));
            }
            if fd.sha256.len() != 64
                || !fd
                    .sha256
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return Err(bad(
                    &format!("files[{i}].sha256"),
                    PackRule::DigestMismatch,
                    "sha256 must be 64 lower-case hex digits.".into(),
                ));
            }
        }
        for (model, hash) in &t.schemas {
            if hash.starts_with("http") || hash.contains("://") {
                return Err(bad(
                    &format!("schemas.{model}"),
                    PackRule::RemoteSchema,
                    "Pack schemas must resolve offline within declared dependencies.".into(),
                ));
            }
        }
        check_origin(&pack, &t.origin)?;
        Ok(Self {
            pack_id: t.pack_id,
            profile_id: t.profile_id,
            rule_release_id: t.rule_release_id,
            pack_version: t.pack_version,
            release_date,
            jurisdictions: jurisdictions.into_iter().collect(),
            schemas: t.schemas,
            origin: t.origin,
            dependencies: t.dependencies,
            files: t.files,
            templates: t.template,
        })
    }
}

fn join(v: &[u32]) -> String {
    v.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
}

fn check_origin(pack: &str, o: &Origin) -> Res<()> {
    let f = PROFILE_FILE;
    let bad = |path: &str, msg: String| err(f, format!("[origin].{path}"), PackRule::Origin, msg);
    let need = |name: &str, v: &str| match v.trim() {
        "" => Err(bad(
            name,
            format!("Pack {pack} origin field {name} is required."),
        )),
        _ => Ok(()),
    };
    need("publisher", &o.publisher)?;
    let licence = check_licence(&o.licence);
    licence.map_err(|m| err(f, "[origin].licence", PackRule::Licence, m))?;
    need(
        "source_document",
        o.source_document.as_deref().unwrap_or(""),
    )?;
    need("source_version", o.source_version.as_deref().unwrap_or(""))?;
    need("reviewer", o.reviewer.as_deref().unwrap_or(""))?;
    if !o
        .verified_on
        .as_ref()
        .is_some_and(|v| toml_date(v).is_some())
    {
        return Err(bad(
            "verified_on",
            "verified_on must be a date YYYY-MM-DD.".into(),
        ));
    }
    if !matches!(o.review_status.as_deref(), Some("pending" | "reviewed")) {
        return Err(bad(
            "review_status",
            "review_status must be pending or reviewed.".into(),
        ));
    }
    Ok(())
}
