//! Generated column expressions, validation, and metadata.
//!
//! Provides the AST, lowering, type validation, and SQL rendering for database-generated
//! scalar fields (VIRTUAL and STORED).

use erp_expr::{BinaryOp, BoolOp, CompareOp, EvalLimits, Expr, Literal, UnaryOp};
use erp_fields::{Error, FieldType, Result};
use rustc_hash::FxHashSet;

use crate::spec::{FieldId, ModelSpec};

/// Hard ceilings for generated expressions.
pub const MAX_GENERATED_EXPR_BYTES: usize = 16 * 1024;
pub const MAX_GENERATED_EXPR_DEPTH: usize = 32;
pub const MAX_GENERATED_EXPR_NODES: usize = 1024;
pub const MAX_GENERATED_SQL_BYTES: usize = 64 * 1024;

pub use erp_fields::{ColumnGenerated, GeneratedStorage};

/// A validated scalar literal in a generated expression.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeneratedLiteral {
    Int(i64),
    Bool(bool),
    Str(String),
    Null,
}

/// Closed set of allowed scalar functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GeneratedFunc {
    Abs,
    Lower,
    Upper,
    Least,
    Greatest,
    Coalesce,
}

impl GeneratedFunc {
    /// The name as rendered in generated SQL.
    ///
    /// `abs`, `lower` and `upper` are real `pg_proc` functions and are schema-qualified with
    /// `pg_catalog.` so a public-schema overload cannot be selected. `COALESCE`, `LEAST` and
    /// `GREATEST` are SQL grammar forms with no `pg_proc` entry (`pg_catalog.coalesce(...)` is a
    /// PostgreSQL error), so they render unqualified; a user function cannot shadow a grammar form.
    pub const fn pg_name(self) -> &'static str {
        match self {
            Self::Abs => "pg_catalog.abs",
            Self::Lower => "pg_catalog.lower",
            Self::Upper => "pg_catalog.upper",
            Self::Least => "LEAST",
            Self::Greatest => "GREATEST",
            Self::Coalesce => "COALESCE",
        }
    }
}

/// Allowed unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GeneratedUnaryOp {
    UAdd,
    USub,
    Not,
}

/// Allowed binary arithmetic operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GeneratedBinaryOp {
    Add,
    Sub,
    Mult,
}

/// Allowed boolean logical operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GeneratedBoolOp {
    And,
    Or,
}

/// Allowed comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum GeneratedCompareOp {
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,
}

/// A validated scalar expression AST node.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeneratedExpr {
    Field(FieldId),
    Literal(GeneratedLiteral),
    Unary {
        op: GeneratedUnaryOp,
        expr: Box<GeneratedExpr>,
    },
    Binary {
        op: GeneratedBinaryOp,
        left: Box<GeneratedExpr>,
        right: Box<GeneratedExpr>,
    },
    BoolOp {
        op: GeneratedBoolOp,
        values: Vec<GeneratedExpr>,
    },
    Compare {
        left: Box<GeneratedExpr>,
        op: GeneratedCompareOp,
        right: Box<GeneratedExpr>,
    },
    IfExp {
        test: Box<GeneratedExpr>,
        body: Box<GeneratedExpr>,
        orelse: Box<GeneratedExpr>,
    },
    Call {
        func: GeneratedFunc,
        args: Vec<GeneratedExpr>,
    },
}

/// Inferred scalar type of an expression node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScalarType {
    Boolean,
    Integer,
    Float,
    Monetary,
    Text,
    Null,
}

/// Intermediate unvalidated AST node with field names.
#[derive(Debug, Clone)]
enum RawAst {
    Name(String),
    Literal(GeneratedLiteral),
    Unary {
        op: GeneratedUnaryOp,
        expr: Box<RawAst>,
    },
    Binary {
        op: GeneratedBinaryOp,
        left: Box<RawAst>,
        right: Box<RawAst>,
    },
    BoolOp {
        op: GeneratedBoolOp,
        values: Vec<RawAst>,
    },
    Compare {
        left: Box<RawAst>,
        op: GeneratedCompareOp,
        right: Box<RawAst>,
    },
    IfExp {
        test: Box<RawAst>,
        body: Box<RawAst>,
        orelse: Box<RawAst>,
    },
    Call {
        func: GeneratedFunc,
        args: Vec<RawAst>,
    },
}

