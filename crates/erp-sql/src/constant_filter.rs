//! Constant domain filters compiled to an SQL predicate with literals (design §5.4): the
//! `WHERE` of a typed exclusion constraint (and later of typed indexes). DDL takes no parameters,
//! so values become SQL literals here; identifiers are quoted.
//!
//! Only literal operands are allowed: no paths, no names (`uid`, `context`, `ref`), no
//! calls, no arithmetic. The text is parsed with `erp-expr`'s bounded parser (untrusted input from
//! `models/*.toml`) and the domain with `erp-domain`'s list-syntax parser.
//!
//! ```
//! # use erp_registry::{FieldDecl, ModelDecl, RegistryBuilder, AddonDescriptor};
//! # static ADDON: AddonDescriptor = AddonDescriptor { name: "doc_cf", depends: &["base"], exact_only: false };
//! # static FIELDS: [FieldDecl; 1] = [FieldDecl::selection("state", &[("open", "Open"), ("cancel", "Cancelled")]).required(true)];
//! # let decl: &'static ModelDecl = Box::leak(Box::new(ModelDecl::new("doc_cf", "doc.booking").fields(&FIELDS)));
//! # let registry = RegistryBuilder::new([&ADDON]).linked(false).with_decls([decl]).build().unwrap();
//! # let model = registry.model("doc.booking").unwrap();
//! use erp_sql::constant_filter::compile;
//!
//! assert_eq!(compile("[('state', '!=', 'cancel')]", model).unwrap(), "\"state\" <> 'cancel'");
//! let err = compile("[('state', '=', uid)]", model).unwrap_err();
//! assert_eq!(err, "only literal values are allowed, found the name 'uid'");
//! ```

use erp_domain::{Domain, DomainKind, Operator, Value};
use erp_expr::{Expr, Literal, UnaryOp};
use erp_fields::FieldType;
use erp_registry::{FieldSpec, ModelSpec};

use crate::param::{Scalar, ScalarKind, falsy_value, to_scalar};
use crate::{literal, quote};

/// Compile the constant filter `text` on `model` to an SQL predicate (unqualified quoted columns,
/// SQL literals). Equal inputs give equal text, so the result can be part of a canonical object
/// definition.
///
/// # Errors
///
/// The reason the filter is refused, without the object and model (the caller adds them): a
/// syntax error, a non-literal operand, a path, an unknown or non-stored field, a field type or an
/// operator a constant filter does not take, a value that does not convert to the field's type.
pub fn compile(text: &str, model: &ModelSpec) -> Result<String, String> {
    let ast = erp_expr::parse(text).map_err(|e| format!("not a domain literal: {e}"))?;
    let value = literal_value(&ast)?;
    let domain = Domain::from_value(&value).map_err(|e| e.to_string())?;
    node(&domain, model)
}

/// [`compile`] as the compiled filter the conflict diagnostic accepts.
///
/// # Errors
///
/// See [`compile`].
pub fn compile_filter(
    text: &str,
    model: &ModelSpec,
) -> Result<crate::conflict::CompiledFilter, String> {
    compile(text, model).map(crate::conflict::CompiledFilter::new)
}

/// The literal value of `expr`: lists, tuples, strings, numbers, booleans and `None`.
fn literal_value(expr: &Expr) -> Result<Value, String> {
    Ok(match expr {
        Expr::Literal(lit) => match lit {
            Literal::None | Literal::Bool(false) => Value::Bool(false),
            Literal::Bool(true) => Value::Bool(true),
            Literal::Int(i) => Value::Int(*i),
            Literal::Float(f) => Value::Float(*f),
            Literal::Str(s) => {
                if s.contains('\0') {
                    return Err("a string literal holds a NUL character".to_owned());
                }
                Value::Str(s.clone())
            }
            Literal::Bytes(_) => {
                return Err("only literal values are allowed, found a bytes literal".to_owned());
            }
            // `Literal` is non-exhaustive: a kind added later is named, never mislabelled
            other => {
                return Err(format!(
                    "only literal values are allowed, found the unsupported literal {other:?}"
                ));
            }
        },
        Expr::Unary {
            op: UnaryOp::USub,
            operand,
        } => match operand.as_ref() {
            Expr::Literal(Literal::Int(i)) => Value::Int(
                i.checked_neg()
                    .ok_or_else(|| format!("the integer -{i} is out of range"))?,
            ),
            Expr::Literal(Literal::Float(f)) => Value::Float(-f),
            _ => return Err("only literal values are allowed, found an expression".to_owned()),
        },
        Expr::List(items) | Expr::Tuple(items) => {
            Value::List(items.iter().map(literal_value).collect::<Result<_, _>>()?)
        }
        Expr::Name(name) => {
            return Err(format!(
                "only literal values are allowed, found the name '{name}'"
            ));
        }
        Expr::Call { .. } => {
            return Err("only literal values are allowed, found a call".to_owned());
        }
        Expr::Attribute { .. } | Expr::Subscript { .. } => {
            return Err("only literal values are allowed, found an attribute access".to_owned());
        }
        _ => return Err("only literal values are allowed, found an expression".to_owned()),
    })
}

