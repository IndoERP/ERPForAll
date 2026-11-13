//! Keyset pagination: the predicate "after this row" for an order specification.
//!
//! A page after a cursor is the same search with one more condition: the rows whose order key comes
//! after the cursor's key, compared term by term with each term's direction and NULLS placement
//! (the same rules as [`crate::order::compile_order`], whose expressions the terms reuse), and the
//! `id` tiebreaker last so the key is unique. Unlike `OFFSET`, the database does not read and
//! discard the skipped rows: when every term has the same direction and none can be NULL, the
//! predicate is a row comparison `(a, b, id) > ($1, $2, $3)`, which a btree index on the leading
//! column(s) answers as a range scan.
//!
//! Supported terms: the model's own stored columns (`id`, numbers, text, dates, booleans, static
//! selections by declaration order) and many2one fields ordered by their raw id (`f.id`, or a
//! comodel ordered by `id`). A many2one ordered through its comodel's order, and a field
//! inherited through `_inherits`, are refused with an error, since their key lives on another
//! table.

use erp_fields::{Error, FieldType, Result};
use erp_registry::{FieldId, ModelSpec, Registry};

use crate::filter::can_be_null;
use crate::literal;
use crate::order::{Direction, invalid_order_error, parse_order};
use crate::param::{Param, Scalar, ScalarKind};
use crate::quote;

/// How a term's key is compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyKind {
    /// A column compared with a parameter of this type.
    Column(ScalarKind),
    /// A boolean (nullable ones ordered as `COALESCE(f, FALSE)`), compared with `TRUE`/`FALSE`.
    Bool,
    /// A static selection ordered by declaration order: its keys, in order.
    Selection(Vec<String>),
}

/// One term of a keyset order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyTerm {
    /// The field ordered by.
    pub field: FieldId,
    /// The ordered expression (as in `ORDER BY`).
    pub expr: String,
    /// `DESC`.
    pub descending: bool,
    /// NULLs come first (PostgreSQL's default for the direction unless a NULLS clause is given).
    pub nulls_first: bool,
    /// Whether the expression can be NULL.
    pub nullable: bool,
    /// How the key is compared.
    pub kind: KeyKind,
}

/// A term's value in a cursor.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyValue {
    /// NULL.
    Null,
    /// A boolean term.
    Bool(bool),
    /// A column value (the term's kind).
    Scalar(Scalar),
    /// A selection key (its position is compared; an unknown key compares as NULL, as
    /// `array_position` returns).
    Key(String),
}

fn unsupported(model: &ModelSpec, field: &str, why: &str) -> Error {
    Error::programming(format!(
        "keyset pagination cannot order by {}.{field}: {why}",
        model.name
    ))
}

