// SPDX-License-Identifier: MIT OR Apache-2.0
//! Staging: read a pack directory, validate the whole template graph, and register the immutable release.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::model::{Date, FieldType, ModelSchema, Raw, SchemaSet, Value, convert, hex};
use super::profile::{FileDecl, PROFILE_FILE, Profile, TemplateDecl};
use super::{PackError, PackLimits, PackRule, Res, err};
use crate::safe_read::{extra_hard_links, open_nofollow, read_capped};

/// What a loader needs from its environment; nothing here is read from the network or the clock.
#[derive(Debug, Clone, Copy)]
pub struct StageContext<'a> {
    /// Schemas of the models the packs may carry.
    pub schemas: &'a SchemaSet,
    pub limits: PackLimits,
}

/// One effective record of a template (inherited records included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub model: String,
    pub key: String,
    /// `pack_id/template` that defined it or last overrode it.
    pub template: String,
    /// Typed fields (`Value::Null` for a field a child cleared).
    pub fields: BTreeMap<String, Value>,
}

/// A validated template with its records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateData {
    /// Length of the parent chain: 0 for a root template.
    pub depth: usize,
    /// Key of the root template of the chain; its records share one account-code space.
    pub root: String,
    /// Account code width, own or inherited.
    pub code_digits: Option<u32>,
    /// Inherited records this chain removed with a tombstone (D2 evaluates their removal at activation).
    pub tombstones: BTreeSet<(String, String)>,
    /// The effective records: inherited ones with the overrides applied.
    pub records: BTreeMap<(String, String), Record>,
    /// Translated text by `(model, record key, field, language)`. A translation of a removed record is dropped.
    pub translations: BTreeMap<(String, String, String, String), String>,
}

/// A staged pack release: validated, immutable, identified by its digest.
#[derive(Debug, Clone, PartialEq)]
pub struct Pack {
    pub profile: Profile,
    /// SHA-256 of `pack.toml` and the declared file digests.
    pub digest: String,
    pub templates: BTreeMap<String, TemplateData>,
}

/// Result of registering a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageOutcome {
    /// A new release was registered.
    New,
    /// The same release with the same digest was already registered; nothing changed.
    Unchanged,
}

#[derive(Debug, Clone, Default)]
pub struct PackRegistry {
    releases: BTreeMap<(String, String), Pack>,
}

impl PackRegistry {
    /// A staged release.
    #[must_use]
    pub fn get(&self, pack_id: &str, release: &str) -> Option<&Pack> {
        self.releases.get(&(pack_id.to_owned(), release.to_owned()))
    }

    /// Read, validate and register the pack in `root`. Nothing is registered on a refusal.
    /// Errors: The first [`PackError`].
    pub fn stage_dir(&mut self, root: &Path, ctx: &StageContext<'_>) -> Res<StageOutcome> {
        let mut total = 0u64;
        let profile_bytes = read_file(root, PROFILE_FILE, &ctx.limits, &mut total)?;
        let src = String::from_utf8(profile_bytes.clone()).map_err(|_| {
            err(
                PROFILE_FILE,
                "file",
                PackRule::Syntax,
                "pack.toml is not UTF-8.",
            )
        })?;
        let profile = Profile::parse(&src, ctx)?;
        let mut contents = BTreeMap::new();
        let mut digest = Sha256::new();
        digest.update(Sha256::digest(&profile_bytes));
        let mut declared: Vec<&FileDecl> = profile.files.iter().collect();
        declared.sort_by_key(|d| &d.path);
        for fd in declared {
            let bytes = read_file(root, &fd.path, &ctx.limits, &mut total)?;
            let actual = hex(&Sha256::digest(&bytes));
            if actual != fd.sha256 {
                return Err(err(
                    &fd.path,
                    "sha256",
                    PackRule::DigestMismatch,
                    format!(
                        "File digest {actual} differs from the declared {}.",
                        fd.sha256
                    ),
                ));
            }
            digest.update(format!("{}:{actual}\n", fd.path));
            contents.insert(fd.path.clone(), bytes);
        }
        let digest = hex(&digest.finalize());
        let templates = Builder {
            reg: self,
            ctx,
            profile: &profile,
            contents: &contents,
            done: BTreeMap::new(),
            rows: 0,
        }
        .build()?;
        self.stage(Pack {
            profile,
            digest,
            templates,
        })
    }

