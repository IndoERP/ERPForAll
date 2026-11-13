//! The environment an operation runs.
//!
//! The base is the transaction's environment without `lang` (data files load in the source
//! language). The `context` attributes of the operation's containers and of the
//! operation itself are evaluated outer to inner and merged key by key; the innermost `uid`
//! becomes the acting user (creates carry it as `create_uid`).

use std::collections::HashMap;

use erp_fields::Value;
use erp_orm::{Env, Records, Transaction};

use super::expr::{Known, qualify, to_value};
use super::{FileContext, LoadErrorKind, LoadSession};
use crate::ast::{OpEnv, XmlId};

/// The model a `uid` attribute must point at.
const USER_MODEL: &str = "res.users";

/// The view model, whose batches tell each write how far the batch got
/// (`load_state::TRACK_BATCH`).
pub(crate) const VIEW_MODEL: &str = "ir.ui.view";

/// The environment for an operation with the containers' `uid` and `context` in `openv`.
///
/// `known` collects the external ids the expressions resolved, for the caller to reuse.
pub(crate) async fn operation_env(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    openv: &OpEnv,
    known: &mut HashMap<String, Known>,
) -> Result<Env, LoadErrorKind> {
    let base = tx.env();
    let mut context = base.context().clone();
    // data files load in the source language
    context.remove("lang");
    if file.is_demo() {
        context.set(INSTALL_DEMO, Value::Bool(true));
    }
    for expr in openv.context_chain() {
        let value = session.eval(tx, file.module(), expr, known).await?;
        let erp_expr::ExprValue::Dict(map) = &value else {
            return Err(LoadErrorKind::NotAFieldValue {
                model: String::new(),
                field: "context".to_owned(),
                got: format!("a {}, not a dict", value.type_name()),
            });
        };
        for (key, item) in map.iter() {
            let erp_expr::ExprValue::Str(key) = key else {
                return Err(LoadErrorKind::NotAFieldValue {
                    model: String::new(),
                    field: "context".to_owned(),
                    got: format!("a dict with a {} key", key.type_name()),
                });
            };
            let item = to_value(item).map_err(|got| LoadErrorKind::NotAFieldValue {
                model: String::new(),
                field: format!("context['{key}']"),
                got,
            })?;
            context.set(key.clone(), item);
        }
    }
    // Stamp after authored contexts, also for idless records and nested commands.
    context.set(
        super::load_state::DECLARING_MODULE,
        Value::str(file.module()),
    );
    let mut env = base.with_context_replaced(context);
    if let Some(uid) = openv.uid() {
        let user = user_id(session, tx, file, uid, known).await?;
        env = env.with_user(user);
    }
    Ok(env)
}

/// The id of the user the external id `uid` names (`id_get`, then a check that it is a user).
async fn user_id(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    uid: &XmlId,
    known: &mut HashMap<String, Known>,
) -> Result<i64, LoadErrorKind> {
    let name = qualify(&uid.to_string(), file.module());
    session
        .resolve(tx, std::slice::from_ref(&name), known)
        .await?;
    let Some(id) = known.get(&name).and_then(|k| k.id()) else {
        return Err(LoadErrorKind::ExternalIdNotFound { xid: name });
    };
    let xid = XmlId::parse(&name).map_err(LoadErrorKind::InvalidExternalId)?;
    match session.cache.get(&xid) {
        Some(row) if row.model == USER_MODEL => Ok(id),
        Some(row) => Err(LoadErrorKind::NotAUser {
            xid: name,
            model: row.model.clone(),
        }),
        None => Err(LoadErrorKind::ExternalIdNotFound { xid: name }),
    }
}

/// The context key a demo file loads with (`install_demo`, `True`).
pub const INSTALL_DEMO: &str = "install_demo";

pub(crate) fn with_install_keys(records: &Records, file: &FileContext) -> Records {
    records.with_context([
        (super::load_state::INSTALL_MODE, Value::Bool(true)),
        (super::load_state::INSTALL_MODULE, Value::str(file.module())),
        (
            super::load_state::INSTALL_FILENAME,
            Value::Str(std::sync::Arc::clone(file.install_filename())),
        ),
    ])
}