impl RawAst {
    fn count_nodes(&self) -> usize {
        match self {
            Self::Name(_) | Self::Literal(_) => 1,
            Self::Unary { expr, .. } => 1 + expr.count_nodes(),
            Self::Binary { left, right, .. } => 1 + left.count_nodes() + right.count_nodes(),
            Self::BoolOp { values, .. } => 1 + values.iter().map(Self::count_nodes).sum::<usize>(),
            Self::Compare { left, right, .. } => 1 + left.count_nodes() + right.count_nodes(),
            Self::IfExp { test, body, orelse } => {
                1 + test.count_nodes() + body.count_nodes() + orelse.count_nodes()
            }
            Self::Call { args, .. } => 1 + args.iter().map(Self::count_nodes).sum::<usize>(),
        }
    }

    fn max_depth(&self) -> usize {
        match self {
            Self::Name(_) | Self::Literal(_) => 1,
            Self::Unary { expr, .. } => 1 + expr.max_depth(),
            Self::Binary { left, right, .. } => 1 + left.max_depth().max(right.max_depth()),
            Self::BoolOp { values, .. } => {
                1 + values.iter().map(Self::max_depth).max().unwrap_or(0)
            }
            Self::Compare { left, right, .. } => 1 + left.max_depth().max(right.max_depth()),
            Self::IfExp { test, body, orelse } => {
                1 + test
                    .max_depth()
                    .max(body.max_depth())
                    .max(orelse.max_depth())
            }
            Self::Call { args, .. } => 1 + args.iter().map(Self::max_depth).max().unwrap_or(0),
        }
    }
}

/// Parse raw expression string and lower to `RawAst`, verifying grammar and bounds.
fn parse_and_lower_raw(model: &str, field: &str, source: &str) -> Result<RawAst> {
    if source.len() > MAX_GENERATED_EXPR_BYTES {
        return Err(Error::programming(format!(
            "Invalid generated field '{model}.{field}': expression exceeds {MAX_GENERATED_EXPR_BYTES} bytes source limit"
        )));
    }
    if source.trim().is_empty() || source.contains('\0') {
        return Err(Error::programming(format!(
            "Invalid generated field '{model}.{field}': unsupported expression"
        )));
    }

    let limits = EvalLimits::default()
        .with_max_source_bytes(MAX_GENERATED_EXPR_BYTES)
        .with_max_depth(MAX_GENERATED_EXPR_DEPTH);

    let parsed = erp_expr::parse_with_limits(source, limits).map_err(|e| match e {
        erp_expr::ExprError::LimitExceeded { message } => {
            let limit_name = if message.contains("depth") {
                "32 depth"
            } else if message.contains("source") {
                "16384 bytes source"
            } else {
                "limit"
            };
            Error::programming(format!(
                "Invalid generated field '{model}.{field}': expression exceeds {limit_name} limit"
            ))
        }
        _ => Error::programming(format!(
            "Invalid generated field '{model}.{field}': unsupported expression"
        )),
    })?;

    let raw = lower_expr_to_raw(model, field, parsed, 1)?;

    let depth = raw.max_depth();
    if depth > MAX_GENERATED_EXPR_DEPTH {
        return Err(Error::programming(format!(
            "Invalid generated field '{model}.{field}': expression exceeds {MAX_GENERATED_EXPR_DEPTH} depth limit"
        )));
    }

    let nodes = raw.count_nodes();
    if nodes > MAX_GENERATED_EXPR_NODES {
        return Err(Error::programming(format!(
            "Invalid generated field '{model}.{field}': expression exceeds {MAX_GENERATED_EXPR_NODES} nodes limit"
        )));
    }

    Ok(raw)
}

