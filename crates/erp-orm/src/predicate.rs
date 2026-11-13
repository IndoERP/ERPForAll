//! In-memory evaluation of domains: the engine of `filtered_domain` (SPEC-DOM §4.17, RSET §4.10,
//!
//! Evaluation is column-wise (RSET W1): every field the domain reads is loaded
//! once for the whole recordset (one prefetch query at most for the stored ones, none when cached),
//! each condition is prepared once — literals converted to the field's type with the converter the
//! SQL compiler uses (`erp_sql::param::to_scalar`) — and then runs as one tight loop over the rows
//! still in play.
//!
//! The NULL rules are the ones the SQL compiler implements (`erp_sql::filter`), so
//! `search(D) == search([]).filtered_domain(D)`: a type's falsy value (`0`, `0.0`,
//! `''`) behaves like NULL in `in`/`not in`; inequalities accept NULL when the falsy value satisfies
//! them; negative operators are the negation of the positive test, so NULL rows satisfy `not in`
//! and `not like`; booleans compare NULL as false; `id` of a new record is its origin's id.
//!
//! `like` on a non-text
//! column matches the text PostgreSQL's `::text` cast produces (shortest-digit floats with
//! `e+16`/`e-05` exponents, numeric with its stored scale, ISO dates, timestamps with trimmed
//! microseconds, `true`/`false`); JSON is refused. A pattern ending with the escape character is
//! an error, as in PostgreSQL. Unknown fields are reported even when optimization would discard
//! their condition.
//!
//! Known differences from SQL: on a database with another collation, text order and case folding
//! follow `C` anyway; accent folding (`OrmConfig::unaccent`) covers Latin-1 and Latin Extended-A
//! letters (PostgreSQL's `unaccent` rules for those, up to case), not the extension's full rule
//! file.
//!
//! Records queries (`records.as_query()`) are their ids.
//! Hierarchy operators are resolved to id conditions first (one recursive query per condition, as
//! in a search). SQL values, custom nodes and property paths are refused with a "not supported
//! until" error.

use std::cmp::Ordering;
use std::sync::Arc;

use erp_domain::{
    Condition, Domain, DomainKind, OpaqueKind, Operator, OptimizationLevel, Value as DomainValue,
};
use erp_fields::{
    Cell, Date, DateRange, DateTime, DatetimeRange, Decimal, Error, FieldType, Range, RangeBounds,
    RecId, Result,
};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec, Registry};
use erp_sql::param::{Scalar, ScalarKind, falsy_value, to_scalar};
use erp_sql::reference::ReferenceQuery;
use rustc_hash::FxHashSet;

use crate::Transaction;
use crate::dispatch::BoxFuture;
use crate::env::Env;
use crate::optimize::{OptimizeCtx, ZoneTz, now_utc, optimize_domain};
use crate::prepare::RecordsQuery;
use crate::records::Records;

/// `filtered_domain(domain)`: the records of `records` satisfying `domain`, in
/// order with duplicates, sharing the prefetch set. An empty recordset or the `TRUE` domain gives
/// the recordset back without any work.
pub(crate) fn filter_records<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    domain: &'a Domain,
) -> BoxFuture<'a, Result<Records>> {
    Box::pin(filter_records_inner(tx, records, domain, None))
}

/// `('id', 'access', op)` becomes `id in <records allowed for op>`;
/// `('m2o', 'access', op)` becomes `id in <readable records> AND m2o in <targets allowed for op>`.
async fn access_in_memory(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
) -> Result<Domain> {
    let spec = records.spec();
    let mut replacements: Vec<(Domain, Domain)> = Vec::new();
    for node in domain.iter_conditions() {
        let Some(condition) = node.as_condition() else {
            continue;
        };
        if condition.operator() != Operator::Access {
            continue;
        }
        let op = match condition.value() {
            DomainValue::Str(name) => crate::access::Operation::parse(name),
            _ => None,
        };
        let (Some(op), Some(field)) = (op, spec.field(condition.field_expr())) else {
            // left as is: the optimization below raises the error of a bad operation or field
            continue;
        };
        let user = records.sudo(false);
        let ids = |r: &Records| -> DomainValue {
            DomainValue::list(
                r.rec_ids()
                    .iter()
                    .filter_map(|i| i.as_real())
                    .map(|i| i.get()),
            )
        };
        let replacement = match &field.ty {
            FieldType::Id => {
                let allowed = user.filtered_access(tx, op).await?;
                Domain::condition("id", Operator::In, ids(&allowed))?
            }
            FieldType::Many2one { .. } => {
                let readable = user
                    .filtered_access(tx, crate::access::Operation::Read)
                    .await?;
                let targets = records.engine_sudo().related(tx, field.name).await?;
                let allowed = targets
                    .with_env(user.env().clone())
                    .filtered_access(tx, op)
                    .await?;
                Domain::condition("id", Operator::In, ids(&readable))?
                    & Domain::condition(field.name, Operator::In, ids(&allowed))?
            }
            // left as is: the optimization below refuses `access` on other field types
            _ => continue,
        };
        replacements.push((node.clone(), replacement));
    }
    if replacements.is_empty() {
        return Ok(domain.clone());
    }
    Ok(domain.map_conditions(|node| {
        replacements
            .iter()
            .find(|(from, _)| from == node)
            .map_or_else(|| node.clone(), |(_, to)| to.clone())
    }))
}

fn filter_sub<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    domain: &'a Domain,
    reset: Option<Env>,
) -> BoxFuture<'a, Result<Records>> {
    Box::pin(filter_records_inner(tx, records, domain, reset))
}

