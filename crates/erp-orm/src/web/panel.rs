//! Search panel range methods (GRP §4.13):
//! `search_panel_select_range` and `search_panel_select_multi_range`.
//!
//! Categories (`search_panel_select_range`) provide hierarchical navigation over many2one,
//! selection, and many2many fields. Multi-selection filters (`search_panel_select_multi_range`)
//! provide checklists with optional sub-grouping over many2one, many2many, and selection fields.
//! Record counts for both methods reuse grouped queries, querying counts in a single statement
//! rather than executing N individual count statements.

use std::sync::Arc;

use erp_domain::{Domain, Operator};
use erp_fields::rows::{Bitmap, CellRef, Column, ColumnData, Key, Layout, Rows, RowsBuilder};
use erp_fields::{Error, FieldType, Result, Value, ValueMap};
use rustc_hash::FxHashMap;

use crate::Transaction;
use crate::read::Load;
use crate::read_group::ReadGroup;
use crate::records::Records;
use crate::search::SearchOptions;

/// Error message returned when the maximal limit is reached.
pub const SEARCH_PANEL_ERROR_MESSAGE: &str = "Too many items to display.";

// Bound the shape of an input map, like grouped reads' 64-entry argument lists. This is not
// a display/result cap: absent display limits and every count query remain unlimited.
const MAX_GROUP_DOMAINS: usize = 64;

/// Identifier representation for search panel image values.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ImageId {
    Int(i64),
    Str(String),
}

impl ImageId {
    fn as_int(&self) -> Result<i64> {
        match self {
            Self::Int(id) => Ok(*id),
            Self::Str(_) => Err(Error::programming(
                "search panel expected a relational image id",
            )),
        }
    }

    fn into_str(self) -> Result<String> {
        match self {
            Self::Str(key) => Ok(key),
            Self::Int(_) => Err(Error::programming(
                "search panel expected a selection image key",
            )),
        }
    }
}

/// A value entry extracted from the model's domain image.
#[derive(Debug, Clone)]
struct ImageEntry {
    id: ImageId,
    display_name: Value,
    count: i64,
}

/// Arguments of `search_panel_select_range`.
///
/// The method validates field names and field access before reading values. A missing or zero
/// `limit` is unlimited; reaching a positive limit returns the display-error value.
///
/// ```
/// use erp_orm::SearchPanelSelectRange;
/// let mut args = SearchPanelSelectRange::new("category_id");
/// args.enable_counters = true;
/// assert!(args.hierarchize);
/// assert_eq!(args.limit, None);
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SearchPanelSelectRange {
    /// Field name for the category.
    pub field_name: String,
    /// Category domain restriction.
    pub category_domain: Option<Domain>,
    /// Filter domain restriction.
    pub filter_domain: Option<Domain>,
    /// Search domain restriction.
    pub search_domain: Option<Domain>,
    /// Comodel domain restriction.
    pub comodel_domain: Option<Domain>,
    /// Whether to compute record counts for category values.
    pub enable_counters: bool,
    /// Whether to return all comodel values rather than only those present in the domain image.
    pub expand: bool,
    /// Whether to structure categories hierarchically using the comodel's parent field.
    pub hierarchize: bool,
    /// Maximal number of elements to return before failing with a display limit error.
    pub limit: Option<u64>,
}

impl SearchPanelSelectRange {
    /// Create search panel select range arguments with default options.
    pub fn new(field_name: impl Into<String>) -> Self {
        Self {
            field_name: field_name.into(),
            category_domain: None,
            filter_domain: None,
            search_domain: None,
            comodel_domain: None,
            enable_counters: false,
            expand: false,
            hierarchize: true,
            limit: None,
        }
    }
}

/// Arguments of `search_panel_select_multi_range`.
///
/// A many2many `group_domain` is a map with JSON-encoded scalar keys; a many2one
/// `group_domain` is a domain value. Invalid shapes return `Validation`.
///
/// ```
/// use erp_orm::SearchPanelSelectMultiRange;
/// let mut args = SearchPanelSelectMultiRange::new("tag_ids");
/// args.enable_counters = true;
/// args.group_by = Some("category_id".to_owned());
/// assert_eq!(args.limit, None);
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SearchPanelSelectMultiRange {
    /// Field name for the filter checklist.
    pub field_name: String,
    /// Search domain restriction.
    pub search_domain: Option<Domain>,
    /// Category domain restriction.
    pub category_domain: Option<Domain>,
    /// Filter domain restriction.
    pub filter_domain: Option<Domain>,
    /// Comodel domain restriction.
    pub comodel_domain: Option<Domain>,
    /// Whether to compute record counts for filter values.
    pub enable_counters: bool,
    /// Whether to return all comodel values rather than only those present in the domain image.
    pub expand: bool,
    /// Optional field on the comodel to sub-group filter items.
    pub group_by: Option<String>,
    /// Many2many: at most 64 domain overrides keyed by JSON-encoded group ids. Many2one:
    /// a domain intersected with the counter domain. Invalid shapes return `Validation`.
    pub group_domain: Option<Value>,
    /// Maximal number of elements to return before failing with a display limit error.
    pub limit: Option<u64>,
}

impl SearchPanelSelectMultiRange {
    /// Create search panel select multi-range arguments with default options.
    pub fn new(field_name: impl Into<String>) -> Self {
        Self {
            field_name: field_name.into(),
            search_domain: None,
            category_domain: None,
            filter_domain: None,
            comodel_domain: None,
            enable_counters: false,
            expand: false,
            group_by: None,
            group_domain: None,
            limit: None,
        }
    }
}

fn val_as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

