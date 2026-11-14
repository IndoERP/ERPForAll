//! `models/*.toml`: model declarations of data-only addons.
//!
//! A data-only addon (a directory with `manifest.toml`, no Rust crate) declares new models and
//! extends existing ones in TOML. Field names are free (no `x_` rule); computed fields are only
//! `related` (Luau compute formulas in model files are not scheduled yet). The result is the same
//! registry metadata as `#[model]`/`#[extend]` ([`ModelDef`]s). Unknown keys, parameters that do
//! not belong to the field type and unsupported features are errors naming the file and line.
//!
//! Model files are untrusted input: a file is at most [`MAX_MODEL_FILE_BYTES`], an addon has at
//! most [`MAX_MODEL_FILES`] of them and a file declares at most [`MAX_MODELS_PER_FILE`] models;
//! names and texts are bounded by [`ModelDef::check_limits`]. TOML nesting depth is bounded by the
//! `toml` parser's recursion limit.
//!
//! Typed table objects name fields, never SQL (design §9): `[[model.exclude]]` declares an
//! exclusion constraint over a period of two fields, `[[model.key]]` with `without_overlaps` a key
//! on a `date_range`/`datetime_range` field:
//!
//! ```toml
//! [[model]]
//! extend = "resource.booking"
//!
//! [[model.exclude]]
//! name = "_no_double_booking"
//! equal = ["resource_id"]
//! period = { from = "start", to = "stop", bounds = "[)" }
//! filter = "[('state', '!=', 'cancel')]"
//! message = "This resource is already booked for an overlapping period."
//! ```

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use erp_fields::{Digits, IndexKind, OnDelete, RangeBounds};
use erp_registry::{
    AddonDescriptor, CompanyAccessDef, CompanyBranch, CompanyMatch, DefaultValue, EmptyCompany,
    ExclusionDef, FieldDef, IndexDef, IndexMethod, KindDef, M2mTable, MAX_FIELDS_PER_MODEL,
    MAX_INDEXES_PER_MODEL, MAX_PARENTS, MAX_TABLE_OBJECTS_PER_MODEL, ModelDef, ModelKind,
    TemporalKeyDef, intern_addon_with_policy,
};
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::manifest::{Manifest, ModuleName};

/// Directory of an addon holding its model files.
pub const MODELS_DIR: &str = "models";
/// Largest model file, in bytes.
pub const MAX_MODEL_FILE_BYTES: u64 = 1024 * 1024;
/// Most bytes of model files one addon may have in total.
pub const MAX_MODEL_FILES_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
/// Most model files one addon may have.
pub const MAX_MODEL_FILES: usize = 256;
/// Most models one model file may declare.
pub const MAX_MODELS_PER_FILE: usize = 1000;
/// Most models one addon may declare over all its files (an overlay holds at most this many).
pub const MAX_MODELS_PER_ADDON: usize = erp_registry::MAX_ADDON_DEFS;

/// The registry descriptor of an addon known from its manifest (data-only addons, packages).
/// Equal manifests (same name and dependencies) return the same descriptor.
///
/// # Errors
///
/// [`Error::Registry`] naming the addon when the names are over the registry's size limits or
/// the process has used up its intern budget ([`erp_registry::intern_budget`]; the registry
/// error kind is `ResourceExhausted`).
pub fn addon_descriptor(manifest: &Manifest) -> Result<&'static AddonDescriptor> {
    let depends: Vec<&str> = manifest.depends().iter().map(|d| d.as_str()).collect();
    intern_addon_with_policy(manifest.name().as_str(), &depends, manifest.exact_only()).map_err(
        |source| Error::Registry {
            addon: manifest.name().clone(),
            source,
        },
    )
}

/// Read every `models/*.toml` of `addon` in `addon_dir`, in file-name order. An addon without a
/// `models` directory declares no models; any other entry in it is an error.
///
/// # Errors
///
/// [`Error::ModelFile`] naming the file (and line) for a refused entry, file or declaration,
/// [`Error::Io`] when reading fails.
pub fn read_model_files(addon_dir: &Path, addon: &ModuleName) -> Result<Vec<ModelDef>> {
    let dir = addon_dir.join(MODELS_DIR);
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Io { path, source }
    };
    // `symlink_metadata`: a symlinked `models` is refused like any other non-directory.
    let meta = match fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        // the addon has no model files (the only absent case that is not an error)
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io(&dir)(e)),
    };
    if !meta.is_dir() {
        return Err(Error::model_file(&dir, None, "not a directory"));
    }
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&dir).map_err(io(&dir))? {
        let entry = entry.map_err(io(&dir))?;
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::model_file(&path, None, "file name is not valid UTF-8"))?;
        let file_type = entry.file_type().map_err(io(&path))?;
        if !name.ends_with(".toml") || !file_type.is_file() {
            return Err(unexpected_entry(&path));
        }
        if files.len() >= MAX_MODEL_FILES {
            return Err(Error::model_file(
                &dir,
                None,
                format!("more than {MAX_MODEL_FILES} model files"),
            ));
        }
        files.push((name, path));
    }
    files.sort();
    let mut defs = Vec::new();
    let mut total_bytes = 0u64;
    for (name, path) in files {
        let content = read_bounded(&path)?;
        // `read_bounded` returned at most MAX_MODEL_FILE_BYTES, so the length fits in u64
        let len = u64::try_from(content.len())
            .map_err(|_| Error::model_file(&path, None, "file size does not fit in 64 bits"))?;
        total_bytes = total_bytes.saturating_add(len);
        if total_bytes > MAX_MODEL_FILES_TOTAL_BYTES {
            return Err(Error::model_file(
                &dir,
                None,
                format!("model files larger than {MAX_MODEL_FILES_TOTAL_BYTES} bytes in total"),
            ));
        }
        let parsed = parse_model_file(addon, &format!("{MODELS_DIR}/{name}"), &content)
            .map_err(|e| e.with_model_file_path(&path))?;
        if defs.len().saturating_add(parsed.len()) > MAX_MODELS_PER_ADDON {
            return Err(Error::model_file(
                &dir,
                None,
                format!("more than {MAX_MODELS_PER_ADDON} models in one addon"),
            ));
        }
        defs.extend(parsed);
    }
    Ok(defs)
}

