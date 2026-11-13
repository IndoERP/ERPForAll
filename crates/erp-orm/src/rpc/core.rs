//! Registered by [`super::ExposureTable::build`] directly, like the core method implementations
//! of [`crate::Dispatch::build`]: a `linkme` element in the crate that declares the slice would
//! expand `#[link_section]` here, which `forbid(unsafe_code)` rejects.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use erp_domain::{Domain, Operator};
use erp_fields::{Error, Value, ValueMap};

use super::{
    ArgError, ArgShape, BindError, Effect, FromRpc, IntoRpc, ParamSpec, Receiver, RpcArgs, RpcDecl,
    RpcImpl, json_type, mismatch, null, present,
};
use crate::dispatch::{BoxFuture, call_method};
use crate::read::Load;
use crate::read_group::ReadGroup;
use crate::records::Records;
use crate::search::{NameSearch, SearchOptions};
use crate::web::{
    FormattedReadGroup, FormattedReadGroupingSets, ProgressBar, SearchPanelSelectMultiRange,
    SearchPanelSelectRange, Spec, WebNameSearch, WebReadGroup, WebSave, WebSearchRead,
};
use crate::{Operation, Transaction};

macro_rules! core_rpc {
    ($stat:ident, $addon:literal, $method:literal, $effect:ident, $receiver:ident, $params:expr, $bind:ident) => {
        static $stat: RpcImpl = RpcImpl {
            addon: $addon,
            model: "base",
            method: $method,
            decl: RpcDecl::Expose {
                effect: Effect::$effect,
                receiver: Receiver::$receiver,
                params: $params,
                bind: $bind,
            },
            file: file!(),
            line: line!(),
        };
    };
}

/// The core declarations.
pub(super) static CORE_RPC: [&RpcImpl; 33] = [
    &SEARCH,
    &SEARCH_COUNT,
    &RELATION_COUNT,
    &SEARCH_READ,
    &READ,
    &NAME_SEARCH,
    &FIELDS_GET,
    &HAS_ACCESS,
    &CREATE,
    &WRITE,
    &UNLINK,
    &DEFAULT_GET,
    &COPY,
    &COPY_DATA,
    &NAME_CREATE,
    &ACTION_ARCHIVE,
    &ACTION_UNARCHIVE,
    &READ_GROUP,
    &WEB_READ,
    &WEB_SEARCH_READ,
    &WEB_NAME_SEARCH,
    &WEB_SAVE,
    &WEB_SAVE_MULTI,
    &WEB_RESEQUENCE,
    &ONCHANGE,
    &ONCHANGE_BATCH,
    &WEB_UNLINK,
    &FORMATTED_READ_GROUP,
    &FORMATTED_READ_GROUPING_SETS,
    &WEB_READ_GROUP,
    &READ_PROGRESS_BAR,
    &SEARCH_PANEL_SELECT_RANGE,
    &SEARCH_PANEL_SELECT_MULTI_RANGE,
];

mod args {
    use super::*;

    /// `read(load=…)`: absent or `"_classic_read"` is the classic load; any other value,
    /// `null` included, reads many2one values as ids.
    pub(super) struct LoadArg(pub(super) Load);