async fn filter_records_inner(
    tx: &mut Transaction,
    records: &Records,
    domain: &Domain,
    reset: Option<Env>,
) -> Result<Records> {
    if records.is_empty() || domain.is_true() {
        return Ok(records.clone());
    }
    tx.admit_records(records).await?;
    if domain.has_params() {
        // access predicates are decided in SQL only (R-S6); the property tests bind
        // the parameters to values before they evaluate a predicate in memory
        return Err(Error::programming(format!(
            "filtered_domain cannot evaluate principal parameters (user.id, company_ids, …) in \
             {domain:?}: access predicates are decided in SQL only"
        )));
    }
    let env = records.env().clone();
    tx.prepare_access_for_domain(&env, domain).await?;
    let registry = Arc::clone(env.registry());
    let spec = registry.model_by_id(records.model_id());
    let mut builder = Builder {
        registry: &registry,
        spec,
        su: env.su(),
        external: domain.is_external(),
        unaccent: tx.config().unaccent,
        fields: Vec::new(),
    };
    // an unknown field is an error even next to a condition that makes it irrelevant once
    // optimized (`[('nope', '=', 1), ('id', 'in', [])]`)
    for node in domain.iter_conditions() {
        if let Some(condition) = node.as_condition() {
            let expr = condition.field_expr();
            let head = expr.split_once('.').map_or(expr, |(head, _)| head);
            if spec.field(head).is_none() {
                return Err(builder.input_error(format!(
                    "Invalid field {}.{head} in condition {condition:?}",
                    spec.name
                )));
            }
        }
    }
    let resolved = crate::hierarchy::resolve(tx, records, domain, false).await?;
    let resolved = access_in_memory(tx, records, &resolved).await?;
    let tz = ZoneTz::new(env.tz());
    let optimized = {
        let lookup = crate::access::TxAccess::new(tx, &env);
        optimize_domain(
            &OptimizeCtx {
                registry: &registry,
                tz: &tz,
                now: now_utc()?,
                su: env.su(),
                access: Some(&lookup),
            },
            spec,
            &resolved,
            OptimizationLevel::DynamicValues,
        )?
    };
    if optimized.has_params() {
        // an `access` condition resolved to a predicate with principal parameters: decided in
        // SQL only (R-S6)
        return Err(Error::programming(format!(
            "filtered_domain cannot evaluate principal parameters (user.id, company_ids, …) in \
             {optimized:?}: access predicates are decided in SQL only"
        )));
    }
    let mut node = builder.node(&optimized)?;
    if let Node::Const(constant) = node {
        return Ok(if constant {
            records.clone()
        } else {
            records.derive(Vec::new())
        });
    }
    let ids = records.rec_ids();
    let mut columns: Vec<Vec<Cell>> = Vec::with_capacity(builder.fields.len());
    let sudo = records.engine_sudo();
    for (fid, as_superuser) in &builder.fields {
        let field = spec.field_by_id(*fid);
        columns.push(if matches!(field.ty, FieldType::Id) {
            ids.iter()
                .map(|id| {
                    id.as_real()
                        .or(id.origin())
                        .map_or(Cell::Null, |r| Cell::Id(r.get()))
                })
                .collect()
        } else {
            // a bypass condition reads its x2many targets as superuser (no record rules)
            let source = if *as_superuser { &sudo } else { records };
            let cells = tx.field_cells(source, *fid).await?;
            check_column(spec, field, &cells)?;
            cells
        });
    }
    resolve_any(tx, records, &mut node, &columns, reset.as_ref()).await?;
    let rows: Vec<u32> = (0..ids.len())
        .map(|i| u32::try_from(i).map_err(|_| too_many_records()))
        .collect::<Result<_>>()?;
    let mut eval = Evaluator {
        columns: &columns,
        unaccent: builder.unaccent,
        buffer: Vec::new(),
        folded: String::new(),
    };
    let matched = eval.node(&node, rows)?;
    let kept: Vec<RecId> = matched.into_iter().map(|r| ids[r as usize]).collect();
    if kept.len() == ids.len() {
        return Ok(records.clone());
    }
    Ok(records.derive(kept))
}

/// The bypass forms (`any!`, `bypass_search_access`) evaluate
/// their targets as superuser; a plain `any` reached under one of them first keeps the targets
/// the original user may read (`reset`).
async fn resolve_any(
    tx: &mut Transaction,
    records: &Records,
    node: &mut Node,
    columns: &[Vec<Cell>],
    reset: Option<&Env>,
) -> Result<()> {
    let mut leaves: Vec<&mut Leaf> = Vec::new();
    collect_any(node, &mut leaves);
    for leaf in leaves {
        let Test::Any {
            comodel,
            sub,
            bypass,
        } = &leaf.test
        else {
            continue;
        };
        let mut seen: FxHashSet<RecId> = FxHashSet::default();
        let mut targets: Vec<RecId> = Vec::new();
        for cell in &columns[leaf.column] {
            match cell {
                Cell::Id(i) => {
                    if let Some(r) = RecId::from_real(*i)
                        && seen.insert(r)
                    {
                        targets.push(r);
                    }
                }
                Cell::Ids(ids) => targets.extend(ids.iter().copied().filter(|r| seen.insert(*r))),
                _ => {}
            }
        }
        let env = records.env();
        let (env, targets, nested_reset) = match (*bypass, reset) {
            (true, _) if !env.su() => (env.engine_sudo(), targets, Some(env.clone())),
            (true, reset) => (env.clone(), targets, reset.cloned()),
            (false, Some(user)) => {
                let visible = tx.readable_ids(user, *comodel, &targets).await?;
                (user.clone(), visible, None)
            }
            (false, None) if !env.su() => {
                let visible = tx.readable_ids(env, *comodel, &targets).await?;
                (env.clone(), visible, None)
            }
            (false, None) => (env.clone(), targets, None),
        };
        let set: FxHashSet<RecId> = if targets.is_empty() {
            FxHashSet::default()
        } else {
            let ids: Arc<[RecId]> = Arc::from(targets);
            let corecords = Records::from_parts(env, *comodel, Arc::clone(&ids), ids);
            let kept = filter_sub(tx, &corecords, sub, nested_reset).await?;
            kept.rec_ids().iter().copied().collect()
        };
        leaf.test = Test::Ids {
            set,
            null_matches: false,
        };
    }
    Ok(())
}

fn collect_any<'a>(node: &'a mut Node, out: &mut Vec<&'a mut Leaf>) {
    match node {
        Node::Not(child) => collect_any(child, out),
        Node::And(children) | Node::Or(children) => {
            for child in children {
                collect_any(child, out);
            }
        }
        Node::Leaf(leaf) if matches!(leaf.test, Test::Any { .. }) => out.push(leaf),
        Node::Leaf(_) | Node::Const(_) => {}
    }
}

/// Whether a non-NULL cell has the storage kind of the field type.
fn cell_fits(ty: &FieldType, cell: &Cell) -> bool {
    match ty {
        FieldType::Boolean => matches!(cell, Cell::Bool(_)),
        FieldType::Integer => matches!(cell, Cell::Int(_)),
        FieldType::Id
        | FieldType::Many2one { .. }
        | FieldType::Many2oneReference { .. }
        | FieldType::RowVersion => matches!(cell, Cell::Id(_)),
        FieldType::One2many { .. } | FieldType::Many2many { .. } => matches!(cell, Cell::Ids(_)),
        FieldType::Reference { .. } => matches!(cell, Cell::Str(_)),
        FieldType::Float { digits: None } => matches!(cell, Cell::Float(_)),
        FieldType::Float { digits: Some(_) } | FieldType::Monetary { .. } => {
            matches!(cell, Cell::Decimal(_))
        }
        FieldType::Char { .. }
        | FieldType::Text
        | FieldType::Html
        | FieldType::Selection { .. } => {
            matches!(cell, Cell::Str(_))
        }
        FieldType::Date => matches!(cell, Cell::Date(_)),
        FieldType::Datetime => matches!(cell, Cell::DateTime(_)),
        FieldType::Uuid => matches!(cell, Cell::Uuid(_)),
        FieldType::Xid8 => matches!(cell, Cell::Xid8(_)),
        FieldType::TimestampTz => matches!(cell, Cell::TimestampTz(_)),

        FieldType::Json => matches!(cell, Cell::Json(_)),
        FieldType::DateRange { .. } => matches!(cell, Cell::DateRange(_)),
        FieldType::DatetimeRange { .. } => matches!(cell, Cell::DatetimeRange(_)),
    }
}

/// In-memory comparisons assume every cell of a column has the field's storage kind; a cell of
/// another kind (a bug in whatever cached it) is a `Programming` error instead of a silent
/// mismatch.
pub(crate) fn check_column(spec: &ModelSpec, field: &FieldSpec, cells: &[Cell]) -> Result<()> {
    match cells
        .iter()
        .find(|c| !c.is_null() && !cell_fits(&field.ty, c))
    {
        None => Ok(()),
        Some(cell) => Err(Error::programming(format!(
            "The cached values of {}.{} ({}) include {cell:?}, which is not of that type",
            spec.name,
            field.name,
            field.ty.type_name()
        ))),
    }
}