/// The terms of `spec` (the model's `_order` when empty) for keyset pagination, `id` appended
/// unless present (the tiebreaker of [`crate::order::compile_order`]).
pub fn keyset_terms(registry: &Registry, model: &ModelSpec, spec: &str) -> Result<Vec<KeyTerm>> {
    let spec = if spec.trim().is_empty() {
        model.order
    } else {
        spec
    };
    let terms = if spec.trim().is_empty() {
        Vec::new()
    } else {
        parse_order(spec).ok_or_else(|| invalid_order_error(spec))?
    };
    let table = quote(&model.table);
    let mut out: Vec<KeyTerm> = Vec::with_capacity(terms.len() + 1);
    for term in &terms {
        let field = model.field(term.field).ok_or_else(|| {
            Error::programming(format!(
                "Invalid field '{}' on model '{}'",
                term.field, model.name
            ))
        })?;
        if field.company_dependent {
            return Err(unsupported(
                model,
                field.name,
                "company-dependent keys require prepared company values until a later release; use offset pagination",
            ));
        }
        if field.inherited.is_some() {
            return Err(unsupported(
                model,
                field.name,
                "it is inherited through _inherits, so its key is on the parent's table, which \
                 keyset pagination does not join",
            ));
        }
        let col = format!("{table}.{}", quote(field.name));
        let descending = term.direction == Direction::Desc;
        let nulls_first = term.nulls.first_for(term.direction);
        let nullable = can_be_null(field);
        let (expr, kind, nullable) = match (&field.ty, term.property) {
            (FieldType::Many2one { comodel, .. }, property) => {
                let coorder = registry
                    .model(comodel)
                    .map_or("id", |c| c.order.trim())
                    .to_owned();
                match property {
                    Some("id") => {}
                    Some(p) => {
                        return Err(unsupported(
                            model,
                            field.name,
                            &format!("'{p}' is a property of the comodel"),
                        ));
                    }
                    None if coorder.is_empty() || coorder == "id" => {}
                    None => {
                        return Err(unsupported(
                            model,
                            field.name,
                            &format!(
                                "it is ordered by {comodel}'s order ('{coorder}'), whose key is on \
                                 another table; order by '{}.id' or by a field of the model",
                                field.name
                            ),
                        ));
                    }
                }
                (col, KeyKind::Column(ScalarKind::Int8), nullable)
            }
            (_, Some(p)) => {
                return Err(unsupported(
                    model,
                    field.name,
                    &format!("ordering by the property '{p}' is not supported"),
                ));
            }
            _ if !matches!(field.ty, FieldType::Id) && !field.has_column() => {
                return Err(unsupported(model, field.name, "it is not stored"));
            }
            (FieldType::Boolean, None) if nullable => {
                (format!("COALESCE({col}, FALSE)"), KeyKind::Bool, false)
            }
            (FieldType::Boolean, None) => (col, KeyKind::Bool, false),
            (FieldType::Selection { values, .. }, None) => match values {
                Some(values) => {
                    let keys: Vec<String> = values.iter().map(|(k, _)| k.clone()).collect();
                    let list: Vec<String> = keys.iter().map(|k| literal(k)).collect();
                    (
                        format!(
                            "array_position(ARRAY[{}]::text[], {col}::text)",
                            list.join(", ")
                        ),
                        KeyKind::Selection(keys),
                        true,
                    )
                }
                None => {
                    return Err(unsupported(
                        model,
                        field.name,
                        "its values come from a method",
                    ));
                }
            },
            (ty, None) => match ScalarKind::of_field(ty) {
                Some(kind) => (col, KeyKind::Column(kind), nullable),
                None => {
                    return Err(unsupported(
                        model,
                        field.name,
                        "its values have no order in SQL",
                    ));
                }
            },
        };
        out.push(KeyTerm {
            field: field.id,
            expr,
            descending,
            nulls_first,
            nullable,
            kind,
        });
    }
    let has_id = out.iter().any(|t| {
        model
            .fields
            .get(t.field.index())
            .is_some_and(|f| matches!(f.ty, FieldType::Id))
    });
    if !has_id {
        let id = model
            .field("id")
            .ok_or_else(|| Error::programming(format!("model '{}' has no id", model.name)))?;
        out.push(KeyTerm {
            field: id.id,
            expr: format!("{table}.{}", quote("id")),
            descending: false,
            nulls_first: false,
            nullable: false,
            kind: KeyKind::Column(ScalarKind::Int8),
        });
    }
    Ok(out)
}

/// The predicates "after `values`" for `terms`, in order, with their parameters numbered from
/// `first_param`: the rows of the next page are those of the first piece, then (when it runs
/// short) those of the second. Usually one piece. Two when the leading term can be NULL, its NULLs
/// come last and the cursor's value is not NULL: the rows after are its non-null rows after the
/// key, then all its NULL rows; one predicate would need `OR … IS NULL`, which no index range
/// answers (measured: as slow as `OFFSET`), while each piece is a range or `IS NULL` index scan.
/// `values` has one entry per term (checked by the caller against the cursor).
pub fn keyset_pieces(
    terms: &[KeyTerm],
    values: &[KeyValue],
    first_param: usize,
) -> Result<Vec<(String, Vec<Param>)>> {
    if terms.len() != values.len() || terms.is_empty() {
        return Err(Error::programming(format!(
            "keyset: {} values for {} order terms",
            values.len(),
            terms.len()
        )));
    }
    let mut params: Vec<Param> = Vec::new();
    // per term: the bound operand (`$n::type`, a boolean literal) or None for NULL
    let mut operands: Vec<Option<String>> = Vec::with_capacity(terms.len());
    for (term, value) in terms.iter().zip(values) {
        let operand = match (&term.kind, value) {
            (_, KeyValue::Null) => None,
            (KeyKind::Bool, KeyValue::Bool(b)) => {
                Some(if *b { "TRUE" } else { "FALSE" }.to_owned())
            }
            (KeyKind::Column(kind), KeyValue::Scalar(s)) if s.kind() == *kind => {
                params.push(Param::Scalar(s.clone()));
                Some(format!(
                    "${}::{}",
                    first_param + params.len() - 1,
                    kind.sql()
                ))
            }
            (KeyKind::Selection(keys), KeyValue::Key(k)) => {
                match keys.iter().position(|key| key == k) {
                    Some(i) => {
                        let pos = i64::try_from(i + 1).map_err(|_| {
                            Error::programming("selection with more keys than i64 can count")
                        })?;
                        params.push(Param::Scalar(Scalar::Int8(pos)));
                        Some(format!("${}::int8", first_param + params.len() - 1))
                    }
                    None => None,
                }
            }
            (kind, value) => {
                return Err(Error::validation(format!(
                    "keyset cursor value {value:?} does not fit the order term {} ({kind:?})",
                    term.expr
                )));
            }
        };
        operands.push(operand);
    }
    let first = &terms[0];
    if first.nullable && !first.nulls_first && operands[0].is_some() {
        let mut non_null = terms.to_vec();
        non_null[0].nullable = false;
        return Ok(vec![
            (predicate(&non_null, &operands), params),
            (format!("{} IS NULL", first.expr), Vec::new()),
        ]);
    }
    Ok(vec![(predicate(terms, &operands), params)])
}