fn lower_expr_to_raw(model: &str, field: &str, expr: Expr, depth: usize) -> Result<RawAst> {
    if depth > MAX_GENERATED_EXPR_DEPTH {
        return Err(Error::programming(format!(
            "Invalid generated field '{model}.{field}': expression exceeds {MAX_GENERATED_EXPR_DEPTH} depth limit"
        )));
    }

    let unsupp = || {
        Error::programming(format!(
            "Invalid generated field '{model}.{field}': unsupported expression"
        ))
    };

    match expr {
        Expr::Name(name) => {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(unsupp());
            }
            Ok(RawAst::Name(name))
        }
        Expr::Literal(lit) => match lit {
            Literal::Int(i) => Ok(RawAst::Literal(GeneratedLiteral::Int(i))),
            Literal::Bool(b) => Ok(RawAst::Literal(GeneratedLiteral::Bool(b))),
            Literal::Str(s) => Ok(RawAst::Literal(GeneratedLiteral::Str(s))),
            Literal::None => Ok(RawAst::Literal(GeneratedLiteral::Null)),
            _ => Err(unsupp()),
        },
        Expr::Unary { op, operand } => {
            let gen_op = match op {
                UnaryOp::UAdd => GeneratedUnaryOp::UAdd,
                UnaryOp::USub => GeneratedUnaryOp::USub,
                UnaryOp::Not => GeneratedUnaryOp::Not,
                _ => return Err(unsupp()),
            };
            let child = lower_expr_to_raw(model, field, *operand, depth + 1)?;
            Ok(RawAst::Unary {
                op: gen_op,
                expr: Box::new(child),
            })
        }
        Expr::Binary { op, left, right } => {
            let gen_op = match op {
                BinaryOp::Add => GeneratedBinaryOp::Add,
                BinaryOp::Sub => GeneratedBinaryOp::Sub,
                BinaryOp::Mult => GeneratedBinaryOp::Mult,
                _ => return Err(unsupp()),
            };
            let l = lower_expr_to_raw(model, field, *left, depth + 1)?;
            let r = lower_expr_to_raw(model, field, *right, depth + 1)?;
            Ok(RawAst::Binary {
                op: gen_op,
                left: Box::new(l),
                right: Box::new(r),
            })
        }
        Expr::BoolOp { op, values } => {
            let gen_op = match op {
                BoolOp::And => GeneratedBoolOp::And,
                BoolOp::Or => GeneratedBoolOp::Or,
                _ => return Err(unsupp()),
            };
            let mut list = Vec::with_capacity(values.len());
            for v in values {
                list.push(lower_expr_to_raw(model, field, v, depth + 1)?);
            }
            Ok(RawAst::BoolOp {
                op: gen_op,
                values: list,
            })
        }
        Expr::Compare {
            left,
            ops,
            comparators,
        } => {
            if ops.len() != 1 || comparators.len() != 1 {
                return Err(unsupp());
            }
            let op = match ops[0] {
                CompareOp::Eq => GeneratedCompareOp::Eq,
                CompareOp::NotEq => GeneratedCompareOp::NotEq,
                CompareOp::Lt => GeneratedCompareOp::Lt,
                CompareOp::LtE => GeneratedCompareOp::LtE,
                CompareOp::Gt => GeneratedCompareOp::Gt,
                CompareOp::GtE => GeneratedCompareOp::GtE,
                _ => return Err(unsupp()),
            };
            let l = lower_expr_to_raw(model, field, *left, depth + 1)?;
            let Some(right_expr) = comparators.into_iter().next() else {
                return Err(unsupp());
            };
            let r = lower_expr_to_raw(model, field, right_expr, depth + 1)?;
            Ok(RawAst::Compare {
                left: Box::new(l),
                op,
                right: Box::new(r),
            })
        }
        Expr::IfExp { test, body, orelse } => {
            let t = lower_expr_to_raw(model, field, *test, depth + 1)?;
            let b = lower_expr_to_raw(model, field, *body, depth + 1)?;
            let o = lower_expr_to_raw(model, field, *orelse, depth + 1)?;
            Ok(RawAst::IfExp {
                test: Box::new(t),
                body: Box::new(b),
                orelse: Box::new(o),
            })
        }
        Expr::Call {
            func,
            args,
            keywords,
        } => {
            if !keywords.is_empty() {
                return Err(unsupp());
            }
            let Expr::Name(fn_name) = *func else {
                return Err(unsupp());
            };
            let (func_kind, min_args, max_args) = match fn_name.to_ascii_lowercase().as_str() {
                "abs" => (GeneratedFunc::Abs, 1, 1),
                "lower" => (GeneratedFunc::Lower, 1, 1),
                "upper" => (GeneratedFunc::Upper, 1, 1),
                "least" => (GeneratedFunc::Least, 2, 16),
                "greatest" => (GeneratedFunc::Greatest, 2, 16),
                "coalesce" => (GeneratedFunc::Coalesce, 2, 16),
                _ => return Err(unsupp()),
            };
            if args.len() < min_args || args.len() > max_args {
                return Err(unsupp());
            }
            let mut list = Vec::with_capacity(args.len());
            for a in args {
                list.push(lower_expr_to_raw(model, field, a, depth + 1)?);
            }
            Ok(RawAst::Call {
                func: func_kind,
                args: list,
            })
        }
        _ => Err(unsupp()),
    }
}

