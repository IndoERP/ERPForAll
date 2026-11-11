//! Attribute rules shared by every element:
//! - unknown attributes are errors (per-element allow-lists from the schema);

use std::sync::Arc;

use super::html;
use super::tree::{Child, Node, is_xml_whitespace, push_normalized};
use crate::ast::{
    AssetOp, DataOperation, DeleteOp, DeleteTarget, Expr, FieldOp, FieldValue, FunctionArg,
    FunctionOp, GroupCommand, MenuItemOp, OpEnv, RecordOp, TemplateOp, ValueOp, XmlId,
};
use crate::error::{ParseError, ParseErrorKind};

/// What the conversion needs from the reader.
pub(crate) struct Cx<'a> {
    pub(crate) src: &'a str,
    pub(crate) file: &'a Arc<str>,
    pub(crate) module: &'a str,
}

impl<'a> Cx<'a> {
    pub(crate) fn err(
        &self,
        line: usize,
        kind: ParseErrorKind,
        message: impl Into<String>,
    ) -> ParseError {
        ParseError::new(self.file, line, kind, message)
    }

    fn unknown_element(&self, parent: &Node, child: &Node, allowed: &str) -> ParseError {
        self.err(
            child.line,
            ParseErrorKind::UnknownElement {
                parent: parent.tag.clone(),
                tag: child.tag.clone(),
            },
            format!(
                "element <{}> is not allowed in <{}>; {allowed}",
                child.tag, parent.tag
            ),
        )
    }

    fn conflict(&self, node: &Node, message: impl Into<String>) -> ParseError {
        self.err(
            node.line,
            ParseErrorKind::ConflictingAttributes {
                tag: node.tag.clone(),
            },
            message,
        )
    }

    fn invalid_attr(&self, node: &Node, attr: &str, message: impl Into<String>) -> ParseError {
        self.err(
            node.line,
            ParseErrorKind::InvalidAttributeValue {
                tag: node.tag.clone(),
                attr: attr.to_string(),
            },
            message,
        )
    }

    fn invalid_value(&self, node: &Node, message: impl Into<String>) -> ParseError {
        self.err(
            node.line,
            ParseErrorKind::InvalidValue {
                tag: node.tag.clone(),
            },
            message,
        )
    }

    fn refused(&self, node: &Node, message: impl Into<String>) -> ParseError {
        self.err(
            node.line,
            ParseErrorKind::Refused {
                tag: node.tag.clone(),
            },
            message,
        )
    }

    fn missing_element(&self, node: &Node, tag: &str, message: impl Into<String>) -> ParseError {
        self.err(
            node.line,
            ParseErrorKind::MissingElement {
                parent: node.tag.clone(),
                tag: tag.to_string(),
            },
            message,
        )
    }

    fn source(&self, node: &Node, from: usize, to: usize) -> Result<&'a str, ParseError> {
        self.src.get(from..to).ok_or_else(|| {
            ParseError::syntax(
                self.file,
                node.line,
                format!(
                    "internal: source range {from}..{to} of <{}> is invalid",
                    node.tag
                ),
            )
        })
    }
}

// ---------------------------------------------------------------- attribute helpers

fn check_attrs(cx: &Cx<'_>, node: &Node, allowed: &[&str]) -> Result<(), ParseError> {
    for (key, _) in &node.attrs {
        if !allowed.contains(&key.as_str()) {
            return Err(cx.err(
                node.line,
                ParseErrorKind::UnknownAttribute {
                    tag: node.tag.clone(),
                    attr: key.clone(),
                },
                if allowed.is_empty() {
                    format!(
                        "unknown attribute '{key}' on <{}>; it takes no attributes",
                        node.tag
                    )
                } else {
                    format!(
                        "unknown attribute '{key}' on <{}>; allowed: {}",
                        node.tag,
                        allowed.join(", ")
                    )
                },
            ));
        }
    }
    Ok(())
}

/// An optional attribute; `""` counts as absent.
fn opt<'n>(node: &'n Node, name: &str) -> Option<&'n str> {
    node.attr(name).filter(|v| !v.is_empty())
}

fn required<'n>(cx: &Cx<'_>, node: &'n Node, name: &str) -> Result<&'n str, ParseError> {
    opt(node, name).ok_or_else(|| {
        cx.err(
            node.line,
            ParseErrorKind::MissingRequiredAttribute {
                tag: node.tag.clone(),
                attr: name.to_string(),
            },
            format!("<{}> requires a non-empty '{name}' attribute", node.tag),
        )
    })
}

fn xml_id(cx: &Cx<'_>, node: &Node, attr: &str, value: &str) -> Result<XmlId, ParseError> {
    XmlId::parse(value).map_err(|e| {
        ParseError::invalid_xml_id(
            cx.file,
            node.line,
            format!("attribute '{attr}' on <{}>", node.tag),
            e,
        )
    })
}

fn opt_xml_id(cx: &Cx<'_>, node: &Node, attr: &str) -> Result<Option<XmlId>, ParseError> {
    opt(node, attr)
        .map(|v| xml_id(cx, node, attr, v))
        .transpose()
}

