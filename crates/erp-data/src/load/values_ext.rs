//! The value forms that read the database or the addon's files: the
//! `search=` form, `type="file"`, `file=`, lists and tuples, and `type="xml"`/`"html"` with
//! `%(xid)d` markers.
//!
//! [`load_form`] answers for one `<field>` of a record; [`value_op`] for one `<value>` of a list
//! or tuple (and, for `<function>`, one argument). Both return an `erp_fields::Value` that the
//! caller coerces by the field's type ([`super::values::coerce`]).
//!
//!   The marker names the text up to the first `)`.
//! - The `obj` of `eval` stays unbound.

use std::collections::HashMap;
use std::sync::Arc;

use erp_fields::{FieldType, Value};
use erp_orm::{Env, Records, Transaction};

use super::error::{DataNotice, NoticeKind};
use super::expr::{Known, qualify, to_value};
use super::{FileContext, LoadErrorKind, LoadSession};
use crate::ast::{FieldValue, ValueOp};

/// Where a `<field>` value is being built.
pub(crate) struct FormScope<'a> {
    /// The file being loaded.
    pub(crate) file: &'a FileContext,
    /// The records of the field's model (its environment is the operation's).
    pub(crate) records: &'a Records,
    /// The field's name.
    pub(crate) field: &'a str,
}

/// The value of a `<field>` form that needs the database or the addon's files.
///
/// # Errors
/// [`LoadErrorKind`]: `SearchNeedsModel`, `ModelMissing` for a `search=` model, `File` /
/// `FileNotText` for files, `ExternalIdNotFound` for an unknown `%(xid)d`, `Eval` and
/// `NotAFieldValue` for the `search=` domain.
pub(crate) async fn load_form(
    session: &mut LoadSession,
    tx: &mut Transaction,
    scope: &FormScope<'_>,
    form: &FieldValue,
    known: &mut HashMap<String, Known>,
) -> Result<Value, LoadErrorKind> {
    let module = scope.file.module();
    match form {
        FieldValue::Search {
            domain,
            model,
            use_field,
        } => {
            let field = scope.records.spec().field(scope.field);
            // `model="..."`, else the comodel of a relational field
            let comodel = field.and_then(|f| f.ty.comodel()).map(str::to_owned);
            let target = model
                .clone()
                .or(comodel)
                .ok_or(LoadErrorKind::SearchNeedsModel)?;
            let found = search_values(
                session,
                tx,
                module,
                scope.records.env(),
                &target,
                domain,
                use_field.as_deref(),
                known,
            )
            .await?;
            if field.is_some_and(|f| matches!(f.ty, FieldType::Many2many { .. })) {
                // `[Command.set(ids)]`
                Ok(Value::List(
                    vec![Value::List(
                        vec![Value::Int(6), Value::Int(0), Value::List(found.into())].into(),
                    )]
                    .into(),
                ))
            } else {
                Ok(found.into_iter().next().map_or(Value::False, first_scalar))
            }
        }
        FieldValue::ModulePath(path) => module_path(session, module, path).await,
        FieldValue::FileText(path) => file_text(session, path).await,
        FieldValue::FileBytes(path) => file_bytes(session, path).await,
        FieldValue::FileBase64(path) => {
            let bytes = read_addon_file(session, path).await?;
            session.notices.push(DataNotice::new(
                module,
                scope.file.path(),
                NoticeKind::DeprecatedBase64 {
                    model: scope.records.spec().name.to_owned(),
                    field: scope.field.to_owned(),
                },
            ));
            Ok(Value::str(base64(&bytes)))
        }
        FieldValue::List(items) | FieldValue::Tuple(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(
                    list_item(session, tx, scope.file, scope.records.env(), item, known).await?,
                );
            }
            Ok(Value::List(out.into()))
        }
        FieldValue::Xml(text) => {
            if scope
                .records
                .spec()
                .field(scope.field)
                .is_some_and(|f| matches!(f.ty, FieldType::Html))
            {
                session.notices.push(DataNotice::new(
                    module,
                    scope.file.path(),
                    NoticeKind::HtmlDeclaredXml {
                        model: scope.records.spec().name.to_owned(),
                        field: scope.field.to_owned(),
                    },
                ));
            }
            substitute(session, tx, module, text, known)
                .await
                .map(Value::str)
        }
        FieldValue::Html(text) => substitute(session, tx, module, text, known)
            .await
            .map(Value::str),
        // `FieldValue` is non-exhaustive: a form the reader gains later must be handled here
        other => Err(LoadErrorKind::Internal(format!(
            "the data loader does not know the <field> form {other:?}"
        ))),
    }
}

