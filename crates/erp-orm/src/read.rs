//! Reading field values (ENV §4.8, FIELD §4.4, RSET §4.15).
//!
//! A cache miss on a stored field loads every prefetchable field of the model for the uncached ids
//! of the record's prefetch set, one `SELECT … WHERE id = ANY($1)` per chunk of ids. Values already
//! cached (including pending writes) are never overwritten.
//!
//! Every public field read of a database record (or its in-memory shadow) checks model, record
//! and field access for its environment before exposing a cached value. Values remain shared by the transaction; only the principal-keyed
//! access memo can reuse a permission decision. Outside superuser mode the read
//! predicate also filters stored fetches in SQL, so hidden prefetch companions are
//! not loaded. Reading an explicitly hidden record is an `Access` error; a deleted one remains
//! `Missing`.
//!
//! Computed, related and cache-only fields go through the compute engine
//! ([`Transaction::ensure`]): pending recomputations are settled first, non-stored values are
//! computed for the uncached part of the prefetch set, new records read their origin or defaults.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::transaction::raw_row::{CellError, CellSource};
use erp_fields::{
    Cell, ColumnType, Date, DateTime, Decimal, Error, FieldType, RecId, Result, Value,
};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec};
use rustc_hash::FxHashSet;

use crate::Transaction;
use crate::access::Operation;
use crate::env::Env;
use crate::field_access::FieldOp;
use crate::records::{Ids, Records};
use crate::rows_build::{Level, Sink};

/// Decode column `idx` of a fetched row (a [`RawRow`](crate::transaction::raw_row::RawRow)) or a
/// collected driver `Row` as a cache cell of column type `ty`.
pub(crate) fn decode<R: CellSource + ?Sized>(row: &R, idx: usize, ty: ColumnType) -> Result<Cell> {
    let map = |e: CellError| {
        Error::new(
            erp_fields::ErrorKind::Database,
            "could not decode a column value",
        )
        .with_source(e)
    };
    Ok(match ty {
        ColumnType::Bool => row
            .cell::<Option<bool>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Bool),
        ColumnType::Int4 => match row.column_type(idx) {
            Some(&tokio_postgres::types::Type::INT8) => row
                .cell::<Option<i64>>(idx)
                .map_err(map)?
                .map(|v| {
                    i32::try_from(v).map_err(|e| {
                        Error::new(erp_fields::ErrorKind::Database, "int8 out of int4 range")
                            .with_source(e)
                    })
                })
                .transpose()?
                .map_or(Cell::Null, Cell::Int),
            Some(&tokio_postgres::types::Type::INT2) => row
                .cell::<Option<i16>>(idx)
                .map_err(map)?
                .map(i32::from)
                .map_or(Cell::Null, Cell::Int),
            _ => row
                .cell::<Option<i32>>(idx)
                .map_err(map)?
                .map_or(Cell::Null, Cell::Int),
        },
        ColumnType::Int8 => row
            .cell::<Option<i64>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Id),
        ColumnType::Float8 => row
            .cell::<Option<f64>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Float),
        ColumnType::Numeric => row
            .cell::<Option<Decimal>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Decimal),
        ColumnType::Varchar(_) | ColumnType::Text => row
            .cell::<Option<&str>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, |s| Cell::Str(Arc::from(s))),
        ColumnType::Date => row
            .cell::<Option<Date>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Date),
        ColumnType::Timestamp => match row.column_type(idx) {
            Some(&tokio_postgres::types::Type::TIMESTAMPTZ) => row
                .cell::<Option<erp_fields::TimestampTz>>(idx)
                .map_err(map)?
                .map(|tz| DateTime::from_micros(tz.unix_micros()))
                .map_or(Cell::Null, Cell::DateTime),
            _ => row
                .cell::<Option<DateTime>>(idx)
                .map_err(map)?
                .map_or(Cell::Null, Cell::DateTime),
        },
        ColumnType::Uuid => row
            .cell::<Option<erp_fields::Uuid>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Uuid),
        ColumnType::Xid8 => row
            .cell::<Option<erp_fields::Xid8>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::Xid8),
        ColumnType::TimestampTz => row
            .cell::<Option<erp_fields::TimestampTz>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, Cell::TimestampTz),
        ColumnType::Jsonb => row
            .cell::<Option<erp_fields::serde_json::Value>>(idx)
            .map_err(map)?
            .map_or(Cell::Null, |v| Cell::Json(Arc::new(v))),
        // ranges are read with their field's bounds
        ColumnType::DateRange(bounds) => {
            match row
                .cell::<Option<erp_fields::StoredRange<Date>>>(idx)
                .map_err(map)?
            {
                None => Cell::Null,
                Some(stored) => Cell::DateRange(
                    stored
                        .into_declared(bounds)
                        .map_err(|reason| Error::new(erp_fields::ErrorKind::Database, reason))?,
                ),
            }
        }
        ColumnType::TsRange(bounds) => {
            match row
                .cell::<Option<erp_fields::StoredRange<DateTime>>>(idx)
                .map_err(map)?
            {
                None => Cell::Null,
                Some(stored) => Cell::DatetimeRange(
                    stored
                        .into_declared(bounds)
                        .map_err(|reason| Error::new(erp_fields::ErrorKind::Database, reason))?,
                ),
            }
        }
    })
}