fn bool_attr(cx: &Cx<'_>, node: &Node, name: &str) -> Result<Option<bool>, ParseError> {
    let Some(raw) = opt(node, name) else {
        return Ok(None);
    };
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        _ => Err(cx.invalid_attr(
            node,
            name,
            format!(
                "invalid boolean value '{raw}' for attribute '{name}' on <{}>; expected 1/0, \
                 true/false, yes/no or on/off",
                node.tag
            ),
        )),
    }
}

fn literal_bool_attr(cx: &Cx<'_>, node: &Node, name: &str) -> Result<Option<bool>, ParseError> {
    match opt(node, name) {
        None => Ok(None),
        Some("True") => Ok(Some(true)),
        Some("False") => Ok(Some(false)),
        Some(other) => Err(cx.invalid_attr(
            node,
            name,
            format!(
                "attribute '{name}' on <{}> must be 'True' or 'False', found '{other}' \
                 (the reference ERP ignores any other value)",
                node.tag
            ),
        )),
    }
}

fn int_attr(cx: &Cx<'_>, node: &Node, name: &str) -> Result<Option<i32>, ParseError> {
    let Some(raw) = opt(node, name) else {
        return Ok(None);
    };
    raw.trim().parse::<i32>().map(Some).map_err(|e| {
        cx.invalid_attr(
            node,
            name,
            format!(
                "invalid integer '{raw}' for attribute '{name}' on <{}>: {e}",
                node.tag
            ),
        )
    })
}

/// Text of a text-valued element (char, int, float, file path, bundle/path).
fn text_value(cx: &Cx<'_>, node: &Node) -> Result<String, ParseError> {
    let interrupted = node.children.iter().any(|c| {
        matches!(
            c,
            Child::Comment { .. } | Child::ProcessingInstruction { .. }
        )
    });
    if interrupted {
        let what = match node.attr("name") {
            Some(name) => format!("<{} name=\"{name}\">", node.tag),
            None => format!("<{}>", node.tag),
        };
        return Err(cx.refused(
            node,
            format!(
                "comment or processing instruction inside the text of {what}: the reference ERP keeps only \
                 the text before it; remove it"
            ),
        ));
    }
    Ok(node.text())
}

fn no_text(cx: &Cx<'_>, node: &Node) -> Result<(), ParseError> {
    match node.significant_text() {
        None => Ok(()),
        Some(text) => Err(cx.err(
            node.line,
            ParseErrorKind::UnexpectedText {
                tag: node.tag.clone(),
            },
            format!(
                "text is not allowed in <{}> here: {:?}",
                node.tag,
                text.trim().chars().take(80).collect::<String>()
            ),
        )),
    }
}

fn no_elements(cx: &Cx<'_>, node: &Node, allowed: &str) -> Result<(), ParseError> {
    match node.elements().next() {
        None => Ok(()),
        Some(child) => Err(cx.unknown_element(node, child, allowed)),
    }
}

fn no_content(cx: &Cx<'_>, node: &Node) -> Result<(), ParseError> {
    no_elements(cx, node, "this element takes no child elements here")?;
    no_text(cx, node)
}

// ---------------------------------------------------------------- containers

const CONTAINER_ATTRS: &[&str] = &["noupdate", "auto_sequence", "uid", "context"];

pub(crate) fn container_env(cx: &Cx<'_>, node: &Node, parent: &OpEnv) -> Result<OpEnv, ParseError> {
    check_attrs(cx, node, CONTAINER_ATTRS)?;
    // Absent or empty: `nodeattr2bool(el, 'noupdate', self.noupdate)` inherits, and
    // `nodeattr2bool(el, 'auto_sequence', False)` is false.
    let noupdate = bool_attr(cx, node, "noupdate")?.unwrap_or(parent.noupdate());
    let auto_sequence = bool_attr(cx, node, "auto_sequence")?.unwrap_or(false);
    let uid = opt_xml_id(cx, node, "uid")?;
    let context = opt(node, "context").map(Expr::new);
    Ok(parent.container(noupdate, auto_sequence, uid, context))
}

/// Convert one top-level operation element read under `env`.
pub(crate) fn operation(
    cx: &Cx<'_>,
    node: &Node,
    env: &OpEnv,
) -> Result<DataOperation, ParseError> {
    match node.tag.as_str() {
        "record" => record(cx, node, env).map(DataOperation::Record),
        "template" => template(cx, node, env).map(DataOperation::Template),
        "asset" => asset(cx, node, env).map(DataOperation::Asset),
        "menuitem" => menuitem(cx, node, env, None).map(DataOperation::MenuItem),
        "delete" => delete(cx, node, env).map(DataOperation::Delete),
        "function" => function(cx, node, env, false).map(DataOperation::Function),
        other => Err(cx.err(
            node.line,
            ParseErrorKind::UnknownElement {
                parent: String::new(),
                tag: other.to_string(),
            },
            format!("element <{other}> is not a data operation"),
        )),
    }
}