    impl FromRpc for LoadArg {
        const EXPECTED: &'static str = "a string or null";
        const SHAPE: ArgShape = ArgShape::Open("a load mode");
        const REQUIRED: bool = false;

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match value {
                None => Ok(Self(Load::Classic)),
                Some(Value::Str(s)) if &*s == "_classic_read" => Ok(Self(Load::Classic)),
                Some(Value::Str(_) | Value::False) => Ok(Self(Load::Raw)),
                Some(other) => Err(ArgError::Mismatch {
                    got: json_type(&other),
                }),
            }
        }
    }

    /// `create(vals_list)`: a mapping creates one record and returns its id; a list creates
    /// several and returns their ids (`@api.model_create_multi`).
    pub(super) enum ValsArg {
        One(ValueMap),
        Many(Vec<ValueMap>),
    }

    impl FromRpc for ValsArg {
        const EXPECTED: &'static str = "an object or an array of objects";
        const SHAPE: ArgShape = ArgShape::Open("field values");

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match value {
                None => Err(ArgError::Missing),
                Some(Value::Map(map)) => Ok(Self::One(Arc::unwrap_or_clone(map))),
                Some(list @ Value::List(_)) => {
                    Vec::<ValueMap>::from_rpc(Some(list)).map(Self::Many)
                }
                Some(other) => Err(ArgError::Mismatch {
                    got: json_type(&other),
                }),
            }
        }
    }

    /// The operator names `OperatorArg` accepts, in `Operator::ALL` order.
    pub(super) const OPERATOR_NAMES: [&str; Operator::ALL.len()] = {
        let mut out = [""; Operator::ALL.len()];
        let mut i = 0;
        while i < out.len() {
            out[i] = Operator::ALL[i].as_str();
            i += 1;
        }
        out
    };

    /// The operation names `OperationArg` accepts (a unit test pins them to its parser).
    pub(super) const OPERATION_NAMES: [&str; 4] = ["read", "write", "create", "unlink"];

    /// `name_search(operator=…)`: an operator name.
    pub(super) struct OperatorArg(pub(super) Operator);

    impl FromRpc for OperatorArg {
        const EXPECTED: &'static str = "a domain operator";
        const SHAPE: ArgShape = ArgShape::Enum(&OPERATOR_NAMES);

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match value {
                None => Err(ArgError::Missing),
                Some(Value::Str(s)) => Operator::parse(&s).map(Self).ok_or_else(|| {
                    ArgError::Invalid(Error::validation(format!("Invalid operator {s:?}")))
                }),
                Some(other) => Err(ArgError::Mismatch {
                    got: json_type(&other),
                }),
            }
        }
    }

    /// `has_access(operation)`: `read`, `write`, `create` or `unlink`.
    pub(super) struct OperationArg(pub(super) Operation);

    impl FromRpc for OperationArg {
        const EXPECTED: &'static str = "'read', 'write', 'create' or 'unlink'";
        const SHAPE: ArgShape = ArgShape::Enum(&OPERATION_NAMES);

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match value {
                None => Err(ArgError::Missing),
                Some(Value::Str(s)) => match &*s {
                    "read" => Ok(Self(Operation::Read)),
                    "write" => Ok(Self(Operation::Write)),
                    "create" => Ok(Self(Operation::Create)),
                    "unlink" => Ok(Self(Operation::Unlink)),
                    other => Err(ArgError::Invalid(Error::validation(format!(
                        "Invalid access operation {other:?}"
                    )))),
                },
                Some(other) => Err(ArgError::Mismatch {
                    got: json_type(&other),
                }),
            }
        }
    }

    /// JSON `false`
    /// (`Value::Bool(false)`) is not `null` and goes to `T`, which refuses it unless it accepts
    /// booleans. Used by `web_name_search(limit=…)`, `web_resequence(field_name=…)` and
    /// `web_read_group(unfold_read_default_limit=…)`.
    #[derive(Debug)]
    pub(super) enum Nullable<T> {
        Absent,
        Null,
        Given(T),
    }

    impl<T> Nullable<T> {
        /// `Some(default)` when the argument is absent, `None` for `null`, `Some(value)` otherwise.
        pub(super) fn or_absent(self, default: T) -> Option<T> {
            match self {
                Self::Absent => Some(default),
                Self::Null => None,
                Self::Given(v) => Some(v),
            }
        }

        pub(super) fn map<U>(self, f: impl FnOnce(T) -> U) -> Nullable<U> {
            match self {
                Self::Absent => Nullable::Absent,
                Self::Null => Nullable::Null,
                Self::Given(v) => Nullable::Given(f(v)),
            }
        }
    }

    /// The accepted shape of `T` or `null`, for [`Nullable`]'s binding errors: `T::EXPECTED` plus
    /// " or null" (a `const` cannot be concatenated from `T::EXPECTED`; a unit test checks each
    /// impl).
    pub(super) trait OrNull: FromRpc {
        const EXPECTED_OR_NULL: &'static str;
    }

    impl OrNull for u64 {
        const EXPECTED_OR_NULL: &'static str = "a non-negative integer or null";
    }

    impl OrNull for String {
        const EXPECTED_OR_NULL: &'static str = "a string or null";
    }

    impl<T: OrNull> FromRpc for Nullable<T> {
        const EXPECTED: &'static str = T::EXPECTED_OR_NULL;
        const SHAPE: ArgShape = ArgShape::OrNull(&T::SHAPE);
        const REQUIRED: bool = false;

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match value {
                None => Ok(Self::Absent),
                Some(Value::False) => Ok(Self::Null),
                Some(other) => T::from_rpc(Some(other)).map(Self::Given),
            }
        }
    }

    /// Used by `web_resequence(offset=…)`, `web_search_read(offset=…)` and
    /// `web_name_search(operator=…)`.
    #[derive(Debug)]
    pub(super) struct Defaulted<T>(Option<T>);

    impl<T> Defaulted<T> {
        /// The value, or `default` when the argument is absent.
        pub(super) fn or(self, default: T) -> T {
            self.0.unwrap_or(default)
        }
    }

    impl<T: FromRpc> FromRpc for Defaulted<T> {
        const EXPECTED: &'static str = T::EXPECTED;
        const SHAPE: ArgShape = T::SHAPE;
        const REQUIRED: bool = false;

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match value {
                None => Ok(Self(None)),
                Some(Value::False) => Err(ArgError::Mismatch {
                    got: json_type(&Value::False),
                }),
                Some(v) => T::from_rpc(Some(v)).map(|v| Self(Some(v))),
            }
        }
    }

    /// Search-panel limits: absent, `null`, `false`, and zero mean unlimited.
    /// Positive integers retain the caller's limit; other values fail binding.
    #[derive(Debug)]
    pub(super) struct SearchPanelLimitArg(pub(super) Option<u64>);

    impl FromRpc for SearchPanelLimitArg {
        const EXPECTED: &'static str = "a non-negative integer, false or null";
        const SHAPE: ArgShape = ArgShape::OrNull(&ArgShape::Integer { min: Some(0) });
        const REQUIRED: bool = false;

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match value {
                None | Some(Value::False | Value::Bool(false) | Value::Int(0)) => Ok(Self(None)),
                Some(other) => u64::from_rpc(Some(other)).map(|limit| Self(Some(limit))),
            }
        }
    }

    /// `web_read_group(groupby_read_specification=…)`: absent or `null` is `None`;
    /// an object is parsed into `{field: spec}`.
    #[derive(Debug)]
    pub(super) struct GroupbySpecArg(pub(super) Option<BTreeMap<String, Spec>>);

    impl FromRpc for GroupbySpecArg {
        const EXPECTED: &'static str = "a specification mapping (object) or null";
        const SHAPE: ArgShape = ArgShape::Open("a read specification mapping");
        const REQUIRED: bool = false;

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match value {
                // Some(Value::False) maps JSON false to "no spec" like null (binding convention)
                None | Some(Value::False) => Ok(Self(None)),
                Some(Value::Map(map)) => {
                    let mut out = BTreeMap::new();
                    for (k, v) in map.iter() {
                        let spec = Spec::from_value(v).map_err(|e| {
                            ArgError::Invalid(Error::validation(format!(
                                "groupby_read_specification['{k}']: {e}"
                            )))
                        })?;
                        out.insert(k.clone(), spec);
                    }
                    Ok(Self(Some(out)))
                }
                Some(other) => Err(ArgError::Mismatch {
                    got: json_type(&other),
                }),
            }
        }
    }

    /// `web_read_group(opening_info=…)`: absent or `null` or `false` is `None`;
    /// an array of objects is parsed into `Some(Vec<Value>)`.
    #[derive(Debug)]
    pub(super) struct OpeningInfoArg(pub(super) Option<Vec<Value>>);

    impl FromRpc for OpeningInfoArg {
        const EXPECTED: &'static str = "an array of objects";
        const SHAPE: ArgShape = ArgShape::Open("group opening state");
        const REQUIRED: bool = false;

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match value {
                None | Some(Value::False) => Ok(Self(None)),
                Some(Value::List(items)) => {
                    for item in items.iter() {
                        if !matches!(item, Value::Map(_)) {
                            return Err(ArgError::Mismatch {
                                got: json_type(item),
                            });
                        }
                    }
                    Ok(Self(Some(items.to_vec())))
                }
                Some(other) => Err(ArgError::Mismatch {
                    got: json_type(&other),
                }),
            }
        }
    }

    /// `read_progress_bar(progress_bar=…)`: `{field: …, colors: {…}}`.
    #[derive(Debug)]
    pub(super) struct ProgressBarArg {
        pub(super) field: String,
        pub(super) colors: Vec<String>,
    }

    impl FromRpc for ProgressBarArg {
        const EXPECTED: &'static str = "an object";
        const SHAPE: ArgShape = ArgShape::Open("a progress bar specification");
        const REQUIRED: bool = true;

        fn from_rpc(value: Option<Value>) -> Result<Self, ArgError> {
            match present(value)? {
                Value::Map(map) => {
                    let field = match map.get("field") {
                        Some(Value::Str(s)) => s.to_string(),
                        Some(other) => {
                            return Err(ArgError::Invalid(Error::validation(format!(
                                "'field' must be a string, got {}",
                                json_type(other)
                            ))));
                        }
                        None => {
                            return Err(ArgError::Invalid(Error::validation(
                                "'field' must be a string, got null".to_string(),
                            )));
                        }
                    };
                    let colors = match map.get("colors") {
                        Some(Value::Map(colors)) => colors.keys().cloned().collect(),
                        Some(other) => {
                            return Err(ArgError::Invalid(Error::validation(format!(
                                "'colors' must be an object, got {}",
                                json_type(other)
                            ))));
                        }
                        None => {
                            return Err(ArgError::Invalid(Error::validation(
                                "'colors' must be an object, got null".to_string(),
                            )));
                        }
                    };
                    Ok(Self { field, colors })
                }
                other => mismatch(&other),
            }
        }
    }
}

use args::{
    Defaulted, GroupbySpecArg, LoadArg, Nullable, OpeningInfoArg, OperationArg, OperatorArg,
    ProgressBarArg, SearchPanelLimitArg, ValsArg,
};

// `ParamSpec::new` is const, but `T::REQUIRED`/`T::EXPECTED` through a generic fn is not: the
// tables are spelled out with the trait constants.
macro_rules! param {
    ($name:literal, $ty:ty) => {
        ParamSpec::new(
            $name,
            <$ty as FromRpc>::REQUIRED,
            <$ty as FromRpc>::EXPECTED,
        )
    };
}

fn ids_of(records: &Records) -> Value {
    Value::list(records.ids().into_iter().map(Value::Int))
}