/// Convert scalar field type to `ScalarType`.
fn scalar_type_of_field(ty: &FieldType) -> Option<ScalarType> {
    match ty {
        FieldType::Boolean => Some(ScalarType::Boolean),
        FieldType::Integer => Some(ScalarType::Integer),
        FieldType::Float { .. } => Some(ScalarType::Float),
        FieldType::Monetary { .. } => Some(ScalarType::Monetary),
        FieldType::Char { .. } | FieldType::Text => Some(ScalarType::Text),
        _ => None,
    }
}

/// Resolve field references, check dependency types and ordinary local column rules.
fn resolve_and_typecheck(
    model_name: &str,
    field_name: &str,
    raw: RawAst,
    model: &ModelSpec,
    deps: &mut FxHashSet<FieldId>,
) -> Result<(GeneratedExpr, ScalarType)> {
    let unsupp = || {
        Error::programming(format!(
            "Invalid generated field '{model_name}.{field_name}': unsupported expression"
        ))
    };

    match raw {
        RawAst::Name(name) => {
            let Some(f) = model.field(&name) else {
                return Err(Error::programming(format!(
                    "Invalid generated field '{model_name}.{field_name}': unknown source '{name}'"
                )));
            };

            let not_ordinary = || {
                Error::programming(format!(
                    "Invalid generated field '{model_name}.{field_name}': source '{name}' is not an ordinary local column"
                ))
            };

            // Self reference
            if name == field_name {
                return Err(not_ordinary());
            }
            // Another generated field
            if f.is_generated() {
                return Err(not_ordinary());
            }
            // Computed or related
            if f.is_computed() {
                return Err(not_ordinary());
            }
            // Delegated from an _inherits parent
            if f.inherited.is_some() {
                return Err(not_ordinary());
            }
            // Must be an allowed scalar column
            let Some(scalar_ty) = scalar_type_of_field(&f.ty) else {
                return Err(not_ordinary());
            };

            deps.insert(f.id);
            Ok((GeneratedExpr::Field(f.id), scalar_ty))
        }
        RawAst::Literal(lit) => {
            let ty = match &lit {
                GeneratedLiteral::Int(_) => ScalarType::Integer,
                GeneratedLiteral::Bool(_) => ScalarType::Boolean,
                GeneratedLiteral::Str(_) => ScalarType::Text,
                GeneratedLiteral::Null => ScalarType::Null,
            };
            Ok((GeneratedExpr::Literal(lit), ty))
        }
        RawAst::Unary { op, expr } => {
            let (child_expr, child_ty) =
                resolve_and_typecheck(model_name, field_name, *expr, model, deps)?;
            match op {
                GeneratedUnaryOp::Not => {
                    if !matches!(child_ty, ScalarType::Boolean | ScalarType::Null) {
                        return Err(unsupp());
                    }
                    Ok((
                        GeneratedExpr::Unary {
                            op,
                            expr: Box::new(child_expr),
                        },
                        ScalarType::Boolean,
                    ))
                }
                GeneratedUnaryOp::UAdd | GeneratedUnaryOp::USub => {
                    if !matches!(
                        child_ty,
                        ScalarType::Integer
                            | ScalarType::Float
                            | ScalarType::Monetary
                            | ScalarType::Null
                    ) {
                        return Err(unsupp());
                    }
                    Ok((
                        GeneratedExpr::Unary {
                            op,
                            expr: Box::new(child_expr),
                        },
                        child_ty,
                    ))
                }
            }
        }
        RawAst::Binary { op, left, right } => {
            let (l_expr, l_ty) = resolve_and_typecheck(model_name, field_name, *left, model, deps)?;
            let (r_expr, r_ty) =
                resolve_and_typecheck(model_name, field_name, *right, model, deps)?;

            let res_ty = match (l_ty, r_ty) {
                (ScalarType::Integer, ScalarType::Integer) => ScalarType::Integer,
                (ScalarType::Integer, ScalarType::Float)
                | (ScalarType::Float, ScalarType::Integer)
                | (ScalarType::Float, ScalarType::Float) => ScalarType::Float,
                (ScalarType::Integer, ScalarType::Monetary)
                | (ScalarType::Monetary, ScalarType::Integer)
                | (ScalarType::Monetary, ScalarType::Monetary) => ScalarType::Monetary,
                (ScalarType::Null, t) | (t, ScalarType::Null)
                    if matches!(
                        t,
                        ScalarType::Integer | ScalarType::Float | ScalarType::Monetary
                    ) =>
                {
                    t
                }
                (ScalarType::Null, ScalarType::Null) => ScalarType::Null,
                _ => return Err(unsupp()),
            };

            Ok((
                GeneratedExpr::Binary {
                    op,
                    left: Box::new(l_expr),
                    right: Box::new(r_expr),
                },
                res_ty,
            ))
        }
        RawAst::BoolOp { op, values } => {
            let mut list = Vec::with_capacity(values.len());
            for v in values {
                let (expr, ty) = resolve_and_typecheck(model_name, field_name, v, model, deps)?;
                if !matches!(ty, ScalarType::Boolean | ScalarType::Null) {
                    return Err(unsupp());
                }
                list.push(expr);
            }
            Ok((
                GeneratedExpr::BoolOp { op, values: list },
                ScalarType::Boolean,
            ))
        }
        RawAst::Compare { left, op, right } => {
            let (l_expr, l_ty) = resolve_and_typecheck(model_name, field_name, *left, model, deps)?;
            let (r_expr, r_ty) =
                resolve_and_typecheck(model_name, field_name, *right, model, deps)?;

            let ok = match (l_ty, r_ty) {
                (ScalarType::Boolean, ScalarType::Boolean) => true,
                (ScalarType::Text, ScalarType::Text) => true,
                (
                    ScalarType::Integer | ScalarType::Float | ScalarType::Monetary,
                    ScalarType::Integer | ScalarType::Float | ScalarType::Monetary,
                ) => {
                    // Do not allow mixing Float and Monetary
                    !(l_ty == ScalarType::Float && r_ty == ScalarType::Monetary
                        || l_ty == ScalarType::Monetary && r_ty == ScalarType::Float)
                }
                (ScalarType::Null, _) | (_, ScalarType::Null) => true,
                _ => false,
            };
            if !ok {
                return Err(unsupp());
            }

            Ok((
                GeneratedExpr::Compare {
                    left: Box::new(l_expr),
                    op,
                    right: Box::new(r_expr),
                },
                ScalarType::Boolean,
            ))
        }
        RawAst::IfExp { test, body, orelse } => {
            let (t_expr, t_ty) = resolve_and_typecheck(model_name, field_name, *test, model, deps)?;
            if !matches!(t_ty, ScalarType::Boolean | ScalarType::Null) {
                return Err(unsupp());
            }

            let (b_expr, b_ty) = resolve_and_typecheck(model_name, field_name, *body, model, deps)?;
            let (o_expr, o_ty) =
                resolve_and_typecheck(model_name, field_name, *orelse, model, deps)?;

            let res_ty = unify_scalar_types(b_ty, o_ty).ok_or_else(unsupp)?;

            Ok((
                GeneratedExpr::IfExp {
                    test: Box::new(t_expr),
                    body: Box::new(b_expr),
                    orelse: Box::new(o_expr),
                },
                res_ty,
            ))
        }
        RawAst::Call { func, args } => match func {
            GeneratedFunc::Abs => {
                let Some(first_arg) = args.into_iter().next() else {
                    return Err(unsupp());
                };
                let (arg_expr, arg_ty) =
                    resolve_and_typecheck(model_name, field_name, first_arg, model, deps)?;
                if !matches!(
                    arg_ty,
                    ScalarType::Integer
                        | ScalarType::Float
                        | ScalarType::Monetary
                        | ScalarType::Null
                ) {
                    return Err(unsupp());
                }
                Ok((
                    GeneratedExpr::Call {
                        func,
                        args: vec![arg_expr],
                    },
                    arg_ty,
                ))
            }
            GeneratedFunc::Lower | GeneratedFunc::Upper => {
                let Some(first_arg) = args.into_iter().next() else {
                    return Err(unsupp());
                };
                let (arg_expr, arg_ty) =
                    resolve_and_typecheck(model_name, field_name, first_arg, model, deps)?;
                if !matches!(arg_ty, ScalarType::Text | ScalarType::Null) {
                    return Err(unsupp());
                }
                Ok((
                    GeneratedExpr::Call {
                        func,
                        args: vec![arg_expr],
                    },
                    ScalarType::Text,
                ))
            }
            GeneratedFunc::Least | GeneratedFunc::Greatest | GeneratedFunc::Coalesce => {
                let mut list = Vec::with_capacity(args.len());
                let mut common_ty = ScalarType::Null;
                for a in args {
                    let (expr, ty) = resolve_and_typecheck(model_name, field_name, a, model, deps)?;
                    common_ty = unify_scalar_types(common_ty, ty).ok_or_else(unsupp)?;
                    list.push(expr);
                }
                Ok((GeneratedExpr::Call { func, args: list }, common_ty))
            }
        },
    }
}

