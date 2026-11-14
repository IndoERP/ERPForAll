//! Each level reads its columns for the whole recordset at once (one fetch of the stored
//! fields, one read per relational field), and a relational column group is exported once for
//! the union of the related records, whose lines are then spliced under each record: the number
//! of statements depends on the column paths, not on the number of records. External ids are
//! resolved at the end, one lookup (and at most one `create`) per model.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use erp_fields::{Cell, Error, FieldType, RecId, Result, Value};
use erp_orm::{BoxFuture, Env, Records, Transaction};
use erp_registry::{FieldSpec, ModelId};

use super::path::checked_path;
use crate::model_data;

/// Superuser mode always may; for other environments [`export_data`] asks this. Groups arrive
/// with `res.groups` and the security layer (M3); until then a caller states its rule
/// explicitly, and [`SuperuserOnly`] refuses everyone else (fail closed).
pub trait ExportRights: Send + Sync {
    /// Whether the user of `env` may export data.
    fn may_export<'a>(
        &'a self,
        tx: &'a mut Transaction,
        env: &'a Env,
    ) -> BoxFuture<'a, Result<bool>>;
}

/// Only superuser mode exports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuperuserOnly;

impl ExportRights for SuperuserOnly {
    fn may_export<'a>(
        &'a self,
        _tx: &'a mut Transaction,
        env: &'a Env,
    ) -> BoxFuture<'a, Result<bool>> {
        let su = env.su();
        Box::pin(async move { Ok(su) })
    }
}

/// The result of `export_data`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportResult {
    pub datas: Vec<Vec<Value>>,
}

impl ExportResult {
    pub fn to_value(&self) -> Value {
        let mut map = erp_fields::ValueMap::new();
        map.insert(
            "datas".to_owned(),
            Value::list(self.datas.iter().map(|l| Value::list(l.iter().cloned()))),
        );
        Value::Map(Arc::new(map))
    }
}

const NO_RIGHTS: &str =
    "You don't have the rights to export data. Please contact an Administrator.";