// ---------------------------------------------------------------- record and field

const RECORD_ATTRS: &[&str] = &["id", "model", "forcecreate", "uid", "context"];

fn record(cx: &Cx<'_>, node: &Node, container: &OpEnv) -> Result<RecordOp, ParseError> {
    check_attrs(cx, node, RECORD_ATTRS)?;
    let model = required(cx, node, "model")?.to_string();
    let id = opt_xml_id(cx, node, "id")?;
    if id.is_none() && opt(node, "forcecreate").is_some() {
        return Err(cx.conflict(
            node,
            "'forcecreate' on <record> requires an 'id' (import_xml.rng)",
        ));
    }
    let forcecreate = bool_attr(cx, node, "forcecreate")?;
    let uid = opt_xml_id(cx, node, "uid")?;
    let context = opt(node, "context").map(Expr::new);
    no_text(cx, node)?;
    let mut fields = Vec::new();
    let mut translations = Vec::new();
    for child in node.elements() {
        if child.tag != "field" {
            return Err(cx.unknown_element(node, child, "only <field> is allowed"));
        }
        let field = field(cx, child, container)?;
        // Translation fields (`name@fr_FR` or `<field lang="fr_FR">`) are not loaded into
        // standard record fields; they are captured on `RecordOp::translations`.
        if field.lang().is_some() {
            translations.push(field);
        } else {
            fields.push(field);
        }
    }
    Ok(RecordOp {
        id,
        model,
        forcecreate,
        fields,
        translations,
        env: container.with_own(uid, context),
        file: Arc::clone(cx.file),
        line: node.line,
        origin: None,
    })
}

const FIELD_ATTRS: &[&str] = &[
    "name", "type", "file", "ref", "eval", "search", "model", "use", "lang",
];
const VALUE_ATTRS: &[&str] = &["name", "type", "file", "eval", "search", "model", "use"];