/// Parse the same model-file contract from immutable admitted bytes. No path is reopened.
pub(crate) fn read_snapshot_model_files(
    snapshot: &crate::artifact_identity::ArtifactSnapshot,
    addon: &ModuleName,
) -> Result<Vec<ModelDef>> {
    let dir = snapshot.root_path().join(MODELS_DIR);
    if snapshot.files().contains_key(MODELS_DIR) {
        return Err(Error::model_file(&dir, None, "not a directory"));
    }
    if !snapshot.directories().contains(MODELS_DIR) {
        return Ok(Vec::new()); // The specified absent-model-directory case.
    }
    for directory in snapshot.directories() {
        if directory.starts_with("models/") {
            return Err(unexpected_entry(&snapshot.root_path().join(directory)));
        }
    }
    let mut files = Vec::new();
    for (path, bytes) in snapshot.files() {
        let Some(name) = path.strip_prefix("models/") else {
            continue;
        };
        if name.contains('/') || !name.ends_with(".toml") {
            return Err(unexpected_entry(&snapshot.root_path().join(path)));
        }
        if files.len() >= MAX_MODEL_FILES {
            return Err(Error::model_file(
                &dir,
                None,
                format!("more than {MAX_MODEL_FILES} model files"),
            ));
        }
        files.push((path, bytes));
    }
    let mut defs = Vec::new();
    let mut total_bytes = 0u64;
    for (relative, bytes) in files {
        let path = snapshot.root_path().join(relative);
        let length = u64::try_from(bytes.len())
            .map_err(|_| Error::model_file(&path, None, "file size does not fit in 64 bits"))?;
        if length > MAX_MODEL_FILE_BYTES {
            return Err(Error::model_file(
                &path,
                None,
                format!("larger than {MAX_MODEL_FILE_BYTES} bytes"),
            ));
        }
        total_bytes = total_bytes
            .checked_add(length)
            .ok_or_else(|| Error::model_file(&dir, None, "model file bytes overflow"))?;
        if total_bytes > MAX_MODEL_FILES_TOTAL_BYTES {
            return Err(Error::model_file(
                &dir,
                None,
                format!("model files larger than {MAX_MODEL_FILES_TOTAL_BYTES} bytes in total"),
            ));
        }
        let content = std::str::from_utf8(bytes).map_err(|error| {
            Error::model_file(
                &path,
                None,
                format!("not valid UTF-8 (at byte {})", error.valid_up_to()),
            )
        })?;
        let parsed = parse_model_file(addon, relative, content)
            .map_err(|error| error.with_model_file_path(&path))?;
        let count = defs
            .len()
            .checked_add(parsed.len())
            .ok_or_else(|| Error::model_file(&dir, None, "model count overflow"))?;
        if count > MAX_MODELS_PER_ADDON {
            return Err(Error::model_file(
                &dir,
                None,
                format!("more than {MAX_MODELS_PER_ADDON} models in one addon"),
            ));
        }
        defs.extend(parsed);
    }
    Ok(defs)
}

fn unexpected_entry(path: &Path) -> Error {
    Error::model_file(
        path,
        None,
        format!("unexpected entry in {MODELS_DIR}/: only regular files named *.toml belong here"),
    )
}

/// Open `path` without following a symlink, check the handle is a regular file, and read at most
/// [`MAX_MODEL_FILE_BYTES`] of UTF-8 text from it.
fn read_bounded(path: &Path) -> Result<String> {
    let io = |source| Error::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // a symlink swapped in after the directory scan fails to open (ELOOP); a FIFO does not
        // block the open
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        #[cfg(unix)]
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(unexpected_entry(path)),
        Err(e) => return Err(io(e)),
    };
    if !file.metadata().map_err(io)?.is_file() {
        return Err(unexpected_entry(path));
    }
    let mut bytes = Vec::new();
    file.take(MAX_MODEL_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_MODEL_FILE_BYTES) {
        return Err(Error::model_file(
            path,
            None,
            format!("larger than {MAX_MODEL_FILE_BYTES} bytes"),
        ));
    }
    String::from_utf8(bytes).map_err(|e| {
        Error::model_file(
            path,
            None,
            format!("not valid UTF-8 (at byte {})", e.utf8_error().valid_up_to()),
        )
    })
}

