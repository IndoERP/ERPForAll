//! `<function>`.
//!
//! The call goes through the data-callable level of the RPC machinery (
//! decision 3): only a method whose chain declares `#[data_callable]` or `#[rpc]` can be named,
//! and its arguments bind to the declared parameters by the same typed binder as a JSON-2 call.
//!
//! A nested `<function>` runs
//! first and its result is the argument (a recordset result is its ids). For a records method the
//! first positional argument is the ids (falsy ids dropped, a scalar accepted); a model method
//! takes all of them as parameters. A `context` keyword is merged into the call's context and is
//! not passed on. The loader then restores its authoritative source module after that authored
//! merge; a function cannot replace lifecycle provenance through its keyword context.
//!
//! The operation is skipped in update mode under an effective `noupdate`:
//! that is the specified behaviour, not a failure, so it neither errors nor reports.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use erp_expr::ExprValue;
use erp_fields::{Value, ValueMap};
use erp_orm::Transaction;
use erp_orm::rpc::{BindError, DataCallable, Receiver, RpcArgs};

use super::error::OpLabel;
use super::expr::{Known, to_value};
use super::{FileContext, LoadError, LoadErrorKind, LoadSession, env, values_ext};
use crate::ast::{FunctionArg, FunctionOp, ValueOp};
use crate::reader::ConvertMode;

type Step<'a, T> = Pin<Box<dyn Future<Output = Result<T, LoadErrorKind>> + Send + 'a>>;

/// Call a data-callable method.
pub(crate) async fn load(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: &FunctionOp,
) -> Result<(), LoadError> {
    // a noupdate <function> runs on install only
    if file.mode() == ConvertMode::Update && op.env().noupdate() {
        return Ok(());
    }
    let label = OpLabel::new(format!(
        "<function model=\"{}\" name=\"{}\">",
        op.model(),
        op.name()
    ));
    let mut known: HashMap<String, Known> = HashMap::new();
    let result = async {
        let env = env::operation_env(session, tx, file, op.env(), &mut known).await?;
        call(session, tx, file, op, &env, &mut known).await
    }
    .await;
    // the method may have changed `ir.model.data` behind the cache
    session.clear_cache();
    result
        .map(|_| ())
        .map_err(|kind| LoadError::at(op.source_file(), op.line(), label, kind))
}

/// Evaluate the arguments of `op` and call its method; the result as a dynamic value.
fn call<'a>(
    session: &'a mut LoadSession,
    tx: &'a mut Transaction,
    file: &'a FileContext,
    op: &'a FunctionOp,
    env: &'a erp_orm::Env,
    known: &'a mut HashMap<String, Known>,
) -> Step<'a, Value> {
    Box::pin(async move {
        let call_name = format!("{}.{}", op.model(), op.name());
        let invalid = |message: String| LoadErrorKind::FunctionArguments {
            call: call_name.clone(),
            message,
        };
        let registry = env.registry();
        let Some(model_id) = registry.model_id(op.model()) else {
            return Err(LoadErrorKind::ModelMissing {
                what: "<function>".to_owned(),
                model: op.model().to_owned(),
            });
        };
        let Some(callable) = tx.data_callable(model_id, op.name()).cloned() else {
            return Err(if tx.has_method(model_id, op.name()) {
                LoadErrorKind::NotDataCallable {
                    model: op.model().to_owned(),
                    method: op.name().to_owned(),
                }
            } else {
                LoadErrorKind::UnknownMethod {
                    model: op.model().to_owned(),
                    method: op.name().to_owned(),
                }
            });
        };

        // positional arguments: `eval` first, then the children in order
        let module = file.module();
        let mut positional: Vec<Value> = Vec::new();
        if let Some(expr) = op.eval() {
            let list = session
                .eval_with_model(tx, module, expr, Some(op.model()), known)
                .await?;
            let (ExprValue::List(items) | ExprValue::Tuple(items)) = &list else {
                return Err(invalid(format!(
                    "eval \"{expr}\" gave a {}, not a list or tuple of arguments",
                    list.type_name()
                )));
            };
            for item in items.iter() {
                positional.push(argument(item, &call_name)?);
            }
        }
        for arg in op.args() {
            let value = match arg {
                FunctionArg::Value(v) => {
                    value_of(session, tx, file, env, v, known, &call_name).await?
                }
                FunctionArg::Function(nested) => {
                    call(session, tx, file, nested, env, known).await?
                }
            };
            positional.push(value);
        }
        let mut keywords: Vec<(&str, Value)> = Vec::new();
        let mut context: Option<ValueMap> = None;
        for (name, value) in op.kwargs() {
            let value = value_of(session, tx, file, env, value, known, &call_name).await?;
            if name == "context" {
                // merged into the call's context, not passed on
                let Value::Map(map) = value else {
                    return Err(invalid(format!(
                        "the 'context' keyword must be a dict, got {}",
                        describe(&value)
                    )));
                };
                context = Some((*map).clone());
            } else {
                keywords.push((name.as_str(), value));
            }
        }

        // the receiver: the ids for a records method, an empty set for a model method
        let mut records = env.records(op.model()).map_err(LoadErrorKind::Orm)?;
        if let Some(map) = context {
            records = records.with_context(map);
        }
        // Authored kwargs are data, not lifecycle authority. Nested calls take this same path;
        // restore the actual file module after every merge, preserving ordinary context keys.
        records = records.with_context([(super::load_state::DECLARING_MODULE, Value::str(module))]);
        if callable.receiver() == Receiver::Records {
            if positional.is_empty() {
                return Err(invalid(
                    "a records method takes the record ids as its first argument".to_owned(),
                ));
            }
            let ids = record_ids(positional.remove(0)).map_err(&invalid)?;
            records = records.browse(ids).map_err(LoadErrorKind::Orm)?;
        }

        let args = bind(&callable, positional, keywords, &call_name)?;
        match callable.call(tx, records, args).await {
            Ok(value) => Ok(normalize(value)),
            Err(BindError::Call(error)) => Err(LoadErrorKind::Orm(error)),
            // an argument the binder refuses: its own text names the parameter
            Err(refusal) => Err(invalid(refusal.to_string())),
        }
    })
}

