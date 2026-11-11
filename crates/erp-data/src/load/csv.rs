//! CSV data files through `impex::load`.
//!
//! A CSV file without an `id` column in update mode never reaches this module: the reader reports
//! it as skipped and the module system returns a `SkippedFile` notice.

use erp_fields::Value;
use erp_orm::{Context, Transaction};

use super::error::OpLabel;
use super::expr::qualify;
use super::load_state;
use super::{DataNotice, FileContext, LoadError, LoadErrorKind, LoadSession, NoticeKind};
use crate::ast::{CsvOp, XmlId};
use crate::impex::{MessageType, load_rows};
use crate::reader::ConvertMode;

/// Load a CSV data file.
pub(crate) async fn load_file(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: &CsvOp,
) -> Result<(), LoadError> {
    let label = OpLabel::new(format!(
        "CSV file {} (model {})",
        op.source_file(),
        op.import().model()
    ));
    let outcome = match run(session, tx, file, op).await {
        // collected once the rows are loaded; the session saves them at the end of the file
        Ok(()) => session
            .importer
            .load_csv(file.module(), op.import())
            .map_err(LoadErrorKind::Parse),
        Err(kind) => Err(kind),
    };
    outcome.map_err(|kind| LoadError::at(op.source_file(), op.line(), label, kind))
}

fn import_context(base: &Context, file: &FileContext, noupdate: bool) -> Context {
    let mut context = base.clone();
    context.remove("lang");
    let mode = match file.mode() {
        ConvertMode::Init => "init",
        ConvertMode::Update => "update",
    };
    for (key, value) in [
        ("mode", Value::str(mode)),
        (load_state::MODULE, Value::str(file.module())),
        (load_state::DECLARING_MODULE, Value::str(file.module())),
        (load_state::INSTALL_MODE, Value::Bool(true)),
        (load_state::INSTALL_MODULE, Value::str(file.module())),
        (
            load_state::INSTALL_FILENAME,
            Value::Str(std::sync::Arc::clone(file.install_filename())),
        ),
        (load_state::NOUPDATE, Value::Bool(noupdate)),
    ] {
        context.set(key, value);
    }
    context
}

async fn run(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: &CsvOp,
) -> Result<(), LoadErrorKind> {
    let import = op.import();
    let base = tx.env();
    if base.registry().model(import.model()).is_none() {
        return Err(LoadErrorKind::ModelMissing {
            what: "CSV file".to_owned(),
            model: import.model().to_owned(),
        });
    }
    let context = import_context(base.context(), file, op.env().noupdate());
    let records = base
        .with_context_replaced(context)
        .records(import.model())
        .map_err(LoadErrorKind::Orm)?;
    // the import carries `install_module`, so it carries the load state too (review A-S2)
    let records = session.with_load_state(&records, file.module());
    let fields: Vec<&str> = import.fields().iter().map(String::as_str).collect();
    // the external ids of other modules that do not exist yet: `load` creates their records
    // (warns about it)
    let foreign = missing_foreign_ids(tx, &base, import.model(), import, file).await?;
    let result = load_rows(&records, tx, &fields, import.rows())
        .await
        .map_err(LoadErrorKind::Orm)?;
    if result.messages.iter().any(|m| m.kind == MessageType::Error) {
        return Err(LoadErrorKind::CsvImport {
            module: file.module().to_owned(),
            file: file.path().to_owned(),
            messages: result.messages.into_iter().map(|m| m.message).collect(),
        });
    }
    for xid in foreign {
        session.notices.push(DataNotice::new(
            file.module(),
            file.path(),
            NoticeKind::ForeignRecordCreated {
                xid: xid.to_string(),
            },
        ));
    }
    // every row's external id counts as loaded, also the rows `update` mode left alone
    for (col_index, field_name) in import.fields().iter().enumerate() {
        if is_one2many_id_col(&base, import.model(), field_name) {
            for row in import.rows() {
                let Some(cell) = row.values().get(col_index).filter(|c| !c.is_empty()) else {
                    continue;
                };
                for item in cell.split(',') {
                    let trimmed = item.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let xid = XmlId::parse(&qualify(trimmed, file.module())).map_err(|e| {
                        LoadErrorKind::Orm(erp_fields::Error::validation(e.to_string()))
                    })?;
                    session.mark_loaded(&xid);
                }
            }
        }
    }
    Ok(())
}