/// The task that owns the native-only SQL object surface, named in the refusal.
const SQL_OBJECT_TASK: &str = "a later release";

/// The first SQL object key a model or a file sets, by name, or `None`. Function bodies,
/// triggers, condition registrations, grants and table persistence are declared by native addons
/// only, so a model file refuses each of them at model and file level.
/// Each name comes from the field itself (`stringify!`), so a key cannot be checked under another
/// key's name; the five fields are declared on both `RawFile` and `RawModel`.
macro_rules! first_sql_key {
    ($raw:expr) => {
        first_sql_key!(@ $raw; sql_function, sql_foreign_key, sql_table_options, sql_trigger, sql_condition, sql_grant, persistence)
    };
    (@ $raw:expr; $($key:ident),+) => {
        [$((stringify!($key), $raw.$key.is_some())),+]
            .into_iter()
            .find_map(|(key, set)| set.then_some(key))
    };
}

/// Parse one model file of `addon`; `file` (e.g. `models/tag.toml`) names it in errors and orders
/// its declarations inside the addon.
///
/// # Errors
///
/// [`Error::ModelFile`] with the line of the refused declaration or field: TOML syntax, unknown
/// keys, wrong value types, parameters that do not apply to the field type, defaults that do not
/// fit it, unsupported features, size limits.
pub fn parse_model_file(addon: &ModuleName, file: &str, content: &str) -> Result<Vec<ModelDef>> {
    let path = Path::new(file);
    if u64::try_from(content.len()).map_or(true, |len| len > MAX_MODEL_FILE_BYTES) {
        return Err(Error::model_file(
            path,
            None,
            format!("larger than {MAX_MODEL_FILE_BYTES} bytes"),
        ));
    }
    let lines = Lines::new(content);
    let raw: RawFile = toml::from_str(content).map_err(|e| {
        let line = e.span().map(|s| lines.line_of(s.start));
        Error::model_file(path, line, e.message())
    })?;
    refuse_sql_key(path, None, first_sql_key!(raw))?;
    if raw.model.len() > MAX_MODELS_PER_FILE {
        return Err(Error::model_file(
            path,
            None,
            format!("more than {MAX_MODELS_PER_FILE} models in one file"),
        ));
    }
    let mut defs = Vec::with_capacity(raw.model.len());
    for spanned in raw.model {
        let line = lines.line_of(spanned.span().start);
        let model = spanned.into_inner();
        refuse_sql_key(path, Some(line), first_sql_key!(model))?;
        let def = model_def(addon.as_str(), file, line, model, &lines)
            .map_err(|(at, m)| Error::model_file(path, Some(at.unwrap_or(line)), m))?;
        def.check_limits()
            .map_err(|e| Error::model_file_refused(path, Some(line), e))?;
        defs.push(def);
    }
    Ok(defs)
}

/// Refuse a SQL object key a model file sets with a `Programming` error naming the key and the
/// native-only surface.
fn refuse_sql_key(path: &Path, line: Option<usize>, key: Option<&str>) -> Result<()> {
    match key {
        Some(key) => Err(Error::model_file_refused(
            path,
            line,
            erp_fields::Error::programming(format!(
                "SQL object key '{key}' is native-only in {SQL_OBJECT_TASK}"
            )),
        )),
        None => Ok(()),
    }
}

/// Byte offsets of line starts, for 1-based line numbers in O(log n) per lookup.
struct Lines(Vec<usize>);

impl Lines {
    fn new(content: &str) -> Self {
        let starts = std::iter::once(0)
            .chain(
                content
                    .bytes()
                    .enumerate()
                    .filter(|(_, b)| *b == b'\n')
                    .map(|(i, _)| i.saturating_add(1)),
            )
            .collect();
        Self(starts)
    }

    /// The 1-based line holding byte `offset` (the first entry is 0, so at least 1).
    fn line_of(&self, offset: usize) -> usize {
        self.0.partition_point(|start| *start <= offset)
    }
}