/// A many2one target and its display name.
pub(crate) type Label = (RecId, Option<Arc<str>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Load {
    /// `load='_classic_read'` (the default): a many2one reads `(id, display_name)`.
    #[default]
    Classic,
    /// Any other load (`load=None`): a many2one reads its id.
    Raw,
}

/// The dynamic value of a cell as reads return it: NULL reads as the type's null value;
/// a many2one_reference reads `0` (REL §4.11), an x2many a list
/// of ids.
pub fn read_value(ty: &FieldType, cell: &Cell) -> Value {
    match (ty, cell) {
        (FieldType::Boolean, Cell::Null) => Value::Bool(false),
        (FieldType::Integer | FieldType::Many2oneReference { .. }, Cell::Null) => Value::Int(0),
        (FieldType::One2many { .. } | FieldType::Many2many { .. }, Cell::Null) => Value::list([]),
        (FieldType::Float { .. } | FieldType::Monetary { .. }, Cell::Null) => Value::Float(0.0),
        (_, cell) => Value::from_cell(cell),
    }
}

pub(crate) fn missing_error(records: &Records, id: RecId) -> Error {
    Error::missing(format!(
        "Record does not exist or has been deleted.\n(Record: {}({id},), User: {})",
        records.model_name(),
        records.env().uid()
    ))
}

pub(crate) fn field_of<'a>(spec: &'a ModelSpec, name: &str) -> Result<&'a FieldSpec> {
    spec.field(name)
        .ok_or_else(|| Error::programming(format!("Invalid field '{name}' on '{}'", spec.name)))
}

impl Transaction {
    /// Make sure `field` is cached for `records`, prefetching for the prefetch set.
    pub(crate) async fn load(&mut self, records: &Records, fid: FieldId) -> Result<()> {
        self.load_columns(records, fid, None, &[]).await
    }

    /// [`Transaction::load`] fetching `field` without its prefetch group (for the prefetch set
    /// all the same): the old value a compute compares its result with (audit W12). The columns
    /// that marking the field's dependents reads on the same records come with it
    /// ([`Transaction::trigger_columns`]), so a changed value costs no second read.
    pub(crate) async fn load_own(&mut self, records: &Records, fid: FieldId) -> Result<()> {
        let also = self.trigger_columns((records.model_id(), fid))?;
        self.load_columns(records, fid, Some(&also), &[]).await
    }

    /// The old values of a compute's own stored fields `group` for `records`, in one statement:
    /// the fields together, plus the columns that marking their dependents reads on the same
    /// records ([`Transaction::trigger_columns`]); not the prefetch group (audit W12). The rows
    /// read are those that lack any group field, so a partly cached group still costs one
    /// statement (a value already cached is never replaced); a field without a column is skipped.
    pub(crate) async fn load_own_group(
        &mut self,
        records: &Records,
        group: &[FieldId],
    ) -> Result<()> {
        let model = records.model_id();
        let spec = records.spec();
        let columns: Vec<FieldId> = group
            .iter()
            .copied()
            .filter(|f| spec.field_by_id(*f).column().is_some())
            .collect();
        // the statement is keyed on a field some record lacks; every record lacking any group
        // field is read
        let Some(&first) = columns.iter().find(|f| {
            records
                .rec_ids()
                .iter()
                .any(|id| !id.is_new() && !self.cache.contains(model, **f, *id))
        }) else {
            return Ok(());
        };
        let mut also: Vec<FieldId> = Vec::new();
        for &f in &columns {
            for g in std::iter::once(f).chain(self.trigger_columns((model, f))?) {
                if g != first && !also.contains(&g) {
                    also.push(g);
                }
            }
        }
        self.load_columns(records, first, Some(&also), &columns)
            .await
    }

