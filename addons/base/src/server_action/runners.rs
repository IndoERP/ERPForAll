//! The runners `base` declares: `object_write`, `object_create`, `multi`, and the states whose
//! runner is a later task (`code`, `object_copy`, `webhook`).

use std::borrow::Cow;
use std::str::FromStr;

use erp_sdk::prelude::*;
use erp_sdk::{BoxFuture, Date, DateTime, FieldType, Registry};

use super::error::excerpt;
use super::registry::{RunnerCtx, RunnerDecl};
use super::tree::{EvaluationType, M2mOperation, ServerAction};
use super::{ActionResult, ServerActionError};

/// `object_write`: write the action's value on `update_path` of every active record, in one
/// ORM `write` as the caller.
pub static OBJECT_WRITE: RunnerDecl = RunnerDecl::__declare(
    &crate::ADDON,
    "object_write",
    true,
    object_write,
    file!(),
    line!(),
);

/// `object_create`: create `{rec_name: value}` in the action's target model as the caller,
/// then link it from the active record (`_run_action_object_create`).
pub static OBJECT_CREATE: RunnerDecl = RunnerDecl::__declare(
    &crate::ADDON,
    "object_create",
    false,
    object_create,
    file!(),
    line!(),
);

/// `multi`: run the children in order, each as the caller (`_run_action_multi`).
pub static MULTI: RunnerDecl =
    RunnerDecl::__declare(&crate::ADDON, "multi", true, multi, file!(), line!());

/// `code`: Luau on `erp-script`,.
pub static CODE: RunnerDecl =
    RunnerDecl::__declare(&crate::ADDON, "code", true, code, file!(), line!());

/// `object_copy`: needs `copy` over JSON-2,.
pub static OBJECT_COPY: RunnerDecl = RunnerDecl::__declare(
    &crate::ADDON,
    "object_copy",
    true,
    object_copy,
    file!(),
    line!(),
);

/// `webhook`: post-commit delivery,.
pub static WEBHOOK: RunnerDecl =
    RunnerDecl::__declare(&crate::ADDON, "webhook", true, webhook, file!(), line!());

/// `base`'s runners. They are listed here rather than placed in [`super::SERVER_ACTION_RUNNERS`]:
/// a link-time element declared in the crate that declares the slice needs `#[link_section]`
/// in this crate, which the workspace's `unsafe_code = "forbid"` refuses (the ORM lists its
/// built-in methods the same way, `erp_orm::dispatch`).
pub(crate) static BUILTIN: [&RunnerDecl; 6] = [
    &OBJECT_WRITE,
    &OBJECT_CREATE,
    &MULTI,
    &CODE,
    &OBJECT_COPY,
    &WEBHOOK,
];

fn unsupported(
    ctx: &RunnerCtx<'_>,
    task: &'static str,
) -> BoxFuture<'static, Result<Option<ActionResult>>> {
    let error = ServerActionError::Unsupported {
        feature: format!("Server actions of type '{}'", ctx.action().state()),
        task,
    };
    Box::pin(async move { Err(error.into()) })
}

