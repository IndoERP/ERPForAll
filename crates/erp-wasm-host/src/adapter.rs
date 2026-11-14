//! The ORM behind a guest's `orm-host` imports.
//!
//! Every operation goes through the ORM's own access-checked entry points on the running
//! transaction: `search_with`, `cells`, `create_values`, `write_values`, `unlink` and the method
//! chain. Record rules, access rights and field groups apply exactly as for the same call from
//! native code or over RPC; this module builds no SQL and elevates nothing. The environment is the
//! one of the records the running method was invoked on (`records.env()`: user, company, language,
//! superuser mode, context), not the transaction's opening environment, so a `with_user` or `sudo`
//! caller keeps its identity through the guest. [`execute`] refuses a request whose bridge identity
//! differs from the one this activation was started for.
//!
//! Supported value kinds (a kind outside them is a typed `unsupported` error that names this
//! module's follow-up, never a substituted value): boolean, integer, float, decimal, char, text,
//! html, selection, many2one, one2many, many2many, JSON and binary on the way in; the first
//! nine (without JSON and binary), plus dates and datetimes on both paths. Ranges remain a follow-up.
//! `write-columns` writes one record at a time through the ORM (correct and access-checked, not yet
//! set-based); a failure after the first record poisons the transaction, since earlier records are
//! already written.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use erp_domain::Domain;
use erp_fields::{Cell, Date, DateTime, Decimal, Error, ErrorKind, FieldType, Value};
use erp_orm::rpc::Exposure;
use erp_orm::{DynSuper, Env, Operation, Records, SearchOptions, Transaction};

use crate::bindings::erpforall::addon::types::{
    BoundaryError, BoundaryErrorKind, ColumnData, ColumnVector, RecordBatch, ScalarValue,
    X2manyCommand,
};
use crate::bridge::{Caller, boundary_error};

// Field metadata is shared by every row. Retain first-seen order while charging
// each distinct name once; value decoding still sees every original row.
fn create_field_names(values: &[Vec<(String, ScalarValue)>]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut names = Vec::new();
    for row in values {
        for (name, _) in row {
            if seen.insert(name.as_str()) {
                names.push(name.clone());
            }
        }
    }
    names
}

/// One guest operation waiting for the transaction.
pub(crate) enum Request {
    Search {
        model: String,
        domain_json: String,
        limit: Option<u32>,
        offset: Option<u32>,
        order: Option<String>,
    },
    Read {
        model: String,
        ids: Vec<i64>,
        fields: Vec<String>,
    },
    Create {
        model: String,
        values: Vec<Vec<(String, ScalarValue)>>,
    },
    Write {
        model: String,
        ids: Vec<i64>,
        values: Vec<(String, ScalarValue)>,
    },
    WriteColumns {
        model: String,
        ids: Vec<i64>,
        columns: Vec<ColumnVector>,
    },
    Unlink {
        model: String,
        ids: Vec<i64>,
    },
    Call {
        model: String,
        method: String,
        ids: Vec<i64>,
        args_json: String,
    },
    Super {
        model: String,
        method: String,
        ids: Vec<i64>,
        args_json: String,
    },
}

/// The answer to a [`Request`].
pub(crate) enum Reply {
    Ids(Vec<i64>),
    Batch(RecordBatch),
    Unit,
    Text(String),
}

/// The part a method plays, which decides what its guest may ask of the host
/// (security review r1 H1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodRole {
    /// A method or an override: the bridge as approved, bounded by the invoking environment.
    Method,
    /// A compute of a package: its inputs arrive as data, so it may ask for nothing.
    Compute,
    /// A constraint of a package: it may read, nothing else.
    Constraint,
}

/// What a guest activation may ask the host for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    /// Everything the bridge approved.
    Full,
    /// `search` and `read-columns` only.
    ReadOnly,
    /// `super-call` only: the environment is elevated (`sudo` or the System principal), which the
    /// guest did not ask for and must not be able to spend on its own requests.
    SuperOnly,
    /// Nothing.
    Nothing,
}