    /// `also`: `None` fetches the prefetch group with `fid`, `Some` these fields only. A record is
    /// read when it lacks `fid` or any field of `keyed` (fields fetched by the same statement:
    /// one read for a partly cached set); values already cached are kept.
    async fn load_columns(
        &mut self,
        records: &Records,
        fid: FieldId,
        also: Option<&[FieldId]>,
        keyed: &[FieldId],
    ) -> Result<()> {
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let field = spec.field_by_id(fid);
        if field.column().is_none() {
            return Ok(());
        }
        self.admit_records(records).await?;
        let model = records.model_id();
        let lacks = |cache: &crate::cache::Cache, id: RecId| {
            !cache.contains(model, fid, id) || keyed.iter().any(|f| !cache.contains(model, *f, id))
        };
        let mut seen: FxHashSet<RecId> = FxHashSet::default();
        let mut wanted: Vec<RecId> = records
            .rec_ids()
            .iter()
            .copied()
            .filter(|id| !id.is_new() && lacks(&self.cache, *id) && seen.insert(*id))
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }
        if !records.env().su() {
            self.prepare_access(records.env()).await?;
        }
        self.check_model(records.env(), model, Operation::Read)?;
        let chunk = self.config().prefetch_chunk.max(1);
        let limit = chunk.max(wanted.len());
        for id in records.prefetch_ids() {
            if wanted.len() >= limit {
                break;
            }
            if !id.is_new() && lacks(&self.cache, *id) && seen.insert(*id) {
                wanted.push(*id);
            }
        }
        let fields: Vec<FieldId> = match also {
            None if field.prefetch && records.env().context().prefetch_fields() => {
                let mask = self.field_mask(records.env(), model)?;
                let mut group = spec.prefetch_fields.clone();
                if !mask.is_all() {
                    group.retain(|f| mask.readable(*f));
                    if !group.contains(&fid) {
                        group.push(fid);
                    }
                }
                group
            }
            Some(also) if !also.is_empty() => {
                let mask = self.field_mask(records.env(), model)?;
                let mut fields = vec![fid];
                for f in also {
                    if mask.readable(*f) && !fields.contains(f) {
                        fields.push(*f);
                    }
                }
                fields
            }
            _ => vec![fid],
        };
        let generated: Vec<FieldId> = fields
            .iter()
            .copied()
            .filter(|f| spec.field_by_id(*f).is_generated())
            .collect();
        if !generated.is_empty() {
            Box::pin(self.flush_fields(model, &generated)).await?;
        }
        // Record rules restrict the rows in SQL; unrestricted reads, and records
        // the read memo already let through (SEC-017), keep the plain statement.
        let env = records.env().clone();
        let wanted_real: Vec<erp_fields::RealId> =
            wanted.iter().filter_map(|id| id.as_real()).collect();
        let has_unknown = if env.su() {
            false
        } else {
            self.has_unknown_read(&env, model, &wanted_real)?
        };
        let restriction = if !has_unknown {
            None
        } else {
            crate::search::read_restriction(self, records).await?
        };
        if let Some(filter) = &restriction {
            crate::search::flush_read_inputs(self, &filter.fields).await?;
            let fetched =
                crate::search::fetch_rows(self, spec, &fields, &wanted_real, Some(filter)).await?;
            // a decision made with a start set found by a search is never remembered
            if !filter.volatile {
                self.remember_read(
                    &env,
                    model,
                    fetched.iter().filter_map(|id| id.as_real()),
                    &filter.fields,
                )?;
            }
            if let Some(id) = records
                .rec_ids()
                .iter()
                .find(|id| !id.is_new() && !self.cache.contains(model, fid, **id))
            {
                let real: Vec<erp_fields::RealId> = id.as_real().into_iter().collect();
                if let Some(error) = self
                    .record_access_error(&env, model, Operation::Read, &real)
                    .await?
                {
                    return Err(error);
                }
                return Err(missing_error(records, *id));
            }
            return Ok(());
        }
        let sql = erp_sql::dml::select_by_ids_cached(self.registry(), spec, &fields);
        // rows are cached as they arrive (design note `perf-stream.md` §3.1); the row count of
        // every chunk together is known, so the cache grows once for the whole read (no resize
        // of the slot map between chunks), and one id buffer serves every chunk
        let cacher = crate::search::RowCacher::new(spec, &fields);
        let mut rows = cacher.sink(wanted_real.len());
        let mut chunk_ids: Vec<i64> = Vec::with_capacity(wanted_real.len().min(chunk));
        for ids in wanted_real.chunks(chunk) {
            chunk_ids.clear();
            chunk_ids.extend(ids.iter().map(|id| id.get()));
            let params: [&(dyn tokio_postgres::types::ToSql + Sync); 1] = [&chunk_ids];
            self.query_each_args(
                &sql,
                crate::transaction::Args::Prepared(&params),
                |cache, row| rows.cache_row(cache, row).map(|_| ()),
            )
            .await?;
        }
        if let Some(id) = records
            .rec_ids()
            .iter()
            .find(|id| !id.is_new() && !self.cache.contains(model, fid, **id))
        {
            return Err(missing_error(records, *id));
        }
        Ok(())
    }

    /// The model's resolved `display_name`, including an addon compute when declared.
    pub(crate) async fn display_names(&mut self, records: &Records) -> Result<Vec<Cell>> {
        let reader = records.env().clone();
        self.display_names_for(records, &reader).await
    }

    /// [`Self::display_names`] for `records` that may be read in a superuser environment on behalf
    /// of `reader`, the environment the labels are finally shown to: the company bound of a
    /// record name applies to the reader at every level of a chain of links (review SD3).
    pub(crate) async fn display_names_for(
        &mut self,
        records: &Records,
        reader: &Env,
    ) -> Result<Vec<Cell>> {
        let field = records.spec().field("display_name").ok_or_else(|| {
            Error::programming(format!("{}.display_name is missing", records.model_name()))
        })?;
        if self
            .shared
            .graph
            .model(records.model_id())
            .is_builtin_display_name(field.id)
        {
            return self.builtin_display_names_for(records, reader).await;
        }
        let records = records.with_env(records.env().with_label_reader(reader));
        self.field_cells(&records, field.id).await
    }

    /// The framework fallback (RSET §4.15), separate from custom-compute resolution.
    /// `reader` retains the company bound of names reached through privileged links.
    pub(crate) async fn builtin_display_names_for(
        &mut self,
        records: &Records,
        reader: &Env,
    ) -> Result<Vec<Cell>> {
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let Some(rec) = spec.rec_name.map(|f| spec.field_by_id(f)) else {
            return Ok(records
                .rec_ids()
                .iter()
                .map(|id| Cell::Str(Arc::from(format!("{},{id}", spec.name))))
                .collect());
        };
        if matches!(rec.ty, FieldType::Many2one { .. }) {
            // the label is the comodel's display_name (b)
            self.ensure(records, rec.id).await?;
            let cells =
                self.cells_of(records.env(), records.model_id(), rec.id, records.rec_ids())?;
            let labels = self
                .many2one_labels_for(records, reader, rec.id, &cells)
                .await?;
            return Ok(labels
                .into_iter()
                .map(|label| match label {
                    Some((_, Some(name))) => Cell::Str(name),
                    _ => Cell::Null,
                })
                .collect());
        }
        let rec_id = rec.id;
        let cells = if rec.inherited.is_some() {
            // `_inherits`: the parent's name
            self.inherited_cells(records, rec_id).await?
        } else {
            if spec.field_by_id(rec_id).translate {
                self.active_languages().await?;
            }
            self.ensure(records, rec_id).await?;
            self.cells_of(records.env(), records.model_id(), rec_id, records.rec_ids())?
        };
        let tz = records.env().tz().to_owned();
        Ok(cells
            .into_iter()
            .map(|cell| match cell {
                Cell::Str(s) if s.is_empty() => Cell::Null,
                Cell::Str(s) => Cell::Str(s),
                Cell::Null | Cell::Bool(false) | Cell::Int(0) => Cell::Null,
                Cell::Date(d) => Cell::Str(Arc::from(d.to_string())),
                Cell::DateTime(d) => {
                    Cell::Str(Arc::from(d.to_timezone(&tz).unwrap_or(d).to_string()))
                }
                Cell::Uuid(v) => Cell::Str(Arc::from(v.to_string())),
                Cell::Xid8(v) => Cell::Str(Arc::from(v.to_string())),
                Cell::TimestampTz(v) => Cell::Str(Arc::from(v.to_string())),
                Cell::Int(i) => Cell::Str(Arc::from(i.to_string())),
                Cell::Float(0.0) => Cell::Null,
                Cell::Float(f) => Cell::Str(Arc::from(format!("{f:?}"))),
                Cell::Decimal(d) if d.is_zero() => Cell::Null,
                Cell::Decimal(d) => Cell::Str(Arc::from(d.to_string())),
                Cell::Bool(true) => Cell::Str(Arc::from("True")),
                // many2one rec names are handled above; a many2one_reference reads its id
                Cell::Id(id) => Cell::Str(Arc::from(id.to_string())),
                Cell::Json(j) => Cell::Str(Arc::from(j.to_string())),
                // `[lower, upper]` with False for an unbounded end, like the dynamic value
                cell @ (Cell::DateRange(_) | Cell::DatetimeRange(_)) => {
                    Cell::Str(Arc::from(Value::from_cell(&cell).to_string()))
                }
                Cell::Ids(ids) => Cell::Str(Arc::from(
                    ids.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                )),
            })
            .collect())
    }

    /// `(id, display_name)` of the targets of many2one cells, computed as superuser (REL §4.4.1,
    /// the targets' rec names at most.
    pub(crate) fn many2one_labels<'a>(
        &'a mut self,
        records: &'a Records,
        field: FieldId,
        cells: &'a [Cell],
    ) -> crate::dispatch::BoxFuture<'a, Result<Vec<Option<Label>>>> {
        self.many2one_labels_for(records, records.env(), field, cells)
    }

    /// [`Self::many2one_labels`] for the `reader` the labels are shown to (review SD3): the
    /// record names of the targets that are company-bound related fields are read with the
    /// reader's companies, however deep the chain of links goes.
    pub(crate) fn many2one_labels_for<'a>(
        &'a mut self,
        records: &'a Records,
        reader: &'a Env,
        field: FieldId,
        cells: &'a [Cell],
    ) -> crate::dispatch::BoxFuture<'a, Result<Vec<Option<Label>>>> {
        // boxed: display names of a model whose rec name is a many2one recurse through here
        Box::pin(self.many2one_labels_inner(records, reader, field, cells))
    }

    async fn many2one_labels_inner(
        &mut self,
        records: &Records,
        reader: &Env,
        field: FieldId,
        cells: &[Cell],
    ) -> Result<Vec<Option<Label>>> {
        let registry = Arc::clone(records.env().registry());
        let spec = registry.model_by_id(records.model_id());
        let comodel = crate::relational::comodel_of(&registry, spec.field_by_id(field))?;
        let targets: Vec<RecId> = {
            let mut seen = FxHashSet::default();
            cells
                .iter()
                .filter_map(crate::relational::id_of)
                .filter(|id| seen.insert(*id))
                .collect()
        };
        if targets.is_empty() {
            return Ok(vec![None; cells.len()]);
        }
        let co = Records::from_parts(
            records.env().engine_sudo(),
            comodel.id,
            Arc::from(targets.clone()),
            Arc::from(targets.clone()),
        );
        let rec_name = comodel
            .rec_name
            .filter(|f| comodel.field_by_id(*f).has_column());
        let existing: Vec<RecId> = match rec_name {
            Some(f) => {
                let name = comodel.field_by_id(f).name;
                co.fetch(self, Some(&[name])).await?;
                targets
                    .iter()
                    .copied()
                    .filter(|t| self.cache.contains(comodel.id, f, *t))
                    .collect()
            }
            None => targets.clone(),
        };
        let co = co.browse_ids(existing.iter().copied());
        // a rec name that is a company-bound related field is read with the reader's companies,
        // not as superuser, which would drop the bound (review SC1); the value is still computed
        // without the reader's record rules
        let bound_name = comodel.rec_name.is_some_and(|f| {
            crate::related_company::Bound::of(reader, comodel.id, comodel.field_by_id(f)).is_some()
        });
        // a refusal of the bound names no hop id: the holder was reached through a link, and the
        // reader may not read the record behind its hop (review SD4); the environment says so
        // explicitly (review TD3)
        let co = if bound_name {
            Records::from_parts(
                reader.through_link(),
                comodel.id,
                Arc::from(existing.clone()),
                Arc::from(existing.clone()),
            )
        } else {
            co
        };
        let names = self.display_names_for(&co, reader).await?;
        let by_id: rustc_hash::FxHashMap<RecId, Option<Arc<str>>> = existing
            .iter()
            .copied()
            .zip(names.into_iter().map(|c| match c {
                Cell::Str(s) => Some(s),
                _ => None,
            }))
            .collect();
        Ok(cells
            .iter()
            .map(|c| {
                let id = crate::relational::id_of(c)?;
                by_id.get(&id).map(|name| (id, name.clone()))
            })
            .collect())
    }

    /// The cells of one field for every record, in order.
    pub(crate) async fn field_cells(
        &mut self,
        records: &Records,
        fid: FieldId,
    ) -> Result<Vec<Cell>> {
        self.field_read_checks(records, fid).await?;
        self.field_cells_checked(records, fid).await
    }

    /// The access steps every read of a field's value passes before it touches the cache
    /// ([`Self::field_cells`], and the column builder of [`Records::read_rows`], which must never
    /// skip them: step 5, §13).
    pub(crate) async fn field_read_checks(
        &mut self,
        records: &Records,
        fid: FieldId,
    ) -> Result<()> {
        self.admit_records(records).await?;
        if !records.env().su() {
            // a read served from the cache still decides on the field and record checks, so bring
            // the policy up to date and resolve the subject before them, whether or not a change is
            // pending (same as `load` and the write path). A superuser needs no decision, so no
            // flush starts from inside a compute or constraint that runs elevated.
            self.prepare_access(records.env()).await?;
        }
        // attribute access, `mapped`, `read`: every read of a field's value passes here
        self.check_field(records.env(), records.model_id(), fid, FieldOp::Read)?;
        crate::database_generated::check_persisted(
            records.spec(),
            records.spec().field_by_id(fid),
            records.rec_ids(),
        )?;
        if records.env().su() {
            return self.check_model(records.env(), records.model_id(), Operation::Read);
        }
        // Origin-less new records contain only in-memory form/default values: there is no
        // database record to authorize (ENV/new-record semantics). Prefetch extras do not
        // change that; their own origins are checked if those records are actually read.
        if records
            .rec_ids()
            .iter()
            .all(|id| id.is_new() && id.origin().is_none())
        {
            return Ok(());
        }
        self.check_model(records.env(), records.model_id(), Operation::Read)?;
        // Cache occupancy decides only whether a stored load can carry the decision; it
        // never grants access. Already-cached form values must not force an origin data read.
        let generated_alias = !records.spec().field_by_id(fid).is_generated()
            && self
                .shared
                .graph
                .generated_read((records.model_id(), fid))
                .is_some();
        let load_cold = !generated_alias
            && records
                .rec_ids()
                .iter()
                .any(|id| !self.cache.contains(records.model_id(), fid, *id));
        if records.rec_ids().iter().any(|id| id.origin().is_some()) {
            // A new record can already hold values copied from its origin by another
            // environment. Check those origins even when no load will occur (SEC-080).
            self.field_record_read_checks(&records.origin(), fid, load_cold)
                .await?;
        } else {
            self.field_record_read_checks(records, fid, load_cold)
                .await?;
        }
        if generated_alias {
            let path = self
                .shared
                .graph
                .generated_read((records.model_id(), fid))
                .map(|read| Arc::clone(&read.path))
                .ok_or_else(|| {
                    Error::programming("Generated alias access path was not prepared")
                })?;
            // Even a cached/stored alias must authorize its owning records as the caller.
            let _authorized_values = Box::pin(self.related_values(records, &path, None)).await?;
        }
        Ok(())
    }

    /// The record half after field/model checks. A cold stored load decides access in its
    /// existing SELECT and remembers that decision; cached values still pass through the
    /// principal-keyed read memo. Reuse that memo (not just the explicit-check memo), so a
    /// preceding read/fetch or onchange-origin check does not add a second decision statement.
    async fn field_record_read_checks(
        &mut self,
        records: &Records,
        fid: FieldId,
        load_cold: bool,
    ) -> Result<()> {
        let field = records.spec().field_by_id(fid);
        if load_cold && field.has_column() && !self.reads_by_own_path(records.model_id(), field) {
            self.load(records, fid).await?;
        }
        // Origins were resolved by the caller; remaining new ids have no database row.
        let real: Vec<erp_fields::RealId> = records
            .rec_ids()
            .iter()
            .filter_map(|id| id.as_real())
            .collect();
        // Explicit check_access and a cold stored fetch may already have authorized all of
        // these records under this exact binding. Occupancy of the value cache is irrelevant.
        if !self.has_unknown_read(records.env(), records.model_id(), &real)? {
            return Ok(());
        }
        let visible: FxHashSet<RecId> = self
            .readable_ids(records.env(), records.model_id(), records.rec_ids())
            .await?
            .into_iter()
            .collect();
        let mut seen = FxHashSet::default();
        let denied: Vec<erp_fields::RealId> = records
            .rec_ids()
            .iter()
            .filter(|id| !visible.contains(id))
            .filter_map(|id| id.as_real())
            .filter(|id| seen.insert(*id))
            .collect();
        if !denied.is_empty()
            && let Some(error) = self
                .record_access_error(records.env(), records.model_id(), Operation::Read, &denied)
                .await?
        {
            return Err(error);
        }
        Ok(())
    }

    /// Whether the values of `field` are read through their own path ([`Self::field_cells`]'s
    /// branches for `id`, the default `display_name`, `_inherits` fields and stored x2many), not
    /// from the field's cache column after [`Self::ensure`].
    fn reads_by_own_path(&self, model: ModelId, field: &FieldSpec) -> bool {
        matches!(field.ty, FieldType::Id)
            || self
                .shared
                .graph
                .model(model)
                .is_builtin_display_name(field.id)
            || field.inherited.is_some()
            || field.translate
            || field.company_dependent
            || (field.ty.is_x2many() && field.compute.is_none() && field.related.is_none())
    }

    /// [`Self::field_cells`] after [`Self::field_read_checks`].
    /// Currency normalization may admit only its output-field lookup separately;
    /// native computations and their input reads still execute in the original environment.
    pub(crate) async fn field_cells_checked(
        &mut self,
        records: &Records,
        fid: FieldId,
    ) -> Result<Vec<Cell>> {
        let registry = Arc::clone(records.env().registry());
        let field = registry.model_by_id(records.model_id()).field_by_id(fid);
        crate::database_generated::check_persisted(records.spec(), field, records.rec_ids())?;
        if matches!(field.ty, FieldType::Id) {
            return Ok(records
                .rec_ids()
                .iter()
                .map(|id| Cell::Id(id.get()))
                .collect());
        }
        if self
            .shared
            .graph
            .model(records.model_id())
            .is_builtin_display_name(fid)
        {
            return self.builtin_display_names_for(records, records.env()).await;
        }
        if field.inherited.is_some() {
            return self.inherited_cells(records, fid).await;
        }
        if field.ty.is_x2many() && field.compute.is_none() && field.related.is_none() {
            return self.x2many_cells(records, fid).await;
        }
        if field.translate {
            self.active_languages().await?;
        }
        self.ensure(records, fid).await?;
        self.cells_of(records.env(), records.model_id(), fid, records.rec_ids())
    }
}