fn too_many_records() -> Error {
    Error::programming("filtered_domain() supports at most 2^32 records at once")
}

// ----- preparation -------------------------------------------------------------------------------

/// A prepared domain node.
#[derive(Debug)]
enum Node {
    Const(bool),
    Not(Box<Node>),
    And(Vec<Node>),
    Or(Vec<Node>),
    Leaf(Leaf),
}

/// A prepared condition: a positive test on one loaded column, negated for negative operators.
#[derive(Debug)]
struct Leaf {
    column: usize,
    negative: bool,
    test: Test,
}

#[derive(Debug)]
enum Test {
    /// Boolean `in`: whether true rows and false (or NULL) rows match.
    Bool { on_true: bool, on_false: bool },
    /// JSON "not set" (`in [False]`).
    JsonNull,
    /// `in` over values of the field's kind; `null_matches` for NULL and falsy-value rows.
    In { set: ValueSet, null_matches: bool },
    /// The `like` family.
    Like {
        pattern: Vec<LikeToken>,
        insensitive: bool,
    },
    /// An inequality; `null_matches` when the falsy value satisfies it.
    Inequality {
        op: Operator,
        value: Scalar,
        null_matches: bool,
    },
    /// A relational `any` before its targets are filtered (`resolve_any` turns it into `Ids`).
    Any {
        comodel: ModelId,
        sub: Domain,
        bypass: bool,
    },
    /// Whether one of the row's targets is in the set.
    /// x2many `in` too: `null_matches` (a `False` member) for records without targets.
    Ids {
        set: FxHashSet<RecId>,
        null_matches: bool,
    },
    /// `overlaps` / `contains` on a range field; NULL rows never match.
    Range(RangeTest),
}

/// The operand of a range condition, with the field's bounds.
#[derive(Debug)]
enum RangeTest {
    DateOverlaps(DateRange, RangeBounds),
    DatetimeOverlaps(DatetimeRange, RangeBounds),
    DateContains(Date, RangeBounds),
    DatetimeContains(DateTime, RangeBounds),
}

impl RangeTest {
    /// PostgreSQL's `&&` / `@>` on the row's value (`false` for NULL and for a cell of another
    /// kind, which `check_column` refuses before evaluation).
    fn matches(&self, cell: &Cell) -> bool {
        match (self, cell) {
            (Self::DateOverlaps(r, b), Cell::DateRange(v)) => v.overlaps(r, *b),
            (Self::DatetimeOverlaps(r, b), Cell::DatetimeRange(v)) => v.overlaps(r, *b),
            (Self::DateContains(p, b), Cell::DateRange(v)) => v.contains(*p, *b),
            (Self::DatetimeContains(p, b), Cell::DatetimeRange(v)) => v.contains(*p, *b),
            _ => false,
        }
    }
}

/// The values of an `in` condition, hashed where equality is plain value equality (floats and
/// decimals compare numerically, so `-0.0 = 0.0` and `1.50 = 1.5` as in SQL: those stay a list).
#[derive(Debug)]
enum ValueSet {
    Ints(FxHashSet<i64>),
    Texts(FxHashSet<Arc<str>>),
    Dates(FxHashSet<Date>),
    DateTimes(FxHashSet<DateTime>),
    Uuid(FxHashSet<erp_fields::Uuid>),
    Xid8(FxHashSet<erp_fields::Xid8>),
    TimestampTz(FxHashSet<erp_fields::TimestampTz>),

    Numbers(Vec<Scalar>),
}

/// A literal of another kind than its condition's: the converter guarantees it never happens.
fn kind_mismatch(kind: ScalarKind, value: &Scalar) -> Error {
    Error::programming(format!(
        "filtered_domain(): a {:?} literal in a condition on a {} field",
        value.kind(),
        kind.sql()
    ))
}

impl ValueSet {
    fn new(kind: ScalarKind, values: Vec<Scalar>) -> Result<Self> {
        if let Some(other) = values.iter().find(|v| v.kind() != kind) {
            return Err(kind_mismatch(kind, other));
        }
        let mut set = match kind {
            ScalarKind::Int8 => Self::Ints(FxHashSet::default()),
            ScalarKind::Text => Self::Texts(FxHashSet::default()),
            ScalarKind::Date => Self::Dates(FxHashSet::default()),
            ScalarKind::Timestamp => Self::DateTimes(FxHashSet::default()),
            ScalarKind::Uuid => Self::Uuid(FxHashSet::default()),
            ScalarKind::Xid8 => Self::Xid8(FxHashSet::default()),
            ScalarKind::TimestampTz => Self::TimestampTz(FxHashSet::default()),

            ScalarKind::Float8 | ScalarKind::Numeric => return Ok(Self::Numbers(values)),
            // range fields take `overlaps`/`contains`; their `in` tests only "not set"
            ScalarKind::DateRange | ScalarKind::TsRange => {
                return Err(Error::programming(
                    "filtered_domain(): an `in` condition over range values",
                ));
            }
        };
        for value in values {
            match (&mut set, value) {
                (Self::Ints(s), Scalar::Int8(i)) => {
                    s.insert(i);
                }
                (Self::Texts(s), Scalar::Text(t)) => {
                    s.insert(t);
                }
                (Self::Dates(s), Scalar::Date(d)) => {
                    s.insert(d);
                }
                (Self::DateTimes(s), Scalar::Timestamp(d)) => {
                    s.insert(d);
                }
                (Self::Uuid(s), Scalar::Uuid(v)) => {
                    s.insert(v);
                }
                (Self::Xid8(s), Scalar::Xid8(v)) => {
                    s.insert(v);
                }
                (Self::TimestampTz(s), Scalar::TimestampTz(v)) => {
                    s.insert(v);
                }
                (_, other) => return Err(kind_mismatch(kind, &other)),
            }
        }
        Ok(set)
    }

    /// Whether the set holds `key`; `None` when the row's value is of another kind than the
    /// set's (a column whose cells do not match its field type).
    fn contains(&self, key: &Key<'_>) -> Option<bool> {
        match (self, key) {
            (Self::Ints(set), Key::Int(i)) => Some(set.contains(i)),
            (Self::Texts(set), Key::Text(t)) => Some(set.contains(*t)),
            (Self::Dates(set), Key::Date(d)) => Some(set.contains(d)),
            (Self::DateTimes(set), Key::DateTime(d)) => Some(set.contains(d)),
            (Self::Uuid(set), Key::Uuid(v)) => Some(set.contains(v)),
            (Self::Xid8(set), Key::Xid8(v)) => Some(set.contains(v)),
            (Self::TimestampTz(set), Key::TimestampTz(v)) => Some(set.contains(v)),
            (Self::Numbers(values), key) => {
                let mut found = false;
                for v in values {
                    found |= key.compare(v)? == Ordering::Equal;
                }
                Some(found)
            }
            _ => None,
        }
    }
}

/// One element of a `LIKE` pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LikeToken {
    Char(char),
    /// `_`.
    One,
    /// `%`.
    Any,
}