fn val_as_str(v: &Value) -> Option<&str> {
    match v {
        Value::Str(s) => Some(s.as_ref()),
        _ => None,
    }
}

fn value_map(entries: impl IntoIterator<Item = (impl Into<String>, Value)>) -> Value {
    let mut map = ValueMap::new();
    for (k, v) in entries {
        map.insert(k.into(), v);
    }
    Value::Map(Arc::new(map))
}

fn search_panel_error() -> Value {
    value_map([("error_msg", Value::str(SEARCH_PANEL_ERROR_MESSAGE))])
}

fn required_int(rows: &Rows, row: usize, key: &str) -> Result<i64> {
    rows.i64(row, key)?.ok_or_else(|| {
        Error::programming(format!("search panel row expected an integer for '{key}'"))
    })
}

fn required_text(value: Value, key: &str) -> Result<Option<Arc<str>>> {
    match value {
        Value::Str(text) => Ok(Some(text)),
        Value::False | Value::Bool(false) => Ok(None),
        _ => Err(Error::programming(format!(
            "search panel row expected text for '{key}'"
        ))),
    }
}

/// Columns are accumulated directly; keys belong to one layout, never individual choices.
struct PanelColumns {
    ids: Vec<i64>,
    names: Vec<Option<Arc<str>>>,
    groups: Option<(Vec<Value>, Vec<Value>)>,
    counts: Option<Vec<i64>>,
}

impl PanelColumns {
    fn new(len: usize, groups: bool, counters: bool) -> Self {
        Self {
            ids: Vec::with_capacity(len),
            names: Vec::with_capacity(len),
            groups: groups.then(|| (Vec::with_capacity(len), Vec::with_capacity(len))),
            counts: counters.then(|| Vec::with_capacity(len)),
        }
    }

    fn push(&mut self, rows: &Rows, row: usize) -> Result<i64> {
        let id = required_int(rows, row, "id")?;
        self.ids.push(id);
        self.names.push(required_text(
            rows.get(row, "display_name")?,
            "display_name",
        )?);
        Ok(id)
    }

    fn group(&mut self, field: &erp_registry::FieldSpec, rows: &Rows, row: usize) -> Result<Value> {
        let (id, name) = group_at(field, rows, row)?;
        let (ids, names) = self
            .groups
            .as_mut()
            .ok_or_else(|| Error::programming("search panel missing group columns"))?;
        ids.push(id.clone());
        names.push(name);
        Ok(id)
    }

    fn finish(self, parent: Option<(&str, Vec<Option<i64>>)>) -> Result<Value> {
        let mut keys = vec![Key::Field("id"), Key::Field("display_name")];
        if let Some((name, _)) = &parent {
            keys.push(Key::Owned(Arc::from(*name)));
        }
        if self.groups.is_some() {
            keys.extend([Key::Field("group_id"), Key::Field("group_name")]);
        }
        if self.counts.is_some() {
            keys.push(Key::Field("__count"));
        }
        let mut builder = RowsBuilder::new(Layout::new(keys)?, self.ids.len())?;
        builder.column("id", Column::new(ColumnData::Int(self.ids)))?;
        builder.column("display_name", Column::new(ColumnData::Str(self.names)))?;
        if let Some((name, values)) = parent {
            let false_rows = Bitmap::from_rows(
                values
                    .iter()
                    .enumerate()
                    .filter_map(|(row, id)| id.is_none().then_some(row)),
            )?;
            // The false bitmap owns emptiness; zero is only the unused backing slot.
            let ids = values.into_iter().map(|id| id.unwrap_or(0)).collect();
            builder.column(
                name,
                Column::new(ColumnData::Int(ids)).with_false(false_rows),
            )?;
        }
        if let Some((ids, names)) = self.groups {
            builder.column("group_id", Column::new(ColumnData::Dyn(ids)))?;
            builder.column("group_name", Column::new(ColumnData::Dyn(names)))?;
        }
        if let Some(counts) = self.counts {
            builder.column("__count", Column::new(ColumnData::Int(counts)))?;
        }
        Ok(Value::Rows(builder.finish()?.into()))
    }
}

fn selection_rows(
    ids: Vec<Option<Arc<str>>>,
    names: Vec<Option<Arc<str>>>,
    counts: Option<Vec<i64>>,
) -> Result<Value> {
    let mut keys = vec![Key::Field("id"), Key::Field("display_name")];
    if counts.is_some() {
        keys.push(Key::Field("__count"));
    }
    let mut builder = RowsBuilder::new(Layout::new(keys)?, ids.len())?;
    builder.column("id", Column::new(ColumnData::Str(ids)))?;
    builder.column("display_name", Column::new(ColumnData::Str(names)))?;
    if let Some(counts) = counts {
        builder.column("__count", Column::new(ColumnData::Int(counts)))?;
    }
    Ok(Value::Rows(builder.finish()?.into()))
}

fn relational_pair(value: &Value, field: &str) -> Result<(i64, Value)> {
    if let Value::List(pair) = value
        && pair.len() == 2
        && let Some(id) = val_as_i64(&pair[0])
    {
        return Ok((id, pair[1].clone()));
    }
    Err(Error::programming(format!(
        "search panel expected [id, display_name] for '{field}'"
    )))
}

// Empty many2one values become False on the wire, rather than a synthetic parent id.
fn get_parent_id(val: Option<&Value>) -> Result<Option<i64>> {
    match val {
        Some(Value::False | Value::Bool(false)) => Ok(None),
        Some(Value::List(pair)) => {
            relational_pair(&Value::List(Arc::clone(pair)), "parent").map(|(id, _)| Some(id))
        }
        Some(Value::Int(id)) => Ok(Some(*id)),
        _ => Err(Error::programming(
            "search panel expected a parent id or an empty parent",
        )),
    }
}