/// `text` for an error message: at most 64 characters, then `…`.
fn clip(text: &str) -> String {
    if text.chars().nth(64).is_some() {
        let head: String = text.chars().take(64).collect();
        format!("{head}…")
    } else {
        text.to_owned()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    model: Vec<toml::Spanned<RawModel>>,
    // refused with a reason: SQL objects are declared by native addons only; read so
    // the refusal names the key instead of calling it unknown
    sql_function: Option<toml::Value>,
    sql_foreign_key: Option<toml::Value>,
    sql_table_options: Option<toml::Value>,
    sql_trigger: Option<toml::Value>,
    sql_condition: Option<toml::Value>,
    sql_grant: Option<toml::Value>,
    persistence: Option<toml::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    name: Option<String>,
    extend: Option<String>,
    #[serde(default)]
    inherit: Vec<String>,
    description: Option<String>,
    table: Option<String>,
    order: Option<String>,
    rec_name: Option<String>,
    /// `false` opts the model out of the default company check.
    check_company_auto: Option<bool>,
    /// `"exact"`, `"parent_of"`, `"any_of"` or `"parent_of_any"`.
    company_match: Option<String>,
    /// `[model.company_access]` (A1-D2).
    company_access: Option<RawCompanyAccess>,
    eventful: Option<bool>,
    audit_reads: Option<bool>,
    transient: Option<bool>,
    #[serde(rename = "abstract")]
    is_abstract: Option<bool>,
    /// Cleanup policy of a transient model; read as a TOML value so a wrong type is
    /// refused naming the attribute. `0` disables the threshold.
    transient_max_age_seconds: Option<toml::Value>,
    transient_max_count: Option<toml::Value>,
    #[serde(default)]
    field: Vec<toml::Spanned<RawField>>,
    #[serde(default)]
    index: Vec<toml::Spanned<RawIndex>>,
    // refused with a reason: partitioning is declared by native addons only
    partition: Option<toml::Value>,
    /// `[[model.exclude]]`.
    #[serde(default)]
    exclude: Vec<toml::Spanned<RawExclude>>,
    /// `[[model.key]]`; only `without_overlaps` keys until.
    #[serde(default)]
    key: Vec<toml::Spanned<RawKey>>,
    // refused with a reason: SQL objects are declared by native addons only; read so
    // the refusal names the key instead of calling it unknown
    sql_function: Option<toml::Value>,
    sql_foreign_key: Option<toml::Value>,
    sql_table_options: Option<toml::Value>,
    sql_trigger: Option<toml::Value>,
    sql_condition: Option<toml::Value>,
    sql_grant: Option<toml::Value>,
    persistence: Option<toml::Value>,
}

/// `[model.company_access]`: `direct = { … }` and/or `parent = "edge"`, or `unrestricted =
/// "reason"` alone.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCompanyAccess {
    direct: Option<RawDirect>,
    parent: Option<String>,
    unrestricted: Option<String>,
}

/// `direct = { field = "company_id", branch = "exact", empty = "deny" }`: all three required;
/// `escape = "share"` is optional (a stored, required boolean of the model).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDirect {
    field: String,
    branch: String,
    empty: String,
    escape: Option<String>,
}

/// `[[model.index]]`: a typed index (field names only, so untrusted files may declare it).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIndex {
    name: String,
    fields: Vec<String>,
    method: Option<String>,
    #[serde(default)]
    include: Vec<String>,
    filter: Option<String>,
}

/// `[[model.exclude]]`: a typed exclusion constraint.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExclude {
    name: String,
    #[serde(default)]
    equal: Vec<String>,
    period: RawPeriod,
    filter: Option<String>,
    message: String,
}

/// `period = { from = "...", to = "...", bounds = "[)" }`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPeriod {
    from: String,
    to: String,
    bounds: String,
}

/// `[[model.key]]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawKey {
    name: String,
    #[serde(default)]
    fields: Vec<String>,
    without_overlaps: Option<String>,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    string: Option<String>,
    help: Option<String>,
    required: Option<bool>,
    readonly: Option<bool>,
    store: Option<bool>,
    copy: Option<bool>,
    index: Option<toml::Value>,
    default: Option<toml::Value>,
    related: Option<String>,
    groups: Option<String>,
    domain: Option<String>,
    context: Option<String>,
    check_company: Option<bool>,
    // type parameters
    digits: Option<Vec<u32>>,
    size: Option<u32>,
    currency_field: Option<String>,
    selection: Option<Vec<Vec<String>>>,
    selection_add: Option<Vec<Vec<String>>>,
    comodel: Option<String>,
    inverse: Option<String>,
    relation: Option<String>,
    column1: Option<String>,
    column2: Option<String>,
    ondelete: Option<String>,
    bounds: Option<String>,
    generated: Option<String>,
    generated_storage: Option<String>,
    input_adapted: Option<bool>,
    collation: Option<toml::Value>,
    // refused with a reason
    compute: Option<toml::Value>,
    depends: Option<toml::Value>,
}

/// A refusal: the line of the offending field (`None`: the model's line) and the message.
type Refusal = (Option<usize>, String);

fn model_def(
    addon: &str,
    file: &str,
    line: usize,
    mut raw: RawModel,
    lines: &Lines,
) -> std::result::Result<ModelDef, Refusal> {
    let raw_fields = std::mem::take(&mut raw.field);
    let raw_indexes = std::mem::take(&mut raw.index);
    let raw_excludes = std::mem::take(&mut raw.exclude);
    let raw_keys = std::mem::take(&mut raw.key);
    let mut def = model_header(addon, file, line, raw, raw_fields.len()).map_err(|m| (None, m))?;
    let name = def.name.clone();
    for spanned in raw_fields {
        let at = Some(lines.line_of(spanned.span().start));
        let raw_field = spanned.into_inner();
        let fname = raw_field.name.clone();
        if def.fields.iter().any(|f| f.name == fname) {
            return Err((
                at,
                format!(
                    "model '{}': field '{}' is declared twice",
                    clip(&name),
                    clip(&fname)
                ),
            ));
        }
        let field = field_def(raw_field)
            .map_err(|m| (at, format!("field {}.{}: {m}", clip(&name), clip(&fname))))?;
        // size limits and the default's type, reported at the field's line
        field
            .check_limits(&name)
            .map_err(|e| (at, e.message().to_owned()))?;
        def.fields.push(field);
    }
    // counted first: the duplicate check below is quadratic in it
    if raw_indexes.len() > MAX_INDEXES_PER_MODEL {
        return Err((
            None,
            format!(
                "model '{}': {} indexes, more than the limit of {MAX_INDEXES_PER_MODEL}",
                clip(&name),
                raw_indexes.len()
            ),
        ));
    }
    for spanned in raw_indexes {
        let at = Some(lines.line_of(spanned.span().start));
        let index = index_def(spanned.into_inner())
            .map_err(|m| (at, format!("model '{}': {m}", clip(&name))))?;
        if def.indexes.iter().any(|i| i.name == index.name) {
            return Err((
                at,
                format!(
                    "model '{}': index '{}' is declared twice",
                    clip(&name),
                    clip(&index.name)
                ),
            ));
        }
        def.indexes.push(index);
    }
    typed_objects(&mut def, raw_excludes, raw_keys, lines)?;
    Ok(def)
}