fn unify_scalar_types(a: ScalarType, b: ScalarType) -> Option<ScalarType> {
    match (a, b) {
        (ScalarType::Null, other) | (other, ScalarType::Null) => Some(other),
        (x, y) if x == y => Some(x),
        (ScalarType::Integer, ScalarType::Float) | (ScalarType::Float, ScalarType::Integer) => {
            Some(ScalarType::Float)
        }
        (ScalarType::Integer, ScalarType::Monetary)
        | (ScalarType::Monetary, ScalarType::Integer) => Some(ScalarType::Monetary),
        _ => None,
    }
}

/// Check that the inferred expression type is compatible with target field type.
fn validate_target_type(
    model: &str,
    field: &str,
    target_type: &FieldType,
    inferred: ScalarType,
) -> Result<()> {
    let unsupp = || {
        Error::programming(format!(
            "Invalid generated field '{model}.{field}': unsupported expression"
        ))
    };

    match target_type {
        FieldType::Boolean => {
            if !matches!(inferred, ScalarType::Boolean | ScalarType::Null) {
                return Err(unsupp());
            }
        }
        FieldType::Integer => {
            if !matches!(inferred, ScalarType::Integer | ScalarType::Null) {
                return Err(unsupp());
            }
        }
        FieldType::Float { .. } => {
            if !matches!(
                inferred,
                ScalarType::Float | ScalarType::Integer | ScalarType::Null
            ) {
                return Err(unsupp());
            }
        }
        FieldType::Monetary { .. } => {
            if !matches!(
                inferred,
                ScalarType::Monetary | ScalarType::Integer | ScalarType::Null
            ) {
                return Err(unsupp());
            }
        }
        FieldType::Char { .. } | FieldType::Text => {
            if !matches!(inferred, ScalarType::Text | ScalarType::Null) {
                return Err(unsupp());
            }
        }
        _ => return Err(unsupp()),
    }
    Ok(())
}