/// Borrow parent labels directly: materializing a classic many2one Value would allocate a pair.
fn parent_at(rows: &Rows, row: usize, position: usize) -> Result<Option<i64>> {
    match rows.cell(row, position)? {
        CellRef::False | CellRef::Bool(false) => Ok(None),
        CellRef::Int(id) | CellRef::Label { id, .. } => Ok(Some(id)),
        CellRef::Value(value) => get_parent_id(Some(value)),
        _ => Err(Error::programming(
            "search panel expected a parent id or an empty parent",
        )),
    }
}

fn parent_position(rows: &Rows, name: &str) -> Result<usize> {
    rows.layout()
        .position(name)
        .ok_or_else(|| Error::programming(format!("search panel row is missing '{name}'")))
}

/// Read group ids and counts without fetching relational display names. Relational callers
/// obtain names from their access-filtered comodel search; selection labels come from metadata.
async fn search_panel_domain_image(
    tx: &mut Transaction,
    records: &Records,
    field_name: &str,
    domain: &Domain,
    _set_count: bool,
    limit: Option<u64>,
) -> Result<Vec<ImageEntry>> {
    let registry = Arc::clone(records.env().registry());
    let model = registry.model_by_id(records.model_id());
    let field = model.field(field_name).ok_or_else(|| {
        Error::validation(format!(
            "Field '{field_name}' does not exist on model '{}'",
            model.name
        ))
    })?;
    if !matches!(
        &field.ty,
        FieldType::Many2one { .. } | FieldType::Many2many { .. } | FieldType::Selection { .. }
    ) {
        return Err(Error::programming(format!(
            "search panel domain image does not support field type {}",
            field.ty.type_name()
        )));
    }
    let mut args =
        ReadGroup::new(domain.clone() & Domain::condition(field_name, Operator::Ne, false)?)
            .groupby([field_name])
            .aggregates(["__count"]);
    args.limit = limit.filter(|limit| *limit > 0);
    let rows = records.read_group(tx, args).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let [id, Value::Int(count)] = row.as_slice() else {
            return Err(Error::programming(
                "search panel expected a grouped id and integer count",
            ));
        };
        if !id.is_truthy() {
            // A relational grouping may include its empty/hidden relation group. It has no
            // selectable panel value.
            continue;
        }
        let (id, display_name) = match (&field.ty, id) {
            (FieldType::Many2one { .. } | FieldType::Many2many { .. }, Value::Int(id)) => {
                // Names are fetched by the comodel read, after applying its record rules.
                (ImageId::Int(*id), Value::False)
            }
            (FieldType::Selection { values, .. }, Value::Str(key)) => {
                let label = values
                    .as_ref()
                    .and_then(|values| values.iter().find(|(k, _)| k == key.as_ref()))
                    .map(|(_, label)| label)
                    .ok_or_else(|| {
                        Error::programming(format!(
                            "search panel unknown selection key '{key}' for '{field_name}'"
                        ))
                    })?;
                (ImageId::Str(key.to_string()), Value::str(label))
            }
            _ => {
                return Err(Error::programming(format!(
                    "search panel invalid grouped id for '{field_name}'"
                )));
            }
        };
        out.push(ImageEntry {
            id,
            display_name,
            count: *count,
        });
    }
    Ok(out)
}

/// Compute values in the image of `model_domain` enriched with counts from `extra_domain`.
#[allow(clippy::too_many_arguments)]
async fn search_panel_field_image(
    tx: &mut Transaction,
    records: &Records,
    field_name: &str,
    model_domain: &Domain,
    extra_domain: &Domain,
    enable_counters: bool,
    counts_only: bool,
    limit: Option<u64>,
    set_limit: bool,
) -> Result<Vec<ImageEntry>> {
    let count_domain = model_domain.clone() & extra_domain.clone();
    if counts_only {
        return search_panel_domain_image(tx, records, field_name, &count_domain, true, None).await;
    }

    let no_extra = extra_domain.is_true();
    let lim = if set_limit { limit } else { None };
    let mut model_image = search_panel_domain_image(
        tx,
        records,
        field_name,
        model_domain,
        enable_counters && no_extra,
        lim,
    )
    .await?;

    if enable_counters && !no_extra {
        let count_image =
            search_panel_domain_image(tx, records, field_name, &count_domain, true, None).await?;
        let count_map: FxHashMap<ImageId, i64> =
            count_image.into_iter().map(|e| (e.id, e.count)).collect();

        for entry in &mut model_image {
            entry.count = count_map.get(&entry.id).copied().unwrap_or(0);
        }
    }

    Ok(model_image)
}