    /// Register a validated release (idempotence and upgrade rules).
    /// Errors: [`PackRule::ReleaseImmutable`] for the same release id with different content; [`PackRule::Upgrade`]
    /// when a new release of the pack does not carry a greater `pack_version` and a release date that is not
    /// earlier,
    pub fn stage(&mut self, pack: Pack) -> Res<StageOutcome> {
        let p = &pack.profile;
        let id = (p.pack_id.clone(), p.rule_release_id.clone());
        if let Some(old) = self.releases.get(&id) {
            return if old.digest == pack.digest {
                Ok(StageOutcome::Unchanged)
            } else {
                Err(err(
                    PROFILE_FILE,
                    "rule_release_id",
                    PackRule::ReleaseImmutable,
                    format!(
                        "Rule release {}/{} already exists with different content.",
                        id.0, id.1
                    ),
                ))
            };
        }
        for old in self
            .releases
            .values()
            .filter(|o| o.profile.pack_id == p.pack_id)
        {
            let o = &old.profile;
            let why = if p.pack_version <= o.pack_version {
                Some(format!(
                    "pack_version {} must be greater than {} of release {}.",
                    p.pack_version, o.pack_version, o.rule_release_id
                ))
            } else if p.release_date < o.release_date {
                Some(format!(
                    "release_date {} is earlier than {} of release {}.",
                    p.release_date, o.release_date, o.rule_release_id
                ))
            } else {
                None
            };
            if let Some(m) = why {
                return Err(err(PROFILE_FILE, "pack_version", PackRule::Upgrade, m));
            }
        }
        self.releases.insert(id, pack);
        Ok(StageOutcome::New)
    }
}

fn read_file(root: &Path, rel: &str, limits: &PackLimits, total: &mut u64) -> Res<Vec<u8>> {
    let escape = || {
        err(
            rel,
            "path",
            PackRule::PathEscape,
            "Pack path escapes package root.",
        )
    };
    if rel.contains("://") {
        return Err(err(
            rel,
            "path",
            PackRule::RemoteSchema,
            "Pack schemas must resolve offline within declared dependencies.",
        ));
    }
    let p = Path::new(rel);
    if rel.is_empty()
        || rel.contains('\\')
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(escape());
    }
    let io = |e: std::io::Error| {
        err(
            rel,
            "file",
            PackRule::MissingFile,
            format!("Cannot read {rel}: {e}."),
        )
    };
    let mut cur = root.to_path_buf();
    for c in p.components() {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => return Err(escape()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(err(
                    rel,
                    "path",
                    PackRule::MissingFile,
                    format!("Declared pack file {rel} is missing."),
                ));
            }
            Err(e) => return Err(io(e)),
        }
    }
    let file = open_nofollow(&cur).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() || extra_hard_links(&meta).is_some() {
        return Err(escape());
    }
    let allowed = limits
        .max_file_bytes
        .min(limits.max_total_bytes.saturating_sub(*total));
    let bytes = read_capped(file, allowed).map_err(io)?.ok_or_else(|| {
        err(
            rel,
            "file",
            PackRule::Limit,
            "Pack input exceeds configured resource limit.",
        )
    })?;
    *total += bytes.len() as u64;
    Ok(bytes)
}

// ---- rows ----

