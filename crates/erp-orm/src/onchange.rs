//! The `onchange` protocol (GRP §4.12) and `#[onchange(...)]` methods.
//!
//! A form sends the values it holds and the fields the user changed; the server builds an
//! in-memory record (with the edited record as origin, or a new record), applies the values,
//! marks the changed fields modified (their dependents recompute), runs the onchange methods of
//! the changed fields, then returns what differs from the values the form had, formatted like
//! `web_read` (x2many fields as command lists).
//!
//! - **Snapshot once.** The record's values (lines of x2many fields included) are read into one
//!   snapshot before the onchange methods run; the result is computed by comparing the record's
//!   current values with it, without building a second tree of every line (GRP §8 W5). Changed
//!   lines are formatted with one `web_read` per x2many field and the linked lines' stored values
//!   with one more, never one per line.
//! - **Mirror many2many of lines**.
//! - **Delegated (`_inherits`) fields**. They have no storage on the child, so
//!   every delegate link of the in-memory record points at an in-memory parent (the in-memory
//!   record of the database parent, or a new parent when the link is empty), whose inverse
//!   one2many lists the child. Form values of delegated fields, first-call defaults included, go
//!   to that parent under the parent's field name; changed ones are protected and marked modified
//!   there, so the parent's computed fields see them.
//!   A record or line moved to another
//!   parent leaves the old in-memory parent's one2many. Database parents are checked once for all
//!   records, by their ids only: one that does not exist or that the user may not read is an
//!   error. Onchange methods may assign delegated fields: the values are written on the in-memory
//!   parents.
//! - **x2many values inside lines**. A line's values may hold the line's own
//!   x2many values, command lists again, at any depth: they apply on the line's current value
//!   (its origin's lines, or nothing for a new line), level by level for all lines of a value
//!   together, so reads are batched; one2many sub-lines point at their in-memory line. Snapshots
//!   and diffs recurse through the sub-specifications, a line's x2many changes coming back as
//!   nested command lists; a linked line is compared with its stored record's lines by origin.
//!   Client input is bounded: command lists nested deeper than [`MAX_SPEC_DEPTH`] levels, or more
//!   than 100,000 line commands in one call, are `Validation` errors.
//!
//! Not supported yet: `change_default` fields with `ir.default` conditions (M3).

use std::sync::Arc;

use crate::html::input_cell as to_cell;
use erp_fields::convert::FieldRef;
use erp_fields::{Cell, Error, FieldType, RealId, RecId, Result, Value, ValueMap};
use erp_registry::{FieldId, ModelId, ModelSpec, Registry};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::Transaction;
use crate::access::Operation;
use crate::dispatch::{BoxFuture, Dispatch, MethodFn, call_method};
use crate::records::Records;
use crate::vals::RawVals;
use crate::web::read::{Row, web_read_rows};
use crate::web::spec::{Kwargs, MAX_SPEC_DEPTH, Spec};

/// Validated notification presentation options for an onchange warning.
///
/// Absence omits the corresponding wire member; explicit empty class text and `false` sticky
/// remain present. Options on a combined warning dialog are deliberately omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct WarningOptions {
    class_name: Option<String>,
    sticky: Option<bool>,
}

impl WarningOptions {
    /// Admit optional CSS identifier hooks and a notification lifetime option.
    ///
    /// Class text is bounded to 512 raw UTF-8 bytes, eight distinct tokens and 64 bytes per
    /// token. Tokens match `[A-Za-z_][A-Za-z0-9_-]*`, separated only by ASCII spaces; extra
    /// spaces are canonicalized. Token order is retained.
    ///
    /// # Errors
    /// Returns a [`erp_fields::ErrorKind::Validation`] error for oversized, malformed or
    /// duplicate class tokens. Validation occurs before retaining an owned class string.
    ///
    /// ```
    /// use erp_orm::onchange::WarningOptions;
    /// let options = WarningOptions::new(Some(" addon_notice  _urgent-2 "), Some(false))?;
    /// assert_eq!(options.class_name(), Some("addon_notice _urgent-2"));
    /// assert_eq!(options.sticky(), Some(false));
    /// # Ok::<(), erp_fields::Error>(())
    /// ```
    pub fn new(class_name: Option<&str>, sticky: Option<bool>) -> Result<Self> {
        let class_name = class_name.map(Self::normalize_class_name).transpose()?;
        Ok(Self { class_name, sticky })
    }

    /// The canonical class text, including an explicit empty string when supplied.
    pub fn class_name(&self) -> Option<&str> {
        self.class_name.as_deref()
    }

    /// The supplied lifetime option; `Some(false)` is distinct from absence.
    pub fn sticky(&self) -> Option<bool> {
        self.sticky
    }

    fn normalize_class_name(raw: &str) -> Result<String> {
        if raw.len() > 512 {
            return Err(Error::validation(
                "Invalid onchange warning className: exceeds 512 UTF-8 bytes",
            ));
        }
        // Empty segments are the repeated/leading/trailing ASCII spaces admitted.
        let tokens: Vec<&str> = raw.split(' ').filter(|token| !token.is_empty()).collect();
        for token in &tokens {
            let mut bytes = token.bytes();
            let valid_start = bytes
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
            if !valid_start || !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                return Err(Error::validation(
                    "Invalid onchange warning className: tokens must match [A-Za-z_][A-Za-z0-9_-]* and be separated by ASCII spaces",
                ));
            }
        }
        if tokens.len() > 8 {
            return Err(Error::validation(
                "Invalid onchange warning className: expected at most 8 tokens",
            ));
        }
        for (index, token) in tokens.iter().enumerate() {
            if token.len() > 64 {
                return Err(Error::validation(
                    "Invalid onchange warning className: token exceeds 64 bytes",
                ));
            }
            if tokens[..index].contains(token) {
                return Err(Error::validation(format!(
                    "Invalid onchange warning className: duplicate token '{token}'",
                )));
            }
        }
        Ok(tokens.join(" "))
    }
}

/// A warning returned by an onchange method, with optional validated presentation data.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Warning {
    /// Title (`"Warning"` when empty).
    pub title: Option<String>,
    /// Message (empty when absent).
    pub message: Option<String>,
    /// How the client shows it (`"dialog"` when empty; other strings select notifications).
    pub kind: Option<String>,
    /// Validated `className` and `sticky` options; default options omit both wire members.
    pub options: WarningOptions,
}

impl Warning {
    /// Set validated notification presentation options.
    ///
    /// # Errors
    /// Returns a validation error under the same class admission rules as [`WarningOptions::new`].
    ///
    /// ```
    /// use erp_orm::OnchangeWarning;
    /// let warning = OnchangeWarning::default().with_options(Some("addon_notice"), Some(true))?;
    /// assert_eq!(warning.options.sticky(), Some(true));
    /// # Ok::<(), erp_fields::Error>(())
    /// ```
    pub fn with_options(mut self, class_name: Option<&str>, sticky: Option<bool>) -> Result<Self> {
        self.options = WarningOptions::new(class_name, sticky)?;
        Ok(self)
    }

    fn normalized(self) -> NormalizedWarning {
        // These are the specified GRP-050 defaults, applied before complete-identity deduplication.
        let title = match self.title {
            Some(title) if !title.is_empty() => title,
            _ => "Warning".to_owned(),
        };
        let message = self.message.unwrap_or_default();
        let kind = match self.kind {
            Some(kind) if !kind.is_empty() => kind,
            _ => "dialog".to_owned(),
        };
        NormalizedWarning {
            title,
            message,
            kind,
            options: self.options,
        }
    }
}

/// Request-local warning identity after legacy defaults and option admission.
#[derive(Debug, PartialEq, Eq)]
struct NormalizedWarning {
    title: String,
    message: String,
    kind: String,
    options: WarningOptions,
}

impl NormalizedWarning {
    fn into_value(self) -> Value {
        let mut out = ValueMap::from([
            ("title".to_owned(), Value::str(self.title)),
            ("message".to_owned(), Value::str(self.message)),
            ("type".to_owned(), Value::str(self.kind)),
        ]);
        if let Some(class_name) = self.options.class_name {
            out.insert("className".to_owned(), Value::str(class_name));
        }
        if let Some(sticky) = self.options.sticky {
            out.insert("sticky".to_owned(), Value::Bool(sticky));
        }
        Value::Map(Arc::new(out))
    }
}

/// ```
/// use erp_orm::OnchangeResult;
/// use erp_fields::Value;
///
/// let empty = OnchangeResult::none();
/// assert!(empty.warning.is_none());
///
/// let warn = OnchangeResult::warning("Notice", "Value updated")
///     .value("state", Value::from("confirmed"));
/// assert!(warn.warning.is_some());
/// assert_eq!(warn.value.get("state"), Some(&Value::from("confirmed")));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OnchangeResult {
    /// Values assigned on the record.
    pub value: ValueMap,
    /// A warning for the user.
    pub warning: Option<Warning>,
}

impl OnchangeResult {
    /// Nothing to add.
    pub fn none() -> Self {
        Self::default()
    }

    /// A warning with a title and a message.
    pub fn warning(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            value: ValueMap::new(),
            warning: Some(Warning {
                title: Some(title.into()),
                message: Some(message.into()),
                kind: None,
                options: WarningOptions::default(),
            }),
        }
    }

    /// Also assign `field`.
    #[must_use]
    pub fn value(mut self, field: impl Into<String>, value: Value) -> Self {
        self.value.insert(field.into(), value);
        self
    }
}

/// An `#[onchange("a", "b")]` declaration; the method itself is in the model's method chain.
#[derive(Debug)]
pub struct OnchangeImpl {
    /// Contributing addon.
    pub addon: &'static str,
    /// Model of the `#[methods]` block.
    pub model: &'static str,
    /// Method name.
    pub method: &'static str,
    /// Fields whose change runs it.
    pub fields: &'static [&'static str],
    /// Source file.
    pub file: &'static str,
    /// Source line.
    pub line: u32,
}

/// Onchange declarations linked into the binary.
#[linkme::distributed_slice]
pub static ONCHANGE_IMPLS: [OnchangeImpl];

type OnchangeFn = MethodFn<(), OnchangeResult>;

/// Per model: field → onchange method names, in method-name order (built with the dispatch
/// tables; unknown field names and methods of the wrong signature fail the build).
#[derive(Debug, Default)]
pub(crate) struct OnchangeMethods {
    by_field: FxHashMap<(ModelId, FieldId), Vec<&'static str>>,
}

impl OnchangeMethods {
    pub(crate) fn build(registry: &Registry, dispatch: &Dispatch) -> Result<Self> {
        let mut by_field: FxHashMap<(ModelId, FieldId), Vec<&'static str>> = FxHashMap::default();
        for model in registry.models() {
            let owners: FxHashSet<(&str, &str)> =
                model.mro.iter().map(|d| (d.name, d.addon)).collect();
            for imp in ONCHANGE_IMPLS
                .iter()
                .filter(|i| owners.contains(&(i.model, i.addon)))
            {
                let chain = dispatch.implementations(model.id, imp.method);
                if chain.iter().any(|m| !(m.entry)().is::<OnchangeFn>()) {
                    return Err(Error::programming(format!(
                        "#[onchange] method '{}' of model '{}' at {}:{} must be \
                         async fn(this: &RecordSet<M>, tx: &mut Transaction) -> \
                         Result<OnchangeResult>",
                        imp.method, model.name, imp.file, imp.line
                    )));
                }
                for name in imp.fields {
                    let field = model.field(name).ok_or_else(|| {
                        Error::programming(format!(
                            "@onchange parameters must be field names: '{name}' is not a field \
                             of '{}' ({}:{})",
                            model.name, imp.file, imp.line
                        ))
                    })?;
                    let methods = by_field.entry((model.id, field.id)).or_default();
                    if !methods.contains(&imp.method) {
                        methods.push(imp.method);
                    }
                }
            }
        }
        for methods in by_field.values_mut() {
            methods.sort_unstable();
        }
        Ok(Self { by_field })
    }