/// A `[[model.index]]` table; the registry checks its fields against the model when it builds.
fn index_def(raw: RawIndex) -> std::result::Result<IndexDef, String> {
    let at = format!("index '{}'", clip(&raw.name));
    if !raw.name.starts_with('_') {
        return Err(format!(
            "{at}: names of SQL objects in a model start with '_'"
        ));
    }
    let method = match raw.method.as_deref() {
        None => IndexMethod::Btree,
        Some(text) => IndexMethod::from_name(text).ok_or_else(|| {
            format!(
                "{at}: unknown method '{}' (expected btree, brin, gin, gist or trigram)",
                clip(text)
            )
        })?,
    };
    let mut index = IndexDef::new(raw.name, raw.fields)
        .method(method)
        .include(raw.include);
    if let Some(filter) = raw.filter {
        index = index.filter(filter);
    }
    Ok(index)
}

/// `[[model.exclude]]` and `[[model.key]]` of a model, refused at their line.
fn typed_objects(
    def: &mut ModelDef,
    excludes: Vec<toml::Spanned<RawExclude>>,
    keys: Vec<toml::Spanned<RawKey>>,
    lines: &Lines,
) -> std::result::Result<(), Refusal> {
    let name = clip(&def.name);
    let count = excludes.len().saturating_add(keys.len());
    if count > MAX_TABLE_OBJECTS_PER_MODEL {
        return Err((
            None,
            format!(
                "model '{name}': {count} exclusions and keys, more than the limit of \
                 {MAX_TABLE_OBJECTS_PER_MODEL}"
            ),
        ));
    }
    let mut seen: Vec<String> = Vec::with_capacity(count);
    let mut once = |object: &str, at: Option<usize>| {
        if seen.iter().any(|s| s == object) {
            return Err((
                at,
                format!("model '{name}': '{}' is declared twice", clip(object)),
            ));
        }
        seen.push(object.to_owned());
        Ok(())
    };
    for spanned in excludes {
        let at = Some(lines.line_of(spanned.span().start));
        let raw = spanned.into_inner();
        once(&raw.name, at)?;
        let bounds = RangeBounds::parse(&raw.period.bounds).ok_or_else(|| {
            (
                at,
                format!(
                    "exclusion '{}': unknown bounds '{}' (expected \"[)\" or \"[]\")",
                    clip(&raw.name),
                    clip(&raw.period.bounds)
                ),
            )
        })?;
        let mut exclusion = ExclusionDef::new(
            raw.name,
            (raw.period.from, raw.period.to),
            bounds,
            raw.message,
        )
        .equal(raw.equal);
        if let Some(filter) = raw.filter {
            exclusion = exclusion.filter(filter);
        }
        def.exclusions.push(exclusion);
    }
    for spanned in keys {
        let at = Some(lines.line_of(spanned.span().start));
        let raw = spanned.into_inner();
        once(&raw.name, at)?;
        let Some(range) = raw.without_overlaps else {
            return Err((
                at,
                format!(
                    "key '{}': business keys in model files arrive with a later release; this \
                     build declares keys with `without_overlaps = \"<range field>\"` only",
                    clip(&raw.name)
                ),
            ));
        };
        def.temporal_keys
            .push(TemporalKeyDef::new(raw.name, range, raw.message).fields(raw.fields));
    }
    Ok(())
}