impl Records {
    /// The cell of a field on a singleton; NULL on an empty set (no query); the singleton error on
    /// several records (/010).
    pub async fn cell(&self, tx: &mut Transaction, field: &str) -> Result<Cell> {
        tx.admit_records(self).await?;
        let fid = field_of(self.spec(), field)?.id;
        match self.len() {
            0 => Ok(Cell::Null),
            1 => Ok(tx
                .field_cells(self, fid)
                .await?
                .into_iter()
                .next()
                .unwrap_or(Cell::Null)),
            _ => Err(Error::programming(format!("Expected singleton: {self}"))),
        }
    }

    /// One cell per record (`mapped` on a scalar field).
    pub async fn cells(&self, tx: &mut Transaction, field: &str) -> Result<Vec<Cell>> {
        let fid = field_of(self.spec(), field)?.id;
        tx.field_cells(self, fid).await
    }

    pub async fn value(&self, tx: &mut Transaction, field: &str) -> Result<Value> {
        let cell = self.cell(tx, field).await?;
        let spec_field = field_of(self.spec(), field)?;
        if matches!(
            spec_field.ty,
            FieldType::Char { .. } | FieldType::Text | FieldType::Html
        ) && matches!(cell, Cell::Json(_))
        {
            return Err(Error::programming(format!(
                "unprojected translated cell in {}.{field}",
                self.model_name()
            )));
        }
        Ok(read_value(&spec_field.ty, &cell))
    }

