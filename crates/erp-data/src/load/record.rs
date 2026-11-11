//! What happens where. When the loader meets a record ([`load`]): the `noupdate` skip of a record
//! without an id (`:377-380`), the check that the module of a foreign external id is part of the
//! install step (`:259-267`), the check that the model exists, and the question whether the
//! window must be written before the record can be appended (it names an external id that is
//! still waiting, or reads the database while it is evaluated). Then the record joins the window.
//! When the window is written ([`super::batch`]): the decisions that need the database (`noupdate`
//! skip of an existing record, foreign external id, the model of an existing external id), the
//! values of the records that were not skipped ([`build_values`]), the sequence and `xmlid`
//! fields, and one `load_records` call. A record with `<field>` sub-records is written at once and
//! its children follow with the inverse field set (`:459-465`, `:487-488`), depth first.

use std::collections::HashMap;
use std::sync::Arc;

use erp_fields::{FieldType, Value};
use erp_orm::Transaction;
use erp_registry::ModelSpec;

use super::batch::{self, Outcome, Push};
use super::expr::{Known, qualify, scan_refs, to_value};
use super::{FileContext, LoadError, LoadErrorKind, LoadSession, values, values_ext};
use crate::ast::{FieldValue, RecordOp, XmlId};
use crate::reader::ConvertMode;

/// What the fields of a record read, before the record is evaluated.
struct Scan {
    /// External ids it names, qualified.
    refs: Vec<String>,
    /// It reads the database while it is evaluated, or the reader's expression does not parse
    /// (the evaluation reports that): the window is written first.
    reads_db: bool,
}

fn scan_expression(expr: &crate::ast::Expr, module: &str, scan: &mut Scan) {
    match expr.parsed() {
        Ok(ast) => {
            let mut dynamic = false;
            scan_refs(ast, module, &mut scan.refs, &mut dynamic);
            scan.reads_db |= dynamic || expr_reads_db(ast);
        }
        // reported with the expression's own text when the record is evaluated
        Err(_) => scan.reads_db = true,
    }
}

fn expr_reads_db(ast: &erp_expr::Expr) -> bool {
    let mut reads = false;
    walk_reads_db(ast, &mut reads);
    reads
}

fn walk_reads_db(ast: &erp_expr::Expr, out: &mut bool) {
    if *out {
        return;
    }
    match ast {
        erp_expr::Expr::Name(name) if name == "obj" || name == "env" => {
            *out = true;
        }
        erp_expr::Expr::Call {
            func,
            args,
            keywords,
        } => {
            walk_reads_db(func, out);
            for a in args {
                walk_reads_db(a, out);
            }
            for (_, v) in keywords {
                walk_reads_db(v, out);
            }
        }
        erp_expr::Expr::Attribute { value, .. } => walk_reads_db(value, out),
        erp_expr::Expr::Subscript { value, slice } => {
            walk_reads_db(value, out);
            if let erp_expr::SubscriptKind::Index(i) = slice {
                walk_reads_db(i, out);
            }
        }
        erp_expr::Expr::Unary { operand, .. } => walk_reads_db(operand, out),
        erp_expr::Expr::Binary { left, right, .. } => {
            walk_reads_db(left, out);
            walk_reads_db(right, out);
        }
        erp_expr::Expr::Compare {
            left, comparators, ..
        } => {
            walk_reads_db(left, out);
            for c in comparators {
                walk_reads_db(c, out);
            }
        }
        erp_expr::Expr::List(items) | erp_expr::Expr::Tuple(items) => {
            for item in items {
                walk_reads_db(item, out);
            }
        }
        _ => {}
    }
}