/// `None` when the pattern ends with the
/// escape character, which PostgreSQL rejects ("LIKE pattern must not end with escape
/// character").
fn like_tokens(pattern: &str) -> Option<Vec<LikeToken>> {
    let mut out = Vec::with_capacity(pattern.len());
    let mut escaped = false;
    for c in pattern.chars() {
        if escaped {
            escaped = false;
            out.push(LikeToken::Char(c));
        } else if c == '\\' {
            escaped = true;
        } else if c == '%' {
            if out.last() != Some(&LikeToken::Any) {
                out.push(LikeToken::Any);
            }
        } else if c == '_' {
            out.push(LikeToken::One);
        } else {
            out.push(LikeToken::Char(c));
        }
    }
    (!escaped).then_some(out)
}

/// Whether `text` matches the whole pattern (greedy wildcard matching with backtracking to the
/// last `%`: linear for patterns with one `%` run, O(n·m) at worst).
fn like_match(pattern: &[LikeToken], text: &[char]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(LikeToken::Char(c)) if *c == text[t] => {
                p += 1;
                t += 1;
            }
            Some(LikeToken::One) => {
                p += 1;
                t += 1;
            }
            Some(LikeToken::Any) => {
                star = Some((p, t));
                p += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|tok| *tok == LikeToken::Any)
}

fn fold_into(text: &str, unaccent: bool, out: &mut String) {
    out.clear();
    for c in text.chars() {
        match strip_accent(c).filter(|_| unaccent) {
            Some(base) => out.push_str(base),
            None => {
                for lower in c.to_lowercase() {
                    out.push(lower);
                }
            }
        }
    }
}

fn fold(text: &str, unaccent: bool) -> String {
    let mut out = String::with_capacity(text.len());
    fold_into(text, unaccent, &mut out);
    out
}

/// The base letters (in lower case) of a Latin-1 Supplement or Latin Extended-A letter, as
/// PostgreSQL's `unaccent` rules give them up to case (`é`/`É` → `e`, `ß` → `ss`, `æ` → `ae`,
/// `œ` → `oe`, `ø` → `o`, `ł` → `l`, `þ` → `th`); `None` for characters kept as they are.
fn strip_accent(c: char) -> Option<&'static str> {
    // U+00E0…U+00FF; U+00F7 is the division sign and stays
    const LATIN1: [Option<&str>; 32] = [
        Some("a"),
        Some("a"),
        Some("a"),
        Some("a"),
        Some("a"),
        Some("a"),
        Some("ae"),
        Some("c"),
        Some("e"),
        Some("e"),
        Some("e"),
        Some("e"),
        Some("i"),
        Some("i"),
        Some("i"),
        Some("i"),
        Some("d"),
        Some("n"),
        Some("o"),
        Some("o"),
        Some("o"),
        Some("o"),
        Some("o"),
        None,
        Some("o"),
        Some("u"),
        Some("u"),
        Some("u"),
        Some("u"),
        Some("y"),
        Some("th"),
        Some("y"),
    ];
    // U+0100…U+017F, mostly (upper, lower) pairs mapping to the same base letter
    const EXTENDED_A: [&str; 128] = [
        "a", "a", "a", "a", "a", "a", "c", "c", "c", "c", "c", "c", "c", "c", "d", "d", "d", "d",
        "e", "e", "e", "e", "e", "e", "e", "e", "e", "e", "g", "g", "g", "g", "g", "g", "g", "g",
        "h", "h", "h", "h", "i", "i", "i", "i", "i", "i", "i", "i", "i", "i", "ij", "ij", "j", "j",
        "k", "k", "k", "l", "l", "l", "l", "l", "l", "l", "l", "l", "l", "n", "n", "n", "n", "n",
        "n", "n", "n", "n", "o", "o", "o", "o", "o", "o", "oe", "oe", "r", "r", "r", "r", "r", "r",
        "s", "s", "s", "s", "s", "s", "s", "s", "t", "t", "t", "t", "t", "t", "u", "u", "u", "u",
        "u", "u", "u", "u", "u", "u", "u", "u", "w", "w", "y", "y", "y", "z", "z", "z", "z", "z",
        "z", "s",
    ];
    let code = c as u32;
    match code {
        0xDF => Some("ss"),
        // upper case: the same base letter (ILIKE folds its ASCII case afterwards)
        0xC0..=0xDE => LATIN1.get((code - 0xC0) as usize).copied().flatten(),
        0xE0..=0xFF => LATIN1.get((code - 0xE0) as usize).copied().flatten(),
        0x100..=0x17F => EXTENDED_A.get((code - 0x100) as usize).copied(),
        _ => None,
    }
}

struct Builder<'a> {
    registry: &'a Registry,
    spec: &'a ModelSpec,
    /// Whether the environment is superuser (bypass conditions then read like the others).
    su: bool,
    external: bool,
    unaccent: bool,
    /// Fields to load, in column order, and whether to read them as superuser.
    fields: Vec<(FieldId, bool)>,
}