impl Policy {
    /// The policy of a `role` running in `env`: the stricter of what the role and the environment
    /// allow.
    pub(crate) fn of(role: MethodRole, env: &Env) -> Self {
        let elevated = env.su() || env.principal().kind() == erp_orm::PrincipalKind::System;
        match (role, elevated) {
            (MethodRole::Compute, _) => Self::Nothing,
            // a constraint has no layer below it, so `super-call` is of no use to it either
            (MethodRole::Constraint, true) => Self::Nothing,
            (MethodRole::Constraint, false) => Self::ReadOnly,
            (MethodRole::Method, true) => Self::SuperOnly,
            (MethodRole::Method, false) => Self::Full,
        }
    }

    fn allows(self, request: &Request) -> bool {
        match self {
            Self::Full => true,
            Self::ReadOnly => matches!(request, Request::Search { .. } | Request::Read { .. }),
            Self::SuperOnly => matches!(request, Request::Super { .. }),
            Self::Nothing => false,
        }
    }
}

fn request_name(request: &Request) -> &'static str {
    match request {
        Request::Search { .. } => "search",
        Request::Read { .. } => "read-columns",
        Request::Create { .. } => "create-batch",
        Request::Write { .. } => "write-batch",
        Request::WriteColumns { .. } => "write-columns",
        Request::Unlink { .. } => "unlink-batch",
        Request::Call { .. } => "call-method",
        Request::Super { .. } => "super-call",
    }
}

/// What the running guest call is: the method whose `super()` a guest may ask for, the records it
/// was invoked on, and the environment those records carry.
pub(crate) struct Running {
    pub(crate) method: &'static str,
    pub(crate) sup: DynSuper,
    /// The environment of the invoking records: every request of this activation runs in it.
    pub(crate) env: Env,
    /// The identity the bridge of this activation was bound to.
    pub(crate) caller: Caller,
    /// The ids the method was invoked on; `super-call` may use these and no others.
    pub(crate) ids: BTreeSet<i64>,
    /// `(model, method)` pairs the addon itself defines (`method` contributions): callable by
    /// name although the registry does not expose them over RPC.
    pub(crate) own: Arc<HashSet<(String, String)>>,
    /// What this activation may ask for.
    pub(crate) policy: Policy,
    /// The ids the activation's `super-call`s of `create` returned, in order (security review r1
    /// H3): a `create` override must answer exactly these.
    pub(crate) created: std::sync::Mutex<Vec<i64>>,
}

/// What a guest is told for an error that is not its business: constraint, table and column names
/// of the database or the host's own failure text stay in the host log.
const GENERIC_INTERNAL: &str = "internal error; see the server log";

/// An ORM error as the boundary error the guest sees: the closest kind and, except for database
/// and internal failures, the same message. Those two carry names a guest has no use for, so the
/// guest gets a generic text and the full error goes to the host log.
pub(crate) fn boundary_of(error: &Error) -> BoundaryError {
    if erp_orm::is_wasm_abi1_native_refusal(error) {
        return boundary_error(BoundaryErrorKind::Unsupported, error.message().to_owned());
    }
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
    let message = match error.kind() {
        ErrorKind::Database | ErrorKind::Internal | ErrorKind::Redirect => {
            tracing::error!(
                target: "erp::wasm_guest",
                kind = ?error.kind(),
                request_id = erp_orm::trace::current_request_id().as_deref(),
                "ORM failure hidden from a guest: {}",
                error.message()
            );
            GENERIC_INTERNAL.to_owned()
        }
        _ => error.message().to_owned(),
    };
    boundary_error(kind, message)
}

/// A guest answer as an ORM error, keeping the closest kind and the message.
pub(crate) fn error_of(error: &BoundaryError) -> Error {
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
    Error::new(kind, error.message.clone())
}

type Outcome<T> = Result<T, BoundaryError>;

fn orm<T>(result: erp_fields::Result<T>) -> Outcome<T> {
    result.map_err(|e| boundary_of(&e))
}

fn unsupported(what: &str) -> BoundaryError {
    boundary_error(
        BoundaryErrorKind::Unsupported,
        format!("{what} is not supported across the WASM boundary yet (a later release)"),
    )
}