/// `export_data(fields)` on `records`.
///
/// Column paths are split like import columns (`m2o/id`, `m2o.id`); `id` gives the external id
/// (created under `__export__` when missing), `.id` the database id.
/// Context key `import_compat` (default true): many2many columns are one cell of
/// comma-separated external ids or display names.
pub async fn export_data(
    records: &Records,
    tx: &mut Transaction,
    fields: &[&str],
    rights: &dyn ExportRights,
) -> Result<ExportResult> {
    let env = records.env().clone();
    if !env.su() && !rights.may_export(tx, &env).await? {
        return Err(Error::user(NO_RIGHTS));
    }
    let import_compat = match env.context().get("import_compat") {
        None => true,
        Some(Value::Bool(b)) => b,
        Some(Value::False) => false,
        Some(other) => {
            return Err(Error::programming(format!(
                "context key 'import_compat' must be a boolean, got {other}"
            )));
        }
    };
    let paths: Vec<Vec<String>> = fields
        .iter()
        .map(|f| checked_path(f))
        .collect::<Result<_>>()?;
    validate(records.env().registry(), records.spec(), &paths)?;
    let exporter = Exporter {
        import_compat,
        tz: env.tz().to_owned(),
    };
    let blocks = exporter.rows(tx, records, &paths).await?;
    let mut lines: Vec<Vec<XCell>> = blocks.into_iter().flatten().collect();
    resolve_xids(tx, &env, &mut lines).await?;
    let datas = lines
        .into_iter()
        .map(|line| {
            line.into_iter()
                .map(|cell| match cell {
                    XCell::Val(v) => Ok(v),
                    XCell::Xid(..) | XCell::Xids(..) => Err(Error::programming(
                        "an external id placeholder was left in the export",
                    )),
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ExportResult { datas })
}

fn validate(
    registry: &erp_registry::Registry,
    spec: &erp_registry::ModelSpec,
    paths: &[Vec<String>],
) -> Result<()> {
    let mut groups: Vec<(&str, &str, Vec<Vec<String>>)> = Vec::new();
    for path in paths {
        let Some(name) = path.first() else { continue };
        let fname = name.split_once('@').map_or(name.as_str(), |(f, _)| f);
        let relational = match fname {
            "id" | ".id" => None,
            _ => {
                let field = spec.field(fname).ok_or_else(|| {
                    Error::programming(format!("Invalid field '{fname}' on model '{}'", spec.name))
                })?;
                match &field.ty {
                    FieldType::Many2one { comodel, .. }
                    | FieldType::One2many { comodel, .. }
                    | FieldType::Many2many { comodel, .. } => Some(comodel.as_str()),
                    _ => None,
                }
            }
        };
        if path.len() == 1 {
            continue;
        }
        let Some(comodel) = relational else {
            return Err(Error::programming(format!(
                "Invalid field '{}': '{fname}' of model '{}' is not a relational field and has \
                 no sub-fields",
                path.join("/"),
                spec.name
            )));
        };
        match groups.iter_mut().find(|(n, _, _)| *n == name.as_str()) {
            Some((_, _, subs)) => subs.push(path[1..].to_vec()),
            None => groups.push((name.as_str(), comodel, vec![path[1..].to_vec()])),
        }
    }
    for (_, comodel_name, subs) in groups {
        let comodel = registry.model(comodel_name).ok_or_else(|| {
            Error::programming(format!(
                "Model '{comodel_name}' does not exist in registry."
            ))
        })?;
        validate(registry, comodel, &subs)?;
    }
    Ok(())
}

/// A cell during export: a value, or external ids resolved at the end.
#[derive(Debug, Clone)]
enum XCell {
    Val(Value),
    Xid(ModelId, i64),
    Xids(ModelId, Vec<i64>),
}

impl XCell {
    fn blank() -> Self {
        Self::Val(Value::str(""))
    }

    fn merges(&self) -> bool {
        match self {
            Self::Val(Value::Str(s)) => !s.is_empty(),
            Self::Val(Value::False) => false,
            _ => true,
        }
    }
}

type Line = Vec<XCell>;
type Block = Vec<Line>;

struct Exporter {
    import_compat: bool,
    tz: String,
}

impl Exporter {
    /// One block (the record's line, then its extra lines) per record of `records`.
    fn rows<'a>(
        &'a self,
        tx: &'a mut Transaction,
        records: &'a Records,
        paths: &'a [Vec<String>],
    ) -> BoxFuture<'a, Result<Vec<Block>>> {
        Box::pin(async move {
            let n = paths.len();
            let spec = records.spec();
            let registry = Arc::clone(records.env().registry());
            let ids: Vec<RecId> = records.rec_ids().to_vec();
            let mut blocks: Vec<Block> =
                ids.iter().map(|_| vec![vec![XCell::blank(); n]]).collect();
            if ids.is_empty() {
                return Ok(blocks);
            }
            // stored fields of this level in one fetch
            let mut stored: Vec<&str> = Vec::new();
            for path in paths {
                let Some(name) = path.first() else { continue };
                let fname = name.split_once('@').map_or(name.as_str(), |(f, _)| f);
                if let Some(field) = spec.field(fname)
                    && field.has_column()
                    && !stored.contains(&field.name)
                {
                    stored.push(field.name);
                }
            }
            if !stored.is_empty() {
                records.fetch(tx, Some(stored.as_slice())).await?;
            }
            let mut done: Vec<&str> = Vec::new();
            for (i, path) in paths.iter().enumerate() {
                let Some(name) = path.first() else { continue };
                if done.contains(&name.as_str()) {
                    continue;
                }
                if name == ".id" {
                    for (block, id) in blocks.iter_mut().zip(&ids) {
                        block[0][i] = XCell::Val(Value::str(id.get().to_string()));
                    }
                    continue;
                }
                if name == "id" {
                    for (block, id) in blocks.iter_mut().zip(&ids) {
                        block[0][i] = XCell::Xid(spec.id, id.get());
                    }
                    continue;
                }
                // `field@lang`: the language only matters for translated fields (none yet)
                let fname = name.split_once('@').map_or(name.as_str(), |(f, _)| f);
                let field = spec.field(fname).ok_or_else(|| {
                    Error::programming(format!("Invalid field '{fname}' on model '{}'", spec.name))
                })?;
                let cells = records.cells(tx, field.name).await?;
                let relational = matches!(
                    field.ty,
                    FieldType::Many2one { .. }
                        | FieldType::One2many { .. }
                        | FieldType::Many2many { .. }
                );
                if !relational {
                    if matches!(field.ty, FieldType::Reference { .. }) && !self.import_compat {
                        return Err(Error::programming(format!(
                            "exporting reference field '{}' outside import-compatible mode is \
                             not supported until M3",
                            field.name
                        )));
                    }
                    for (block, cell) in blocks.iter_mut().zip(&cells) {
                        block[0][i] = XCell::Val(self.scalar(spec.name, field, cell)?);
                    }
                    continue;
                }
                done.push(name);
                let comodel_name = field.ty.comodel().ok_or_else(|| {
                    Error::programming(format!("field '{}' has no comodel", field.name))
                })?;
                let comodel = registry.model(comodel_name).ok_or_else(|| {
                    Error::programming(format!(
                        "Model '{comodel_name}' does not exist in registry."
                    ))
                })?;
                let related: Vec<Vec<RecId>> = cells
                    .iter()
                    .map(|cell| match cell {
                        Cell::Id(id) => RecId::from_raw(*id).map(|r| vec![r]).ok_or_else(|| {
                            Error::programming(format!(
                                "invalid id 0 in {}.{}",
                                spec.name, field.name
                            ))
                        }),
                        Cell::Ids(list) => Ok(list.to_vec()),
                        Cell::Null => Ok(Vec::new()),
                        other => Err(Error::programming(format!(
                            "unexpected {other:?} in relational field {}.{}",
                            spec.name, field.name
                        ))),
                    })
                    .collect::<Result<_>>()?;
                // distinct related records, in order of first appearance
                let mut union: Vec<RecId> = Vec::new();
                let mut position: HashMap<RecId, usize> = HashMap::new();
                for id in related.iter().flatten() {
                    if !position.contains_key(id) {
                        position.insert(*id, union.len());
                        union.push(*id);
                    }
                }
                let targets = records
                    .env()
                    .records(comodel.name)?
                    .browse_ids(union.iter().copied());
                let sub_paths: Vec<Vec<String>> = paths
                    .iter()
                    .map(|p| {
                        if p.first() == Some(name) {
                            if p.len() > 1 {
                                p[1..].to_vec()
                            } else {
                                vec!["display_name".to_owned()]
                            }
                        } else {
                            Vec::new()
                        }
                    })
                    .collect();
                if self.import_compat && matches!(field.ty, FieldType::Many2many { .. }) {
                    // one cell: comma-separated external ids or display names
                    let found = ["id", "name", "display_name"].iter().find_map(|wanted| {
                        sub_paths
                            .iter()
                            .position(|p| p.len() == 1 && p[0] == *wanted)
                            .map(|j| (j, *wanted))
                    });
                    let (index, wanted) = found.map_or((i, None), |(j, w)| (j, Some(w)));
                    if wanted == Some("id") {
                        for (block, rel) in blocks.iter_mut().zip(&related) {
                            block[0][index] =
                                XCell::Xids(comodel.id, rel.iter().map(|r| r.get()).collect());
                        }
                    } else {
                        let names = targets.cells(tx, "display_name").await?;
                        for (block, rel) in blocks.iter_mut().zip(&related) {
                            // every related id is in the union by construction
                            let joined = rel
                                .iter()
                                .filter_map(|r| position.get(r))
                                .map(|&p| match names.get(p) {
                                    Some(Cell::Str(s)) => Ok(s.to_string()),
                                    // a record without display name shows as empty text
                                    Some(Cell::Null) => Ok(String::new()),
                                    other => Err(Error::programming(format!(
                                        "unexpected display name {other:?} of {}",
                                        comodel.name
                                    ))),
                                })
                                .collect::<Result<Vec<String>>>()?;
                            block[0][index] = XCell::Val(Value::str(joined.join(",")));
                        }
                    }
                    continue;
                }
                let sub_blocks = self.rows(tx, &targets, &sub_paths).await?;
                for (block, rel) in blocks.iter_mut().zip(&related) {
                    // every related id is in the union by construction
                    let lines: Vec<Line> = rel
                        .iter()
                        .filter_map(|r| position.get(r))
                        .flat_map(|&p| sub_blocks[p].iter().cloned())
                        .collect();
                    let mut lines = lines.into_iter();
                    match lines.next() {
                        None => block[0][i] = XCell::blank(),
                        Some(first) => {
                            for (j, cell) in first.into_iter().enumerate() {
                                if cell.merges() {
                                    block[0][j] = cell;
                                }
                            }
                            block.extend(lines);
                        }
                    }
                }
            }
            Ok(blocks)
        })
    }

    fn scalar(&self, model: &str, field: &FieldSpec, cell: &Cell) -> Result<Value> {
        let blank = || Value::str("");
        Ok(match (&field.ty, cell) {
            // NULL booleans and numbers read as False/0, exported as such
            (FieldType::Boolean, Cell::Bool(b)) => Value::Bool(*b),
            (FieldType::Boolean, Cell::Null) => Value::Bool(false),
            (FieldType::Integer, Cell::Int(i)) => Value::Int(i64::from(*i)),
            (FieldType::Integer, Cell::Null) => Value::Int(0),
            (FieldType::Float { .. } | FieldType::Monetary { .. }, Cell::Float(f)) => {
                Value::Float(*f)
            }
            (FieldType::Float { .. } | FieldType::Monetary { .. }, Cell::Decimal(d)) => {
                Value::Float(d.to_string().parse::<f64>().map_err(|e| {
                    Error::programming(format!(
                        "decimal {d} of {model}.{} as float: {e}",
                        field.name
                    ))
                })?)
            }
            (FieldType::Float { .. } | FieldType::Monetary { .. }, Cell::Null) => Value::Float(0.0),
            (FieldType::Selection { values, .. }, Cell::Str(key)) => {
                let items = values.as_ref().ok_or_else(|| {
                    Error::programming(format!(
                        "exporting '{}' of '{model}' needs its selection method, which the \
                         export does not call until M3",
                        field.name
                    ))
                })?;
                match items.iter().find(|(k, _)| **k == **key) {
                    Some((_, label)) => Value::str(label),
                    // a stored key no longer in the selection exports as itself
                    None => Value::Str(Arc::clone(key)),
                }
            }
            (FieldType::Uuid, Cell::Uuid(value)) => Value::str(value.to_string()),
            (FieldType::BigInt, Cell::BigInt(value)) => Value::str(value.to_string()),
            (FieldType::Xid8, Cell::Xid8(value)) => Value::str(value.to_string()),
            (FieldType::TimestampTz, Cell::TimestampTz(value)) => Value::str(value.to_string()),
            (FieldType::Uuid | FieldType::Xid8 | FieldType::TimestampTz, Cell::Null) => blank(),
            (FieldType::Uuid | FieldType::Xid8 | FieldType::TimestampTz, _) => {
                return Err(Error::programming(format!(
                    "export cache value does not match native field {model}.{} ({})",
                    field.name,
                    field.ty.type_name()
                )));
            }
            (FieldType::Date, Cell::Date(d)) => Value::Date(*d),
            (FieldType::Datetime, Cell::DateTime(dt)) => Value::DateTime(
                dt.to_timezone(&self.tz)
                    .ok_or_else(|| Error::programming(format!("unknown timezone '{}'", self.tz)))?,
            ),
            (FieldType::Json, Cell::Json(j)) => Value::str(py_json(j)),
            // ranges export as their `[lower, upper]` repr, datetimes in the user's timezone
            (FieldType::DateRange { .. }, Cell::DateRange(r)) => {
                Value::str(erp_fields::RangeValue::Date(*r).to_string())
            }
            (FieldType::DatetimeRange { .. }, Cell::DatetimeRange(r)) => {
                let local = |end: Option<erp_fields::DateTime>| {
                    end.map(|dt| {
                        dt.to_timezone(&self.tz).ok_or_else(|| {
                            Error::programming(format!("unknown timezone '{}'", self.tz))
                        })
                    })
                    .transpose()
                };
                let range = erp_fields::Range::new(local(r.lower())?, local(r.upper())?);
                Value::str(erp_fields::RangeValue::Datetime(range).to_string())
            }
            (FieldType::Many2oneReference { .. }, Cell::Id(id)) => Value::Int(*id),
            (FieldType::Many2oneReference { .. }, Cell::Null) => Value::Int(0),
            (
                FieldType::Char { .. }
                | FieldType::Text
                | FieldType::Html
                | FieldType::Reference { .. },
                Cell::Str(s),
            ) => {
                if s.is_empty() {
                    blank()
                } else {
                    Value::Str(Arc::clone(s))
                }
            }
            (
                FieldType::Char { .. }
                | FieldType::Text
                | FieldType::Html
                | FieldType::Reference { .. }
                | FieldType::Selection { .. }
                | FieldType::Date
                | FieldType::Datetime
                | FieldType::Json
                | FieldType::DateRange { .. }
                | FieldType::DatetimeRange { .. },
                Cell::Null,
            ) => blank(),
            (ty, other) => {
                return Err(Error::programming(format!(
                    "cannot export {other:?} of {model}.{} ({})",
                    field.name,
                    ty.type_name()
                )));
            }
        })
    }
}

/// Replace external-id placeholders, one lookup per model.
async fn resolve_xids(tx: &mut Transaction, env: &Env, lines: &mut [Line]) -> Result<()> {
    let mut wanted: BTreeMap<usize, (ModelId, Vec<i64>)> = BTreeMap::new();
    for cell in lines.iter().flatten() {
        let (model, ids): (ModelId, &[i64]) = match cell {
            XCell::Xid(model, id) => (*model, std::slice::from_ref(id)),
            XCell::Xids(model, ids) => (*model, ids),
            XCell::Val(_) => continue,
        };
        let entry = wanted
            .entry(model.index())
            .or_insert_with(|| (model, Vec::new()));
        for id in ids {
            if !entry.1.contains(id) {
                entry.1.push(*id);
            }
        }
    }
    let mut xids: HashMap<(usize, i64), String> = HashMap::new();
    for (index, (model, ids)) in wanted {
        let name = env.registry().model_by_id(model).name;
        let records = env.records(name)?.browse(ids.iter().copied())?;
        let found = model_data::ensure_xml_ids(tx, &records).await?;
        for (id, xid) in ids.into_iter().zip(found) {
            xids.insert((index, id), xid);
        }
    }
    let lookup = |model: ModelId, id: i64| {
        xids.get(&(model.index(), id)).cloned().ok_or_else(|| {
            Error::programming(format!("failed to export the external id of record {id}"))
        })
    };
    for cell in lines.iter_mut().flatten() {
        let value = match cell {
            XCell::Xid(model, id) => lookup(*model, *id)?,
            XCell::Xids(model, ids) => ids
                .iter()
                .map(|id| lookup(*model, *id))
                .collect::<Result<Vec<_>>>()?
                .join(","),
            XCell::Val(_) => continue,
        };
        *cell = XCell::Val(Value::str(value));
    }
    Ok(())
}

/// Python's `json.dumps` (default separators `', '` and `': '`, ASCII output).
fn py_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_py_json(value, &mut out);
    out
}