impl Builder<'_> {
    /// `Validation` for external domains, `Programming` for domains built in Rust.
    fn input_error(&self, message: String) -> Error {
        if self.external {
            Error::validation(message)
        } else {
            Error::programming(message)
        }
    }

    fn not_supported(&self, what: String, slice: &str) -> Error {
        self.input_error(format!("{what} is not supported until {slice}"))
    }

    fn invalid_value(
        &self,
        field: &FieldSpec,
        value: &DomainValue,
        condition: &Condition,
    ) -> Error {
        self.input_error(format!(
            "Invalid value for {}.{}: {value} in condition {condition:?}",
            self.spec.name, field.name
        ))
    }

    fn column(&mut self, field: FieldId) -> usize {
        self.column_as(field, false)
    }

    fn column_as(&mut self, field: FieldId, superuser: bool) -> usize {
        let key = (field, superuser && !self.su);
        match self.fields.iter().position(|f| *f == key) {
            Some(i) => i,
            None => {
                self.fields.push(key);
                self.fields.len() - 1
            }
        }
    }

    fn node(&mut self, domain: &Domain) -> Result<Node> {
        Ok(match domain.kind() {
            DomainKind::True => Node::Const(true),
            DomainKind::False => Node::Const(false),
            DomainKind::Not(child) => match self.node(child)? {
                Node::Const(b) => Node::Const(!b),
                child => Node::Not(Box::new(child)),
            },
            DomainKind::And(children) => Node::And(
                children
                    .iter()
                    .map(|c| self.node(c))
                    .collect::<Result<_>>()?,
            ),
            DomainKind::Or(children) => Node::Or(
                children
                    .iter()
                    .map(|c| self.node(c))
                    .collect::<Result<_>>()?,
            ),
            DomainKind::Condition(condition) => Node::Leaf(self.condition(condition)?),
            DomainKind::Custom(custom) => {
                return Err(self.not_supported(
                    format!("The custom domain node {custom:?}"),
                    "a later milestone (Domain.custom)",
                ));
            }
        })
    }

    fn condition(&mut self, condition: &Condition) -> Result<Leaf> {
        let spec = self.spec;
        let operator = condition.operator();
        let field_expr = condition.field_expr();
        let (name, property) = match field_expr.split_once('.') {
            Some((name, property)) => (name, Some(property)),
            None => (field_expr, None),
        };
        let field = spec.field(name).ok_or_else(|| {
            self.input_error(format!(
                "Invalid field {}.{name} in condition {condition:?}",
                spec.name
            ))
        })?;
        let comodel = field.ty.comodel().and_then(|c| self.registry.model_id(c));
        if operator.is_any() && ReferenceQuery::of(condition.value()).is_some() {
            return Err(Error::programming(format!(
                "the reference query {condition:?} is decided in SQL only: it cannot be evaluated \
                 on loaded records"
            )));
        }
        if operator.is_any() {
            let sub = match (comodel, condition.value()) {
                (Some(comodel), DomainValue::Domain(sub)) => Some((comodel, sub.clone())),
                (_, value) => RecordsQuery::of(value)
                    .map(|q| q.domain().map(|d| (q.model, d)))
                    .transpose()?,
            };
            let Some((comodel, sub)) = sub else {
                return Err(match condition.value() {
                    DomainValue::Opaque(opaque) if opaque.kind() == OpaqueKind::Sql => self
                        .not_supported(
                            format!("The SQL-valued condition {condition:?}"),
                            "a later milestone",
                        ),
                    _ => Error::programming(format!(
                        "filter_function() operator '{operator}' accepts a domain or a Query, \
                         got {}",
                        condition.value()
                    )),
                });
            };
            let bypass = operator.is_internal() || field.bypass_search_access;
            let column = self.column_as(field.id, bypass);
            return Ok(Leaf {
                column,
                negative: operator.is_negative(),
                test: Test::Any {
                    comodel,
                    sub,
                    bypass,
                },
            });
        }
        if matches!(operator, Operator::ChildOf | Operator::ParentOf) {
            // resolved to id conditions before optimization
            return Err(Error::programming(format!(
                "Unresolved hierarchy condition {condition:?}"
            )));
        }
        if let DomainValue::Opaque(opaque) = condition.value() {
            return Err(if opaque.kind() == OpaqueKind::Sql {
                self.not_supported(
                    format!("The SQL-valued condition {condition:?}"),
                    "a later milestone",
                )
            } else {
                self.not_supported(
                    format!("The query-valued condition {condition:?}"),
                    "a later milestone",
                )
            });
        }
        if !operator.is_standard() {
            return Err(Error::programming(format!(
                "Invalid operator '{operator}' in domain term {condition:?}"
            )));
        }
        if let Some(property) = property {
            return Err(self.not_supported(
                format!(
                    "The property condition {condition:?} ('{property}' of {}.{})",
                    spec.name, field.name
                ),
                "a later milestone (date parts and properties,)",
            ));
        }
        let column = self.column(field.id);
        let test = match operator.positive() {
            Operator::In => self.in_test(field, condition)?,
            op if op.is_range() => self.range_test(field, condition)?,
            op if op.is_like() => self.like_test(field, condition)?,
            op if op.is_inequality() => self.inequality_test(field, condition)?,
            _ => {
                return Err(Error::programming(format!(
                    "Invalid operator '{operator}' in domain term {condition:?}"
                )));
            }
        };
        Ok(Leaf {
            column,
            negative: operator.is_negative(),
            test,
        })
    }

    fn in_test(&self, field: &FieldSpec, condition: &Condition) -> Result<Test> {
        let items: Vec<&DomainValue> = match condition.value() {
            DomainValue::Set(set) => set.iter().collect(),
            DomainValue::List(list) => list.iter().collect(),
            other => {
                return Err(Error::programming(format!(
                    "filter_function() 'in' operator expects a collection, not a {other}"
                )));
            }
        };
        match &field.ty {
            FieldType::Boolean => {
                let has = |b: bool| items.iter().any(|v| v.is_truthy() == b);
                return Ok(Test::Bool {
                    on_true: has(true),
                    on_false: has(false),
                });
            }
            FieldType::Json => {
                if !items.is_empty() && items.iter().all(|v| matches!(v, DomainValue::Bool(false)))
                {
                    return Ok(Test::JsonNull);
                }
                return Err(self.not_supported(
                    format!(
                        "Filtering the JSON field {}.{} with {condition:?}",
                        self.spec.name, field.name
                    ),
                    "a later milestone",
                ));
            }
            // a range is compared with `overlaps`/`contains`; `in [False]` tests "not set"
            FieldType::DateRange { .. } | FieldType::DatetimeRange { .. } => {
                if !items.is_empty() && items.iter().all(|v| matches!(v, DomainValue::Bool(false)))
                {
                    return Ok(Test::JsonNull);
                }
                return Err(self.input_error(format!(
                    "{}.{} is a range field: compare it with 'overlaps' or 'contains' (only = \
                     False and != False test whether it is set), in condition {condition:?}",
                    self.spec.name, field.name
                )));
            }
            FieldType::One2many { .. } | FieldType::Many2many { .. } => {
                let mut set: FxHashSet<RecId> = FxHashSet::default();
                let mut null_matches = false;
                for value in items {
                    match value {
                        DomainValue::Bool(false) => null_matches = true,
                        DomainValue::Int(i) => {
                            let id = RecId::from_real(*i)
                                .ok_or_else(|| self.invalid_value(field, value, condition))?;
                            set.insert(id);
                        }
                        other => return Err(self.invalid_value(field, other, condition)),
                    }
                }
                return Ok(Test::Ids { set, null_matches });
            }
            _ => {}
        }
        let kind = self.kind(field, condition)?;
        let mut values: Vec<Scalar> = Vec::with_capacity(items.len());
        let mut null_matches = false;
        for value in items {
            if matches!(value, DomainValue::Bool(false)) {
                null_matches = true;
                continue;
            }
            let scalar =
                to_scalar(kind, value).map_err(|_| self.invalid_value(field, value, condition))?;
            if !values.contains(&scalar) {
                values.push(scalar);
            }
        }
        // the falsy value (0, 0.0, '') and NULL are the same "not set" for these types
        if let Some(null_value) = falsy_value(&field.ty)
            && values.contains(&null_value)
        {
            null_matches = true;
        }
        Ok(Test::In {
            set: ValueSet::new(kind, values)?,
            null_matches,
        })
    }

    /// `overlaps` / `contains` on a range field (the same operand rules as the SQL compiler).
    fn range_test(&self, field: &FieldSpec, condition: &Condition) -> Result<Test> {
        let operator = condition.operator();
        let value = condition.value();
        let invalid = || self.invalid_value(field, value, condition);
        let (bounds, element) = match field.ty {
            FieldType::DateRange { bounds } => (bounds, ScalarKind::Date),
            FieldType::DatetimeRange { bounds } => (bounds, ScalarKind::Timestamp),
            _ => {
                return Err(self.input_error(format!(
                    "operator '{operator}' applies to date_range and datetime_range fields, not \
                     to {}.{} ({}) in condition {condition:?}",
                    self.spec.name,
                    field.name,
                    field.ty.type_name()
                )));
            }
        };
        if operator == Operator::Contains {
            return Ok(Test::Range(match to_scalar(element, value) {
                Ok(Scalar::Date(d)) => RangeTest::DateContains(d, bounds),
                Ok(Scalar::Timestamp(t)) => RangeTest::DatetimeContains(t, bounds),
                _ => return Err(invalid()),
            }));
        }
        let DomainValue::List(ends) = value else {
            return Err(invalid());
        };
        let [lower, upper] = ends.as_slice() else {
            return Err(invalid());
        };
        // each end converts to the field's element type (`False`: unbounded); the kind of the
        // range is the field's, never guessed from the ends (`[False, False]` has none)
        let end = |v: &DomainValue| match v {
            DomainValue::Bool(false) => Ok(None),
            other => to_scalar(element, other).map(Some).map_err(|_| invalid()),
        };
        let (lo, hi) = (end(lower)?, end(upper)?);
        let inverted = |l: Option<&Scalar>, u: Option<&Scalar>| {
            matches!((l, u), (Some(Scalar::Date(l)), Some(Scalar::Date(u))) if l > u)
                || matches!((l, u), (Some(Scalar::Timestamp(l)), Some(Scalar::Timestamp(u))) if l > u)
        };
        if inverted(lo.as_ref(), hi.as_ref()) {
            return Err(invalid());
        }
        let test = if element == ScalarKind::Date {
            let date = |s: Option<Scalar>| match s {
                None => Ok(None),
                Some(Scalar::Date(d)) => Ok(Some(d)),
                Some(_) => Err(invalid()),
            };
            RangeTest::DateOverlaps(Range::new(date(lo)?, date(hi)?), bounds)
        } else {
            let stamp = |s: Option<Scalar>| match s {
                None => Ok(None),
                Some(Scalar::Timestamp(t)) => Ok(Some(t)),
                Some(_) => Err(invalid()),
            };
            RangeTest::DatetimeOverlaps(Range::new(stamp(lo)?, stamp(hi)?), bounds)
        };
        Ok(Test::Range(test))
    }

    fn like_test(&self, field: &FieldSpec, condition: &Condition) -> Result<Test> {
        let operator = condition.operator().positive();
        if matches!(
            field.ty,
            FieldType::Uuid | FieldType::Xid8 | FieldType::TimestampTz
        ) {
            return Err(self.input_error(format!(
                "operator '{}' is not supported for native field {}.{} ({})",
                condition.operator(),
                self.spec.name,
                field.name,
                field.ty.type_name()
            )));
        }
        if field.ty.range_bounds().is_some() {
            return Err(self.input_error(format!(
                "operator '{}' does not apply to the range field {}.{} in condition \
                 {condition:?}",
                condition.operator(),
                self.spec.name,
                field.name
            )));
        }
        if matches!(field.ty, FieldType::Json) {
            return Err(self.not_supported(
                format!(
                    "Filtering the JSON field {}.{} with {condition:?}",
                    self.spec.name, field.name
                ),
                "a later milestone",
            ));
        }
        let value = match condition.value() {
            DomainValue::Str(s) => s.clone(),
            DomainValue::Int(i) => i.to_string(),
            other => return Err(self.invalid_value(field, other, condition)),
        };
        let exact = matches!(operator, Operator::EqLike | Operator::EqILike);
        let pattern = if exact { value } else { format!("%{value}%") };
        let insensitive = matches!(operator, Operator::ILike | Operator::EqILike);
        let pattern = if insensitive {
            fold(&pattern, self.unaccent)
        } else {
            pattern
        };
        Ok(Test::Like {
            pattern: like_tokens(&pattern).ok_or_else(|| {
                self.input_error(format!(
                    "LIKE pattern must not end with escape character in condition {condition:?}"
                ))
            })?,
            insensitive,
        })
    }

    fn inequality_test(&self, field: &FieldSpec, condition: &Condition) -> Result<Test> {
        let operator = condition.operator();
        let kind = ScalarKind::of_field(&field.ty).ok_or_else(|| {
            Error::programming(format!(
                "Invalid operator '{operator}' in domain term {condition:?}"
            ))
        })?;
        let null_value = falsy_value(&field.ty);
        let value = condition.value();
        let scalar = match (&null_value, value) {
            (Some(null), DomainValue::Bool(false)) => null.clone(),
            _ => to_scalar(kind, value).map_err(|_| self.invalid_value(field, value, condition))?,
        };
        let null_matches = null_value
            .as_ref()
            .and_then(|null| Key::of_scalar(null)?.compare(&scalar))
            .is_some_and(|ordering| satisfies(operator, ordering));
        Ok(Test::Inequality {
            op: operator,
            value: scalar,
            null_matches,
        })
    }

    /// The kind a field's values compare as; x2many fields have none (their `in` conditions are
    /// rewritten to `any` by optimization).
    fn kind(&self, field: &FieldSpec, condition: &Condition) -> Result<ScalarKind> {
        ScalarKind::of_field(&field.ty).ok_or_else(|| {
            self.input_error(format!(
                "Invalid condition {condition:?} on {}.{} ({})",
                self.spec.name,
                field.name,
                field.ty.type_name()
            ))
        })
    }
}