/// Compute selection field range values for categories or filters.
#[allow(clippy::too_many_arguments)]
async fn search_panel_selection_range(
    tx: &mut Transaction,
    records: &Records,
    field_name: &str,
    model_domain: &Domain,
    extra_domain: &Domain,
    enable_counters: bool,
    expand: bool,
    limit: Option<u64>,
) -> Result<Value> {
    let registry = Arc::clone(records.env().registry());
    let model = registry.model_by_id(records.model_id());
    let field = model.field(field_name).ok_or_else(|| {
        Error::validation(format!(
            "Field '{field_name}' does not exist on model '{}'",
            model.name
        ))
    })?;

    let set_limit = limit.is_some() && !expand;
    let domain_image = if enable_counters || !expand {
        search_panel_field_image(
            tx,
            records,
            field_name,
            model_domain,
            extra_domain,
            enable_counters,
            expand,
            limit,
            set_limit,
        )
        .await?
    } else {
        Vec::new()
    };

    if !expand {
        if limit.is_some_and(|lim| domain_image.len() as u64 >= lim) {
            return Ok(search_panel_error());
        }
        let mut ids = Vec::with_capacity(domain_image.len());
        let mut names = Vec::with_capacity(domain_image.len());
        let mut counts = enable_counters.then(|| Vec::with_capacity(domain_image.len()));
        for entry in domain_image {
            ids.push(Some(Arc::from(entry.id.into_str()?)));
            names.push(required_text(entry.display_name, "display_name")?);
            if let Some(counts) = &mut counts {
                counts.push(entry.count);
            }
        }
        return selection_rows(ids, names, counts);
    }

    let count_map: FxHashMap<String, i64> = domain_image
        .into_iter()
        .map(|e| Ok((e.id.into_str()?, e.count)))
        .collect::<Result<_>>()?;

    let static_options = match &field.ty {
        FieldType::Selection {
            values: Some(values),
            ..
        } => values.as_slice(),
        _ => {
            return Err(Error::programming(
                "search panel expected static selection options",
            ));
        }
    };

    if limit.is_some_and(|lim| static_options.len() as u64 >= lim) {
        return Ok(search_panel_error());
    }
    let mut ids = Vec::with_capacity(static_options.len());
    let mut names = Vec::with_capacity(static_options.len());
    let mut counts = enable_counters.then(|| Vec::with_capacity(static_options.len()));
    for (key, label) in static_options {
        ids.push(Some(Arc::from(key.as_str())));
        names.push(Some(Arc::from(label.as_str())));
        if let Some(counts) = &mut counts {
            // Options with no matching source group have zero sources.
            counts.push(count_map.get(key.as_str()).copied().unwrap_or(0));
        }
    }
    selection_rows(ids, names, counts)
}

/// Retain positions whose entire parent chain is visible, preserving comodel order.
fn search_panel_sanitized_parent_hierarchy(
    rows: &Rows,
    parent_name: &str,
    target_ids: &[i64],
) -> Result<Vec<usize>> {
    let mut positions = FxHashMap::default();
    let position = parent_position(rows, parent_name)?;
    let mut parents = Vec::with_capacity(rows.len());
    for row in 0..rows.len() {
        let id = required_int(rows, row, "id")?;
        if positions.insert(id, row).is_some() {
            return Err(Error::programming("search panel duplicate comodel id"));
        }
        parents.push(parent_at(rows, row, position)?);
    }
    let mut keep = vec![None; rows.len()];
    let mut seen = vec![None; rows.len()];
    let mut chain = Vec::new();
    for (target_index, &target) in target_ids.iter().enumerate() {
        let mut current = Some(target);
        chain.clear();
        let mut included = true;
        while let Some(id) = current {
            let Some(&row) = positions.get(&id) else {
                // A hidden/missing ancestor excludes the entire chain (the sanitizer contract).
                included = false;
                break;
            };
            if seen[row] == Some(target_index) {
                return Err(Error::programming(format!(
                    "search panel cycle in parent field '{parent_name}'"
                )));
            }
            seen[row] = Some(target_index);
            if let Some(status) = keep[row] {
                included = status;
                break;
            }
            chain.push(row);
            current = parents[row];
        }
        for &row in &chain {
            keep[row] = Some(included);
        }
    }
    Ok(keep
        .into_iter()
        .enumerate()
        .filter_map(|(row, keep)| {
            // Non-target/non-ancestor rows and chains with hidden ancestors are excluded.
            (keep == Some(true)).then_some(row)
        })
        .collect())
}

/// Roll each original local count up its parent chain, using typed columns and checked sums.
fn search_panel_global_counters(
    ids: &[i64],
    parents: &[Option<i64>],
    counts: &mut [i64],
    parent_name: &str,
) -> Result<()> {
    if ids.len() != parents.len() || ids.len() != counts.len() {
        return Err(Error::programming(
            "search panel category columns differ in length",
        ));
    }
    let mut positions = FxHashMap::default();
    for (row, &id) in ids.iter().enumerate() {
        if positions.insert(id, row).is_some() {
            return Err(Error::programming("search panel duplicate comodel id"));
        }
    }
    let initial = counts.to_vec();
    let mut seen = vec![None; ids.len()];
    for (row, &count) in initial.iter().enumerate() {
        let mut current = parents[row];
        seen[row] = Some(row);
        while let Some(parent) = current {
            let &position = positions
                .get(&parent)
                .ok_or_else(|| Error::programming("search panel missing sanitized parent"))?;
            if seen[position] == Some(row) {
                return Err(Error::programming(format!(
                    "search panel cycle in parent field '{parent_name}'"
                )));
            }
            seen[position] = Some(row);
            counts[position] = counts[position]
                .checked_add(count)
                .ok_or_else(|| Error::programming("search panel category counter overflow"))?;
            current = parents[position];
        }
    }
    Ok(())
}

