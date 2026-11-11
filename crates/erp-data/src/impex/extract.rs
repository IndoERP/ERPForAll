//! A record starts at a row and continues over the following rows that only fill one2many
//! columns. Non-relational columns give text entries; each relational column group (all columns
//! whose path starts with the same relational field) gives the list of sub-records extracted from
//! the group's cells of the spanned rows, recursively on the comodel, keeping only rows where the
//! group has a value.

use std::marker::PhantomData;
use std::sync::Arc;

use erp_fields::{Error, FieldType, Result};
use erp_registry::{FieldSpec, ModelSpec, Registry};

use super::message::RowSpan;
use super::row::{MappedRowView, RowSource};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// Display name (name search).
    Name,
    /// A field name, `id` (external id) or `.id` (database id).
    Field(Arc<str>),
}

impl Key {
    /// Construct a `Key::Field` from a string or `Arc<str>`.
    pub fn field_name(name: impl Into<Arc<str>>) -> Self {
        Self::Field(name.into())
    }

    /// The field name, if any.
    pub fn field(&self) -> Option<&str> {
        match self {
            Self::Name => None,
            Self::Field(name) => Some(name.as_ref()),
        }
    }

    /// `id`, `.id` or the display name: keys that reference an existing record.
    pub fn is_reference(&self) -> bool {
        match self {
            Self::Name => true,
            Self::Field(name) => name.as_ref() == "id" || name.as_ref() == ".id",
        }
    }
}

/// An extracted value: a cell, or the sub-records of a relational field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Raw<'a> {
    /// A cell.
    Text(&'a str),
    /// Sub-records.
    Records(Vec<RawRecord<'a>>),
}

impl<'a> Raw<'a> {
    /// Python truthiness: a non-empty cell or a non-empty list.
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Text(t) => !t.is_empty(),
            Self::Records(r) => !r.is_empty(),
        }
    }
}

/// An extracted record: entries in column order. Duplicate columns are refused before
/// extraction, so a key is set once per record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawRecord<'a> {
    pub entries: Vec<(Key, Raw<'a>)>,
}

impl<'a> RawRecord<'a> {
    pub fn set(&mut self, key: Key, raw: Raw<'a>) {
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = raw,
            None => self.entries.push((key, raw)),
        }
    }

    /// The value of a key.
    pub fn get(&self, key: &Key) -> Option<&Raw<'a>> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The value of a named field.
    pub fn get_field(&self, field: &str) -> Option<&Raw<'a>> {
        self.entries
            .iter()
            .find(|(k, _)| k.field() == Some(field))
            .map(|(_, v)| v)
    }
}

/// A top-level extracted record and the rows it spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted<'a> {
    pub record: RawRecord<'a>,
    pub rows: RowSpan,
}

/// A column path: field names (or `id`/`.id`), or `[Name]` for a sub-record's display name.
pub type Path = Vec<Key>;

fn relational<'s>(spec: &'s ModelSpec, key: &Key) -> Option<&'s FieldSpec> {
    let field = spec.field(key.field()?)?;
    matches!(
        field.ty,
        FieldType::Many2one { .. } | FieldType::One2many { .. } | FieldType::Many2many { .. }
    )
    .then_some(field)
}

fn is_one2many(spec: &ModelSpec, key: &Key) -> bool {
    key.field()
        .and_then(|name| spec.field(name))
        .is_some_and(|f| matches!(f.ty, FieldType::One2many { .. }))
}

/// The comodel of a relational field.
pub(crate) fn comodel<'r>(registry: &'r Registry, field: &FieldSpec) -> Result<&'r ModelSpec> {
    let name = field
        .ty
        .comodel()
        .ok_or_else(|| Error::programming(format!("field '{}' has no comodel", field.name)))?;
    registry
        .model(name)
        .ok_or_else(|| Error::programming(format!("Model '{name}' does not exist in registry.")))
}

/// The columns of one relational field: `(column index, sub-path)` pairs; a column naming the
/// field alone gets the sub-path `[Name]`.
struct Group<'s> {
    field: &'s FieldSpec,
    key: Key,
    columns: Vec<(usize, Path)>,
}

/// The relational column groups, in order of their first column.
fn groups<'s>(spec: &'s ModelSpec, paths: &[Path]) -> Vec<Group<'s>> {
    let mut out: Vec<Group<'s>> = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let Some(first) = path.first() else { continue };
        let Some(field) = relational(spec, first) else {
            continue;
        };
        let sub = if path.len() > 1 {
            path[1..].to_vec()
        } else {
            vec![Key::Name]
        };
        match out.iter_mut().find(|g| g.key == *first) {
            Some(group) => group.columns.push((index, sub)),
            None => out.push(Group {
                field,
                key: first.clone(),
                columns: vec![(index, sub)],
            }),
        }
    }
    out
}

