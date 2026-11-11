use crate::args::{Sig, int_arg, required};
use crate::error::{ExprError, ExprResult};
use crate::limits::Budget;
use crate::value::{ExprObject, ExprValue, Kwargs, SLOT};

use super::builtin;

/// The `Command` object of the standard context.
#[derive(Debug)]
pub(crate) struct CommandNamespace;

impl ExprObject for CommandNamespace {
    fn type_name(&self) -> &str {
        "Command"
    }

    fn repr(&self) -> String {
        "<enum 'Command'>".to_string()
    }

    fn get_attr(&self, name: &str, _budget: &mut Budget) -> ExprResult<ExprValue> {
        Ok(match name {
            "CREATE" => ExprValue::Int(0),
            "UPDATE" => ExprValue::Int(1),
            "DELETE" => ExprValue::Int(2),
            "UNLINK" => ExprValue::Int(3),
            "LINK" => ExprValue::Int(4),
            "CLEAR" => ExprValue::Int(5),
            "SET" => ExprValue::Int(6),
            "create" => builtin("create", create),
            "update" => builtin("update", update),
            "delete" => builtin("delete", delete),
            "unlink" => builtin("unlink", unlink),
            "link" => builtin("link", link),
            "clear" => builtin("clear", clear),
            "set" => builtin("set", set),
            _ => return Err(ExprError::type_attribute_error("Command", name)),
        })
    }

    fn self_charging(&self) -> bool {
        true
    }
}

/// `(code, id, value)`; the value was charged when it was produced, the triple charges its slots.
fn triple(code: i64, id: i64, value: ExprValue, budget: &mut Budget) -> ExprResult<ExprValue> {
    budget.alloc(SLOT * 4)?;
    let t = ExprValue::tuple(vec![ExprValue::Int(code), ExprValue::Int(id), value]);
    budget.check_value(&t)?;
    Ok(t)
}

fn values_arg(fname: &str, v: &ExprValue, budget: &mut Budget) -> ExprResult<ExprValue> {
    match v {
        ExprValue::Dict(_) => {
            budget.admit(v)?;
            Ok(v.clone())
        }
        other => Err(ExprError::type_error(format!(
            "{fname}() argument 'values' must be dict, not {}",
            other.type_name()
        ))),
    }
}

/// Command builders take named parameters, so CPython's function messages apply
/// (`Command.link() missing 1 required positional argument: 'id'`).
fn sig(name: &'static str, params: &'static [&'static str]) -> Sig {
    Sig {
        name,
        params,
        required: params.len(),
        pos_only: 0,
        max_pos: params.len(),
    }
}

fn create(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    let s = sig("Command.create", &["values"]);
    let slots = s.bind(args, kwargs)?;
    let values = values_arg("Command.create", required(&s, &slots, 0)?, budget)?;
    triple(0, 0, values, budget)
}

fn update(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    let s = sig("Command.update", &["id", "values"]);
    let slots = s.bind(args, kwargs)?;
    let id = int_arg("Command.update", "id", required(&s, &slots, 0)?)?;
    let values = values_arg("Command.update", required(&s, &slots, 1)?, budget)?;
    triple(1, id, values, budget)
}

fn id_command(
    code: i64,
    name: &'static str,
    args: &[ExprValue],
    kwargs: &Kwargs,
    budget: &mut Budget,
) -> ExprResult<ExprValue> {
    let s = sig(name, &["id"]);
    let slots = s.bind(args, kwargs)?;
    let id = int_arg(name, "id", required(&s, &slots, 0)?)?;
    triple(code, id, ExprValue::Int(0), budget)
}

fn delete(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    id_command(2, "Command.delete", args, kwargs, budget)
}

fn unlink(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    id_command(3, "Command.unlink", args, kwargs, budget)
}

fn link(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    id_command(4, "Command.link", args, kwargs, budget)
}

fn clear(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    sig("Command.clear", &[]).bind(args, kwargs)?;
    triple(5, 0, ExprValue::Int(0), budget)
}

fn set(args: &[ExprValue], kwargs: &Kwargs, budget: &mut Budget) -> ExprResult<ExprValue> {
    let s = sig("Command.set", &["ids"]);
    let slots = s.bind(args, kwargs)?;
    let ids_value = required(&s, &slots, 0)?;
    let ids = match ids_value {
        ExprValue::List(items) | ExprValue::Tuple(items) => items,
        other => {
            return Err(ExprError::type_error(format!(
                "Command.set() argument 'ids' must be a list or tuple of ids, not {}",
                other.type_name()
            )));
        }
    };
    for (i, item) in ids.iter().enumerate() {
        if item.as_int().is_none() {
            return Err(ExprError::type_error(format!(
                "Command.set() id #{i} must be int, not {}",
                item.type_name()
            )));
        }
    }
    budget.admit(ids_value)?;
    triple(6, 0, ids_value.clone(), budget)
}