/// The bridge identity of an activation: the user of `env` and its active companies as the
/// transaction resolved them (`Transaction::companies`, which validates them against the
/// subject's memberships); without any company, [`Caller::without_company`].
pub(crate) async fn caller_of(tx: &mut Transaction, env: &Env) -> erp_fields::Result<Caller> {
    let allowed: Vec<i64> = tx.companies(env).await?.iter().map(|c| c.get()).collect();
    // An environment with no company at all (the System environment of a module operation, a
    // data load) runs the guest with no company: where native code of that environment runs, and
    // nothing broader. A company is never invented for it.
    let Some(company) = allowed.first().copied() else {
        // only the plain System principal has no company authority; a user whose environment
        // names no company stays refused, as before
        if env.principal().kind() == erp_orm::PrincipalKind::System {
            return Caller::without_company(env.uid()).map_err(|e| Error::access(e.to_string()));
        }
        return Err(Error::access(
            "a WASM addon needs a company on the calling environment",
        ));
    };
    Caller::new(env.uid(), company, allowed).map_err(|e| Error::access(e.to_string()))
}

/// Runs `request` on the transaction in the environment of the running activation, for the bridge
/// identity `caller`. A request whose identity is not the activation's is refused.
pub(crate) async fn execute(
    tx: &mut Transaction,
    caller: &Caller,
    running: &Running,
    request: Request,
) -> Outcome<Reply> {
    if *caller != running.caller || running.env.uid() != caller.user_id() {
        return Err(boundary_error(
            BoundaryErrorKind::AccessDenied,
            "the request carries another identity than the call it belongs to",
        ));
    }
    if !running.policy.allows(&request) {
        return Err(boundary_error(
            BoundaryErrorKind::AccessDenied,
            format!(
                "`{}` is refused in this call: {}",
                request_name(&request),
                match running.policy {
                    Policy::Nothing =>
                        "a compute may not access the host; a constraint in a sudo or System environment may not access the host",
                    Policy::ReadOnly => "a constraint may only read",
                    Policy::SuperOnly =>
                        "the environment is elevated (sudo or System), where a guest may only continue its chain",
                    Policy::Full => "not allowed",
                }
            ),
        ));
    }
    let env = &running.env;
    match request {
        Request::Search {
            model,
            domain_json,
            limit,
            offset,
            order,
        } => {
            let records = orm(env.records(&model))?;
            let json: serde_json::Value = serde_json::from_str(&domain_json).map_err(|e| {
                boundary_error(
                    BoundaryErrorKind::InvalidArgument,
                    format!("the domain is not valid JSON: {e}"),
                )
            })?;
            let domain = Domain::from_json(&json).map_err(|e| {
                boundary_error(
                    BoundaryErrorKind::InvalidArgument,
                    format!("invalid domain: {e}"),
                )
            })?;
            let mut options = SearchOptions::new();
            if let Some(limit) = limit {
                options = options.limit(u64::from(limit));
            }
            if let Some(offset) = offset {
                options = options.offset(u64::from(offset));
            }
            if let Some(order) = order {
                options = options.order(order);
            }
            let found = orm(records.search_with(tx, &domain, &options).await)?;
            Ok(Reply::Ids(found.ids()))
        }
        Request::Read { model, ids, fields } => {
            let records = orm(orm(env.records(&model))?.browse(ids.iter().copied()))?;
            // Unknown-field diagnostics must not precede the model/record ACL.
            orm(records.check_access(tx, Operation::Read).await)?;
            orm(erp_orm::wasm_abi1_check_fields(tx, &records, Operation::Read, &fields).await)?;
            let mut columns = Vec::with_capacity(fields.len());
            for field in &fields {
                let Some(spec) = records.spec().field(field) else {
                    return Err(boundary_error(
                        BoundaryErrorKind::NotFound,
                        format!("model '{model}' has no field '{field}'"),
                    ));
                };
                let data = if field == "id" {
                    // the ids are the caller's own; the model and record access checks still apply
                    orm(records.check_access(tx, Operation::Read).await)?;
                    ColumnData::ColInt(ids.clone())
                } else {
                    let cells = orm(records.cells(tx, field).await)?;
                    column_of(field, &spec.ty, &cells)?
                };
                columns.push(ColumnVector {
                    field: field.clone(),
                    data,
                });
            }
            Ok(Reply::Batch(RecordBatch {
                model,
                ids,
                columns,
            }))
        }
        Request::Create { model, values } => {
            let records = orm(env.records(&model))?;
            let names = create_field_names(&values);
            orm(erp_orm::wasm_abi1_check_fields(tx, &records, Operation::Create, &names).await)?;
            let rows = values
                .iter()
                .map(|row| values_of(row))
                .collect::<Outcome<Vec<_>>>()?;
            let created = orm(records.create_values(tx, &rows).await)?;
            Ok(Reply::Ids(created.ids()))
        }
        Request::Write { model, ids, values } => {
            let records = orm(orm(env.records(&model))?.browse(ids.iter().copied()))?;
            let names: Vec<String> = values.iter().map(|(name, _)| name.clone()).collect();
            orm(erp_orm::wasm_abi1_check_fields(tx, &records, Operation::Write, &names).await)?;
            let map = values_of(&values)?;
            orm(records.write_values(tx, map.iter()).await)?;
            Ok(Reply::Unit)
        }
        Request::WriteColumns {
            model,
            ids,
            columns,
        } => {
            let base = orm(env.records(&model))?;
            let names: Vec<String> = columns.iter().map(|column| column.field.clone()).collect();
            let targets = orm(base.browse(ids.iter().copied()))?;
            orm(erp_orm::wasm_abi1_check_fields(tx, &targets, Operation::Write, &names).await)?;
            // every row is converted before the first write, so a bad column never leaves a
            // partial write behind
            let mut rows = Vec::with_capacity(ids.len());
            for position in 0..ids.len() {
                let mut map = BTreeMap::new();
                for column in &columns {
                    map.insert(column.field.clone(), element_value(&column.data, position)?);
                }
                rows.push(map);
            }
            for (position, (id, map)) in ids.iter().zip(&rows).enumerate() {
                let one = orm(base.browse([*id]))?;
                if let Err(error) = one.write_values(tx, map.iter()).await {
                    if position > 0 {
                        // earlier records are written already: the transaction must not commit
                        tx.poison(&error);
                    }
                    return Err(boundary_of(&error));
                }
            }
            Ok(Reply::Unit)
        }
        Request::Unlink { model, ids } => {
            let records = orm(orm(env.records(&model))?.browse(ids.iter().copied()))?;
            orm(records.unlink(tx).await)?;
            Ok(Reply::Unit)
        }
        Request::Call {
            model,
            method,
            ids,
            args_json,
        } => {
            let records = orm(orm(env.records(&model))?.browse(ids.iter().copied()))?;
            // The registry decides what is callable on this model, whatever the bridge's list says:
            // a name exposed on another model, or one the guest declared elsewhere, does not make
            // this model's hidden or internal method callable.
            let exposed = matches!(
                tx.exposure().get(records.model_id(), &method),
                Some(Exposure::Exposed(_))
            );
            let own = running.own.contains(&(model.clone(), method.clone()));
            if !exposed && !own {
                return Err(boundary_error(
                    BoundaryErrorKind::AccessDenied,
                    format!("method `{method}` is not exposed on `{model}`"),
                ));
            }
            orm(erp_orm::wasm_abi1_check_call(tx, &records, &method, &args_json).await)?;
            let answer = orm(erp_orm::call_method_json(tx, &records, &method, &args_json).await)?;
            Ok(Reply::Text(answer))
        }
        Request::Super {
            model,
            method,
            ids,
            args_json,
        } => {
            // A guest can only continue the chain it runs in: `super()` of another method would
            // let an override reach an implementation it was never layered on.
            if method != running.method {
                return Err(boundary_error(
                    BoundaryErrorKind::AccessDenied,
                    format!(
                        "super-call of '{method}' from the running method '{}'",
                        running.method
                    ),
                ));
            }
            // ... and only on the records the running method was invoked on: a guest cannot widen
            // the set a lower layer sees.
            if let Some(foreign) = ids.iter().find(|id| !running.ids.contains(id)) {
                return Err(boundary_error(
                    BoundaryErrorKind::AccessDenied,
                    format!(
                        "super-call of '{method}' names record {foreign}, which the running call \
                         was not invoked on"
                    ),
                ));
            }
            let records = orm(orm(env.records(&model))?.browse(ids.iter().copied()))?;
            orm(erp_orm::wasm_abi1_check_call(tx, &records, &method, &args_json).await)?;
            let answer = orm(running
                .sup
                .clone()
                .call_json(tx, &records, &args_json)
                .await)?;
            if method == "create" {
                // the ids the core (or a layer below) created: the override's own answer is
                // checked against them when it returns
                let made: Vec<i64> = serde_json::from_str(&answer).map_err(|e| {
                    boundary_error(
                        BoundaryErrorKind::InternalError,
                        format!("super-call of create answered something but ids: {e}"),
                    )
                })?;
                running
                    .created
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(made);
            }
            Ok(Reply::Text(answer))
        }
    }
}