    fn methods(&self, model: ModelId, field: FieldId) -> &[&'static str] {
        self.by_field
            .get(&(model, field))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

// ----- values --------------------------------------------------------------------------------

/// A line reference in a command: a database id or a client-side reference (`virtual_3`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum LineRef {
    Real(i64),
    Virtual(String),
}

/// x2many commands as forms send them (ids may be client references).
#[derive(Debug, Clone)]
enum FormCommand {
    Create(Option<String>, ValueMap),
    Update(LineRef, ValueMap),
    Remove(LineRef),
    Link(i64),
    Clear,
    Set(Vec<i64>),
}

fn bad_value(model: &ModelSpec, field: &str, value: &Value) -> Error {
    Error::validation(format!("Wrong value for {}.{field}: {value}", model.name))
}

fn line_ref(model: &ModelSpec, field: &str, value: &Value) -> Result<LineRef> {
    match value {
        Value::Int(i) if *i > 0 => Ok(LineRef::Real(*i)),
        Value::Str(s) if !s.is_empty() => Ok(LineRef::Virtual(s.to_string())),
        other => Err(bad_value(model, field, other)),
    }
}

fn vals_map(model: &ModelSpec, field: &str, value: Option<&Value>) -> Result<ValueMap> {
    match value {
        Some(Value::Map(m)) => Ok(m.as_ref().clone()),
        None | Some(Value::False) | Some(Value::Int(0)) => Ok(ValueMap::new()),
        Some(other) => Err(bad_value(model, field, other)),
    }
}

fn form_commands(model: &ModelSpec, field: &str, value: &Value) -> Result<Vec<FormCommand>> {
    let items = match value {
        Value::False => return Ok(vec![FormCommand::Clear]),
        Value::List(items) => items,
        other => return Err(bad_value(model, field, other)),
    };
    if items.is_empty() {
        return Ok(Vec::new());
    }
    if !matches!(items[0], Value::List(_)) {
        let ids = items
            .iter()
            .map(|v| match v {
                Value::Int(i) if *i > 0 => Ok(*i),
                other => Err(bad_value(model, field, other)),
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(vec![FormCommand::Set(ids)]);
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter() {
        let Value::List(triple) = item else {
            return Err(bad_value(model, field, item));
        };
        if triple.len() != 3 {
            return Err(bad_value(model, field, item));
        }
        let Value::Int(code) = triple[0] else {
            return Err(bad_value(model, field, item));
        };
        out.push(match code {
            0 => FormCommand::Create(
                match &triple[1] {
                    Value::Str(s) if !s.is_empty() => Some(s.to_string()),
                    _ => None,
                },
                vals_map(model, field, triple.get(2))?,
            ),
            1 => FormCommand::Update(
                line_ref(model, field, &triple[1])?,
                vals_map(model, field, triple.get(2))?,
            ),
            2 | 3 => FormCommand::Remove(line_ref(model, field, &triple[1])?),
            4 => match &triple[1] {
                Value::Int(i) if *i > 0 => FormCommand::Link(*i),
                other => return Err(bad_value(model, field, other)),
            },
            5 => FormCommand::Clear,
            6 => match &triple[2] {
                Value::List(ids) => FormCommand::Set(
                    ids.iter()
                        .map(|v| match v {
                            Value::Int(i) if *i > 0 => Ok(*i),
                            other => Err(bad_value(model, field, other)),
                        })
                        .collect::<Result<Vec<_>>>()?,
                ),
                other => return Err(bad_value(model, field, other)),
            },
            other => {
                return Err(Error::validation(format!(
                    "Unknown command {other} for {}.{field}: {item}",
                    model.name
                )));
            }
        });
    }
    Ok(out)
}

/// A form's value of a scalar or many2one field as a cell (the input converter; a many2one may
/// also be sent as `{'id': …, 'display_name': …}`).
fn form_cell(model: &ModelSpec, name: &str, value: &Value) -> Result<Cell> {
    let field = model.field(name).ok_or_else(|| {
        Error::validation(format!("Invalid field '{name}' on model '{}'", model.name))
    })?;
    // a dictionary is a client's map or a row of a `web_read` result
    let value = match (&field.ty, value.dict()) {
        (FieldType::Many2one { .. }, Some(dict)) if dict.contains("id") => dict.get("id")?,
        (FieldType::Many2one { .. }, Some(_)) => {
            return Err(Error::validation(format!(
                "Wrong value for {}.{name}: {value} (a many2one sent as a dictionary needs its \
                 'id')",
                model.name
            )));
        }
        _ => value.clone(),
    };
    to_cell(
        FieldRef {
            model: model.name,
            field: field.name,
            label: &field.string,
        },
        &field.ty,
        &value,
    )
}

/// The one value `cells` holds for singleton `record` (a missing or extra value is an engine bug).
fn single_cell(mut cells: Vec<Cell>, record: &Records, field: &str) -> Result<Cell> {
    match (cells.pop(), cells.is_empty()) {
        (Some(cell), true) => Ok(cell),
        _ => Err(Error::programming(format!(
            "onchange: expected one value of '{field}' for {record}"
        ))),
    }
}

/// The in-memory record of database id `origin`, an id a form sent: one that is not positive, or
/// too large for an in-memory record to carry it (2^62 and more), is a `Validation` error.
fn new_id_of(origin: i64) -> Result<RecId> {
    RealId::new(origin)
        .and_then(RecId::new_from_origin)
        .ok_or_else(|| Error::validation(format!("Invalid record id {origin}")))
}

impl Transaction {
    /// Forget everything cached for the in-memory `ids` of `model`: the edited record, and the
    /// form's own lines ([`apply_commands`] with `first_level`, [`origin_cell`]), which then start
    /// from their stored values. Nothing an earlier onchange of the same transaction cached on the
    /// same origin's in-memory record can be here, as the whole computation runs in an isolated
    /// scope that restores the cache; what is reset is what this call itself cached before (a line
    /// that two many2many fields of the form name).
    /// The many2many values that have
    /// a mirror are kept: other in-memory records hold the other side of those links
    /// ([`Transaction::link_mirrors`], the inverse updates of new-record writes), which forgetting
    /// the value would cut.
    fn reset_new(&mut self, model: ModelId, ids: &[RecId]) {
        let shared = Arc::clone(&self.shared);
        let fields: Vec<FieldId> = shared
            .registry
            .model_by_id(model)
            .fields
            .iter()
            // every field but the many2many fields that have a mirror
            .filter(|f| {
                !matches!(f.ty, FieldType::Many2many { .. })
                    || shared.relations.inverses(model, f.id).is_empty()
            })
            .map(|f| f.id)
            .collect();
        self.cache
            .model_mut(model)
            .invalidate(Some(fields.as_slice()), Some(ids));
        self.engine
            .context_mut()
            .invalidate(model, Some(fields.as_slice()), Some(ids));
        for f in fields {
            self.cache.model_mut(model).clear_pending(f, ids);
        }
    }

    fn set_new_cells(&mut self, record: &Records, cells: Vec<(FieldId, Cell)>) -> Result<()> {
        let mut patches = MirrorPatches::default();
        self.set_new_cells_into(record, cells, &mut patches)?;
        self.apply_mirror_patches(patches)
    }

    /// [`Transaction::set_new_cells`] for one of many records set together: the mirror patches are
    /// collected in `patches` instead of applied, so that the caller applies the patches of all
    /// records once ([`Transaction::apply_mirror_patches`]). Many records listing one line (a tag on
    /// every line) then cost one pass over that line's mirror value, not one per record.
    fn set_new_cells_into(
        &mut self,
        record: &Records,
        cells: Vec<(FieldId, Cell)>,
        patches: &mut MirrorPatches,
    ) -> Result<()> {
        let env = record.env().clone();
        let spec = record.spec();
        let cells = cells
            .into_iter()
            .map(|(fid, cell)| {
                self.admit_html_cell(spec, spec.field_by_id(fid), cell)
                    .map(|cell| (fid, cell))
            })
            .collect::<Result<Vec<_>>>()?;
        for (field, cell) in cells {
            let slot = self.slot(&env, record.model_id(), field)?;
            self.slot_set(
                &slot,
                record.model_id(),
                field,
                record.rec_ids().iter().map(|id| (*id, cell.clone())),
            )?;
            if let Cell::Ids(lines) = &cell {
                self.link_mirrors(record, field, lines, patches)?;
            }
        }
        Ok(())
    }

    /// Apply the collected mirror patches: one extension per mirror value, in the order the mirror
    /// values were first named.
    fn apply_mirror_patches(&mut self, patches: MirrorPatches) -> Result<()> {
        for ((model, field, line), records) in patches.groups {
            self.patch_new_x2many(model, field, line, &records)?;
        }
        Ok(())
    }

    /// When applied ([`Transaction::apply_mirror_patches`]),
    /// a mirror value already cached gets the records appended; one not read yet keeps them as a
    /// patch applied when it is read (no statement).
    /// Other fields, empty values and comodels without a mirror change nothing.
    ///
    /// Errors: `Programming` when a record or a line is a database record (the onchange caches
    /// values on in-memory records, whose lines are in-memory records).
    fn link_mirrors(
        &mut self,
        records: &Records,
        field: FieldId,
        lines: &[RecId],
        patches: &mut MirrorPatches,
    ) -> Result<()> {
        let shared = Arc::clone(&self.shared);
        let model = shared.registry.model_by_id(records.model_id());
        let fs = model.field_by_id(field);
        if !matches!(fs.ty, FieldType::Many2many { .. }) || lines.is_empty() || records.is_empty() {
            return Ok(());
        }
        let comodel = crate::relational::comodel_of(&shared.registry, fs)?;
        // a field of the comodel on the same table that points at another model (a relation
        // shared with an SQL view) does not hold this model's records
        let mirrors: Vec<FieldId> = shared
            .relations
            .inverses(model.id, field)
            .iter()
            .filter(|(m, f)| {
                *m == comodel.id && comodel.field_by_id(*f).ty.comodel() == Some(model.name)
            })
            .map(|(_, f)| *f)
            .collect();
        if mirrors.is_empty() {
            return Ok(());
        }
        if let Some(real) = records.rec_ids().iter().find(|id| !id.is_new()) {
            return Err(Error::programming(format!(
                "onchange: {}.{} cached on database record {real} of '{}': the onchange caches \
                 values on in-memory records only",
                model.name, fs.name, model.name
            )));
        }
        if let Some(real) = lines.iter().find(|id| !id.is_new()) {
            return Err(Error::programming(format!(
                "onchange: {}.{} of {records} lists database record {real} of '{}': the lines of \
                 an in-memory record are in-memory records",
                model.name, fs.name, comodel.name
            )));
        }
        let mut seen = FxHashSet::default();
        for line in lines.iter().copied().filter(|l| seen.insert(*l)) {
            for &mirror in &mirrors {
                patches.patch(comodel.id, mirror, line, records.rec_ids());
            }
        }
        Ok(())
    }
}

/// The records to add to mirror many2many values, collected while the values of several records
/// are set: per mirror value (`(model, field, line)`), the records to list in it, each mirror value
/// in the order it was first named.
type MirrorPatches = Groups<(ModelId, FieldId, RecId), RecId>;

/// Values grouped by key: keys in the order they were first named, values in the order they came,
/// membership decided by hash, so that grouping any number of values stays linear.
#[derive(Debug)]
struct Groups<K, V> {
    groups: Vec<(K, Vec<V>)>,
    index: FxHashMap<K, usize>,
    /// The `(key, value)` pairs added with [`Groups::add_once`].
    seen: FxHashSet<(K, V)>,
}

impl<K, V> Default for Groups<K, V> {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            index: FxHashMap::default(),
            seen: FxHashSet::default(),
        }
    }
}

impl<K: Copy + Eq + std::hash::Hash, V: Copy + Eq + std::hash::Hash> Groups<K, V> {
    /// The position of `key`'s group, made at the end when new.
    fn slot(&mut self, key: K) -> usize {
        let next = self.groups.len();
        let slot = *self.index.entry(key).or_insert(next);
        if slot == next {
            self.groups.push((key, Vec::new()));
        }
        slot
    }

    /// Add `values` to `key`'s group.
    fn add(&mut self, key: K, values: &[V]) {
        let slot = self.slot(key);
        // `slot` is an index of `groups` by construction
        self.groups[slot].1.extend_from_slice(values);
    }

    /// Add `value` to `key`'s group unless the group has it.
    fn add_once(&mut self, key: K, value: V) {
        if self.seen.insert((key, value)) {
            let slot = self.slot(key);
            self.groups[slot].1.push(value);
        }
    }
}

impl MirrorPatches {
    /// Add `records` to the mirror value `field` of `line` (of `model`).
    fn patch(&mut self, model: ModelId, field: FieldId, line: RecId, records: &[RecId]) {
        self.add((model, field, line), records);
    }
}

/// The database record an in-memory line was made from.
fn line_origin(line: RecId) -> Result<RecId> {
    line.origin()
        .map(RecId::real)
        .ok_or_else(|| Error::programming(format!("onchange: line {line} has no origin")))
}

async fn seed_lines(
    tx: &mut Transaction,
    env: &crate::Env,
    comodel: ModelId,
    lines: &[RecId],
    sub: &Spec,
) -> Result<()> {
    let todo: Vec<RecId> = lines
        .iter()
        .copied()
        .filter(|l| l.origin().is_some())
        .collect();
    if todo.is_empty() || sub.is_empty() {
        return Ok(());
    }
    let mut seen = FxHashSet::default();
    let origins: Vec<RecId> = todo
        .iter()
        .map(|l| line_origin(*l))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|o| seen.insert(*o))
        .collect();
    let registry = Arc::clone(env.registry());
    let model = registry.model_by_id(comodel);
    let origin_records = Records::from_parts(
        env.clone(),
        comodel,
        Arc::from(origins.clone()),
        Arc::from(origins.clone()),
    );
    for name in sub.names() {
        let Some(field) = model.field(name) else {
            continue;
        };
        if field.ty.is_x2many()
            || matches!(field.ty, FieldType::Id)
            || tx.shared.graph.model(comodel).is_builtin_display_name(field.id)
            // a line's delegated values are read through its link (an in-memory parent once
            // the line receives delegated values), not stored on the line
            || field.inherited.is_some()
        {
            continue;
        }
        let missing: Vec<RecId> = todo
            .iter()
            .copied()
            .filter(|l| !tx.cache.contains(comodel, field.id, *l))
            .collect();
        if missing.is_empty() {
            continue;
        }
        let cells = origin_records.cells(tx, name).await?;
        if cells.len() != origins.len() {
            return Err(Error::programming(format!(
                "onchange: {} values of '{name}' for {} lines of '{}'",
                cells.len(),
                origins.len(),
                model.name
            )));
        }
        let by_origin: FxHashMap<RecId, Cell> = origins.iter().copied().zip(cells).collect();
        let values: Vec<(RecId, Cell)> = missing
            .iter()
            .map(|l| {
                let origin = line_origin(*l)?;
                let cell = by_origin.get(&origin).cloned().ok_or_else(|| {
                    Error::programming(format!(
                        "onchange: no value of '{name}' read for {origin} of '{}'",
                        model.name
                    ))
                })?;
                Ok((*l, cell))
            })
            .collect::<Result<_>>()?;
        let slot = tx.slot(env, comodel, field.id)?;
        tx.slot_set(&slot, comodel, field.id, values)?;
    }
    Ok(())
}

/// Fields of in-memory parents that received delegated values: `(model, field, record)` at the end
/// of each `_inherits` chain, in assignment order.
type Owners = Vec<(ModelId, FieldId, RecId)>;

/// Fields marked modified on records of one model, as [`Transaction::modified`] takes them.
type Changes = Vec<(FieldId, Vec<RecId>)>;

/// Most line commands in one onchange's values, every x2many value of every level together (a
/// `SET` counts its ids, a `CLEAR` nothing): each command builds or names at most one line, so this
/// bounds the in-memory records one call builds from its input and the database records it reads
/// for them (one read per level and field, of at most this many ids).
const MAX_FORM_COMMANDS: usize = 100_000;

/// The x2many commands of `field` are nested deeper than [`MAX_SPEC_DEPTH`] levels.
fn nested_too_deep(model: &ModelSpec, field: &str) -> Error {
    Error::validation(format!(
        "Wrong value for {}.{field}: x2many commands nested deeper than {MAX_SPEC_DEPTH} levels",
        model.name
    ))
}

/// The level of the x2many values of a record at `level` (0: the form's record and its parents),
/// refused beyond [`MAX_SPEC_DEPTH`]. Nesting of x2many command lists is bounded by the depth a
/// specification may have (the form's own x2many values are level 1, those inside their lines level
/// 2, and so on, GRP §4.12): one constant, so every value a specification can describe is accepted
/// and nothing deeper is.
fn level_below(level: usize, model: &ModelSpec, field: &str) -> Result<usize> {
    level
        .checked_add(1)
        .filter(|l| *l <= MAX_SPEC_DEPTH)
        .ok_or_else(|| nested_too_deep(model, field))
}

/// What applying a form's values collects across records and levels: the parent fields that
/// received delegated values, and the line commands applied so far (bounded by
/// [`MAX_FORM_COMMANDS`] for the whole call).
#[derive(Debug, Default)]
struct Applying {
    /// Only the first onchange of an originally empty recordset supplies initial
    /// native inputs. In-memory ids alone cannot identify that phase: later edits
    /// and persisted-origin forms also operate on new ids.
    first_new_form: bool,
    /// `(model, field, record)` at the end of each `_inherits` chain, in assignment order, once
    /// each.
    owners: Owners,
    seen: FxHashSet<(ModelId, FieldId, RecId)>,
    commands: usize,
}

impl Applying {
    /// Record that `field` of in-memory parent `record` received a delegated value.
    fn own(&mut self, model: ModelId, field: FieldId, record: RecId) {
        if self.seen.insert((model, field, record)) {
            self.owners.push((model, field, record));
        }
    }