/// One predicate over bound operands (see [`keyset_pieces`]).
fn predicate(terms: &[KeyTerm], operands: &[Option<String>]) -> String {
    // A row comparison is exact when every term goes the same way and no row it would drop for a
    // NULL belongs after the key: a term that cannot be NULL, or whose NULLs come first while the
    // cursor's value is not NULL (those rows are before the key).
    let same_direction = terms.iter().all(|t| t.descending == terms[0].descending);
    let row_exact = terms
        .iter()
        .zip(operands)
        .all(|(t, v)| v.is_some() && (!t.nullable || t.nulls_first));
    if same_direction && row_exact {
        let op = if terms[0].descending { "<" } else { ">" };
        let exprs: Vec<&str> = terms.iter().map(|t| t.expr.as_str()).collect();
        let vals: Vec<&str> = operands.iter().flatten().map(String::as_str).collect();
        return if terms.len() == 1 {
            format!("{} {op} {}", exprs[0], vals[0])
        } else {
            format!("({}) {op} ({})", exprs.join(", "), vals.join(", "))
        };
    }
    let mut disjuncts: Vec<String> = Vec::new();
    let mut equal_so_far: Vec<String> = Vec::new();
    for (term, operand) in terms.iter().zip(operands) {
        let after = match operand {
            Some(v) => {
                let op = if term.descending { "<" } else { ">" };
                if term.nullable && !term.nulls_first {
                    Some(format!("({e} {op} {v} OR {e} IS NULL)", e = term.expr))
                } else {
                    Some(format!("{} {op} {v}", term.expr))
                }
            }
            None if term.nulls_first => Some(format!("{} IS NOT NULL", term.expr)),
            None => None,
        };
        if let Some(after) = after {
            let mut parts = equal_so_far.clone();
            parts.push(after);
            disjuncts.push(format!("({})", parts.join(" AND ")));
        }
        equal_so_far.push(match operand {
            Some(v) => format!("{} = {v}", term.expr),
            None => format!("{} IS NULL", term.expr),
        });
    }
    let sql = if disjuncts.is_empty() {
        "FALSE".to_owned()
    } else {
        disjuncts.join(" OR ")
    };
    // An index-friendly bound on the leading term when no row after the key has it NULL.
    match (terms.first(), operands.first()) {
        (Some(first), Some(Some(v)))
            if disjuncts.len() > 1 && (!first.nullable || first.nulls_first) =>
        {
            let op = if first.descending { "<=" } else { ">=" };
            format!("{} {op} {v} AND ({sql})", first.expr)
        }
        _ => sql,
    }
}

