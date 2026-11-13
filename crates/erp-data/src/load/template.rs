//! `<template>` as an `ir.ui.view` record.
//!
//! - the target model: `theme.ir.ui.view` for a module named `theme_*`, else `ir.ui.view`;
//! - whether a root `active="…"` is written: in init mode, or when the view does not exist yet;
//!   an upgrade of an existing view keeps the active state the user chose (`:543-547`).

use std::collections::HashMap;
use std::sync::Arc;

use erp_orm::Transaction;

use super::error::OpLabel;
use super::expr::{Known, qualify};
use super::{FileContext, LoadError, LoadErrorKind, LoadSession, record};
use crate::ast::{RootActive, ShorthandTarget, TemplateOp};
use crate::reader::ConvertMode;

/// The view model.
const VIEW_MODEL: &str = "ir.ui.view";

/// Where the view model comes from: a registry without it lacks the `base` addon.
const VIEW_SOURCE: &str = "addon base";

/// Load a `<template>`.
pub(crate) async fn load(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: &TemplateOp,
) -> Result<(), LoadError> {
    let label = OpLabel::new(format!("<template id=\"{}\">", op.id()));
    let fail = |kind| LoadError::at(op.source_file(), op.line(), label.clone(), kind);

    let target = if file.module().starts_with("theme_") {
        ShorthandTarget::Theme
    } else {
        ShorthandTarget::Standard
    };
    // the standard model comes with addon `base`; the theme models belong to the website addons,
    // so the record pipeline's own "not in the registry" error answers for them
    if target == ShorthandTarget::Standard && tx.env().registry().model(VIEW_MODEL).is_none() {
        return Err(fail(LoadErrorKind::ModelArrivesLater {
            what: "<template>".to_owned(),
            model: VIEW_MODEL.to_owned(),
            slice: VIEW_SOURCE.to_owned(),
        }));
    }

    // `active` on the root is applied in init mode or for a view that does not exist yet
    let root_active = if op.active().is_some() && file.mode() == ConvertMode::Update {
        let name = qualify(&op.id().to_string(), file.module());
        let mut known: HashMap<String, Known> = HashMap::new();
        session
            .resolve(tx, std::slice::from_ref(&name), &mut known)
            .await
            .map_err(&fail)?;
        match known.get(&name).and_then(|k| k.id()) {
            Some(_) => RootActive::Skip,
            None => RootActive::Apply,
        }
    } else {
        RootActive::Apply
    };

    let as_record = op.to_record_op(target, root_active);
    record::load(session, tx, file, Arc::new(as_record))
        .await
        .map_err(|e| e.relabel(label.clone()))
}