fn satisfies(operator: Operator, ordering: Ordering) -> bool {
    match operator {
        Operator::Lt => ordering.is_lt(),
        Operator::Gt => ordering.is_gt(),
        Operator::Le => ordering.is_le(),
        _ => ordering.is_ge(),
    }
}

// ----- evaluation --------------------------------------------------------------------------------

/// A non-NULL value of a cell, borrowed.
#[derive(Debug, Clone, Copy)]
enum Key<'a> {
    Int(i64),
    Float(f64),
    Dec(Decimal),
    Text(&'a str),
    Date(Date),
    DateTime(DateTime),
    Uuid(erp_fields::Uuid),
    Xid8(erp_fields::Xid8),
    TimestampTz(erp_fields::TimestampTz),
}

impl<'a> Key<'a> {
    /// `None` for NULL (and for booleans and JSON, which have their own tests).
    fn of_cell(cell: &'a Cell) -> Option<Self> {
        Some(match cell {
            Cell::Int(i) => Self::Int(i64::from(*i)),
            Cell::Id(i) => Self::Int(*i),
            Cell::Float(f) => Self::Float(*f),
            Cell::Decimal(d) => Self::Dec(*d),
            Cell::Str(s) => Self::Text(s),
            Cell::Date(d) => Self::Date(*d),
            Cell::DateTime(d) => Self::DateTime(*d),
            Cell::Uuid(v) => Self::Uuid(*v),
            Cell::Xid8(v) => Self::Xid8(*v),
            Cell::TimestampTz(v) => Self::TimestampTz(*v),

            // ranges have their own tests
            Cell::Null
            | Cell::Bool(_)
            | Cell::Json(_)
            | Cell::Ids(_)
            | Cell::DateRange(_)
            | Cell::DatetimeRange(_) => return None,
        })
    }

    /// `None` for range literals, which only range tests use.
    fn of_scalar(scalar: &'a Scalar) -> Option<Self> {
        Some(match scalar {
            Scalar::Int8(i) => Self::Int(*i),
            Scalar::Float8(f) => Self::Float(*f),
            Scalar::Numeric(d) => Self::Dec(*d),
            Scalar::Text(t) => Self::Text(t),
            Scalar::Date(d) => Self::Date(*d),
            Scalar::Timestamp(d) => Self::DateTime(*d),
            Scalar::Uuid(v) => Self::Uuid(*v),
            Scalar::Xid8(v) => Self::Xid8(*v),
            Scalar::TimestampTz(v) => Self::TimestampTz(*v),

            Scalar::DateRange(..) | Scalar::TsRange(..) => return None,
        })
    }

    /// The falsy value of its type (`0`, `0.0`, `''`): "not set" like NULL.
    fn is_falsy(&self) -> bool {
        match self {
            Self::Int(i) => *i == 0,
            Self::Float(f) => *f == 0.0,
            Self::Dec(d) => d.is_zero(),
            Self::Text(t) => t.is_empty(),
            Self::Date(_)
            | Self::DateTime(_)
            | Self::Uuid(_)
            | Self::Xid8(_)
            | Self::TimestampTz(_) => false,
        }
    }