    /// Count the commands of one x2many value before any of them is applied.
    fn charge(&mut self, commands: &[FormCommand], model: &ModelSpec, field: &str) -> Result<()> {
        let too_many = || {
            Error::validation(format!(
                "Wrong value for {}.{field}: more than {MAX_FORM_COMMANDS} x2many commands in one \
                 onchange",
                model.name
            ))
        };
        let mut total = self.commands;
        for command in commands {
            // a `SET` names each of its ids, a `CLEAR` names no line (a value of `false`, which a
            // first call fills in for every field of the specification, is one: it must not eat
            // into the budget of the commands of the same field), every other command one line at
            // most
            let weight = match command {
                FormCommand::Set(ids) => ids.len(),
                FormCommand::Clear => 0,
                FormCommand::Create(..)
                | FormCommand::Update(..)
                | FormCommand::Remove(_)
                | FormCommand::Link(_) => 1,
            };
            total = total
                .checked_add(weight)
                .filter(|t| *t <= MAX_FORM_COMMANDS)
                .ok_or_else(too_many)?;
        }
        self.commands = total;
        Ok(())
    }

    /// The parent fields recorded so far, which the caller marks modified; forgotten here.
    fn take_owners(&mut self) -> Owners {
        self.seen.clear();
        std::mem::take(&mut self.owners)
    }

    /// Forget the parent fields recorded so far (initial values are the form's state, not
    /// changes).
    fn forget_owners(&mut self) {
        self.seen.clear();
        self.owners.clear();
    }
}

/// Values of delegated
/// (`_inherits`) fields go to the record's in-memory parents ([`ensure_parents`]) under the
/// parents' field names, as inherited fields have no storage of their own; the parent fields that
/// received them are recorded in `applying`.
fn update_cache<'a>(
    tx: &'a mut Transaction,
    record: &'a Records,
    values: &'a ValueMap,
    spec: &'a Spec,
    applying: &'a mut Applying,
    level: usize,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let registry = Arc::clone(record.env().registry());
        let model = registry.model_by_id(record.model_id());
        // Convert and normalize the whole scalar input before publishing any value. In
        // particular, an inherited currency supplied with an own amount belongs to
        // this same staging row. Delegated amounts normalize once at their owner in
        // the recursive update_cache below.
        let monetary: Vec<FieldId> = values
            .keys()
            .filter_map(|name| model.field(name))
            .filter(|field| {
                field.inherited.is_none() && matches!(field.ty, FieldType::Monetary { .. })
            })
            .map(|field| field.id)
            .collect();
        let mut normalized;
        let values = if monetary.is_empty() {
            values
        } else {
            let budget = crate::monetary::StagingBudget::new(1);
            budget.check_grid(1, monetary.len())?;
            let shared = Arc::clone(&tx.shared);
            let slots =
                crate::monetary::staging_slots(model, shared.graph.model(model.id), &monetary);
            let mut rows = vec![vec![None; slots.len()]];
            for (name, value) in values {
                if name == "id" {
                    continue;
                }
                let field = model.field(name).ok_or_else(|| {
                    Error::validation(format!("Invalid field '{name}' on model '{}'", model.name))
                })?;
                if !field.ty.is_x2many()
                    && let Ok(slot) = slots.binary_search(&field.id)
                {
                    rows[0][slot] = Some(form_cell(model, name, value)?);
                }
            }
            crate::monetary::normalize(
                tx,
                record.env(),
                crate::monetary::Stage {
                    initial: applying.first_new_form && single_id(record)?.origin().is_none(),
                    spec: model,
                    ids: Some(record.rec_ids()),
                    fields: &slots,
                    rows: &mut rows,
                    included: None,
                    budget: &budget,
                    precharged: None,
                    kind: crate::monetary::StageKind::Direct,
                },
                &monetary,
                false,
                true,
            )
            .await?;
            normalized = values.clone();
            for field in monetary {
                let slot = slots
                    .binary_search(&field)
                    .map_err(|_| Error::programming("Monetary onchange staging has no slot"))?;
                match rows[0][slot].take() {
                    Some(Cell::Decimal(amount)) => {
                        normalized.insert(
                            model.field_by_id(field).name.to_owned(),
                            Value::Decimal(amount),
                        );
                    }
                    Some(Cell::Null) => {}
                    _ => {
                        return Err(Error::programming(
                            "Monetary onchange staging lost an input",
                        ));
                    }
                }
            }
            &normalized
        };
        let mut cells = Vec::new();
        let mut x2many: Vec<(&str, &Value)> = Vec::new();
        let mut delegated: Vec<(&str, &Value)> = Vec::new();
        for (name, value) in values {
            if name == "id" {
                continue;
            }
            let field = model.field(name).ok_or_else(|| {
                Error::validation(format!("Invalid field '{name}' on model '{}'", model.name))
            })?;
            if field.inherited.is_some() {
                delegated.push((name, value));
            } else if field.ty.is_x2many() {
                x2many.push((name, value));
            } else if field.translate {
                let cell = form_cell(model, name, value)?;
                let active = tx.active_languages().await?;
                let old = tx.cache.get(model.id, field.id, single_id(record)?);
                let json_cell = crate::translated::apply_write_unchecked(
                    field,
                    old.as_ref(),
                    &cell,
                    record.env().lang(),
                    &active,
                )?;
                let json_cell = tx.admit_html_cell(model, field, json_cell)?;
                cells.push((field.id, json_cell));
            } else {
                let cell = tx.admit_html_cell(model, field, form_cell(model, name, value)?)?;
                cells.push((field.id, cell));
            }
        }
        // the record's own values first: they may hold the delegate links, whose old in-memory
        // parents stop listing the record (the new ones list it once `ensure_parents` ran)
        let mut relinked: Vec<(FieldId, Option<Cell>, Cell)> = Vec::new();
        let mut own = Vec::with_capacity(cells.len());
        for (f, cell) in cells {
            if !model.inherits.iter().any(|(_, d)| *d == f) {
                own.push((f, cell));
                continue;
            }
            let old = tx.cache.get(model.id, f, single_id(record)?);
            let kept = old
                .as_ref()
                .and_then(crate::relational::id_of)
                .and_then(RecId::origin)
                .is_some_and(|o| crate::relational::id_of(&cell) == Some(RecId::real(o)));
            // the database parent whose in-memory record already is the link: kept as it is
            if kept {
                continue;
            }
            relinked.push((f, old, cell.clone()));
            own.push((f, cell));
        }
        tx.set_new_cells(record, own)?;
        for (delegate, old, new) in relinked {
            if old.as_ref().is_none_or(|o| !o.same_value(&new)) {
                relink(tx, record, delegate, &[old], &new).await?;
            }
        }
        for (name, value) in x2many {
            let field = model.field(name).ok_or_else(|| {
                Error::programming(format!("field '{name}' vanished from '{}'", model.name))
            })?;
            let below = level_below(level, model, name)?;
            let commands = form_commands(model, name, value)?;
            applying.charge(&commands, model, name)?;
            let empty = Spec::new();
            let sub = spec
                .get(name)
                .and_then(|i| i.fields.as_ref())
                .unwrap_or(&empty);
            let applied = apply_commands(tx, record, field.id, commands, sub, below == 1).await?;
            tx.set_new_cells(
                record,
                vec![(field.id, Cell::Ids(applied.lines.clone().into()))],
            )?;
            link_lines(tx, record, field.id, &applied.lines, &[]).await?;
            let comodel = crate::relational::comodel_of(&registry, field)?;
            let co = Records::empty(record.env().clone(), comodel.id);
            update_lines(tx, &co, applied.later, sub, applying, below).await?;
        }
        if model.inherits.is_empty() {
            return Ok(());
        }
        ensure_parents(tx, record).await?;
        // grouped per parent record, recursively for several `_inherits` levels
        let mut groups: Vec<(Records, ValueMap, Spec)> = Vec::new();
        for (name, value) in delegated {
            let (owner, owner_field) = delegated_target(tx, record, name).await?;
            // a value the specification does not list has no sub-specification (it is applied,
            // not returned)
            let item = spec.get(name).cloned().unwrap_or_default();
            let slot = match groups.iter().position(|(r, _, _)| {
                r.model_id() == owner.model_id() && r.rec_ids() == owner.rec_ids()
            }) {
                Some(i) => i,
                None => {
                    groups.push((owner, ValueMap::new(), Spec::new()));
                    groups.len() - 1
                }
            };
            let (owner, vals, sub) = &mut groups[slot];
            let fs = owner.spec().field(owner_field).ok_or_else(|| {
                Error::programming(format!(
                    "'{owner_field}' is not a field of '{}'",
                    owner.model_name()
                ))
            })?;
            // a field delegated further is recorded by the parent's own update; a field set twice
            // (initial and changed value, several lines) is recorded once
            if fs.inherited.is_none() {
                for id in owner.rec_ids() {
                    applying.own(owner.model_id(), fs.id, *id);
                }
            }
            vals.insert(owner_field.to_owned(), value.clone());
            *sub = std::mem::take(sub).with(owner_field, item);
        }
        // the parents are at the record's level: delegation adds no x2many level
        for (owner, vals, sub) in groups {
            update_cache(tx, &owner, &vals, &sub, applying, level).await?;
        }
        Ok(())
    })
}

/// Values of an in-memory line applied once its x2many value is set: the line's own x2many values
/// (a nested command list each) and its delegated values.
#[derive(Debug)]
struct LineValues {
    line: RecId,
    x2many: ValueMap,
    delegated: ValueMap,
}

