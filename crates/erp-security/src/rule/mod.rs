//! Rule compilation, stages 2 and 3 of the pipeline (
//! ): parse an `ir.access` domain string, evaluate it
//! **symbolically** and validate it against the model, giving a [`Domain`] whose principal values
//! are [`PrincipalParam`] leaves.
//!
//! Here those names are symbols, never values, so one compiled domain serves every principal with
//! the same groups (decision 2). The domain string is parsed by `erp-expr` under the
//! limits (source size, nesting depth); the evaluation below is a small interpreter over the
//! syntax tree that accepts exactly the forms of the §3.1 table and refuses everything else with
//! [`RuleError::Unsupported`]:
//!
//! | In the rule | Becomes |
//! |---|---|
//! | literals, lists, tuples, `-<number>` | the literal value |
//! | `user.id`; `user` or `user.ids` as a value | `Param(Uid)`; `[Param(Uid)]` |
//! | `user.<relational path>.id` | `Param(UserPath(path.id))`: one id, `False` when a many2one on the path is empty |
//! | `user.<relational path>.ids` or the path alone | `Param(UserPath(path.ids))`: the ids, none when a link on the path is empty |
//! | `company_ids`, `company_id` | `Param(CompanyIds)`, `Param(CompanyId)` |
//! | `<list> + <list>` where a side is `company_ids` or a user path's ids | one list holding the parameter (`company_ids + [False]`) |
//! | `time.strftime('%Y-%m-%d')`, `fields.Date.today()`, `datetime.date.today()`, `context_today()` | `Param(Now(Date))` |
//! | `time.strftime('%Y-%m-%d %H:%M:%S')`, `datetime.now()`, `datetime.datetime.now()`, `fields.Datetime.now()` | `Param(Now(Timestamp))` |
//! | `(f, 'in' \| 'not in' \| '=' \| '!=', user.env['m'].search(<literal domain>)[.ids])` | `(f, 'any!' \| 'not any!', <domain> & active)` |
//!
//! Relational traversals of the rule become `any!` (the predicate never applies the comodel's
//! read rules); the `access` operator is kept and resolved
//! when the predicate is combined (DOM-035). Computing on principal values (`user.x + 1`, string
//! formatting, `user.has_group(...)`, `user.env.ref(...)`, a search with a computed domain) is
//! refused: the rule must say it with a path, a literal search or the rule's group.

mod unparse;

use std::fmt;
use std::sync::Arc;

use erp_domain::{
    Domain, DomainKind, ModelContext, NowKind, Operation, Operator, PrincipalParam, UserPath, Value,
};
use erp_expr::{EvalLimits, Expr, ExprError, Literal, SubscriptKind, UnaryOp};
use erp_fields::{Error, FieldType};
use erp_orm::RuleOperatorDecl;
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec, Registry};

pub(crate) use crate::analyze::AccessLeaf;
use crate::analyze::{analyze, comodel_of};
use crate::rule_op::{Operators, RuleOperatorLeaf};
use crate::schema::RegistrySchema;

pub(crate) use unparse::py_str_repr;
use unparse::render;

/// The model whose own rules may not have a domain (SEC-016).
pub const ACCESS_MODEL: &str = "ir.access";

/// The users model every `user` path starts from.
pub const USERS_MODEL: &str = "res.users";

/// Most conditions (and domain operators) in one rule, sub-domains included (the bound of
/// `erp_domain::MAX_ITEMS` would allow more; a rule string is at most 64 KiB anyway).
pub const MAX_RULE_ITEMS: usize = 10_000;

/// Why a rule's domain does not compile. A rule with an error denies the (model, operation) it
/// applies to, for every principal it applies to (decision 2), and is listed in the build report.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RuleError {
    /// The domain string does not parse, or breaks an `erp-expr` limit.
    Parse(ExprError),
    /// A form the compiler does not turn into SQL (design note §3.1 table).
    Unsupported {
        /// The refused expression, as text (cut at 120 characters).
        expr: String,
        /// Why, and what to write instead.
        reason: String,
    },
    Invalid {
        /// The message.
        message: String,
    },
    /// The rule's model is not in the registry.
    UnknownModel {
        /// The model name.
        model: String,
    },
    /// A domain on an access rule of `ir.access` itself (SEC-016,
    /// ).
    OnAccessModel,
    /// The rule's `access` conditions reach back to an operation they are needed for (A → B → A,
    /// DOM-035 with cycle detection, R-S4).
    AccessCycle {
        /// The cycle, as (model, operation) pairs from the rule's own pair back to it.
        cycle: Vec<(String, Operation)>,
    },
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(err) => write!(f, "{err}"),
            Self::Unsupported { expr, reason } => {
                write!(f, "'{expr}' is not supported in access rules: {reason}")
            }
            Self::Invalid { message } => f.write_str(message),
            Self::UnknownModel { model } => write!(f, "the model '{model}' does not exist"),
            Self::OnAccessModel => {
                f.write_str("Accesses with domain can not be applied on model Access itself.")
            }
            Self::AccessCycle { cycle } => {
                f.write_str("its 'access' conditions form a cycle: ")?;
                for (i, (model, op)) in cycle.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" -> ")?;
                    }
                    write!(f, "{model} ({op})")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for RuleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(err) => Some(err),
            _ => None,
        }
    }
}