fn node(domain: &Domain, model: &ModelSpec) -> Result<String, String> {
    Ok(match domain.kind() {
        DomainKind::True => "TRUE".to_owned(),
        DomainKind::False => "FALSE".to_owned(),
        DomainKind::Not(child) => format!("({}) IS NOT TRUE", node(child, model)?),
        DomainKind::And(children) | DomainKind::Or(children) => {
            let sep = if matches!(domain.kind(), DomainKind::And(_)) {
                " AND "
            } else {
                " OR "
            };
            let parts = children
                .iter()
                .map(|c| node(c, model))
                .collect::<Result<Vec<_>, _>>()?;
            format!("({})", parts.join(sep))
        }
        DomainKind::Condition(condition) => {
            let name = condition.field_expr();
            if name.contains('.') {
                return Err(format!("paths are not allowed ('{name}')"));
            }
            let field = model
                .field(name)
                .ok_or_else(|| format!("field '{name}' does not exist on '{}'", model.name))?;
            if field.company_dependent {
                return Err(format!(
                    "field '{name}' is company-dependent and requires a prepared company context"
                ));
            }
            if !field.has_column() {
                return Err(format!("field '{name}' is not stored"));
            }
            let positive = match condition.operator() {
                Operator::Eq | Operator::In => true,
                Operator::Ne | Operator::NotIn => false,
                other => {
                    return Err(format!(
                        "operator '{other}' is not allowed (use =, !=, in or not in)"
                    ));
                }
            };
            let values: Vec<&Value> = match (condition.operator(), condition.value()) {
                (Operator::Eq | Operator::Ne, value) => vec![value],
                (_, Value::List(items)) => items.iter().collect(),
                (_, Value::Set(set)) => set.iter().collect(),
                (op, other) => {
                    return Err(format!("'{op}' takes a list of values, not {other}"));
                }
            };
            condition_sql(field, positive, &values)?
        }
        DomainKind::Custom(_) => return Err("custom domain nodes are not allowed".to_owned()),
    })
}

fn condition_sql(field: &FieldSpec, positive: bool, values: &[&Value]) -> Result<String, String> {
    let col = quote(field.name);
    let nullable = crate::filter::can_be_null(field);
    if matches!(field.ty, FieldType::Boolean) {
        let has = |b: bool| values.iter().any(|v| v.is_truthy() == b);
        let (with_true, with_false) = if positive {
            (has(true), has(false))
        } else {
            (!has(true), !has(false))
        };
        return Ok(match (with_true, with_false) {
            (true, true) => "TRUE".to_owned(),
            (false, false) => "FALSE".to_owned(),
            (true, false) => format!("{col} IS TRUE"),
            (false, true) => format!("{col} IS NOT TRUE"),
        });
    }
    let kind = scalar_kind(field)?;
    let mut scalars: Vec<Scalar> = Vec::with_capacity(values.len());
    let mut null_in = false;
    for value in values {
        match value {
            Value::Bool(false) => null_in = true,
            Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(true) => {
                let scalar = to_scalar(kind, value).map_err(|_| {
                    format!(
                        "{value} is not a valid value for the {} field '{}'",
                        field.ty.type_name(),
                        field.name
                    )
                })?;
                if let FieldType::Selection {
                    values: options, ..
                } = &field.ty
                {
                    let Some(options) = options else {
                        return Err(format!(
                            "the selection field '{}' has options computed by a method; a constant \
                             filter needs a fixed list to check '{value}' against",
                            field.name
                        ));
                    };
                    let known = match &scalar {
                        Scalar::Text(t) => options.iter().any(|(k, _)| k.as_str() == &**t),
                        // `to_scalar` gives selection values as text; anything else is no option
                        _ => false,
                    };
                    if !known {
                        return Err(format!(
                            "{value} is not an option of the selection field '{}'",
                            field.name
                        ));
                    }
                }
                if matches!(field.ty, FieldType::Many2one { .. })
                    && !matches!(scalar, Scalar::Int8(id) if id > 0)
                {
                    return Err(format!(
                        "{value} is not a record id for the many2one field '{}'",
                        field.name
                    ));
                }
                if !scalars.contains(&scalar) {
                    scalars.push(scalar);
                }
            }
            other => {
                return Err(format!(
                    "{other} is not a literal value for the field '{}'",
                    field.name
                ));
            }
        }
    }
    // a value treated as null (0, 0.0, '') matches NULL rows and the other way round
    if let Some(null_value) = falsy_value(&field.ty) {
        if scalars.contains(&null_value) {
            null_in = true;
        } else if null_in {
            scalars.push(null_value);
        }
    }
    let test = (!scalars.is_empty())
        .then(|| {
            let lits = scalars
                .iter()
                .map(scalar_literal)
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, String>(match (positive, lits.as_slice()) {
                (true, [one]) => format!("{col} = {one}"),
                (false, [one]) => format!("{col} <> {one}"),
                (true, many) => format!("{col} IN ({})", many.join(", ")),
                (false, many) => format!("{col} NOT IN ({})", many.join(", ")),
            })
        })
        .transpose()?;
    let has_values = test.is_some();
    Ok(if positive == null_in {
        // field in {v, False}  => field = v OR field IS NULL
        // field not in {v}     => field <> v OR field IS NULL
        match (nullable, test) {
            (false, Some(t)) => t,
            (false, None) => "FALSE".to_owned(),
            (true, Some(t)) => format!("({t} OR {col} IS NULL)"),
            (true, None) => format!("{col} IS NULL"),
        }
    } else if !positive && null_in && !has_values {
        if nullable {
            format!("{col} IS NOT NULL")
        } else {
            "TRUE".to_owned()
        }
    } else {
        match test {
            Some(t) => t,
            None if positive => "FALSE".to_owned(),
            None => "TRUE".to_owned(),
        }
    })
}