/// The delegated values come
/// last, their in-memory parents made for every line at once.
fn update_lines<'a>(
    tx: &'a mut Transaction,
    co: &'a Records,
    later: Vec<LineValues>,
    spec: &'a Spec,
    applying: &'a mut Applying,
    level: usize,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        if later.is_empty() {
            return Ok(());
        }
        let registry = Arc::clone(co.env().registry());
        let comodel = registry.model_by_id(co.model_id());
        let mut by_field: FxHashMap<FieldId, Vec<(RecId, &Value)>> = FxHashMap::default();
        for item in &later {
            for (name, value) in &item.x2many {
                let field = comodel.field(name).ok_or_else(|| {
                    Error::programming(format!("field '{name}' vanished from '{}'", comodel.name))
                })?;
                by_field
                    .entry(field.id)
                    .or_default()
                    .push((item.line, value));
            }
        }
        for field in &comodel.fields {
            let Some(targets) = by_field.remove(&field.id) else {
                continue;
            };
            let below = level_below(level, comodel, field.name)?;
            // every line's commands are checked and counted before any is applied
            let mut parsed: Vec<(RecId, Vec<FormCommand>)> = Vec::with_capacity(targets.len());
            for (line, value) in targets {
                let commands = form_commands(comodel, field.name, value)?;
                applying.charge(&commands, comodel, field.name)?;
                parsed.push((line, commands));
            }
            let empty = Spec::new();
            let sub = spec
                .get(field.name)
                .and_then(|i| i.fields.as_ref())
                .unwrap_or(&empty);
            // the lines' current values, read together: in-memory lines with an origin read their
            // origins' values in one query
            let mut seen = FxHashSet::default();
            let owners: Vec<RecId> = parsed
                .iter()
                .map(|(l, _)| *l)
                .filter(|l| seen.insert(*l))
                .collect();
            let current = co
                .browse_ids(owners.iter().copied())
                .cells(tx, field.name)
                .await?;
            let sub_model = crate::relational::comodel_of(&registry, field)?;
            // the sub-lines the lines may link (a one2many): the prefetch set of the moved
            // sub-lines, so that what the field's domain reads is read for all of them together
            let mut sub_lines: Vec<RecId> = Vec::new();
            if let FieldType::One2many { inverse, .. } = &field.ty {
                // the inverse fields of the sub-lines the lines may link, read together: their
                // current sub-lines and the database lines their commands name (a new sub-line has
                // no stored inverse to read); `link_lines` then finds them in the cache
                let mut seen = FxHashSet::default();
                let mut subs: Vec<RecId> = Vec::new();
                for cell in &current {
                    if let Cell::Ids(ids) = cell {
                        subs.extend(ids.iter().copied().filter(|l| seen.insert(*l)));
                    }
                }
                for (_, commands) in &parsed {
                    for command in commands {
                        let named: &[i64] = match command {
                            FormCommand::Update(LineRef::Real(id), _) | FormCommand::Link(id) => {
                                std::slice::from_ref(id)
                            }
                            FormCommand::Set(ids) => ids,
                            // the other commands name no database line
                            FormCommand::Update(LineRef::Virtual(_), _)
                            | FormCommand::Create(..)
                            | FormCommand::Remove(_)
                            | FormCommand::Clear => &[],
                        };
                        for id in named {
                            let sub_line = new_id_of(*id)?;
                            if seen.insert(sub_line) {
                                subs.push(sub_line);
                            }
                        }
                    }
                }
                if !subs.is_empty() {
                    Records::empty(co.env().clone(), sub_model.id)
                        .browse_ids(subs.iter().copied())
                        .cells(tx, inverse)
                        .await?;
                }
                sub_lines = subs;
            }
            let mut next: Vec<LineValues> = Vec::new();
            // the mirror many2many patches of all the lines at once: a tag on every line is one
            // extension of the tag's mirror value, not one per line
            let mut mirrors = MirrorPatches::default();
            for (line, commands) in parsed {
                let record = co.browse_ids([line]);
                let applied = apply_commands(tx, &record, field.id, commands, sub, false).await?;
                tx.set_new_cells_into(
                    &record,
                    vec![(field.id, Cell::Ids(applied.lines.clone().into()))],
                    &mut mirrors,
                )?;
                link_lines(tx, &record, field.id, &applied.lines, &sub_lines).await?;
                next.extend(applied.later);
            }
            tx.apply_mirror_patches(mirrors)?;
            let sub_co = Records::empty(co.env().clone(), sub_model.id);
            update_lines(tx, &sub_co, next, sub, applying, below).await?;
        }
        // the parents of every line with delegated values together: one read of their links and
        // of their database parents, not one per line
        let delegated: Vec<(RecId, &ValueMap)> = later
            .iter()
            .filter(|l| !l.delegated.is_empty())
            .map(|l| (l.line, &l.delegated))
            .collect();
        if delegated.is_empty() {
            return Ok(());
        }
        ensure_parents(tx, &co.browse_ids(delegated.iter().map(|(l, _)| *l))).await?;
        for (line, vals) in delegated {
            update_cache(tx, &co.browse_ids([line]), vals, spec, applying, level).await?;
        }
        Ok(())
    })
}

/// The in-memory parent record and its field holding the value of delegated field `name` of
/// `record` (one `_inherits` level; the parent's field may itself be delegated). The record's
/// links must point at in-memory parents ([`ensure_parents`]).
async fn delegated_target(
    tx: &mut Transaction,
    record: &Records,
    name: &str,
) -> Result<(Records, &'static str)> {
    let registry = Arc::clone(record.env().registry());
    let model = registry.model_by_id(record.model_id());
    let inherited = model.field(name).and_then(|f| f.inherited).ok_or_else(|| {
        Error::programming(format!("{}.{name} is not a delegated field", model.name))
    })?;
    let delegate = model.field_by_id(inherited.delegate).name;
    let parent = record.related(tx, delegate).await?;
    if parent.len() != 1 || parent.rec_ids().iter().any(|p| !p.is_new()) {
        return Err(Error::programming(format!(
            "onchange: {}.{delegate} of {record} does not point at an in-memory parent",
            model.name
        )));
    }
    let field = registry
        .model_by_id(inherited.parent)
        .field_by_id(inherited.parent_field)
        .name;
    Ok((parent, field))
}

/// Many2one `field` of in-memory `records` now holds `new` instead of `old` (one per record):
/// the cached inverse one2many values follow ([`Transaction::update_inverses`]). The values of old
/// targets that are in-memory records are loaded first (their origin's lines plus pending
/// additions), so the records leave them even when nothing read them yet.
async fn relink(
    tx: &mut Transaction,
    records: &Records,
    field: FieldId,
    old: &[Option<Cell>],
    new: &Cell,
) -> Result<()> {
    let mut seen = FxHashSet::default();
    let in_memory: Vec<RecId> = old
        .iter()
        .flatten()
        .filter_map(crate::relational::id_of)
        .filter(|o| o.is_new() && seen.insert(*o))
        .collect();
    if !in_memory.is_empty() {
        let registry = Arc::clone(records.env().registry());
        let inverses = tx
            .shared
            .relations
            .inverses(records.model_id(), field)
            .to_vec();
        for (co_model, o2m) in inverses {
            let name = registry.model_by_id(co_model).field_by_id(o2m).name;
            Records::empty(records.env().clone(), co_model)
                .browse_ids(in_memory.iter().copied())
                .cells(tx, name)
                .await?;
        }
    }
    tx.update_inverses(records.model_id(), field, records, old, new)
        .await
}

/// The one id of singleton `record` (an onchange works on one record at a time).
fn single_id(record: &Records) -> Result<RecId> {
    match record.rec_ids() {
        [id] => Ok(*id),
        _ => Err(Error::programming(format!(
            "onchange: expected one record, not {record}"
        ))),
    }
}

/// The parents' inverse one2many values list the records (so their aggregates over the
/// delegating records see them) and an old in-memory parent's no longer does. Several `_inherits`
/// levels are followed.
fn ensure_parents<'a>(tx: &'a mut Transaction, records: &'a Records) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let registry = Arc::clone(records.env().registry());
        let model = registry.model_by_id(records.model_id());
        if model.inherits.is_empty() || records.is_empty() {
            return Ok(());
        }
        if let Some(real) = records.rec_ids().iter().find(|i| !i.is_new()) {
            return Err(Error::programming(format!(
                "onchange: in-memory parents are made for in-memory records, not {real} of '{}'",
                model.name
            )));
        }
        let mut seen = FxHashSet::default();
        let ids: Vec<RecId> = records
            .rec_ids()
            .iter()
            .copied()
            .filter(|i| seen.insert(*i))
            .collect();
        let records = records.browse_ids(ids.iter().copied());
        for &(parent_model, delegate) in &model.inherits {
            let delegate_name = model.field_by_id(delegate).name;
            let currents = records.cells(tx, delegate_name).await?;
            if currents.len() != ids.len() {
                return Err(Error::programming(format!(
                    "onchange: {} values of '{delegate_name}' for {records}",
                    currents.len()
                )));
            }
            let empty = Records::empty(records.env().clone(), parent_model);
            let mut seen = FxHashSet::default();
            // the database parents (an empty link names none)
            let stored: Vec<RecId> = currents
                .iter()
                .filter_map(crate::relational::id_of)
                .filter(|p| !p.is_new() && seen.insert(*p))
                .collect();
            if !stored.is_empty() {
                check_readable(tx, &empty.browse_ids(stored)).await?;
            }
            // per parent: the records to point at it, with their current links
            let mut groups: Vec<(RecId, Vec<RecId>, Vec<Option<Cell>>)> = Vec::new();
            let mut slot_of: FxHashMap<RecId, usize> = FxHashMap::default();
            for (id, current) in ids.iter().zip(currents) {
                // no `reset_new` for the in-memory record of a database parent: it may already be
                // in use in this onchange (the form's record when a line points at it, another
                // line's parent), and the isolated scope drops what an earlier onchange left
                let parent = match crate::relational::id_of(&current) {
                    Some(p) if p.is_new() => p,
                    Some(p) => {
                        single_id(&tx.new_record(&empty, RawVals::new(), Some(p), None).await?)?
                    }
                    None => single_id(&tx.new_record(&empty, RawVals::new(), None, None).await?)?,
                };
                let slot = *slot_of.entry(parent).or_insert_with(|| {
                    groups.push((parent, Vec::new(), Vec::new()));
                    groups.len() - 1
                });
                if !Cell::Id(parent.get()).same_value(&current) {
                    groups[slot].1.push(*id);
                    groups[slot].2.push(Some(current));
                }
            }
            let parents: Vec<RecId> = groups.iter().map(|(p, _, _)| *p).collect();
            for (parent, moved, old) in groups {
                if moved.is_empty() {
                    continue;
                }
                let link = Cell::Id(parent.get());
                let moved = records.browse_ids(moved);
                tx.set_new_cells(&moved, vec![(delegate, link.clone())])?;
                relink(tx, &moved, delegate, &old, &link).await?;
            }
            ensure_parents(tx, &empty.browse_ids(parents)).await?;
        }
        Ok(())
    })
}

/// Check database `records` as the onchange's user, reading their ids only (one statement when the
/// user may read them all): the model's read access, the access domain (a record it hides is an
/// `Access` error, as on reads) and existence (`Missing`). No column is read, so a field-level
/// restriction of a column (field groups, enforced) cannot refuse a record
/// the user may read; the in-memory records of these origins read their values when asked.
async fn check_readable(tx: &mut Transaction, records: &Records) -> Result<()> {
    tx.check_records(records)?;
    let env = records.env();
    let spec = records.spec();
    let mut seen = FxHashSet::default();
    let mut ids: Vec<RealId> = Vec::with_capacity(records.len());
    for id in records.rec_ids() {
        let real = id.as_real().ok_or_else(|| {
            Error::programming(format!(
                "onchange: {id} of '{}' is not a database record to check",
                spec.name
            ))
        })?;
        if seen.insert(real) {
            ids.push(real);
        }
    }
    if ids.is_empty() {
        return Ok(());
    }
    if !env.su() {
        tx.check_model(env, spec.id, Operation::Read)?;
    }
    let restriction = crate::search::read_restriction(tx, records).await?;
    if let Some(filter) = &restriction {
        crate::search::flush_read_inputs(tx, &filter.fields).await?;
    }
    let found = crate::search::fetch_rows(tx, spec, &[], &ids, restriction.as_deref()).await?;
    let absent: Vec<RealId> = ids
        .iter()
        .copied()
        .filter(|id| !found.contains(&RecId::real(*id)))
        .collect();
    let Some(&first) = absent.first() else {
        return Ok(());
    };
    // hidden by the access domain rather than deleted
    if restriction.is_some()
        && let Some(error) = tx
            .record_access_error(env, spec.id, Operation::Read, &absent)
            .await?
    {
        return Err(error);
    }
    Err(crate::read::missing_error(records, RecId::real(first)))
}

/// Every in-memory parent of `record`, nearest first (after [`ensure_parents`]).
fn parents_of<'a>(
    tx: &'a mut Transaction,
    record: &'a Records,
) -> BoxFuture<'a, Result<Vec<Records>>> {
    Box::pin(async move {
        let registry = Arc::clone(record.env().registry());
        let model = registry.model_by_id(record.model_id());
        let mut out = Vec::new();
        for &(_, delegate) in &model.inherits {
            let name = model.field_by_id(delegate).name;
            let parent = record.related(tx, name).await?;
            if parent.is_empty() {
                return Err(Error::programming(format!(
                    "onchange: {}.{name} of {record} has no in-memory parent",
                    model.name
                )));
            }
            let above = parents_of(tx, &parent).await?;
            out.push(parent);
            out.extend(above);
        }
        Ok(out)
    })
}

/// Lines that pointed elsewhere leave their old parents' cached one2many values. The
/// lines of a many2many get the record in the comodel's mirror when the value is cached
/// ([`Transaction::set_new_cells`]). `siblings` are the in-memory lines of the other records set
/// together with this one: they share the prefetch set of the lines moved here, so that the
/// fields the one2many's domain reads are read once for all of them.
async fn link_lines(
    tx: &mut Transaction,
    record: &Records,
    field: FieldId,
    lines: &[RecId],
    siblings: &[RecId],
) -> Result<()> {
    let registry = Arc::clone(record.env().registry());
    let model = registry.model_by_id(record.model_id());
    let fs = model.field_by_id(field);
    let FieldType::One2many { inverse, .. } = &fs.ty else {
        return Ok(());
    };
    if lines.is_empty() {
        return Ok(());
    }
    let owner = single_id(record)?;
    let comodel = crate::relational::comodel_of(&registry, fs)?;
    let inverse = comodel.field(inverse).ok_or_else(|| {
        Error::programming(format!(
            "{}.{}: inverse field '{inverse}' is not a field of '{}'",
            model.name, fs.name, comodel.name
        ))
    })?;
    let target = match inverse.ty {
        FieldType::Many2one { .. } => Cell::Id(owner.get()),
        FieldType::Many2oneReference { .. } => Cell::Null,
        // the registry refuses other inverses
        ref other => {
            return Err(Error::programming(format!(
                "{}.{}: inverse field '{}.{}' is a {} field",
                model.name,
                fs.name,
                comodel.name,
                inverse.name,
                other.type_name()
            )));
        }
    };
    let mut seen = FxHashSet::default();
    let distinct: Vec<RecId> = lines.iter().copied().filter(|l| seen.insert(*l)).collect();
    let mut prefetch = distinct.clone();
    prefetch.extend(siblings.iter().copied().filter(|l| seen.insert(*l)));
    let prefetch: Arc<[RecId]> = Arc::from(prefetch);
    let co = Records::from_parts(
        record.env().clone(),
        comodel.id,
        Arc::from(distinct.clone()),
        Arc::clone(&prefetch),
    );
    let current = co.cells(tx, inverse.name).await?;
    if current.len() != distinct.len() {
        return Err(Error::programming(format!(
            "onchange: {} values of '{}' for {co}",
            current.len(),
            inverse.name
        )));
    }
    let mut moved: Vec<RecId> = Vec::new();
    let mut old: Vec<Option<Cell>> = Vec::new();
    for (line, cell) in distinct.iter().zip(current) {
        if !cell.same_value(&target) {
            moved.push(*line);
            old.push(Some(cell));
        }
    }
    if moved.is_empty() {
        return Ok(());
    }
    let slot = tx.slot(record.env(), comodel.id, inverse.id)?;
    tx.slot_set(
        &slot,
        comodel.id,
        inverse.id,
        moved.iter().map(|l| (*l, target.clone())),
    )?;
    let moved = Records::from_parts(record.env().clone(), comodel.id, Arc::from(moved), prefetch);
    relink(tx, &moved, inverse.id, &old, &target).await
}

