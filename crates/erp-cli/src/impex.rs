// SPDX-License-Identifier: MIT OR Apache-2.0
//! Composition of list import/export from the shared data loader and the web workbook writer.
use bytes::Bytes;
use erp_addon_base::approval_request::{self, Admission};
use erp_addon_web::table_xlsx;
use erp_data::impex::{self, ExchangeField, GroupExportRights, ImportFileOptions, ImportFileType};
use erp_fields::{Error, Result, Value, convert::value_to_json};
use erp_orm::rpc::Exposure;
use erp_orm::{BoxFuture, Operation, Records, Transaction};
use erp_server::web::{
    ExchangeDownload, ExchangeFile, ExchangeUpload, ImportMode, WebDataExchange,
};
use serde_json::{Value as Json, json};
use std::collections::{HashMap, HashSet};

const RECORD_LIMIT: usize = 10_000;
const PREVIEW_ROWS: usize = 10;
const CONTACTS_GUIDE: &[u8] = include_bytes!("../../../addons/contacts/data/import_guide.csv");

const STEP_KEY: &str = "ai_implementation_step_id";

#[erp_orm::linkme::distributed_slice(impex::RPC_IMPORT_ADAPTERS)]
#[linkme(crate = erp_orm::linkme)]
static STEP_IMPORT: impex::RpcImportAdapter = impex::RpcImportAdapter {
    context_key: STEP_KEY,
    load: load_implementation_step,
};

fn rpc_fingerprint(model: &str, fields: &[&str], rows: impex::ValueRows<'_>) -> String {
    use impex::RowSource;
    erp_addon_ai_base::implementation::import_fingerprint_iter(
        model,
        fields,
        (0..rows.len()).map(|r| (0..rows.row_len(r)).map(move |c| rows.cell(r, c))),
    )
}

fn load_implementation_step<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    fields: &'a [&'a str],
    rows: impex::ValueRows<'a>,
) -> BoxFuture<'a, Result<impex::LoadResult>> {
    use impex::RowSource;
    Box::pin(async move {
        records.check_access(tx, Operation::Create).await?;
        records.check_access(tx, Operation::Write).await?;
        if fields.is_empty()
            || rows.is_empty()
            || rows.len() > RECORD_LIMIT
            || records.env().context().contains("_import_limit")
        {
            return Err(Error::validation(format!(
                "An implementation import requires mapped columns and 1 to {RECORD_LIMIT} rows in a complete batch."
            )));
        }
        impex::validate_import_columns(records, tx, fields).await?;
        tx.savepoint(async |tx| {
            let model = records.model_name();
            let step = erp_addon_ai_base::implementation::StepImport::prepare_scoped(
                tx,
                records.env(),
                model,
                || rpc_fingerprint(model, fields, rows),
            )
            .await?;
            let loaded = impex::load_rows(records, tx, fields, rows).await?;
            if let Some(ids) = &loaded.ids {
                step.complete(tx, ids.len()).await?;
            }
            Ok(loaded)
        })
        .await
    })
}

pub(crate) struct DataExchange;

impl WebDataExchange for DataExchange {
    fn upload<'a>(
        &'a self,
        tx: &'a mut Transaction,
        mode: ImportMode,
        options: ExchangeUpload,
        bytes: Bytes,
    ) -> BoxFuture<'a, Result<Json>> {
        Box::pin(upload(tx, mode, options, bytes))
    }

    fn download<'a>(
        &'a self,
        tx: &'a mut Transaction,
        template: bool,
        options: ExchangeDownload,
    ) -> BoxFuture<'a, Result<ExchangeFile>> {
        Box::pin(download(tx, template, options))
    }
}

async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|e| Error::internal(format!("Data exchange worker failed: {e}")))?
}

/// Binary file transfers have no idempotency receipt. Only dispatch an operation that its
/// ordinary RPC admission allows without a key. Governed operations use the JSON-2 lifecycle.
async fn check_transfer_policy(
    tx: &mut Transaction,
    records: &Records,
    method: &str,
) -> Result<()> {
    let model = records.model_name();
    let Some(Exposure::Exposed(exposed)) = tx.exposure().get(records.model_id(), method) else {
        return Err(Error::access("This data transfer method is unavailable."));
    };
    let call = approval_request::Call {
        model,
        method,
        effect: exposed.effect(),
        key: None,
        payload: None,
        binding: None,
    };
    let admission = approval_request::admit(tx, call).await?;
    if method == "load"
        && matches!(admission, Admission::KeyRequired)
        && let Some(Value::Int(step)) = records.env().context().get(STEP_KEY)
    {
        return Err(Error::validation(format!(
            "This implementation import requires approval. Submit {model}.load through JSON-2 with an Idempotency-Key and context.{STEP_KEY} set to {step}. Retry the same call after approval to record the import on this step."
        )));
    }
    file_admission(admission, model, method)
}