struct Row {
    loc: String,
    template: Option<String>,
    model: String,
    key: String,
    tombstone: bool,
    clear: Vec<String>,
    fields: BTreeMap<String, Raw>,
    /// CSV only: columns present with an empty cell.
    empties: BTreeSet<String>,
    csv: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordsToml {
    #[serde(default)]
    record: Vec<RecordToml>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordToml {
    template: Option<String>,
    model: Option<String>,
    key: String,
    #[serde(default)]
    tombstone: bool,
    #[serde(default)]
    clear_fields: Vec<String>,
    #[serde(default)]
    fields: BTreeMap<String, toml::Value>,
}

/// A language code: two or three lower-case letters, optionally `_` and two to four upper-case letters or digits.
fn lang_code(s: &str) -> bool {
    let (lang, region) = s.split_once('_').map_or((s, None), |(l, r)| (l, Some(r)));
    (2..=3).contains(&lang.len())
        && lang.bytes().all(|c| c.is_ascii_lowercase())
        && region.is_none_or(|r| {
            (2..=4).contains(&r.len())
                && r.bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        })
}

fn limit(file: &str, path: impl Into<String>) -> PackError {
    err(
        file,
        path,
        PackRule::Limit,
        "Pack input exceeds configured resource limit.",
    )
}

/// Read a CSV file into cells, row by row, under the byte, field and string caps. Every row after the header
/// counts into `rows` and is refused as soon as the running count passes `max_rows`.
fn csv_cells(
    file: &str,
    bytes: &[u8],
    lim: &PackLimits,
    rows: &mut u64,
) -> Res<Vec<(String, Vec<String>)>> {
    let mut rd = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes);
    let mut out = Vec::new();
    for (i, rec) in rd.records().enumerate() {
        if i > 0 {
            *rows += 1;
            if *rows > lim.max_rows {
                return Err(limit(file, "rows"));
            }
        }
        let loc = format!("row {}", i + 1);
        let rec = rec.map_err(|e| err(file, &loc, PackRule::Syntax, e.to_string()))?;
        if rec.len() as u64 > lim.max_fields_per_row
            || rec.iter().any(|c| c.len() as u64 > lim.max_string_bytes)
        {
            return Err(limit(file, loc));
        }
        out.push((loc, rec.iter().map(str::to_owned).collect()));
    }
    Ok(out)
}

/// The header row: names are plain; a slash path, an `@lang` suffix or an empty or repeated name is refused.
fn check_header(file: &str, cells: &[String]) -> Res<()> {
    let mut seen = BTreeSet::new();
    for c in cells {
        if c.contains(['/', '@', ':']) {
            return Err(err(file, "row 1", PackRule::SourceGrammar, super::GRAMMAR));
        }
        let msg = if c.trim().is_empty() {
            "Empty pack CSV header name."
        } else if !seen.insert(c) {
            "Duplicate pack CSV header name."
        } else {
            continue;
        };
        return Err(err(file, "row 1", PackRule::DuplicateHeader, msg));
    }
    Ok(())
}

/// The header cells and the body rows `(location, cells)` of a CSV file.
type CsvTable = (Vec<String>, Vec<(String, Vec<String>)>);

/// A CSV file with its header checked and every row's width equal to the header's.
fn csv_table(file: &str, bytes: &[u8], lim: &PackLimits, rows: &mut u64) -> Res<CsvTable> {
    let mut all = csv_cells(file, bytes, lim, rows)?.into_iter();
    let Some((_, head)) = all.next() else {
        return Ok((vec![], vec![]));
    };
    check_header(file, &head)?;
    let body: Vec<_> = all.collect();
    if let Some((loc, cells)) = body.iter().find(|(_, c)| c.len() != head.len()) {
        let msg = format!(
            "Row has {} fields, the header has {}.",
            cells.len(),
            head.len()
        );
        return Err(err(file, loc, PackRule::Syntax, msg));
    }
    Ok((head, body))
}

fn csv_record_rows(
    file: &str,
    bytes: &[u8],
    decl: &FileDecl,
    lim: &PackLimits,
    rows: &mut u64,
) -> Res<Vec<Row>> {
    let (head, body) = csv_table(file, bytes, lim, rows)?;
    let model = decl.model.clone().ok_or_else(|| {
        let msg = "A CSV records file declares its model.";
        err(file, "files", PackRule::UnknownModel, msg)
    })?;
    let mut out = Vec::new();
    for (loc, cells) in body {
        let mut row = Row {
            loc: loc.clone(),
            template: None,
            model: model.clone(),
            key: String::new(),
            tombstone: false,
            clear: vec![],
            fields: BTreeMap::new(),
            empties: BTreeSet::new(),
            csv: true,
        };
        for (name, cell) in head.iter().zip(cells) {
            match name.as_str() {
                "key" => row.key = cell,
                "template" => row.template = Some(cell).filter(|c| !c.is_empty()),
                "tombstone" => {
                    row.tombstone = match cell.as_str() {
                        "" | "false" => false,
                        "true" => true,
                        _ => {
                            let msg = "Column tombstone is true, false or empty.";
                            return Err(err(file, format!("{loc}.tombstone"), PackRule::Type, msg));
                        }
                    }
                }
                _ if cell.is_empty() => {
                    row.empties.insert(name.clone());
                }
                _ => {
                    row.fields.insert(name.clone(), Raw::Str(cell));
                }
            }
        }
        // A row without a key continues the row above in the source grammar.
        if row.key.is_empty() {
            return Err(err(file, loc, PackRule::SourceGrammar, super::GRAMMAR));
        }
        out.push(row);
    }
    Ok(out)
}

fn record_rows(
    file: &str,
    bytes: &[u8],
    decl: &FileDecl,
    lim: &PackLimits,
    rows: &mut u64,
) -> Res<Vec<Row>> {
    if file.ends_with(".csv") {
        return csv_record_rows(file, bytes, decl, lim, rows);
    }
    if !file.ends_with(".toml") {
        let msg = "Record files are .toml or .csv.";
        return Err(perr!(file, "file", PackRule::Type, "{}", msg));
    }
    let src = std::str::from_utf8(bytes)
        .map_err(|_| perr!(file, "file", PackRule::Syntax, "File is not UTF-8."))?;
    let t: RecordsToml = toml::from_str(src).map_err(|e| {
        let line = e.span().map_or(0, |s| {
            src[..s.start.min(src.len())].matches('\n').count() + 1
        });
        err(
            file,
            format!("line {line}"),
            PackRule::Syntax,
            e.message().to_owned(),
        )
    })?;
    *rows += t.record.len() as u64;
    if *rows > lim.max_rows {
        return Err(limit(file, "rows"));
    }
    let mut out = Vec::new();
    for (i, r) in t.record.into_iter().enumerate() {
        let loc = format!("record[{i}]");
        if (r.fields.len() + r.clear_fields.len()) as u64 > lim.max_fields_per_row {
            return Err(limit(file, loc));
        }
        let long = |s: &str| s.len() as u64 > lim.max_string_bytes;
        let named = [
            ("key", Some(r.key.as_str())),
            ("template", r.template.as_deref()),
            ("model", r.model.as_deref()),
        ];
        if let Some((what, _)) = named.iter().find(|(_, v)| v.is_some_and(long)) {
            return Err(limit(file, format!("{loc}.{what}")));
        }
        if r.clear_fields.iter().any(|c| long(c)) {
            return Err(limit(file, format!("{loc}.clear_fields")));
        }
        let model = r.model.or_else(|| decl.model.clone());
        let model = model.ok_or_else(|| {
            err(
                file,
                &loc,
                PackRule::UnknownModel,
                "A record names its model.",
            )
        })?;
        let raw = |v: toml::Value| match v {
            toml::Value::String(s) => Raw::Str(s),
            toml::Value::Integer(i) => Raw::Int(i),
            toml::Value::Boolean(b) => Raw::Bool(b),
            toml::Value::Float(_) => Raw::Float,
            toml::Value::Datetime(d) => Raw::Date(d.to_string()),
            _ => Raw::Other,
        };
        let fields = r.fields.into_iter().map(|(k, v)| (k, raw(v))).collect();
        out.push(Row {
            loc,
            template: r.template,
            model,
            key: r.key,
            tombstone: r.tombstone,
            clear: r.clear_fields,
            fields,
            empties: BTreeSet::new(),
            csv: false,
        });
    }
    Ok(out)
}

// ---- templates ----

/// Where a row is: its file, its template and the template's qualified name.
struct Where<'a> {
    file: &'a str,
    template: &'a TemplateDecl,
    qualified: &'a str,
}