/// A removed line leaves an
/// empty slot, so removing and adding lines stays linear in the number of commands.
#[derive(Debug, Default)]
struct LineSet {
    slots: Vec<Option<RecId>>,
    index: FxHashMap<RecId, usize>,
}

impl LineSet {
    fn new(lines: impl IntoIterator<Item = RecId>) -> Self {
        let mut set = Self::default();
        for line in lines {
            set.add(line);
        }
        set
    }

    fn contains(&self, line: RecId) -> bool {
        self.index.contains_key(&line)
    }

    /// Add `line` at the end, unless it is there already.
    fn add(&mut self, line: RecId) {
        if !self.index.contains_key(&line) {
            self.index.insert(line, self.slots.len());
            self.slots.push(Some(line));
        }
    }

    fn remove(&mut self, line: RecId) {
        if let Some(slot) = self.index.remove(&line).and_then(|i| self.slots.get_mut(i)) {
            *slot = None;
        }
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.index.clear();
    }

    fn into_vec(self) -> Vec<RecId> {
        self.slots.into_iter().flatten().collect()
    }
}

/// An x2many value once its commands are applied: the in-memory lines, and per command the line
/// values that wait for the lines to be linked ([`update_lines`]).
#[derive(Debug)]
struct AppliedLines {
    lines: Vec<RecId>,
    later: Vec<LineValues>,
}

/// A line's
/// own scalar values go into `new_record`; its x2many and delegated values come back in `later`.
async fn apply_commands(
    tx: &mut Transaction,
    record: &Records,
    field: FieldId,
    commands: Vec<FormCommand>,
    sub: &Spec,
    first_level: bool,
) -> Result<AppliedLines> {
    let registry = Arc::clone(record.env().registry());
    let model = registry.model_by_id(record.model_id());
    let fs = model.field_by_id(field);
    let comodel = crate::relational::comodel_of(&registry, fs)?;
    let co = Records::empty(record.env().clone(), comodel.id);
    let mut lines = match single_cell(record.cells(tx, fs.name).await?, record, fs.name)? {
        Cell::Ids(ids) => LineSet::new(ids.iter().copied()),
        Cell::Null => LineSet::default(),
        other => {
            return Err(Error::programming(format!(
                "onchange: {}.{} of {record} holds {other:?}, not a list of records",
                model.name, fs.name
            )));
        }
    };
    if first_level {
        // the database lines the commands name start from their stored values
        let mut named: Vec<RecId> = Vec::new();
        for command in &commands {
            match command {
                FormCommand::Update(LineRef::Real(id), _) | FormCommand::Link(id) => {
                    named.push(new_id_of(*id)?);
                }
                FormCommand::Set(ids) => {
                    for id in ids {
                        named.push(new_id_of(*id)?);
                    }
                }
                // the other commands name no database line
                FormCommand::Update(LineRef::Virtual(_), _)
                | FormCommand::Create(..)
                | FormCommand::Remove(_)
                | FormCommand::Clear => {}
            }
        }
        let fresh: Vec<RecId> = named
            .iter()
            .copied()
            .filter(|l| !lines.contains(*l))
            .collect();
        tx.reset_new(comodel.id, &fresh);
        seed_lines(tx, record.env(), comodel.id, &named, sub).await?;
    }
    // a line's own scalar values as raw values; its x2many and delegated values apart
    let line_vals = |vals: &ValueMap| -> Result<(RawVals, ValueMap, ValueMap)> {
        let mut raw = RawVals::new();
        let mut x2many = ValueMap::new();
        let mut delegated = ValueMap::new();
        for (name, v) in vals {
            if name == "id" {
                continue;
            }
            let f = comodel.field(name).ok_or_else(|| {
                Error::validation(format!(
                    "Invalid field '{name}' on model '{}'",
                    comodel.name
                ))
            })?;
            if f.inherited.is_some() {
                delegated.insert(name.clone(), v.clone());
            } else if f.ty.is_x2many() {
                x2many.insert(name.clone(), v.clone());
            } else {
                raw.insert(name.clone().into(), form_cell(comodel, name, v)?);
            }
        }
        Ok((raw, x2many, delegated))
    };
    let mut later: Vec<LineValues> = Vec::new();
    let mut wait = |line: RecId, x2many: ValueMap, delegated: ValueMap| {
        if !x2many.is_empty() || !delegated.is_empty() {
            later.push(LineValues {
                line,
                x2many,
                delegated,
            });
        }
    };
    for command in commands {
        match command {
            FormCommand::Create(reference, vals) => {
                let (raw, x2many, delegated) = line_vals(&vals)?;
                let line = tx.new_record(&co, raw, None, reference.as_deref()).await?;
                let id = single_id(&line)?;
                lines.add(id);
                wait(id, x2many, delegated);
            }
            FormCommand::Update(target, vals) => {
                let (raw, x2many, delegated) = line_vals(&vals)?;
                let line = match &target {
                    LineRef::Real(id) => {
                        tx.new_record(&co, raw, Some(new_id_of(*id)?), None).await?
                    }
                    LineRef::Virtual(r) => tx.new_record(&co, raw, None, Some(r)).await?,
                };
                let id = single_id(&line)?;
                lines.add(id);
                wait(id, x2many, delegated);
            }
            FormCommand::Remove(target) => {
                let id = match target {
                    LineRef::Real(id) => Some(new_id_of(id)?),
                    // a reference the form never created names no line
                    LineRef::Virtual(r) => tx.engine.refs.get(&r).copied(),
                };
                if let Some(id) = id {
                    lines.remove(id);
                }
            }
            FormCommand::Link(id) => lines.add(new_id_of(id)?),
            FormCommand::Clear => lines.clear(),
            FormCommand::Set(ids) => {
                lines.clear();
                for id in ids {
                    lines.add(new_id_of(id)?);
                }
            }
        }
    }
    let lines = lines.into_vec();
    let kept: FxHashSet<RecId> = lines.iter().copied().collect();
    for item in &mut later {
        if !kept.contains(&item.line) {
            item.delegated.clear();
        }
    }
    later.retain(|item| !item.x2many.is_empty() || !item.delegated.is_empty());
    Ok(AppliedLines { lines, later })
}

// ----- snapshots -----------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Snapshot {
    values: FxHashMap<String, Snap>,
}

#[derive(Debug, Clone)]
enum Snap {
    Cell(Cell),
    Lines(Vec<(RecId, Snapshot)>),
}

fn same(a: &Cell, b: &Cell) -> bool {
    a.same_value(b)
}

/// A specification with fields nested deeper than a client may send one.
fn spec_too_deep() -> Error {
    Error::validation(format!(
        "Invalid specification: nested deeper than {MAX_SPEC_DEPTH} levels"
    ))
}

/// The depth of `sub`, the sub-specification of an x2many field of a specification at `depth`
/// (1: the form's). Snapshots and diffs recurse through sub-specifications, so one that names
/// fields deeper than [`MAX_SPEC_DEPTH`] is refused, as [`Spec::from_value`] refuses it from a
/// client (a specification built in code can be deeper); an empty one ends the recursion.
fn depth_below(depth: usize, sub: &Spec) -> Result<usize> {
    let below = depth.checked_add(1).ok_or_else(spec_too_deep)?;
    if below > MAX_SPEC_DEPTH && !sub.is_empty() {
        return Err(spec_too_deep());
    }
    Ok(below)
}

/// Most lines the snapshots and diffs of one onchange visit, every snapshot, comparison and diff
/// of every level together. Depth is bounded ([`MAX_SPEC_DEPTH`]) and so are the commands, but
/// many2many values and cycles through client references make a graph of lines in which a
/// specification as deep as the bound follows fan-out^depth paths; this bounds what the walks
/// cost in total. A form of the largest size the command budget allows, at the form's level, is
/// visited a few times (the snapshots, the comparisons, the diff), well inside the bound.
const MAX_VISITS: usize = 1_000_000;

#[cfg(feature = "test-faults")]
tokio::task_local! {
    static OBSERVED_VISITS: std::cell::Cell<usize>;
}

/// Test-only observation of attempted snapshot/diff line visits in this future's task.
/// Returns the future's unchanged output and the number of visits, including rejected visits.
/// Spawned tasks are not observed; nested scopes count independently. No production budget is
/// changed. The task-local scope is removed on cancellation as well as completion.
#[cfg(feature = "test-faults")]
#[doc(hidden)]
pub async fn count_onchange_visits_for_tests<F: std::future::Future>(
    future: F,
) -> (F::Output, usize) {
    OBSERVED_VISITS
        .scope(std::cell::Cell::new(0), async move {
            let output = future.await;
            (output, OBSERVED_VISITS.with(std::cell::Cell::get))
        })
        .await
}

/// The lines visited so far by the snapshots and diffs of one onchange ([`MAX_VISITS`]).
#[derive(Debug, Default)]
struct Work {
    visits: usize,
}

impl Work {
    /// Count one line of x2many `field` of `model` visited; `Validation` beyond the budget.
    fn visit(&mut self, model: &str, field: &str) -> Result<()> {
        #[cfg(feature = "test-faults")]
        {
            // No scope is the normal case for tests not observing visits. The only error
            // `try_with` can return means that this task has no observation scope.
            match OBSERVED_VISITS.try_with(|count| count.set(count.get().saturating_add(1))) {
                Ok(()) | Err(_) => {}
            }
        }
        self.visits = self
            .visits
            .checked_add(1)
            .filter(|v| *v <= MAX_VISITS)
            .ok_or_else(|| {
                Error::validation(format!(
                    "Wrong value for {model}.{field}: the specification and the values visit more \
                     than {MAX_VISITS} lines in one onchange"
                ))
            })?;
        Ok(())
    }
}

fn snapshot_lines(lines: &[(RecId, Snapshot)]) -> Result<Vec<(RecId, &Snapshot)>> {
    lines
        .iter()
        .map(|(id, snapshot)| Ok((crate::newrec::align(*id)?, snapshot)))
        .collect()
}

/// The value of one specification field of a singleton; `depth` is the depth of `spec`.
fn snap_field<'a>(
    tx: &'a mut Transaction,
    record: &'a Records,
    spec: &'a Spec,
    name: &'a str,
    depth: usize,
    work: &'a mut Work,
) -> BoxFuture<'a, Result<Snap>> {
    Box::pin(async move {
        let field = record.spec().field(name).ok_or_else(|| {
            Error::validation(format!(
                "Invalid field '{name}' on model '{}'",
                record.model_name()
            ))
        })?;
        if !field.ty.is_x2many() {
            let cell = single_cell(record.cells(tx, name).await?, record, name)?;
            return Ok(Snap::Cell(cell));
        }
        let item = spec.get(name);
        let mut lines = record.related(tx, name).await?;
        if let Some(ctx) = item.and_then(|i| i.context.as_ref()) {
            lines = lines.with_context(ctx.iter().cloned());
        }
        let empty = Spec::new();
        let sub = item.and_then(|i| i.fields.as_ref()).unwrap_or(&empty);
        let below = depth_below(depth, sub)?;
        let mut out = Vec::with_capacity(lines.len());
        for line in lines.iter() {
            work.visit(record.model_name(), name)?;
            let snapshot = take_snapshot(tx, &line, sub, below, work).await?;
            out.push((single_id(&line)?, snapshot));
        }
        Ok(Snap::Lines(out))
    })
}

/// The values of the fields of `spec` (at `depth`, 1 for the form's) of singleton `record`.
fn take_snapshot<'a>(
    tx: &'a mut Transaction,
    record: &'a Records,
    spec: &'a Spec,
    depth: usize,
    work: &'a mut Work,
) -> BoxFuture<'a, Result<Snapshot>> {
    Box::pin(async move {
        let mut snapshot = Snapshot::default();
        for name in spec.names() {
            if record.spec().field(name).is_some_and(|f| {
                tx.shared
                    .graph
                    .requires_persisted((record.model_id(), f.id))
            }) {
                continue;
            }
            let value = snap_field(tx, record, spec, name, depth, work).await?;
            snapshot.values.insert(name.to_owned(), value);
        }
        Ok(snapshot)
    })
}