    /// `mapped(field)` on the dynamic surface.
    pub async fn values(&self, tx: &mut Transaction, field: &str) -> Result<Vec<Value>> {
        let spec_field = field_of(self.spec(), field)?;
        let cells = self.cells(tx, field).await?;
        if matches!(
            spec_field.ty,
            FieldType::Char { .. } | FieldType::Text | FieldType::Html
        ) {
            for cell in &cells {
                if matches!(cell, Cell::Json(_)) {
                    return Err(Error::programming(format!(
                        "unprojected translated cell in {}.{field}",
                        self.model_name()
                    )));
                }
            }
        }
        let ty = spec_field.ty.clone();
        Ok(cells.iter().map(|c| read_value(&ty, c)).collect())
    }

    /// `read(fields)`: one dictionary per existing record with `id` and the requested fields
    /// (every field when `fields` is empty). The stored columns are fetched in one query; records
    /// that no longer exist are left out, not an error. Many2one values read as
    /// `[id, display_name]` (display names computed as superuser, `False` for a target that no
    /// longer exists), x2many values as the list of visible ids, references as `"model,id"`.
    pub async fn read(
        &self,
        tx: &mut Transaction,
        fields: &[&str],
    ) -> Result<Vec<BTreeMap<String, Value>>> {
        self.read_with(tx, fields, Load::Classic).await
    }

