//! Guest run time: what the code the macros generate for a component calls.
//!
//! The method functions of an addon are the same source as in a native build. Here their records,
//! transaction and `super()` are backed by the host: each operation is one `orm-host` import, made
//! as the user and company of the call (taken from the host's store, never from this crate).

use erp_fields::{
    Cell, Date, DateTime, Error, ErrorKind, FieldValue, IntoFieldValue, RecId, Result, serde_json,
};
use erp_guest::{
    BoundaryError, BoundaryErrorKind, CallContext, ColumnData, GuestMethodFn, RecordId,
    ScalarValue, host,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{
    Batch, ENV, Env, Field, Inherits, Model, RecordSet, Super, Transaction, TypedRelation, Vals,
};

/// A method this component runs, registered by `#[methods]` (`inventory` collects them).
pub struct GuestMethod {
    /// The addon that wrote it.
    pub addon: &'static str,
    /// `_name` of the model of its declaration.
    pub model: &'static str,
    /// Method name.
    pub method: &'static str,
    /// Runs it.
    pub run: GuestMethodFn,
}

inventory::collect!(GuestMethod);

/// The function registered by `addon` for `method` of `model`. A component runs the methods of its
/// own addon only; the ones of addons it was linked with belong to those addons' layers.
pub fn find_guest_method(
    addon: &str,
    model: &str,
    method: &str,
) -> std::result::Result<Option<GuestMethodFn>, BoundaryError> {
    let mut found = inventory::iter::<GuestMethod>()
        .filter(|m| m.addon == addon && m.model == model && m.method == method);
    let first = found.next().map(|m| m.run);
    if found.next().is_some() {
        return Err(boundary(
            BoundaryErrorKind::InternalError,
            format!("addon '{addon}' registers method '{method}' of '{model}' more than once"),
        ));
    }
    Ok(first)
}

fn boundary(kind: BoundaryErrorKind, message: impl Into<String>) -> BoundaryError {
    BoundaryError {
        kind,
        message: message.into(),
    }
}

/// A host answer as an ORM error, keeping the message and the closest kind.
pub fn from_boundary(error: BoundaryError) -> Error {
    let kind = match error.kind {
        BoundaryErrorKind::NotFound => ErrorKind::Missing,
        BoundaryErrorKind::AccessDenied => ErrorKind::Access,
        BoundaryErrorKind::ValidationFailed | BoundaryErrorKind::InvalidArgument => {
            ErrorKind::Validation
        }
        BoundaryErrorKind::ConcurrencyConflict => ErrorKind::Concurrency,
        BoundaryErrorKind::ResourceExhausted => ErrorKind::ResourceExhausted,
        BoundaryErrorKind::Unsupported => ErrorKind::Programming,
        BoundaryErrorKind::InternalError => ErrorKind::Internal,
    };
    Error::new(kind, error.message)
}

/// An ORM error as a boundary error for the host, keeping the message and the closest kind.
pub fn to_boundary(error: &Error) -> BoundaryError {
    let kind = match error.kind() {
        ErrorKind::Missing => BoundaryErrorKind::NotFound,
        ErrorKind::Access | ErrorKind::AccessDenied | ErrorKind::SessionExpired => {
            BoundaryErrorKind::AccessDenied
        }
        ErrorKind::UserError | ErrorKind::Validation => BoundaryErrorKind::ValidationFailed,
        ErrorKind::Lock | ErrorKind::Concurrency | ErrorKind::Conflict => {
            BoundaryErrorKind::ConcurrencyConflict
        }
        ErrorKind::ResourceExhausted => BoundaryErrorKind::ResourceExhausted,
        ErrorKind::Programming => BoundaryErrorKind::InvalidArgument,
        ErrorKind::Redirect | ErrorKind::Internal | ErrorKind::Database => {
            BoundaryErrorKind::InternalError
        }
    };
    boundary(kind, error.message().to_owned())
}

/// The entry a macro-generated registration runs: decode the arguments, run the future of the
/// method on the records, encode the result.
pub fn run_method<A, R, F, Fut>(
    ctx: &CallContext,
    ids: Vec<RecordId>,
    args_json: &str,
    method: &'static str,
    body: F,
) -> std::result::Result<String, BoundaryError>
where
    A: Serialize + DeserializeOwned,
    R: Serialize + DeserializeOwned,
    F: FnOnce(Vec<RecId>, Transaction, A, Super<A, R>) -> Fut,
    Fut: std::future::Future<Output = Result<R>>,
{
    let args: A = serde_json::from_str(args_json).map_err(|e| {
        boundary(
            BoundaryErrorKind::InvalidArgument,
            format!("arguments of '{method}' do not match its signature: {e}"),
        )
    })?;
    let rec_ids = ids
        .iter()
        .map(|id| {
            RecId::from_real(*id).ok_or_else(|| {
                boundary(
                    BoundaryErrorKind::InvalidArgument,
                    format!("'{method}' was called with the record id {id}"),
                )
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let tx = Transaction::new(ctx.clone());
    let sup = Super::new(method);
    let result = erp_guest::block_on(body(rec_ids, tx, args, sup))?.map_err(|e| to_boundary(&e))?;
    serde_json::to_string(&result).map_err(|e| {
        boundary(
            BoundaryErrorKind::InternalError,
            format!("result of '{method}' is not serialisable: {e}"),
        )
    })
}

impl Transaction {
    pub(super) fn new(ctx: CallContext) -> Self {
        Self { ctx }
    }

    /// The context the host gave this call (language, time zone, cache epoch). The user and the
    /// companies in it are informative: every operation runs under the host's own identity.
    pub fn call_context(&self) -> &CallContext {
        &self.ctx
    }

    /// The environment of the call, to reach the empty recordset of a model (`env().model::<M>()`).
    pub fn env(&self) -> Env {
        ENV
    }

    /// The typed `Domain` of the native surface is not available in a component yet.
    pub async fn search_json<M: Model>(
        &mut self,
        domain_json: &str,
        limit: Option<u32>,
        order: Option<&str>,
    ) -> Result<RecordSet<M>> {
        let ids =
            host::search(M::MODEL_NAME, domain_json, limit, None, order).map_err(from_boundary)?;
        let ids = ids
            .into_iter()
            .map(|id| {
                RecId::from_real(id)
                    .ok_or_else(|| Error::internal(format!("search answered the id {id}")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(RecordSet::__from_ids(ids))
    }
}

impl<M: Model> RecordSet<M> {
    /// The set itself when it holds exactly one record; the singleton error otherwise.
    pub fn ensure_one(&self) -> Result<&Self> {
        match self.ids.len() {
            1 => Ok(self),
            n => Err(Error::new(
                ErrorKind::Validation,
                format!("expected singleton: {} ({n} records)", M::MODEL_NAME),
            )),
        }
    }

    /// The environment of the call; see [`Transaction::env`].
    pub fn env(&self) -> &Env {
        &ENV
    }

    /// Singleton recordsets in order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = Self> + ExactSizeIterator + '_ {
        self.ids.iter().map(|id| Self::__from_ids(vec![*id]))
    }

    /// The ids, in order (the native `Records::rec_ids`).
    pub fn rec_ids(&self) -> &[RecId] {
        &self.ids
    }

    /// Typed values for `create`/`write` on this model: only field handles of `M` and of the
    /// models it inherits are accepted.
    pub fn vals(&self) -> Vals<M> {
        Vals::new()
    }

    /// `browse(ids)`: a recordset of these database ids, order and duplicates kept. Ids must be
    /// positive.
    pub fn browse(&self, ids: impl IntoIterator<Item = i64>) -> Result<Self> {
        let ids = ids
            .into_iter()
            .map(|i| match i {
                0 => Err(Error::programming("Invalid falsy real id")),
                i => RecId::from_real(i).ok_or_else(|| {
                    Error::programming(format!("Invalid real id {i}: database ids are positive"))
                }),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self::__from_ids(ids))
    }

    fn raw_ids(&self) -> Vec<RecordId> {
        self.ids.iter().map(|id| id.get()).collect()
    }

    /// A field value of a singleton (the type's null value on an empty set; the singleton error on
    /// several records). One `read-columns` call.
    pub async fn get<P: Model, T: FieldValue>(
        &self,
        tx: &mut Transaction,
        field: Field<P, T>,
    ) -> Result<T>
    where
        M: Inherits<P>,
    {
        if self.ids.is_empty() {
            return T::from_cell(&Cell::Null);
        }
        self.ensure_one()?;
        let fields = [field.name().to_owned()];
        let store =
            host::read(&tx.ctx, M::MODEL_NAME, &self.raw_ids(), &fields).map_err(from_boundary)?;
        let column = store.column(field.name()).map_err(from_boundary)?;
        T::from_cell(&cell_at(&column.data, 0)?)
    }

    /// One value of a scalar field per record, in order: one `read-columns` call on the whole set.
    pub async fn mapped<P: Model, T: FieldValue>(
        &self,
        tx: &mut Transaction,
        field: Field<P, T>,
    ) -> Result<Vec<T>>
    where
        M: Inherits<P>,
    {
        if self.ids.is_empty() {
            return Ok(Vec::new());
        }
        let fields = [field.name().to_owned()];
        let store =
            host::read(&tx.ctx, M::MODEL_NAME, &self.raw_ids(), &fields).map_err(from_boundary)?;
        let column = store.column(field.name()).map_err(from_boundary)?;
        (0..self.ids.len())
            .map(|i| T::from_cell(&cell_at(&column.data, i)?))
            .collect()
    }

    /// Assign one field on every record: one `write-batch` call on the whole set.
    pub async fn set<P: Model, T: FieldValue, V: IntoFieldValue<T>>(
        &self,
        _tx: &mut Transaction,
        field: Field<P, T>,
        value: V,
    ) -> Result<()>
    where
        M: Inherits<P>,
    {
        if field.name() == "id" {
            return Err(Error::programming("field 'id' cannot be assigned"));
        }
        if self.ids.is_empty() {
            return Ok(());
        }
        let cell = value.into_field_value().into_cell();
        let values = [(field.name().to_owned(), scalar_of(cell)?)];
        host::write(M::MODEL_NAME, &self.raw_ids(), &values).map_err(from_boundary)
    }
}

impl<M: Model> RecordSet<M> {
    /// `create(vals_list)` through the model's method chain (every addon's `create` override
    /// runs, as in a native `create`): one `create-batch` call for the whole list. Needs the
    /// `orm:create` capability; the host validates every value against its field.
    pub async fn create(&self, _tx: &mut Transaction, vals_list: Vec<Vals<M>>) -> Result<Self> {
        if vals_list.is_empty() {
            return Ok(Self::__from_ids(Vec::new()));
        }
        let rows = vals_list
            .into_iter()
            .map(|vals| rows_of(&vals.into_raw()))
            .collect::<Result<Vec<_>>>()?;
        let ids = host::create(M::MODEL_NAME, &rows).map_err(from_boundary)?;
        let ids = ids
            .into_iter()
            .map(|id| {
                RecId::from_real(id)
                    .ok_or_else(|| Error::internal(format!("create answered the id {id}")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self::__from_ids(ids))
    }

    /// `write(vals)` through the method chain: every value of `vals` on every record, in one
    /// `write-batch` call. Needs the `orm:write` capability. Nothing happens on an empty set or
    /// with no value.
    pub async fn write(&self, _tx: &mut Transaction, vals: Vals<M>) -> Result<()> {
        let raw = vals.into_raw();
        if raw.contains("id") {
            return Err(Error::programming("field 'id' cannot be assigned"));
        }
        if self.ids.is_empty() || raw.is_empty() {
            return Ok(());
        }
        let values = rows_of(&raw)?;
        host::write(M::MODEL_NAME, &self.raw_ids(), &values).map_err(from_boundary)
    }

    /// The records reached through a typed relational field (`loan.book_id`, `book.loan_ids`) as a
    /// recordset of the comodel, in order and without duplicates. The comodel the handle names was
    /// checked against the registry when the addon was installed (`model_ref!`, `#[model]`).
    pub async fn related<P: Model, T: TypedRelation>(
        &self,
        tx: &mut Transaction,
        field: Field<P, T>,
    ) -> Result<RecordSet<T::Comodel>>
    where
        M: Inherits<P>,
    {
        // not a HashSet: its random seed is a WASI import this host does not provide
        let mut seen = std::collections::BTreeSet::new();
        let mut ids = Vec::new();
        for value in self.mapped(tx, field).await? {
            for id in value.__ids() {
                if seen.insert(id) {
                    ids.push(id);
                }
            }
        }
        Ok(RecordSet::__from_ids(ids))
    }
}

/// The values of a `create` or `write` as the host takes them.
fn rows_of(raw: &super::RawVals) -> Result<Vec<(String, ScalarValue)>> {
    raw.iter()
        .map(|(name, cell)| Ok((name.to_owned(), scalar_of(cell.clone())?)))
        .collect()
}

impl<A: Serialize, R: DeserializeOwned> Super<A, R> {
    pub(super) fn new(method: &'static str) -> Self {
        Self {
            method,
            _p: std::marker::PhantomData,
        }
    }

    /// Call the next implementation below this addon in the chain, on `records`. The host knows
    /// which layer is calling and runs the one below it, native or WebAssembly.
    pub async fn call<M: Model>(
        self,
        _tx: &mut Transaction,
        records: &RecordSet<M>,
        args: A,
    ) -> Result<R> {
        let args_json = serde_json::to_string(&args).map_err(|e| {
            Error::programming(format!(
                "arguments of super() of '{}' are not serialisable: {e}",
                self.method
            ))
        })?;
        let ids: Vec<RecordId> = records.ids.iter().map(|id| id.get()).collect();
        let answer = host::super_call(M::MODEL_NAME, self.method, &ids, &args_json)
            .map_err(from_boundary)?;
        serde_json::from_str(&answer).map_err(|e| {
            Error::validation(format!(
                "super() of '{}' answered a result of another type: {e}",
                self.method
            ))
        })
    }
}

/// Row `index` of a column as a cell. The kinds an addon reads today; the rest is named as
/// unsupported, never read as a default.
fn cell_at(data: &ColumnData, index: usize) -> Result<Cell> {
    let unsupported = |kind: &str| {
        Error::programming(format!(
            "reading a {kind} field in a component is not supported yet (a later release follow-up)"
        ))
    };
    let missing = || Error::internal("the host answered fewer values than records");
    Ok(match data {
        ColumnData::ColBool(v) => Cell::Bool(*v.get(index).ok_or_else(missing)?),
        ColumnData::ColOptBool(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Cell::Null, Cell::Bool),
        ColumnData::ColInt(v) => int_cell(*v.get(index).ok_or_else(missing)?),
        ColumnData::ColOptInt(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Cell::Null, int_cell),
        ColumnData::ColFloat(v) => Cell::Float(*v.get(index).ok_or_else(missing)?),
        ColumnData::ColOptFloat(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Cell::Null, Cell::Float),
        ColumnData::ColString(v) => Cell::Str(v.get(index).ok_or_else(missing)?.as_str().into()),
        ColumnData::ColOptString(v) => v
            .get(index)
            .ok_or_else(missing)?
            .as_deref()
            .map_or(Cell::Null, |s| Cell::Str(s.into())),
        ColumnData::ColOptId(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Cell::Null, Cell::Id),
        ColumnData::ColIds(v) => {
            let ids = v.get(index).ok_or_else(missing)?;
            let ids = ids
                .iter()
                .map(|id| {
                    RecId::from_real(*id)
                        .ok_or_else(|| Error::internal(format!("the host answered the id {id}")))
                })
                .collect::<Result<Vec<_>>>()?;
            Cell::Ids(ids.into())
        }
        ColumnData::ColOptDecimal(_) => return Err(unsupported("decimal")),
        ColumnData::ColOptDate(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Cell::Null, |days| Cell::Date(Date::from_days(days))),
        ColumnData::ColOptDatetime(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Cell::Null, |micros| {
                Cell::DateTime(DateTime::from_micros(micros))
            }),
        ColumnData::ColOptDateRange(_) | ColumnData::ColOptDatetimeRange(_) => {
            return Err(unsupported("range"));
        }
        ColumnData::ColOptJson(_) => return Err(unsupported("json")),
        ColumnData::ColOptBytes(_) => return Err(unsupported("binary")),
    })
}

fn int_cell(value: i64) -> Cell {
    i32::try_from(value).map_or(Cell::Id(value), Cell::Int)
}

fn scalar_of(cell: Cell) -> Result<ScalarValue> {
    Ok(match cell {
        Cell::Null => ScalarValue::ValNull,
        Cell::Bool(b) => ScalarValue::ValBool(b),
        Cell::Int(i) => ScalarValue::ValInt(i64::from(i)),
        Cell::Id(id) => ScalarValue::ValId(id),
        Cell::Float(f) => ScalarValue::ValFloat(f),
        Cell::Date(d) => ScalarValue::ValDate(d.days()),
        Cell::DateTime(d) => ScalarValue::ValDatetime(d.micros()),
        Cell::Uuid(_) | Cell::Xid8(_) | Cell::TimestampTz(_) => {
            return Err(Error::programming(
                "native scalar values are not supported across frozen WASM ABI1; requires a future ABI major with typed native fields",
            ));
        }
        Cell::Str(s) => ScalarValue::ValStr(s.to_string()),
        Cell::Ids(ids) => ScalarValue::ValIds(ids.iter().map(|id| id.get()).collect()),
        other => {
            return Err(Error::programming(format!(
                "writing a {other:?} value in a component is not supported yet \
                 (a later release follow-up)"
            )));
        }
    })
}

/// A typed call of `method` on `records` through the host's override chain (`call-method`): the
/// arguments go as the JSON of the argument tuple, the answer is parsed as `R`. This is what the
/// `<Model>Methods` traits of a component call.
pub async fn call_by_name<M: Model, A: Serialize, R: DeserializeOwned>(
    _tx: &mut Transaction,
    records: &RecordSet<M>,
    method: &'static str,
    args: A,
) -> Result<R> {
    let args_json = serde_json::to_string(&args).map_err(|e| {
        Error::programming(format!("arguments of '{method}' are not serialisable: {e}"))
    })?;
    let ids: Vec<RecordId> = records.ids.iter().map(|id| id.get()).collect();
    let answer =
        host::call_method(M::MODEL_NAME, method, &ids, &args_json).map_err(from_boundary)?;
    serde_json::from_str(&answer).map_err(|e| {
        Error::validation(format!(
            "'{method}' answered a result of another type than the signature says: {e}"
        ))
    })
}

/// A cell as the JSON the host takes: `null` for an empty value, otherwise the field's JSON form
/// (the host converts it with the type of the field it is written to).
pub(super) fn cell_json(cell: &Cell) -> Result<serde_json::Value> {
    match cell {
        Cell::Null => Ok(serde_json::Value::Null),
        Cell::Uuid(_) | Cell::Xid8(_) | Cell::TimestampTz(_) => Err(Error::programming(
            "native scalar values are not supported across frozen WASM ABI1; requires a future ABI major with typed native fields",
        )),
        other => erp_fields::convert::value_to_json(&erp_fields::Value::from_cell(other)),
    }
}

/// A JSON value from the host as a cell: scalars by their JSON type, anything structured (an
/// x2many command list, a JSON document) as `Cell::Json`, which `cell_json` sends back unchanged.
pub(super) fn json_cell(json: serde_json::Value) -> Cell {
    match json {
        serde_json::Value::Null => Cell::Null,
        serde_json::Value::Bool(b) => Cell::Bool(b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => int_cell(i),
            None => n.as_f64().map_or_else(
                || Cell::Json(std::sync::Arc::new(serde_json::Value::Number(n))),
                Cell::Float,
            ),
        },
        serde_json::Value::String(s) => Cell::Str(s.as_str().into()),
        other => Cell::Json(std::sync::Arc::new(other)),
    }
}

/// What the host hands a compute: the ids and the input columns, as JSON.
#[derive(serde::Deserialize)]
struct ComputeInput {
    ids: Vec<i64>,
    columns: std::collections::BTreeMap<String, Vec<serde_json::Value>>,
}

/// The entry a macro-generated registration of a `#[compute]` runs. The host sends the inputs of
/// the batch as data (`{"ids": [...], "columns": {field: [value, ...]}}`, the single argument), so
/// the function needs no host call and also works on new records; the answer is the assigned
/// columns, field name to one value per record.
pub fn run_compute<M, F>(
    method: &'static str,
    args_json: &str,
    body: F,
) -> std::result::Result<String, BoundaryError>
where
    M: Model,
    F: FnOnce(&mut Batch<'_, M>) -> Result<()>,
{
    let (input,): (ComputeInput,) = serde_json::from_str(args_json).map_err(|e| {
        boundary(
            BoundaryErrorKind::InvalidArgument,
            format!("inputs of compute '{method}' do not match its wire form: {e}"),
        )
    })?;
    let mut ids = Vec::with_capacity(input.ids.len());
    for id in &input.ids {
        ids.push(RecId::from_raw(*id).ok_or_else(|| {
            boundary(
                BoundaryErrorKind::InvalidArgument,
                format!("compute '{method}' was given the record id {id}"),
            )
        })?);
    }
    let mut columns = std::collections::BTreeMap::new();
    for (name, values) in input.columns {
        if values.len() != ids.len() {
            return Err(boundary(
                BoundaryErrorKind::InvalidArgument,
                format!(
                    "compute '{method}': column '{name}' has {} values for {} records",
                    values.len(),
                    ids.len()
                ),
            ));
        }
        columns.insert(name, values.into_iter().map(json_cell).collect::<Vec<_>>());
    }
    let mut batch = Batch::<M>::__new(method, ids, columns);
    body(&mut batch).map_err(|e| to_boundary(&e))?;
    let outputs = batch.__into_outputs().map_err(|e| to_boundary(&e))?;
    let mut answer = serde_json::Map::new();
    for (name, cells) in outputs {
        let values = cells
            .iter()
            .map(cell_json)
            .collect::<Result<Vec<_>>>()
            .map_err(|e| to_boundary(&e))?;
        answer.insert(name, serde_json::Value::Array(values));
    }
    serde_json::to_string(&answer).map_err(|e| {
        boundary(
            BoundaryErrorKind::InternalError,
            format!("result of compute '{method}' is not serialisable: {e}"),
        )
    })
}

impl<M: Model> RecordSet<M> {
    /// `fields.Date.context_today(record)`: today's date in the time zone of the call's context
    /// (the host resolves it from the recordset's environment: the context `tz`, else UTC). The
    /// clock is the host's `wasi:clocks/wall-clock`. Same name and signature as the native
    /// `RecordSet::context_today`, which reads the zone from the recordset's own environment.
    ///
    /// # Errors
    ///
    /// `Validation` when the call context carries a zone the clock database does not know.
    pub fn context_today(&self, tx: &Transaction) -> Result<Date> {
        Date::context_today(&tx.call_context().tz)
    }
}