/// A rule's domain after stages 2 and 3.
#[derive(Debug, Clone)]
pub(crate) struct RuleDomain {
    /// The domain, principal values as parameters, traversals as `any!`, `access` kept.
    pub(crate) domain: Domain,
    /// Every (model, field) the domain reads (conditions, paths, `any!` sub-domains, user paths,
    /// searched models), each once, in first-seen order.
    pub(crate) reads: Vec<(ModelId, FieldId)>,
    /// Whether the domain names the company anywhere (DN §2.3).
    pub(crate) mentions_company: bool,
    /// The `access` conditions, each once.
    pub(crate) access: Vec<AccessLeaf>,
    /// The models a declared rule operator in the domain selects from, each once.
    pub(crate) operator_targets: Vec<ModelId>,
}

/// # Errors
///
/// A [`RuleError`]: the text does not parse (or breaks a limit), uses a form outside the §3.1
/// table, or names fields, operators or values the model does not accept.
pub(crate) fn compile(
    registry: &Registry,
    model: &ModelSpec,
    text: Option<&str>,
) -> Result<RuleDomain, RuleError> {
    compile_with(registry, model, text, &Operators::none())
}

/// [`compile`] with the declared rule operators of the policy build (Amendment 1): their
/// spelling is accepted in `text`, as a [`RuleOperatorLeaf`] that the combined predicate expands.
///
/// # Errors
///
/// As [`compile`]; a declared operator used on another model, on another field, with a value that
/// is not one of its operations, or under a negation is [`RuleError::Invalid`].
pub(crate) fn compile_with(
    registry: &Registry,
    model: &ModelSpec,
    text: Option<&str>,
    operators: &Operators,
) -> Result<RuleDomain, RuleError> {
    let mut builder = Builder::new(registry, operators);
    if model.name == ACCESS_MODEL && text.is_some_and(|t| !t.is_empty()) {
        return Err(RuleError::OnAccessModel);
    }
    let text = match text {
        Some(text) => text.trim(),
        // no domain: every record (`(access.domain or "").strip()`)
        None => "",
    };
    let domain = if text.is_empty() {
        Domain::TRUE
    } else {
        let expr =
            erp_expr::parse_with_limits(text, EvalLimits::default()).map_err(RuleError::Parse)?;
        let value = Walker { registry }.eval(&expr)?;
        builder.top_level(model, &value)?
    };
    let schema = RegistrySchema(registry);
    domain
        .optimize(&ModelContext::new(&schema, model.name))
        .map_err(|err| invalid(err.to_string()))?;
    let analysis = analyze(registry, model, &domain).map_err(|err| invalid(err.message()))?;
    Ok(RuleDomain {
        domain,
        reads: analysis.reads,
        mentions_company: builder.mentions_company,
        access: analysis.access,
        operator_targets: analysis.operator_targets,
    })
}

/// Validate the domain text of an active `ir.access` row, as the model's constraint does before
/// the row is stored (SEC-016): a row
/// without domain (`None` or empty) has nothing to validate; `access` conditions are checked but
/// not resolved (they involve other rules, which are validated on their own), so a broken rule on
/// another model never blocks this one.
///
/// # Errors
///
/// ```
/// # use erp_registry::RegistryBuilder;
/// let registry = RegistryBuilder::new([]).linked(false).build()?;
/// erp_security::rule::validate(&registry, "res.users", Some("[('login', '=', 'admin')]"))?;
/// let err = erp_security::rule::validate(&registry, "res.users", Some("[('nope', '=', 1)]"))
///     .unwrap_err();
/// assert_eq!(err.kind(), erp_fields::ErrorKind::Validation);
/// assert!(err.message().starts_with("Invalid domain [('nope', '=', 1)]: "));
/// # Ok::<(), erp_fields::Error>(())
/// ```
pub fn validate(registry: &Registry, model: &str, text: Option<&str>) -> erp_fields::Result<()> {
    // the declared rule operators linked into the binary (Amendment 1)
    let operators = Operators::new(registry, erp_orm::RULE_OPERATORS.iter())?;
    let shown = match text {
        Some(shown) if !shown.is_empty() => shown,
        Some(_) | None => return Ok(()),
    };
    let result = match registry.model(model) {
        Some(spec) => compile_with(registry, spec, text, &operators).map(|_| ()),
        None => Err(RuleError::UnknownModel {
            model: model.to_owned(),
        }),
    };
    match result {
        Ok(()) => Ok(()),
        Err(RuleError::OnAccessModel) => {
            Err(Error::validation(RuleError::OnAccessModel.to_string()))
        }
        Err(err) => {
            Err(Error::validation(format!("Invalid domain {shown}: {err}")).with_source(err))
        }
    }
}

// ----- symbolic evaluation ---------------------------------------------------------------------

/// A value of the symbolic evaluation.
#[derive(Debug, Clone)]
enum Sym {
    /// A literal (`None` is `False`, as in domain conditions).
    Lit(Value),
    /// A list or tuple; items may be parameters.
    Seq { items: Vec<Sym>, tuple: bool },
    /// A principal value.
    Param(PrincipalParam),
    /// A principal value written as a list of ids (`user.<path>.ids`, a record value): it
    /// concatenates with lists like `company_ids` does.
    Ids(PrincipalParam),
    /// The `user` record (as a value: its ids).
    User,
    /// `user.env['<model>'].search(<literal domain>)`, with or without `.ids`.
    Search { model: String, domain: Box<Sym> },
}