    fn compare(&self, other: &Scalar) -> Option<Ordering> {
        match (self, other) {
            (Self::Int(a), Scalar::Int8(b)) => Some(a.cmp(b)),
            (Self::Float(a), Scalar::Float8(b)) => a.partial_cmp(b),
            (Self::Dec(a), Scalar::Numeric(b)) => Some(a.cmp(b)),
            (Self::Text(a), Scalar::Text(b)) => Some((*a).cmp(&**b)),
            (Self::Date(a), Scalar::Date(b)) => Some(a.cmp(b)),
            (Self::DateTime(a), Scalar::Timestamp(b)) => Some(a.cmp(b)),
            (Self::Uuid(a), Scalar::Uuid(b)) => Some(a.cmp(b)),
            (Self::Xid8(a), Scalar::Xid8(b)) => Some(a.cmp(b)),
            (Self::TimestampTz(a), Scalar::TimestampTz(b)) => Some(a.cmp(b)),

            _ => None,
        }
    }
}

/// The text a `like` condition matches against: the value itself for text columns, otherwise the
/// text PostgreSQL's `::text` cast gives (ISO dates); `None` for NULL. JSON is refused when the
/// condition is prepared.
fn like_text(cell: &Cell) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    Some(match cell {
        // `like` on a range field is refused when the condition is prepared
        Cell::Null | Cell::Json(_) | Cell::Ids(_) | Cell::DateRange(_) | Cell::DatetimeRange(_) => {
            return None;
        }
        Cell::Uuid(_) | Cell::Xid8(_) | Cell::TimestampTz(_) => return None,
        Cell::Str(s) => Cow::Borrowed(&**s),
        Cell::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
        Cell::Int(i) => Cow::Owned(i.to_string()),
        Cell::Id(i) => Cow::Owned(i.to_string()),
        Cell::Float(f) => Cow::Owned(pg_float8_text(*f)),
        // numeric keeps the stored scale (`1.00`), as rust_decimal does
        Cell::Decimal(d) => Cow::Owned(d.to_string()),
        Cell::Date(d) => Cow::Owned(d.to_string()),
        Cell::DateTime(d) => Cow::Owned(pg_timestamp_text(*d)),
    })
}

/// `float8::text` in PostgreSQL 12+ (`extra_float_digits = 1`): the shortest digits that read back
/// exactly, positional for decimal exponents -4…14, otherwise `d.ddde±XX` with at least two
/// exponent digits (`1e+16`, `1e-05`); negative zero is `-0`.
fn pg_float8_text(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }
    // `{:e}` is Rust's shortest round-trip representation in scientific form: `2.5e0`, `1e16`
    let scientific = format!("{f:e}");
    let Some((mantissa, exponent)) = scientific.split_once('e') else {
        return scientific;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return scientific;
    };
    if (-4..15).contains(&exponent) {
        // Rust's `Display` prints the same shortest digits positionally
        f.to_string()
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs())
    }
}

/// `timestamp::text` with the ISO date style: `YYYY-MM-DD HH:MM:SS`, then the microseconds without
/// trailing zeros when there are any (`…:00.5`).
fn pg_timestamp_text(d: DateTime) -> String {
    let (h, m, s, us) = d.time();
    let mut text = format!("{} {h:02}:{m:02}:{s:02}", d.date());
    if us > 0 {
        let fraction = format!("{us:06}");
        text.push('.');
        text.push_str(fraction.trim_end_matches('0'));
    }
    text
}

struct Evaluator<'a> {
    columns: &'a [Vec<Cell>],
    /// Accent folding of `ilike` (`OrmConfig::unaccent`).
    unaccent: bool,
    /// Reused per `like` row: the characters of the folded text.
    buffer: Vec<char>,
    folded: String,
}

/// `rows` minus `remove`, both ascending.
fn difference(rows: &[u32], remove: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(rows.len().saturating_sub(remove.len()));
    let mut j = 0;
    for &r in rows {
        while j < remove.len() && remove[j] < r {
            j += 1;
        }
        if j < remove.len() && remove[j] == r {
            continue;
        }
        out.push(r);
    }
    out
}