fn has_changed<'a>(
    tx: &'a mut Transaction,
    record: &'a Records,
    spec: &'a Spec,
    snapshot: &'a Snapshot,
    name: &'a str,
    depth: usize,
    work: &'a mut Work,
) -> BoxFuture<'a, Result<bool>> {
    Box::pin(async move {
        let Some(before) = snapshot.values.get(name) else {
            return Ok(true);
        };
        match (before, snap_now(tx, record, spec, name).await?) {
            (Snap::Cell(a), Now::Cell(b)) => Ok(!same(a, &b)),
            (Snap::Lines(lines), Now::Lines(current)) => {
                let lines: FxHashMap<RecId, &Snapshot> =
                    snapshot_lines(lines)?.into_iter().collect();
                let now: FxHashSet<RecId> = current.rec_ids().iter().copied().collect();
                if now.len() != lines.len() || now.iter().any(|id| !lines.contains_key(id)) {
                    return Ok(true);
                }
                let empty = Spec::new();
                let sub = spec
                    .get(name)
                    .and_then(|i| i.fields.as_ref())
                    .unwrap_or(&empty);
                let below = depth_below(depth, sub)?;
                // the lines share the value's prefetch set: one read per field for all of them
                for line in current.iter() {
                    work.visit(record.model_name(), name)?;
                    let id = single_id(&line)?;
                    let line_snapshot = lines.get(&id).copied().ok_or_else(|| {
                        Error::programming(format!("onchange: no snapshot of line {id}"))
                    })?;
                    for sub_name in sub.names() {
                        if has_changed(tx, &line, sub, line_snapshot, sub_name, below, work).await?
                        {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
            _ => Ok(true),
        }
    })
}

/// The current value of a field: a cell, or the current lines (read live, not snapshotted).
enum Now {
    Cell(Cell),
    Lines(Records),
}

async fn snap_now(tx: &mut Transaction, record: &Records, spec: &Spec, name: &str) -> Result<Now> {
    let field = record.spec().field(name).ok_or_else(|| {
        Error::validation(format!(
            "Invalid field '{name}' on model '{}'",
            record.model_name()
        ))
    })?;
    if !field.ty.is_x2many() {
        return Ok(Now::Cell(single_cell(
            record.cells(tx, name).await?,
            record,
            name,
        )?));
    }
    let mut lines = record.related(tx, name).await?;
    if let Some(ctx) = spec.get(name).and_then(|i| i.context.as_ref()) {
        lines = lines.with_context(ctx.iter().cloned());
    }
    Ok(Now::Lines(lines))
}

fn command(code: i64, id: Value, vals: Value) -> Value {
    Value::list([Value::Int(code), id, vals])
}

/// The client references of in-memory lines, by line.
fn references(tx: &Transaction) -> FxHashMap<RecId, String> {
    tx.engine
        .refs
        .iter()
        .map(|(r, id)| (*id, r.clone()))
        .collect()
}

fn line_key(refs: &FxHashMap<RecId, String>, id: RecId) -> Value {
    // the origin's id, else the client reference, else 0
    if let Some(origin) = id.as_real().or(id.origin()) {
        return Value::Int(origin.get());
    }
    refs.get(&id).map_or(Value::Int(0), Value::str)
}

/// Where a diff is: the client references of the in-memory lines (collected once per onchange)
/// and the depth of the specification being compared (1: the form's).
#[derive(Debug, Clone, Copy)]
struct DiffLevel<'r> {
    refs: &'r FxHashMap<RecId, String>,
    depth: usize,
}

fn diff<'a>(
    tx: &'a mut Transaction,
    record: &'a Records,
    spec: &'a Spec,
    before: Option<&'a Snapshot>,
    level: DiffLevel<'a>,
    work: &'a mut Work,
) -> BoxFuture<'a, Result<Row>> {
    Box::pin(async move {
        let mut simple = Spec::new();
        let mut x2many: Vec<&str> = Vec::new();
        for (name, item) in spec.iter() {
            if name == "id"
                || record.spec().field(name).is_some_and(|f| {
                    tx.shared
                        .graph
                        .requires_persisted((record.model_id(), f.id))
                })
            {
                continue;
            }
            let changed = match before {
                None => true,
                Some(snapshot) => {
                    has_changed(tx, record, spec, snapshot, name, level.depth, work).await?
                }
            };
            if !changed {
                continue;
            }
            if record.spec().field(name).is_some_and(|f| f.ty.is_x2many()) {
                x2many.push(name);
            } else {
                simple = simple.with(name, item.clone());
            }
        }
        let mut result = web_read_rows(tx, record, &simple)
            .await?
            .pop()
            .unwrap_or_default();
        result.shift_remove("id");
        for name in x2many {
            let commands = x2many_commands(tx, record, spec, before, name, level, work).await?;
            if !commands.is_empty() {
                result.insert(name.to_owned(), Value::list(commands));
            }
        }
        Ok(result)
    })
}

/// The commands that turn the lines of x2many `name` in `before` into its current lines
/// : removed lines, then per current line an `UPDATE` with its
/// differences, a `CREATE` with all its values, or for a line not in `before` that has an origin a
/// `LINK` with its stored values and an `UPDATE` with its differences from them. Lines are diffed
/// recursively, their own x2many fields as nested command lists.
async fn x2many_commands(
    tx: &mut Transaction,
    record: &Records,
    spec: &Spec,
    before: Option<&Snapshot>,
    name: &str,
    level: DiffLevel<'_>,
    work: &mut Work,
) -> Result<Vec<Value>> {
    let one2many = record
        .spec()
        .field(name)
        .is_some_and(|f| matches!(f.ty, FieldType::One2many { .. }));
    let empty = Spec::new();
    let sub = spec
        .get(name)
        .and_then(|i| i.fields.as_ref())
        .unwrap_or(&empty);
    let below = DiffLevel {
        refs: level.refs,
        depth: depth_below(level.depth, sub)?,
    };
    let Now::Lines(current) = snap_now(tx, record, spec, name).await? else {
        return Err(Error::programming(format!(
            "x2many field '{name}' of '{}' has no lines value",
            record.model_name()
        )));
    };
    let previous: Vec<(RecId, &Snapshot)> = match before.and_then(|s| s.values.get(name)) {
        Some(Snap::Lines(lines)) => snapshot_lines(lines)?,
        Some(Snap::Cell(_)) => {
            return Err(Error::programming(format!(
                "x2many field '{name}' of '{}' has a plain-cell snapshot",
                record.model_name()
            )));
        }
        None => Vec::new(),
    };
    let previous_by_id: FxHashMap<RecId, &Snapshot> = previous.iter().copied().collect();
    let current_ids: FxHashSet<RecId> = current.rec_ids().iter().copied().collect();
    let mut commands = Vec::new();
    for (id, _) in &previous {
        if !current_ids.contains(id) {
            let code = if one2many { 2 } else { 3 };
            commands.push(command(code, line_key(level.refs, *id), Value::Int(0)));
        }
    }
    // lines whose origin's stored values are sent with a LINK, read together; a line without
    // origin has no stored values to send (it is created, not linked)
    let linked: Vec<RecId> = current
        .rec_ids()
        .iter()
        .filter(|id| !previous_by_id.contains_key(*id))
        .filter_map(|id| id.origin().map(RecId::real))
        .collect();
    // The stored lines of every line of the level share one prefetch set: the origins of all the
    // lines `current` is prefetched with (the same field of every sibling record), so that the
    // first read loads the stored values of every line the level links, and the snapshots of the
    // stored lines below read them from the cache, not one record at a time.
    let origin_prefetch: Arc<[RecId]> = if linked.is_empty() {
        Arc::from([])
    } else {
        let mut seen: FxHashSet<RecId> = linked.iter().copied().collect();
        let mut all = linked.clone();
        all.extend(
            current
                .prefetch_ids()
                .iter()
                .filter_map(|id| id.origin().map(RecId::real))
                .filter(|o| seen.insert(*o)),
        );
        Arc::from(all)
    };
    let base_rows: FxHashMap<i64, Row> = if linked.is_empty() {
        FxHashMap::default()
    } else {
        let bases = Records::from_parts(
            current.env().clone(),
            current.model_id(),
            Arc::from(linked.clone()),
            Arc::clone(&origin_prefetch),
        );
        web_read_rows(tx, &bases, sub)
            .await?
            .into_iter()
            .filter_map(|row| match row.get("id") {
                Some(Value::Int(i)) => Some((*i, row)),
                _ => None,
            })
            .collect()
    };
    // the lines share the value's prefetch set: their fields and their own lines are read once
    // for all of them
    for line in current.iter() {
        work.visit(record.model_name(), name)?;
        let id = single_id(&line)?;
        if let Some(snapshot) = previous_by_id.get(&id).copied() {
            let line_diff = diff(tx, &line, sub, Some(snapshot), below, work).await?;
            if !line_diff.is_empty() {
                commands.push(command(
                    1,
                    line_key(level.refs, id),
                    Value::Map(Arc::new(line_diff)),
                ));
            }
        } else if let Some(origin) = id.origin() {
            let base = base_rows
                .get(&origin.get())
                .cloned()
                .ok_or_else(|| crate::read::missing_error(&current, RecId::real(origin)))?;
            commands.push(command(
                4,
                Value::Int(origin.get()),
                Value::Map(Arc::new(base)),
            ));
            let base_line = Records::from_parts(
                current.env().clone(),
                current.model_id(),
                Arc::from([RecId::real(origin)]),
                Arc::clone(&origin_prefetch),
            );
            let base_snapshot = take_snapshot(tx, &base_line, sub, below.depth, work).await?;
            let line_diff = diff(tx, &line, sub, Some(&base_snapshot), below, work).await?;
            if !line_diff.is_empty() {
                commands.push(command(
                    1,
                    Value::Int(origin.get()),
                    Value::Map(Arc::new(line_diff)),
                ));
            }
        } else {
            let line_diff = diff(tx, &line, sub, None, below, work).await?;
            commands.push(command(
                0,
                line_key(level.refs, id),
                Value::Map(Arc::new(line_diff)),
            ));
        }
    }
    Ok(commands)
}

// ----- the protocol --------------------------------------------------------------------------

/// `base.onchange`: the whole computation runs in an isolated scope: the record
/// cache is snapshotted and restored afterwards, and a savepoint, always rolled back, is opened
/// only if something inside changes data, so an onchange can never persist anything whatever its
/// methods do (lead decision G4) and costs no extra statement when it only reads. ORM errors raised
/// inside leave the transaction usable; a database error on a read before any change aborts it,
/// as anywhere else.
pub(crate) async fn onchange(
    tx: &mut Transaction,
    records: &Records,
    values: ValueMap,
    field_names: Vec<String>,
    spec: Spec,
) -> Result<Value> {
    tx.check_records(records)?;
    if records.len() > 1 {
        return Err(Error::validation(format!(
            "onchange works on one record at a time, not on {records}"
        )));
    }
    // The model check comes before the caller's pending writes are flushed (a refused user costs
    // no flush); the edited record's write and read checks after it, outside the isolated scope,
    // so they stay memoized for the next onchange of the same record (design note §3.3).
    let env = records.env().clone();
    let operation = if records.is_empty() {
        Operation::Create
    } else {
        Operation::Write
    };
    tx.prepare_access(&env).await?;
    tx.check_model(&env, records.model_id(), operation)?;
    tx.flush().await?;
    if !records.is_empty() {
        tx.check_record_access(records, Operation::Write).await?;
        // record rules: the edited record must be visible
        records.fetch(tx, Some(&["id"])).await?;
    }
    tx.isolated(async |tx: &mut Transaction| {
        onchange_inner(tx, records, values, field_names, spec).await
    })
    .await
}

/// What an onchange call works with once its arguments are checked.
struct Plan {
    unknown_field: bool,
    first_call: bool,
    field_names: Vec<String>,
    initial: ValueMap,
    changed: ValueMap,
}

/// Phase 1: access, arguments, defaults of the first call, and the split of the form's values into
/// initial and changed values.
async fn plan(
    tx: &mut Transaction,
    records: &Records,
    mut values: ValueMap,
    field_names: Vec<String>,
    spec: &Spec,
) -> Result<Plan> {
    let env = records.env().clone();
    let model = records.spec();
    let operation = if records.is_empty() {
        Operation::Create
    } else {
        Operation::Write
    };
    // the edited record's write and read checks ran before the isolated scope (`onchange`)
    tx.check_model(&env, model.id, operation)?;
    if let Some(unknown) = field_names.iter().find(|f| model.field(f).is_none()) {
        tracing::warn!(target: "erp::orm", model = model.name, field = %unknown, "onchange on an unknown field");
        return Ok(Plan {
            unknown_field: true,
            first_call: false,
            field_names: Vec::new(),
            initial: ValueMap::new(),
            changed: ValueMap::new(),
        });
    }
    for name in spec.names() {
        if model.field(name).is_none() {
            return Err(Error::validation(format!(
                "Invalid field '{name}' on model '{}'",
                model.name
            )));
        }
    }
    let shared = Arc::clone(&tx.shared);
    let first_call = field_names.is_empty();
    let mut field_names = field_names;
    if first_call {
        field_names = values.keys().filter(|k| *k != "id").cloned().collect();
    }
    if first_call {
        let missing: Vec<String> = spec
            .names()
            .filter(|n| !values.contains_key(*n))
            .map(str::to_owned)
            .collect();
        let names: Vec<&str> = missing.iter().map(String::as_str).collect();
        let mut defaults = records.default_get(tx, &names).await?;
        for name in missing {
            if let Some(value) = defaults.shift_remove(&name) {
                values.insert(name.clone(), value);
                field_names.push(name);
            } else if let Some(field) = model.field(&name) {
                // computed fields without dependencies are left unset so they get computed (a
                // delegated field: the parent's field it reads)
                let (owner, owner_field) = delegated_source(&shared.registry, model, field);
                let no_deps = shared
                    .graph
                    .model(owner)
                    .compute(owner_field)
                    .is_some_and(|c| c.depends.is_empty());
                if !no_deps {
                    values.insert(name, Value::False);
                }
            }
        }
    }
    let mut initial = values;
    let changed: ValueMap = field_names
        .iter()
        .filter_map(|f| initial.shift_remove(f).map(|v| (f.clone(), v)))
        .collect();
    // falsy delegate links are not forced
    for (_, delegate) in &model.inherits {
        let name = model.field_by_id(*delegate).name;
        if initial.get(name).is_some_and(|v| !v.is_truthy()) {
            initial.shift_remove(name);
        }
    }
    Ok(Plan {
        unknown_field: false,
        first_call,
        field_names,
        initial,
        changed,
    })
}