fn code(mut ctx: RunnerCtx<'_>) -> BoxFuture<'_, Result<Option<ActionResult>>> {
    Box::pin(async move {
        let source = ctx.action().code.clone().unwrap_or_default();
        let env = ctx.env().clone();
        let records = ctx.records().clone();
        let output = super::script::evaluate(ctx.tx(), &env, &records, source).await?;
        super::script::action_result(output)
    })
}

fn object_copy(ctx: RunnerCtx<'_>) -> BoxFuture<'_, Result<Option<ActionResult>>> {
    unsupported(&ctx, "a later release")
}

fn webhook(ctx: RunnerCtx<'_>) -> BoxFuture<'_, Result<Option<ActionResult>>> {
    unsupported(&ctx, "a later release")
}

fn multi(mut ctx: RunnerCtx<'_>) -> BoxFuture<'_, Result<Option<ActionResult>>> {
    Box::pin(async move { ctx.run_children().await })
}

fn path_error(action: &ServerAction, path: &str, reason: impl Into<String>) -> ServerActionError {
    ServerActionError::UpdatePath {
        name: action.name().to_owned(),
        path: excerpt(path),
        reason: reason.into(),
    }
}

pub(crate) fn resolve_path<'r>(
    registry: &'r Registry,
    action: &ServerAction,
    path: &str,
) -> Result<(&'r str, &'r str), ServerActionError> {
    let mut steps = super::path::steps(path)?.peekable();
    let mut model = registry
        .model(action.model())
        .ok_or_else(|| action.misconfigured("its model is not in the registry"))?;
    while let Some(step) = steps.next() {
        let field = model.field(step).ok_or_else(|| {
            path_error(
                action,
                path,
                format!(
                    "names '{}', which {} does not have",
                    excerpt(step),
                    model.name
                ),
            )
        })?;
        if steps.peek().is_none() {
            return Ok((model.name, field.name));
        }
        let comodel = match &field.ty {
            FieldType::Many2one { comodel, .. }
            | FieldType::One2many { comodel, .. }
            | FieldType::Many2many { comodel, .. } => comodel,
            other => {
                return Err(path_error(
                    action,
                    path,
                    format!(
                        "goes through {}.{}, a {} field: only the last step may be a field that \
                         is not relational",
                        model.name,
                        field.name,
                        other.type_name()
                    ),
                ));
            }
        };
        model = registry.model(comodel).ok_or_else(|| {
            path_error(
                action,
                path,
                format!("leads to '{comodel}', which is not in the registry"),
            )
        })?;
    }
    // `split` yields at least one step, so the loop returns
    Err(path_error(action, path, "is empty"))
}

/// A stored selection `check_tree` refuses when empty, found empty by the runner: an internal
/// invariant broke (the tree was not checked, or the check and the runner disagree).
fn unchecked(action: &ServerAction, what: &str) -> Error {
    Error::programming(format!(
        "server action {} ({}) reached the object_write runner with an empty {what}, which \
         the check before the run refuses",
        action.id(),
        action.name()
    ))
}

/// What `object_write` writes on the field.
enum Write {
    Cell(Cell),
    Commands(Vec<Command>),
}

fn convert(action: &ServerAction, model: &str, field: &str, ty: &FieldType) -> Result<Write> {
    let value = action.value();
    let qualified = || format!("{model}.{field}");
    let bad = |expected: &'static str| ServerActionError::BadValue {
        name: action.name().to_owned(),
        field: qualified(),
        value: excerpt(value.unwrap_or("")),
        expected,
    };
    let text = || value.map_or(Cell::Null, |v| Cell::Str(Arc::from(v)));
    let number_text = || value.map(str::trim).unwrap_or("0");
    let record_id = || -> Result<RecId, ServerActionError> {
        value
            .map(str::trim)
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(RecId::from_real)
            .ok_or_else(|| bad("record id"))
    };
    Ok(Write::Cell(match ty {
        FieldType::Char { .. }
        | FieldType::Selection { .. }
        | FieldType::Reference { .. }
        | FieldType::Text => text(),
        FieldType::Html => {
            return Err(ServerActionError::Unsupported {
                feature: format!(
                    "Update Record of the html field {} (from html_value)",
                    qualified()
                ),
                task: "a later release",
            }
            .into());
        }
        FieldType::Integer => Cell::Int(number_text().parse::<i32>().map_err(|_| bad("integer"))?),
        FieldType::Float { digits: None } => {
            let f = number_text().parse::<f64>().map_err(|_| bad("number"))?;
            if !f.is_finite() {
                return Err(bad("number").into());
            }
            Cell::Float(f)
        }
        FieldType::Float { digits: Some(_) } | FieldType::Monetary { .. } => {
            Cell::Decimal(erp_sdk::Decimal::from_str(number_text()).map_err(|_| bad("number"))?)
        }
        // `update_boolean_value == 'true'` (`:1349`): an absent value is false there too; a
        // stored value outside the selection was refused before the run
        FieldType::Boolean => Cell::Bool(action.boolean_value == Some(true)),
        FieldType::Date => match value {
            None => Cell::Null,
            Some(v) => Cell::Date(Date::parse_odoo(v.trim()).ok_or_else(|| bad("date"))?),
        },
        FieldType::Datetime => match value {
            None => Cell::Null,
            Some(v) => {
                Cell::DateTime(DateTime::parse_odoo(v.trim()).ok_or_else(|| bad("datetime"))?)
            }
        },
        // a database id; 0 or no value clears it (`:1353-1356`); a negative number would name a
        // new record, so it is no id here
        FieldType::Many2one { .. } => match number_text().parse::<i64>() {
            Ok(0) => Cell::Null,
            Ok(id) => Cell::Id(RecId::from_real(id).ok_or_else(|| bad("record id"))?.get()),
            Err(_) => return Err(bad("record id").into()),
        },
        FieldType::One2many { .. } | FieldType::Many2many { .. } => {
            let commands = match action.m2m_operation {
                Some(M2mOperation::Add) => vec![Command::link(record_id()?)],
                Some(M2mOperation::Remove) => vec![Command::unlink(record_id()?)],
                Some(M2mOperation::Set) => vec![Command::set([record_id()?])],
                Some(M2mOperation::Clear) => vec![Command::clear()],
                // `check_tree` refuses an empty operation on an x2many field before the run
                None => return Err(unchecked(action, "many2many operation")),
            };
            return Ok(Write::Commands(commands));
        }
        FieldType::Id | FieldType::RowVersion => {
            return Err(ServerActionError::SystemField {
                name: action.name().to_owned(),
                field: qualified(),
            }
            .into());
        }
        FieldType::Uuid | FieldType::Xid8 | FieldType::TimestampTz => {
            return Err(ServerActionError::Unsupported {
                feature: format!(
                    "Update Record of native {} field {}",
                    ty.type_name(),
                    qualified()
                ),
                task: "the future native server-action field slice",
            }
            .into());
        }
        FieldType::Json
        | FieldType::Many2oneReference { .. }
        | FieldType::DateRange { .. }
        | FieldType::DatetimeRange { .. } => {
            return Err(ServerActionError::FieldType {
                name: action.name().to_owned(),
                field: qualified(),
                type_name: ty.type_name(),
            }
            .into());
        }
    }))
}

fn object_write(mut ctx: RunnerCtx<'_>) -> BoxFuture<'_, Result<Option<ActionResult>>> {
    Box::pin(async move {
        let action = ctx.action();
        let records = ctx.records();
        let registry = Arc::clone(ctx.env().registry());
        match action.evaluation_type {
            Some(EvaluationType::Value | EvaluationType::Equation) => {}
            Some(EvaluationType::Sequence) => {
                return Err(ServerActionError::Unsupported {
                    feature: "Update Record from a sequence (evaluation type 'sequence')"
                        .to_owned(),
                    task: "secured base sequence dispatch",
                }
                .into());
            }
            // `check_tree` refuses an empty value type before the run
            None => return Err(unchecked(action, "value type")),
        }
        let Some(path) = action.update_path.as_deref().filter(|p| !p.is_empty()) else {
            return Err(action.misconfigured("it has no field to update").into());
        };
        let (model, field) = resolve_path(&registry, action, path)?;
        if let Some((uf_model, uf_name)) = &action.update_field
            && (uf_model != model || uf_name != field)
        {
            return Err(path_error(
                action,
                path,
                format!("ends on {model}.{field}, but its field to update is {uf_model}.{uf_name}"),
            )
            .into());
        }
        let spec = registry
            .model(model)
            .and_then(|m| m.field(field))
            .ok_or_else(|| path_error(action, path, "does not resolve"))?;
        if action.evaluation_type == Some(EvaluationType::Equation) {
            if matches!(spec.ty, FieldType::Html) {
                return Err(ServerActionError::Unsupported {
                    feature: format!(
                        "Update Record of the html field {model}.{field} (from html_value)"
                    ),
                    task: "a later release",
                }
                .into());
            }
            if matches!(spec.ty, FieldType::Id | FieldType::RowVersion) {
                return Err(ServerActionError::SystemField {
                    name: action.name().to_owned(),
                    field: format!("{model}.{field}"),
                }
                .into());
            }
            super::script::check_active(records)?;
            let expression = action
                .value()
                .ok_or_else(|| action.misconfigured("its equation is empty"))?;
            let source = format!("return ({expression})");
            let env = ctx.env().clone();
            for index in 0..records.len() {
                let one = records.slice(
                    Some(
                        isize::try_from(index)
                            .map_err(|_| Error::validation("Too many script records"))?,
                    ),
                    Some(
                        isize::try_from(index + 1)
                            .map_err(|_| Error::validation("Too many script records"))?,
                    ),
                    1,
                )?;
                let id = one.ids()[0];
                let single_env = env.with_context([
                    ("active_id", Value::Int(id)),
                    ("active_ids", Value::list([Value::Int(id)])),
                ]);
                let one = one.with_env(single_env.clone());
                let output =
                    super::script::evaluate(ctx.tx(), &single_env, &one, source.clone()).await?;
                if output.captured.get("action").is_some_and(|a| !a.is_nil()) {
                    return Err(ServerActionError::AmbiguousResult.into());
                }
                let value = super::script::equation_value(output.value)?;
                let targets = match path.rsplit_once('.') {
                    None => one,
                    Some((prefix, _)) => one
                        .mapped_path(ctx.tx(), prefix)
                        .await?
                        .into_records()
                        .ok_or_else(|| {
                            path_error(action, path, "does not end its prefix on records")
                        })?,
                };
                let field_name = field.to_owned();
                let values: erp_sdk::ValueMap = [(field_name, value)].into_iter().collect();
                let vals = super::script::checked_values(&targets, &values)?;
                targets.write(ctx.tx(), vals).await?;
            }
            return Ok(None);
        }
        let write = convert(action, model, field, &spec.ty)?;
        let targets = match path.rsplit_once('.') {
            None => records.clone(),
            Some((prefix, _)) => records
                .mapped_path(ctx.tx(), prefix)
                .await?
                .into_records()
                .ok_or_else(|| path_error(action, path, "does not end its prefix on records"))?,
        };
        let name: Cow<'static, str> = Cow::Owned(field.to_owned());
        let vals = match write {
            Write::Cell(cell) => RawVals::new().set_cell(name, cell),
            Write::Commands(commands) => RawVals::new().set_commands(name, commands),
        };
        targets.write(ctx.tx(), vals).await?;
        Ok(None)
    })
}

fn object_create(mut ctx: RunnerCtx<'_>) -> BoxFuture<'_, Result<Option<ActionResult>>> {
    Box::pin(async move {
        let action = ctx.action();
        let env = ctx.env();
        let registry = Arc::clone(env.registry());
        let target = match &action.crud_model {
            Some(name) => registry.model(name).ok_or_else(|| {
                action.misconfigured(format!(
                    "the model to create in, '{}', is not in the registry",
                    excerpt(name)
                ))
            })?,
            None => registry
                .model(action.model())
                .ok_or_else(|| action.misconfigured("its model is not in the registry"))?,
        };
        let rec_name = target
            .rec_name
            .map(|id| target.field_by_id(id).name)
            .ok_or_else(|| ServerActionError::NoRecName {
                model: target.name.to_owned(),
            })?;
        // the link field is checked before anything is created (review R-2)
        let link = match &action.link_field {
            None => None,
            Some((link_model, link_name)) => Some(link_field(
                &registry,
                action,
                link_model,
                link_name,
                target.name,
            )?),
        };
        let value = action
            .value()
            .map_or(Cell::Null, |v| Cell::Str(Arc::from(v)));
        let created = env
            .records(target.name)?
            .create(ctx.tx(), vec![RawVals::new().set_cell(rec_name, value)])
            .await?;
        let Some((link_name, is_many2one)) = link else {
            return Ok(None);
        };
        let id = created
            .ids()
            .first()
            .copied()
            .and_then(RecId::from_real)
            .ok_or_else(|| {
                Error::programming("create returned no database record for object_create")
            })?;
        let vals = if is_many2one {
            RawVals::new().set_cell(link_name, Cell::Id(id.get()))
        } else {
            // link the new record (`:1128-1129`)
            RawVals::new().set_commands(link_name, [Command::link(id)])
        };
        ctx.records().write(ctx.tx(), vals).await?;
        Ok(None)
    })
}

/// The link field of an `object_create`: a relational field of the action's model pointing to
/// the created model. Its name, and whether it is a many2one (else an x2many).
fn link_field(
    registry: &Registry,
    action: &ServerAction,
    link_model: &str,
    link_name: &str,
    target: &str,
) -> Result<(&'static str, bool), ServerActionError> {
    let link_error = |reason: String| ServerActionError::LinkField {
        name: action.name().to_owned(),
        reason,
    };
    let link = registry
        .model(action.model())
        .filter(|m| m.name == link_model)
        .and_then(|m| m.field(link_name))
        .ok_or_else(|| {
            link_error(format!(
                "{}.{} is not a field of {}",
                excerpt(link_model),
                excerpt(link_name),
                action.model()
            ))
        })?;
    let is_many2one = match &link.ty {
        FieldType::Many2one { .. } => true,
        FieldType::One2many { .. } | FieldType::Many2many { .. } => false,
        other => {
            return Err(link_error(format!(
                "{}.{} is a {} field, not a relational one",
                action.model(),
                link.name,
                other.type_name()
            )));
        }
    };
    if link.ty.comodel() != Some(target) {
        return Err(link_error(format!(
            "{}.{} does not point to {target}",
            action.model(),
            link.name
        )));
    }
    Ok((link.name, is_many2one))
}
