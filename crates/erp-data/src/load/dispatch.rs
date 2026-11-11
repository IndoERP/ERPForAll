//! The router: every operation of a file goes to the module that owns its kind.

use std::sync::Arc;

use erp_orm::Transaction;

use super::{
    FileContext, LoadError, LoadSession, batch, csv, delete, function, menu, record, template,
};
use crate::ast::{DataOperation, FieldValue, RecordOp};

/// Run one operation of `file`.
pub(crate) async fn run(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: DataOperation,
) -> Result<(), LoadError> {
    // everything but a `<record>` may read what waiting records are about to write
    if !matches!(op, DataOperation::Record(_)) {
        batch::flush(session, tx).await?;
    }
    match op {
        DataOperation::Record(record_op) => {
            // translations of sub-records are not collected: refuse instead of dropping them
            if let Some(child) = translated_child(&record_op) {
                let err_msg = translated_values_message(child);
                let op = DataOperation::Record(record_op);
                return Err(LoadError::unsupported(
                    &op,
                    err_msg,
                    "a later release (translations of one2many sub-records)",
                ));
            }
            let record_op = Arc::new(record_op);
            record::load(session, tx, file, Arc::clone(&record_op)).await?;
            // collected once the record exists, so a failed record leaves no orphan entry; the
            // session saves them at the end of the file
            session.importer.load_record_op(file.module(), &record_op)?;
            Ok(())
        }
        DataOperation::Template(template_op) => {
            template::load(session, tx, file, &template_op).await
        }
        DataOperation::MenuItem(menu_op) => menu::load(session, tx, file, &menu_op).await,
        DataOperation::Function(function_op) => {
            function::load(session, tx, file, &function_op).await
        }
        DataOperation::Delete(delete_op) => delete::load(session, tx, file, &delete_op).await,
        DataOperation::Csv(csv_op) => csv::load_file(session, tx, file, &csv_op).await,
        // `<asset>` writes `ir.asset`, which belongs to the web client's asset bundles (Q11).
        DataOperation::Asset(asset_op) => Err(LoadError::unsupported(
            &DataOperation::Asset(asset_op),
            "<asset>",
            "M4 (web client asset bundles)",
        )),
    }
}

/// The first one2many sub-record, depth first, that carries translated values.
fn translated_child(op: &RecordOp) -> Option<&RecordOp> {
    op.fields().iter().find_map(|field| match field.value() {
        FieldValue::SubRecords(children) => children.iter().find_map(|child| {
            if child.translations().is_empty() {
                translated_child(child)
            } else {
                Some(child)
            }
        }),
        _ => None,
    })
}

/// The refusal message for the translated values of `owner` (`op` or a sub-record of it).
fn translated_values_message(owner: &RecordOp) -> String {
    let owner_text = owner.id().map_or_else(
        || "a record without an id".to_owned(),
        |id| format!("record '{id}'"),
    );
    // the first translation exists: `first_translated` found `owner` by it
    match owner.translations().first() {
        Some(tr) => {
            let column = match tr.lang() {
                Some(lang) => format!("'{}@{lang}'", tr.name()),
                // the parser gives every translation a language: say so if that ever breaks
                None => format!("'{}' (no language)", tr.name()),
            };
            format!(
                "translated field values ({column} on {owner_text}; {} in all)",
                owner.translations().len()
            )
        }
        None => format!("translated field values (on {owner_text})"),
    }
}