type Span = (Date, Option<Date>, String);

struct Builder<'a> {
    reg: &'a PackRegistry,
    ctx: &'a StageContext<'a>,
    profile: &'a Profile,
    contents: &'a BTreeMap<String, Vec<u8>>,
    done: BTreeMap<String, TemplateData>,
    rows: u64,
}

impl<'a> Builder<'a> {
    fn build(mut self) -> Res<BTreeMap<String, TemplateData>> {
        let f = PROFILE_FILE;
        let mut keys = BTreeSet::new();
        let mut owned = BTreeSet::new();
        for (i, t) in self.profile.templates.iter().enumerate() {
            if !super::profile::ident(&t.key) || !keys.insert(t.key.as_str()) {
                let msg = format!("Template key {:?} is invalid or duplicate.", t.key);
                return Err(err(
                    f,
                    format!("template[{i}].key"),
                    PackRule::DuplicateKey,
                    msg,
                ));
            }
            for p in &t.files {
                if self.profile.files.iter().all(|d| &d.path != p) {
                    let msg = format!("Declared pack file {p} is missing.");
                    return Err(err(
                        f,
                        format!("template[{i}].files"),
                        PackRule::MissingFile,
                        msg,
                    ));
                }
                if !owned.insert(p.as_str()) {
                    let msg = format!("File {p} belongs to two templates.");
                    return Err(err(
                        f,
                        format!("template[{i}].files"),
                        PackRule::DuplicateKey,
                        msg,
                    ));
                }
            }
        }
        if let Some(d) = self
            .profile
            .files
            .iter()
            .find(|d| !owned.contains(d.path.as_str()))
        {
            let msg = format!("Declared file {} belongs to no template.", d.path);
            return Err(perr!(f, "files", PackRule::MissingFile, "{}", msg));
        }
        for t in &self.profile.templates {
            self.resolve(&t.key, &mut vec![])?;
        }
        Ok(self.done)
    }