fn search_options(offset: Option<u64>, limit: Option<u64>, order: Option<String>) -> SearchOptions {
    SearchOptions {
        offset: offset.unwrap_or(0),
        limit,
        order,
        ..SearchOptions::default()
    }
}

fn field_list(fields: &Option<Vec<String>>) -> Vec<&str> {
    fields
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(String::as_str)
        .collect()
}

// ---- read ---------------------------------------------------------------------------------------

const SEARCH_PARAMS: &[ParamSpec] = &[
    param!("domain", Domain),
    param!("offset", Option<u64>),
    param!("limit", Option<u64>),
    param!("order", Option<String>),
];
core_rpc!(
    SEARCH,
    "base",
    "search",
    Read,
    Model,
    SEARCH_PARAMS,
    bind_search
);

fn bind_search<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let domain: Domain = args.take("domain")?;
        let opts = search_options(
            args.take("offset")?,
            args.take("limit")?,
            args.take("order")?,
        );
        let found = records.search_with(tx, &domain, &opts).await?;
        Ok(ids_of(&found))
    })
}

const SEARCH_COUNT_PARAMS: &[ParamSpec] = &[param!("domain", Domain), param!("limit", Option<u64>)];
core_rpc!(
    SEARCH_COUNT,
    "base",
    "search_count",
    Read,
    Model,
    SEARCH_COUNT_PARAMS,
    bind_search_count
);

fn bind_search_count<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let domain: Domain = args.take("domain")?;
        let limit: Option<u64> = args.take("limit")?;
        let count = records.search_count(tx, &domain, limit).await?;
        Ok(count.into_rpc()?)
    })
}

// ERPForAll extension: the relation descriptor is resolved on the server, never supplied
// by the browser. Keeping the binder private avoids a second unchecked count API.
core_rpc!(
    RELATION_COUNT,
    "base",
    "relation_count",
    Read,
    Records,
    &[param!("field", String)],
    bind_relation_count
);

fn bind_relation_count<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let field: String = args.take("field")?;
        let count = super::relation_count::count(tx, &records, &field).await?;
        Ok(count.into_rpc()?)
    })
}

const SEARCH_READ_PARAMS: &[ParamSpec] = &[
    param!("domain", Option<Domain>),
    param!("fields", Option<Vec<String>>),
    param!("offset", Option<u64>),
    param!("limit", Option<u64>),
    param!("order", Option<String>),
    param!("load", LoadArg),
];
core_rpc!(
    SEARCH_READ,
    "base",
    "search_read",
    Read,
    Model,
    SEARCH_READ_PARAMS,
    bind_search_read
);

fn bind_search_read<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        // `domain=None` searches everything
        let domain = args
            .take::<Option<Domain>>("domain")?
            .unwrap_or(Domain::TRUE);
        let fields: Option<Vec<String>> = args.take("fields")?;
        let opts = search_options(
            args.take("offset")?,
            args.take("limit")?,
            args.take("order")?,
        );
        let LoadArg(load) = args.take("load")?;
        let names = field_list(&fields);
        let opts = if names.is_empty() {
            opts.fetch_prefetchable()
        } else {
            opts.fetch_fields(names.iter().copied())
        };
        let found = records.search_with(tx, &domain, &opts).await?;
        let rows = found.read_with(tx, &names, load).await?;
        Ok(rows.into_rpc()?)
    })
}

const READ_PARAMS: &[ParamSpec] = &[
    param!("fields", Option<Vec<String>>),
    param!("load", LoadArg),
];
core_rpc!(READ, "base", "read", Read, Records, READ_PARAMS, bind_read);

fn bind_read<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let fields: Option<Vec<String>> = args.take("fields")?;
        let LoadArg(load) = args.take("load")?;
        let rows = records.read_with(tx, &field_list(&fields), load).await?;
        Ok(rows.into_rpc()?)
    })
}

const NAME_SEARCH_PARAMS: &[ParamSpec] = &[
    param!("name", Option<String>),
    param!("domain", Option<Domain>),
    param!("operator", Option<OperatorArg>),
    param!("limit", Option<u64>),
];
core_rpc!(
    NAME_SEARCH,
    "base",
    "name_search",
    Read,
    Model,
    NAME_SEARCH_PARAMS,
    bind_name_search
);

fn bind_name_search<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let name = args.take::<Option<String>>("name")?.unwrap_or_default();
        let mut options = NameSearch::default();
        if let Some(domain) = args.take::<Option<Domain>>("domain")? {
            options.domain = domain;
        }
        if let Some(OperatorArg(operator)) = args.take("operator")? {
            options.operator = operator;
        }
        options.limit = match args.take::<Option<Value>>("limit")? {
            None => options.limit,
            Some(value) => {
                Some(
                    u64::from_rpc(Some(value)).map_err(|error| BindError::Argument {
                        name: "limit",
                        expected: u64::EXPECTED,
                        error,
                    })?,
                )
            }
        };
        let found = records.name_search_with(tx, &name, &options).await?;
        let pairs: Vec<(i64, Option<Arc<str>>)> = found;
        Ok(pairs.into_rpc()?)
    })
}

const FIELDS_GET_PARAMS: &[ParamSpec] = &[
    param!("allfields", Option<Vec<String>>),
    param!("attributes", Option<Vec<String>>),
];
core_rpc!(
    FIELDS_GET,
    "base",
    "fields_get",
    Read,
    Model,
    FIELDS_GET_PARAMS,
    bind_fields_get
);

fn bind_fields_get<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let allfields: Option<Vec<String>> = args.take("allfields")?;
        let attributes: Option<Vec<String>> = args.take("attributes")?;
        let all: Option<Vec<&str>> = allfields
            .as_ref()
            .map(|v| v.iter().map(String::as_str).collect());
        let attrs: Option<Vec<&str>> = attributes
            .as_ref()
            .map(|v| v.iter().map(String::as_str).collect());
        // the policy generation and the subject's groups first (as `read_with` does): the field
        // masks `fields_get` consults must see this transaction's own group writes
        if !records.env().su() {
            tx.prepare_access(records.env()).await?;
        }
        let described = records
            .fields_get(tx, all.as_deref(), attrs.as_deref())
            .await?;
        let out: ValueMap = described
            .into_iter()
            .map(|(name, d)| (name, Value::Map(Arc::new(d.into_iter().collect()))))
            .collect();
        Ok(Value::Map(Arc::new(out)))
    })
}

const HAS_ACCESS_PARAMS: &[ParamSpec] = &[param!("operation", OperationArg)];
core_rpc!(
    HAS_ACCESS,
    "base",
    "has_access",
    Read,
    Records,
    HAS_ACCESS_PARAMS,
    bind_has_access
);

fn bind_has_access<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let OperationArg(op) = args.take("operation")?;
        Ok(Value::Bool(has_access(tx, &records, op).await?))
    })
}