    /// `read(fields, load=…)`: [`Records::read`] with many2one values as ids for [`Load::Raw`].
    pub async fn read_with(
        &self,
        tx: &mut Transaction,
        fields: &[&str],
        load: Load,
    ) -> Result<Vec<BTreeMap<String, Value>>> {
        // the batch is converted at this public boundary until PERF.ROWS-API switches the
        // signature (step 1); callers that can take the batch use `read_rows`
        let rows = self.read_rows(tx, fields, load).await?;
        let keys: Vec<&str> = rows.keys().collect();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows.indices() {
            let mut map = BTreeMap::new();
            for (pos, key) in keys.iter().enumerate() {
                let value = rows.get_at(row, pos)?.ok_or_else(|| {
                    Error::programming(format!(
                        "key '{key}' is absent in row {row} of a read of {} (internal error)",
                        self.model_name()
                    ))
                })?;
                map.insert((*key).to_owned(), value);
            }
            out.push(map);
        }
        Ok(out)
    }

    /// Records that no longer exist are left out; values are those of
    /// [`Records::read_with`].
    ///
    /// # Errors
    /// An unknown field (`Programming`), a field or record the environment may not read
    /// (`Access`), a database error, or a record whose value is not cached after the fetch
    /// (`Programming`, never substituted).
    ///
    /// ```ignore
    /// let rows = orders.read_rows(&mut tx, &["code", "amount_total"], Load::Classic).await?;
    /// for i in rows.indices() {
    ///     let total = rows.f64(i, "amount_total")?;
    /// }
    /// ```
    pub async fn read_rows(
        &self,
        tx: &mut Transaction,
        fields: &[&str],
        load: Load,
    ) -> Result<erp_fields::rows::Rows> {
        let (_, level) = self.read_level(tx, fields, load).await?;
        level.finish()
    }

    /// One level of [`Records::read_rows`] before it is finished, with the records it holds (the
    /// existing ones, in order): `web_read` expands its relational keys in place.
    pub(crate) async fn read_level(
        &self,
        tx: &mut Transaction,
        fields: &[&str],
        load: Load,
    ) -> Result<(Records, Level)> {
        self.read_level_into(tx, fields, load, None).await
    }