    /// Build template `key` after its parent chain (root first). `stack` is the chain being resolved.
    fn resolve(&mut self, key: &str, stack: &mut Vec<String>) -> Res<()> {
        if self.done.contains_key(key) {
            return Ok(());
        }
        let f = PROFILE_FILE;
        let pack = &self.profile.pack_id;
        let decl = self
            .profile
            .templates
            .iter()
            .find(|t| t.key == key)
            .ok_or_else(|| {
                let msg = format!("Unknown parent template {key}.");
                err(f, "template.parent", PackRule::Reference, msg)
            })?;
        if stack.iter().any(|s| s == key) {
            stack.push(key.to_owned());
            let msg = format!("Template parent cycle: {}.", stack.join(" -> "));
            return Err(err(f, "template.parent", PackRule::Cycle, msg));
        }
        stack.push(key.to_owned());
        // Bound the chain in hand before recursing into its parent: a long chain never runs to its far end.
        if stack.len() as u64 > self.ctx.limits.max_parent_depth + 1 {
            let path = format!("template[{key}].parent");
            let msg = "Template parent depth exceeds configured limit.";
            return Err(err(f, path, PackRule::Depth, msg));
        }
        let mut data = TemplateData {
            depth: 0,
            root: key.to_owned(),
            code_digits: None,
            tombstones: BTreeSet::new(),
            records: BTreeMap::new(),
            translations: BTreeMap::new(),
        };
        if let Some(p) = &decl.parent {
            data = match p.split_once('/') {
                Some((other, k)) if other != pack => self.foreign_parent(other, k)?,
                other => {
                    let local = other.map_or(p.as_str(), |(_, k)| k);
                    self.resolve(local, stack)?;
                    self.done[local].clone()
                }
            };
            // Expanded content counts: every inherited record and translation is a row of this template too.
            self.rows += (data.records.len() + data.translations.len()) as u64;
            if self.rows > self.ctx.limits.max_rows {
                return Err(limit(f, format!("template[{key}].parent")));
            }
            data.depth += 1;
            if data.depth as u64 > self.ctx.limits.max_parent_depth {
                let msg = "Template parent depth exceeds configured limit.";
                let path = format!("template[{key}].parent");
                return Err(err(f, path, PackRule::Depth, msg));
            }
        }
        if let Some(n) = decl.code_digits {
            if n == 0 {
                let msg = "code_digits must be positive.";
                return Err(err(f, "template.code_digits", PackRule::Padding, msg));
            }
            // Refused before any padding: the bound is a setting and never exceeds the string cap.
            let cap = self
                .ctx
                .limits
                .max_code_digits
                .min(self.ctx.limits.max_string_bytes);
            if u64::from(n) > cap {
                let msg =
                    format!("code_digits {n} exceeds the limit l10n.pack.max_code_digits ({cap}).");
                return Err(err(
                    f,
                    format!("template[{key}].code_digits"),
                    PackRule::Padding,
                    msg,
                ));
            }
            data.code_digits = Some(n);
        }
        self.apply_files(decl, &mut data)?;
        self.finish(decl, &mut data)?;
        stack.pop();
        self.done.insert(key.to_owned(), data);
        Ok(())
    }

    /// The effective data of template `k` of the pinned dependency `other`.
    fn foreign_parent(&self, other: &str, k: &str) -> Res<TemplateData> {
        let f = PROFILE_FILE;
        let missing = || {
            let msg = format!("Unresolved or undeclared pack reference {other}/{k}.");
            err(f, "template.parent", PackRule::Reference, msg)
        };
        let dep = self.profile.dependencies.get(other).ok_or_else(missing)?;
        let pack = self.reg.get(other, &dep.release).ok_or_else(missing)?;
        if pack.digest != dep.sha256 {
            let msg = format!(
                "Pinned digest of {other}/{} differs from the staged release.",
                dep.release
            );
            let path = format!("dependencies.{other}");
            return Err(err(f, path, PackRule::DigestMismatch, msg));
        }
        pack.templates.get(k).cloned().ok_or_else(missing)
    }