/// `has_access(operation)`: the model-level check and the record rules of
/// `operation` for every record, without raising. Archived records count (no active test).
async fn has_access(tx: &mut Transaction, records: &Records, op: Operation) -> Result<bool, Error> {
    let env = records.env().clone();
    // a refused company context is an error, never "no access" (review F1/S5)
    env.companies()?;
    tx.prepare_access(&env).await?;
    match tx.check_model(&env, records.model_id(), op) {
        Ok(()) => {}
        // only a refusal means "no access"; any other failure is the caller's error
        Err(error) if error.kind() == erp_fields::ErrorKind::Access => return Ok(false),
        Err(error) => return Err(error),
    }
    let mut ids: Vec<i64> = records
        .rec_ids()
        .iter()
        .filter(|id| !id.is_new())
        .map(|id| id.get())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() || env.su() {
        return Ok(true);
    }
    let rules = tx.access_predicate(&env, records.model_id(), op)?;
    let wanted = ids.len() as u64;
    let condition = Domain::condition(
        "id",
        Operator::In,
        erp_domain::Value::list(ids.into_iter().map(erp_domain::Value::Int)),
    )?;
    let opts = SearchOptions {
        active_test: false,
        ..SearchOptions::default()
    };
    // the rules are enforced under the fixed predicate context; agree with the enforcement
    let visible = crate::search::predicate_records(records)
        .engine_sudo()
        .search_query(tx, &(condition & rules), &opts)
        .await?
        .count(tx)
        .await?;
    Ok(visible == wanted)
}

// ---- write --------------------------------------------------------------------------------------

const CREATE_PARAMS: &[ParamSpec] = &[param!("vals_list", ValsArg)];
core_rpc!(
    CREATE,
    "base",
    "create",
    Write,
    Model,
    CREATE_PARAMS,
    bind_create
);

fn bind_create<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let (list, single) = match args.take::<ValsArg>("vals_list")? {
            ValsArg::One(map) => (vec![map], true),
            ValsArg::Many(list) => (list, false),
        };
        let vals = list
            .iter()
            .map(|map| records.vals_from_values(map.iter()))
            .collect::<Result<Vec<_>, Error>>()?;
        let created = records.create(tx, vals).await?;
        if single {
            match created.ids().as_slice() {
                [id] => Ok(Value::Int(*id)),
                ids => Err(BindError::Call(Error::programming(format!(
                    "create of one mapping returned {} records",
                    ids.len()
                )))),
            }
        } else {
            Ok(ids_of(&created))
        }
    })
}

const WRITE_PARAMS: &[ParamSpec] = &[param!("vals", ValueMap)];
core_rpc!(
    WRITE,
    "base",
    "write",
    Write,
    Records,
    WRITE_PARAMS,
    bind_write
);

fn bind_write<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let vals: ValueMap = args.take("vals")?;
        records.write_values(tx, vals.iter()).await?;
        Ok(Value::Bool(true))
    })
}

core_rpc!(
    UNLINK,
    "base",
    "unlink",
    Destructive,
    Records,
    &[],
    bind_unlink
);

fn bind_unlink<'a>(
    tx: &'a mut Transaction,
    records: Records,
    _args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        records.unlink(tx).await?;
        Ok(Value::Bool(true))
    })
}

const DEFAULT_GET_PARAMS: &[ParamSpec] = &[param!("fields", Vec<String>)];
core_rpc!(
    DEFAULT_GET,
    "base",
    "default_get",
    Write,
    Model,
    DEFAULT_GET_PARAMS,
    bind_default_get
);

fn bind_default_get<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let fields: Vec<String> = args.take("fields")?;
        let names: Vec<&str> = fields.iter().map(String::as_str).collect();
        Ok(records.default_get(tx, &names).await?.into_rpc()?)
    })
}

const READ_GROUP_PARAMS: &[ParamSpec] = &[
    param!("domain", Domain),
    param!("groupby", Option<Vec<String>>),
    param!("aggregates", Option<Vec<String>>),
    param!("having", Option<Vec<Value>>),
    param!("offset", Option<u64>),
    param!("limit", Option<u64>),
    param!("order", Option<String>),
];
core_rpc!(
    READ_GROUP,
    "base",
    "read_group",
    Write,
    Model,
    READ_GROUP_PARAMS,
    bind_read_group
);

/// Most entries of a grouping list argument (`groupby`, `aggregates`, `having`, `grouping_sets`),
/// a [per-call shape bound](crate::web#per-call-shape-bounds): each entry is a column or an
/// expression of one `GROUP BY` statement; 64 is far above what any view groups or aggregates by.
const MAX_LIST_ENTRIES: usize = 64;

fn check_list_len(name: &'static str, len: usize, item_name: &'static str) -> Result<(), Error> {
    if len > MAX_LIST_ENTRIES {
        Err(Error::validation(format!(
            "'{name}' accepts at most {MAX_LIST_ENTRIES} {item_name}, got {len}"
        )))
    } else {
        Ok(())
    }
}

struct GroupArgs {
    domain: Domain,
    groupby: Vec<String>,
    aggregates: Vec<String>,
    having: Vec<Value>,
    offset: u64,
    limit: Option<u64>,
    order: Option<String>,
}

fn extract_group_args(args: &mut RpcArgs) -> Result<GroupArgs, BindError> {
    let domain: Domain = args.take("domain")?;
    let groupby: Vec<String> = args
        .take::<Option<Vec<String>>>("groupby")?
        .unwrap_or_default();
    let aggregates: Vec<String> = args
        .take::<Option<Vec<String>>>("aggregates")?
        .unwrap_or_default();
    let having: Vec<Value> = args
        .take::<Option<Vec<Value>>>("having")?
        .unwrap_or_default();
    let offset = args.take::<Option<u64>>("offset")?.unwrap_or(0);
    let limit: Option<u64> = args.take("limit")?;
    let order: Option<String> = args.take("order")?;

    check_list_len("groupby", groupby.len(), "entries")?;
    check_list_len("aggregates", aggregates.len(), "entries")?;
    check_list_len("having", having.len(), "entries")?;

    Ok(GroupArgs {
        domain,
        groupby,
        aggregates,
        having,
        offset,
        limit,
        order,
    })
}

fn bind_read_group<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let g = extract_group_args(&mut args)?;
        let mut call = ReadGroup::new(g.domain);
        call.groupby = g.groupby;
        call.aggregates = g.aggregates;
        call.having = g.having;
        call.offset = g.offset;
        call.limit = g.limit;
        call.order = g.order;
        call.external = true;
        Ok(records.read_group(tx, call).await?.into_rpc()?)
    })
}

const COPY_PARAMS: &[ParamSpec] = &[param!("default", Option<ValueMap>)];
core_rpc!(COPY, "base", "copy", Write, Records, COPY_PARAMS, bind_copy);

fn bind_copy<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let default: Option<ValueMap> = args.take("default")?;
        let copied = records.copy(tx, default).await?;
        Ok(ids_of(&copied))
    })
}

const COPY_DATA_PARAMS: &[ParamSpec] = &[param!("default", Option<ValueMap>)];
core_rpc!(
    COPY_DATA,
    "base",
    "copy_data",
    Write,
    Records,
    COPY_DATA_PARAMS,
    bind_copy_data
);

fn bind_copy_data<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let default: Option<ValueMap> = args.take("default")?;
        let list = records.copy_data(tx, default.as_ref()).await?;
        let items: Vec<Value> = list
            .into_iter()
            .map(|opt| match opt {
                Some(map) => Value::Map(Arc::new(map)),
                None => null(),
            })
            .collect();
        Ok(Value::List(items.into()))
    })
}

const NAME_CREATE_PARAMS: &[ParamSpec] = &[param!("name", String)];
core_rpc!(
    NAME_CREATE,
    "base",
    "name_create",
    Write,
    Model,
    NAME_CREATE_PARAMS,
    bind_name_create
);

fn bind_name_create<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let name: String = args.take("name")?;
        let result = records.name_create(tx, &name).await?;
        Ok(result.into_rpc()?)
    })
}