pub(crate) fn validate(registry: &Registry, spec: &ModelSpec, paths: &[Path]) -> Result<()> {
    for path in paths {
        let first = match path.first() {
            None => return Err(Error::programming("Invalid field name ''")),
            Some(Key::Name) => continue,
            Some(Key::Field(name)) if name.as_ref() == ".id" => name.as_ref(),
            Some(Key::Field(name)) => {
                if name.contains('.') || spec.field(name.as_ref()).is_none() {
                    return Err(Error::programming(format!("Invalid field name '{name}'")));
                }
                name.as_ref()
            }
        };
        if path.len() > 1 && relational(spec, &path[0]).is_none() {
            return Err(Error::programming(format!(
                "Invalid field name '{}': '{first}' of model '{}' is not a relational field and \
                 has no sub-fields",
                display(path),
                spec.name
            )));
        }
    }
    for group in groups(spec, paths) {
        let comodel = comodel(registry, group.field)?;
        let sub: Vec<Path> = group.columns.into_iter().map(|(_, p)| p).collect();
        validate(registry, comodel, &sub)?;
    }
    Ok(())
}

/// A path as typed (`a/b/.id`, the display name as the bare field).
fn display(path: &Path) -> String {
    path.iter()
        .filter_map(Key::field)
        .collect::<Vec<_>>()
        .join("/")
}

/// Extract up to `batch_records_limit` records from `rows` starting at `start_row`.
/// Stops before exceeding `limit` row index or `batch_records_limit` extracted records.
/// Returns the batch of extracted records and the index of the next row to process.
pub fn extract_batch<'a, R: RowSource<'a>>(
    registry: &Registry,
    spec: &ModelSpec,
    paths: &[Path],
    rows: R,
    start_row: usize,
    batch_records_limit: usize,
    limit: usize,
) -> Result<(Vec<Extracted<'a>>, usize)> {
    extract_batch_dyn(
        registry,
        spec,
        paths,
        &rows,
        start_row,
        batch_records_limit,
        limit,
    )
}

fn extract_batch_dyn<'s, 'a>(
    registry: &Registry,
    spec: &ModelSpec,
    paths: &[Path],
    rows: &'s (dyn RowSource<'a> + 's),
    start_row: usize,
    batch_records_limit: usize,
    limit: usize,
) -> Result<(Vec<Extracted<'a>>, usize)> {
    for index in start_row..rows.len().min(limit) {
        let len = rows.row_len(index);
        if len != paths.len() {
            return Err(Error::programming(format!(
                "row {index} has {} cells for {} columns",
                len,
                paths.len()
            )));
        }
    }

    let one2many: Vec<bool> = paths
        .iter()
        .map(|p| p.first().is_some_and(|k| is_one2many(spec, k)))
        .collect();

    let only_one2many = |r: usize| {
        let mut any_o2m = false;
        let n = rows.row_len(r);
        for (i, is_o2m) in one2many.iter().enumerate().take(n) {
            let cell = rows.cell(r, i);
            if !cell.is_empty() {
                if *is_o2m {
                    any_o2m = true;
                } else {
                    return false;
                }
            }
        }
        any_o2m
    };

    let groups = groups(spec, paths);
    let mut out = Vec::new();
    let mut index = start_row;
    while index < rows.len() && index < limit && out.len() < batch_records_limit {
        let mut record = RawRecord::default();
        let n_cols = rows.row_len(index);
        for (path_idx, path) in paths.iter().enumerate().take(n_cols) {
            let Some(first) = path.first() else { continue };
            if relational(spec, first).is_none() {
                record.set(first.clone(), Raw::Text(rows.cell(index, path_idx)));
            }
        }
        let mut end = index + 1;
        while end < rows.len() && only_one2many(end) {
            end += 1;
        }
        for Group {
            field,
            key,
            columns,
        } in &groups
        {
            let comodel = comodel(registry, field)?;
            let sub_paths: Vec<Path> = columns.iter().map(|(_, p)| p.clone()).collect();
            let sub_row_indices: Vec<usize> = (index..end)
                .filter(|&r| columns.iter().any(|(c, _)| !rows.cell(r, *c).is_empty()))
                .collect();
            if !sub_row_indices.is_empty() {
                let sub_cols: Vec<usize> = columns.iter().map(|(c, _)| *c).collect();
                let sub_view = MappedRowView {
                    source: rows,
                    row_indices: &sub_row_indices,
                    col_indices: &sub_cols,
                    _marker: PhantomData,
                };
                let (subs, _) = extract_batch_dyn(
                    registry,
                    comodel,
                    &sub_paths,
                    &sub_view,
                    0,
                    usize::MAX,
                    usize::MAX,
                )?;
                record.set(
                    key.clone(),
                    Raw::Records(subs.into_iter().map(|e| e.record).collect()),
                );
            } else {
                record.set(key.clone(), Raw::Records(Vec::new()));
            }
        }
        out.push(Extracted {
            record,
            rows: RowSpan {
                from: index,
                to: end - 1,
            },
        });
        index = end;
    }
    Ok((out, index))
}