impl Sym {
    /// Whether this is a parameter standing for several ids (`company_ids`, `user.<path>.ids`).
    fn is_collection_param(&self) -> bool {
        match self {
            Sym::Ids(_) => true,
            Sym::Param(param) => param.is_collection() == Some(true),
            Sym::Lit(_) | Sym::Seq { .. } | Sym::User | Sym::Search { .. } => false,
        }
    }
}

fn unsupported(expr: &Expr, reason: impl Into<String>) -> RuleError {
    RuleError::Unsupported {
        expr: render(expr),
        reason: reason.into(),
    }
}

fn invalid(message: impl Into<String>) -> RuleError {
    RuleError::Invalid {
        message: message.into(),
    }
}

const NAMES: &str = "a rule may use literals and the names user, company_ids, company_id and time";

struct Walker<'r> {
    registry: &'r Registry,
}

impl Walker<'_> {
    /// Evaluate `expr`. Recursion follows the syntax tree, whose depth the parser bounds
    /// (100 levels).
    fn eval(&self, expr: &Expr) -> Result<Sym, RuleError> {
        match expr {
            Expr::Literal(literal) => literal_value(expr, literal),
            Expr::List(items) => Ok(Sym::Seq {
                items: items
                    .iter()
                    .map(|item| self.eval(item))
                    .collect::<Result<_, _>>()?,
                tuple: false,
            }),
            Expr::Tuple(items) => Ok(Sym::Seq {
                items: items
                    .iter()
                    .map(|item| self.eval(item))
                    .collect::<Result<_, _>>()?,
                tuple: true,
            }),
            Expr::Unary { op, operand } => self.unary(expr, *op, operand),
            Expr::Name(name) => match name.as_str() {
                "user" => Ok(Sym::User),
                "company_ids" => Ok(Sym::Param(PrincipalParam::CompanyIds)),
                "company_id" => Ok(Sym::Param(PrincipalParam::CompanyId)),
                _ => Err(unsupported(expr, NAMES)),
            },
            Expr::Attribute { value, attr } => self.attribute(expr, value, attr),
            Expr::Call {
                func,
                args,
                keywords,
            } => self.call(expr, func, args, keywords),
            Expr::Binary {
                op: erp_expr::BinaryOp::Add,
                left,
                right,
            } => self.concat(expr, self.eval(left)?, self.eval(right)?),
            _ => Err(unsupported(
                expr,
                "rules are lists of conditions on literal values and principal symbols; \
                 computations are not compiled to SQL",
            )),
        }
    }

    fn unary(&self, expr: &Expr, op: UnaryOp, operand: &Expr) -> Result<Sym, RuleError> {
        let Expr::Literal(literal) = operand else {
            return Err(unsupported(expr, "only a number literal can be negated"));
        };
        match (op, literal) {
            (UnaryOp::USub, Literal::Int(i)) => match i.checked_neg() {
                Some(n) => Ok(Sym::Lit(Value::Int(n))),
                None => Err(invalid(format!("OverflowError: -({i}) is out of range"))),
            },
            (UnaryOp::USub, Literal::Float(f)) => Ok(Sym::Lit(Value::Float(-f))),
            (UnaryOp::UAdd, Literal::Int(i)) => Ok(Sym::Lit(Value::Int(*i))),
            (UnaryOp::UAdd, Literal::Float(f)) => Ok(Sym::Lit(Value::Float(*f))),
            _ => Err(unsupported(expr, "only a number literal can be negated")),
        }
    }

    /// `user.<path>`, `user.env['m'].search(…).ids`.
    fn attribute(&self, expr: &Expr, value: &Expr, attr: &str) -> Result<Sym, RuleError> {
        if attr == "ids"
            && let Expr::Call {
                func,
                args,
                keywords,
            } = value
            && let Some(search) = self.search(value, func, args, keywords)?
        {
            return Ok(search);
        }
        let Some((base, path)) = attribute_chain(expr) else {
            return Err(unsupported(
                expr,
                "only attributes of user (a path of relational fields ending in .id or .ids) \
                 are compiled",
            ));
        };
        if base != "user" {
            return Err(unsupported(expr, NAMES));
        }
        self.user_path(expr, &path)
    }

    /// The parameter of `user.<path>` (the attributes after `user`).
    fn user_path(&self, expr: &Expr, path: &[&str]) -> Result<Sym, RuleError> {
        match path {
            ["id"] => return Ok(Sym::Param(PrincipalParam::Uid)),
            ["ids"] => {
                return Ok(Sym::Seq {
                    items: vec![Sym::Param(PrincipalParam::Uid)],
                    tuple: false,
                });
            }
            ["env", ..] => {
                return Err(unsupported(
                    expr,
                    "user.env is available only as user.env['<model>'].search(<literal domain>)",
                ));
            }
            _ => {}
        }
        let (fields, terminal) = match path.split_last() {
            Some((&"id", fields)) => (fields, Some("id")),
            Some((&"ids", fields)) => (fields, Some("ids")),
            _ => (path, None),
        };
        let Some(mut model) = self.registry.model(USERS_MODEL) else {
            return Err(invalid(format!(
                "the model {USERS_MODEL} is not in the registry, so 'user.' paths cannot be \
                 compiled"
            )));
        };
        let mut through_many = false;
        for (i, name) in fields.iter().enumerate() {
            let Some(field) = model.field(name) else {
                return Err(invalid(format!(
                    "AttributeError: '{}' object has no attribute {} (in {})",
                    model.name,
                    py_str_repr(name),
                    render(expr)
                )));
            };
            let Some(comodel) = comodel_of(&field.ty) else {
                let rest = i + 1 < fields.len() || terminal.is_some();
                let reason = if rest {
                    format!(
                        "'{}' of {} is not a relational field, so the path cannot continue",
                        field.name, model.name
                    )
                } else {
                    format!(
                        "user.{} is a value of the user's record, not a record id: rules \
                         compare ids (user.<path>.id or .ids)",
                        fields.join(".")
                    )
                };
                return Err(unsupported(expr, reason));
            };
            // the path becomes a sub-select from the user's row: every hop must be stored
            // (the same rule as erp-sql's lowering, so the error comes at build time)
            if !field.store || field.is_computed() {
                return Err(unsupported(
                    expr,
                    format!(
                        "'{}' of {} is not a stored field, so the path cannot be read in SQL",
                        field.name, model.name
                    ),
                ));
            }
            through_many |= matches!(
                field.ty,
                FieldType::One2many { .. } | FieldType::Many2many { .. }
            );
            let Some(next) = self.registry.model(comodel) else {
                return Err(invalid(format!(
                    "the comodel {comodel} of {}.{} is not in the registry",
                    model.name, field.name
                )));
            };
            model = next;
        }
        if terminal == Some("id") && through_many {
            return Err(unsupported(
                expr,
                "'.id' of a path through a one2many or many2many field names several records: \
                 use '.ids'",
            ));
        }
        let terminal = match terminal {
            Some("id") => "id",
            _ => "ids",
        };
        let path = UserPath::new(fields.iter().copied().chain([terminal]))
            .map_err(|err| unsupported(expr, err.to_string()))?;
        let param = PrincipalParam::UserPath(path);
        Ok(if terminal == "id" {
            Sym::Param(param)
        } else {
            Sym::Ids(param)
        })
    }

    fn call(
        &self,
        expr: &Expr,
        func: &Expr,
        args: &[Expr],
        keywords: &[(Option<String>, Expr)],
    ) -> Result<Sym, RuleError> {
        if let Some(search) = self.search(expr, func, args, keywords)? {
            return Ok(search);
        }
        let Some((base, path)) = attribute_chain(func) else {
            return Err(unsupported(expr, NAMES));
        };
        let no_args = args.is_empty() && keywords.is_empty();
        let now = match (base, path.as_slice()) {
            ("time", ["strftime"]) => {
                let format = match (args, keywords.is_empty()) {
                    ([Expr::Literal(Literal::Str(format))], true) => format.as_str(),
                    _ => {
                        return Err(unsupported(
                            expr,
                            "time.strftime takes one literal format: '%Y-%m-%d' (today) or \
                             '%Y-%m-%d %H:%M:%S' (now)",
                        ));
                    }
                };
                match format {
                    "%Y-%m-%d" => NowKind::Date,
                    "%Y-%m-%d %H:%M:%S" => NowKind::Timestamp,
                    _ => {
                        return Err(unsupported(
                            expr,
                            "time.strftime is compiled for the formats '%Y-%m-%d' (today) and \
                             '%Y-%m-%d %H:%M:%S' (now) only",
                        ));
                    }
                }
            }
            ("fields", ["Date", "today"])
            | ("datetime", ["date", "today"])
            | ("context_today", [])
                if no_args =>
            {
                NowKind::Date
            }
            ("datetime", ["now"])
            | ("datetime", ["datetime", "now"])
            | ("fields", ["Datetime", "now"])
                if no_args =>
            {
                NowKind::Timestamp
            }
            ("user", ["has_group"]) => {
                return Err(unsupported(
                    expr,
                    "group checks belong in the rule's group: make it a permission of that group",
                ));
            }
            _ => {
                return Err(unsupported(
                    expr,
                    "calls are not compiled to SQL, except the 'today' and 'now' forms and \
                     user.env['<model>'].search(<literal domain>)",
                ));
            }
        };
        Ok(Sym::Param(PrincipalParam::Now(now)))
    }

    /// `user.env['<model>'].search(<literal domain>)`, or `None` when `func` is not that form.
    fn search(
        &self,
        expr: &Expr,
        func: &Expr,
        args: &[Expr],
        keywords: &[(Option<String>, Expr)],
    ) -> Result<Option<Sym>, RuleError> {
        let Expr::Attribute { value, attr } = func else {
            return Ok(None);
        };
        if attr != "search" {
            return Ok(None);
        }
        let Expr::Subscript {
            value: env,
            slice: SubscriptKind::Index(index),
        } = &**value
        else {
            return Ok(None);
        };
        if attribute_chain(env) != Some(("user", vec!["env"])) {
            return Ok(None);
        }
        let Expr::Literal(Literal::Str(model)) = &**index else {
            return Err(unsupported(
                expr,
                "the searched model must be a literal name: user.env['<model>']",
            ));
        };
        let ([domain], true) = (args, keywords.is_empty()) else {
            return Err(unsupported(
                expr,
                "search takes one literal domain (no offset, limit or order)",
            ));
        };
        let domain = self.eval(domain)?;
        if !is_literal(&domain) {
            return Err(unsupported(
                expr,
                "the domain of a search in a rule must be literal (no user, company or time \
                 symbols)",
            ));
        }
        Ok(Some(Sym::Search {
            model: model.clone(),
            domain: Box::new(domain),
        }))
    }

    /// `a + b` on lists (`company_ids + [False]`).
    fn concat(&self, expr: &Expr, left: Sym, right: Sym) -> Result<Sym, RuleError> {
        // (items, is a tuple); a collection parameter is a list (`company_ids` is a list of ids)
        let as_items = |sym: Sym| -> Option<(Vec<Sym>, bool)> {
            if sym.is_collection_param() {
                return Some((vec![sym], false));
            }
            match sym {
                Sym::Seq { items, tuple } => Some((items, tuple)),
                Sym::Lit(_) | Sym::Param(_) | Sym::Ids(_) | Sym::User | Sym::Search { .. } => None,
            }
        };
        let (Some((mut a, ta)), Some((b, tb))) = (as_items(left), as_items(right)) else {
            return Err(unsupported(
                expr,
                "'+' is compiled only to join lists (company_ids + [False]); computations on \
                 values are not",
            ));
        };
        if ta != tb {
            return Err(invalid(format!(
                "TypeError: can only concatenate list to list and tuple to tuple (in {})",
                render(expr)
            )));
        }
        a.extend(b);
        Ok(Sym::Seq {
            items: a,
            tuple: ta,
        })
    }
}