/// The model part of a declaration (everything but its fields; `field_count` is checked here so
/// the quadratic duplicate checks stay bounded).
fn model_header(
    addon: &str,
    file: &str,
    line: usize,
    raw: RawModel,
    field_count: usize,
) -> std::result::Result<ModelDef, String> {
    let (name, extension) = match (raw.name, raw.extend) {
        (Some(name), None) => (name, false),
        (None, Some(target)) => (target, true),
        (Some(_), Some(_)) => {
            return Err(
                "a model sets either `name` (new model) or `extend` (extension), not both"
                    .to_owned(),
            );
        }
        (None, None) => {
            return Err("a model needs `name` (new model) or `extend` (extension)".to_owned());
        }
    };
    if raw.partition.is_some() {
        return Err(format!(
            "model '{name}': partitioning is declared by native addons only ({})",
            erp_registry::PARTITION_ADR
        ));
    }
    let mut def = if extension {
        ModelDef::extension(addon, name.clone())
    } else {
        ModelDef::new(addon, name.clone())
    };
    // Counts first: the duplicate checks are quadratic in them.
    if raw.inherit.len() > MAX_PARENTS {
        return Err(format!(
            "model '{name}': {} parents, more than the limit of {MAX_PARENTS}",
            raw.inherit.len()
        ));
    }
    if field_count > MAX_FIELDS_PER_MODEL {
        return Err(format!(
            "model '{name}': {field_count} fields, more than the limit of {MAX_FIELDS_PER_MODEL}"
        ));
    }
    for parent in raw.inherit {
        if parent == name {
            return Err(format!(
                "model '{name}': use `extend = \"{name}\"` to extend a model in place"
            ));
        }
        if def.inherit.contains(&parent) {
            return Err(format!("model '{name}': parent '{parent}' is listed twice"));
        }
        def.inherit.push(parent);
    }
    if extension && raw.table.is_some() {
        return Err(format!(
            "model '{name}': an extension cannot set `table` (it belongs to the model's definition)"
        ));
    }
    // Not stated: a new model is regular, an extension takes the kind of the model it extends.
    def.kind = match (raw.transient, raw.is_abstract) {
        (None, None) => None,
        (Some(true), Some(true)) => {
            return Err(format!(
                "model '{name}': `transient` and `abstract` exclude each other"
            ));
        }
        (Some(true), None | Some(false)) => Some(ModelKind::Transient),
        (None | Some(false), Some(true)) => Some(ModelKind::Abstract),
        (Some(false), None | Some(false)) | (None, Some(false)) => Some(ModelKind::Regular),
    };
    def.transient_max_age_seconds = raw
        .transient_max_age_seconds
        .map(|v| {
            policy_value(
                &name,
                erp_registry::TRANSIENT_MAX_AGE_ATTR,
                &v,
                erp_registry::MAX_TRANSIENT_MAX_AGE_SECONDS,
            )
        })
        .transpose()?;
    def.transient_max_count = raw
        .transient_max_count
        .map(|v| {
            policy_value(
                &name,
                erp_registry::TRANSIENT_MAX_COUNT_ATTR,
                &v,
                erp_registry::MAX_TRANSIENT_MAX_COUNT,
            )
        })
        .transpose()?;
    def.description = raw.description;
    def.table = raw.table;
    def.order = raw.order;
    def.rec_name = raw.rec_name;
    def.check_company_auto = raw.check_company_auto;
    // Absence is the default-false declaration policy, never a fallback for a wrong type.
    def.eventful = raw.eventful.unwrap_or(false);
    // absent declarations do not opt in; false never narrows composition.
    def.audit_reads = raw.audit_reads.unwrap_or(false);
    def.company_match = raw
        .company_match
        .as_deref()
        .map(CompanyMatch::parse)
        .transpose()
        .map_err(|e| format!("model '{name}': {}", e.message()))?;
    def.company_access = raw
        .company_access
        .map(|access| company_access_def(&name, access))
        .transpose()?;
    def.file = file.to_owned();
    def.line = u32::try_from(line).map_err(|_| format!("model '{name}': line {line} too large"))?;
    Ok(def)
}

/// A transient policy threshold of a model file: a non-negative TOML integer up to `bound`.
fn policy_value(
    model: &str,
    attr: &str,
    value: &toml::Value,
    bound: u64,
) -> std::result::Result<u64, String> {
    let toml::Value::Integer(n) = value else {
        return Err(format!(
            "model '{model}': `{attr}` must be an integer (0 disables the threshold), not {}",
            value.type_str()
        ));
    };
    let Ok(n) = u64::try_from(*n) else {
        return Err(format!(
            "model '{model}': `{attr}` must not be negative (0 disables the threshold)"
        ));
    };
    if n > bound {
        return Err(format!(
            "model '{model}': `{attr}` = {n} is too large: at most {bound}"
        ));
    }
    Ok(n)
}

/// The owned declaration of a `[model.company_access]` table (design §3.3).
fn company_access_def(
    name: &str,
    raw: RawCompanyAccess,
) -> std::result::Result<CompanyAccessDef, String> {
    let direct = |d: RawDirect| -> std::result::Result<_, String> {
        let at = |e: erp_fields::Error| {
            format!("model '{name}': company_access.direct: {}", e.message())
        };
        let branch = CompanyBranch::parse(&d.branch).map_err(at)?;
        let empty = EmptyCompany::parse(&d.empty).map_err(at)?;
        Ok((d.field, branch, empty, d.escape))
    };
    match (raw.direct, raw.parent, raw.unrestricted) {
        (None, None, Some(reason)) => Ok(CompanyAccessDef::unrestricted(reason)),
        (_, _, Some(_)) => Err(format!(
            "model '{name}': company_access: unrestricted cannot be combined with direct or parent"
        )),
        (Some(d), None, None) => {
            let (field, branch, empty, escape) = direct(d)?;
            Ok(match escape {
                Some(escape) => CompanyAccessDef::direct_escaping(field, branch, empty, escape),
                None => CompanyAccessDef::direct(field, branch, empty),
            })
        }
        (Some(d), Some(edge), None) => {
            let (field, branch, empty, escape) = direct(d)?;
            if escape.is_some() {
                return Err(format!(
                    "model '{name}': company_access.direct: escape cannot be combined with parent"
                ));
            }
            Ok(CompanyAccessDef::direct_and_parent(
                field, branch, empty, edge,
            ))
        }
        (None, Some(edge), None) => Ok(CompanyAccessDef::parent(edge)),
        (None, None, None) => Err(format!(
            "model '{name}': company_access needs direct, parent or unrestricted"
        )),
    }
}