core_rpc!(
    ACTION_ARCHIVE,
    "base",
    "action_archive",
    Write,
    Records,
    &[],
    bind_action_archive
);

fn bind_action_archive<'a>(
    tx: &'a mut Transaction,
    records: Records,
    _args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        records.action_archive(tx).await?;
        Ok(null())
    })
}

core_rpc!(
    ACTION_UNARCHIVE,
    "base",
    "action_unarchive",
    Write,
    Records,
    &[],
    bind_action_unarchive
);

fn bind_action_unarchive<'a>(
    tx: &'a mut Transaction,
    records: Records,
    _args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        records.action_unarchive(tx).await?;
        Ok(null())
    })
}

// ---- web ----------------------------------------------------------------------------------------

const WEB_READ_PARAMS: &[ParamSpec] = &[param!("specification", Spec)];
core_rpc!(
    WEB_READ,
    "web",
    "web_read",
    Read,
    Records,
    WEB_READ_PARAMS,
    bind_web_read
);

fn bind_web_read<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let spec: Spec = args.take("specification")?;
        Ok(records.web_read(tx, spec).await?.into_rpc()?)
    })
}

const WEB_SEARCH_READ_PARAMS: &[ParamSpec] = &[
    param!("domain", Domain),
    param!("specification", Spec),
    param!("offset", Defaulted<u64>),
    param!("limit", Option<u64>),
    param!("order", Option<String>),
    param!("count_limit", Option<u64>),
];
core_rpc!(
    WEB_SEARCH_READ,
    "web",
    "web_search_read",
    Read,
    Model,
    WEB_SEARCH_READ_PARAMS,
    bind_web_search_read
);

fn bind_web_search_read<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let domain: Domain = args.take("domain")?;
        let spec: Spec = args.take("specification")?;
        let mut call = WebSearchRead::new(domain, spec);
        // `offset: int = 0`; `null` refused
        call.offset = args.take::<Defaulted<u64>>("offset")?.or(call.offset);
        call.limit = args.take("limit")?;
        call.order = args.take("order")?;
        call.count_limit = args.take("count_limit")?;
        Ok(records.web_search_read(tx, call).await?)
    })
}

const WEB_NAME_SEARCH_PARAMS: &[ParamSpec] = &[
    param!("name", String),
    param!("specification", Spec),
    param!("domain", Option<Domain>),
    param!("operator", Defaulted<OperatorArg>),
    param!("limit", Nullable<u64>),
];
core_rpc!(
    WEB_NAME_SEARCH,
    "web",
    "web_name_search",
    Read,
    Model,
    WEB_NAME_SEARCH_PARAMS,
    bind_web_name_search
);

fn bind_web_name_search<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let name: String = args.take("name")?;
        let specification: Spec = args.take("specification")?;
        // `domain: DomainType | None = None`: `None` is a
        // documented input there, so absent and `null` both search without a domain.
        let domain = match args.take::<Option<Domain>>("domain")? {
            Some(domain) => domain,
            None => Domain::TRUE,
        };
        // `operator: str = 'ilike'`, `limit: int = 100` (`NAME_SEARCH_LIMIT`;
        // )
        let OperatorArg(operator) = args
            .take::<Defaulted<OperatorArg>>("operator")?
            .or(OperatorArg(crate::web::WEB_NAME_SEARCH_OPERATOR));
        let limit = args
            .take::<Nullable<u64>>("limit")?
            .or_absent(crate::NAME_SEARCH_LIMIT);
        let call = WebNameSearch {
            name,
            specification,
            domain,
            operator,
            limit,
        };
        Ok(records.web_name_search(tx, call).await?.into_rpc()?)
    })
}

const WEB_SAVE_PARAMS: &[ParamSpec] = &[
    param!("vals", ValueMap),
    param!("specification", Spec),
    param!("next_id", Option<i64>),
];
core_rpc!(
    WEB_SAVE,
    "web",
    "web_save",
    Write,
    Records,
    WEB_SAVE_PARAMS,
    bind_web_save
);

fn bind_web_save<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let vals: ValueMap = args.take("vals")?;
        let specification: Spec = args.take("specification")?;
        let next_id: Option<i64> = args.take("next_id")?;
        let call = WebSave {
            vals,
            specification,
            next_id,
        };
        Ok(records.web_save(tx, call).await?.into_rpc()?)
    })
}

const WEB_SAVE_MULTI_PARAMS: &[ParamSpec] = &[
    param!("vals_list", Vec<ValueMap>),
    param!("specification", Spec),
];
core_rpc!(
    WEB_SAVE_MULTI,
    "web",
    "web_save_multi",
    Write,
    Records,
    WEB_SAVE_MULTI_PARAMS,
    bind_web_save_multi
);

fn bind_web_save_multi<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let vals_list: Vec<ValueMap> = args.take("vals_list")?;
        let specification: Spec = args.take("specification")?;
        Ok(records
            .web_save_multi(tx, vals_list, specification)
            .await?
            .into_rpc()?)
    })
}

const WEB_RESEQUENCE_PARAMS: &[ParamSpec] = &[
    param!("specification", Spec),
    param!("field_name", Nullable<String>),
    param!("offset", Defaulted<i64>),
];
core_rpc!(
    WEB_RESEQUENCE,
    "web",
    "web_resequence",
    Write,
    Records,
    WEB_RESEQUENCE_PARAMS,
    bind_web_resequence
);

fn bind_web_resequence<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        // every argument is converted before the `null` field name returns, so a bad `offset`
        // is refused whatever the field name
        let specification: Spec = args.take("specification")?;
        // `field_name: str = 'sequence'`, `offset: int = 0`
        let field_name = args
            .take::<Nullable<String>>("field_name")?
            .map(Cow::Owned)
            .or_absent(Cow::Borrowed(crate::web::WEB_RESEQUENCE_FIELD));
        let offset = args
            .take::<Defaulted<i64>>("offset")?
            .or(crate::web::WEB_RESEQUENCE_OFFSET);
        let Some(field_name) = field_name else {
            return Ok(Value::list([]));
        };
        Ok(records
            .web_resequence(tx, specification, field_name, offset)
            .await?
            .into_rpc()?)
    })
}

const ONCHANGE_PARAMS: &[ParamSpec] = &[
    param!("values", ValueMap),
    param!("field_names", Vec<String>),
    param!("fields_spec", Spec),
];
core_rpc!(
    ONCHANGE,
    "web",
    "onchange",
    Write,
    Records,
    ONCHANGE_PARAMS,
    bind_onchange
);

fn bind_onchange<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let values: ValueMap = args.take("values")?;
        let field_names: Vec<String> = args.take("field_names")?;
        let spec: Spec = args.take("fields_spec")?;
        Ok(call_method::<(ValueMap, Vec<String>, Spec), Value>(
            tx,
            &records,
            "onchange",
            (values, field_names, spec),
        )
        .await?)
    })
}

const ONCHANGE_BATCH_PARAMS: &[ParamSpec] = &[
    param!("values_list", Vec<ValueMap>),
    param!("field_names", Vec<String>),
    param!("fields_spec", Spec),
];
core_rpc!(
    ONCHANGE_BATCH,
    "web",
    "onchange_batch",
    Write,
    Model,
    ONCHANGE_BATCH_PARAMS,
    bind_onchange_batch
);

fn bind_onchange_batch<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let values_list: Vec<ValueMap> = args.take("values_list")?;
        let field_names: Vec<String> = args.take("field_names")?;
        let spec: Spec = args.take("fields_spec")?;
        Ok(records
            .onchange_batch(tx, values_list, field_names, spec)
            .await?
            .into_rpc()?)
    })
}