fn literal_value(expr: &Expr, literal: &Literal) -> Result<Sym, RuleError> {
    Ok(Sym::Lit(match literal {
        Literal::None => Value::FALSE,
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Int(i) => Value::Int(*i),
        Literal::Float(f) => Value::Float(*f),
        Literal::Str(s) => Value::Str(s.clone()),
        _ => {
            return Err(unsupported(
                expr,
                "bytes and other literals are not domain values",
            ));
        }
    }))
}

/// `base.a.b.c` as `("base", ["a", "b", "c"])` (an empty path for a bare name).
fn attribute_chain(expr: &Expr) -> Option<(&str, Vec<&str>)> {
    let mut attrs = Vec::new();
    let mut current = expr;
    loop {
        match current {
            Expr::Attribute { value, attr } => {
                attrs.push(attr.as_str());
                current = value;
            }
            Expr::Name(name) => {
                attrs.reverse();
                return Some((name.as_str(), attrs));
            }
            _ => return None,
        }
    }
}

fn is_literal(sym: &Sym) -> bool {
    match sym {
        Sym::Lit(_) => true,
        Sym::Seq { items, .. } => items.iter().all(is_literal),
        Sym::Param(_) | Sym::Ids(_) | Sym::User | Sym::Search { .. } => false,
    }
}