fn file_admission(admission: Admission, model: &str, method: &str) -> Result<()> {
    match admission {
        Admission::Dispatch => Ok(()),
        Admission::KeyRequired => Err(Error::validation(format!(
            "This transfer requires approval. Submit {model}.{method} through JSON-2 with an Idempotency-Key and retry that call after approval."
        ))),
        Admission::Defer(error) | Admission::ReplayOnly(error) => Err(error),
    }
}

async fn upload(
    tx: &mut Transaction,
    mode: ImportMode,
    options: ExchangeUpload,
    bytes: Bytes,
) -> Result<Json> {
    let kind = ImportFileType::parse(&options.format)?;
    let env = tx.env().with_context([("import_file", Value::Bool(true))]);
    if env.context().get("_import_limit").is_some() {
        return Err(Error::validation(
            "A browser import must process the whole file.",
        ));
    }
    let records = env.records(&options.model)?;
    records.check_access(tx, Operation::Create).await?;
    records.check_access(tx, Operation::Write).await?;
    if mode != ImportMode::Preview {
        check_transfer_policy(tx, &records, "load").await?;
    }
    let (read, sheets, selected_sheet) = blocking(move || {
        let sheets = if kind == ImportFileType::Xlsx {
            impex::import_sheet_names(&bytes)?
        } else {
            Vec::new()
        };
        let read_options = ImportFileOptions {
            sheet: options.sheet.or_else(|| sheets.first().cloned()),
            separator: options.separator,
        };
        let read = impex::read_import_file_with_positions(&bytes, kind, &read_options).and_then(|(file, positions)| {
            if file.rows.len() > RECORD_LIMIT {
                return Err(Error::validation(format!(
                    "The file has {} data rows. At most {RECORD_LIMIT} may be imported at once.",
                    file.rows.len()
                )));
            }
            Ok((file, positions))
        });
        Ok((read, sheets, read_options.sheet))
    })
    .await?;
    let (mut file, mut positions) = match read {
        Ok(data) => data,
        Err(error)
            if mode == ImportMode::Preview
                && sheets.len() > 1
                && selected_sheet.as_ref().is_some_and(|s| sheets.contains(s))
                && error.kind() == erp_fields::ErrorKind::Validation =>
        {
            // An empty or instructional sheet should still leave the other sheets selectable.
            return Ok(
                json!({"header": [], "rows": [], "row_count": 0, "sheet": selected_sheet,
                    "sheets": sheets, "sheet_error": error.message()}),
            );
        }
        Err(error) => return Err(error),
    };
    if mode == ImportMode::Preview {
        let row_count = file.rows.len();
        file.rows.truncate(PREVIEW_ROWS);
        return Ok(
            json!({"header": file.header, "rows": file.rows, "row_count": row_count, "sheet": file.sheet, "sheets": sheets}),
        );
    }
    if file.rows.is_empty() {
        return Err(Error::validation(
            "The file has no data rows below its column names.",
        ));
    }
    let mapping = options
        .fields
        .ok_or_else(|| Error::validation("Map the file columns before importing."))?;
    if mapping.len() != file.header.len() {
        return Err(Error::validation(
            "The mapping must contain one entry per file column.",
        ));
    }
    let fields: Vec<_> = mapping.iter().filter_map(Option::as_deref).collect();
    if fields.is_empty() {
        return Err(Error::validation("Map at least one file column."));
    }
    impex::validate_import_columns(&records, tx, &fields).await?;
    if fields.len() != mapping.len() {
        // Removing an ignored column can turn a note or footer into a blank row.
        // Keep each surviving row paired with its original worksheet row or CSV line.
        (file.rows, positions) = file
            .rows
            .into_iter()
            .zip(positions)
            .filter_map(|(mut row, position)| {
                let mut keep = mapping.iter().map(Option::is_some);
                row.retain(|_| keep.next() == Some(true));
                row.iter()
                    .any(|cell| !cell.is_empty())
                    .then_some((row, position))
            })
            .unzip();
        if file.rows.is_empty() {
            return Err(Error::validation(
                "The mapped columns contain no data rows.",
            ));
        }
    }
    let step_import = if mode == ImportMode::Import {
        erp_addon_ai_base::implementation::StepImport::prepare(
            tx,
            &options.model,
            &fields,
            &file.rows,
        )
        .await?
    } else {
        None
    };
    let result = impex::load(&records, tx, &fields, &file.rows).await?;
    if let (Some(step), Some(ids)) = (step_import, &result.ids) {
        step.complete(tx, ids.len()).await?;
    }
    let mut reply = value_to_json(&result.to_value())?;
    let messages = reply["messages"]
        .as_array_mut()
        .ok_or_else(|| Error::programming("Invalid load messages."))?;
    for (wire, source) in messages.iter_mut().zip(&result.messages) {
        if let Some(span) = source.rows {
            let (Some(from), Some(to)) = (positions.get(span.from), positions.get(span.to)) else {
                return Err(Error::programming(
                    "Import message refers to a row outside the file.",
                ));
            };
            wire["file_rows"] = json!({"from": from, "to": to});
        }
    }
    if mode == ImportMode::Test {
        let map = reply
            .as_object_mut()
            .ok_or_else(|| Error::programming("Invalid load result."))?;
        map.remove("ids");
        map.insert(
            "count".into(),
            result.ids.map_or(Json::Bool(false), |ids| json!(ids.len())),
        );
    }
    Ok(reply)
}