/// Render AST to PostgreSQL SQL.
fn render_sql_rec<F>(expr: &GeneratedExpr, col_name_fn: &F) -> Result<String>
where
    F: Fn(FieldId) -> Result<&'static str>,
{
    match expr {
        GeneratedExpr::Field(id) => {
            let col = col_name_fn(*id)?;
            Ok(format!("\"{}\"", col.replace('"', "\"\"")))
        }
        GeneratedExpr::Literal(lit) => match lit {
            GeneratedLiteral::Int(i) => Ok(format!("{i}")),
            GeneratedLiteral::Bool(true) => Ok("TRUE".to_string()),
            GeneratedLiteral::Bool(false) => Ok("FALSE".to_string()),
            GeneratedLiteral::Str(s) => {
                let escaped = s.replace('\'', "''");
                if s.contains('\\') {
                    Ok(format!("E'{}'", escaped.replace('\\', "\\\\")))
                } else {
                    Ok(format!("'{escaped}'"))
                }
            }
            GeneratedLiteral::Null => Ok("NULL".to_string()),
        },
        GeneratedExpr::Unary { op, expr } => {
            let inner = render_sql_rec(expr, col_name_fn)?;
            match op {
                GeneratedUnaryOp::UAdd => Ok(format!("(OPERATOR(pg_catalog.+) {inner})")),
                GeneratedUnaryOp::USub => Ok(format!("(OPERATOR(pg_catalog.-) {inner})")),
                GeneratedUnaryOp::Not => Ok(format!("(NOT {inner})")),
            }
        }
        GeneratedExpr::Binary { op, left, right } => {
            let l = render_sql_rec(left, col_name_fn)?;
            let r = render_sql_rec(right, col_name_fn)?;
            let op_str = match op {
                GeneratedBinaryOp::Add => "OPERATOR(pg_catalog.+)",
                GeneratedBinaryOp::Sub => "OPERATOR(pg_catalog.-)",
                GeneratedBinaryOp::Mult => "OPERATOR(pg_catalog.*)",
            };
            Ok(format!("({l} {op_str} {r})"))
        }
        GeneratedExpr::BoolOp { op, values } => {
            let op_str = match op {
                GeneratedBoolOp::And => " AND ",
                GeneratedBoolOp::Or => " OR ",
            };
            let mut parts = Vec::with_capacity(values.len());
            for v in values {
                parts.push(render_sql_rec(v, col_name_fn)?);
            }
            Ok(format!("({})", parts.join(op_str)))
        }
        GeneratedExpr::Compare { left, op, right } => {
            let l = render_sql_rec(left, col_name_fn)?;
            let r = render_sql_rec(right, col_name_fn)?;
            let op_str = match op {
                GeneratedCompareOp::Eq => "OPERATOR(pg_catalog.=)",
                GeneratedCompareOp::NotEq => "OPERATOR(pg_catalog.<>)",
                GeneratedCompareOp::Lt => "OPERATOR(pg_catalog.<)",
                GeneratedCompareOp::LtE => "OPERATOR(pg_catalog.<=)",
                GeneratedCompareOp::Gt => "OPERATOR(pg_catalog.>)",
                GeneratedCompareOp::GtE => "OPERATOR(pg_catalog.>=)",
            };
            Ok(format!("({l} {op_str} {r})"))
        }
        GeneratedExpr::IfExp { test, body, orelse } => {
            let t = render_sql_rec(test, col_name_fn)?;
            let b = render_sql_rec(body, col_name_fn)?;
            let o = render_sql_rec(orelse, col_name_fn)?;
            Ok(format!("(CASE WHEN {t} THEN {b} ELSE {o} END)"))
        }
        GeneratedExpr::Call { func, args } => {
            let mut parts = Vec::with_capacity(args.len());
            for a in args {
                parts.push(render_sql_rec(a, col_name_fn)?);
            }
            Ok(format!("{}({})", func.pg_name(), parts.join(", ")))
        }
    }
}