// ----- domain building and validation ------------------------------------------------------------

/// Python repr of a symbolic value, for messages.
fn sym_repr(sym: &Sym) -> String {
    match sym {
        Sym::Lit(Value::Str(s)) => py_str_repr(s),
        Sym::Lit(Value::Bool(true)) => "True".to_owned(),
        Sym::Lit(Value::Bool(false)) => "False".to_owned(),
        Sym::Lit(Value::Int(i)) => i.to_string(),
        Sym::Lit(Value::Float(f)) => format!("{f:?}"),
        Sym::Lit(other) => format!("{other:?}"),
        Sym::Seq { items, tuple } => {
            let inner: Vec<String> = items.iter().map(sym_repr).collect();
            if *tuple {
                if inner.len() == 1 {
                    format!("({},)", inner[0])
                } else {
                    format!("({})", inner.join(", "))
                }
            } else {
                format!("[{}]", inner.join(", "))
            }
        }
        Sym::Param(param) => param.symbol(),
        Sym::Ids(param) => format!("{}.ids", param.symbol()),
        Sym::User => "user".to_owned(),
        Sym::Search { model, domain } => {
            format!(
                "user.env[{}].search({})",
                py_str_repr(model),
                sym_repr(domain)
            )
        }
    }
}

fn clipped(text: String) -> String {
    match text.char_indices().nth(200) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

fn condition_repr(field: &Sym, op: &Sym, value: &Sym) -> String {
    clipped(format!(
        "({}, {}, {})",
        sym_repr(field),
        sym_repr(op),
        sym_repr(value)
    ))
}

/// A stack entry of the prefix-notation parser: a domain and its connective depth.
struct Entry {
    domain: Domain,
    depth: usize,
}

/// Builds the domain of a rule from its symbolic value, validating it against the models; it
/// records whether the input names the company before any constant is folded away (DN §2.3).
struct Builder<'r> {
    registry: &'r Registry,
    operators: &'r Operators,
    mentions_company: bool,
    items: usize,
}