async fn download(
    tx: &mut Transaction,
    template: bool,
    options: ExchangeDownload,
) -> Result<ExchangeFile> {
    let kind = ImportFileType::parse(&options.format)?;
    // The record picker offers only single-cell columns. Join many-to-many display names
    // in that cell so one selected record always occupies one exported row.
    let env = tx
        .env()
        .with_context([("import_compat", Value::Bool(true))]);
    let records = env.records(&options.model)?;
    let available = if template {
        impex::import_fields(&records, tx).await?
    } else {
        impex::export_fields(&records, tx).await?
    };
    let by_name: HashMap<_, _> = available.iter().map(|f| (f.name.as_str(), f)).collect();
    let mut seen = HashSet::new();
    let mut columns = Vec::new();
    for column in &options.fields {
        let field = by_name.get(column.name.as_str()).ok_or_else(|| {
            Error::validation(format!("Column '{}' is unavailable.", column.name))
        })?;
        if !seen.insert(&field.name) {
            return Err(Error::validation(format!(
                "Column '{}' was requested twice.",
                field.name
            )));
        }
        columns.push(*field);
    }
    let (header, rows, guide) = if template {
        if options.ids.is_some() {
            return Err(Error::validation("A template does not take record IDs."));
        }
        let mut preferred: Vec<_> = columns.iter().map(|f| f.name.clone()).collect();
        // Contacts lists display complete_name, a computed field. A new contact must supply
        // name, whose requirement is conditional on address type and lives in a constraint.
        if options.model == "res.partner" && by_name.contains_key("name") {
            preferred.insert(0, "name".into());
        }
        let selected = impex::template_columns(&available, &preferred);
        if selected.is_empty() {
            return Err(Error::validation("This model has no importable columns."));
        }
        let guide = if kind == ImportFileType::Xlsx {
            template_guide(&selected, &options.model)?
        } else {
            Vec::new()
        };
        (
            selected.iter().map(|f| f.name.clone()).collect(),
            Vec::new(),
            guide,
        )
    } else {
        if columns.is_empty() {
            return Err(Error::validation("Choose at least one export field."));
        }
        let ids = options
            .ids
            .ok_or_else(|| Error::validation("Export requires record IDs."))?;
        if ids.is_empty() || ids.len() > RECORD_LIMIT || ids.iter().any(|id| *id <= 0) {
            return Err(Error::validation(format!(
                "Choose between 1 and {RECORD_LIMIT} records with positive IDs."
            )));
        }
        if ids.len() * columns.len() > impex::MAX_FILE_CELLS {
            return Err(Error::validation(format!(
                "Export at most {} cells per file.",
                impex::MAX_FILE_CELLS
            )));
        }
        let selected = records.browse(ids)?;
        selected.check_access(tx, Operation::Read).await?;
        check_transfer_policy(tx, &selected, "export_data").await?;
        if selected.exists(tx).await?.len() != selected.len() {
            return Err(Error::validation(
                "One or more selected records no longer exist.",
            ));
        }
        let fields: Vec<_> = columns.iter().map(|f| f.name.as_str()).collect();
        let data = impex::export_data(&selected, tx, &fields, &GroupExportRights).await?;
        let rows = data
            .datas
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| {
                        if v == Value::False {
                            Ok(Json::Null)
                        } else {
                            value_to_json(&v)
                        }
                    })
                    .collect()
            })
            .collect::<Result<Vec<Vec<_>>>>()?;
        (
            columns.iter().map(|f| f.label.clone()).collect::<Vec<_>>(),
            rows,
            Vec::new(),
        )
    };
    let title = if template {
        format!("{}-template", options.model)
    } else {
        options.model
    };
    blocking(move || {
        let (content_type, bytes) = match kind {
            ImportFileType::Csv => ("text/csv; charset=utf-8", csv_file(&header, &rows)?),
            ImportFileType::Xlsx => (
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                table_xlsx::write_record_table(&header, &rows, &guide)?,
            ),
        };
        Ok(ExchangeFile {
            name: table_xlsx::download_name(&title, &options.format, "Records"),
            content_type,
            bytes,
        })
    })
    .await
}