/// `search_panel_select_range`: category values, hierarchy and counters.
pub(crate) async fn search_panel_select_range(
    tx: &mut Transaction,
    records: &Records,
    mut args: SearchPanelSelectRange,
) -> Result<Value> {
    args.limit = args.limit.filter(|limit| *limit > 0);
    records.check_access(tx, crate::Operation::Read).await?;

    let registry = Arc::clone(records.env().registry());
    let model = registry.model_by_id(records.model_id());
    let field = model.field(&args.field_name).ok_or_else(|| {
        Error::validation(format!(
            "Field '{}' does not exist on model '{}'",
            args.field_name, model.name
        ))
    })?;

    tx.check_read_pairs(records.env(), &[(records.model_id(), field.id)])?;
    match &field.ty {
        FieldType::Many2one { .. } | FieldType::Selection { .. } | FieldType::Many2many { .. } => {}
        other => {
            return Err(Error::user(format!(
                "Only types many2one, selection, many2many are supported for category (found type {})",
                other.type_name()
            )));
        }
    }

    let model_domain = args.search_domain.unwrap_or(Domain::TRUE);
    let extra_domain =
        args.category_domain.unwrap_or(Domain::TRUE) & args.filter_domain.unwrap_or(Domain::TRUE);

    if matches!(&field.ty, FieldType::Selection { .. }) {
        let values = search_panel_selection_range(
            tx,
            records,
            &args.field_name,
            &model_domain,
            &extra_domain,
            args.enable_counters,
            args.expand,
            args.limit,
        )
        .await?;
        if matches!(&values, Value::Map(m) if m.contains_key("error_msg")) {
            return Ok(values);
        }
        return Ok(value_map([
            ("parent_field", Value::False),
            ("values", values),
        ]));
    }

    let comodel_name = match &field.ty {
        FieldType::Many2one { comodel, .. } | FieldType::Many2many { comodel, .. } => {
            comodel.as_str()
        }
        _ => return Err(Error::programming("expected relational field")),
    };

    let comodel = records.env().records(comodel_name)?;
    let comodel_spec = records.env().registry().model_by_id(comodel.model_id());

    let mut hierarchize = args.hierarchize;
    let mut parent_name = None;
    if hierarchize {
        if let Some(pid) = comodel_spec.parent_name {
            parent_name = Some(comodel_spec.field_by_id(pid).name.to_owned());
        } else {
            hierarchize = false;
        }
    }

    let comodel_domain = args.comodel_domain.unwrap_or(Domain::TRUE);

    let domain_image = if args.enable_counters || !args.expand {
        search_panel_field_image(
            tx,
            records,
            &args.field_name,
            &model_domain,
            &extra_domain,
            args.enable_counters,
            args.expand,
            args.limit,
            false,
        )
        .await?
    } else {
        Vec::new()
    };

    let image_element_ids: Vec<i64> = domain_image
        .iter()
        .map(|e| e.id.as_int())
        .collect::<Result<_>>()?;

    let mut full_comodel_domain = comodel_domain;
    if !args.expand {
        let condition = if hierarchize {
            Domain::condition("id", Operator::ParentOf, image_element_ids.clone())?
        } else {
            Domain::condition("id", Operator::In, image_element_ids.clone())?
        };
        full_comodel_domain = full_comodel_domain & condition;
    }

    let mut fields_to_read = vec!["display_name"];
    if let Some(ref pname) = parent_name {
        fields_to_read.push(pname.as_str());
    }

    let mut opts = SearchOptions::new();
    if let Some(lim) = args.limit {
        opts = opts.limit(lim);
    }

    let found = comodel
        .search_with(
            tx,
            &full_comodel_domain,
            &opts.clone().fetch_fields(fields_to_read.iter().copied()),
        )
        .await?;
    let comodel_records = found.read_rows(tx, &fields_to_read, Load::Classic).await?;
    let positions = if hierarchize {
        let ids_to_keep = if args.expand {
            (0..comodel_records.len())
                .map(|row| required_int(&comodel_records, row, "id"))
                .collect::<Result<Vec<_>>>()?
        } else {
            image_element_ids
        };
        search_panel_sanitized_parent_hierarchy(
            &comodel_records,
            parent_name
                .as_deref()
                .ok_or_else(|| Error::programming("search panel hierarchy has no parent field"))?,
            &ids_to_keep,
        )?
    } else {
        (0..comodel_records.len()).collect()
    };
    if args.limit.is_some_and(|lim| positions.len() as u64 >= lim) {
        return Ok(search_panel_error());
    }
    let count_map: FxHashMap<i64, i64> = domain_image
        .into_iter()
        .map(|entry| Ok((entry.id.as_int()?, entry.count)))
        .collect::<Result<_>>()?;
    let mut columns = PanelColumns::new(positions.len(), false, args.enable_counters);
    let mut parents = parent_name
        .as_ref()
        .map(|_| Vec::with_capacity(positions.len()));
    let parent_column = parent_name
        .as_deref()
        .map(|name| parent_position(&comodel_records, name))
        .transpose()?;
    for row in positions {
        let id = columns.push(&comodel_records, row)?;
        if let (Some(position), Some(parents)) = (parent_column, &mut parents) {
            parents.push(parent_at(&comodel_records, row, position)?);
        }
        if let Some(counts) = &mut columns.counts {
            // A visible choice absent from the uncapped count image has zero sources.
            counts.push(count_map.get(&id).copied().unwrap_or(0));
        }
    }
    if let (Some(name), Some(parents), Some(counts)) = (&parent_name, &parents, &mut columns.counts)
    {
        search_panel_global_counters(&columns.ids, parents, counts, name)?;
    }
    let parent = parent_name.as_deref().zip(parents);
    let values = columns.finish(parent)?;
    Ok(value_map([
        (
            "parent_field",
            parent_name.as_deref().map_or(Value::False, Value::str),
        ),
        ("values", values),
    ]))
}