fn is_one2many_id_col(env: &erp_orm::Env, model: &str, field_name: &str) -> bool {
    if field_name == "id" {
        return true;
    }
    if let Some((prefix, suffix)) = field_name.rsplit_once('/')
        && suffix == "id"
    {
        return env
            .registry()
            .model(model)
            .and_then(|m| m.field(prefix))
            .is_some_and(|f| matches!(f.ty, erp_fields::FieldType::One2many { .. }));
    }
    false
}

/// The external ids of the file's rows that name another module and are not in the database.
async fn missing_foreign_ids(
    tx: &mut Transaction,
    env: &erp_orm::Env,
    model: &str,
    import: &crate::csv::CsvImport,
    file: &FileContext,
) -> Result<Vec<XmlId>, LoadErrorKind> {
    let id_indices: Vec<usize> = import
        .fields()
        .iter()
        .enumerate()
        .filter_map(|(i, f)| is_one2many_id_col(env, model, f).then_some(i))
        .collect();
    if id_indices.is_empty() || env.registry().model(crate::model_data::MODEL).is_none() {
        return Ok(Vec::new());
    }
    let mut foreign: Vec<XmlId> = Vec::new();
    for col_index in id_indices {
        for row in import.rows() {
            let Some(cell) = row.values().get(col_index).filter(|c| !c.is_empty()) else {
                continue;
            };
            for item in cell.split(',') {
                let trimmed = item.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let xid = XmlId::parse(&qualify(trimmed, file.module())).map_err(|e| {
                    LoadErrorKind::Orm(erp_fields::Error::validation(e.to_string()))
                })?;
                if xid.module().is_some_and(|m| m != file.module()) && !foreign.contains(&xid) {
                    foreign.push(xid);
                }
            }
        }
    }
    if foreign.is_empty() {
        return Ok(foreign);
    }
    let found = crate::model_data::lookup(tx, env, &foreign)
        .await
        .map_err(LoadErrorKind::Orm)?;
    foreign.retain(|xid| !found.contains_key(xid));
    Ok(foreign)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    fn base() -> Context {
        let mut context = Context::new();
        context.set("lang", Value::str("fr_FR"));
        context.set("tz", Value::str("Europe/Brussels"));
        context
    }

    #[test]
    fn the_import_context_has_references_keys_and_no_lang() {
        for (mode, noupdate) in [
            (ConvertMode::Init, false),
            (ConvertMode::Update, false),
            (ConvertMode::Update, true),
        ] {
            let file = FileContext::new("m", "security/ir.access.csv", mode);
            let context = import_context(&base(), &file, noupdate);
            assert_eq!(context.get("lang"), None);
            // what the caller had stays
            assert_eq!(context.get("tz"), base().get("tz"));
            let expected_mode = match mode {
                ConvertMode::Init => "init",
                ConvertMode::Update => "update",
            };
            assert_eq!(context.get("mode"), Some(Value::str(expected_mode)));
            assert_eq!(context.get("module"), Some(Value::str(file.module())));
            assert_eq!(context.get("noupdate"), Some(Value::Bool(noupdate)));
            assert_eq!(context.get("install_mode"), Some(Value::Bool(true)));
            assert_eq!(
                context.get("install_module"),
                Some(Value::str(file.module()))
            );
            assert_eq!(
                context.get("install_filename"),
                Some(Value::str(format!("{}/{}", file.module(), file.path())))
            );
        }
    }
}