fn scan(op: &RecordOp, module: &str) -> Scan {
    let mut scan = Scan {
        refs: Vec::new(),
        reads_db: false,
    };
    if let Some(uid) = op.env().uid() {
        scan.refs.push(qualify(&uid.to_string(), module));
    }
    for expr in op.env().context_chain() {
        scan_expression(expr, module, &mut scan);
    }
    for field in op.fields() {
        match field.value() {
            FieldValue::Ref(id) => scan.refs.push(qualify(&id.to_string(), module)),
            FieldValue::Eval { expr, .. } => scan_expression(expr, module, &mut scan),
            FieldValue::Text(_)
            | FieldValue::Int(_)
            | FieldValue::Float(_)
            | FieldValue::SubRecords(_) => {}
            // addon files: no record of the window can change them, so the record joins the
            // window; the content read counts toward its byte limit (`batch::run_window`)
            FieldValue::ModulePath(_)
            | FieldValue::FileText(_)
            | FieldValue::FileBytes(_)
            | FieldValue::FileBase64(_) => {}
            // `search=`, lists, `%(xid)d` markers and anything added later read the database or
            // other records' ids
            _ => scan.reads_db = true,
        }
    }
    scan
}

/// Load one `<record>`: append it to the window (and, with sub-records, write it and load its
/// children).
pub(crate) async fn load(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: Arc<RecordOp>,
) -> Result<(), LoadError> {
    let tree = session.counters.open(op.env().auto_sequence());
    let result = process(session, tx, file, op, None, tree).await;
    session.counters.close(tree);
    result
}

fn located(op: &RecordOp, kind: LoadErrorKind) -> LoadError {
    LoadError::at(
        op.source_file(),
        op.line(),
        super::error::OpLabel::of_record(op),
        kind,
    )
}

/// The `<field>` sub-records of `op` with the inverse field each one sets.
fn sub_records<'a>(
    op: &'a RecordOp,
    spec: &ModelSpec,
) -> Result<Vec<(&'a RecordOp, &'a str, String)>, LoadErrorKind> {
    let mut out = Vec::new();
    for field in op.fields() {
        let FieldValue::SubRecords(children) = field.value() else {
            continue;
        };
        let inverse = match spec.field(field.name()).map(|f| &f.ty) {
            Some(FieldType::One2many { inverse, .. }) => inverse.clone(),
            _ => {
                return Err(LoadErrorKind::NotOne2many {
                    model: op.model().to_owned(),
                    field: field.name().to_owned(),
                });
            }
        };
        for child in children {
            out.push((child, field.name(), inverse.clone()));
        }
    }
    Ok(out)
}

async fn process(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: Arc<RecordOp>,
    extra: Option<(String, i64)>,
    tree: u64,
) -> Result<(), LoadError> {
    let at = |kind| located(&op, kind);
    let module = file.module();
    let update = file.mode() == ConvertMode::Update;
    let registry = std::sync::Arc::clone(tx.registry());

    let xid = match op.id() {
        Some(id) => Some(batch::qualified(id, module).map_err(|e| at(invalid(e)))?),
        None => None,
    };
    let foreign = xid
        .as_ref()
        .is_some_and(|x| x.module().is_some_and(|m| m != module));
    // `_test_xml_id` (369)
    if foreign
        && let Some(x) = &xid
        && let Some(kind) = batch::uninstalled(x, &registry)
    {
        return Err(at(kind));
    }
    // an unnamed record under `noupdate` in update mode is skipped
    if update && op.env().noupdate() && xid.is_none() {
        return Ok(());
    }
    let Some(spec) = registry.model(op.model()) else {
        return Err(at(LoadErrorKind::ModelMissing {
            what: "<record>".to_owned(),
            model: op.model().to_owned(),
        }));
    };
    let children = sub_records(&op, spec).map_err(at)?;

    let scan = scan(&op, module);
    let must_flush = scan.reads_db
        || xid.as_ref().is_some_and(|x| batch::pending_xid(session, x))
        || scan.refs.iter().any(|r| batch::is_pending(session, r));
    if must_flush {
        batch::flush(session, tx).await?;
    }
    let outcome = batch::push(
        session,
        tx,
        Push {
            file,
            op: Arc::clone(&op),
            xid,
            foreign,
            extra,
            tree,
            refs: scan.refs,
            flush_now: !children.is_empty(),
        },
    )
    .await?;
    if children.is_empty() {
        return Ok(());
    }
    let Some(Outcome::Loaded(parent)) = outcome else {
        // skipped: the children are not loaded either (395, 446)
        return Ok(());
    };
    for (child, _field, inverse) in children {
        Box::pin(process(
            session,
            tx,
            file,
            Arc::new(child.clone()),
            Some((inverse, parent)),
            tree,
        ))
        .await?;
    }
    Ok(())
}