fn write_py_json(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value as J;
    match value {
        J::Null => out.push_str("null"),
        J::Bool(true) => out.push_str("true"),
        J::Bool(false) => out.push_str("false"),
        J::Number(n) => out.push_str(&n.to_string()),
        J::String(s) => write_py_str(s, out),
        J::Array(items) => {
            out.push('[');
            for (k, item) in items.iter().enumerate() {
                if k > 0 {
                    out.push_str(", ");
                }
                write_py_json(item, out);
            }
            out.push(']');
        }
        J::Object(map) => {
            out.push('{');
            for (k, (key, item)) in map.iter().enumerate() {
                if k > 0 {
                    out.push_str(", ");
                }
                write_py_str(key, out);
                out.push_str(": ");
                write_py_json(item, out);
            }
            out.push('}');
        }
    }
}

fn write_py_str(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c.is_ascii() && !c.is_ascii_control() => out.push(c),
            c => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::py_json;

    #[test]
    fn json_like_python_dumps() {
        let v = serde_json::json!({"a": [1, "\u{e9}", null], "b": {"c": true}});
        let bs = '\\';
        assert_eq!(
            py_json(&v),
            format!(r#"{{"a": [1, "{bs}u00e9", null], "b": {{"c": true}}}}"#)
        );
        assert_eq!(
            py_json(&serde_json::json!("\u{1f600}")),
            format!(r#""{bs}ud83d{bs}ude00""#)
        );
    }
}

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::{TimestampTz, Uuid, Xid8};
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
    #[test]
    fn native_fields_export_exact_text_and_refuse_wrong_cache_kinds() {
        static ADDON: AddonDescriptor = AddonDescriptor {
            name: "native_export",
            depends: &["base"],
            exact_only: false,
        };
        static MODEL: ModelDecl = ModelDecl::new("native_export", "native.export").fields(
            const {
                &[
                    FieldDecl::uuid("u"),
                    FieldDecl::xid8("x"),
                    FieldDecl::timestamptz("t"),
                ]
            },
        );
        let registry = RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([&MODEL])
            .build()
            .unwrap();
        let spec = registry.model("native.export").unwrap();
        let exporter = Exporter {
            import_compat: true,
            tz: "Asia/Jakarta".into(),
        };
        let instant = TimestampTz::parse("2024-03-01T01:00:00.123456+07:00").unwrap();
        for (name, cell, text) in [
            ("u", Cell::Uuid(Uuid::NIL), Uuid::NIL.to_string()),
            (
                "x",
                Cell::Xid8(Xid8::from_u64(u64::MAX)),
                Xid8::from_u64(u64::MAX).to_string(),
            ),
            ("t", Cell::TimestampTz(instant), instant.to_string()),
        ] {
            let field = spec.field(name).unwrap();
            assert_eq!(
                exporter.scalar(spec.name, field, &cell).unwrap(),
                Value::str(text)
            );
            assert_eq!(
                exporter.scalar(spec.name, field, &Cell::Null).unwrap(),
                Value::str("")
            );
            let error = exporter
                .scalar(spec.name, field, &Cell::Int(1))
                .unwrap_err();
            assert_eq!(error.kind(), erp_fields::ErrorKind::Programming);
            assert_eq!(
                error.message(),
                format!(
                    "export cache value does not match native field {}.{name} ({})",
                    spec.name,
                    field.ty.type_name()
                )
            );
        }
    }
}
