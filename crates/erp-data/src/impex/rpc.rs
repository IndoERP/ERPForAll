//! JSON-2 RPC exposure and argument binders for `load`, `read_import_file`, `export_data`, and
//! `get_import_templates`.
//!
//! Contributes [`RpcImpl`] entries to [`erp_orm::rpc::RPC_IMPLS`] under model `"base"` and addon `"base"`.

use erp_fields::{Error, Result, Value};
use erp_orm::dispatch::BoxFuture;
use erp_orm::rpc::{BindError, Effect, ParamSpec, Receiver, RpcArgs, RpcDecl, RpcImpl};
use erp_orm::{Env, Records, Transaction};

use super::{ExportRights, MAX_ROWS};

const MAX_COLUMNS: usize = 1_000;
const MAX_EXPORT_FIELDS: usize = 1_000;

struct RpcExportRights;

impl ExportRights for RpcExportRights {
    fn may_export<'a>(
        &'a self,
        tx: &'a mut Transaction,
        env: &'a Env,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            if env.su() {
                return Ok(true);
            }
            tx.has_groups(env, "base.group_allow_export,base.group_erp_manager")
                .await
        })
    }
}

const LOAD_PARAMS: &[ParamSpec] = &[
    ParamSpec::new("fields", true, "an array"),
    ParamSpec::new("data", true, "an array"),
];

#[erp_orm::linkme::distributed_slice(erp_orm::rpc::RPC_IMPLS)]
#[linkme(crate = erp_orm::linkme)]
static LOAD: RpcImpl = RpcImpl {
    addon: "base",
    model: "base",
    method: "load",
    decl: RpcDecl::Expose {
        effect: Effect::Write,
        receiver: Receiver::Model,
        params: LOAD_PARAMS,
        bind: bind_load,
    },
    file: file!(),
    line: line!(),
};

fn bind_load<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let fields: Vec<String> = args.take("fields")?;
        let data: Vec<Value> = args.take("data")?;
        check_count("load", "columns", fields.len(), MAX_COLUMNS)?;
        let rows = super::ValueRows::validate(&data, MAX_ROWS)?;

        let field_refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        let result = super::load_rows(&records, tx, &field_refs, rows).await?;
        Ok(result.to_value())
    })
}

#[cfg(test)]
fn load_data(data: &[Value], max_rows: usize) -> Result<Vec<Vec<String>>> {
    use super::row::RowSource;
    let rows = super::ValueRows::validate(data, max_rows)?;
    Ok((0..rows.len())
        .map(|r| {
            let n = rows.row_len(r);
            (0..n).map(|c| rows.cell(r, c).to_owned()).collect()
        })
        .collect())
}

fn check_count(method: &str, item: &str, count: usize, limit: usize) -> Result<()> {
    if count > limit {
        return Err(Error::validation(format!(
            "{method} received {count} {item}; at most {limit} are accepted per call"
        )));
    }
    Ok(())
}

const READ_IMPORT_FILE_PARAMS: &[ParamSpec] = &[
    ParamSpec::new("file", true, "a base64 string"),
    ParamSpec::new("file_type", true, "'csv' or 'xlsx'"),
    ParamSpec::new("sheet", false, "a sheet name"),
    ParamSpec::new("separator", false, "',', ';' or a tab"),
];

/// `read_import_file(file, file_type, sheet?, separator?)`: the column titles and data rows of a
/// CSV or XLSX file, as `load` takes them. Reads nothing from the database and writes nothing.
#[erp_orm::linkme::distributed_slice(erp_orm::rpc::RPC_IMPLS)]
#[linkme(crate = erp_orm::linkme)]
static READ_IMPORT_FILE: RpcImpl = RpcImpl {
    addon: "base",
    model: "base",
    method: "read_import_file",
    decl: RpcDecl::Expose {
        effect: Effect::Read,
        receiver: Receiver::Model,
        params: READ_IMPORT_FILE_PARAMS,
        bind: bind_read_import_file,
    },
    file: file!(),
    line: line!(),
};

fn bind_read_import_file<'a>(
    _tx: &'a mut Transaction,
    _records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let file: String = args.take("file")?;
        let file_type: String = args.take("file_type")?;
        let sheet: Option<String> = args.take("sheet")?;
        let separator: Option<String> = args.take("separator")?;
        Ok(read_import_file_value(
            &file,
            &file_type,
            sheet,
            separator.as_deref(),
        )?)
    })
}

fn read_import_file_value(
    file: &str,
    file_type: &str,
    sheet: Option<String>,
    separator: Option<&str>,
) -> Result<Value> {
    let kind = super::ImportFileType::parse(file_type)?;
    // base64 grows the file by a third; refuse an oversized text before decoding it
    if file.len() > super::MAX_FILE_BYTES.div_ceil(3) * 4 + 4 {
        return Err(Error::validation(format!(
            "The import file is larger than {} bytes.",
            super::MAX_FILE_BYTES
        )));
    }
    let bytes = erp_fields::base64::decode_strict(file)
        .map_err(|_| Error::validation("The import file is not valid base64."))?;
    let separator = match separator {
        None => None,
        Some(text) => {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Some(ch),
                _ => {
                    return Err(Error::validation(
                        "The CSV separator must be a single character.",
                    ));
                }
            }
        }
    };
    let read =
        super::read_import_file(&bytes, kind, &super::ImportFileOptions { sheet, separator })?;
    let strings = |row: &[String]| Value::list(row.iter().map(Value::str));
    let mut map = erp_fields::ValueMap::new();
    map.insert("header".to_owned(), strings(&read.header));
    map.insert(
        "rows".to_owned(),
        Value::list(read.rows.iter().map(|row| strings(row))),
    );
    map.insert(
        "sheet".to_owned(),
        read.sheet.as_deref().map_or(Value::False, Value::str),
    );
    Ok(Value::Map(std::sync::Arc::new(map)))
}