/// Generation metadata stored on `FieldSpec`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct GeneratedSpec {
    pub(crate) storage: GeneratedStorage,
    pub(crate) source: &'static str,
    pub(crate) dependencies: Vec<FieldId>,
    pub(crate) tree: GeneratedExpr,
    pub(crate) sql: String,
}

impl GeneratedSpec {
    /// Create a new generated column specification. Crate-private: a spec is only valid when
    /// produced by [`compile_generated_spec`] (no raw-SQL escape hatch).
    pub(crate) fn new(
        storage: GeneratedStorage,
        source: &'static str,
        dependencies: Vec<FieldId>,
        tree: GeneratedExpr,
        sql: String,
    ) -> Self {
        Self {
            storage,
            source,
            dependencies,
            tree,
            sql,
        }
    }

    /// Create an uncompiled placeholder; `finish` replaces it with the compiled spec.
    pub(crate) fn raw(source: &'static str, storage: GeneratedStorage) -> Self {
        Self {
            storage,
            source,
            dependencies: Vec::new(),
            tree: GeneratedExpr::Literal(GeneratedLiteral::Null),
            sql: String::new(),
        }
    }

    /// Physical materialization kind.
    pub fn storage(&self) -> GeneratedStorage {
        self.storage
    }

    /// True if virtual.
    pub fn is_virtual(&self) -> bool {
        self.storage.is_virtual()
    }

    /// True if stored.
    pub fn is_stored(&self) -> bool {
        self.storage.is_stored()
    }

    /// The original expression source text.
    pub fn source(&self) -> &'static str {
        self.source
    }

    /// Sorted, unique `FieldId`s of source fields referenced by this expression.
    pub fn dependencies(&self) -> &[FieldId] {
        &self.dependencies
    }

    /// The validated scalar AST tree.
    pub fn tree(&self) -> &GeneratedExpr {
        &self.tree
    }

    /// The emitted PostgreSQL SQL expression.
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

/// Validate expression, resolve references, check types, render SQL and build `GeneratedSpec`.
pub(crate) fn compile_generated_spec(
    model: &ModelSpec,
    field_name: &'static str,
    target_type: &FieldType,
    source: &'static str,
    storage: GeneratedStorage,
) -> Result<GeneratedSpec> {
    let model_name = &model.name;
    let raw = parse_and_lower_raw(model_name, field_name, source)?;

    let mut deps_set = FxHashSet::default();
    let (tree, inferred_ty) =
        resolve_and_typecheck(model_name, field_name, raw, model, &mut deps_set)?;

    validate_target_type(model_name, field_name, target_type, inferred_ty)?;

    let mut deps: Vec<FieldId> = deps_set.into_iter().collect();
    deps.sort();

    let col_fn = |id: FieldId| -> Result<&'static str> {
        let f = model.field_by_id(id);
        Ok(f.name)
    };

    let sql = render_sql_rec(&tree, &col_fn)?;
    if sql.len() > MAX_GENERATED_SQL_BYTES {
        return Err(Error::programming(format!(
            "Invalid generated field '{model_name}.{field_name}': expression exceeds {MAX_GENERATED_SQL_BYTES} bytes output limit"
        )));
    }

    Ok(GeneratedSpec::new(storage, source, deps, tree, sql))
}