fn template_guide(columns: &[&ExchangeField], model: &str) -> Result<Vec<Vec<String>>> {
    let mut guide = vec![
        vec![
            "Column".into(),
            "Label".into(),
            "Required for creation".into(),
            "Example".into(),
        ],
        vec!["Enter your records on Data, one record per row below the field names.".into()],
        vec!["Test the file to check values and references before importing.".into()],
    ];
    let mut examples = HashMap::new();
    if model == "res.partner" {
        for row in csv::Reader::from_reader(CONTACTS_GUIDE).deserialize::<(String, String)>() {
            let (field, value) =
                row.map_err(|e| Error::internal(format!("Invalid Contacts import guide: {e}")))?;
            examples.insert(field, value);
        }
    }
    for column in columns {
        guide.push(vec![
            column.name.clone(),
            column.label.clone(),
            match (column.required, model, column.name.as_str()) {
                (true, _, _) => "Yes",
                (_, "res.partner", "name") => "For contacts",
                _ => "",
            }
            .into(),
            examples.remove(&column.name).unwrap_or_default(),
        ]);
    }
    Ok(guide)
}

fn csv_text(text: &str) -> String {
    if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{text}")
    } else {
        text.to_owned()
    }
}

fn csv_file(header: &[String], rows: &[Vec<Json>]) -> Result<Vec<u8>> {
    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .from_writer(vec![0xef, 0xbb, 0xbf]);
    let fail = |e: csv::Error| Error::internal(format!("Cannot write CSV: {e}"));
    writer
        .write_record(header.iter().map(|v| csv_text(v)))
        .map_err(fail)?;
    for row in rows {
        for value in row {
            let cell = match value {
                Json::Null => String::new(),
                Json::String(text) => csv_text(text),
                Json::Number(n) => n.to_string(),
                Json::Bool(b) => b.to_string(),
                _ => return Err(Error::validation("A CSV export cell must be scalar.")),
            };
            writer.write_field(cell).map_err(fail)?;
        }
        writer.write_record(None::<&[u8]>).map_err(fail)?;
    }
    writer
        .into_inner()
        .map_err(|e| Error::internal(format!("Cannot finish CSV: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{Admission, Error, file_admission};
    use erp_fields::ErrorKind;

    #[test]
    fn step_import_rpc_and_file_identify_same_mapped_values() {
        use erp_fields::Value;
        let rows = [vec!["one".to_owned(), "two".to_owned()]];
        let values = [Value::list(rows[0].iter().map(Value::str))];
        let source = erp_data::impex::ValueRows::validate(&values, 10).unwrap();
        assert_eq!(
            super::rpc_fingerprint("res.partner", &["ref", "name"], source),
            erp_addon_ai_base::implementation::import_fingerprint(
                "res.partner",
                &["ref", "name"],
                &rows
            )
        );
        assert_eq!(
            erp_data::impex::RPC_IMPORT_ADAPTERS
                .iter()
                .filter(|a| a.context_key == super::STEP_KEY)
                .count(),
            1
        );
    }

    #[test]
    fn files_dispatch_only_when_rpc_admission_allows_the_transfer() {
        assert!(file_admission(Admission::Dispatch, "res.partner", "load").is_ok());
        for method in ["load", "export_data"] {
            let error = file_admission(Admission::KeyRequired, "res.partner", method).err();
            assert!(error.is_some_and(|e| e.kind() == ErrorKind::Validation
                && e.message().contains(&format!(
                    "res.partner.{method} through JSON-2 with an Idempotency-Key"
                ))));
        }
        let error = file_admission(
            Admission::ReplayOnly(Error::access("Expired receipt.")),
            "res.partner",
            "load",
        )
        .err();
        assert!(error.is_some_and(|e| e.kind() == ErrorKind::Access && e.message() == "Expired receipt."));
    }
}