fn values_of(row: &[(String, ScalarValue)]) -> Outcome<BTreeMap<String, Value>> {
    let mut map = BTreeMap::new();
    for (name, value) in row {
        map.insert(name.clone(), value_of(value)?);
    }
    Ok(map)
}

fn value_of(value: &ScalarValue) -> Outcome<Value> {
    Ok(match value {
        ScalarValue::ValNull => Value::False,
        ScalarValue::ValBool(b) => Value::Bool(*b),
        ScalarValue::ValInt(i) => Value::Int(*i),
        ScalarValue::ValFloat(f) => Value::Float(*f),
        ScalarValue::ValDecimal(text) => Value::Decimal(text.parse::<Decimal>().map_err(|e| {
            boundary_error(
                BoundaryErrorKind::InvalidArgument,
                format!("'{text}' is not a decimal: {e}"),
            )
        })?),
        ScalarValue::ValStr(text) => Value::Str(Arc::from(text.as_str())),
        ScalarValue::ValId(id) => Value::Int(*id),
        ScalarValue::ValIds(ids) => Value::List(Arc::from(vec![command(6, 0, Some(ids))])),
        ScalarValue::ValX2many(commands) => Value::List(
            commands
                .iter()
                .map(|c| match c {
                    X2manyCommand::Link(id) => command(4, *id, None),
                    X2manyCommand::Unlink(id) => command(3, *id, None),
                    X2manyCommand::Delete(id) => command(2, *id, None),
                    X2manyCommand::Clear => command(5, 0, None),
                    X2manyCommand::Set(ids) => command(6, 0, Some(ids)),
                })
                .collect::<Vec<_>>()
                .into(),
        ),
        ScalarValue::ValJson(text) => {
            Value::Json(Arc::new(serde_json::from_str(text).map_err(|e| {
                boundary_error(
                    BoundaryErrorKind::InvalidArgument,
                    format!("not valid JSON: {e}"),
                )
            })?))
        }
        ScalarValue::ValBytes(bytes) => Value::Bytes(Arc::from(bytes.as_slice())),
        ScalarValue::ValDate(days) => Value::Date(Date::from_days(*days)),
        ScalarValue::ValDatetime(micros) => Value::DateTime(DateTime::from_micros(*micros)),
        ScalarValue::ValDateRange(_) | ScalarValue::ValDatetimeRange(_) => {
            return Err(unsupported("a range value"));
        }
    })
}