impl<'r> Builder<'r> {
    fn new(registry: &'r Registry, operators: &'r Operators) -> Self {
        Self {
            registry,
            operators,
            mentions_company: false,
            items: 0,
        }
    }

    /// `(field, '<declared operator>', '<operation>')` (Amendment 1): valid only on the
    /// declaring model, on its declared field, with one of its operations. Becomes a
    /// [`RuleOperatorLeaf`] node, expanded when the combined predicate is built.
    fn declared_operator(
        &self,
        model: &ModelSpec,
        decl: &'static RuleOperatorDecl,
        triple: (&Sym, &Sym),
        render: &dyn Fn() -> String,
    ) -> Result<Domain, RuleError> {
        let (field, value) = triple;
        if model.name != decl.model() {
            return Err(invalid(format!(
                "The '{}' operator works only on {} in condition {}",
                decl.name(),
                decl.model(),
                render()
            )));
        }
        if !matches!(field, Sym::Lit(Value::Str(f)) if f == decl.field()) {
            return Err(invalid(format!(
                "The '{}' operator works only on the field '{}' in condition {}",
                decl.name(),
                decl.field(),
                render()
            )));
        }
        let op = match value {
            Sym::Lit(Value::Str(v)) => Operation::parse(v),
            _ => None,
        };
        match op {
            Some(op) if decl.operations().contains(&op) => {
                Ok(Domain::custom(Arc::new(RuleOperatorLeaf::new(decl, op))))
            }
            _ => Err(invalid(format!(
                "Invalid value for '{}' operator in condition {}",
                decl.name(),
                render()
            ))),
        }
    }