/// The model and field a field's value is stored on: the field itself, or for a delegated field
/// the parent's field at the end of its `_inherits` chain.
fn delegated_source(
    registry: &Registry,
    model: &ModelSpec,
    field: &erp_registry::FieldSpec,
) -> (ModelId, FieldId) {
    let (mut owner, mut field) = (model, field);
    while let Some(inherited) = field.inherited {
        owner = registry.model_by_id(inherited.parent);
        field = owner.field_by_id(inherited.parent_field);
    }
    (owner.id, field.id)
}

/// `applying` counts the
/// commands of the whole call; the parent fields the initial values reach are not kept (they are
/// the form's state, not changes).
async fn build_record(
    tx: &mut Transaction,
    records: &Records,
    plan: &mut Plan,
    spec: &Spec,
    applying: &mut Applying,
) -> Result<Records> {
    let env = records.env().clone();
    let registry = Arc::clone(env.registry());
    let model = registry.model_by_id(records.model_id());
    let Some(origin) = records.rec_ids().first().copied() else {
        for name in &plan.field_names {
            plan.initial.insert(name.clone(), Value::False);
        }
        let record = tx.new_record(records, RawVals::new(), None, None).await?;
        update_cache(tx, &record, &plan.initial, spec, applying, 0).await?;
        // initial values are the form's state, not changes: the parents they reach are not marked
        applying.forget_owners();
        return Ok(record);
    };
    let new_id = new_id_of(origin.get())?;
    tx.reset_new(model.id, &[new_id]);
    let record = tx
        .new_record(records, RawVals::new(), Some(origin), None)
        .await?;
    let mut cells = Vec::new();
    let mut delegated: Vec<(&str, Cell)> = Vec::new();
    for name in spec.names() {
        let Some(field) = model.field(name) else {
            continue;
        };
        if matches!(field.ty, FieldType::Id)
            || tx.shared.graph.requires_persisted((model.id, field.id))
        {
            continue;
        }
        let cell = if field.translate {
            tx.ensure(records, field.id).await?;
            tx.cache
                .get(model.id, field.id, origin)
                .unwrap_or(Cell::Null)
        } else {
            let cell = single_cell(records.cells(tx, name).await?, records, name)?;
            origin_cell(tx, &env, field, cell, spec.get(name)).await?
        };
        if field.inherited.is_some() {
            delegated.push((name, cell));
        } else {
            cells.push((field.id, cell));
        }
    }
    tx.set_new_cells(&record, cells)?;
    ensure_parents(tx, &record).await?;
    // the delegated values of the edited record start on its in-memory parents
    for (name, cell) in delegated {
        let (owner, field) = delegated_owner(tx, &record, name).await?;
        // a parent's own delegate link (a note's `move_id`, the link of its in-memory payment):
        // `ensure_parents` already points it at the in-memory record of this database value, and
        // the database id would cut the chain of in-memory parents
        if owner.spec().inherits.iter().any(|(_, d)| *d == field) {
            continue;
        }
        tx.set_new_cells(&owner, vec![(field, cell)])?;
    }
    update_cache(tx, &record, &plan.initial, spec, applying, 0).await?;
    // initial values are the form's state, not changes: the parents they reach are not marked
    applying.forget_owners();
    Ok(record)
}

/// A database record's value as the starting value of its in-memory record: x2many lines become
/// the in-memory lines of the same origins, seeded with their specification values.
async fn origin_cell(
    tx: &mut Transaction,
    env: &crate::Env,
    field: &erp_registry::FieldSpec,
    cell: Cell,
    item: Option<&crate::web::spec::SpecItem>,
) -> Result<Cell> {
    let Cell::Ids(ids) = cell else {
        return Ok(cell);
    };
    let registry = Arc::clone(env.registry());
    let lines: Vec<RecId> = ids
        .iter()
        .map(|l| new_id_of(l.get()))
        .collect::<Result<_>>()?;
    if let Some(comodel) = field.ty.comodel().and_then(|c| registry.model(c)) {
        tx.reset_new(comodel.id, &lines);
        // no sub-specification: the lines' values are not read, nothing to seed
        let sub = item.and_then(|i| i.fields.clone()).unwrap_or_default();
        seed_lines(tx, env, comodel.id, &lines, &sub).await?;
    }
    Ok(Cell::Ids(lines.into()))
}

/// The in-memory record and field that hold delegated field `name` of in-memory `record`, at the
/// end of its `_inherits` chain (after [`ensure_parents`]).
async fn delegated_owner(
    tx: &mut Transaction,
    record: &Records,
    name: &str,
) -> Result<(Records, FieldId)> {
    let (mut owner, mut field) = delegated_target(tx, record, name).await?;
    while owner
        .spec()
        .field(field)
        .is_some_and(|f| f.inherited.is_some())
    {
        (owner, field) = delegated_target(tx, &owner, field).await?;
    }
    let id = owner.spec().field_id(field).ok_or_else(|| {
        Error::programming(format!(
            "'{field}' is not a field of '{}'",
            owner.model_name()
        ))
    })?;
    Ok((owner, id))
}

/// The fields to mark modified on in-memory parents, per model, on which records: each record once
/// per field, models and fields in the order they were first named. Hash sets decide membership,
/// so that a form with many lines that delegate (one parent record each) stays linear.
#[derive(Debug, Default)]
struct ParentChanges {
    groups: Groups<(ModelId, FieldId), RecId>,
}

impl ParentChanges {
    fn add(&mut self, model: ModelId, field: FieldId, id: RecId) {
        self.groups.add_once((model, field), id);
    }

    /// `(model, changes)` in the order the models were first named, each model's fields in the
    /// order they were first named.
    fn into_models(self) -> Vec<(ModelId, Changes)> {
        let mut out: Vec<(ModelId, Changes)> = Vec::new();
        for ((model, field), ids) in self.groups.groups {
            match out.iter_mut().find(|(m, _)| *m == model) {
                Some((_, changes)) => changes.push((field, ids)),
                None => out.push((model, vec![(field, ids)])),
            }
        }
        out
    }
}

/// Phase 3: snapshot 0 (empty on the first call), the changed values applied and recorded in it,
/// the changed fields protected while every field to process is marked modified. Changed
/// delegated values are protected and marked modified on the in-memory parents that hold them
/// (lines' delegated values included); on the first call every field of every in-memory parent is
/// marked modified, like the record's own.
async fn apply_changes(
    tx: &mut Transaction,
    record: &Records,
    plan: &Plan,
    spec: &Spec,
    applying: &mut Applying,
    work: &mut Work,
) -> Result<(Snapshot, Vec<String>)> {
    let model = record.spec();
    let shared = Arc::clone(&tx.shared);
    let graph = shared.graph.model(model.id);
    let mut snapshot0 = if plan.first_call {
        Snapshot::default()
    } else {
        take_snapshot(tx, record, spec, 1, work).await?
    };
    update_cache(tx, record, &plan.changed, spec, applying, 0).await?;
    let owners = applying.take_owners();
    for name in &plan.field_names {
        if spec.contains(name) {
            let value = snap_field(tx, record, spec, name, 1, work).await?;
            snapshot0.values.insert(name.clone(), value);
        }
    }
    let todo: Vec<String> = if plan.first_call {
        let mut all = plan.field_names.clone();
        for name in spec.names() {
            if !all.iter().any(|a| a == name) {
                all.push(name.to_owned());
            }
        }
        all
    } else {
        plan.field_names.clone()
    };
    // the changed fields (and the fields computed with them) keep their values
    let mut protected: Vec<FieldId> = Vec::new();
    for name in &plan.field_names {
        if let Some(field) = model.field(name) {
            let group: Vec<FieldId> = graph
                .compute(field.id)
                .map_or_else(|| vec![field.id], |c| c.group.to_vec());
            for f in group {
                if !protected.contains(&f) {
                    protected.push(f);
                }
            }
        }
    }
    let modified: Vec<FieldId> = if plan.first_call {
        model.fields.iter().map(|f| f.id).collect()
    } else {
        todo.iter().filter_map(|n| model.field_id(n)).collect()
    };
    // per model of the in-memory parents: the fields to mark modified, on which records
    let mut parent_changes = ParentChanges::default();
    if plan.first_call {
        for parent in parents_of(tx, record).await? {
            for f in &parent.spec().fields {
                for id in parent.rec_ids() {
                    parent_changes.add(parent.model_id(), f.id, *id);
                }
            }
        }
    }
    for &(m, f, id) in &owners {
        parent_changes.add(m, f, id);
    }
    let guard = tx.guard();
    let depth = tx.protect(model.id, &protected, record.rec_ids());
    for &(m, f, id) in &owners {
        let group: Vec<FieldId> = shared
            .graph
            .model(m)
            .compute(f)
            .map_or_else(|| vec![f], |c| c.group.to_vec());
        tx.protect(m, &group, &[id]);
    }
    let mut result = tx
        .modified(
            model.id,
            modified
                .into_iter()
                .map(|f| (f, record.rec_ids().to_vec()))
                .collect(),
        )
        .await;
    for (m, changes) in parent_changes.into_models() {
        if result.is_err() {
            break;
        }
        result = tx.modified(m, changes).await;
    }
    tx.unprotect_to(depth);
    guard.disarm();
    result?;
    Ok((snapshot0, todo))
}

/// Phase 4: the onchange methods of the fields to process, pass after pass while specification
/// fields keep changing; the warnings they return, deduplicated in call order.
async fn run_methods(
    tx: &mut Transaction,
    record: &Records,
    spec: &Spec,
    snapshot0: &Snapshot,
    mut todo: Vec<String>,
    work: &mut Work,
) -> Result<Vec<NormalizedWarning>> {
    let model = record.spec();
    let recursive = record
        .env()
        .context()
        .get("recursive_onchanges")
        .is_none_or(|v| v.is_truthy());
    let mut done: FxHashSet<String> = FxHashSet::default();
    let mut warnings: Vec<NormalizedWarning> = Vec::new();
    while !todo.is_empty() {
        let mut visited: FxHashSet<&'static str> = FxHashSet::default();
        for name in &todo {
            let Some(field) = model.field(name) else {
                continue;
            };
            let methods: Vec<&'static str> =
                tx.shared.onchange.methods(model.id, field.id).to_vec();
            for method in &methods {
                if visited.contains(method) {
                    continue;
                }
                let result: OnchangeResult =
                    call_method::<(), OnchangeResult>(tx, record, method, ()).await?;
                if !result.value.is_empty() {
                    let mut assign = ValueMap::new();
                    for (key, value) in result.value {
                        if key == "id" {
                            continue;
                        }
                        if model.field(&key).is_none() {
                            return Err(Error::programming(format!(
                                "onchange method '{method}' of '{}' returned a value for \
                                 '{key}', which is not a field of the model",
                                model.name
                            )));
                        }
                        assign.insert(key, value);
                    }
                    record.write_values(tx, &assign).await?;
                }
                if let Some(w) = result.warning {
                    let entry = w.normalized();
                    if !warnings.contains(&entry) {
                        warnings.push(entry);
                    }
                }
            }
            if field.change_default {
                let cell = if let Ok(rec_id) = single_id(record) {
                    tx.cache.get(model.id, field.id, rec_id)
                } else {
                    None
                };
                let val_str = match cell {
                    Some(Cell::Str(s)) => s.to_string(),
                    Some(Cell::Int(i)) => i.to_string(),
                    Some(Cell::Id(id)) => id.to_string(),
                    Some(Cell::Bool(b)) => b.to_string(),
                    _ => String::new(),
                };
                // Base not installed => no stored defaults in ir.default.
                if !val_str.is_empty() && crate::ir_default::is_available(record.env().registry()) {
                    let condition = format!("{name}={val_str}");
                    let defaults = crate::ir_default::get_model_defaults(
                        tx,
                        record.env(),
                        record.model_name(),
                        Some(&condition),
                    )
                    .await?;
                    if !defaults.is_empty() {
                        let mut assign = ValueMap::new();
                        for (k, v) in defaults {
                            if model.field(&k).is_some() {
                                assign.insert(k, v);
                            }
                        }
                        record.write_values(tx, &assign).await?;
                    }
                }
            }
            visited.extend(methods);
            done.insert(name.clone());
        }
        if !recursive {
            break;
        }
        let mut next = Vec::new();
        for name in spec.names() {
            if !done.contains(name)
                && has_changed(tx, record, spec, snapshot0, name, 1, work).await?
            {
                next.push(name.to_owned());
            }
        }
        todo = next;
    }
    Ok(warnings)
}

fn format_warnings(mut warnings: Vec<NormalizedWarning>) -> Option<Value> {
    match warnings.len() {
        0 => None,
        1 => Some(warnings.remove(0).into_value()),
        _ => {
            let message = warnings
                .iter()
                .map(|warning| format!("{}\n\n{}", warning.title, warning.message))
                .collect::<Vec<_>>()
                .join("\n\n");
            // GRP-050 retains the ordered combined dialog; deliberately drops
            // notification-only options rather than selecting one contributor's options.
            Some(
                NormalizedWarning {
                    title: "Warnings".to_owned(),
                    message,
                    kind: "dialog".to_owned(),
                    options: WarningOptions::default(),
                }
                .into_value(),
            )
        }
    }
}