    fn schema(&self, file: &str, loc: &str, model: &str) -> Res<&'a ModelSchema> {
        let unknown = || {
            err(
                file,
                loc,
                PackRule::UnknownModel,
                format!("Unknown pack model {model}."),
            )
        };
        let declared = self.profile.schemas.get(model).ok_or_else(unknown)?;
        let s = self.ctx.schemas.get(model).ok_or_else(unknown)?;
        if &s.digest() != declared {
            let msg = format!("Schema hash of {model} differs from the declared {declared}.");
            return Err(perr!(file, loc, PackRule::SchemaMismatch, "{}", msg));
        }
        Ok(s)
    }

    fn apply_files(&mut self, decl: &TemplateDecl, data: &mut TemplateData) -> Res<()> {
        let qualified = format!("{}/{}", self.profile.pack_id, decl.key);
        let mut own = BTreeSet::new();
        for path in &decl.files {
            let Some(fd) = self.profile.files.iter().find(|d| &d.path == path) else {
                continue;
            };
            let Some(bytes) = self.contents.get(path).filter(|_| fd.kind == "records") else {
                continue;
            };
            for row in record_rows(path, bytes, fd, &self.ctx.limits, &mut self.rows)? {
                let at = Where {
                    file: path,
                    template: decl,
                    qualified: &qualified,
                };
                self.apply_row(&at, data, &mut own, &row)?;
            }
        }
        // Translations come after every record file, so a row may name a record of a later file.
        let mut own_tr = BTreeSet::new();
        for path in &decl.files {
            let fd = self.profile.files.iter().find(|d| &d.path == path);
            if let (Some(fd), Some(bytes)) = (fd, self.contents.get(path))
                && fd.kind == "translations"
            {
                self.apply_translations(path, bytes, data, &mut own_tr)?;
            }
        }
        Ok(())
    }

    /// Read a translation file: CSV with the columns `model,key,field,lang,text`, one row per
    /// `(model, key, field, lang)`. A row translates a text field of a record of this template's chain.
    fn apply_translations(
        &mut self,
        file: &str,
        bytes: &[u8],
        data: &mut TemplateData,
        own: &mut BTreeSet<(String, String, String, String)>,
    ) -> Res<()> {
        if !file.ends_with(".csv") {
            let msg = "Translation files are .csv with the columns model, key, field, lang, text.";
            return Err(perr!(file, "file", PackRule::Type, "{}", msg));
        }
        let lim = self.ctx.limits;
        let (head, body) = csv_table(file, bytes, &lim, &mut self.rows)?;
        let col = |name: &str| head.iter().position(|h| h == name);
        let cols = ["model", "key", "field", "lang", "text"].map(col);
        if head.len() != 5 || cols.iter().any(Option::is_none) {
            let msg = "A translation file has exactly the columns model, key, field, lang, text.";
            return Err(perr!(file, "row 1", PackRule::Translation, "{}", msg));
        }
        let [m, k, fld, l, t] = cols.map(|c| c.unwrap_or(0));
        for (loc, cells) in body {
            let (model, key, field, lang, text) =
                (&cells[m], &cells[k], &cells[fld], &cells[l], &cells[t]);
            let bad = |col: &str, rule, msg: String| err(file, format!("{loc}.{col}"), rule, msg);
            if !lang_code(lang) {
                let msg = format!("{lang:?} is not a language code such as fr or pt_BR.");
                return Err(bad("lang", PackRule::Translation, msg));
            }
            if text.is_empty() {
                let msg = "Required pack field text is empty.".to_owned();
                return Err(bad("text", PackRule::EmptyRequired, msg));
            }
            let schema = self.schema(file, &loc, model)?;
            let spec = schema.field(field).ok_or_else(|| {
                bad(
                    "field",
                    PackRule::UnknownField,
                    format!("Unknown pack field {field}."),
                )
            })?;
            if spec.ty != FieldType::Text {
                let msg = format!("Field {field} is not text and cannot be translated.");
                return Err(bad("field", PackRule::Translation, msg));
            }
            if !data.records.contains_key(&(model.clone(), key.clone())) {
                let msg = format!("Unresolved or undeclared pack reference {key}.");
                return Err(bad("key", PackRule::Reference, msg));
            }
            let id = (model.clone(), key.clone(), field.clone(), lang.clone());
            if !own.insert(id.clone()) {
                let msg = format!("Duplicate translation of {model} {key} {field} into {lang}.");
                return Err(bad("key", PackRule::DuplicateKey, msg));
            }
            data.translations.insert(id, text.clone());
        }
        Ok(())
    }

    /// Apply one row to the effective records: a new record, an override of an inherited one, or a tombstone.
    fn apply_row(
        &self,
        at: &Where<'_>,
        data: &mut TemplateData,
        own: &mut BTreeSet<(String, String)>,
        row: &Row,
    ) -> Res<()> {
        let (file, loc) = (at.file, &row.loc);
        if let Some(t) = row
            .template
            .as_deref()
            .filter(|t| *t != at.template.key && *t != at.qualified)
        {
            let msg = format!(
                "Pack {} cannot extend template {t} in place; declare a child template.",
                self.profile.pack_id
            );
            return Err(perr!(file, loc, PackRule::InPlaceExtension, "{}", msg));
        }
        let schema = self.schema(file, loc, &row.model)?;
        let id = (row.model.clone(), row.key.clone());
        if !own.insert(id.clone()) {
            let msg = format!("Duplicate key {} in template {}.", row.key, at.template.key);
            return Err(perr!(file, loc, PackRule::DuplicateKey, "{}", msg));
        }
        let inherited = data.records.contains_key(&id);
        if row.tombstone {
            if !inherited || !row.fields.is_empty() || !row.clear.is_empty() {
                let msg = format!(
                    "Tombstone {} must remove an inherited record and carry no fields.",
                    row.key
                );
                return Err(perr!(file, loc, PackRule::Tombstone, "{}", msg));
            }
            data.records.remove(&id);
            data.tombstones.insert(id);
            return Ok(());
        }
        data.tombstones.remove(&id);
        let mut rec = data.records.remove(&id).unwrap_or_else(|| Record {
            model: row.model.clone(),
            key: row.key.clone(),
            template: at.qualified.to_owned(),
            fields: BTreeMap::new(),
        });
        rec.template = at.qualified.to_owned();
        let field = |name: &str, path: &str| {
            schema.field(name).ok_or_else(|| {
                let msg = format!("Unknown pack field {name}.");
                err(file, path, PackRule::UnknownField, msg)
            })
        };
        for (name, raw) in &row.fields {
            let path = format!("{loc}.{name}");
            let spec = field(name, &path)?;
            if row.clear.contains(name) {
                let msg =
                    format!("Field {name} cannot have both clear_fields and a nonempty value.");
                return Err(perr!(file, path, PackRule::ClearFields, "{}", msg));
            }
            let max = self.ctx.limits.max_string_bytes;
            let value = convert(&self.profile.pack_id, file, &path, spec, raw, row.csv, max)?;
            rec.fields.insert(name.clone(), value);
        }
        for name in &row.clear {
            let path = format!("{loc}.clear_fields");
            let spec = field(name, &path)?;
            let msg = if !inherited {
                "clear_fields is only valid on a record that overrides an inherited one.".to_owned()
            } else if !spec.nullable {
                format!("Field {name} is not nullable and cannot be cleared.")
            } else {
                rec.fields.insert(name.clone(), Value::Null);
                continue;
            };
            return Err(perr!(file, path, PackRule::ClearFields, "{}", msg));
        }
        for name in &row.empties {
            let path = format!("{loc}.{name}");
            let spec = field(name, &path)?;
            let (rule, msg) = if inherited {
                let m = format!(
                    "Empty override cell {name} requires clear_fields or an explicit typed value."
                );
                (PackRule::EmptyOverride, m)
            } else if spec.required {
                (
                    PackRule::EmptyRequired,
                    format!("Required pack field {name} is empty."),
                )
            } else if spec.ty == FieldType::Text {
                rec.fields.insert(name.clone(), Value::Text(String::new()));
                continue;
            } else if spec.nullable {
                rec.fields.insert(name.clone(), Value::Null);
                continue;
            } else {
                (
                    PackRule::EmptyRequired,
                    format!("Required pack field {name} is empty."),
                )
            };
            return Err(perr!(file, path, rule, "{}", msg));
        }
        let missing = schema.fields.iter().find(|s| {
            s.required
                && match rec.fields.get(&s.name) {
                    None | Some(Value::Null) => true,
                    Some(Value::Text(t)) => t.is_empty(),
                    Some(_) => false,
                }
        });
        if let Some(spec) = missing {
            let msg = format!("Required pack field {} is missing.", spec.name);
            return Err(perr!(file, loc, PackRule::EmptyRequired, "{}", msg));
        }
        data.records.insert(id, rec);
        Ok(())
    }

    /// Pad account codes with zeros on the right to `code_digits` (12 becomes 1200), as the reference does:
    /// only active accounts are padded (an inactive one is typically a parent and keeps its code), and a code
    /// longer than the width stays as written: nothing is truncated or refused. Codes that are equal after
    /// padding are refused. The root template of the chain (`data.root`) owns one code space.
    fn pad_codes(&self, tpath: &str, data: &mut TemplateData) -> Res<()> {
        let Some(n) = data.code_digits else {
            return Ok(());
        };
        let width = n as usize;
        let root = data.root.clone();
        let mut seen = BTreeSet::new();
        for rec in data.records.values_mut() {
            let schema = self.any_schema(tpath, &rec.model)?;
            let Some(field) = &schema.code_field else {
                continue;
            };
            let Some(Value::Text(code)) = rec.fields.get(field) else {
                continue;
            };
            let inactive = schema
                .active_field
                .as_ref()
                .is_some_and(|a| rec.fields.get(a) == Some(&Value::Bool(false)));
            let code = if inactive {
                code.clone()
            } else {
                format!("{code:0<width$}")
            };
            if !seen.insert((rec.model.clone(), code.clone())) {
                let path = format!("{tpath}.{}.{field}", rec.key);
                let msg = format!("Account code {code} collides in root {root}.");
                return Err(err(PROFILE_FILE, path, PackRule::Padding, msg));
            }
            rec.fields.insert(field.clone(), Value::Text(code));
        }
        Ok(())
    }

    /// Does the pinned release of dependency `pack` hold the record `id` in one of its templates?
    fn dependency_has(&self, pack: &str, id: &(String, String)) -> bool {
        self.profile
            .dependencies
            .get(pack)
            .and_then(|d| {
                self.reg
                    .get(pack, &d.release)
                    .filter(|p| p.digest == d.sha256)
            })
            .is_some_and(|p| p.templates.values().any(|t| t.records.contains_key(id)))
    }

    /// Schema of an effective record: inherited models need not be declared again by a child pack.
    fn any_schema(&self, path: &str, model: &str) -> Res<&'a ModelSchema> {
        self.ctx.schemas.get(model).ok_or_else(|| {
            let msg = format!("Unknown pack model {model}.");
            err(PROFILE_FILE, path, PackRule::UnknownModel, msg)
        })
    }

    fn finish(&self, decl: &TemplateDecl, data: &mut TemplateData) -> Res<()> {
        let f = PROFILE_FILE;
        let tpath = format!("template[{}]", decl.key);
        self.pad_codes(&tpath, data)?;
        data.translations
            .retain(|(m, k, ..), _| data.records.contains_key(&(m.clone(), k.clone())));
        let mut edges = 0u64;
        let mut spans: BTreeMap<(String, Vec<String>), Vec<Span>> = BTreeMap::new();
        for rec in data.records.values() {
            let schema = self.any_schema(&tpath, &rec.model)?;
            for (name, v) in &rec.fields {
                let (Value::Ref { pack, key }, Some(FieldType::Ref(target))) =
                    (v, schema.field(name).map(|s| &s.ty))
                else {
                    continue;
                };
                edges += 1;
                let id = (target.clone(), key.clone());
                let found = match pack.as_deref().filter(|p| *p != self.profile.pack_id) {
                    None => data.records.contains_key(&id),
                    Some(p) => self.dependency_has(p, &id),
                };
                if !found {
                    let shown = pack
                        .as_ref()
                        .map_or_else(|| key.clone(), |p| format!("{p}/{key}"));
                    let msg = format!("Unresolved or undeclared pack reference {shown}.");
                    return Err(err(
                        f,
                        format!("{tpath}.{}.{name}", rec.key),
                        PackRule::Reference,
                        msg,
                    ));
                }
            }
            let Some(scope) = &schema.dated_scope else {
                continue;
            };
            let Some(Value::Date(from)) = rec.fields.get("valid_from") else {
                continue;
            };
            let until = match rec.fields.get("valid_until") {
                Some(Value::Date(d)) => Some(*d),
                _ => None,
            };
            if until.is_some_and(|u| u <= *from) {
                let msg = format!("Record {} has valid_until not after valid_from.", rec.key);
                return Err(err(
                    f,
                    format!("{tpath}.{}", rec.key),
                    PackRule::DatedRange,
                    msg,
                ));
            }
            let sc = scope
                .iter()
                .map(|n| rec.fields.get(n).map(Value::canon).unwrap_or_default())
                .collect();
            spans
                .entry((rec.model.clone(), sc))
                .or_default()
                .push((*from, until, rec.key.clone()));
        }
        if edges > self.ctx.limits.max_ref_edges {
            return Err(limit(f, "references"));
        }
        for ((model, _), mut v) in spans {
            v.sort_by_key(|s| s.0);
            for w in v.windows(2) {
                if w[0].1.is_none_or(|u| u > w[1].0) {
                    let msg = format!("Dated {model} records {} and {} overlap.", w[0].2, w[1].2);
                    return Err(err(
                        f,
                        format!("{tpath}.{}", w[1].2),
                        PackRule::DatedRange,
                        msg,
                    ));
                }
            }
        }
        Ok(())
    }
}