/// Read a classic many2one group without allocating an intermediate `[id, label]` list.
fn group_at(field: &erp_registry::FieldSpec, rows: &Rows, row: usize) -> Result<(Value, Value)> {
    if matches!(&field.ty, FieldType::Many2one { .. }) {
        let position = rows
            .layout()
            .position(field.name)
            .ok_or_else(|| Error::programming("validated group_by column disappeared"))?;
        match rows.cell(row, position)? {
            CellRef::False | CellRef::Bool(false) => {
                return Ok((Value::False, Value::str("Not Set")));
            }
            CellRef::Label { id, name } => {
                return Ok((Value::Int(id), name.map_or(Value::False, Value::str)));
            }
            CellRef::Value(value) => return extract_group_id_name(field, value),
            _ => {
                return Err(Error::programming(format!(
                    "search panel expected [id, display_name] for '{}'",
                    field.name,
                )));
            }
        }
    }
    extract_group_id_name(field, &rows.get(row, field.name)?)
}

/// Helper for group_id and group_name formatting in multi-range filters.
fn extract_group_id_name(field: &erp_registry::FieldSpec, val: &Value) -> Result<(Value, Value)> {
    if !val.is_truthy() {
        return Ok((Value::False, Value::str("Not Set")));
    }
    match &field.ty {
        FieldType::Many2one { .. } => {
            let (id, name) = relational_pair(val, field.name)?;
            Ok((Value::Int(id), name))
        }
        FieldType::Selection { values, .. } => {
            let key = val_as_str(val).ok_or_else(|| {
                Error::programming(format!(
                    "search panel expected a selection key for '{}'",
                    field.name
                ))
            })?;
            let label = values
                .as_ref()
                .and_then(|values| values.iter().find(|(k, _)| k == key))
                .map(|(_, label)| label)
                .ok_or_else(|| {
                    Error::programming(format!(
                        "search panel unknown selection key '{key}' for '{}'",
                        field.name
                    ))
                })?;
            Ok((val.clone(), Value::str(label)))
        }
        _ if matches!(
            val,
            Value::Str(_)
                | Value::Int(_)
                | Value::Bool(_)
                | Value::Float(_)
                | Value::Decimal(_)
                | Value::Date(_)
                | Value::DateTime(_)
        ) =>
        {
            Ok((val.clone(), val.clone()))
        }
        other => Err(Error::user(format!(
            "Search panel group_by does not support type {}",
            other.type_name()
        ))),
    }
}

/// Decode and validate every custom-domain entry, including entries for groups not listed now.
/// Canonical JSON keys make Python's escaped Unicode and JSON.stringify's UTF-8 equivalent.
fn group_domains(value: Option<&Value>) -> Result<FxHashMap<String, Domain>> {
    let Some(value) = value else {
        return Ok(FxHashMap::default());
    };
    if matches!(value, Value::False | Value::Bool(false)) {
        return Ok(FxHashMap::default());
    }
    let Value::Map(map) = value else {
        return Err(Error::validation(
            "search_panel_select_multi_range: group_domain must be a map of JSON-encoded group ids to domains",
        ));
    };
    if map.len() > MAX_GROUP_DOMAINS {
        return Err(Error::validation(format!(
            "search_panel_select_multi_range: group_domain accepts at most {MAX_GROUP_DOMAINS} entries, got {}",
            map.len()
        )));
    }
    let mut out = FxHashMap::default();
    for (key, value) in map.iter() {
        let id: serde_json::Value = serde_json::from_str(key).map_err(|error| {
            Error::validation(format!(
                "search_panel_select_multi_range: invalid JSON group id '{key}': {error}"
            ))
        })?;
        if id.is_array() || id.is_object() || id.is_null() {
            return Err(Error::validation(
                "search_panel_select_multi_range: group ids must be JSON scalars other than null",
            ));
        }
        let key = id.to_string();
        let domain = super::spec::domain_from_value(value)?;
        if out.insert(key.clone(), domain).is_some() {
            return Err(Error::validation(format!(
                "search_panel_select_multi_range: duplicate JSON group id {key}"
            )));
        }
    }
    Ok(out)
}

fn group_key(value: &Value) -> Result<String> {
    Ok(erp_fields::convert::value_to_json(value)?.to_string())
}