#[cfg(test)]
mod tests {
    use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};

    use super::*;

    static T: AddonDescriptor = AddonDescriptor {
        name: "t",
        depends: &["base"],
        exact_only: false,
    };
    static M: ModelDecl = ModelDecl::new("t", "t.m").fields(const { &[FieldDecl::integer("n")] });

    /// A field id from a real registry (ids cannot be forged); the predicate does not use it.
    fn some_field() -> FieldId {
        let registry = RegistryBuilder::new([&T]).with_decls([&M]).build().unwrap();
        registry.model("t.m").unwrap().field("id").unwrap().id
    }

    fn term(expr: &str, descending: bool, nullable: bool, kind: ScalarKind) -> KeyTerm {
        KeyTerm {
            field: some_field(),
            expr: expr.to_owned(),
            descending,
            nulls_first: descending,
            nullable,
            kind: KeyKind::Column(kind),
        }
    }

    fn one(terms: &[KeyTerm], values: &[KeyValue]) -> (String, usize) {
        let pieces = keyset_pieces(terms, values, 1).unwrap();
        assert_eq!(pieces.len(), 1, "{pieces:?}");
        (pieces[0].0.clone(), pieces[0].1.len())
    }

    #[test]
    fn same_direction_not_null_is_a_row_comparison() {
        let terms = [
            term("\"t\".\"seq\"", false, false, ScalarKind::Int8),
            term("\"t\".\"id\"", false, false, ScalarKind::Int8),
        ];
        let pieces = keyset_pieces(
            &terms,
            &[
                KeyValue::Scalar(Scalar::Int8(5)),
                KeyValue::Scalar(Scalar::Int8(42)),
            ],
            3,
        )
        .unwrap();
        assert_eq!(
            pieces,
            vec![(
                "(\"t\".\"seq\", \"t\".\"id\") > ($3::int8, $4::int8)".to_owned(),
                vec![
                    Param::Scalar(Scalar::Int8(5)),
                    Param::Scalar(Scalar::Int8(42))
                ]
            )]
        );
        // NULLs first (descending default) at a non-null value: still one row comparison
        let terms = [
            term("\"t\".\"d\"", true, true, ScalarKind::Date),
            term("\"t\".\"id\"", true, false, ScalarKind::Int8),
        ];
        let (sql, _) = one(
            &terms,
            &[
                KeyValue::Scalar(Scalar::Date(erp_fields::Date::from_days(19_000))),
                KeyValue::Scalar(Scalar::Int8(1)),
            ],
        );
        assert_eq!(sql, "(\"t\".\"d\", \"t\".\"id\") < ($1::date, $2::int8)");
    }

    #[test]
    fn nullable_leading_term_with_nulls_last_splits_in_two() {
        let terms = [
            term("\"t\".\"name\"", false, true, ScalarKind::Text),
            term("\"t\".\"id\"", false, false, ScalarKind::Int8),
        ];
        let pieces = keyset_pieces(
            &terms,
            &[
                KeyValue::Scalar(Scalar::Text("b".into())),
                KeyValue::Scalar(Scalar::Int8(7)),
            ],
            1,
        )
        .unwrap();
        assert_eq!(pieces.len(), 2);
        assert_eq!(
            pieces[0].0,
            "(\"t\".\"name\", \"t\".\"id\") > ($1::text, $2::int8)"
        );
        assert_eq!(pieces[1], ("\"t\".\"name\" IS NULL".to_owned(), Vec::new()));
        // after a NULL name: only the NULL group, by id
        let (sql, n) = one(&terms, &[KeyValue::Null, KeyValue::Scalar(Scalar::Int8(7))]);
        assert_eq!(sql, "(\"t\".\"name\" IS NULL AND \"t\".\"id\" > $1::int8)");
        assert_eq!(n, 1);
    }

    #[test]
    fn mixed_directions_expand_with_a_leading_bound() {
        let terms = [
            term("\"t\".\"n\"", true, false, ScalarKind::Int8),
            term("\"t\".\"name\"", false, true, ScalarKind::Text),
            term("\"t\".\"id\"", false, false, ScalarKind::Int8),
        ];
        let (sql, n) = one(
            &terms,
            &[
                KeyValue::Scalar(Scalar::Int8(3)),
                KeyValue::Scalar(Scalar::Text("b".into())),
                KeyValue::Scalar(Scalar::Int8(9)),
            ],
        );
        assert_eq!(
            sql,
            "\"t\".\"n\" <= $1::int8 AND ((\"t\".\"n\" < $1::int8) OR (\"t\".\"n\" = $1::int8 AND \
             (\"t\".\"name\" > $2::text OR \"t\".\"name\" IS NULL)) OR (\"t\".\"n\" = $1::int8 AND \
             \"t\".\"name\" = $2::text AND \"t\".\"id\" > $3::int8))"
        );
        assert_eq!(n, 3);
    }

    #[test]
    fn mismatched_values_are_refused() {
        let terms = [term("\"t\".\"id\"", false, false, ScalarKind::Int8)];
        let err =
            keyset_pieces(&terms, &[KeyValue::Scalar(Scalar::Text("x".into()))], 1).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
        assert!(keyset_pieces(&terms, &[], 1).is_err());
    }
}