    /// [`Records::read_level`] filling the emptied columns of `spare`, the builder of a recycled
    /// batch (a stream's previous window, design note `perf-stream.md` §3.6): the column at each
    /// layout position is taken out of it for the sink of that position. Finish the level with
    /// [`Level::finish_reusing`] and the same builder.
    pub(crate) async fn read_level_into(
        &self,
        tx: &mut Transaction,
        fields: &[&str],
        load: Load,
        mut spare: Option<&mut erp_fields::rows::RowsBuilder>,
    ) -> Result<(Records, Level)> {
        let registry = Arc::clone(self.env().registry());
        let spec = registry.model_by_id(self.model_id());
        // the policy generation and the subject's groups first: every field check below reads the
        // binding, so a membership or policy write of this transaction (or a fresh `with_user`) is
        // seen before the first mask is asked for
        if !self.env().su() {
            tx.prepare_access(self.env()).await?;
        }
        let names: Vec<&str> = if fields.is_empty() {
            let mask = tx.field_mask(self.env(), self.model_id())?;
            spec.fields
                .iter()
                .filter(|f| mask.readable(f.id))
                .map(|f| f.name)
                .collect()
        } else {
            fields.to_vec()
        };
        let mut probe = None;
        for name in &names {
            let field = field_of(spec, name)?;
            if probe.is_none() && field.has_column() {
                probe = Some(field.id);
            }
            if probe.is_none()
                && tx
                    .shared
                    .graph
                    .model(spec.id)
                    .is_builtin_display_name(field.id)
            {
                probe = spec.rec_name.filter(|f| spec.field_by_id(*f).has_column());
            }
        }
        if probe.is_none() {
            let graph = tx.shared.graph.model(spec.id);
            let mut visited = FxHashSet::default();
            for name in &names {
                if let Ok(field) = field_of(spec, name) {
                    let mut deps = Vec::new();
                    crate::search::resolve_stored_dependencies(
                        spec,
                        field.id,
                        Some(graph),
                        &mut visited,
                        &mut deps,
                    );
                    if let Some(&first) = deps.first() {
                        probe = Some(first);
                        break;
                    }
                }
            }
        }
        tx.check_field_names(
            self.env(),
            self.model_id(),
            names.iter().copied(),
            FieldOp::Read,
        )?;
        self.fetch(tx, Some(&names)).await?;
        let model = self.model_id();
        let present = match probe {
            Some(fid) => {
                let existing: Vec<RecId> = self
                    .rec_ids()
                    .iter()
                    .copied()
                    .filter(|id| id.is_new() || tx.cache.contains(model, fid, *id))
                    .collect();
                if existing.len() == self.len() {
                    self.clone()
                } else {
                    let prefetch: Vec<RecId> = self
                        .prefetch_ids()
                        .iter()
                        .copied()
                        .filter(|id| id.is_new() || tx.cache.contains(model, fid, *id))
                        .collect();
                    Records::from_parts(self.env().clone(), model, existing.into(), prefetch.into())
                }
            }
            None => self.clone(),
        };
        let id_spare = spare.as_deref_mut().and_then(|b| b.take_column(0));
        let mut level = Level::new(
            spec.name,
            Sink::ids_reusing(present.rec_ids().iter().map(|id| id.get()), id_spare),
        );
        let mut slots = Slots::default();
        // a name given twice reads once (a dict has one entry per key); `id` is the first key
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        seen.insert("id");
        for name in &names {
            let field = field_of(spec, name)?;
            if !seen.insert(field.name) {
                if matches!(field.ty, FieldType::Id) {
                    // `id` asked for explicitly still passes the field checks, as every read does
                    tx.field_read_checks(&present, field.id).await?;
                }
                continue;
            }
            let column_spare = spare
                .as_deref_mut()
                .and_then(|b| b.take_column(level.width()));
            let sink = tx
                .read_column(&present, field, load, &mut slots, column_spare)
                .await?;
            level.push(erp_fields::rows::Key::Field(field.name), sink)?;
        }
        Ok((present, level))
    }
}

/// The cache slots of one level's records, taken once per level and taken again
/// only when the cache's slot epoch moved in between, or when asked for other records.
#[derive(Debug, Default)]
pub(crate) struct Slots {
    /// What the slots were taken for: the cache epoch, the model, and the records' shared id
    /// list. The list is held (an `Arc` clone), so its address cannot be reused by another list
    /// while it is the key and its ids cannot change: the same `Arc` means the same ids, in any
    /// build, wherever a `Slots` is kept.
    taken: Option<(u64, ModelId, Ids)>,
    slots: Vec<u32>,
}

impl Slots {
    /// The slot of each of `ids` in `mc` (the cache of `model` at `epoch`), in order; taken
    /// again unless they were taken for the same id list of the same model under the same epoch.
    ///
    /// # Errors
    /// The error of `missing` for a record without a slot (its fields were never cached).
    fn refresh(
        &mut self,
        epoch: u64,
        model: ModelId,
        mc: &crate::cache::ModelCache,
        ids: &Ids,
        missing: impl Fn(RecId) -> Error,
    ) -> Result<&[u32]> {
        let fresh = match &self.taken {
            Some((e, m, held)) => *e == epoch && *m == model && Arc::ptr_eq(held, ids),
            None => false,
        };
        if !fresh {
            self.taken = None;
            self.slots.clear();
            self.slots.reserve(ids.len());
            for id in ids.iter() {
                self.slots
                    .push(mc.slot_of(*id).ok_or_else(|| missing(*id))?);
            }
            self.taken = Some((epoch, model, Arc::clone(ids)));
        }
        Ok(&self.slots)
    }
}