fn command(code: i64, id: i64, ids: Option<&Vec<i64>>) -> Value {
    let third = match ids {
        Some(ids) => Value::List(
            ids.iter()
                .map(|i| Value::Int(*i))
                .collect::<Vec<_>>()
                .into(),
        ),
        None => Value::Int(0),
    };
    Value::List(Arc::from(vec![Value::Int(code), Value::Int(id), third]))
}

/// Element `index` of a column as a dynamic value (for `write-columns`).
fn element_value(data: &ColumnData, index: usize) -> Outcome<Value> {
    let missing = || {
        boundary_error(
            BoundaryErrorKind::InvalidArgument,
            "a column holds fewer values than ids",
        )
    };
    Ok(match data {
        ColumnData::ColBool(v) => Value::Bool(*v.get(index).ok_or_else(missing)?),
        ColumnData::ColOptBool(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Value::False, Value::Bool),
        ColumnData::ColInt(v) => Value::Int(*v.get(index).ok_or_else(missing)?),
        ColumnData::ColOptInt(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Value::False, Value::Int),
        ColumnData::ColFloat(v) => Value::Float(*v.get(index).ok_or_else(missing)?),
        ColumnData::ColOptFloat(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Value::False, Value::Float),
        ColumnData::ColString(v) => {
            Value::Str(Arc::from(v.get(index).ok_or_else(missing)?.as_str()))
        }
        ColumnData::ColOptString(v) => match v.get(index).ok_or_else(missing)? {
            Some(text) => Value::Str(Arc::from(text.as_str())),
            None => Value::False,
        },
        ColumnData::ColOptId(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Value::False, Value::Int),
        ColumnData::ColIds(v) => {
            let ids = v.get(index).ok_or_else(missing)?;
            Value::List(Arc::from(vec![command(6, 0, Some(ids))]))
        }
        ColumnData::ColOptDecimal(v) => match v.get(index).ok_or_else(missing)? {
            Some(text) => value_of(&ScalarValue::ValDecimal(text.clone()))?,
            None => Value::False,
        },
        ColumnData::ColOptDate(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Value::False, |days| Value::Date(Date::from_days(days))),
        ColumnData::ColOptDatetime(v) => v
            .get(index)
            .ok_or_else(missing)?
            .map_or(Value::False, |micros| {
                Value::DateTime(DateTime::from_micros(micros))
            }),
        _ => return Err(unsupported("this column kind")),
    })
}