/// The value source of a `<field>` or `<value>`.
enum Source<'n> {
    Search(&'n str),
    Ref(&'n str),
    Eval(&'n str),
    Typed {
        ty: Option<&'n str>,
        file: Option<&'n str>,
    },
}

/// At most one of `search`/`ref`/`eval`; none of them next to `type`/`file`; `model` only with
/// `eval`/`search`; `use` only with `search` (import_xml.rng `field` and `value`).
fn value_source<'n>(cx: &Cx<'_>, node: &'n Node) -> Result<Source<'n>, ParseError> {
    let search = opt(node, "search");
    let reference = opt(node, "ref");
    let eval = opt(node, "eval");
    let given: Vec<&str> = [("search", search), ("ref", reference), ("eval", eval)]
        .iter()
        .filter(|(_, v)| v.is_some())
        .map(|(k, _)| *k)
        .collect();
    if given.len() > 1 {
        return Err(cx.conflict(
            node,
            format!(
                "<{}> takes one value source; found {}",
                node.tag,
                given.join(" and ")
            ),
        ));
    }
    let ty = node.attr("type");
    let file = opt(node, "file");
    if let Some(src) = given.first()
        && (ty.is_some() || file.is_some())
    {
        return Err(cx.conflict(
            node,
            format!(
                "'{src}' on <{}> cannot be combined with 'type' or 'file'",
                node.tag
            ),
        ));
    }
    if opt(node, "model").is_some() && search.is_none() && eval.is_none() {
        return Err(cx.conflict(
            node,
            format!(
                "'model' on <{}> applies only with 'eval' or 'search'",
                node.tag
            ),
        ));
    }
    if opt(node, "use").is_some() && search.is_none() {
        return Err(cx.conflict(
            node,
            format!("'use' on <{}> applies only with 'search'", node.tag),
        ));
    }
    Ok(match (search, reference, eval) {
        (Some(s), _, _) => Source::Search(s),
        (_, Some(r), _) => Source::Ref(r),
        (_, _, Some(e)) => Source::Eval(e),
        _ => Source::Typed { ty, file },
    })
}

fn field(cx: &Cx<'_>, node: &Node, container: &OpEnv) -> Result<FieldOp, ParseError> {
    check_attrs(cx, node, FIELD_ATTRS)?;
    let raw_name = required(cx, node, "name")?;
    let (name, lang_from_name) = match raw_name.split_once('@') {
        Some((n, l)) => {
            if n.is_empty() || l.is_empty() {
                return Err(cx.invalid_attr(
                    node,
                    "name",
                    "name attribute with '@' must specify both field name and language (e.g. 'name@fr_FR')",
                ));
            }
            (n.to_string(), Some(l.to_string()))
        }
        None => (raw_name.to_string(), None),
    };
    // `lang=""` is a malformed language, not an absent one (`opt` would read it as absent)
    if node.attr("lang").is_some_and(str::is_empty) {
        return Err(cx.invalid_attr(
            node,
            "lang",
            "lang attribute must not be empty (e.g. lang=\"fr_FR\")",
        ));
    }
    let lang_attr = opt(node, "lang").map(str::to_string);
    if lang_from_name.is_some() && lang_attr.is_some() {
        return Err(cx.conflict(
            node,
            "cannot specify both 'name@lang' and 'lang' attribute on <field>",
        ));
    }
    let lang = lang_from_name.or(lang_attr);
    let model = opt(node, "model").map(str::to_string);
    let value = match value_source(cx, node)? {
        Source::Search(domain) => {
            no_content(cx, node)?;
            FieldValue::Search {
                domain: Expr::new(domain),
                model,
                use_field: opt(node, "use").map(str::to_string),
            }
        }
        Source::Ref(reference) => {
            no_content(cx, node)?;
            FieldValue::Ref(xml_id(cx, node, "ref", reference)?)
        }
        Source::Eval(expr) => {
            no_content(cx, node)?;
            FieldValue::Eval {
                expr: Expr::new(expr),
                model,
            }
        }
        Source::Typed { ty, file } => typed_field(cx, node, ty, file, container)?,
    };
    Ok(FieldOp {
        name,
        value,
        line: node.line,
        lang,
    })
}

fn typed_field(
    cx: &Cx<'_>,
    node: &Node,
    ty: Option<&str>,
    file: Option<&str>,
    container: &OpEnv,
) -> Result<FieldValue, ParseError> {
    match ty {
        None | Some("char") => {
            if let Some(file) = file {
                no_content(cx, node)?;
                return Ok(FieldValue::FileText(file.to_string()));
            }
            if ty.is_none() && node.elements().next().is_some() {
                // One2many sub-records; the schema allows text or records, not both.
                no_text(cx, node)?;
                let mut records = Vec::new();
                for child in node.elements() {
                    if child.tag != "record" {
                        return Err(cx.unknown_element(
                            node,
                            child,
                            "a <field> without type=\"xml\" or type=\"html\" holds text or \
                             <record> sub-records only",
                        ));
                    }
                    records.push(record(cx, child, container)?);
                }
                return Ok(FieldValue::SubRecords(records));
            }
            no_elements(cx, node, "type=\"char\" holds text only")?;
            Ok(FieldValue::Text(text_value(cx, node)?))
        }
        Some("file") => Ok(FieldValue::ModulePath(module_path(cx, node, file)?)),
        Some(t @ ("bytes" | "base64")) => {
            let file = file_only(cx, node, t, file)?;
            Ok(if t == "bytes" {
                FieldValue::FileBytes(file)
            } else {
                FieldValue::FileBase64(file)
            })
        }
        Some("int") => {
            no_file(cx, node, "int", file)?;
            no_elements(cx, node, "type=\"int\" holds text only")?;
            Ok(FieldValue::Int(parse_int(cx, node)?))
        }
        Some("float") => {
            no_file(cx, node, "float", file)?;
            no_elements(cx, node, "type=\"float\" holds text only")?;
            Ok(FieldValue::Float(parse_float(cx, node)?))
        }
        Some(t @ ("list" | "tuple")) => {
            no_file(cx, node, t, file)?;
            no_text(cx, node)?;
            let mut values = Vec::new();
            for child in node.elements() {
                if child.tag != "value" {
                    return Err(cx.unknown_element(node, child, "only <value> is allowed"));
                }
                values.push(value(cx, child)?);
            }
            if values.is_empty() {
                return Err(cx.missing_element(
                    node,
                    "value",
                    format!("<field type=\"{t}\"> needs at least one <value> (import_xml.rng)"),
                ));
            }
            Ok(if t == "list" {
                FieldValue::List(values)
            } else {
                FieldValue::Tuple(values)
            })
        }
        Some(t @ ("xml" | "html")) => {
            no_file(cx, node, t, file)?;
            no_text(cx, node)?;
            if node.elements().next().is_none() {
                return Err(cx.missing_element(
                    node,
                    "element",
                    format!(
                        "<field type=\"{t}\"> needs at least one child element (import_xml.rng)"
                    ),
                ));
            }
            if t == "xml" {
                Ok(FieldValue::Xml(xml_markup(cx, node)?))
            } else {
                Ok(FieldValue::Html(html::serialize_children(cx, node)?))
            }
        }
        Some(other) => Err(cx.invalid_attr(node, "type", format!("Unknown type '{other}'"))),
    }
}

/// `type="file"`: the path text.
fn module_path(cx: &Cx<'_>, node: &Node, file: Option<&str>) -> Result<String, ParseError> {
    if file.is_some() {
        return Err(cx.refused(
            node,
            "type=\"file\" with a 'file' attribute is refused (the reference ERP would read the path out of \
             that file): write the module path as the element text",
        ));
    }
    no_elements(cx, node, "type=\"file\" holds a path only")?;
    let text = text_value(cx, node)?;
    let path = text.trim();
    if path.is_empty() {
        return Err(cx.invalid_value(node, "type=\"file\" needs a path"));
    }
    Ok(path.to_string())
}

/// `type="bytes"`/`"base64"` read only from `file=`.
fn file_only(cx: &Cx<'_>, node: &Node, ty: &str, file: Option<&str>) -> Result<String, ParseError> {
    let Some(file) = file else {
        return Err(cx.invalid_value(node, format!("{ty} type is only compatible with file data")));
    };
    no_content(cx, node)?;
    Ok(file.to_string())
}

fn no_file(cx: &Cx<'_>, node: &Node, ty: &str, file: Option<&str>) -> Result<(), ParseError> {
    match file {
        None => Ok(()),
        Some(_) => Err(cx.conflict(
            node,
            format!("'file' cannot be used with type=\"{ty}\" (import_xml.rng)"),
        )),
    }
}

fn parse_int(cx: &Cx<'_>, node: &Node) -> Result<Option<i64>, ParseError> {
    let text = text_value(cx, node)?;
    let value = text.trim();
    if value == "None" {
        return Ok(None);
    }
    value.parse::<i64>().map(Some).map_err(|e| {
        cx.invalid_value(
            node,
            format!("invalid integer '{value}' in <{}>: {e}", node.tag),
        )
    })
}

fn parse_float(cx: &Cx<'_>, node: &Node) -> Result<f64, ParseError> {
    let text = text_value(cx, node)?;
    let value = text.trim();
    value.parse::<f64>().map_err(|e| {
        cx.invalid_value(
            node,
            format!("invalid float '{value}' in <{}>: {e}", node.tag),
        )
    })
}

/// `type="xml"`: the markup from the first child node to the
/// end tag, several element roots wrapped in `<data>`, line ends normalised.
fn xml_markup(cx: &Cx<'_>, node: &Node) -> Result<String, ParseError> {
    let first = node.children.iter().find_map(Child::start);
    let Some(first) = first else {
        return Err(cx.missing_element(node, "element", "type=\"xml\" needs child elements"));
    };
    let body = cx.source(node, first, node.inner_end)?;
    let mut out = String::from("<?xml version=\"1.0\"?>\n");
    if node.elements().count() > 1 {
        out.push_str("<data>");
        push_normalized(&mut out, body);
        out.push_str("</data>");
    } else {
        push_normalized(&mut out, body);
    }
    Ok(out)
}

fn value(cx: &Cx<'_>, node: &Node) -> Result<ValueOp, ParseError> {
    check_attrs(cx, node, VALUE_ATTRS)?;
    no_elements(cx, node, "<value> holds text only (import_xml.rng)")?;
    let model = opt(node, "model").map(str::to_string);
    Ok(match value_source(cx, node)? {
        Source::Search(domain) => {
            no_text(cx, node)?;
            ValueOp::Search {
                domain: Expr::new(domain),
                model,
                use_field: opt(node, "use").map(str::to_string),
            }
        }
        Source::Eval(expr) => {
            no_text(cx, node)?;
            ValueOp::Eval {
                expr: Expr::new(expr),
                model,
            }
        }
        // `ref` is not in VALUE_ATTRS, so `check_attrs` already refused it.
        Source::Ref(_) => {
            return Err(cx.err(
                node.line,
                ParseErrorKind::UnknownAttribute {
                    tag: node.tag.clone(),
                    attr: "ref".to_string(),
                },
                "unknown attribute 'ref' on <value>",
            ));
        }
        Source::Typed { ty, file } => match ty {
            None | Some("char") => match file {
                Some(file) => {
                    no_text(cx, node)?;
                    ValueOp::FileText(file.to_string())
                }
                None => ValueOp::Text(text_value(cx, node)?),
            },
            Some("file") => ValueOp::ModulePath(module_path(cx, node, file)?),
            Some("bytes") => ValueOp::FileBytes(file_only(cx, node, "bytes", file)?),
            Some("base64") => ValueOp::FileBase64(file_only(cx, node, "base64", file)?),
            Some("int") => {
                no_file(cx, node, "int", file)?;
                ValueOp::Int(parse_int(cx, node)?)
            }
            Some("float") => {
                no_file(cx, node, "float", file)?;
                ValueOp::Float(parse_float(cx, node)?)
            }
            Some(t @ ("list" | "tuple")) => {
                no_file(cx, node, t, file)?;
                no_text(cx, node)?;
                if t == "list" {
                    ValueOp::EmptyList
                } else {
                    ValueOp::EmptyTuple
                }
            }
            Some(other) => {
                return Err(cx.invalid_attr(node, "type", format!("Unknown type '{other}'")));
            }
        },
    })
}

// ---------------------------------------------------------------- function

const FUNCTION_ATTRS: &[&str] = &["model", "name", "eval", "uid", "context"];

const UNSAFE_NAMES: &[&str] = &[
    "f_builtins",
    "f_code",
    "f_globals",
    "f_locals",
    "f_generator",
    "func_code",
    "func_globals",
    "co_code",
    "_co_code_adaptive",
    "mro",
    "tb_frame",
    "gi_code",
    "gi_frame",
    "gi_yieldfrom",
    "cr_await",
    "cr_code",
    "cr_frame",
    "ag_await",
    "ag_code",
    "ag_frame",
];

fn function(cx: &Cx<'_>, node: &Node, env: &OpEnv, nested: bool) -> Result<FunctionOp, ParseError> {
    check_attrs(cx, node, FUNCTION_ATTRS)?;
    let model = required(cx, node, "model")?.to_string();
    let name = required(cx, node, "name")?.to_string();
    if name.contains("__") || UNSAFE_NAMES.contains(&name.as_str()) {
        return Err(cx.err(
            node.line,
            ParseErrorKind::ForbiddenName { name: name.clone() },
            format!("Access to forbidden name '{name}'"),
        ));
    }
    let uid = opt_xml_id(cx, node, "uid")?;
    let context = opt(node, "context").map(Expr::new);
    let env = if nested {
        if uid.is_some() || context.is_some() {
            return Err(cx.refused(
                node,
                "'uid'/'context' on a nested <function> would be ignored (the reference ERP evaluates it in \
                 the enclosing function's environment); set them on the outer <function>",
            ));
        }
        env.clone()
    } else {
        env.with_own(uid, context)
    };
    let eval = opt(node, "eval").map(Expr::new);
    no_text(cx, node)?;
    let mut args = Vec::new();
    let mut kwargs: Vec<(String, ValueOp)> = Vec::new();
    for child in node.elements() {
        match child.tag.as_str() {
            "value" => {
                let v = value(cx, child)?;
                match opt(child, "name") {
                    Some(key) => {
                        if kwargs.iter().any(|(k, _)| k == key) {
                            return Err(cx.err(
                                child.line,
                                ParseErrorKind::DuplicateKeywordArgument {
                                    name: key.to_string(),
                                },
                                format!("repeated keyword argument '{key}' in <function>"),
                            ));
                        }
                        kwargs.push((key.to_string(), v));
                    }
                    None => args.push(FunctionArg::Value(v)),
                }
            }
            "function" => args.push(FunctionArg::Function(Box::new(function(
                cx, child, &env, true,
            )?))),
            _ => {
                return Err(cx.unknown_element(
                    node,
                    child,
                    "only <value> and <function> are allowed",
                ));
            }
        }
    }
    if eval.is_some() && (!args.is_empty() || !kwargs.is_empty()) {
        return Err(cx.conflict(
            node,
            "<function> takes either 'eval' or <value>/<function> children (import_xml.rng)",
        ));
    }
    Ok(FunctionOp {
        model,
        name,
        eval,
        args,
        kwargs,
        env,
        file: Arc::clone(cx.file),
        line: node.line,
    })
}

// ---------------------------------------------------------------- menuitem

const MENU_ATTRS: &[&str] = &[
    "id", "name", "parent", "sequence", "action", "web_icon", "groups", "active",
];

/// `<menuitem>`; `enclosing` is the parent menu for nested ones. Schema rules: a nested menu
/// has no `parent` or `web_icon`; a menu with `parent` has no `web_icon`; a nested menu or a menu
/// with `parent` has either an `action` or child menus.
fn menuitem(
    cx: &Cx<'_>,
    node: &Node,
    env: &OpEnv,
    enclosing: Option<&XmlId>,
) -> Result<MenuItemOp, ParseError> {
    check_attrs(cx, node, MENU_ATTRS)?;
    let id = xml_id(cx, node, "id", required(cx, node, "id")?)?;
    let parent_attr = opt(node, "parent");
    let web_icon = opt(node, "web_icon");
    if enclosing.is_some() {
        if parent_attr.is_some() {
            return Err(cx.conflict(
                node,
                "a nested <menuitem> takes the enclosing menu as parent; remove 'parent'",
            ));
        }
        if web_icon.is_some() {
            return Err(cx.conflict(node, "only top-level menus have a 'web_icon'"));
        }
    } else if parent_attr.is_some() && web_icon.is_some() {
        return Err(cx.conflict(
            node,
            "a <menuitem> with 'parent' cannot have a 'web_icon' (import_xml.rng)",
        ));
    }
    let name = opt(node, "name").map(str::to_string);
    let sequence = int_attr(cx, node, "sequence")?;
    let action = opt_xml_id(cx, node, "action")?;
    let active = bool_attr(cx, node, "active")?;
    let groups = menu_groups(cx, node)?;
    no_text(cx, node)?;
    for child in node.elements() {
        if child.tag != "menuitem" {
            return Err(cx.unknown_element(node, child, "only nested <menuitem> is allowed"));
        }
    }
    let is_submenu = enclosing.is_some() || parent_attr.is_some();
    if is_submenu && action.is_some() && node.elements().next().is_some() {
        return Err(cx.conflict(
            node,
            "a submenu has either an 'action' or child menus, not both (import_xml.rng)",
        ));
    }
    let parent = match enclosing {
        Some(p) => Some(p.clone()),
        None => opt_xml_id(cx, node, "parent")?,
    };
    let mut children = Vec::new();
    for child in node.elements() {
        children.push(menuitem(cx, child, env, Some(&id))?);
    }
    Ok(MenuItemOp {
        id,
        name,
        parent,
        sequence,
        action,
        web_icon: web_icon.map(str::to_string),
        groups,
        active,
        children,
        env: env.clone(),
        file: Arc::clone(cx.file),
        line: node.line,
    })
}

/// `groups="a,-b"`: link `a`, unlink `b`; empty entries skipped.
fn menu_groups(cx: &Cx<'_>, node: &Node) -> Result<Vec<GroupCommand>, ParseError> {
    let Some(groups) = opt(node, "groups") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in groups.split(',') {
        if entry.is_empty() {
            continue;
        }
        out.push(match entry.strip_prefix('-') {
            Some(rest) => GroupCommand::Unlink(xml_id(cx, node, "groups", rest)?),
            None => GroupCommand::Link(xml_id(cx, node, "groups", entry)?),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- delete

const DELETE_ATTRS: &[&str] = &["model", "id", "search"];

fn delete(cx: &Cx<'_>, node: &Node, env: &OpEnv) -> Result<DeleteOp, ParseError> {
    check_attrs(cx, node, DELETE_ATTRS)?;
    let model = required(cx, node, "model")?.to_string();
    let target = match (opt(node, "id"), opt(node, "search")) {
        (Some(id), None) => DeleteTarget::Id(xml_id(cx, node, "id", id)?),
        (None, Some(search)) => DeleteTarget::Search(Expr::new(search)),
        (Some(_), Some(_)) => {
            return Err(cx.conflict(
                node,
                "<delete> takes either 'id' or 'search', not both (import_xml.rng)",
            ));
        }
        (None, None) => {
            return Err(cx.err(
                node.line,
                ParseErrorKind::MissingRequiredAttribute {
                    tag: node.tag.clone(),
                    attr: "id".to_string(),
                },
                "<delete> requires a non-empty 'id' or 'search' attribute",
            ));
        }
    };
    no_content(cx, node)?;
    Ok(DeleteOp {
        model,
        target,
        env: env.clone(),
        file: Arc::clone(cx.file),
        line: node.line,
    })
}

// ---------------------------------------------------------------- template

/// The schema's attribute list for `<template>` (import_xml.rng).
const TEMPLATE_ATTRS: &[&str] = &[
    "id",
    "t-name",
    "name",
    "forcecreate",
    "context",
    "priority",
    "key",
    "website_id",
    "technical_usage",
    "track",
    "inherit_id",
    "primary",
    "groups",
    "active",
    "customize_show",
    "data-number-of-elements",
    "data-number-of-elements-sm",
    "data-number-of-elements-fetch",
    "data-row-per-slide",
    "data-arrow-position",
    "data-extra-classes",
    "data-extra-snippet-classes",
    "data-container-classes",
    "data-content-classes",
    "data-column-classes",
    "data-thumb",
];

const TEMPLATE_RECORD_ATTRS: &[&str] = &[
    "id",
    "forcecreate",
    "context",
    "track",
    "priority",
    "inherit_id",
    "technical_usage",
    "website_id",
    "key",
    "active",
    "customize_show",
    "groups",
    "primary",
];

fn template(cx: &Cx<'_>, node: &Node, env: &OpEnv) -> Result<TemplateOp, ParseError> {
    check_attrs(cx, node, TEMPLATE_ATTRS)?;
    let Some((attr, raw_id)) = opt(node, "id")
        .map(|v| ("id", v))
        .or_else(|| opt(node, "t-name").map(|v| ("t-name", v)))
    else {
        return Err(cx.err(
            node.line,
            ParseErrorKind::MissingRequiredAttribute {
                tag: node.tag.clone(),
                attr: "id".to_string(),
            },
            "<template> requires a non-empty 'id' or 't-name' attribute",
        ));
    };
    let id = xml_id(cx, node, attr, raw_id)?;
    let full_id = id.complete_name(cx.module);
    let inherit_id = opt_xml_id(cx, node, "inherit_id")?;
    let primary = match opt(node, "primary") {
        None => false,
        Some("True") => true,
        Some(other) => {
            return Err(cx.invalid_attr(
                node,
                "primary",
                format!("'primary' on <template> must be 'True', found '{other}' (import_xml.rng)"),
            ));
        }
    };
    if primary && inherit_id.is_none() {
        return Err(cx.conflict(
            node,
            "primary=\"True\" on <template> requires 'inherit_id' (import_xml.rng)",
        ));
    }
    let groups = template_groups(cx, node)?;
    let context = opt(node, "context").map(Expr::new);
    let arch = template_arch(cx, node, &full_id, inherit_id.is_some(), primary)?;
    Ok(TemplateOp {
        id,
        full_id,
        t_name: opt(node, "t-name").map(str::to_string),
        name: opt(node, "name").map(str::to_string),
        forcecreate: bool_attr(cx, node, "forcecreate")?,
        priority: int_attr(cx, node, "priority")?,
        key: opt(node, "key").map(str::to_string),
        website_id: opt_xml_id(cx, node, "website_id")?,
        technical_usage: opt(node, "technical_usage").map(str::to_string),
        track: bool_attr(cx, node, "track")?,
        inherit_id,
        primary,
        groups,
        active: literal_bool_attr(cx, node, "active")?,
        customize_show: literal_bool_attr(cx, node, "customize_show")?,
        arch,
        env: env.with_own(None, context),
        file: Arc::clone(cx.file),
        line: node.line,
    })
}

fn template_groups(cx: &Cx<'_>, node: &Node) -> Result<Vec<GroupCommand>, ParseError> {
    let Some(groups) = opt(node, "groups") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in groups.split(',') {
        if entry.starts_with('-') {
            return Err(cx.invalid_attr(
                node,
                "groups",
                format!("'{entry}': <template> groups cannot remove a group; only menus support '-group'"),
            ));
        }
        out.push(GroupCommand::Link(xml_id(cx, node, "groups", entry)?));
    }
    Ok(out)
}

fn template_arch(
    cx: &Cx<'_>,
    node: &Node,
    full_id: &str,
    inherit: bool,
    primary: bool,
) -> Result<String, ParseError> {
    let tag = if inherit { "data" } else { "t" };
    let mut out = String::new();
    out.push('<');
    out.push_str(tag);
    let mut t_name_written = false;
    for (key, value) in &node.attrs {
        if TEMPLATE_RECORD_ATTRS.contains(&key.as_str()) {
            continue;
        }
        if key == "t-name" && !inherit {
            html::push_attribute(&mut out, key, full_id);
            t_name_written = true;
        } else {
            html::push_attribute(&mut out, key, value);
        }
    }
    if !inherit && !t_name_written {
        html::push_attribute(&mut out, "t-name", full_id);
    }
    let inner = cx.source(node, node.inner_start, node.inner_end)?;
    if inner.is_empty() && !primary {
        out.push_str("/>");
        return Ok(out);
    }
    out.push('>');
    push_normalized(&mut out, inner);
    if primary {
        out.push_str("<xpath expr=\".\" position=\"attributes\"><attribute name=\"t-name\">");
        html::push_text(&mut out, full_id);
        out.push_str("</attribute></xpath>");
    }
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
    Ok(out)
}

// ---------------------------------------------------------------- asset

const ASSET_ATTRS: &[&str] = &["id", "name", "active"];

fn asset(cx: &Cx<'_>, node: &Node, env: &OpEnv) -> Result<AssetOp, ParseError> {
    check_attrs(cx, node, ASSET_ATTRS)?;
    let id = xml_id(cx, node, "id", required(cx, node, "id")?)?;
    let name = required(cx, node, "name")?.to_string();
    let active = literal_bool_attr(cx, node, "active")?;
    no_text(cx, node)?;
    let mut bundle: Option<(String, Option<String>)> = None;
    let mut path: Option<String> = None;
    let mut fields = Vec::new();
    for child in node.elements() {
        match child.tag.as_str() {
            "bundle" => {
                if bundle.is_some() {
                    return Err(repeated(cx, node, child));
                }
                check_attrs(cx, child, &["directive"])?;
                let text = asset_text(cx, child)?;
                bundle = Some((text, opt(child, "directive").map(str::to_string)));
            }
            "path" => {
                if path.is_some() {
                    return Err(repeated(cx, node, child));
                }
                check_attrs(cx, child, &[])?;
                path = Some(asset_text(cx, child)?);
            }
            "field" => {
                let field = field(cx, child, env)?;
                // an <asset> has no translated fields: refuse instead of dropping the value
                if field.lang().is_some() {
                    return Err(cx.refused(
                        child,
                        format!(
                            "translated <field name=\"{}\"> (name@lang or lang=) is not allowed in <asset>",
                            field.name()
                        ),
                    ));
                }
                fields.push(field);
            }
            _ => {
                return Err(cx.unknown_element(node, child, "allowed: <bundle>, <path>, <field>"));
            }
        }
    }
    let Some((bundle, directive)) = bundle else {
        return Err(cx.missing_element(node, "bundle", "<asset> requires one <bundle>"));
    };
    let Some(path) = path else {
        return Err(cx.missing_element(node, "path", "<asset> requires one <path>"));
    };
    Ok(AssetOp {
        id,
        name,
        active,
        bundle,
        directive,
        path,
        fields,
        env: env.clone(),
        file: Arc::clone(cx.file),
        line: node.line,
    })
}

fn repeated(cx: &Cx<'_>, parent: &Node, child: &Node) -> ParseError {
    cx.err(
        child.line,
        ParseErrorKind::RepeatedElement {
            parent: parent.tag.clone(),
            tag: child.tag.clone(),
        },
        format!("<{}> may appear only once in <{}>", child.tag, parent.tag),
    )
}

fn asset_text(cx: &Cx<'_>, node: &Node) -> Result<String, ParseError> {
    no_elements(cx, node, "it holds text only")?;
    let text = text_value(cx, node)?;
    if is_xml_whitespace(&text) {
        return Err(cx.invalid_value(node, format!("<{}> must not be empty", node.tag)));
    }
    Ok(text)
}