    fn model(&self, name: &str) -> Result<&'r ModelSpec, RuleError> {
        self.registry
            .model(name)
            .ok_or_else(|| invalid(format!("the model {name} is not in the registry")))
    }

    fn top_level(&mut self, model: &ModelSpec, value: &Sym) -> Result<Domain, RuleError> {
        match value {
            Sym::Seq { items, .. } => self.domain(model, items),
            Sym::Lit(Value::Bool(b)) => Ok(Domain::constant(*b)),
            other => Err(invalid(format!(
                "TypeError: Domain() invalid argument type for domain: {}",
                clipped(sym_repr(other))
            ))),
        }
    }

    /// Prefix-notation list syntax: `&`, `|`, `!` operators, implicit AND at the top.
    /// Linear; the connective depth is bounded by
    /// `erp_domain::MAX_DEPTH` and the items by [`MAX_RULE_ITEMS`].
    fn domain(&mut self, model: &ModelSpec, items: &[Sym]) -> Result<Domain, RuleError> {
        let malformed = || {
            invalid(format!(
                "ValueError: Domain() malformed domain {}",
                clipped(sym_repr(&Sym::Seq {
                    items: items.to_vec(),
                    tuple: false,
                }))
            ))
        };
        let mut stack: Vec<Entry> = Vec::new();
        for item in items.iter().rev() {
            self.items += 1;
            if self.items > MAX_RULE_ITEMS {
                return Err(invalid(format!(
                    "the domain has more than {MAX_RULE_ITEMS} items"
                )));
            }
            let entry = match item {
                Sym::Seq { items: triple, .. } if triple.len() == 3 => Entry {
                    domain: self.condition(model, triple)?,
                    depth: 0,
                },
                Sym::Lit(Value::Str(s)) if s == "&" || s == "|" => {
                    let left = stack.pop().ok_or_else(malformed)?;
                    let right = stack.pop().ok_or_else(malformed)?;
                    let and = s == "&";
                    // children of the same connective are flattened into the node, so they do
                    // not add a level (as `erp_domain`'s parser counts)
                    let level = |e: &Entry| match (e.domain.kind(), and) {
                        (DomainKind::And(_), true) | (DomainKind::Or(_), false) => e.depth,
                        _ => e.depth + 1,
                    };
                    let depth = level(&left).max(level(&right));
                    let domain = if and {
                        Domain::and([left.domain, right.domain])
                    } else {
                        Domain::or([left.domain, right.domain])
                    };
                    Entry { domain, depth }
                }
                Sym::Lit(Value::Str(s)) if s == "!" => {
                    let child = stack.pop().ok_or_else(malformed)?;
                    // a negated declared operator could let rows through where the expansion
                    // is unknown (NULL, a model the principal cannot read): refused
                    if holds_rule_operator(&child.domain) {
                        return Err(invalid(format!(
                            "a declared rule operator cannot be negated: {}",
                            clipped(sym_repr(&Sym::Seq {
                                items: items.to_vec(),
                                tuple: false,
                            }))
                        )));
                    }
                    Entry {
                        domain: !child.domain,
                        depth: child.depth + 1,
                    }
                }
                other => {
                    return Err(invalid(format!(
                        "ValueError: Domain() invalid item in domain: {}",
                        clipped(sym_repr(other))
                    )));
                }
            };
            if entry.depth > erp_domain::MAX_DEPTH {
                return Err(invalid(format!(
                    "the domain nests more than {} operators",
                    erp_domain::MAX_DEPTH
                )));
            }
            stack.push(entry);
        }
        Ok(Domain::and(stack.into_iter().rev().map(|e| e.domain)))
    }

    fn condition(&mut self, model: &ModelSpec, triple: &[Sym]) -> Result<Domain, RuleError> {
        let [field, op, value] = triple else {
            return Err(invalid("a condition has three items"));
        };
        let render = || condition_repr(field, op, value);
        let field_expr = match field {
            Sym::Lit(Value::Str(f)) if !f.is_empty() => f.as_str(),
            Sym::Lit(Value::Str(_)) => {
                return Err(invalid(format!(
                    "Empty field name in condition {}",
                    render()
                )));
            }
            Sym::Lit(f) => {
                // the constant leaves (1, '=', 1) and (0, '=', 1), with Python equality
                if let (Sym::Lit(o), Sym::Lit(v)) = (op, value)
                    && *o == Value::Str("=".to_owned())
                    && *v == Value::Int(1)
                {
                    if *f == Value::Int(1) {
                        return Ok(Domain::TRUE);
                    }
                    if *f == Value::Int(0) {
                        return Ok(Domain::FALSE);
                    }
                }
                return Err(invalid(format!(
                    "TypeError: Domain() invalid arguments: {}",
                    render()
                )));
            }
            _ => {
                return Err(invalid(format!(
                    "TypeError: Domain() invalid arguments: {}",
                    render()
                )));
            }
        };
        let operator = match op {
            Sym::Lit(Value::Str(o)) => Operator::parse(o),
            _ => None,
        };
        if operator.is_none()
            && let Sym::Lit(Value::Str(name)) = op
            && let Some(decl) = self.operators.by_name(name)
        {
            return self.declared_operator(model, decl, (field, value), &render);
        }
        let Some(operator) = operator else {
            return Err(invalid(format!(
                "Invalid operator in condition {}",
                render()
            )));
        };
        if operator.is_internal() {
            return Err(invalid(format!(
                "ValueError: Domain() invalid item in domain: {}",
                render()
            )));
        }
        let (name, rest) = match field_expr.split_once('.') {
            Some((name, rest)) => (name, Some(rest)),
            None => (field_expr, None),
        };
        let Some(spec) = model.field(name) else {
            return Err(invalid(format!(
                "Invalid field {}.{} in condition {}",
                model.name,
                name,
                render()
            )));
        };
        if matches!(name, "company_id" | "company_ids") {
            self.mentions_company = true;
        }
        let comodel = comodel_of(&spec.ty);
        if let Some(rest) = rest {
            if operator == Operator::Access {
                return Err(invalid(format!(
                    "The 'access' operator does not work for properties in condition {}",
                    render()
                )));
            }
            if let Some(comodel) = comodel {
                // a path: `a.b op v` is `a any!
                let comodel = self.model(comodel)?;
                let inner = Sym::Seq {
                    items: vec![
                        Sym::Lit(Value::Str(rest.to_owned())),
                        op.clone(),
                        value.clone(),
                    ],
                    tuple: true,
                };
                let sub = self.domain(comodel, std::slice::from_ref(&inner))?;
                return condition(name, Operator::AnyBypass, Value::Domain(sub));
            }
            // a property or date part of a non-relational field: kept as written
        }
        match operator {
            Operator::Any | Operator::NotAny => {
                let Some(comodel) = comodel else {
                    return Err(invalid(format!(
                        "The 'any' operator works only on relational fields in condition {}",
                        render()
                    )));
                };
                let comodel = self.model(comodel)?;
                let sub = match value {
                    Sym::Seq { items, .. } => self.domain(comodel, items)?,
                    Sym::Lit(Value::Bool(b)) => Domain::constant(*b),
                    _ => {
                        return Err(invalid(format!(
                            "The value of an 'any' condition is a domain in condition {}",
                            render()
                        )));
                    }
                };
                // `not any` over a declared operator is a negation of it (Amendment 1)
                if operator == Operator::NotAny && holds_rule_operator(&sub) {
                    return Err(invalid(format!(
                        "a declared rule operator cannot be negated: {}",
                        render()
                    )));
                }
                let bypass = if operator == Operator::Any {
                    Operator::AnyBypass
                } else {
                    Operator::NotAnyBypass
                };
                condition(field_expr, bypass, Value::Domain(sub))
            }
            Operator::Access => {
                // `id` (the model itself) or a many2one whose comodel is in the registry; the
                // target's predicate is combined in when a principal's predicate is built
                match (name, &spec.ty, comodel) {
                    ("id", _, _) => {}
                    (_, FieldType::Many2one { .. }, Some(comodel)) => {
                        self.model(comodel)?;
                    }
                    _ => {
                        return Err(invalid(format!(
                            "The 'access' operator works only for many2one and 'id' fields in \
                             condition {}",
                            render()
                        )));
                    }
                }
                let op = match value {
                    Sym::Lit(Value::Str(v)) => Operation::parse(v),
                    _ => None,
                };
                let Some(op) = op else {
                    return Err(invalid(format!(
                        "Invalid value for 'access' operator in condition {}",
                        render()
                    )));
                };
                condition(
                    field_expr,
                    Operator::Access,
                    Value::Str(op.as_str().to_owned()),
                )
            }
            _ => {
                if let Sym::Search {
                    model: searched,
                    domain,
                } = value
                {
                    return self.search_condition(model, spec, operator, searched, domain, &render);
                }
                let value = self.value(value, &render)?;
                condition(field_expr, operator, value)
            }
        }
    }

    /// `(f, in, user.env['m'].search(D))`: `(f, any!, D & active)` when `f` holds ids of `m`.
    fn search_condition(
        &mut self,
        model: &ModelSpec,
        spec: &FieldSpec,
        operator: Operator,
        searched: &str,
        domain: &Sym,
        render: &dyn Fn() -> String,
    ) -> Result<Domain, RuleError> {
        let bypass = match operator {
            Operator::In | Operator::Eq => Operator::AnyBypass,
            Operator::NotIn | Operator::Ne => Operator::NotAnyBypass,
            _ => {
                return Err(RuleError::Unsupported {
                    expr: render(),
                    reason: "the ids of a search compare with 'in', 'not in', '=' or '!='"
                        .to_owned(),
                });
            }
        };
        let Some(target) = self.registry.model(searched) else {
            return Err(invalid(format!(
                "KeyError: {} (no such model) in condition {}",
                py_str_repr(searched),
                render()
            )));
        };
        let holds_ids_of = match comodel_of(&spec.ty) {
            Some(comodel) => comodel == target.name,
            None => matches!(spec.ty, FieldType::Id) && model.id == target.id,
        };
        if !holds_ids_of {
            return Err(RuleError::Unsupported {
                expr: render(),
                reason: format!(
                    "{}.{} does not hold ids of {}, so the search cannot become a sub-query",
                    model.name, spec.name, target.name
                ),
            });
        }
        let mut sub = match domain {
            Sym::Seq { items, .. } => self.domain(target, items)?,
            Sym::Lit(Value::Bool(b)) => Domain::constant(*b),
            other => {
                return Err(invalid(format!(
                    "TypeError: Domain() invalid argument type for domain: {}",
                    clipped(sym_repr(other))
                )));
            }
        };
        // `not in` / `!=` over a search holding a declared operator negates it (Amendment 1)
        if bypass == Operator::NotAnyBypass && holds_rule_operator(&sub) {
            return Err(invalid(format!(
                "a declared rule operator cannot be negated: {}",
                render()
            )));
        }
        // `search` filters archived records (the rule's `user` has an empty context, so
        // `active_test` is on) unless the domain names the active field
        if let Some(active) = target.active_name {
            let active = target.field_by_id(active);
            let named = sub.iter_conditions().any(|d| {
                d.as_condition()
                    .is_some_and(|c| c.field_expr() == active.name)
            });
            if !named {
                sub = Domain::and([
                    condition(active.name, Operator::In, Value::list([true]))?,
                    sub,
                ]);
            }
        }
        condition(spec.name, bypass, Value::Domain(sub))
    }

    /// A condition value: literals as they are, the `user` record as its ids, parameters as
    /// [`Value::Param`] (their paths read).
    fn value(&mut self, sym: &Sym, render: &dyn Fn() -> String) -> Result<Value, RuleError> {
        Ok(match sym {
            Sym::Lit(v) => v.clone(),
            Sym::Seq { items, .. } => Value::List(
                items
                    .iter()
                    .map(|item| match item {
                        Sym::User => Err(RuleError::Unsupported {
                            expr: render(),
                            reason: "a record inside a list is not a value: use user.id".to_owned(),
                        }),
                        item => self.value(item, render),
                    })
                    .collect::<Result<_, _>>()?,
            ),
            Sym::Param(param) | Sym::Ids(param) => {
                self.param(param);
                Value::Param(param.clone())
            }
            Sym::User => Value::List(vec![Value::Param(PrincipalParam::Uid)]),
            Sym::Search { .. } => {
                return Err(RuleError::Unsupported {
                    expr: render(),
                    reason: "a search is compiled only as the value of 'in', 'not in', '=' or \
                             '!='"
                        .to_owned(),
                });
            }
        })
    }

    /// Record whether a parameter names the company (`company_id`, `company_ids`, or a user path
    /// through a field of that name: wider is the safe direction).
    fn param(&mut self, param: &PrincipalParam) {
        let names_company = match param {
            PrincipalParam::CompanyId | PrincipalParam::CompanyIds => true,
            PrincipalParam::UserPath(path) => path
                .fields()
                .iter()
                .any(|f| matches!(&**f, "company_id" | "company_ids")),
            // the user id, the time and grant ids say nothing about the company
            _ => false,
        };
        self.mentions_company |= names_company;
    }
}

/// Whether `domain` holds a declared-operator leaf, sub-domains included.
pub(crate) fn holds_rule_operator(domain: &Domain) -> bool {
    match domain.kind() {
        DomainKind::True | DomainKind::False => false,
        DomainKind::Custom(node) => RuleOperatorLeaf::of(node.as_ref()).is_some(),
        DomainKind::Not(child) => holds_rule_operator(child),
        DomainKind::And(children) | DomainKind::Or(children) => {
            children.iter().any(holds_rule_operator)
        }
        DomainKind::Condition(c) => {
            matches!(c.value(), Value::Domain(sub) if holds_rule_operator(sub))
        }
    }
}

fn condition(field: &str, operator: Operator, value: Value) -> Result<Domain, RuleError> {
    Domain::condition(field, operator, value).map_err(|err| invalid(err.to_string()))
}

#[cfg(test)]
mod tests;