fn field_def(raw: RawField) -> std::result::Result<FieldDef, String> {
    if raw.compute.is_some() || raw.depends.is_some() {
        return Err(
            "`compute`/`depends` are not supported in model files: a computed field there is \
             `related`; Luau compute formulas in model files are not scheduled yet"
                .to_owned(),
        );
    }
    let ty = raw.ty.as_str();
    // Parameters each type accepts beyond the common ones.
    let given: [(&str, bool); 13] = [
        ("digits", raw.digits.is_some()),
        ("size", raw.size.is_some()),
        ("currency_field", raw.currency_field.is_some()),
        ("selection", raw.selection.is_some()),
        ("selection_add", raw.selection_add.is_some()),
        ("comodel", raw.comodel.is_some()),
        ("inverse", raw.inverse.is_some()),
        ("relation", raw.relation.is_some()),
        ("column1", raw.column1.is_some()),
        ("column2", raw.column2.is_some()),
        ("ondelete", raw.ondelete.is_some()),
        ("check_company", raw.check_company.is_some()),
        ("bounds", raw.bounds.is_some()),
    ];
    let allowed: &[&str] = match ty {
        "bigint" | "boolean" | "integer" | "text" | "html" | "date" | "datetime" | "uuid"
        | "xid8" | "timestamptz" | "json" => &[],
        "float" => &["digits"],
        "monetary" => &["currency_field"],
        "char" => &["size"],
        "selection" => &["selection", "selection_add"],
        "many2one" => &["comodel", "ondelete", "check_company"],
        "one2many" => &["comodel", "inverse", "check_company"],
        "many2many" => &[
            "comodel",
            "relation",
            "column1",
            "column2",
            "ondelete",
            "check_company",
        ],
        "date_range" | "datetime_range" => &["bounds"],
        "reference" | "many2one_reference" => {
            return Err(format!(
                "type '{ty}' is not supported in model files (declare it in a native addon)"
            ));
        }
        other => return Err(format!("unknown field type '{}'", clip(other))),
    };
    if let Some((key, _)) = given.iter().find(|(k, set)| *set && !allowed.contains(k)) {
        return Err(format!("`{key}` does not apply to type '{ty}'"));
    }
    let relational = matches!(ty, "many2one" | "one2many" | "many2many");
    if !relational && (raw.domain.is_some() || raw.context.is_some()) {
        return Err(format!("`domain`/`context` do not apply to type '{ty}'"));
    }
    let comodel = || {
        raw.comodel
            .clone()
            .ok_or_else(|| format!("a {ty} field needs `comodel`"))
    };
    let ondelete = raw.ondelete.as_deref().map(ondelete).transpose()?;
    let kind =
        match ty {
            "boolean" => KindDef::Boolean,
            "bigint" => KindDef::BigInt,
            "integer" => KindDef::Integer,
            "text" => KindDef::Text,
            "html" => KindDef::Html,
            "date" => KindDef::Date,
            "datetime" => KindDef::Datetime,
            "uuid" => KindDef::Uuid,
            "xid8" => KindDef::Xid8,
            "timestamptz" => KindDef::TimestampTz,
            "json" => KindDef::Json,
            "float" => KindDef::Float {
                digits: match raw.digits.as_deref() {
                    None => None,
                    Some([precision, scale]) => Some(Digits::Fixed {
                        precision: *precision,
                        scale: *scale,
                    }),
                    Some(_) => return Err("`digits` is [precision, scale]".to_owned()),
                },
            },
            "monetary" => KindDef::Monetary {
                currency_field: raw.currency_field.clone(),
            },
            "char" => KindDef::Char { size: raw.size },
            "selection" => KindDef::Selection {
                values: raw
                    .selection
                    .as_ref()
                    .map(|pairs| {
                        pairs
                            .iter()
                            .map(|p| match p.as_slice() {
                                [k, l] => Ok((k.clone(), l.clone())),
                                _ => Err("`selection` entries are [value, label]".to_owned()),
                            })
                            .collect::<std::result::Result<Vec<_>, String>>()
                    })
                    .transpose()?,
                add: raw
                    .selection_add
                    .as_ref()
                    .map(|pairs| {
                        pairs
                            .iter()
                            .map(|p| match p.as_slice() {
                                [k] => Ok((k.clone(), None)),
                                [k, l] => Ok((k.clone(), Some(l.clone()))),
                                _ => Err("`selection_add` entries are [value, label] or [value]"
                                    .to_owned()),
                            })
                            .collect::<std::result::Result<Vec<_>, String>>()
                    })
                    .transpose()?
                    .unwrap_or_default(),
                ondelete: Vec::new(),
            },
            "many2one" => KindDef::Many2one {
                comodel: comodel()?,
                ondelete,
            },
            "one2many" => KindDef::One2many {
                comodel: comodel()?,
                inverse: raw
                    .inverse
                    .clone()
                    .ok_or_else(|| "a one2many field needs `inverse`".to_owned())?,
            },
            "date_range" | "datetime_range" => {
                let text = raw.bounds.as_deref().ok_or_else(|| {
                    format!("a {ty} field needs `bounds` (\"[)\" or \"[]\"; no default)")
                })?;
                let bounds = RangeBounds::parse(text).ok_or_else(|| {
                    format!(
                        "unknown bounds '{}' (expected \"[)\" or \"[]\")",
                        clip(text)
                    )
                })?;
                if ty == "date_range" {
                    KindDef::DateRange { bounds }
                } else {
                    KindDef::DatetimeRange { bounds }
                }
            }
            "many2many" => KindDef::Many2many {
                comodel: comodel()?,
                table: match (&raw.relation, &raw.column1, &raw.column2) {
                    (None, None, None) => None,
                    (Some(r), Some(c1), Some(c2)) => Some(M2mTable::new(r, c1, c2)),
                    _ => {
                        return Err("`relation`, `column1` and `column2` go together".to_owned());
                    }
                },
                ondelete,
            },
            _ => return Err(format!("unknown field type '{}'", clip(ty))),
        };
    let mut def = FieldDef::new(raw.name, kind);
    def.collation = match raw.collation {
        None => None,
        Some(toml::Value::String(name)) => Some(name),
        Some(other) => {
            return Err(format!(
                "field '{}': collation: expected a string, got {other}",
                def.name
            ));
        }
    };
    def.string = raw.string;
    def.help = raw.help;
    def.required = raw.required;
    def.readonly = raw.readonly;
    def.store = raw.store;
    def.copy = raw.copy;
    def.index = raw.index.map(|v| index(&v)).transpose()?;
    // the registry's rule for `brin`/`gin` (design m3-ormx §6.1), reported here with the line;
    // like the registry, `brin`/`gin` need a stored field
    let stored = raw.store.unwrap_or(raw.related.is_none());
    if let Some(Some(kind)) = def.index
        && kind.field_types().is_some()
    {
        if !stored {
            return Err(format!(
                "index = \"{}\" needs a stored field (this field has no column)",
                kind.as_str()
            ));
        }
        if let Some(reason) = erp_registry::index_kind_mismatch(kind, ty) {
            return Err(reason);
        }
    }
    def.default = raw.default.map(|v| default(&v, &def.kind)).transpose()?;
    def.related = raw.related;
    def.groups = raw.groups;
    def.domain = raw.domain;
    def.context = raw.context;
    def.check_company = raw.check_company;
    if let Some(enabled) = raw.input_adapted {
        def = def.input_adapted(enabled);
    }
    if raw.generated_storage.is_some() && raw.generated.is_none() {
        return Err("attribute 'generated_storage' conflicts with generation".to_owned());
    }
    if let Some(storage_str) = &raw.generated_storage {
        let Some(storage) = erp_registry::GeneratedStorage::parse(storage_str) else {
            return Err(format!(
                "unknown generated_storage '{}', expected 'virtual' or 'stored'",
                clip(storage_str)
            ));
        };
        def = def.generated_storage(storage);
    }
    if let Some(expr) = raw.generated {
        def = def.generated(expr);
    }
    Ok(def)
}