/// The values of a record whose references are resolved in `known`.
pub(crate) enum Built {
    /// The field values to write, in document order.
    Values(Vec<(String, Value)>),
    /// A `ref=` did not resolve and the record says `forcecreate="0"`: skip it.
    UnresolvedRef {
        /// The field.
        field: String,
        /// The external id, qualified.
        reference: String,
    },
}

/// The field values of `op`: every `<field>` coerced by the type of the
/// target field.
///
/// A `ref=` that is not in `known` as an id is an error, except with `forcecreate="0"`, which
/// skips the record (`:438-446`); a reference field takes `model,id` and always errors
/// (`:441-443`).
pub(crate) async fn build_values(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    records: &erp_orm::Records,
    op: &RecordOp,
    spec: &ModelSpec,
    known: &mut HashMap<String, Known>,
) -> Result<Built, LoadErrorKind> {
    let module = file.module();
    let mut pairs: Vec<(String, Value)> = Vec::new();
    for field in op.fields() {
        let name = field.name();
        let spec_field = spec.field(name);
        let value = match field.value() {
            FieldValue::Ref(id) => {
                let qualified = qualify(&id.to_string(), module);
                let is_reference =
                    spec_field.is_some_and(|f| matches!(f.ty, FieldType::Reference { .. }));
                let Some(target) = known.get(&qualified).and_then(|k| k.id()) else {
                    if op.forcecreate() == Some(false) && !is_reference {
                        return Ok(Built::UnresolvedRef {
                            field: name.to_owned(),
                            reference: qualified,
                        });
                    }
                    return Err(LoadErrorKind::ExternalIdNotFound { xid: qualified });
                };
                // a reference field takes `model,id`
                if is_reference {
                    let target_xid = XmlId::parse(&qualified).map_err(invalid)?;
                    let row = session.cache.get(&target_xid).ok_or_else(|| {
                        LoadErrorKind::ExternalIdNotFound {
                            xid: qualified.clone(),
                        }
                    })?;
                    Value::str(format!("{},{target}", row.model))
                } else {
                    Value::Int(target)
                }
            }
            FieldValue::Text(text) => Value::str(text),
            FieldValue::Eval { expr, model } => {
                // `obj` is bound only by the field's own `model=`
                let result = session
                    .eval_with_model(tx, module, expr, model.as_deref(), known)
                    .await?;
                to_value(&result).map_err(|got| LoadErrorKind::NotAFieldValue {
                    model: op.model().to_owned(),
                    field: name.to_owned(),
                    got,
                })?
            }
            FieldValue::Int(Some(i)) => Value::Int(*i),
            FieldValue::Int(None) => Value::False,
            FieldValue::Float(f) => Value::Float(*f),
            // the children follow the parent with the inverse field set
            FieldValue::SubRecords(_) => continue,
            // CSV files load as one `Csv` operation (`super::csv`); no reader puts a cell here
            FieldValue::CsvCell(_) => {
                return Err(LoadErrorKind::Internal(format!(
                    "field '{name}' of a <record> of {} holds a CSV cell, which only CSV \
                     operations carry",
                    op.model()
                )));
            }
            other => {
                let scope = values_ext::FormScope {
                    file,
                    records,
                    field: name,
                };
                values_ext::load_form(session, tx, &scope, other, known).await?
            }
        };
        let value = match spec_field {
            Some(f) => match values::coerce(op.model(), f, value)? {
                Some(v) => v,
                None => continue,
            },
            // an unknown field is the ORM's error at `vals_from_values`
            None => value,
        };
        match pairs.iter_mut().find(|(n, _)| n == name) {
            Some(slot) => slot.1 = value,
            None => pairs.push((name.to_owned(), value)),
        }
    }
    Ok(Built::Values(pairs))
}

pub(crate) fn invalid(e: crate::ast::InvalidXmlId) -> LoadErrorKind {
    LoadErrorKind::InvalidExternalId(e)
}