/// Final composition pass for database-generated fields across models.
pub(crate) fn finish(models: &mut [ModelSpec]) -> Result<()> {
    for model in models.iter_mut() {
        let mut compiled = Vec::new();
        for field in model
            .fields
            .iter()
            .filter(|field| field.inherited.is_none())
        {
            if let Some(spec) = &field.generated {
                let res = compile_generated_spec(
                    model,
                    field.name,
                    &field.ty,
                    spec.source,
                    spec.storage,
                )?;
                compiled.push((field.id, res));
            }
        }
        for (fid, spec) in compiled {
            model.fields[fid.0 as usize].generated = Some(spec);
        }

        for field in &model.fields {
            if field.is_virtual() {
                if field.index.is_some() {
                    return Err(Error::programming(format!(
                        "Invalid generated field '{}.{}': virtual field is referenced by 'index'",
                        model.name, field.name
                    )));
                }

                for obj in &model.table_objects {
                    match &obj.kind {
                        crate::decl::TableObjectKind::TypedIndex(idx)
                            if idx.fields.contains(&field.name)
                                || idx.include.contains(&field.name)
                                || idx.filter.as_ref().is_some_and(|f| f.contains(field.name)) =>
                        {
                            return Err(Error::programming(format!(
                                "Invalid generated field '{}.{}': virtual field is referenced by 'index '{}''",
                                model.name, field.name, obj.name
                            )));
                        }
                        crate::decl::TableObjectKind::Key(key)
                            if key.fields.contains(&field.name) =>
                        {
                            return Err(Error::programming(format!(
                                "Invalid generated field '{}.{}': virtual field is referenced by 'key '{}''",
                                model.name, field.name, obj.name
                            )));
                        }
                        crate::decl::TableObjectKind::Exclusion(excl)
                            if excl.equal.contains(&field.name)
                                || excl.period.0 == field.name
                                || excl.period.1 == field.name
                                || excl.filter.as_ref().is_some_and(|f| f.contains(field.name)) =>
                        {
                            return Err(Error::programming(format!(
                                "Invalid generated field '{}.{}': virtual field is referenced by 'exclusion '{}''",
                                model.name, field.name, obj.name
                            )));
                        }
                        crate::decl::TableObjectKind::TemporalKey(tkey)
                            if tkey.fields.contains(&field.name) || tkey.range == field.name =>
                        {
                            return Err(Error::programming(format!(
                                "Invalid generated field '{}.{}': virtual field is referenced by 'key '{}''",
                                model.name, field.name, obj.name
                            )));
                        }
                        _ => {}
                    }
                }
            }

            if field.is_generated() {
                let matches_part = model
                    .partition
                    .as_ref()
                    .is_some_and(|p| p.key_name() == field.name);
                if matches_part {
                    return Err(Error::programming(format!(
                        "Invalid generated field '{}.{}': partition key cannot be a generated field",
                        model.name, field.name
                    )));
                }
            }
        }
    }
    // Delegated fields are exposures of the owner's column, not expressions on the child.
    // Their dependency ids and SQL refer to the owning model and must not be recompiled locally.
    let mut inherited = Vec::new();
    for (index, model) in models.iter().enumerate() {
        for field in &model.fields {
            let Some(mut owner) = field.inherited.filter(|_| field.is_generated()) else {
                continue;
            };
            let mut resolved = None;
            // Delegation was already checked acyclic; keep a bound if that invariant regresses.
            for _ in 0..=models.len() {
                let parent = models[owner.parent.index()].field_by_id(owner.parent_field);
                match parent.inherited {
                    Some(next) => owner = next,
                    None => {
                        resolved = parent.generated.clone();
                        break;
                    }
                }
            }
            let generated = resolved.ok_or_else(|| {
                Error::programming(format!(
                    "Inherited generated field '{}.{}' has no owning generation metadata",
                    model.name, field.name
                ))
            })?;
            inherited.push((index, field.id, generated));
        }
    }
    for (model, field, generated) in inherited {
        models[model].fields[field.index()].generated = Some(generated);
    }
    Ok(())
}