impl Transaction {
    /// The values of `field` for every record of `records`, in order, as one column of a result
    /// batch: the same access steps and values as [`Self::field_cells`] and `read_value`, gathered
    /// from the cache column by slot for the fields read from their column (one slot lookup per
    /// record per level, not per field). The sink fills the emptied buffers of `spare` (a column of a recycled batch) when they fit
    /// the field ([`Sink::for_field_reusing`]).
    pub(crate) async fn read_column(
        &mut self,
        records: &Records,
        field: &FieldSpec,
        load: Load,
        slots: &mut Slots,
        spare: Option<erp_fields::rows::Column>,
    ) -> Result<Sink> {
        let fid = field.id;
        let n = records.len();
        let mut sink = Sink::for_field_reusing(&field.ty, load, n, spare);
        self.field_read_checks(records, fid).await?;
        if matches!(field.ty, FieldType::Many2one { .. }) && load == Load::Classic {
            let cells = self.field_cells_checked(records, fid).await?;
            for label in self.many2one_labels(records, fid, &cells).await? {
                sink.push_label(label)?;
            }
            return Ok(sink);
        }
        if self.reads_by_own_path(records.model_id(), field) {
            for cell in self.field_cells_checked(records, fid).await? {
                sink.push_cell(cell)?;
            }
            return Ok(sink);
        }
        self.ensure(records, fid).await?;
        let model = records.model_id();
        if !matches!(
            self.slot(records.env(), model, fid)?,
            crate::engine::Slot::Plain
        ) {
            // a computed field that depends on the context is cached per context key
            if records.spec().field_by_id(fid).translate {
                self.active_languages().await?;
            }
            for cell in self.cells_of(records.env(), model, fid, records.rec_ids())? {
                sink.push_cell(cell)?;
            }
            return Ok(sink);
        }
        let epoch = self.cache.epoch();
        let Some(mc) = self.cache.model(model) else {
            return match records.rec_ids().first() {
                None => Ok(sink),
                Some(id) => Err(not_cached(records, *id, field)),
            };
        };
        let taken = slots.refresh(epoch, model, mc, records.shared_ids(), |id| {
            not_cached(records, id, field)
        })?;
        for (id, slot) in records.rec_ids().iter().zip(taken) {
            let cell = mc
                .get_at(fid, *slot)
                .ok_or_else(|| not_cached(records, *id, field))?;
            sink.push_cell(cell)?;
        }
        Ok(sink)
    }
}

/// `Programming`: `bench.order.line(7,).qty has no cached value after fetch` (
/// the read path never substitutes `False` for it).
fn not_cached(records: &Records, id: RecId, field: &FieldSpec) -> Error {
    Error::programming(format!(
        "{}({id},).{} has no cached value after fetch",
        records.model_name(),
        field.name
    ))
}

#[cfg(test)]
mod slots_tests {
    use erp_fields::{Cell, Storage};
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

    use super::*;
    use crate::cache::Cache;

    static T: AddonDescriptor = AddonDescriptor {
        name: "t",
        depends: &["base"],
        exact_only: false,
    };
    static M: ModelDecl =
        ModelDecl::new("t", "t.m").fields(const { &[FieldDecl::integer("number")] });

    fn id(i: i64) -> RecId {
        RecId::from_real(i).unwrap()
    }

    fn missing(id: RecId) -> Error {
        Error::programming(format!("t.m({id},) has no slot"))
    }

    /// A level's slots are taken again after `restore` (the cache epoch moved), never read
    /// stale: the restored cache numbers the same record differently.
    #[test]
    fn slots_are_taken_again_after_a_restore() {
        let registry = RegistryBuilder::new([&T]).with_decls([&M]).build().unwrap();
        let spec = registry.model("t.m").unwrap();
        let (model, number) = (spec.id, spec.field("number").unwrap().id);
        let mut cache = Cache::default();
        let snapshot = cache.snapshot();
        for i in [7_i32, 8] {
            cache
                .model_mut(model)
                .insert_fetched(number, Storage::Int, id(i64::from(i)), Cell::Int(i))
                .unwrap();
        }
        let ids: Ids = Arc::from([id(8)]);
        let mut slots = Slots::default();
        let epoch = cache.epoch();
        let taken = slots
            .refresh(epoch, model, cache.model(model).unwrap(), &ids, missing)
            .unwrap()
            .to_vec();
        assert_eq!(taken, [1]);
        // another recordset of the same length under the same epoch takes its own slots
        let other: Ids = Arc::from([id(7)]);
        let other_slots = slots
            .refresh(epoch, model, cache.model(model).unwrap(), &other, missing)
            .unwrap()
            .to_vec();
        assert_eq!(other_slots, [0]);
        // back to the first list: its own slots again
        let again = slots
            .refresh(epoch, model, cache.model(model).unwrap(), &ids, missing)
            .unwrap()
            .to_vec();
        assert_eq!(again, taken);
        // back to before the inserts, then only 8 cached: it now has slot 0
        cache.restore(snapshot).unwrap();
        assert_ne!(cache.epoch(), epoch);
        cache
            .model_mut(model)
            .insert_fetched(number, Storage::Int, id(8), Cell::Int(8))
            .unwrap();
        let mc = cache.model(model).unwrap();
        let retaken = slots
            .refresh(cache.epoch(), model, mc, &ids, missing)
            .unwrap()
            .to_vec();
        assert_eq!(retaken, [0]);
        assert_eq!(mc.get_at(number, retaken[0]), Some(Cell::Int(8)));
        // a record the cache never saw is an error, not a slot
        let err = slots
            .refresh(cache.epoch(), model, mc, &Arc::from([id(7)]), missing)
            .unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert_eq!(err.message(), "t.m(7,) has no slot");
    }
}