/// The value of one `<value>` of a list, a tuple or a `<function>` argument (`_eval_xml` on a
/// `value` node). A `search=` needs its own `model`.
///
/// # Errors
/// As [`load_form`].
pub(crate) async fn value_op(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    env: &Env,
    op: &ValueOp,
    known: &mut HashMap<String, Known>,
) -> Result<Value, LoadErrorKind> {
    let module = file.module();
    match op {
        ValueOp::Text(text) => Ok(Value::str(text)),
        ValueOp::Eval { expr, model } => {
            let result = session
                .eval_with_model(tx, module, expr, model.as_deref(), known)
                .await?;
            to_value(&result).map_err(|got| LoadErrorKind::NotAFieldValue {
                model: String::new(),
                field: "<value>".to_owned(),
                got,
            })
        }
        ValueOp::Search {
            domain,
            model,
            use_field,
        } => {
            let target = model.clone().ok_or(LoadErrorKind::SearchNeedsModel)?;
            let found = search_values(
                session,
                tx,
                module,
                env,
                &target,
                domain,
                use_field.as_deref(),
                known,
            )
            .await?;
            Ok(found.into_iter().next().map_or(Value::False, first_scalar))
        }
        ValueOp::ModulePath(path) => module_path(session, module, path).await,
        ValueOp::FileText(path) => file_text(session, path).await,
        ValueOp::FileBytes(path) => file_bytes(session, path).await,
        ValueOp::FileBase64(path) => {
            let bytes = read_addon_file(session, path).await?;
            Ok(Value::str(base64(&bytes)))
        }
        ValueOp::Int(Some(i)) => Ok(Value::Int(*i)),
        ValueOp::Int(None) => Ok(Value::False),
        ValueOp::Float(f) => Ok(Value::Float(*f)),
        ValueOp::EmptyList | ValueOp::EmptyTuple => Ok(Value::List(Vec::new().into())),
    }
}

/// A `<value>` inside a `type="list"` / `"tuple"`: Python's `None` stays `null` there (a bare
/// field value `None` is `False`, "not set"; an element of a stored list is data).
async fn list_item(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    env: &Env,
    op: &ValueOp,
    known: &mut HashMap<String, Known>,
) -> Result<Value, LoadErrorKind> {
    let null = || Value::Json(Arc::new(erp_fields::serde_json::Value::Null));
    match op {
        ValueOp::Int(None) => Ok(null()),
        ValueOp::Eval { expr, model } => {
            let result = session
                .eval_with_model(tx, file.module(), expr, model.as_deref(), known)
                .await?;
            if matches!(result, erp_expr::ExprValue::None) {
                return Ok(null());
            }
            to_value(&result).map_err(|got| LoadErrorKind::NotAFieldValue {
                model: String::new(),
                field: "<value>".to_owned(),
                got,
            })
        }
        other => value_op(session, tx, file, env, other, known).await,
    }
}

// ---- search= -------------------------------------------------------------------------------

/// The `use` values (default `id`) of the records `domain` finds on `target`, in search order.
#[allow(
    clippy::too_many_arguments,
    reason = "one internal call site pair; a params struct would only rename the arguments"
)]
async fn search_values(
    session: &mut LoadSession,
    tx: &mut Transaction,
    module: &str,
    env: &Env,
    target: &str,
    domain: &crate::ast::Expr,
    use_field: Option<&str>,
    known: &mut HashMap<String, Known>,
) -> Result<Vec<Value>, LoadErrorKind> {
    if env.registry().model(target).is_none() {
        return Err(LoadErrorKind::ModelMissing {
            what: "search=".to_owned(),
            model: target.to_owned(),
        });
    }
    let result = session.eval(tx, module, domain, known).await?;
    let value = to_value(&result).map_err(|got| LoadErrorKind::NotAFieldValue {
        model: target.to_owned(),
        field: "search".to_owned(),
        got,
    })?;
    let domain = erp_orm::web::domain_from_value(&value).map_err(LoadErrorKind::Orm)?;
    let found = env
        .records(target)
        .map_err(LoadErrorKind::Orm)?
        .search(tx, &domain)
        .await
        .map_err(LoadErrorKind::Orm)?;
    match use_field.unwrap_or("id") {
        "id" => Ok(found.ids().into_iter().map(Value::Int).collect()),
        other => found.values(tx, other).await.map_err(LoadErrorKind::Orm),
    }
}

/// `f_val = s[0][f_use]`: a many2one read as `[id, name]` is its id.
fn first_scalar(value: Value) -> Value {
    match value {
        Value::List(items) => items.first().cloned().unwrap_or(Value::False),
        other => other,
    }
}

// ---- files ---------------------------------------------------------------------------------

async fn module_path(
    session: &mut LoadSession,
    module: &str,
    path: &str,
) -> Result<Value, LoadErrorKind> {
    // the file must exist in the module; the content is not read
    session
        .options
        .files()
        .check(module, path)
        .await
        .map_err(LoadErrorKind::File)?;
    Ok(Value::str(format!("{module},{path}")))
}