core_rpc!(
    WEB_UNLINK,
    "web",
    "web_unlink",
    Destructive,
    Records,
    &[],
    bind_web_unlink
);

fn bind_web_unlink<'a>(
    tx: &'a mut Transaction,
    records: Records,
    _args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move { Ok(Value::Bool(records.web_unlink(tx).await?)) })
}

const FORMATTED_READ_GROUP_PARAMS: &[ParamSpec] = &[
    param!("domain", Domain),
    param!("groupby", Option<Vec<String>>),
    param!("aggregates", Option<Vec<String>>),
    param!("having", Option<Vec<Value>>),
    param!("offset", Option<u64>),
    param!("limit", Option<u64>),
    param!("order", Option<String>),
];
core_rpc!(
    FORMATTED_READ_GROUP,
    "web",
    "formatted_read_group",
    Read,
    Model,
    FORMATTED_READ_GROUP_PARAMS,
    bind_formatted_read_group
);

fn bind_formatted_read_group<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let g = extract_group_args(&mut args)?;
        let call = FormattedReadGroup {
            domain: g.domain,
            groupby: g.groupby,
            aggregates: g.aggregates,
            having: g.having,
            offset: g.offset,
            limit: g.limit,
            order: g.order,
            count_groups: false,
        };
        Ok(records.formatted_read_group(tx, call).await?.into_rpc()?)
    })
}

const FORMATTED_READ_GROUPING_SETS_PARAMS: &[ParamSpec] = &[
    param!("domain", Domain),
    param!("grouping_sets", Vec<Vec<String>>),
    param!("aggregates", Option<Vec<String>>),
    param!("order", Option<String>),
];
core_rpc!(
    FORMATTED_READ_GROUPING_SETS,
    "web",
    "formatted_read_grouping_sets",
    Read,
    Model,
    FORMATTED_READ_GROUPING_SETS_PARAMS,
    bind_formatted_read_grouping_sets
);

fn bind_formatted_read_grouping_sets<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let domain: Domain = args.take("domain")?;
        let grouping_sets: Vec<Vec<String>> = args.take("grouping_sets")?;
        let aggregates: Vec<String> = args
            .take::<Option<Vec<String>>>("aggregates")?
            .unwrap_or_default();
        let order: Option<String> = args.take("order")?;

        check_list_len("grouping_sets", grouping_sets.len(), "sets")?;
        check_list_len("aggregates", aggregates.len(), "entries")?;

        let call = FormattedReadGroupingSets {
            domain,
            grouping_sets,
            aggregates,
            order,
        };
        Ok(records
            .formatted_read_grouping_sets(tx, call)
            .await?
            .into_rpc()?)
    })
}

const WEB_READ_GROUP_PARAMS: &[ParamSpec] = &[
    param!("domain", Domain),
    param!("groupby", Vec<String>),
    param!("aggregates", Option<Vec<String>>),
    param!("limit", Option<u64>),
    param!("offset", Option<u64>),
    param!("order", Option<String>),
    param!("auto_unfold", Option<bool>),
    param!("opening_info", OpeningInfoArg),
    param!("unfold_read_specification", Option<Spec>),
    param!("unfold_read_default_limit", Nullable<u64>),
    param!("groupby_read_specification", GroupbySpecArg),
];
core_rpc!(
    WEB_READ_GROUP,
    "web",
    "web_read_group",
    Read,
    Model,
    WEB_READ_GROUP_PARAMS,
    bind_web_read_group
);

fn bind_web_read_group<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let domain: Domain = args.take("domain")?;
        let groupby: Vec<String> = args.take("groupby")?;
        check_list_len("groupby", groupby.len(), "entries")?;
        let mut call = WebReadGroup::new(domain, groupby);
        call.aggregates = args
            .take::<Option<Vec<String>>>("aggregates")?
            .unwrap_or_default();
        check_list_len("aggregates", call.aggregates.len(), "entries")?;
        call.limit = args.take("limit")?;
        call.offset = args.take::<Option<u64>>("offset")?.unwrap_or(0);
        call.order = args.take("order")?;
        call.auto_unfold = args.take::<Option<bool>>("auto_unfold")?.unwrap_or(false);
        call.opening_info = args.take::<OpeningInfoArg>("opening_info")?.0;
        call.unfold_read_specification = args.take("unfold_read_specification")?;
        // `unfold_read_default_limit: int | None = 80`
        call.unfold_read_default_limit = args
            .take::<Nullable<u64>>("unfold_read_default_limit")?
            .or_absent(crate::web::UNFOLD_READ_DEFAULT_LIMIT);
        call.groupby_read_specification =
            args.take::<GroupbySpecArg>("groupby_read_specification")?.0;
        Ok(records.web_read_group(tx, call).await?)
    })
}

const READ_PROGRESS_BAR_PARAMS: &[ParamSpec] = &[
    param!("domain", Domain),
    param!("group_by", String),
    param!("progress_bar", ProgressBarArg),
];
core_rpc!(
    READ_PROGRESS_BAR,
    "web",
    "read_progress_bar",
    Read,
    Model,
    READ_PROGRESS_BAR_PARAMS,
    bind_read_progress_bar
);

fn bind_read_progress_bar<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let domain: Domain = args.take("domain")?;
        let group_by: String = args.take("group_by")?;
        let pb = args.take::<ProgressBarArg>("progress_bar")?;
        let call = ProgressBar {
            domain,
            group_by,
            field: pb.field,
            colors: pb.colors,
        };
        Ok(records.read_progress_bar(tx, call).await?)
    })
}

const SEARCH_PANEL_SELECT_RANGE_PARAMS: &[ParamSpec] = &[
    param!("field_name", String),
    param!("search_domain", Option<Domain>),
    param!("category_domain", Option<Domain>),
    param!("filter_domain", Option<Domain>),
    param!("comodel_domain", Option<Domain>),
    param!("enable_counters", Option<bool>),
    param!("expand", Option<bool>),
    param!("hierarchize", Option<bool>),
    param!("limit", SearchPanelLimitArg),
];
core_rpc!(
    SEARCH_PANEL_SELECT_RANGE,
    "web",
    "search_panel_select_range",
    Read,
    Model,
    SEARCH_PANEL_SELECT_RANGE_PARAMS,
    bind_search_panel_select_range
);

fn bind_search_panel_select_range<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let field_name: String = args.take("field_name")?;
        let search_domain: Option<Domain> = args.take("search_domain")?;
        let category_domain: Option<Domain> = args.take("category_domain")?;
        let filter_domain: Option<Domain> = args.take("filter_domain")?;
        let comodel_domain: Option<Domain> = args.take("comodel_domain")?;
        let enable_counters = args
            .take::<Option<bool>>("enable_counters")?
            .unwrap_or(false);
        let expand = args.take::<Option<bool>>("expand")?.unwrap_or(false);
        let hierarchize = args.take::<Option<bool>>("hierarchize")?.unwrap_or(true);
        let limit = args.take::<SearchPanelLimitArg>("limit")?.0;

        let call = SearchPanelSelectRange {
            field_name,
            search_domain,
            category_domain,
            filter_domain,
            comodel_domain,
            enable_counters,
            expand,
            hierarchize,
            limit,
        };
        Ok(records.search_panel_select_range(tx, call).await?)
    })
}