const EXPORT_DATA_PARAMS: &[ParamSpec] = &[ParamSpec::new("fields_to_export", true, "an array")];

#[erp_orm::linkme::distributed_slice(erp_orm::rpc::RPC_IMPLS)]
#[linkme(crate = erp_orm::linkme)]
static EXPORT_DATA: RpcImpl = RpcImpl {
    addon: "base",
    model: "base",
    method: "export_data",
    decl: RpcDecl::Expose {
        effect: Effect::Write,
        receiver: Receiver::Records,
        params: EXPORT_DATA_PARAMS,
        bind: bind_export_data,
    },
    file: file!(),
    line: line!(),
};

fn bind_export_data<'a>(
    tx: &'a mut Transaction,
    records: Records,
    mut args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move {
        let fields_to_export: Vec<String> = args.take("fields_to_export")?;
        check_count(
            "export_data",
            "fields",
            fields_to_export.len(),
            MAX_EXPORT_FIELDS,
        )?;
        let field_refs: Vec<&str> = fields_to_export.iter().map(String::as_str).collect();
        let rights = RpcExportRights;
        let result = super::export_data(&records, tx, &field_refs, &rights).await?;
        Ok(result.to_value())
    })
}

const GET_IMPORT_TEMPLATES_PARAMS: &[ParamSpec] = &[];

#[erp_orm::linkme::distributed_slice(erp_orm::rpc::RPC_IMPLS)]
#[linkme(crate = erp_orm::linkme)]
static GET_IMPORT_TEMPLATES: RpcImpl = RpcImpl {
    addon: "base",
    model: "base",
    method: "get_import_templates",
    decl: RpcDecl::Expose {
        effect: Effect::Write,
        receiver: Receiver::Model,
        params: GET_IMPORT_TEMPLATES_PARAMS,
        bind: bind_get_import_templates,
    },
    file: file!(),
    line: line!(),
};

fn bind_get_import_templates<'a>(
    _tx: &'a mut Transaction,
    _records: Records,
    _args: RpcArgs,
) -> BoxFuture<'a, Result<Value, BindError>> {
    Box::pin(async move { Ok(Value::list([])) })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;
    use erp_fields::ErrorKind;

    #[test]
    fn row_limit_rejects_before_matrix_conversion() {
        let data = [Value::list([]), Value::list([])];
        assert_eq!(
            load_data(&data, 2).unwrap(),
            vec![Vec::<String>::new(), vec![]]
        );
        let error = load_data(&data, 1).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "load received 2 rows; at most 1 are accepted per call"
        );
        assert_eq!(load_data(&[], 1).unwrap(), Vec::<Vec<String>>::new());
    }

    #[test]
    fn matrix_reports_exact_cell_position_and_preserves_unicode() {
        let data = [
            Value::list([Value::str("é\r\n")]),
            Value::list([Value::Bool(false)]),
        ];
        assert_eq!(
            load_data(&data[..1], 2).unwrap(),
            vec![vec!["é\r\n".to_owned()]]
        );
        let error = load_data(&data, 2).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "load cell at row 1, column 0 must be a string"
        );
    }

    #[test]
    fn read_import_file_answers_titles_and_rows_from_base64() {
        let file = erp_fields::base64::encode("Name;Code\nAda;1\n".as_bytes());
        let value = read_import_file_value(&file, "csv", None, Some(";")).unwrap();
        let Value::Map(map) = value else {
            panic!("a map");
        };
        assert_eq!(
            map.get("header"),
            Some(&Value::list([Value::str("Name"), Value::str("Code")]))
        );
        assert_eq!(
            map.get("rows"),
            Some(&Value::list([Value::list([
                Value::str("Ada"),
                Value::str("1")
            ])]))
        );
        assert_eq!(map.get("sheet"), Some(&Value::False));
    }

    #[test]
    fn read_import_file_refuses_bad_arguments_by_name() {
        let file = erp_fields::base64::encode(b"Name\nAda\n");
        let cases = [
            (
                read_import_file_value(&file, "xls", None, None),
                "Import files of type 'xls' are not supported; use csv or xlsx.",
            ),
            (
                read_import_file_value("not base64!", "csv", None, None),
                "The import file is not valid base64.",
            ),
            (
                read_import_file_value(&file, "csv", None, Some(";;")),
                "The CSV separator must be a single character.",
            ),
            (
                read_import_file_value(&file, "csv", Some("Data".to_owned()), None),
                "A CSV file has no sheets to choose from.",
            ),
        ];
        for (result, expected) in cases {
            let error = result.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(error.message(), expected);
        }
    }
}