impl Evaluator<'_> {
    /// The rows of `rows` (ascending) that satisfy `node`, ascending.
    fn node(&mut self, node: &Node, rows: Vec<u32>) -> Result<Vec<u32>> {
        Ok(match node {
            Node::Const(true) => rows,
            Node::Const(false) => Vec::new(),
            Node::Not(child) => {
                let matched = self.node(child, rows.clone())?;
                difference(&rows, &matched)
            }
            Node::And(children) => {
                let mut rows = rows;
                for child in children {
                    if rows.is_empty() {
                        break;
                    }
                    rows = self.node(child, rows)?;
                }
                rows
            }
            Node::Or(children) => {
                let mut remaining = rows;
                let mut matched: Vec<u32> = Vec::new();
                for child in children {
                    if remaining.is_empty() {
                        break;
                    }
                    let hit = self.node(child, remaining.clone())?;
                    if !hit.is_empty() {
                        remaining = difference(&remaining, &hit);
                        matched.extend(hit);
                    }
                }
                matched.sort_unstable();
                matched
            }
            Node::Leaf(leaf) => self.leaf(leaf, rows)?,
        })
    }

    /// One condition over the rows: a single loop per test kind, no per-row dispatch.
    fn leaf(&mut self, leaf: &Leaf, mut rows: Vec<u32>) -> Result<Vec<u32>> {
        let column = &self.columns[leaf.column];
        let negative = leaf.negative;
        match &leaf.test {
            Test::Bool { on_true, on_false } => rows.retain(|&r| {
                let value = matches!(column[r as usize], Cell::Bool(true));
                (if value { *on_true } else { *on_false }) != negative
            }),
            Test::JsonNull => rows.retain(|&r| column[r as usize].is_null() != negative),
            Test::Range(test) => rows.retain(|&r| test.matches(&column[r as usize]) != negative),
            Test::Ids { set, null_matches } => rows.retain(|&r| {
                let hit = match &column[r as usize] {
                    Cell::Id(i) => RecId::from_real(*i).is_some_and(|id| set.contains(&id)),
                    Cell::Ids(ids) if !ids.is_empty() => ids.iter().any(|id| set.contains(id)),
                    _ => *null_matches,
                };
                hit != negative
            }),
            Test::Any { .. } => {
                return Err(Error::programming(
                    "filtered_domain(): a relational condition reached evaluation unresolved",
                ));
            }
            Test::In { set, null_matches } => rows.retain(|&r| {
                let hit = match Key::of_cell(&column[r as usize]) {
                    // columns are checked against their field type before evaluation
                    Some(key) if !key.is_falsy() => set.contains(&key).unwrap_or(false),
                    _ => *null_matches,
                };
                hit != negative
            }),
            Test::Inequality {
                op,
                value,
                null_matches,
            } => rows.retain(|&r| {
                let hit = match Key::of_cell(&column[r as usize]) {
                    // falsy values compare like any other value: `0 < 5`
                    Some(key) => key.compare(value).is_some_and(|o| satisfies(*op, o)),
                    None => *null_matches,
                };
                hit != negative
            }),
            Test::Like {
                pattern,
                insensitive,
            } => {
                let (buffer, folded, unaccent) =
                    (&mut self.buffer, &mut self.folded, self.unaccent);
                rows.retain(|&r| {
                    let hit = match like_text(&column[r as usize]) {
                        None => false,
                        Some(text) => {
                            buffer.clear();
                            if *insensitive {
                                fold_into(&text, unaccent, folded);
                                buffer.extend(folded.chars());
                            } else {
                                buffer.extend(text.chars());
                            }
                            like_match(pattern, buffer)
                        }
                    };
                    hit != negative
                });
            }
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, text: &str) -> bool {
        let chars: Vec<char> = text.chars().collect();
        like_match(&like_tokens(pattern).unwrap(), &chars)
    }

    #[test]
    fn like_patterns_follow_sql() {
        assert!(matches("%j%", "Ignitive Labs j"));
        assert!(matches("%%", ""));
        assert!(!matches("%a%", ""));
        assert!(matches("a_c", "abc"));
        assert!(!matches("a_c", "abbc"));
        assert!(matches("a%c", "abbbc"));
        assert!(matches("%b%b%", "abcab"));
        assert!(!matches("%b%b%", "abca"));
        assert!(matches("5\\%", "5%"));
        assert!(!matches("5\\%", "50"));
        assert!(matches("\\_x", "_x"));
        assert!(!matches("\\_x", "ax"));
        assert!(matches("%é%", "Hélène"));
    }

    #[test]
    fn columns_of_another_kind_are_programming_errors() {
        use erp_registry::{AddonDescriptor, FieldDecl, ModelDecl, RegistryBuilder};
        static T: AddonDescriptor = AddonDescriptor {
            name: "t",
            depends: &["base"],
            exact_only: false,
        };
        static P: ModelDecl = ModelDecl::new("t", "t.partner")
            .fields(const { &[FieldDecl::char("name"), FieldDecl::integer("n")] });
        let registry = RegistryBuilder::new([&T]).with_decls([&P]).build().unwrap();
        let spec = registry.model("t.partner").unwrap();
        let name = spec.field("name").unwrap();
        let n = spec.field("n").unwrap();
        assert!(check_column(spec, name, &[Cell::Str(Arc::from("a")), Cell::Null]).is_ok());
        let err = check_column(spec, name, &[Cell::Str(Arc::from("a")), Cell::Int(1)]).unwrap_err();
        assert_eq!(err.kind(), erp_fields::ErrorKind::Programming);
        assert!(
            err.message().contains("t.partner.name"),
            "{}",
            err.message()
        );
        assert!(check_column(spec, n, &[Cell::Int(1)]).is_ok());
        assert!(check_column(spec, n, &[Cell::Id(1)]).is_err());
    }

    #[test]
    fn trailing_escape_is_refused() {
        assert!(like_tokens("a\\").is_none());
        assert!(like_tokens("a\\\\").is_some());
        assert!(like_tokens("%a\\%").is_some());
    }

    #[test]
    fn text_casts_follow_postgresql() {
        for (f, text) in [
            (2.5, "2.5"),
            (1e16, "1e+16"),
            (1e15, "1e+15"),
            (123_456_789_012_345.0, "123456789012345"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (-1.5e-7, "-1.5e-07"),
            (1.0, "1"),
            (0.1, "0.1"),
            (-0.0, "-0"),
            (1e100, "1e+100"),
        ] {
            assert_eq!(pg_float8_text(f), text, "{f:?}");
        }
        let d = Date::from_ymd(2024, 1, 6).unwrap();
        assert_eq!(
            pg_timestamp_text(DateTime::from_parts(d, 8, 0, 0, 500_000).unwrap()),
            "2024-01-06 08:00:00.5"
        );
        assert_eq!(
            pg_timestamp_text(DateTime::from_parts(d, 9, 30, 15, 123_456).unwrap()),
            "2024-01-06 09:30:15.123456"
        );
        assert_eq!(
            pg_timestamp_text(DateTime::from_parts(d, 0, 0, 0, 0).unwrap()),
            "2024-01-06 00:00:00"
        );
    }

    #[test]
    fn value_sets_refuse_mixed_kinds() {
        let set = ValueSet::new(
            ScalarKind::Date,
            vec![Scalar::Date(Date::from_ymd(2024, 1, 5).unwrap())],
        )
        .unwrap();
        assert_eq!(
            set.contains(&Key::Date(Date::from_ymd(2024, 1, 5).unwrap())),
            Some(true)
        );
        assert_eq!(set.contains(&Key::Int(1)), None);
        assert!(ValueSet::new(ScalarKind::Int8, vec![Scalar::Text(Arc::from("1"))]).is_err());
        let zeros = ValueSet::new(ScalarKind::Float8, vec![Scalar::Float8(0.0)]).unwrap();
        assert_eq!(zeros.contains(&Key::Float(-0.0)), Some(true));
    }

    #[test]
    fn folding_with_accents_as_a_separate_step() {
        assert_eq!(
            fold("ÉA", false),
            "éa",
            "C collation with UTF-8 character type: Unicode letters fold"
        );
        assert_eq!(fold("ÉA", true), "ea");
        assert_eq!(fold("ÆØÞ", true), "aeoth");
        assert_eq!(fold("Hélène", false), "hélène");
        assert_eq!(fold("Hélène", true), "helene");
        assert_eq!(fold("ŒUVRE Straße", true), "oeuvre strasse");
        assert_eq!(fold("Ångström", true), "angstrom");
        assert_eq!(fold("日本", true), "日本");
    }

    #[test]
    fn difference_of_sorted_rows() {
        assert_eq!(difference(&[1, 2, 3, 5, 8], &[2, 5, 9]), vec![1, 3, 8]);
        assert_eq!(difference(&[1, 2], &[]), vec![1, 2]);
    }
}

#[cfg(test)]
mod native_fields_tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::{TimestampTz, Uuid, Xid8};

    #[test]
    fn native_fields_predicates_preserve_scalar_order_and_membership() {
        let high = Xid8::from_u64(u64::MAX);
        let high_cell = Cell::Xid8(high);
        let key = Key::of_cell(&high_cell).unwrap();
        assert_eq!(
            key.compare(&Scalar::Xid8(Xid8::from_u64(9))),
            Some(Ordering::Greater)
        );
        let instant = TimestampTz::parse("2024-03-01T01:00:00.123456+07:00").unwrap();
        let normalized = TimestampTz::parse("2024-02-29T18:00:00.123456Z").unwrap();
        for (cell, scalar) in [
            (Cell::Uuid(Uuid::NIL), Scalar::Uuid(Uuid::NIL)),
            (Cell::Xid8(high), Scalar::Xid8(high)),
            (Cell::TimestampTz(instant), Scalar::TimestampTz(normalized)),
        ] {
            let key = Key::of_cell(&cell).unwrap();
            assert!(!key.is_falsy());
            assert_eq!(key.compare(&scalar), Some(Ordering::Equal));
            let set = ValueSet::new(scalar.kind(), vec![scalar]).unwrap();
            assert_eq!(set.contains(&key), Some(true));
            assert_eq!(set.contains(&Key::Text("not native")), None);
            assert!(like_text(&cell).is_none());
        }
    }
}