async fn onchange_inner(
    tx: &mut Transaction,
    records: &Records,
    values: ValueMap,
    field_names: Vec<String>,
    spec: Spec,
) -> Result<Value> {
    let mut plan = plan(tx, records, values, field_names, &spec).await?;
    if plan.unknown_field {
        return Ok(Value::Map(Arc::new(ValueMap::new())));
    }
    // one command budget for the initial and the changed values
    let mut applying = Applying {
        first_new_form: plan.first_call && records.is_empty(),
        ..Applying::default()
    };
    let record = build_record(tx, records, &mut plan, &spec, &mut applying).await?;
    // one budget for every snapshot, comparison and diff of the call
    let mut work = Work::default();
    let (snapshot0, todo) =
        apply_changes(tx, &record, &plan, &spec, &mut applying, &mut work).await?;
    let warnings = run_methods(tx, &record, &spec, &snapshot0, todo, &mut work).await?;
    let before = (!plan.first_call).then_some(&snapshot0);
    let refs = references(tx);
    let level = DiffLevel {
        refs: &refs,
        depth: 1,
    };
    let value = diff(tx, &record, &spec, before, level, &mut work).await?;
    let mut out = ValueMap::new();
    out.insert("value".to_owned(), Value::Map(Arc::new(value)));
    if let Some(warning) = format_warnings(warnings) {
        out.insert("warning".to_owned(), warning);
    }
    Ok(Value::Map(Arc::new(out)))
}

/// Maximum entries that `onchange_batch` accepts in one call, a
/// [per-call shape bound](crate::web#per-call-shape-bounds) (`Parity: divergent`).
/// Every entry runs a whole onchange (snapshot, methods, diff); 1,000 is above the rows a client
/// creates in one batch from a list or an import preview.
pub const MAX_ONCHANGE_BATCH_ENTRIES: usize = 1_000;

/// `base.onchange_batch`: one onchange per values dictionary on an empty recordset.
pub(crate) async fn onchange_batch(
    tx: &mut Transaction,
    records: &Records,
    values_list: Vec<ValueMap>,
    field_names: Vec<String>,
    spec: Spec,
) -> Result<Vec<Value>> {
    if values_list.len() > MAX_ONCHANGE_BATCH_ENTRIES {
        return Err(Error::validation(format!(
            "onchange_batch accepts at most {MAX_ONCHANGE_BATCH_ENTRIES} values entries, got {}",
            values_list.len()
        )));
    }
    if !records.is_empty() {
        return Err(Error::programming("self must be empty"));
    }
    let mut out = Vec::with_capacity(values_list.len());
    for values in values_list {
        // `Spec` shares its fields: the clone is a reference count, not a copy per entry (pinned by
        // `tests/alloc_onchange_batch.rs`). The field names are copied: the chain's `onchange`
        // owns them, and the batch's are the few fields being edited, usually none.
        out.push(
            records
                .onchange(tx, values, field_names.clone(), spec.clone())
                .await?,
        );
    }
    Ok(out)
}

impl Records {
    /// `onchange(values, field_names, fields_spec)` through the method chain: `{'value': …}` with
    /// the values that changed, and `'warning'` when onchange methods warned.
    pub async fn onchange(
        &self,
        tx: &mut Transaction,
        values: ValueMap,
        field_names: Vec<String>,
        spec: Spec,
    ) -> Result<Value> {
        tx.check_records(self)?;
        crate::database_generated::check_write_names(
            tx,
            self.spec(),
            values.keys().map(String::as_str),
        )?;
        call_method::<(ValueMap, Vec<String>, Spec), Value>(
            tx,
            self,
            "onchange",
            (values, field_names, spec),
        )
        .await
    }

    /// `onchange_batch(values_list, field_names, fields_spec)` on an empty recordset.
    pub async fn onchange_batch(
        &self,
        tx: &mut Transaction,
        values_list: Vec<ValueMap>,
        field_names: Vec<String>,
        spec: Spec,
    ) -> Result<Vec<Value>> {
        call_method::<(Vec<ValueMap>, Vec<String>, Spec), Vec<Value>>(
            tx,
            self,
            "onchange_batch",
            (values_list, field_names, spec),
        )
        .await
    }
}

/// `onchange` with keyword arguments as dynamic values.
pub(crate) async fn onchange_call(
    tx: &mut Transaction,
    records: &Records,
    kwargs: &Value,
) -> Result<Value> {
    let mut k = Kwargs::new("onchange", kwargs)?;
    let values = k.map("values")?;
    let field_names = k.strings("field_names")?;
    let spec = k.spec("fields_spec")?;
    k.finish()?;
    records.onchange(tx, values, field_names, spec).await
}

/// `onchange_batch` with keyword arguments as dynamic values.
pub(crate) async fn onchange_batch_call(
    tx: &mut Transaction,
    records: &Records,
    kwargs: &Value,
) -> Result<Value> {
    let mut k = Kwargs::new("onchange_batch", kwargs)?;
    let values_list = k
        .list("values_list")?
        .into_iter()
        .map(|v| match v {
            Value::Map(m) => Ok(m.as_ref().clone()),
            other => Err(Error::validation(format!(
                "onchange_batch(): {other} is not a dictionary of values"
            ))),
        })
        .collect::<Result<Vec<_>>>()?;
    let field_names = k.strings("field_names")?;
    let spec = k.spec("fields_spec")?;
    k.finish()?;
    Ok(Value::list(
        records
            .onchange_batch(tx, values_list, field_names, spec)
            .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{Groups, MAX_VISITS, Work};

    fn warning_map<const N: usize>(pairs: [(&str, erp_fields::Value); N]) -> erp_fields::Value {
        erp_fields::Value::Map(std::sync::Arc::new(
            pairs
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        ))
    }

    #[test]
    fn warning_options_validate_and_canonicalize_class_name() {
        use super::{Warning, WarningOptions, format_warnings};
        let options =
            WarningOptions::new(Some("  addon_notice   _urgent-2  "), Some(false)).unwrap();
        let warning = Warning {
            options,
            ..Warning::default()
        };
        let output = format_warnings(vec![warning.normalized()]).unwrap();
        assert_eq!(
            output,
            warning_map([
                ("title", erp_fields::Value::str("Warning")),
                ("message", erp_fields::Value::str("")),
                ("type", erp_fields::Value::str("dialog")),
                (
                    "className",
                    erp_fields::Value::str("addon_notice _urgent-2")
                ),
                ("sticky", erp_fields::Value::Bool(false)),
            ])
        );
        for class_name in ["", "   "] {
            let options = WarningOptions::new(Some(class_name), None).unwrap();
            assert_eq!(options.class_name(), Some(""));
            assert_eq!(options.sticky(), None);
        }
        let options = WarningOptions::new(None, Some(true)).unwrap();
        assert_eq!(options.class_name(), None);
        assert_eq!(options.sticky(), Some(true));
    }

    #[test]
    fn warning_options_reject_invalid_class_names_with_exact_errors() {
        use super::WarningOptions;
        let too_long = "a".repeat(513);
        let token_too_long = "a".repeat(65);
        let cases = [
            (
                too_long.as_str(),
                "Invalid onchange warning className: exceeds 512 UTF-8 bytes",
            ),
            (
                "a b c d e f g h i",
                "Invalid onchange warning className: expected at most 8 tokens",
            ),
            (
                token_too_long.as_str(),
                "Invalid onchange warning className: token exceeds 64 bytes",
            ),
            (
                "same same",
                "Invalid onchange warning className: duplicate token 'same'",
            ),
        ];
        for (class_name, message) in cases {
            let error = WarningOptions::new(Some(class_name), None).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(error.message(), message);
        }
        for class_name in [
            "a\tb", "a\nb", "a\rb", "a\0b", "é", "a\u{a0}b", "1notice", "-notice", "a:b",
            "<script>",
        ] {
            let error = WarningOptions::new(Some(class_name), None).unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Invalid onchange warning className: tokens must match [A-Za-z_][A-Za-z0-9_-]* and be separated by ASCII spaces"
            );
        }
        let at_limit = format!(
            " {}",
            (0..8)
                .map(|i| format!("a{i}{}", "a".repeat(61)))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert_eq!(at_limit.len(), 512);
        assert!(WarningOptions::new(Some(&at_limit), None).is_ok());
        assert!(WarningOptions::new(Some(&"a".repeat(64)), None).is_ok());
    }

    #[test]
    fn warning_identity_preserves_options_and_normalizes_legacy_defaults() {
        use super::{Warning, WarningOptions, format_warnings};
        let legacy = Warning::default().normalized();
        let explicit = Warning {
            title: Some(String::new()),
            message: Some(String::new()),
            kind: Some("dialog".to_owned()),
            ..Warning::default()
        }
        .normalized();
        assert_eq!(legacy, explicit);
        let notification = Warning {
            title: Some("Notice".to_owned()),
            message: Some("Body".to_owned()),
            kind: Some("addon-kind".to_owned()),
            options: WarningOptions::new(Some("a  b"), Some(false)).unwrap(),
        };
        let mut equivalent = notification.clone();
        equivalent.options = WarningOptions::new(Some(" a b "), Some(false)).unwrap();
        assert_eq!(notification.clone().normalized(), equivalent.normalized());
        let mut absent = notification.clone();
        absent.options = WarningOptions::new(Some("a b"), None).unwrap();
        assert_ne!(
            notification.clone().normalized(),
            absent.clone().normalized()
        );
        let mut different_class = notification.clone();
        different_class.options = WarningOptions::new(Some("b a"), Some(false)).unwrap();
        assert_ne!(
            notification.clone().normalized(),
            different_class.normalized()
        );
        let mut different_kind = notification.clone();
        different_kind.kind = Some("dialog".to_owned());
        assert_ne!(
            notification.clone().normalized(),
            different_kind.normalized()
        );
        let output = format_warnings(vec![notification.normalized(), absent.normalized()]).unwrap();
        assert_eq!(
            output,
            warning_map([
                ("title", erp_fields::Value::str("Warnings")),
                (
                    "message",
                    erp_fields::Value::str("Notice\n\nBody\n\nNotice\n\nBody")
                ),
                ("type", erp_fields::Value::str("dialog")),
            ])
        );
        assert_eq!(format_warnings(Vec::new()), None);
        assert_eq!(
            format_warnings(vec![legacy]),
            Some(warning_map([
                ("title", erp_fields::Value::str("Warning")),
                ("message", erp_fields::Value::str("")),
                ("type", erp_fields::Value::str("dialog")),
            ]))
        );
    }

    #[cfg(feature = "test-faults")]
    #[tokio::test]
    async fn visit_observation_scopes_are_independent_and_keep_the_output() {
        use super::count_onchange_visits_for_tests;

        let (output, outer) = count_onchange_visits_for_tests(async {
            Work::default().visit("form", "lines").unwrap();
            let (output, inner) = count_onchange_visits_for_tests(async {
                let mut work = Work::default();
                work.visit("form", "lines").unwrap();
                work.visit("form", "lines").unwrap();
                42
            })
            .await;
            assert_eq!((output, inner), (42, 2));
            output
        })
        .await;
        assert_eq!((output, outer), (42, 1));
        let (output, visits) = count_onchange_visits_for_tests(async { 7 }).await;
        assert_eq!((output, visits), (7, 0));
    }

    #[test]
    fn line_visit_budget_accepts_the_limit_and_rejects_every_visit_after_it() {
        let mut work = Work::default();
        for _ in 0..MAX_VISITS {
            work.visit("form", "lines").unwrap();
        }
        assert_eq!(work.visits, MAX_VISITS);
        for _ in 0..2 {
            let error = work.visit("form", "lines").unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Wrong value for form.lines: the specification and the values visit more than 1000000 lines in one onchange"
            );
            assert_eq!(work.visits, MAX_VISITS);
        }
    }

    #[test]
    fn groups_keep_first_seen_order_of_keys_and_values() {
        let mut groups: Groups<u32, u32> = Groups::default();
        groups.add(7, &[1, 2]);
        groups.add(3, &[9]);
        groups.add(7, &[2, 5]);
        assert_eq!(
            groups.groups,
            vec![(7, vec![1, 2, 2, 5]), (3, vec![9])],
            "values keep duplicates when added with `add`"
        );
    }

    #[test]
    #[ignore = "DEFERRED_PERFORMANCE: milestone end"]
    fn add_once_drops_repeated_pairs_and_stays_linear() {
        let mut groups: Groups<u32, u32> = Groups::default();
        // 400,000 additions to one group, half of them repeats: a scan of the group per addition
        // would take minutes; hashing takes milliseconds
        let started = std::time::Instant::now();
        for i in 0..200_000_u32 {
            groups.add_once(1, i);
            groups.add_once(1, i);
        }
        groups.add_once(2, 0);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(groups.groups.len(), 2);
        assert_eq!(groups.groups[0].1.len(), 200_000);
        assert_eq!(groups.groups[0].1.first(), Some(&0));
        assert_eq!(groups.groups[0].1.last(), Some(&199_999));
        assert_eq!(groups.groups[1], (2, vec![0]));
    }

    #[test]
    fn add_once_preserves_unique_pairs_and_order() {
        let mut groups: Groups<u32, u32> = Groups::default();
        for i in 0..200_000_u32 {
            groups.add_once(1, i);
            groups.add_once(1, i);
        }
        groups.add_once(2, 0);
        assert_eq!(groups.groups.len(), 2);
        assert_eq!(groups.groups[0].1.len(), 200_000);
        assert_eq!(groups.groups[0].1.first(), Some(&0));
        assert_eq!(groups.groups[0].1.last(), Some(&199_999));
        assert_eq!(groups.groups[1], (2, vec![0]));
    }
}