/// Positional and keyword arguments as the binder's named map (Python's call binding: positional
/// arguments fill the parameters in order, a keyword must name a parameter no positional filled).
fn bind(
    callable: &DataCallable,
    positional: Vec<Value>,
    keywords: Vec<(&str, Value)>,
    call: &str,
) -> Result<RpcArgs, LoadErrorKind> {
    let invalid = |message: String| LoadErrorKind::FunctionArguments {
        call: call.to_owned(),
        message,
    };
    let params = callable.params();
    if positional.len() > params.len() {
        let (n, given) = (params.len(), positional.len());
        return Err(invalid(format!(
            "takes {n} positional argument{} but {given} {} given",
            if n == 1 { "" } else { "s" },
            if given == 1 { "was" } else { "were" }
        )));
    }
    let mut values = ValueMap::new();
    for (spec, value) in params.iter().zip(positional) {
        values.insert(spec.name().to_owned(), value);
    }
    for (name, value) in keywords {
        if !params.iter().any(|p| p.name() == name) {
            return Err(invalid(format!(
                "got an unexpected keyword argument '{name}'"
            )));
        }
        if values.contains_key(name) {
            return Err(invalid(format!(
                "got multiple values for argument '{name}'"
            )));
        }
        values.insert(name.to_owned(), value);
    }
    Ok(RpcArgs::new(values))
}

/// The ids a records method runs on: a list of ids with the falsy ones dropped, or one id
/// (a falsy scalar is no record).
fn record_ids(first: Value) -> Result<Vec<i64>, String> {
    let to_id = |value: &Value| match value {
        Value::Int(i) => Ok(*i),
        other => Err(format!(
            "record ids must be integers, got {}",
            describe(other)
        )),
    };
    let is_falsy =
        |value: &Value| matches!(value, Value::False | Value::Bool(false) | Value::Int(0));
    match &first {
        Value::List(items) => items.iter().filter(|v| !is_falsy(v)).map(to_id).collect(),
        scalar if is_falsy(scalar) => Ok(Vec::new()),
        scalar => Ok(vec![to_id(scalar)?]),
    }
}

/// An expression result as an argument.
fn argument(value: &ExprValue, call: &str) -> Result<Value, LoadErrorKind> {
    to_value(value).map_err(|got| LoadErrorKind::FunctionArguments {
        call: call.to_owned(),
        message: format!("an argument is {got}, which is not a value a method can take"),
    })
}

/// The value of a `<value>` child. An `eval` is converted here, so that a result a method cannot
/// take names the call; every other form (`search=`, `type="file"`, `file=`, numbers, empty
/// lists) is the code `<field>` uses ([`super::values_ext::value_op`]).
async fn value_of(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    env: &erp_orm::Env,
    op: &ValueOp,
    known: &mut HashMap<String, Known>,
    call: &str,
) -> Result<Value, LoadErrorKind> {
    if let ValueOp::Eval { expr, model } = op {
        let result = session
            .eval_with_model(tx, file.module(), expr, model.as_deref(), known)
            .await?;
        return argument(&result, call);
    }
    values_ext::value_op(session, tx, file, env, op, known).await
}

/// A method that returns nothing gives JSON `null`; as an argument it is Python's `None`.
fn normalize(value: Value) -> Value {
    match &value {
        Value::Json(json) if json.is_null() => Value::False,
        _ => value,
    }
}

fn describe(value: &Value) -> String {
    format!("the value {value}")
}