const SEARCH_PANEL_SELECT_MULTI_RANGE_PARAMS: &[ParamSpec] = &[
    param!("field_name", String),
    param!("search_domain", Option<Domain>),
    param!("category_domain", Option<Domain>),
    param!("filter_domain", Option<Domain>),
    param!("comodel_domain", Option<Domain>),
    param!("enable_counters", Option<bool>),
    param!("expand", Option<bool>),
    param!("group_by", Option<String>),
    param!("group_domain", Option<Value>),
    param!("limit", SearchPanelLimitArg),
];
core_rpc!(
    SEARCH_PANEL_SELECT_MULTI_RANGE,
    "web",
    "search_panel_select_multi_range",
    Read,
    Model,
    SEARCH_PANEL_SELECT_MULTI_RANGE_PARAMS,
    bind_search_panel_select_multi_range
);

fn bind_search_panel_select_multi_range<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let field_name: String = args.take("field_name")?;
        let search_domain: Option<Domain> = args.take("search_domain")?;
        let category_domain: Option<Domain> = args.take("category_domain")?;
        let filter_domain: Option<Domain> = args.take("filter_domain")?;
        let comodel_domain: Option<Domain> = args.take("comodel_domain")?;
        let enable_counters = args
            .take::<Option<bool>>("enable_counters")?
            .unwrap_or(false);
        let expand = args.take::<Option<bool>>("expand")?.unwrap_or(false);
        let group_by: Option<String> = args.take("group_by")?;
        let group_domain: Option<Value> = args.take("group_domain")?;
        let limit = args.take::<SearchPanelLimitArg>("limit")?.0;

        let call = SearchPanelSelectMultiRange {
            field_name,
            search_domain,
            category_domain,
            filter_domain,
            comodel_domain,
            enable_counters,
            expand,
            group_by,
            group_domain,
            limit,
        };
        Ok(records.search_panel_select_multi_range(tx, call).await?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p<T: FromRpc>(name: &'static str) -> ParamSpec {
        ParamSpec::new(name, T::REQUIRED, T::EXPECTED)
    }

    #[test]
    fn parameter_tables_follow_the_types() {
        assert_eq!(
            SEARCH_PARAMS
                .iter()
                .map(|p| (p.name(), p.required()))
                .collect::<Vec<_>>(),
            [
                ("domain", true),
                ("offset", false),
                ("limit", false),
                ("order", false)
            ]
        );
        assert_eq!(p::<Domain>("domain"), SEARCH_PARAMS[0]);
        assert!(!READ_PARAMS[1].required(), "load has a default");
        assert_eq!(
            WEB_NAME_SEARCH_PARAMS
                .iter()
                .map(|p| (p.name(), p.required()))
                .collect::<Vec<_>>(),
            [
                ("name", true),
                ("specification", true),
                ("domain", false),
                ("operator", false),
                ("limit", false),
            ]
        );
        assert_eq!(
            WEB_SAVE_MULTI_PARAMS
                .iter()
                .map(|p| (p.name(), p.required()))
                .collect::<Vec<_>>(),
            [("vals_list", true), ("specification", true)]
        );
        assert_eq!(
            WEB_RESEQUENCE_PARAMS
                .iter()
                .map(|p| (p.name(), p.required()))
                .collect::<Vec<_>>(),
            [
                ("specification", true),
                ("field_name", false),
                ("offset", false),
            ]
        );
        assert_eq!(
            ONCHANGE_BATCH_PARAMS
                .iter()
                .map(|p| (p.name(), p.required()))
                .collect::<Vec<_>>(),
            [
                ("values_list", true),
                ("field_names", true),
                ("fields_spec", true),
            ]
        );
        let methods: Vec<&str> = CORE_RPC.iter().map(|i| i.method).collect();
        let mut sorted = methods.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), methods.len(), "no duplicates");
    }

    /// The message a binder reports for `error` on argument `name` of type `T`.
    fn message<T: FromRpc>(name: &'static str, error: ArgError) -> String {
        BindError::Argument {
            name,
            expected: T::EXPECTED,
            error,
        }
        .to_string()
    }

    #[test]
    fn test_nullable_u64_arg() {
        let limit = |v| Nullable::<u64>::from_rpc(v).unwrap();
        // absent takes the binder's default, `null` is no limit, an integer is itself
        assert_eq!(limit(None).or_absent(100), Some(100));
        assert_eq!(limit(Some(Value::False)).or_absent(100), None);
        assert_eq!(limit(Some(Value::Int(42))).or_absent(100), Some(42));
        let err = Nullable::<u64>::from_rpc(Some(Value::Int(-1))).unwrap_err();
        assert!(matches!(err, ArgError::OutOfRange { got: -1 }), "{err:?}");
        assert_eq!(
            message::<Nullable<u64>>("limit", err),
            "argument 'limit': expected a non-negative integer or null, got -1"
        );
        // JSON `false` is a boolean, not `null`
        let err = Nullable::<u64>::from_rpc(Some(Value::Bool(false))).unwrap_err();
        assert!(
            matches!(err, ArgError::Mismatch { got: "boolean" }),
            "{err:?}"
        );
        assert_eq!(
            message::<Nullable<u64>>("unfold_read_default_limit", err),
            "argument 'unfold_read_default_limit': expected a non-negative integer or null, got boolean"
        );
        let err = Nullable::<u64>::from_rpc(Some(Value::str("x"))).unwrap_err();
        assert!(
            matches!(err, ArgError::Mismatch { got: "string" }),
            "{err:?}"
        );
    }

    #[test]
    fn test_nullable_string_arg() {
        let field = |v| {
            Nullable::<String>::from_rpc(v)
                .unwrap()
                .map(Cow::Owned)
                .or_absent(Cow::Borrowed("sequence"))
        };
        // the default stays borrowed: the binder builds no `String` for it; the client's name is
        // moved, never copied (the chain's owned `String` is made once, in `web_resequence`)
        assert!(matches!(field(None), Some(Cow::Borrowed("sequence"))));
        assert_eq!(field(Some(Value::False)), None);
        assert_eq!(
            field(Some(Value::str("my_seq"))),
            Some(Cow::Owned("my_seq".to_owned()))
        );
        let err = Nullable::<String>::from_rpc(Some(Value::Bool(false))).unwrap_err();
        assert!(
            matches!(err, ArgError::Mismatch { got: "boolean" }),
            "{err:?}"
        );
        assert_eq!(
            message::<Nullable<String>>("field_name", err),
            "argument 'field_name': expected a string or null, got boolean"
        );
    }

    /// `Defaulted` refuses `null` itself, whatever `T` accepts (D-T1): `Option<u64>` takes `null`
    /// as `None`, `Defaulted<Option<u64>>` does not.
    #[test]
    fn test_defaulted_refuses_null_whatever_t_accepts() {
        let err = Defaulted::<Option<u64>>::from_rpc(Some(Value::False)).unwrap_err();
        assert!(matches!(err, ArgError::Mismatch { got: "null" }), "{err:?}");
        assert_eq!(
            message::<Defaulted<Option<u64>>>("offset", err),
            "argument 'offset': expected a non-negative integer, got null"
        );
    }

    /// `OrNull::EXPECTED_OR_NULL` is `T::EXPECTED` plus " or null" for every impl (D-T3).
    #[test]
    fn test_or_null_extends_expected() {
        use super::args::OrNull;
        fn check<T: OrNull>() {
            assert_eq!(
                T::EXPECTED_OR_NULL,
                format!("{} or null", T::EXPECTED),
                "{}",
                std::any::type_name::<T>()
            );
        }
        check::<u64>();
        check::<String>();
    }

    #[test]
    fn test_defaulted_args() {
        // offset: absent is the binder's default; `null` and `false` are refused by `i64`
        let offset = |v| Defaulted::<i64>::from_rpc(v);
        assert_eq!(offset(None).unwrap().or(0), 0);
        assert_eq!(offset(Some(Value::Int(15))).unwrap().or(0), 15);
        let err = offset(Some(Value::False)).unwrap_err();
        assert!(matches!(err, ArgError::Mismatch { got: "null" }), "{err:?}");
        assert_eq!(
            message::<Defaulted<i64>>("offset", err),
            "argument 'offset': expected an integer, got null"
        );
        let err = offset(Some(Value::Bool(false))).unwrap_err();
        assert!(
            matches!(err, ArgError::Mismatch { got: "boolean" }),
            "{err:?}"
        );
        // operator: absent is the binder's default; `null` is no operator
        let operator =
            |v| Defaulted::<OperatorArg>::from_rpc(v).map(|d| d.or(OperatorArg(Operator::ILike)).0);
        assert_eq!(operator(None).unwrap(), Operator::ILike);
        assert_eq!(operator(Some(Value::str("="))).unwrap(), Operator::Eq);
        let err = operator(Some(Value::False)).unwrap_err();
        assert!(matches!(err, ArgError::Mismatch { got: "null" }), "{err:?}");
        assert_eq!(
            message::<Defaulted<OperatorArg>>("operator", err),
            "argument 'operator': expected a domain operator, got null"
        );
        assert!(matches!(
            operator(Some(Value::Bool(false))),
            Err(ArgError::Mismatch { got: "boolean" })
        ));
    }

    fn val_map<const N: usize>(entries: [(&str, Value); N]) -> Value {
        Value::Map(Arc::new(
            entries
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        ))
    }

    #[test]
    fn test_groupby_spec_arg() {
        // 3
        assert!(matches!(
            GroupbySpecArg::from_rpc(Some(Value::Int(3))),
            Err(ArgError::Mismatch { got: "integer" })
        ));
        // {"f": 1}
        let bad_map = val_map([("f", Value::Int(1))]);
        match GroupbySpecArg::from_rpc(Some(bad_map)) {
            Err(ArgError::Invalid(e)) => {
                assert!(e.message().contains("groupby_read_specification['f']"));
            }
            other => panic!("expected ArgError::Invalid, got {other:?}"),
        }
        // valid object
        let valid_map = val_map([("f", val_map([("fields", val_map([]))]))]);
        let parsed = GroupbySpecArg::from_rpc(Some(valid_map)).unwrap().0;
        assert!(parsed.is_some());
        assert!(parsed.unwrap().contains_key("f"));
    }

    #[test]
    fn test_progress_bar_arg() {
        // {"field": 1}
        let bad_field = val_map([("field", Value::Int(1))]);
        match ProgressBarArg::from_rpc(Some(bad_field)) {
            Err(ArgError::Invalid(e)) => {
                assert_eq!(e.message(), "'field' must be a string, got integer");
            }
            other => panic!("expected ArgError::Invalid, got {other:?}"),
        }
        // {"field": "login"} (no colors)
        let no_colors = val_map([("field", Value::str("login"))]);
        match ProgressBarArg::from_rpc(Some(no_colors)) {
            Err(ArgError::Invalid(e)) => {
                assert_eq!(e.message(), "'colors' must be an object, got null");
            }
            other => panic!("expected ArgError::Invalid, got {other:?}"),
        }
        // {"colors": []} (colors not an object, missing field)
        let bad_colors = val_map([("colors", Value::list([]))]);
        match ProgressBarArg::from_rpc(Some(bad_colors)) {
            Err(ArgError::Invalid(e)) => {
                assert_eq!(e.message(), "'field' must be a string, got null");
            }
            other => panic!("expected ArgError::Invalid, got {other:?}"),
        }
    }

    #[test]
    fn test_opening_info_arg() {
        // non-object element in list: ["oops", 1]
        let bad_items = Value::list([Value::str("oops"), Value::Int(1)]);
        assert!(matches!(
            OpeningInfoArg::from_rpc(Some(bad_items)),
            Err(ArgError::Mismatch { got: "string" })
        ));
        // valid array of objects
        let valid_items = Value::list([val_map([])]);
        let parsed = OpeningInfoArg::from_rpc(Some(valid_items)).unwrap().0;
        assert_eq!(parsed.unwrap().len(), 1);
    }

    /// Each binder type's shape follows R2, and the enum shapes
    /// list exactly the names the binders accept.
    #[test]
    fn binder_shapes_follow_the_table() {
        use super::args::{OPERATION_NAMES, OPERATOR_NAMES};
        assert_eq!(bool::SHAPE, ArgShape::Boolean);
        assert_eq!(i64::SHAPE, ArgShape::Integer { min: None });
        assert_eq!(u64::SHAPE, ArgShape::Integer { min: Some(0) });
        assert_eq!(f64::SHAPE, ArgShape::Number);
        assert_eq!(String::SHAPE, ArgShape::String);
        assert_eq!(Domain::SHAPE, ArgShape::Domain);
        assert_eq!(
            <Option<Vec<u64>>>::SHAPE,
            ArgShape::OrNull(&ArgShape::Array(&ArgShape::Integer { min: Some(0) }))
        );
        assert!(<Option<Vec<u64>>>::SHAPE.is_closed());
        assert_eq!(
            Nullable::<String>::SHAPE,
            ArgShape::OrNull(&ArgShape::String)
        );
        assert_eq!(Defaulted::<u64>::SHAPE, u64::SHAPE);
        assert_eq!(
            SearchPanelLimitArg::SHAPE,
            ArgShape::OrNull(&ArgShape::Integer { min: Some(0) })
        );
        assert_eq!(OperatorArg::SHAPE, ArgShape::Enum(&OPERATOR_NAMES));
        assert_eq!(OperationArg::SHAPE, ArgShape::Enum(&OPERATION_NAMES));
        for name in OPERATOR_NAMES {
            assert!(
                OperatorArg::from_rpc(Some(Value::str(name))).is_ok(),
                "{name}"
            );
        }
        assert_eq!(OPERATOR_NAMES.len(), Operator::ALL.len());
        for name in OPERATION_NAMES {
            assert!(
                OperationArg::from_rpc(Some(Value::str(name))).is_ok(),
                "{name}"
            );
        }
        assert_eq!(OPERATION_NAMES.len(), Operation::ALL.len());
        for (shape, reason) in [
            (Value::SHAPE, "any value"),
            (ValueMap::SHAPE, "an object with open keys"),
            (Spec::SHAPE, "a read specification"),
            (super::super::Binary::SHAPE, "binary data"),
            (LoadArg::SHAPE, "a load mode"),
            (ValsArg::SHAPE, "field values"),
            (GroupbySpecArg::SHAPE, "a read specification mapping"),
            (OpeningInfoArg::SHAPE, "group opening state"),
            (ProgressBarArg::SHAPE, "a progress bar specification"),
            (<Vec<Value>>::SHAPE, "any value"),
            (<Option<ValueMap>>::SHAPE, "an object with open keys"),
        ] {
            assert_eq!(shape.open_reason(), Some(reason), "{shape:?}");
            assert!(!shape.is_closed());
        }
    }
}