/// The parameter kind values of `field` convert to, for the field types a filter takes.
fn scalar_kind(field: &FieldSpec) -> Result<ScalarKind, String> {
    let allowed = matches!(
        field.ty,
        FieldType::BigInt
            | FieldType::Integer
            | FieldType::Float { .. }
            | FieldType::Monetary { .. }
            | FieldType::Char { .. }
            | FieldType::Text
            | FieldType::Html
            | FieldType::Selection { .. }
            | FieldType::Date
            | FieldType::Datetime
            | FieldType::Uuid
            | FieldType::Xid8
            | FieldType::TimestampTz
            | FieldType::Many2one { .. }
    );
    let kind = allowed.then(|| ScalarKind::of_field(&field.ty)).flatten();
    kind.ok_or_else(|| {
        format!(
            "field '{}' is a {} field; a constant filter takes stored scalar, selection, boolean \
             and many2one fields",
            field.name,
            field.ty.type_name()
        )
    })
}

/// A parameter value as an SQL literal of its type.
fn scalar_literal(scalar: &Scalar) -> Result<String, String> {
    Ok(match scalar {
        Scalar::BigInt(i) => i.to_string(),
        Scalar::Int8(i) => i.to_string(),
        Scalar::Float8(f) if f.is_finite() => format!("'{f:?}'::float8"),
        Scalar::Float8(f) => return Err(format!("{f} is not a finite number")),
        Scalar::Numeric(d) => format!("'{d}'::numeric"),
        Scalar::Text(s) => {
            if s.contains('\0') {
                return Err("a string literal holds a NUL character".to_owned());
            }
            literal(s)
        }
        Scalar::Date(d) => format!("'{d}'::date"),
        Scalar::Timestamp(t) => format!("'{t}'::timestamp"),
        Scalar::Uuid(u) => format!("'{u}'::uuid"),
        Scalar::Xid8(x) => format!("'{x}'::xid8"),
        Scalar::TimestampTz(t) => format!("'{t}'::timestamptz"),
        other => {
            return Err(format!(
                "{other:?} has no literal form in a constant filter"
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use erp_fields::OnDelete;
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, Registry, RegistryBuilder};

    use super::*;

    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "test_constant_filter",
        depends: &["base"],
        exact_only: false,
    };

    static FIELDS: [FieldDecl; 8] = [
        FieldDecl::selection("state", &[("open", "Open"), ("cancel", "Cancelled")]).required(true),
        FieldDecl::selection("kind", &[("a", "A"), ("b", "B")]),
        FieldDecl::char("code"),
        FieldDecl::integer("qty"),
        FieldDecl::boolean("active"),
        FieldDecl::many2one("partner_id", "res.users").ondelete(OnDelete::SetNull),
        FieldDecl::date("day"),
        FieldDecl::json("payload"),
    ];

    fn registry() -> Registry {
        let decl: &'static ModelDecl = Box::leak(Box::new(
            ModelDecl::new("test_constant_filter", "cf.model").fields(&FIELDS),
        ));
        RegistryBuilder::new([&ADDON])
            .linked(false)
            .with_decls([decl])
            .build()
            .unwrap()
    }

    #[test]
    fn literals_compile_with_search_semantics() {
        let registry = registry();
        let m = registry.model("cf.model").unwrap();
        let cases = [
            // required: no NULL branch
            ("[('state', '!=', 'cancel')]", "\"state\" <> 'cancel'"),
            ("[(\"state\", \"=\", \"open\")]", "\"state\" = 'open'"),
            // nullable: a negative operator keeps rows without a value
            (
                "[('kind', '!=', 'a')]",
                "(\"kind\" <> 'a' OR \"kind\" IS NULL)",
            ),
            (
                "[('kind', 'in', ['a', False])]",
                "(\"kind\" = 'a' OR \"kind\" IS NULL)",
            ),
            ("[('kind', '=', False)]", "\"kind\" IS NULL"),
            ("[('kind', '!=', False)]", "\"kind\" IS NOT NULL"),
            (
                "[('kind', 'not in', ('a', 'b'))]",
                "(\"kind\" NOT IN ('a', 'b') OR \"kind\" IS NULL)",
            ),
            // '' and 0 count as not set
            (
                "[('code', '=', False)]",
                "(\"code\" = '' OR \"code\" IS NULL)",
            ),
            ("[('qty', '!=', 0)]", "\"qty\" <> 0"),
            ("[('active', '=', True)]", "\"active\" IS TRUE"),
            ("[('active', '!=', True)]", "\"active\" IS NOT TRUE"),
            ("[('partner_id', '=', 7)]", "\"partner_id\" = 7"),
            (
                "[('day', '=', '2026-01-02')]",
                "\"day\" = '2026-01-02'::date",
            ),
            ("[('code', '=', \"it's\")]", "\"code\" = 'it''s'"),
            (
                "['|', ('kind', '=', 'a'), '!', ('state', '=', 'open')]",
                "(\"kind\" = 'a' OR \"state\" <> 'open')",
            ),
            (
                "[('kind', '=', 'a'), ('qty', '=', -3)]",
                "(\"kind\" = 'a' AND \"qty\" = -3)",
            ),
            ("[]", "TRUE"),
        ];
        for (text, sql) in cases {
            assert_eq!(compile(text, m).unwrap(), sql, "{text}");
        }
    }

    #[test]
    fn non_constant_filters_are_refused_with_the_reason() {
        let registry = registry();
        let m = registry.model("cf.model").unwrap();
        let cases = [
            (
                "[('partner_id.name', '=', 'x')]",
                "paths are not allowed ('partner_id.name')",
            ),
            (
                "[('partner_id', '=', uid)]",
                "only literal values are allowed, found the name 'uid'",
            ),
            (
                "[('state', '=', context.get('x'))]",
                "only literal values are allowed, found a call",
            ),
            (
                "[('state', '=', ref('base.x'))]",
                "only literal values are allowed, found a call",
            ),
            (
                "[('qty', '=', 1 + 1)]",
                "only literal values are allowed, found an expression",
            ),
            (
                "[('qty', '<', 3)]",
                "operator '<' is not allowed (use =, !=, in or not in)",
            ),
            (
                "[('code', 'ilike', 'a')]",
                "operator 'ilike' is not allowed (use =, !=, in or not in)",
            ),
            (
                "[('nope', '=', 1)]",
                "field 'nope' does not exist on 'cf.model'",
            ),
            (
                "[('payload', '=', 'x')]",
                "field 'payload' is a json field; a constant filter takes stored scalar, \
                 selection, boolean and many2one fields",
            ),
            (
                "[('qty', '=', 'many')]",
                "'many' is not a valid value for the integer field 'qty'",
            ),
            (
                "[('state', '=', 'cancelled')]",
                "'cancelled' is not an option of the selection field 'state'",
            ),
            (
                "[('kind', 'in', ['a', 'z'])]",
                "'z' is not an option of the selection field 'kind'",
            ),
            (
                "[('partner_id', '=', -1)]",
                "-1 is not a record id for the many2one field 'partner_id'",
            ),
            (
                "[('code', '=', 'a\\x00')]",
                "a string literal holds a NUL character",
            ),
        ];
        for (text, reason) in cases {
            assert_eq!(compile(text, m).unwrap_err(), reason, "{text}");
        }
        // syntax errors and malformed domains name the parser's reason
        let err = compile("[('state', '=', 'open'", m).unwrap_err();
        assert!(err.starts_with("not a domain literal: "), "{err}");
        let err = compile("[('state', '=')]", m).unwrap_err();
        assert!(!err.is_empty());
        // huge inputs are bounded by the expression parser's limits
        let deep = format!("{}1{}", "[".repeat(10_000), "]".repeat(10_000));
        assert!(
            compile(&deep, m)
                .unwrap_err()
                .starts_with("not a domain literal: ")
        );
    }
}