/// A column of `cells` of a field of type `ty`.
fn column_of(field: &str, ty: &FieldType, cells: &[Cell]) -> Outcome<ColumnData> {
    let wrong = |cell: &Cell| {
        boundary_error(
            BoundaryErrorKind::InternalError,
            format!("field '{field}' holds {cell:?}, which its type does not"),
        )
    };
    Ok(match ty {
        FieldType::BigInt | FieldType::Uuid | FieldType::Xid8 | FieldType::TimestampTz => {
            return Err(boundary_error(
                BoundaryErrorKind::Unsupported,
                format!(
                    "field '{field}' ({}) is not supported across frozen WASM ABI1; requires a future ABI major with typed native fields",
                    ty.type_name()
                ),
            ));
        }
        FieldType::Boolean => ColumnData::ColBool(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Bool(b) => Ok(*b),
                    Cell::Null => Ok(false),
                    other => Err(wrong(other)),
                })
                .collect::<Outcome<_>>()?,
        ),
        FieldType::Integer => ColumnData::ColOptInt(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Int(i) => Ok(Some(i64::from(*i))),
                    Cell::Null => Ok(None),
                    other => Err(wrong(other)),
                })
                .collect::<Outcome<_>>()?,
        ),
        FieldType::Float { digits: None } => ColumnData::ColOptFloat(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Float(f) => Ok(Some(*f)),
                    Cell::Null => Ok(None),
                    other => Err(wrong(other)),
                })
                .collect::<Outcome<_>>()?,
        ),
        FieldType::Float { digits: Some(_) } | FieldType::Monetary { .. } => {
            ColumnData::ColOptDecimal(
                cells
                    .iter()
                    .map(|c| match c {
                        Cell::Decimal(d) => Ok(Some(d.to_string())),
                        Cell::Null => Ok(None),
                        other => Err(wrong(other)),
                    })
                    .collect::<Outcome<_>>()?,
            )
        }
        FieldType::Char { .. }
        | FieldType::Text
        | FieldType::Html
        | FieldType::Selection { .. } => ColumnData::ColOptString(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Str(s) => Ok(Some(s.to_string())),
                    Cell::Null => Ok(None),
                    other => Err(wrong(other)),
                })
                .collect::<Outcome<_>>()?,
        ),
        FieldType::Date => ColumnData::ColOptDate(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Date(d) => Ok(Some(d.days())),
                    Cell::Null => Ok(None),
                    other => Err(wrong(other)),
                })
                .collect::<Outcome<_>>()?,
        ),
        FieldType::Datetime => ColumnData::ColOptDatetime(
            cells
                .iter()
                .map(|c| match c {
                    Cell::DateTime(d) => Ok(Some(d.micros())),
                    Cell::Null => Ok(None),
                    other => Err(wrong(other)),
                })
                .collect::<Outcome<_>>()?,
        ),
        FieldType::Many2one { .. } => ColumnData::ColOptId(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Id(id) => Ok(Some(*id)),
                    Cell::Null => Ok(None),
                    other => Err(wrong(other)),
                })
                .collect::<Outcome<_>>()?,
        ),
        FieldType::One2many { .. } | FieldType::Many2many { .. } => ColumnData::ColIds(
            cells
                .iter()
                .map(|c| match c {
                    Cell::Ids(ids) => Ok(ids.iter().map(|id| id.get()).collect()),
                    Cell::Null => Ok(Vec::new()),
                    other => Err(wrong(other)),
                })
                .collect::<Outcome<_>>()?,
        ),
        _ => {
            return Err(unsupported(&format!(
                "reading the field '{field}' of this type"
            )));
        }
    })
}