fn ondelete(text: &str) -> std::result::Result<OnDelete, String> {
    match text {
        "set null" => Ok(OnDelete::SetNull),
        "restrict" => Ok(OnDelete::Restrict),
        "cascade" => Ok(OnDelete::Cascade),
        other => Err(format!(
            "unknown ondelete '{}' (expected \"set null\", \"restrict\" or \"cascade\")",
            clip(other)
        )),
    }
}

fn index(value: &toml::Value) -> std::result::Result<Option<IndexKind>, String> {
    match value {
        toml::Value::Boolean(true) => Ok(Some(IndexKind::Btree)),
        toml::Value::Boolean(false) => Ok(None),
        toml::Value::String(s) => match IndexKind::from_name(s) {
            Some(kind) => Ok(Some(kind)),
            None => Err(format!("unknown index kind '{}'", clip(s))),
        },
        other => Err(format!(
            "`index` is true, false or an index kind, not {}",
            other.type_str()
        )),
    }
}

/// The static default of a field of kind `kind`.
/// `true` stays a
/// boolean, which only a boolean field takes (the registry refuses it elsewhere).
fn default(value: &toml::Value, kind: &KindDef) -> std::result::Result<DefaultValue, String> {
    match value {
        toml::Value::Boolean(false) if !matches!(kind, KindDef::Boolean) => Ok(DefaultValue::False),
        toml::Value::Boolean(b) => Ok(DefaultValue::Bool(*b)),
        toml::Value::Integer(i) => Ok(DefaultValue::Int(*i)),
        toml::Value::Float(f) => Ok(DefaultValue::Float(*f)),
        toml::Value::String(s) => Ok(DefaultValue::Str(s.clone())),
        other => Err(format!(
            "`default` is a boolean, number or string, not {}",
            other.type_str()
        )),
    }
}