/// `search_panel_select_multi_range`: filter checklist values, grouping and counters.
pub(crate) async fn search_panel_select_multi_range(
    tx: &mut Transaction,
    records: &Records,
    mut args: SearchPanelSelectMultiRange,
) -> Result<Value> {
    args.limit = args.limit.filter(|limit| *limit > 0);
    records.check_access(tx, crate::Operation::Read).await?;

    let registry = Arc::clone(records.env().registry());
    let model = registry.model_by_id(records.model_id());
    let field = model.field(&args.field_name).ok_or_else(|| {
        Error::validation(format!(
            "Field '{}' does not exist on model '{}'",
            args.field_name, model.name
        ))
    })?;

    tx.check_read_pairs(records.env(), &[(records.model_id(), field.id)])?;
    match &field.ty {
        FieldType::Many2one { .. } | FieldType::Many2many { .. } | FieldType::Selection { .. } => {}
        other => {
            return Err(Error::user(format!(
                "Only types many2one, many2many, selection are supported for filter (found type {})",
                other.type_name()
            )));
        }
    }

    let model_domain = args.search_domain.unwrap_or(Domain::TRUE);
    let mut extra_domain =
        args.category_domain.unwrap_or(Domain::TRUE) & args.filter_domain.unwrap_or(Domain::TRUE);

    if matches!(&field.ty, FieldType::Selection { .. }) {
        let values = search_panel_selection_range(
            tx,
            records,
            &args.field_name,
            &model_domain,
            &extra_domain,
            args.enable_counters,
            args.expand,
            args.limit,
        )
        .await?;
        if matches!(&values, Value::Map(m) if m.contains_key("error_msg")) {
            return Ok(values);
        }
        return Ok(value_map([("values", values)]));
    }

    let comodel_name = match &field.ty {
        FieldType::Many2one { comodel, .. } | FieldType::Many2many { comodel, .. } => {
            comodel.as_str()
        }
        _ => return Err(Error::programming("expected relational field")),
    };

    let comodel = records.env().records(comodel_name)?;
    let comodel_spec = records.env().registry().model_by_id(comodel.model_id());

    let mut field_names = vec!["display_name"];
    if let Some(ref gb) = args.group_by {
        let group = comodel_spec.field(gb).ok_or_else(|| {
            Error::validation(format!(
                "Invalid field '{gb}' on model '{}'",
                comodel_spec.name
            ))
        })?;
        if matches!(
            &group.ty,
            FieldType::One2many { .. }
                | FieldType::Many2many { .. }
                | FieldType::Json
                | FieldType::DateRange { .. }
                | FieldType::DatetimeRange { .. }
        ) {
            return Err(Error::user(format!(
                "Search panel group_by does not support type {}",
                group.ty.type_name()
            )));
        }
        field_names.push(gb.as_str());
    }

    let mut comodel_domain = args.comodel_domain.unwrap_or(Domain::TRUE);

    let mut opts = SearchOptions::new();
    if let Some(lim) = args.limit {
        opts = opts.limit(lim);
    }

    if matches!(&field.ty, FieldType::Many2many { .. }) {
        let custom_domains = group_domains(args.group_domain.as_ref())?;
        if !args.expand {
            let img = search_panel_domain_image(
                tx,
                records,
                &args.field_name,
                &model_domain,
                false,
                None,
            )
            .await?;
            let ids: Vec<i64> = img.iter().map(|e| e.id.as_int()).collect::<Result<_>>()?;
            comodel_domain = comodel_domain & Domain::condition("id", Operator::In, ids)?;
        }

        let found = comodel
            .search_with(
                tx,
                &comodel_domain,
                &opts.clone().fetch_fields(field_names.iter().copied()),
            )
            .await?;
        let comodel_records = found.read_rows(tx, &field_names, Load::Classic).await?;
        if args
            .limit
            .is_some_and(|lim| comodel_records.len() as u64 >= lim)
        {
            return Ok(search_panel_error());
        }

        let mut count_domains = Vec::new();
        let mut batches = FxHashMap::default();
        let base_count_domain = model_domain.clone()
            & extra_domain.clone()
            & Domain::condition(&args.field_name, Operator::Ne, false)?;
        if args.enable_counters {
            for row in 0..comodel_records.len() {
                let key = if let Some(ref gb) = args.group_by {
                    let field = comodel_spec.field(gb).ok_or_else(|| {
                        Error::programming("validated group_by field disappeared")
                    })?;
                    let (id, _) = group_at(field, &comodel_records, row)?;
                    group_key(&id)?
                } else {
                    String::new()
                };
                if let std::collections::hash_map::Entry::Vacant(entry) = batches.entry(key) {
                    // Groups without an override share one default batch. Identical domains
                    // also share their grouped read even when their JSON group keys differ.
                    let custom = custom_domains
                        .get(entry.key())
                        .cloned()
                        .unwrap_or(Domain::TRUE);
                    let domain = base_count_domain.clone() & custom;
                    let index = if let Some(index) = count_domains.iter().position(|d| d == &domain)
                    {
                        index
                    } else {
                        let index = count_domains.len();
                        count_domains.push(domain);
                        index
                    };
                    entry.insert(index);
                }
            }
        }
        let counts =
            crate::read_group::panel_counts(tx, records, &args.field_name, &count_domains).await?;

        let mut columns = PanelColumns::new(
            comodel_records.len(),
            args.group_by.is_some(),
            args.enable_counters,
        );
        for row in 0..comodel_records.len() {
            let record_id = columns.push(&comodel_records, row)?;
            let group_id = if let Some(ref gb) = args.group_by {
                let field = comodel_spec
                    .field(gb)
                    .ok_or_else(|| Error::programming("validated group_by field disappeared"))?;
                Some(columns.group(field, &comodel_records, row)?)
            } else {
                None
            };
            if let Some(result_counts) = &mut columns.counts {
                let key = group_id
                    .as_ref()
                    .map(group_key)
                    .transpose()?
                    .unwrap_or_default();
                let batch = batches
                    .get(&key)
                    .and_then(|index| counts.get(*index))
                    .ok_or_else(|| {
                        Error::programming("search panel missing grouped count batch")
                    })?;
                // A choice absent from the complete, uncapped count result has zero sources.
                result_counts.push(batch.get(&record_id).copied().unwrap_or(0));
            }
        }
        return Ok(value_map([("values", columns.finish(None)?)]));
    }

    // Many2one filter
    if let Some(ref value) = args.group_domain {
        extra_domain = extra_domain & super::spec::domain_from_value(value)?;
    }

    let domain_image = if args.enable_counters || !args.expand {
        search_panel_field_image(
            tx,
            records,
            &args.field_name,
            &model_domain,
            &extra_domain,
            args.enable_counters,
            args.expand,
            args.limit,
            false,
        )
        .await?
    } else {
        Vec::new()
    };

    if !args.expand {
        let image_ids: Vec<i64> = domain_image
            .iter()
            .map(|e| e.id.as_int())
            .collect::<Result<_>>()?;
        comodel_domain = comodel_domain & Domain::condition("id", Operator::In, image_ids)?;
    }

    let found = comodel
        .search_with(
            tx,
            &comodel_domain,
            &opts.clone().fetch_fields(field_names.iter().copied()),
        )
        .await?;
    let comodel_records = found.read_rows(tx, &field_names, Load::Classic).await?;
    if args
        .limit
        .is_some_and(|lim| comodel_records.len() as u64 >= lim)
    {
        return Ok(search_panel_error());
    }

    let count_map: FxHashMap<i64, i64> = domain_image
        .into_iter()
        .map(|e| Ok((e.id.as_int()?, e.count)))
        .collect::<Result<_>>()?;

    let mut columns = PanelColumns::new(
        comodel_records.len(),
        args.group_by.is_some(),
        args.enable_counters,
    );
    for row in 0..comodel_records.len() {
        let id = columns.push(&comodel_records, row)?;
        if let Some(ref gb) = args.group_by {
            let field = comodel_spec
                .field(gb)
                .ok_or_else(|| Error::programming("validated group_by field disappeared"))?;
            columns.group(field, &comodel_records, row)?;
        }
        if let Some(counts) = &mut columns.counts {
            // A choice absent from the complete, uncapped count image has zero sources.
            counts.push(count_map.get(&id).copied().unwrap_or(0));
        }
    }
    Ok(value_map([("values", columns.finish(None)?)]))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        reason = "tests"
    )]
    use super::*;
    use erp_fields::ErrorKind;

    #[test]
    fn custom_domain_keys_canonicalize_unicode_and_reject_ambiguous_keys() {
        let map = value_map([(r#""\u00e9""#, Value::list([]))]);
        let domains = group_domains(Some(&map)).unwrap();
        assert_eq!(domains.get(r#""é""#), Some(&Domain::TRUE));

        let map = value_map([
            (r#""\u00e9""#, Value::list([])),
            (r#""é""#, Value::list([])),
        ]);
        let error = group_domains(Some(&map)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "search_panel_select_multi_range: duplicate JSON group id \"é\""
        );

        for key in ["[]", "{}", "null"] {
            let map = value_map([(key, Value::list([]))]);
            let error = group_domains(Some(&map)).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "search_panel_select_multi_range: group ids must be JSON scalars other than null"
            );
        }
    }

    fn hierarchy_rows(ids: Vec<i64>, parents: Vec<Value>) -> Rows {
        let mut builder = RowsBuilder::new(
            Layout::new([Key::Field("id"), Key::Field("parent_id")]).unwrap(),
            ids.len(),
        )
        .unwrap();
        builder
            .column("id", Column::new(ColumnData::Int(ids)))
            .unwrap();
        builder
            .column("parent_id", Column::new(ColumnData::Dyn(parents)))
            .unwrap();
        builder.finish().unwrap()
    }

    #[test]
    fn malformed_grouped_rows_are_programming_errors() {
        let error = required_text(Value::Int(1), "display_name").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "search panel row expected text for 'display_name'"
        );
        let error = get_parent_id(Some(&Value::str("bad"))).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "search panel expected a parent id or an empty parent"
        );
        let error = relational_pair(&Value::list([Value::Int(1)]), "tag_ids").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "search panel expected [id, display_name] for 'tag_ids'"
        );
    }

    #[test]
    fn choice_columns_produce_rows_with_false_parents() {
        let columns = PanelColumns {
            ids: vec![1, 2],
            names: vec![Some(Arc::from("Parent")), Some(Arc::from("Child"))],
            groups: None,
            counts: Some(vec![3, 2]),
        };
        let value = columns
            .finish(Some(("parent_id", vec![None, Some(1)])))
            .unwrap();
        let Value::Rows(rows) = value else {
            panic!("panel values must be columnar rows");
        };
        let choices: Vec<_> = rows.iter().unwrap().collect();
        assert_eq!(choices.len(), 2);
        assert_eq!(choices[0].get("parent_id").unwrap(), Value::False);
        assert_eq!(choices[1].get("parent_id").unwrap(), Value::Int(1));
        assert_eq!(choices[0].get("__count").unwrap(), Value::Int(3));
        let rows = hierarchy_rows(vec![1], vec![Value::False]);
        let error = required_int(&rows, 0, "parent_id").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "search panel row expected an integer for 'parent_id'"
        );
    }

    #[test]
    fn hierarchy_positions_preserve_order_and_exclude_missing_parent_chains() {
        let rows = hierarchy_rows(
            vec![3, 1, 2, 4],
            vec![Value::Int(2), Value::False, Value::Int(1), Value::Int(99)],
        );
        assert_eq!(
            search_panel_sanitized_parent_hierarchy(&rows, "parent_id", &[3, 4]).unwrap(),
            vec![0, 1, 2]
        );
        let mut counts = vec![2, 1, 4];
        search_panel_global_counters(
            &[3, 1, 2],
            &[Some(2), None, Some(1)],
            &mut counts,
            "parent_id",
        )
        .unwrap();
        assert_eq!(counts, vec![2, 7, 6]);
    }

    #[test]
    fn hierarchy_cycles_and_count_overflow_fail_loudly() {
        let rows = hierarchy_rows(vec![1], vec![Value::Int(1)]);
        let error = search_panel_sanitized_parent_hierarchy(&rows, "parent_id", &[1]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "search panel cycle in parent field 'parent_id'"
        );
        let mut counts = [1, i64::MAX];
        let error =
            search_panel_global_counters(&[1, 2], &[Some(2), None], &mut counts, "parent_id")
                .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(error.message(), "search panel category counter overflow");
        let error =
            search_panel_global_counters(&[1], &[Some(1)], &mut [0], "parent_id").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(
            error.message(),
            "search panel cycle in parent field 'parent_id'"
        );
        let error =
            search_panel_global_counters(&[1], &[Some(2)], &mut [0], "parent_id").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Programming);
        assert_eq!(error.message(), "search panel missing sanitized parent");
    }
}