/// True when the records are all real (database) records; new records cannot cross the boundary.
pub(crate) fn check_real(records: &Records) -> erp_fields::Result<Vec<i64>> {
    let ids = records.rec_ids();
    if ids.iter().any(|id| id.is_new()) {
        return Err(Error::programming(
            "records that are not saved yet cannot be passed to a WASM addon",
        ));
    }
    Ok(ids.iter().map(|id| id.get()).collect())
}

/// The answer of a `create` override must be exactly what its `super-call`s created, in order: the
/// ids it returns are trusted by `_inherits`, `copy` and the x2many commands (security review r1
/// H3). The count must also match the number of input values.
///
/// # Errors
/// `Validation` naming the difference.
pub(crate) fn check_create_answer(
    running: &Running,
    answer: &str,
    input_count: usize,
) -> erp_fields::Result<()> {
    let given: Vec<i64> = serde_json::from_str(answer).map_err(|e| {
        Error::validation(format!(
            "a create override answered something that is not a list of record ids: {e}"
        ))
    })?;
    let made = running
        .created
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if given != *made {
        return Err(Error::validation(format!(
            "a create override answered the records {given:?}, but the create it continued made \
             {made:?}: it must answer exactly those, in that order"
        )));
    }
    if given.len() != input_count {
        return Err(Error::validation(format!(
            "a create override answered {} records for {input_count} input values",
            given.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    fn temporal_scalars_and_columns_round_trip_without_timezone_conversion() {
        use erp_fields::{Date, DateTime};

        for days in [i32::MIN, -1, 0, 19782, i32::MAX] {
            let date = Date::from_days(days);
            assert_eq!(
                value_of(&ScalarValue::ValDate(days)).unwrap(),
                Value::Date(date)
            );
            let col = column_of(
                "loan_date",
                &FieldType::Date,
                &[Cell::Date(date), Cell::Null],
            )
            .unwrap();
            assert!(matches!(&col, ColumnData::ColOptDate(v) if v == &[Some(days), None]));
            assert_eq!(element_value(&col, 0).unwrap(), Value::Date(date));
            assert_eq!(element_value(&col, 1).unwrap(), Value::False);
        }
        for micros in [i64::MIN, -1, 0, 1, 1_709_164_800_123_456, i64::MAX] {
            let dt = DateTime::from_micros(micros);
            assert_eq!(
                value_of(&ScalarValue::ValDatetime(micros)).unwrap(),
                Value::DateTime(dt)
            );
            let col = column_of(
                "returned_at",
                &FieldType::Datetime,
                &[Cell::DateTime(dt), Cell::Null],
            )
            .unwrap();
            assert!(matches!(&col, ColumnData::ColOptDatetime(v) if v == &[Some(micros), None]));
            assert_eq!(element_value(&col, 0).unwrap(), Value::DateTime(dt));
            assert_eq!(element_value(&col, 1).unwrap(), Value::False);
        }
    }

    #[test]
    #[allow(clippy::unwrap_used, reason = "test assertions")]
    fn temporal_columns_refuse_wrong_cells_and_missing_elements() {
        for ty in [FieldType::Date, FieldType::Datetime] {
            let err = column_of("when", &ty, &[Cell::Int(3)]).unwrap_err();
            assert_eq!(err.kind, BoundaryErrorKind::InternalError);
            assert_eq!(
                err.message,
                "field 'when' holds Int(3), which its type does not"
            );
        }
        for col in [
            ColumnData::ColOptDate(vec![]),
            ColumnData::ColOptDatetime(vec![]),
        ] {
            let err = element_value(&col, 0).unwrap_err();
            assert_eq!(err.kind, BoundaryErrorKind::InvalidArgument);
            assert_eq!(err.message, "a column holds fewer values than ids");
        }
    }

    #[test]
    fn database_and_internal_errors_are_not_shown_to_a_guest() {
        for kind in [ErrorKind::Database, ErrorKind::Internal] {
            let error = Error::new(
                kind,
                "duplicate key value violates constraint \"res_users_login_key\"",
            );
            let shown = boundary_of(&error);
            assert_eq!(shown.kind, BoundaryErrorKind::InternalError);
            assert_eq!(shown.message, GENERIC_INTERNAL);
        }
    }

    #[test]
    fn user_facing_errors_keep_their_message() {
        let shown = boundary_of(&Error::new(ErrorKind::Access, "no access to 'x'"));
        assert_eq!(shown.kind, BoundaryErrorKind::AccessDenied);
        assert_eq!(shown.message, "no access to 'x'");
        let shown = boundary_of(&Error::new(ErrorKind::Validation, "qty must be positive"));
        assert_eq!(shown.kind, BoundaryErrorKind::ValidationFailed);
        assert_eq!(shown.message, "qty must be positive");
    }
}

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    #[test]
    fn native_fields_columns_refuse_even_when_legacy_wire_could_hold_text_or_null() {
        for ty in [
            FieldType::BigInt,
            FieldType::Uuid,
            FieldType::Xid8,
            FieldType::TimestampTz,
        ] {
            for cells in [vec![], vec![Cell::Null]] {
                let error = column_of("native_value", &ty, &cells).unwrap_err();
                assert_eq!(error.kind, BoundaryErrorKind::Unsupported);
                assert_eq!(
                    error.message,
                    format!(
                        "field 'native_value' ({}) is not supported across frozen WASM ABI1; requires a future ABI major with typed native fields",
                        ty.type_name()
                    )
                );
            }
        }
        let column = column_of(
            "ordinary_neighbor",
            &FieldType::Char { size: None },
            &[Cell::Str("works".into()), Cell::Null],
        )
        .unwrap();
        assert_eq!(element_value(&column, 0).unwrap(), Value::str("works"));
        assert_eq!(element_value(&column, 1).unwrap(), Value::False);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests")]
mod create_name_tests {
    use super::*;

    #[test]
    fn batch_preflight_counts_distinct_names_and_checks_the_last_row() {
        let row: Vec<_> = (0..11)
            .map(|i| (format!("field_{i}"), ScalarValue::ValNull))
            .collect();
        let mut values = vec![row; 100_000];
        values
            .last_mut()
            .expect("last row")
            .push(("native_uuid".into(), ScalarValue::ValNull));
        let names = create_field_names(&values);
        let mut expected: Vec<_> = (0..11).map(|i| format!("field_{i}")).collect();
        expected.push("native_uuid".into());
        assert_eq!(names, expected);
        assert!(create_field_names(&[]).is_empty());
    }
}