/// `file="module/path"`: the first component names the addon (`file_open`).
async fn read_addon_file(session: &mut LoadSession, path: &str) -> Result<Vec<u8>, LoadErrorKind> {
    let Some((module, rest)) = path
        .split_once('/')
        .filter(|(m, r)| !m.is_empty() && !r.is_empty())
    else {
        return Err(LoadErrorKind::File(super::FileReadError::Failed(format!(
            "file=\"{path}\" must be written <module>/<path inside the module>"
        ))));
    };
    let bytes = session
        .options
        .files()
        .read(module, rest)
        .await
        .map_err(LoadErrorKind::File)?;
    session.read_bytes = session.read_bytes.saturating_add(bytes.len());
    Ok(bytes)
}

async fn file_text(session: &mut LoadSession, path: &str) -> Result<Value, LoadErrorKind> {
    let bytes = read_addon_file(session, path).await?;
    let text = String::from_utf8(bytes).map_err(|_| {
        let (module, rest) = path.split_once('/').unwrap_or(("", path));
        LoadErrorKind::FileNotText {
            module: module.to_owned(),
            path: rest.to_owned(),
        }
    })?;
    Ok(Value::str(text))
}

async fn file_bytes(session: &mut LoadSession, path: &str) -> Result<Value, LoadErrorKind> {
    let bytes = read_addon_file(session, path).await?;
    Ok(Value::Bytes(Arc::from(bytes)))
}

/// Standard base64 with padding (RFC 4648 section 4), what `base64.b64encode` gives.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        let at = |shift: u32| char::from(ALPHABET[((n >> shift) & 63) as usize]);
        out.push(at(18));
        out.push(at(12));
        out.push(if chunk.len() > 1 { at(6) } else { '=' });
        out.push(if chunk.len() > 2 { at(0) } else { '=' });
    }
    out
}

// ---- type="xml" / type="html": %(xid)d -----------------------------------------------------

/// One piece of a text with `%(xid)d` markers.
enum Piece {
    Text(String),
    Marker(String),
}

/// Split `text` into literal pieces and markers; `%%` becomes `%`.
fn pieces(text: &str, module: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut literal = String::new();
    let mut at = 0;
    while let Some(ch) = text[at..].chars().next() {
        if ch == '%' {
            let rest = &text[at + 1..];
            if rest.starts_with('%') {
                literal.push('%');
                at += 2;
                continue;
            }
            if let Some(inner) = rest.strip_prefix('(')
                && let Some(close) = inner.find(')')
                && matches!(inner[close + 1..].chars().next(), Some('d' | 's'))
            {
                if !literal.is_empty() {
                    out.push(Piece::Text(std::mem::take(&mut literal)));
                }
                out.push(Piece::Marker(qualify(&inner[..close], module)));
                // `%(`, the name, `)` and the conversion letter
                at += 2 + close + 2;
                continue;
            }
        }
        literal.push(ch);
        at += ch.len_utf8();
    }
    if !literal.is_empty() {
        out.push(Piece::Text(literal));
    }
    out
}

/// `text` with every `%(xid)d` / `%(xid)s` replaced by the record id: one lookup statement for
/// the ids the session does not know.
async fn substitute(
    session: &mut LoadSession,
    tx: &mut Transaction,
    module: &str,
    text: &str,
    known: &mut HashMap<String, Known>,
) -> Result<String, LoadErrorKind> {
    let parts = pieces(text, module);
    let names: Vec<String> = parts
        .iter()
        .filter_map(|p| match p {
            Piece::Marker(name) => Some(name.clone()),
            Piece::Text(_) => None,
        })
        .collect();
    session.resolve(tx, &names, known).await?;
    let mut out = String::with_capacity(text.len());
    for part in parts {
        match part {
            Piece::Text(t) => out.push_str(&t),
            Piece::Marker(name) => match known.get(&name).and_then(|k| k.id()) {
                Some(id) => out.push_str(&id.to_string()),
                None => return Err(LoadErrorKind::ExternalIdNotFound { xid: name }),
            },
        }
    }
    Ok(out)
}

// ---- tests of the pure parts ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(text: &str) -> Vec<String> {
        pieces(text, "m")
            .into_iter()
            .map(|p| match p {
                Piece::Text(t) => format!("T:{t}"),
                Piece::Marker(n) => format!("M:{n}"),
            })
            .collect()
    }

    #[test]
    fn markers_and_escapes_are_found_where_reference_finds_them() {
        assert_eq!(rendered("a %(x)d b"), ["T:a ", "M:m.x", "T: b"]);
        assert_eq!(rendered("%(o.y)s"), ["M:o.y"]);
        assert_eq!(rendered("100%% sure"), ["T:100% sure"]);
        assert_eq!(rendered("%%(x)d"), ["T:%(x)d"]);
        assert_eq!(rendered("%(x)d%(y)d"), ["M:m.x", "M:m.y"]);
        // not a marker: no closing parenthesis, or another conversion letter
        assert_eq!(rendered("%(x"), ["T:%(x"]);
        assert_eq!(rendered("%(x)f"), ["T:%(x)f"]);
        assert_eq!(rendered("é%(x)dü"), ["T:é", "M:m.x", "T:ü"]);
    }

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
